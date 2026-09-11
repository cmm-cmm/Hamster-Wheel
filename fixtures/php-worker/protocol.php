<?php

declare(strict_types=1);

const HAMSTER_PROTOCOL_VERSION = 1;
const HAMSTER_MAX_FRAME = 8 * 1024 * 1024;
const HAMSTER_MAX_HEADERS = 128;
const HAMSTER_READY = 1;
const HAMSTER_REQUEST = 2;
const HAMSTER_RESPONSE = 3;
const HAMSTER_SHUTDOWN = 4;
const HAMSTER_SHUTDOWN_ACK = 5;

function hamster_read_exact($stream, int $length): ?string
{
    $result = '';
    while (strlen($result) < $length) {
        $chunk = fread($stream, $length - strlen($result));
        if ($chunk === false) {
            throw new RuntimeException('stdin read failed');
        }
        if ($chunk === '') {
            if (feof($stream)) {
                return $result === '' ? null : throw new RuntimeException('truncated input');
            }
            continue;
        }
        $result .= $chunk;
    }
    return $result;
}

function hamster_write_all($stream, string $bytes): void
{
    $offset = 0;
    while ($offset < strlen($bytes)) {
        $written = fwrite($stream, substr($bytes, $offset));
        if ($written === false || $written === 0) {
            throw new RuntimeException('stdout write failed');
        }
        $offset += $written;
    }
    fflush($stream);
}

function hamster_read_frame($stream): ?string
{
    $prefix = hamster_read_exact($stream, 4);
    if ($prefix === null) {
        return null;
    }
    $length = unpack('Nlength', $prefix)['length'];
    if ($length < 1 || $length > HAMSTER_MAX_FRAME) {
        throw new RuntimeException("invalid frame length {$length}");
    }
    $payload = hamster_read_exact($stream, $length);
    if ($payload === null) {
        throw new RuntimeException('truncated frame');
    }
    return $payload;
}

function hamster_write_frame($stream, string $payload): void
{
    $length = strlen($payload);
    if ($length < 1 || $length > HAMSTER_MAX_FRAME) {
        throw new RuntimeException("invalid output frame length {$length}");
    }
    hamster_write_all($stream, pack('N', $length) . $payload);
}

function hamster_u16(string $bytes, int &$offset): int
{
    return unpack('nvalue', hamster_take($bytes, $offset, 2))['value'];
}

function hamster_u32(string $bytes, int &$offset): int
{
    return unpack('Nvalue', hamster_take($bytes, $offset, 4))['value'];
}

function hamster_u64(string $bytes, int &$offset): int
{
    $parts = unpack('Nhigh/Nlow', hamster_take($bytes, $offset, 8));
    return ($parts['high'] << 32) | $parts['low'];
}

function hamster_take(string $bytes, int &$offset, int $length): string
{
    if ($length < 0 || $offset + $length > strlen($bytes)) {
        throw new RuntimeException("truncated payload at {$offset}");
    }
    $value = substr($bytes, $offset, $length);
    $offset += $length;
    return $value;
}

function hamster_bytes_u16(string $bytes, int &$offset): string
{
    return hamster_take($bytes, $offset, hamster_u16($bytes, $offset));
}

function hamster_bytes_u32(string $bytes, int &$offset): string
{
    return hamster_take($bytes, $offset, hamster_u32($bytes, $offset));
}

function hamster_decode_request(string $payload): array
{
    $offset = 0;
    $kind = ord(hamster_take($payload, $offset, 1));
    if ($kind !== HAMSTER_REQUEST) {
        throw new RuntimeException("expected request frame, received {$kind}");
    }
    $request = [
        'id' => hamster_u64($payload, $offset),
        'method' => hamster_bytes_u16($payload, $offset),
        'uri' => hamster_bytes_u32($payload, $offset),
        'path' => hamster_bytes_u32($payload, $offset),
        'query' => hamster_bytes_u32($payload, $offset),
        'headers' => [],
    ];
    $headerCount = hamster_u16($payload, $offset);
    if ($headerCount > HAMSTER_MAX_HEADERS) {
        throw new RuntimeException("too many headers: {$headerCount}");
    }
    for ($index = 0; $index < $headerCount; $index++) {
        $request['headers'][] = [
            hamster_bytes_u16($payload, $offset),
            hamster_bytes_u32($payload, $offset),
        ];
    }
    $request['body'] = hamster_bytes_u32($payload, $offset);
    if ($offset !== strlen($payload)) {
        throw new RuntimeException('request has trailing bytes');
    }
    return $request;
}

function hamster_pack_u16(int $value): string
{
    return pack('n', $value);
}

function hamster_pack_u32(int $value): string
{
    return pack('N', $value);
}

function hamster_pack_u64(int $value): string
{
    return pack('N2', ($value >> 32) & 0xffffffff, $value & 0xffffffff);
}

function hamster_pack_bytes_u16(string $value): string
{
    if (strlen($value) > 0xffff) {
        throw new RuntimeException('u16 string too large');
    }
    return hamster_pack_u16(strlen($value)) . $value;
}

function hamster_pack_bytes_u32(string $value): string
{
    return hamster_pack_u32(strlen($value)) . $value;
}

function hamster_encode_response(array $response): string
{
    $headers = $response['headers'] ?? [];
    if (count($headers) > HAMSTER_MAX_HEADERS) {
        throw new RuntimeException('too many response headers');
    }
    $payload = chr(HAMSTER_RESPONSE)
        . hamster_pack_u64($response['id'])
        . hamster_pack_u16($response['status'])
        . hamster_pack_u16(count($headers));
    foreach ($headers as [$name, $value]) {
        $payload .= hamster_pack_bytes_u16($name) . hamster_pack_bytes_u32($value);
    }
    return $payload . hamster_pack_bytes_u32($response['body'] ?? '');
}

function hamster_send_ready($stdout): void
{
    hamster_write_frame(
        $stdout,
        chr(HAMSTER_READY) . hamster_pack_u16(HAMSTER_PROTOCOL_VERSION) . hamster_pack_u32(getmypid())
    );
}

function hamster_send_shutdown_ack($stdout): void
{
    hamster_write_frame($stdout, chr(HAMSTER_SHUTDOWN_ACK));
}
