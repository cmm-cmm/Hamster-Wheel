<?php

declare(strict_types=1);

// Dedicated scratch deployment only; never reads application/database data.
require __DIR__ . '/protocol.php';
$state = getenv('HAMSTER_SERVICE_TEST_STATE');
if (!is_string($state) || !is_dir($state)) {
    exit(2);
}
if (($argv[1] ?? '') === 'descendant') {
    $release = $state . '/descendant-release-' . $argv[3];
    while (!file_exists($release)) {
        clearstatcache(true, $release);
        usleep(1000);
    }
    exit(0);
}
$descendant = proc_open(
    [PHP_BINARY, __FILE__, 'descendant', $state, (string) getmypid()],
    [0 => ['file', 'NUL', 'r'], 1 => ['file', 'NUL', 'a'], 2 => ['file', 'NUL', 'a']],
    $pipes,
    null,
    null,
    ['bypass_shell' => true]
);
if (!is_resource($descendant)) {
    exit(3);
}
$child = proc_get_status($descendant);
file_put_contents($state . '/worker-' . getmypid() . '.json', json_encode([
    'worker' => getmypid(), 'descendant' => $child['pid'],
], JSON_THROW_ON_ERROR));
$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
hamster_send_ready($stdout);
$count = 0;
while (($frame = hamster_read_frame($stdin)) !== null) {
    if (ord($frame[0]) === HAMSTER_SHUTDOWN) {
        file_put_contents($state . '/descendant-release-' . getmypid(), 'release');
        proc_close($descendant);
        hamster_send_shutdown_ack($stdout);
        file_put_contents($state . '/shutdown-' . getmypid(), 'ack');
        exit(0);
    }
    $request = hamster_decode_request($frame);
    parse_str($request['query'], $query);
    if (str_ends_with($request['path'], '/wait')) {
        $id = $query['id'] ?? '';
        if (!is_string($id) || !preg_match('/\A[a-z0-9]{1,24}\z/D', $id)) {
            exit(4);
        }
        file_put_contents($state . '/request-' . $id . '-started', (string) getmypid());
        $release = $state . '/request-' . $id . '-release';
        while (!file_exists($release)) {
            clearstatcache(true, $release);
            usleep(1000);
        }
        file_put_contents($state . '/request-' . $id . '-completed', (string) getmypid());
    }
    if (str_ends_with($request['path'], '/logs')) {
        // Enough scratch stderr to exercise eight 64-KiB WinSW rotations.
        for ($i = 0; $i < 7000; $i++) {
            fwrite(STDERR, 'qualification-log-' . $i . ' ' . str_repeat('x', 128) . PHP_EOL);
        }
    }
    hamster_write_frame($stdout, hamster_encode_response([
        'id' => $request['id'], 'status' => 200,
        'headers' => [['content-type', 'application/json']],
        'body' => json_encode(['sapi' => PHP_SAPI, 'pid' => getmypid(), 'count' => ++$count], JSON_THROW_ON_ERROR),
    ]));
}
file_put_contents($state . '/descendant-release-' . getmypid(), 'release');
proc_close($descendant);
