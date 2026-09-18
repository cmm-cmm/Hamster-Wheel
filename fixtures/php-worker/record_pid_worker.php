<?php

declare(strict_types=1);

/**
 * Test-only wrapper that records the child PID before entering the ordinary
 * protocol-v1 fixture. Multi-pool startup tests can therefore prove that a
 * child created before a later pool failed was actually reaped.
 */

$marker = getenv('HAMSTER_TEST_PID_FILE');
if (!is_string($marker) || $marker === '') {
    fwrite(STDERR, "HAMSTER_TEST_PID_FILE is required\n");
    exit(2);
}

file_put_contents($marker, (string) getmypid() . PHP_EOL, FILE_APPEND | LOCK_EX);
require __DIR__ . '/worker.php';
