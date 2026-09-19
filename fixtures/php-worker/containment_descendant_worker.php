<?php

declare(strict_types=1);

/**
 * Starts one ordinary direct descendant through proc_open, records both exact
 * PIDs, then enters the normal protocol-v2 worker loop.
 */

$workerMarker = getenv('HAMSTER_TEST_PID_FILE');
$descendantMarker = getenv('HAMSTER_TEST_DESCENDANT_PID_FILE');
$descendantExecutable = getenv('HAMSTER_TEST_DESCENDANT_EXECUTABLE');
if (!is_string($workerMarker) || $workerMarker === ''
    || !is_string($descendantMarker) || $descendantMarker === ''
    || !is_string($descendantExecutable) || $descendantExecutable === '') {
    fwrite(STDERR, "containment descendant environment is incomplete\n");
    exit(2);
}

$descriptorSpec = [
    0 => ['pipe', 'r'],
    1 => ['file', 'NUL', 'a'],
    2 => ['file', 'NUL', 'a'],
];
$descendant = proc_open(
    [$descendantExecutable, '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 300'],
    $descriptorSpec,
    $descendantPipes,
    null,
    null,
    ['bypass_shell' => true]
);
if (!is_resource($descendant)) {
    fwrite(STDERR, "cannot start containment descendant\n");
    exit(3);
}
foreach ($descendantPipes as $pipe) {
    fclose($pipe);
}
$status = proc_get_status($descendant);
if (!is_array($status) || !isset($status['pid']) || (int) $status['pid'] <= 0) {
    fwrite(STDERR, "cannot identify containment descendant\n");
    exit(4);
}

file_put_contents($workerMarker, (string) getmypid() . PHP_EOL, FILE_APPEND | LOCK_EX);
file_put_contents($descendantMarker, (string) (int) $status['pid'] . PHP_EOL, FILE_APPEND | LOCK_EX);

require __DIR__ . '/worker.php';
