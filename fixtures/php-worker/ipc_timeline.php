<?php

declare(strict_types=1);

require __DIR__ . '/protocol.php';

$mode = $argv[1] ?? 'default';
$context = $mode === 'pipe_blocking'
    ? stream_context_create(['pipe' => ['blocking' => true]])
    : null;
$stdin = $context === null
    ? fopen('php://stdin', 'rb')
    : fopen('php://stdin', 'rb', false, $context);
$stdout = fopen('php://stdout', 'wb');
if ($stdin === false || $stdout === false) {
    fwrite(STDERR, "cannot open standard streams\n");
    exit(2);
}

hamster_send_ready($stdout);

while (($frame = hamster_read_frame($stdin)) !== null) {
    $t3 = hrtime(true);
    $kind = ord($frame[0]);
    if ($kind === HAMSTER_SHUTDOWN) {
        hamster_send_shutdown_ack($stdout);
        exit(0);
    }

    $request = hamster_decode_request($frame);
    $t4 = hrtime(true);
    $response = [
        'id' => $request['id'],
        'status' => 200,
        'headers' => [['content-type', 'application/octet-stream']],
        'body' => $request['body'],
    ];
    $t5 = hrtime(true);
    $payload = hamster_encode_response($response);
    $t6 = hrtime(true);
    hamster_write_frame($stdout, $payload);
    $t7 = hrtime(true);

    fwrite(
        STDERR,
        sprintf(
            "IPC_TIMING,id=%d,decode_ns=%d,handler_ns=%d,encode_ns=%d,write_ns=%d,php_total_ns=%d\n",
            $request['id'],
            $t4 - $t3,
            $t5 - $t4,
            $t6 - $t5,
            $t7 - $t6,
            $t7 - $t3
        )
    );
    fflush(STDERR);
}
