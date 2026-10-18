<?php

declare(strict_types=1);

// Deterministic process-level stop rehearsal; all paths belong to test scratch.
require __DIR__ . '/protocol.php';
$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
hamster_send_ready($stdout);
while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        $ack = getenv('HAMSTER_TEST_SHUTDOWN_ACK');
        if ($ack !== false) {
            file_put_contents($ack, (string) getmypid());
        }
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }
    $request = hamster_decode_request($frame);
    parse_str($request['query'], $query);
    if ($request['path'] === '/wait') {
        file_put_contents($query['started'], (string) getmypid());
        while (!file_exists($query['release'])) {
            clearstatcache(true, $query['release']);
            usleep(1000);
        }
        file_put_contents($query['completed'], (string) getmypid());
    }
    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'text/plain']],
        'body' => 'completed',
    ]));
}
