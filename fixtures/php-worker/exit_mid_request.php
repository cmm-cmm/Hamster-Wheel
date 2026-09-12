<?php

declare(strict_types=1);

require __DIR__ . '/protocol.php';

$stdin = hamster_open_worker_stdin();
$stdout = fopen('php://stdout', 'wb');
if ($stdout === false) {
    fwrite(STDERR, "cannot open stdout\n");
    exit(2);
}

hamster_send_ready($stdout);

// Consume only the frame prefix, then exit while Rust is still writing the
// large payload. This fixture verifies that a broken worker pipe fails closed.
if (hamster_read_exact($stdin, 4) === null) {
    exit(40);
}
exit(41);
