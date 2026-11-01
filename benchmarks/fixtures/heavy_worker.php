<?php

declare(strict_types=1);

require dirname(__DIR__, 2) . '/fixtures/php-worker/protocol.php';
require __DIR__ . '/heavy_workload.php';
$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
if ($stdout === false) {
    throw new RuntimeException('cannot open benchmark stdout');
}
hamster_send_ready($stdout);
while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        if (strlen($frame) !== 1) {
            throw new RuntimeException('invalid shutdown frame');
        }
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }
    $request = hamster_decode_request($frame);
    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'text/plain; charset=utf-8']],
        'body' => benchmark_heavy_payload($request['path'], $request['body']),
    ]));
}
