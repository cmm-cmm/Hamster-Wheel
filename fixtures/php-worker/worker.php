<?php

declare(strict_types=1);

require __DIR__ . '/protocol.php';

/** @var FixtureApplication $application */
$application = require __DIR__ . '/bootstrap.php';
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
        if (strlen($frame) !== 1) {
            throw new RuntimeException('shutdown frame has trailing bytes');
        }
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $request = hamster_decode_request($frame);
    $application->requestCount++;

    if ($request['path'] === '/malformed') {
        hamster_write_frame($stdout, chr(99));
        continue;
    }
    if ($request['path'] === '/truncated') {
        hamster_write_all($stdout, pack('N', 32) . "short");
        exit(31);
    }

    parse_str($request['query'], $query);
    if ($request['path'] === '/delay') {
        $milliseconds = min(10_000, max(0, (int) ($query['ms'] ?? 0)));
        usleep($milliseconds * 1000);
    }

    $response = [
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'text/plain; charset=utf-8']],
        'body' => 'Hello from PHP',
    ];

    switch ($request['path']) {
        case '/state':
            $response['headers'] = [['content-type', 'application/json']];
            $response['body'] = json_encode([
                'pid' => getmypid(),
                'bootstrap' => $application->bootstrapCount,
                'request' => $application->requestCount,
            ], JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
            break;
        case '/json':
            $response['headers'] = [['content-type', 'application/json']];
            $response['body'] = json_encode(
                ['message' => 'Hello from PHP'],
                JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES
            );
            break;
        case '/inspect':
            $response['headers'] = [['content-type', 'application/json']];
            $response['body'] = json_encode([
                'method' => $request['method'],
                'uri' => $request['uri'],
                'path' => $request['path'],
                'query' => $request['query'],
                'headers' => $request['headers'],
                'body' => $request['body'],
            ], JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
            break;
        case '/echo':
            $response['status'] = 201;
            $response['headers'] = [
                ['content-type', 'application/octet-stream'],
                ['x-php-worker', 'persistent'],
                ['x-repeat', 'one'],
                ['x-repeat', 'two'],
            ];
            $response['body'] = $request['body'];
            break;
        case '/large':
            $response['headers'] = [['content-type', 'application/octet-stream']];
            $response['body'] = $request['body'];
            break;
        case '/delay':
            $response['body'] = 'delayed';
            break;
        case '/exit-after-response':
            $response['body'] = 'exiting';
            break;
    }

    hamster_write_frame($stdout, hamster_encode_response($response));
    if ($request['path'] === '/exit-after-response') {
        exit(0);
    }
}
