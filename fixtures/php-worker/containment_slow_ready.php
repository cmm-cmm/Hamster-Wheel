<?php

declare(strict_types=1);

/**
 * Records the process before deliberately remaining in bootstrap. The parent
 * containment test terminates Hamster while this process has not sent READY.
 */

$marker = getenv('HAMSTER_TEST_PID_FILE');
if (!is_string($marker) || $marker === '') {
    fwrite(STDERR, "HAMSTER_TEST_PID_FILE is required\n");
    exit(2);
}

file_put_contents($marker, (string) getmypid() . PHP_EOL, FILE_APPEND | LOCK_EX);
sleep(300);
