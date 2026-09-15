<?php

declare(strict_types=1);

/**
 * Benchmark-only fresh-application baseline for protocol v1.
 *
 * This deliberately rebuilds Laravel for every request while keeping PHP,
 * OPcache, Rust HTTP handling and Hamster IPC alive. It is not a production
 * worker: unlike laravel_worker.php it cannot isolate direct fd 1 writes,
 * because Laravel/Symfony needs stdout during each bootstrap.
 */

ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

require __DIR__ . '/laravel_stdout.php';
$stdout = hamster_open_protocol_stdout();
require __DIR__ . '/protocol.php';
require __DIR__ . '/laravel_request.php';

use Illuminate\Container\Container;
use Illuminate\Contracts\Http\Kernel as HttpKernel;
use Illuminate\Support\Facades\Facade;
use Symfony\Component\HttpFoundation\BinaryFileResponse;
use Symfony\Component\HttpFoundation\StreamedResponse;

const HAMSTER_FRESH_FORBIDDEN_RESPONSE_HEADERS = [
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

/** @return list<array{0:string,1:string}> */
function hamster_fresh_response_headers($response): array
{
    $headers = [];
    foreach ($response->headers->allPreserveCase() as $name => $values) {
        if (isset(HAMSTER_FRESH_FORBIDDEN_RESPONSE_HEADERS[strtolower($name)])) {
            continue;
        }
        foreach ($values as $value) {
            $headers[] = [$name, (string) $value];
        }
    }

    return $headers;
}

function hamster_fresh_response_body($response): string
{
    if (! $response instanceof StreamedResponse && ! $response instanceof BinaryFileResponse) {
        return (string) $response->getContent();
    }

    $level = ob_get_level();
    ob_start();
    try {
        $response->sendContent();

        return (string) ob_get_contents();
    } finally {
        while (ob_get_level() > $level) {
            ob_end_clean();
        }
    }
}

$appPath = getenv('HAMSTER_LARAVEL_PATH');
if (! is_string($appPath) || $appPath === '') {
    fwrite(STDERR, "fresh Laravel benchmark worker: HAMSTER_LARAVEL_PATH is not set\n");
    exit(2);
}
$appPath = rtrim($appPath, "/\\");
require $appPath . '/vendor/autoload.php';

$stdin = hamster_open_worker_stdin();
hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $decoded = hamster_decode_request($frame);
    $request = hamster_build_request($decoded);
    $level = ob_get_level();
    $encoded = null;
    ob_start();
    try {
        $app = require $appPath . '/bootstrap/app.php';
        $kernel = $app->make(HttpKernel::class);
        $response = $kernel->handle($request);
        $headers = hamster_fresh_response_headers($response);
        $body = hamster_fresh_response_body($response);
        $status = $response->getStatusCode();
        $kernel->terminate($request, $response);
        hamster_discard_uploads();

        $encoded = hamster_encode_response([
            'id' => $decoded['id'],
            'status' => $status,
            'headers' => $headers,
            'body' => $body,
        ]);
    } finally {
        hamster_discard_uploads();
        while (ob_get_level() > $level) {
            ob_end_clean();
        }
        if (isset($app)) {
            $app->flush();
        }
        Facade::clearResolvedInstances();
        Container::setInstance(null);
        unset($app, $kernel, $request, $response, $decoded, $headers, $body, $status);
        gc_collect_cycles();
    }

    // Make the same response-visibility promise as the persistent worker:
    // termination and process-local cleanup complete before frame commit.
    hamster_write_frame($stdout, $encoded);
}
