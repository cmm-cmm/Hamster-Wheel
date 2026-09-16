<?php

declare(strict_types=1);

/**
 * Reserve a duplicate of fd 1 for protocol frames.
 *
 * In CLI PHP, STDOUT owns the first stdout stream and subsequent
 * `php://stdout` opens duplicate fd 1. Laravel still needs its ordinary stdout
 * while bootstrapping Symfony Console; call
 * hamster_disable_application_stdout() after boot and before READY.
 *
 * @return resource
 */
function hamster_open_protocol_stdout()
{
    $protocol = fopen('php://stdout', 'wb');
    if ($protocol === false) {
        throw new RuntimeException('cannot duplicate standard output for protocol');
    }

    return $protocol;
}

/**
 * Remove application access to the protocol pipe and occupy fd 1 with a null
 * sink. The static keeps the sink alive for the rest of the process.
 */
function hamster_disable_application_stdout(): void
{
    static $sink = null;
    if ($sink !== null) {
        return;
    }
    if (! fclose(STDOUT)) {
        throw new RuntimeException('cannot close application standard output');
    }
    $sink = fopen(PHP_OS_FAMILY === 'Windows' ? 'NUL' : '/dev/null', 'wb');
    if ($sink === false) {
        throw new RuntimeException('cannot redirect application standard output');
    }
}
