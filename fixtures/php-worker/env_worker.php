<?php

declare(strict_types=1);

/**
 * Reports the worker process environment so the configured `php.env` map can be
 * observed from an HTTP response.
 *
 *   GET /env?name=FOO   -> the value of FOO, or "<unset>"
 *   GET /crash          -> exits, so a replacement generation can be inspected
 */

require __DIR__ . '/protocol.php';

$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
if ($stdout === false) {
    fwrite(STDERR, "cannot open standard output\n");
    exit(2);
}

hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $request = hamster_decode_request($frame);
    if ($request['path'] === '/crash') {
        exit(42);
    }

    parse_str($request['query'], $query);
    $name = (string) ($query['name'] ?? '');
    $value = $name === '' ? '<unset>' : getenv($name);

    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'text/plain; charset=utf-8']],
        'body' => $value === false ? '<unset>' : $value,
    ]));
}
