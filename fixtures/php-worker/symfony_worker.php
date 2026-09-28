<?php

declare(strict_types=1);

/**
 * Persistent worker for applications built on Symfony's HttpKernel.
 *
 * Symfony supports long-running execution itself: Kernel::handle() resets
 * every service tagged `kernel.reset` (through `services_resetter`) before the
 * next request, and the session listener reads the session cookie from the
 * Request and writes it to the Response instead of relying on the SAPI. This
 * adapter therefore adds no reset of its own. It owns what a SAPI would: the
 * request superglobal work (adapter_support.php), the protocol pipe, upload
 * temporaries, and the commit point.
 *
 * Environment:
 *   HAMSTER_SYMFONY_PATH    project directory (required)
 *   HAMSTER_SYMFONY_KERNEL  kernel class, default App\Kernel
 *   APP_ENV / APP_DEBUG     as for Symfony Runtime; `.env` files are loaded
 *                           with symfony/dotenv when it is installed, and real
 *                           environment variables take precedence.
 *
 * The kernel boots before READY, so its container cache must already be warm
 * for APP_ENV (`bin/console cache:warmup`): the pool allows five seconds.
 *
 * Commit point: kernel.terminate listeners and upload cleanup run before the
 * RESPONSE frame is written, so the server never hands the next request to a
 * worker whose previous request has not finished. The cost is that terminate
 * work (e-mail spooling, for example) delays the response. If handling or
 * termination throws, no frame is written and the worker exits: the server
 * answers 502 and replaces it, and never retries the request.
 */

ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

require __DIR__ . '/laravel_stdout.php';
$stdout = hamster_open_protocol_stdout();
require __DIR__ . '/protocol.php';
require __DIR__ . '/adapter_support.php';
require __DIR__ . '/httpfoundation.php';

use Symfony\Component\Dotenv\Dotenv;
use Symfony\Component\HttpFoundation\File\UploadedFile;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpKernel\HttpKernelInterface;
use Symfony\Component\HttpKernel\TerminableInterface;

function hamster_symfony_fail(string $message): never
{
    fwrite(STDERR, "symfony worker: {$message}\n");
    exit(2);
}

$projectDir = getenv('HAMSTER_SYMFONY_PATH');
if ($projectDir === false || $projectDir === '') {
    hamster_symfony_fail('HAMSTER_SYMFONY_PATH is not set');
}
$projectDir = rtrim($projectDir, "/\\");
if (! is_file($projectDir . '/vendor/autoload.php')) {
    hamster_symfony_fail('HAMSTER_SYMFONY_PATH has no vendor/autoload.php');
}
require $projectDir . '/vendor/autoload.php';

if (class_exists(Dotenv::class) && is_file($projectDir . '/.env')) {
    (new Dotenv())->bootEnv($projectDir . '/.env');
}
$environment = (string) ($_SERVER['APP_ENV'] ?? $_ENV['APP_ENV'] ?? 'prod');
$debug = filter_var($_SERVER['APP_DEBUG'] ?? $_ENV['APP_DEBUG'] ?? ($environment !== 'prod'), FILTER_VALIDATE_BOOL);
$kernelClass = getenv('HAMSTER_SYMFONY_KERNEL') ?: 'App\\Kernel';
if (! class_exists($kernelClass)) {
    hamster_symfony_fail("kernel class {$kernelClass} is not autoloadable");
}

$kernel = null;
$startupError = null;
$strayBoot = hamster_guard_output(static function () use (&$kernel, &$startupError, $kernelClass, $environment, $debug): void {
    try {
        $kernel = new $kernelClass($environment, $debug);
        if (! $kernel instanceof HttpKernelInterface) {
            throw new UnexpectedValueException("{$kernelClass} is not an HttpKernelInterface");
        }
        if (method_exists($kernel, 'boot')) {
            $kernel->boot();
        }
    } catch (Throwable $error) {
        $startupError = $error;
    }
});
if ($startupError !== null) {
    hamster_symfony_fail('bootstrap failed: ' . get_class($startupError) . ': ' . $startupError->getMessage());
}
if ($strayBoot !== '') {
    fwrite(STDERR, 'symfony worker: discarded ' . strlen($strayBoot) . " bytes printed during bootstrap\n");
}

// From here fd 1 is a null sink; only the private duplicate carries frames.
hamster_disable_application_stdout();

$stdin = hamster_open_worker_stdin();
hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        if (strlen($frame) !== 1) {
            throw new RuntimeException('shutdown frame has trailing bytes');
        }
        hamster_guard_output(static function () use ($kernel): void {
            if (method_exists($kernel, 'shutdown')) {
                $kernel->shutdown();
            }
        });
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $decoded = hamster_decode_request($frame);
    try {
        $request = hamster_httpfoundation_request($decoded, Request::class, UploadedFile::class);
    } catch (HamsterBadRequest $error) {
        hamster_discard_uploads();
        fwrite(STDERR, 'symfony worker: rejected malformed request: ' . $error->getMessage() . "\n");
        hamster_write_frame($stdout, hamster_plain_response_frame($decoded['id'], 400, 'Bad Request'));
        continue;
    }

    $response = null;
    $body = '';
    $failure = null;
    // Output a controller echoes would, under a SAPI, precede the response
    // body. Keep that order for ordinary responses; a streamed response's
    // own output is its body.
    $echoed = hamster_guard_output(static function () use ($kernel, $request, &$response, &$body, &$failure): void {
        try {
            $response = $kernel->handle($request);
            $body = hamster_response_body($response);
            if ($kernel instanceof TerminableInterface) {
                $kernel->terminate($request, $response);
            }
            hamster_discard_uploads();
        } catch (Throwable $error) {
            $failure = $error;
        }
    });
    if ($failure !== null) {
        // Possibly-started and possibly half-applied: no frame, exit. The
        // server answers 502 and starts a clean generation.
        fwrite(STDERR, 'symfony worker: request failed outside the kernel error handler: '
            . get_class($failure) . ': ' . $failure->getMessage() . "\n");
        exit(70);
    }
    if ($echoed !== '') {
        $body = $echoed . $body;
    }

    hamster_write_frame($stdout, hamster_response_frame(
        $decoded['id'],
        $response->getStatusCode(),
        hamster_response_headers($response),
        $body,
        'symfony worker'
    ));
    unset($request, $response, $body, $decoded);
}
