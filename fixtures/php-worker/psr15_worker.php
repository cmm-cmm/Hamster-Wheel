<?php

declare(strict_types=1);

/**
 * Persistent worker for PSR-15 applications (PSR-7 messages, PSR-17 factories).
 *
 * A PSR-15 front controller typically builds its request from superglobals and
 * emits the response with header() and echo, neither of which exists in a
 * worker. So the application is reached through an entry file instead of its
 * front controller: a PHP file that builds the application exactly as the
 * front controller does and returns it without handling a request.
 *
 * HAMSTER_PSR15_ENTRY names that file. It must return an array with:
 *   handler                Psr\Http\Server\RequestHandlerInterface
 *   request_factory        Psr\Http\Message\ServerRequestFactoryInterface
 *   stream_factory         Psr\Http\Message\StreamFactoryInterface
 *   uploaded_file_factory  Psr\Http\Message\UploadedFileFactoryInterface
 *   reset (optional)       callable run after every request
 *
 * The application object is reused for every request. Anything it keeps
 * between requests beyond what `reset` clears is shared by the next request;
 * this adapter cannot know what that is. A fatal error ends the worker and the
 * server answers 502: an application's register_shutdown_function error page
 * cannot run here, because the process that would send it is gone.
 */

ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

require __DIR__ . '/laravel_stdout.php';
$stdout = hamster_open_protocol_stdout();
require __DIR__ . '/protocol.php';
require __DIR__ . '/adapter_support.php';
require __DIR__ . '/psr7.php';

use Psr\Http\Message\ServerRequestFactoryInterface;
use Psr\Http\Message\StreamFactoryInterface;
use Psr\Http\Message\UploadedFileFactoryInterface;
use Psr\Http\Server\RequestHandlerInterface;

function hamster_psr15_fail(string $message): never
{
    fwrite(STDERR, "psr-15 worker: {$message}\n");
    exit(2);
}

$entry = getenv('HAMSTER_PSR15_ENTRY');
if ($entry === false || $entry === '' || ! is_file($entry)) {
    hamster_psr15_fail('HAMSTER_PSR15_ENTRY must name the application entry file');
}

$application = null;
$startupError = null;
$strayBoot = hamster_guard_output(static function () use ($entry, &$application, &$startupError): void {
    try {
        $application = require $entry;
    } catch (Throwable $error) {
        $startupError = $error;
    }
});
if ($startupError !== null) {
    hamster_psr15_fail('bootstrap failed: ' . get_class($startupError) . ': ' . $startupError->getMessage());
}
if ($strayBoot !== '') {
    fwrite(STDERR, 'psr-15 worker: discarded ' . strlen($strayBoot) . " bytes printed during bootstrap\n");
}
$expected = [
    'handler' => RequestHandlerInterface::class,
    'request_factory' => ServerRequestFactoryInterface::class,
    'stream_factory' => StreamFactoryInterface::class,
    'uploaded_file_factory' => UploadedFileFactoryInterface::class,
];
foreach ($expected as $key => $interface) {
    if (! is_array($application) || ! ($application[$key] ?? null) instanceof $interface) {
        hamster_psr15_fail("the entry file must return '{$key}' implementing {$interface}");
    }
}
$reset = $application['reset'] ?? null;
if ($reset !== null && ! is_callable($reset)) {
    hamster_psr15_fail("the entry file's 'reset' must be callable");
}

hamster_disable_application_stdout();

$stdin = hamster_open_worker_stdin();
hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        if (strlen($frame) !== 1) {
            throw new RuntimeException('shutdown frame has trailing bytes');
        }
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $decoded = hamster_decode_request($frame);
    try {
        $request = hamster_psr15_request(
            $decoded,
            $application['request_factory'],
            $application['stream_factory'],
            $application['uploaded_file_factory']
        );
    } catch (HamsterBadRequest $error) {
        hamster_discard_uploads();
        fwrite(STDERR, 'psr-15 worker: rejected malformed request: ' . $error->getMessage() . "\n");
        hamster_write_frame($stdout, hamster_plain_response_frame($decoded['id'], 400, 'Bad Request'));
        continue;
    }

    $response = null;
    $body = '';
    $failure = null;
    $echoed = hamster_guard_output(static function () use ($application, $request, $reset, &$response, &$body, &$failure): void {
        try {
            $response = $application['handler']->handle($request);
            $body = (string) $response->getBody();
            if ($reset !== null) {
                $reset();
            }
            hamster_psr15_end_native_session();
            hamster_discard_uploads();
        } catch (Throwable $error) {
            $failure = $error;
        }
    });
    if ($failure !== null) {
        fwrite(STDERR, 'psr-15 worker: request failed outside the application error handler: '
            . get_class($failure) . ': ' . $failure->getMessage() . "\n");
        exit(70);
    }

    hamster_write_frame($stdout, hamster_response_frame(
        $decoded['id'],
        $response->getStatusCode(),
        hamster_psr15_headers($response),
        $echoed . $body,
        'psr-15 worker'
    ));
    unset($request, $response, $body, $decoded);
}
