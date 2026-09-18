<?php

declare(strict_types=1);

/**
 * A worker that reaches READY, serves normally, and then refuses to
 * acknowledge SHUTDOWN.
 *
 * It exists to make a pool's shutdown return an error, which is otherwise hard
 * to reach from outside. The pool gives a worker SHUTDOWN_TIMEOUT to answer,
 * then kills it and reports PhpWorkerError::Shutdown. That is the failure the
 * multi-pool investigation needs in order to observe what shutdown_workers
 * does with the pools it has not reached yet.
 */

require __DIR__ . '/protocol.php';

$pidMarker = getenv('HAMSTER_TEST_PID_FILE');
if (is_string($pidMarker) && $pidMarker !== '') {
    file_put_contents($pidMarker, (string) getmypid() . PHP_EOL, FILE_APPEND | LOCK_EX);
}

$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
if ($stdout === false) {
    fwrite(STDERR, "cannot open standard streams\n");
    exit(2);
}

hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    $kind = ord($frame[0]);
    if ($kind === HAMSTER_SHUTDOWN) {
        // Deliberately no SHUTDOWN_ACK. Sleep past the pool's shutdown budget
        // so it has to kill this process and report the failure.
        fwrite(STDERR, "ignoring SHUTDOWN on purpose\n");
        sleep(30);
        continue;
    }

    $request = hamster_decode_request($frame);
    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'text/plain; charset=utf-8']],
        'body' => 'stubborn',
    ]));
}
