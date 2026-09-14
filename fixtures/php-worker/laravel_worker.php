<?php

declare(strict_types=1);

/**
 * Persistent Laravel worker for Hamster protocol v1.
 *
 * Laravel boots once, before READY is sent, so the pool never reports a worker
 * ready until it can actually serve. Every request afterwards reuses that
 * application and is followed by a request-boundary reset.
 *
 * Configured entirely through the worker's environment, so the server needs no
 * knowledge of Laravel:
 *
 *   HAMSTER_LARAVEL_PATH   required; the application root holding
 *                          vendor/autoload.php and bootstrap/app.php
 *   HAMSTER_LARAVEL_RESET  optional; "targeted" (default) or "none"
 *   HAMSTER_LARAVEL_WARMUP optional; warm-up requests before READY, default 2
 *
 * Protocol v1 is used exactly as it is: no new frame kinds and no new limits.
 */

// Standard output carries protocol frames and nothing else. PHP's own
// diagnostics go to stderr, which Hamster drains, because a single deprecation
// notice printed before the first frame corrupts the stream and the pool sees a
// nonsense frame length. PHP 8.5 emits such notices for real application code.
ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

require __DIR__ . '/protocol.php';
require __DIR__ . '/laravel_request.php';
require __DIR__ . '/laravel_reset.php';

use Illuminate\Contracts\Http\Kernel as HttpKernel;
use Symfony\Component\HttpFoundation\BinaryFileResponse;
use Symfony\Component\HttpFoundation\StreamedResponse;

/** Headers the server owns; a worker that sends them is rejected with 502. */
const HAMSTER_FORBIDDEN_RESPONSE_HEADERS = [
    'connection' => true,
    'content-length' => true,
    'keep-alive' => true,
    'proxy-authenticate' => true,
    'proxy-authorization' => true,
    'server' => true,
    'te' => true,
    'trailer' => true,
    'transfer-encoding' => true,
    'upgrade' => true,
];

function hamster_fail(string $message): never
{
    fwrite(STDERR, "laravel worker: {$message}\n");
    exit(2);
}

$appPath = getenv('HAMSTER_LARAVEL_PATH');
if ($appPath === false || $appPath === '') {
    hamster_fail('HAMSTER_LARAVEL_PATH is not set');
}
$appPath = rtrim($appPath, "/\\");
foreach (['/vendor/autoload.php', '/bootstrap/app.php'] as $required) {
    if (! is_file($appPath . $required)) {
        hamster_fail("HAMSTER_LARAVEL_PATH is missing {$required}");
    }
}

$strategy = getenv('HAMSTER_LARAVEL_RESET') ?: 'targeted';
// At least one warm-up request is mandatory, not a tuning knob: it is what
// boots the application and what makes the reset snapshot describe the state
// the worker actually returns to between requests.
$warmupRequests = max(1, (int) (getenv('HAMSTER_LARAVEL_WARMUP') ?: '2'));

/** @return list<array{0:string,1:string}> */
function hamster_response_headers($response): array
{
    $headers = [];
    foreach ($response->headers->allPreserveCase() as $name => $values) {
        if (isset(HAMSTER_FORBIDDEN_RESPONSE_HEADERS[strtolower($name)])) {
            continue;
        }
        foreach ($values as $value) {
            $headers[] = [$name, (string) $value];
        }
    }

    return $headers;
}

function hamster_response_body($response): string
{
    // A streamed or file-backed response has no content to read: its bytes
    // only exist while sendContent() runs. Capturing that is also what keeps
    // a download honest after Router::prepareResponse() has applied a Range
    // header, because sendContent() sends exactly the range it computed while
    // the file itself is the whole file.
    if ($response instanceof StreamedResponse || $response instanceof BinaryFileResponse) {
        ob_start();
        $response->sendContent();

        return (string) ob_get_clean();
    }

    return (string) $response->getContent();
}

/** Encodes a response the worker produced itself, without involving Laravel. */
function hamster_worker_response(int $id, int $status, string $body): string
{
    return hamster_encode_response([
        'id' => $id,
        'status' => $status,
        'headers' => [['Content-Type', 'text/plain; charset=UTF-8']],
        'body' => $body,
    ]);
}

require $appPath . '/vendor/autoload.php';

$app = require $appPath . '/bootstrap/app.php';
$kernel = $app->make(HttpKernel::class);

/**
 * Warm before READY, and let the first warm-up request perform the bootstrap.
 *
 * Bootstrapping eagerly here instead would run every service provider with no
 * request bound in the container, which `public/index.php` never does: it binds
 * the request first and bootstraps inside `handle()`. An application whose
 * providers touch `request` during boot fails outright under the eager order.
 *
 * Nothing is allowed to reach standard output during this phase either. A
 * bootstrap exception renders to stdout by default, and those bytes would be
 * read as a frame length.
 */
ob_start();
try {
    for ($index = 0; $index < $warmupRequests; $index++) {
        $warmRequest = hamster_build_request([
            'method' => 'GET', 'uri' => '/up', 'query' => '', 'headers' => [], 'body' => '',
        ]);
        $warmResponse = $kernel->handle($warmRequest);
        $warmStatus = $warmResponse->getStatusCode();
        $kernel->terminate($warmRequest, $warmResponse);
        unset($warmRequest, $warmResponse);
    }
} catch (Throwable $error) {
    $strayBoot = (string) ob_get_clean();
    hamster_fail(
        'warm-up failed: ' . get_class($error) . ': ' . $error->getMessage()
        . ($strayBoot === '' ? '' : "\n" . substr($strayBoot, 0, 2000))
    );
}
$strayBoot = (string) ob_get_clean();
if ($strayBoot !== '') {
    fwrite(STDERR, 'laravel worker: discarded ' . strlen($strayBoot) . " bytes printed during warm-up\n");
}
if ($warmStatus >= 500) {
    hamster_fail("warm-up request to /up returned {$warmStatus}; the application is not serving");
}

$reset = hamster_make_reset($strategy, $app);
$reset();

$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
if ($stdout === false) {
    hamster_fail('cannot open standard output');
}

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
        $request = hamster_build_request($decoded);
    } catch (HamsterBadRequest $error) {
        // A body this worker cannot parse is the client's error, not a reason
        // to lose a booted application. Answer it and keep serving.
        hamster_discard_uploads();
        fwrite(STDERR, 'laravel worker: rejected a malformed request: ' . $error->getMessage() . "\n");
        hamster_write_frame($stdout, hamster_worker_response($decoded['id'], 400, 'Bad Request'));
        $reset();
        continue;
    }

    // Anything the application prints belongs in the response body or nowhere;
    // it must never reach the frame stream.
    ob_start();
    try {
        $response = $kernel->handle($request);
        $headers = hamster_response_headers($response);
        $body = hamster_response_body($response);
        $status = $response->getStatusCode();
    } finally {
        $stray = (string) ob_get_clean();
    }
    if ($stray !== '') {
        fwrite(STDERR, 'laravel worker: discarded ' . strlen($stray) . " bytes written to stdout\n");
    }

    // Answer the client first, then clean up. This generation owns the worker
    // exclusively, so the reset always completes before the next request can be
    // assigned and never overlaps one.
    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $decoded['id'],
        'status' => $status,
        'headers' => $headers,
        'body' => $body,
    ]));

    $kernel->terminate($request, $response);
    // PHP deletes its own upload temporaries when the SAPI ends a request.
    // Nothing ends here, so a worker that skipped this would leak one file per
    // upload for the life of the process.
    hamster_discard_uploads();
    unset($request, $response, $decoded, $headers, $body, $stray);
    $reset();
}
