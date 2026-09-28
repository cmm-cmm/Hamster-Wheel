<?php

declare(strict_types=1);

/**
 * Worker entry for the native application: no framework, a Composer PSR-4
 * autoloader, and Hamster's protocol spoken directly through the repository's
 * adapter support files (HAMSTER_ADAPTER_DIR, normally fixtures/php-worker).
 *
 * The application is booted once and serves every request of this process.
 * It deliberately has endpoints a real application would not (fatal errors,
 * memory exhaustion, writes to stdout) so the server's side of those failures
 * can be tested.
 */

ini_set('display_errors', 'stderr');
ini_set('log_errors', '0');
ini_set('html_errors', '0');

$adapterDir = getenv('HAMSTER_ADAPTER_DIR');
if ($adapterDir === false || ! is_file($adapterDir . '/protocol.php')) {
    fwrite(STDERR, "native app: HAMSTER_ADAPTER_DIR must name fixtures/php-worker\n");
    exit(2);
}
require $adapterDir . '/laravel_stdout.php';
$stdout = hamster_open_protocol_stdout();
require $adapterDir . '/protocol.php';
require $adapterDir . '/adapter_support.php';
require __DIR__ . '/vendor/autoload.php';

use NativeApp\Application;
use NativeApp\Request;

$application = new Application(__DIR__ . '/var');
hamster_disable_application_stdout();

$stdin = hamster_open_worker_stdin();
hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $decoded = hamster_decode_request($frame);
    try {
        $parts = hamster_request_parts(
            $decoded,
            static fn (string $name, string $content, ?string $type): array => $name === '' && $content === ''
                ? ['name' => '', 'type' => '', 'tmp_name' => '', 'error' => UPLOAD_ERR_NO_FILE, 'size' => 0]
                : ['name' => $name, 'type' => (string) $type, 'tmp_name' => hamster_store_upload($content), 'error' => UPLOAD_ERR_OK, 'size' => strlen($content)]
        );
    } catch (HamsterBadRequest $error) {
        hamster_discard_uploads();
        hamster_write_frame($stdout, hamster_plain_response_frame($decoded['id'], 400, 'Bad Request'));
        continue;
    }

    $response = null;
    $echoed = hamster_guard_output(static function () use ($application, $decoded, $parts, &$response): void {
        $response = $application->handle(new Request($decoded, $parts));
    });
    hamster_discard_uploads();
    [$status, $headers, $body] = $response;
    if ($echoed !== '') {
        $headers[] = ['X-Discarded-Output', (string) strlen($echoed)];
    }
    hamster_write_frame($stdout, hamster_response_frame($decoded['id'], $status, $headers, $body, 'native app'));
}
