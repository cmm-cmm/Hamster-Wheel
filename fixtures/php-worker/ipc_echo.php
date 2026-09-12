<?php

declare(strict_types=1);

const IPC_MAX_FRAME = 2 * 1024 * 1024;

$mode = $argv[1] ?? 'default';
$context = $mode === 'pipe_blocking'
    ? stream_context_create(['pipe' => ['blocking' => true]])
    : null;
$stdin = $context === null
    ? fopen('php://stdin', 'rb')
    : fopen('php://stdin', 'rb', false, $context);
$stdout = fopen('php://stdout', 'wb');
if ($stdin === false || $stdout === false) {
    fwrite(STDERR, "IPC_ERROR,cannot_open_streams\n");
    exit(2);
}

$readBufferResult = null;
$writeBufferResult = null;
$blockingResult = null;
if ($mode === 'unbuffered') {
    $readBufferResult = stream_set_read_buffer($stdin, 0);
    $writeBufferResult = stream_set_write_buffer($stdout, 0);
} elseif ($mode === 'batched') {
    $writeBufferResult = stream_set_write_buffer($stdout, 8192);
} elseif ($mode === 'stream_blocking') {
    $blockingResult = stream_set_blocking($stdin, true);
}

$reads = 0;
$writes = 0;
$flushes = 0;
$requests = 0;

function ipc_read_exact($stream, int $length): ?string
{
    global $reads;
    $result = '';
    while (strlen($result) < $length) {
        $reads++;
        $chunk = fread($stream, $length - strlen($result));
        if ($chunk === false) {
            throw new RuntimeException('read failed');
        }
        if ($chunk === '') {
            if (feof($stream)) {
                return $result === '' ? null : throw new RuntimeException('truncated frame');
            }
            continue;
        }
        $result .= $chunk;
    }
    return $result;
}

function ipc_write_all($stream, string $bytes): void
{
    global $writes;
    $offset = 0;
    while ($offset < strlen($bytes)) {
        $writes++;
        $written = fwrite($stream, substr($bytes, $offset));
        if ($written === false || $written === 0) {
            throw new RuntimeException('write failed');
        }
        $offset += $written;
    }
}

function ipc_flush($stream): void
{
    global $flushes;
    $flushes++;
    if (!fflush($stream)) {
        throw new RuntimeException('flush failed');
    }
}

function ipc_read_frame($stream): ?string
{
    $prefix = ipc_read_exact($stream, 4);
    if ($prefix === null) {
        return null;
    }
    $length = unpack('Nlength', $prefix)['length'];
    if ($length > IPC_MAX_FRAME) {
        throw new RuntimeException("frame too large: {$length}");
    }
    return ipc_read_exact($stream, $length) ?? throw new RuntimeException('truncated payload');
}

function ipc_write_frame($stream, string $payload, string $mode): void
{
    $prefix = pack('N', strlen($payload));
    if ($mode === 'split') {
        ipc_write_all($stream, $prefix);
        if ($payload !== '') {
            ipc_write_all($stream, $payload);
        }
    } else {
        ipc_write_all($stream, $prefix . $payload);
    }
    if ($mode !== 'no_flush') {
        ipc_flush($stream);
    }
}

ipc_write_frame($stdout, 'READY', 'default');
$reads = $writes = $flushes = 0;

while (($payload = ipc_read_frame($stdin)) !== null) {
    $requests++;
    ipc_write_frame($stdout, $payload, $mode);
}

fwrite(
    STDERR,
    sprintf(
        "IPC_COUNTS,mode=%s,requests=%d,fread=%d,fwrite=%d,fflush=%d,read_buffer=%s,write_buffer=%s,stream_blocking=%s,ob_level=%d\n",
        $mode,
        $requests,
        $reads,
        $writes,
        $flushes,
        var_export($readBufferResult, true),
        var_export($writeBufferResult, true),
        var_export($blockingResult, true),
        ob_get_level()
    )
);
