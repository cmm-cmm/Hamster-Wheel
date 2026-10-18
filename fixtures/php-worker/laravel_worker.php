<?php

declare(strict_types=1);

/**
 * Persistent Laravel worker for Hamster protocol v2.
 *
 * Laravel Octane owns application/container request isolation. Hamster owns
 * protocol framing, request reconstruction, upload temporaries and the final
 * external-resource boundary. The response remains staged until every cleanup
 * step succeeds, so Rust cannot redispatch work to a contaminated generation.
 *
 * HAMSTER_LARAVEL_PATH is required. The application must require
 * laravel/octane and retain Octane's default lifecycle listeners.
 */

ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

require __DIR__ . '/laravel_stdout.php';
$stdout = hamster_open_protocol_stdout();
require __DIR__ . '/protocol.php';
require __DIR__ . '/laravel_request.php';

use Illuminate\Support\Once;
use Laravel\Octane\ApplicationFactory;
use Laravel\Octane\RequestContext;
use Laravel\Octane\Worker;

function hamster_fail(string $message): never
{
    fwrite(STDERR, "laravel worker: {$message}\n");
    exit(2);
}

function hamster_worker_response(int $id, int $status, string $body): string
{
    return hamster_encode_response([
        'id' => $id,
        'status' => $status,
        'headers' => [['Content-Type', 'text/plain; charset=UTF-8']],
        'body' => $body,
    ]);
}

$appPath = getenv('HAMSTER_LARAVEL_PATH');
if ($appPath === false || $appPath === '') {
    hamster_fail('HAMSTER_LARAVEL_PATH is not set');
}
$appPath = rtrim($appPath, "/\\");
if (($legacyReset = getenv('HAMSTER_LARAVEL_RESET')) !== false && $legacyReset !== '' && $legacyReset !== 'octane') {
    hamster_fail('HAMSTER_LARAVEL_RESET no longer accepts unsafe reset strategies; remove it or use "octane"');
}
foreach (['/vendor/autoload.php', '/bootstrap/app.php'] as $required) {
    if (! is_file($appPath . $required)) {
        hamster_fail("HAMSTER_LARAVEL_PATH is missing {$required}");
    }
}

require $appPath . '/vendor/autoload.php';
if (! class_exists(Worker::class)) {
    hamster_fail('laravel/octane is required by the persistent Laravel worker');
}
require __DIR__ . '/laravel_lifecycle.php';
require __DIR__ . '/laravel_octane_client.php';

$client = new HamsterOctaneClient($stdout);
$worker = new Worker(new ApplicationFactory($appPath), $client);
$startupError = null;
$strayBoot = hamster_guard_output(static function () use ($worker, &$startupError): void {
    try {
        $worker->boot();
    } catch (Throwable $error) {
        $startupError = $error;
    }
});
if ($startupError !== null) {
    hamster_fail(
        'bootstrap failed: ' . get_class($startupError) . ': ' . $startupError->getMessage()
        . ($strayBoot === '' ? '' : "\n" . substr($strayBoot, 0, 2000))
    );
}
if ($strayBoot !== '') {
    fwrite(STDERR, 'laravel worker: discarded ' . strlen($strayBoot) . " bytes printed during bootstrap\n");
}

// Symfony Console needs stdout while the application boots. From this point
// onward fd 1 is a null sink and only the private duplicate can carry frames.
hamster_disable_application_stdout();

$app = $worker->application();
try {
    hamster_validate_octane_lifecycle($app);
    $lifecycle = new HamsterLaravelLifecycle($app);
    Once::flush();
} catch (Throwable $error) {
    hamster_fail('invalid Laravel lifecycle: ' . $error->getMessage());
}

$stdin = hamster_open_worker_stdin();
hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        if (strlen($frame) !== 1) {
            throw new RuntimeException('shutdown frame has trailing bytes');
        }
        $stray = hamster_guard_output(static fn () => $worker->terminate());
        if ($stray !== '') {
            fwrite(STDERR, 'laravel worker: discarded output during worker termination\n');
        }
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $decoded = hamster_decode_request($frame);
    try {
        $request = hamster_build_request($decoded);
    } catch (HamsterBadRequest $error) {
        hamster_discard_uploads();
        fwrite(STDERR, 'laravel worker: rejected malformed request: ' . $error->getMessage() . "\n");
        hamster_write_frame($stdout, hamster_worker_response($decoded['id'], 400, 'Bad Request'));
        continue;
    } catch (Throwable $error) {
        hamster_discard_uploads();
        throw $error;
    }

    $stray = '';
    $guardLevel = ob_get_level();
    ob_start();
    try {
        $worker->handle($request, new RequestContext([
            'id' => $decoded['id'],
            'request' => $request,
        ]));
        $lifecycle->completeRequest();
        hamster_discard_uploads();
        if (! $client->shouldStop()) {
            // The response is the commit point visible to Rust. Everything
            // which can poison this application generation has completed
            // first. A retiring client deliberately leaves the response
            // uncommitted; EOF makes Rust return 502 without replaying the
            // possibly-started request.
            $client->markCleanupCompleted();
            $client->flushResponse();
        }
    } finally {
        hamster_discard_uploads();
        $chunks = [];
        while (ob_get_level() > $guardLevel) {
            $chunk = ob_get_clean();
            if (is_string($chunk) && $chunk !== '') {
                $chunks[] = $chunk;
            }
        }
        $stray = implode('', array_reverse($chunks));
        unset($request, $decoded);
    }
    if ($stray !== '') {
        fwrite(STDERR, 'laravel worker: discarded ' . strlen($stray) . " bytes written during lifecycle cleanup\n");
    }
    if ($client->shouldStop()) {
        hamster_fail('Octane retired the worker after a lifecycle error');
    }
}

$stray = hamster_guard_output(static fn () => $worker->terminate());
if ($stray !== '') {
    fwrite(STDERR, 'laravel worker: discarded output during worker termination\n');
}
