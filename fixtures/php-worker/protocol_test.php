<?php

declare(strict_types=1);

require __DIR__ . '/protocol.php';

$checks = 0;
$failures = 0;

function check_protocol(string $what, mixed $actual, mixed $expected): void
{
    global $checks, $failures;
    $checks++;
    if ($actual !== $expected) {
        $failures++;
        fwrite(STDERR, sprintf(
            "FAIL %s\n  expected: %s\n  actual:   %s\n",
            $what,
            var_export($expected, true),
            var_export($actual, true)
        ));
    }
}

function endpoint(int $family, string $address, int $port): string
{
    $packed = inet_pton($address);
    if ($packed === false) {
        throw new RuntimeException("cannot pack {$address}");
    }

    return chr($family) . $packed . hamster_pack_u16($port);
}

function request_payload(int $httpVersion = 1, int $peerFamily = 4, int $scheme = 1): string
{
    return chr(HAMSTER_REQUEST)
        . hamster_pack_u64(7)
        . chr($httpVersion)
        . chr($scheme)
        . endpoint($peerFamily, $peerFamily === 4 ? '127.0.0.2' : '2001:db8::2', 43210)
        . endpoint(6, '2001:db8::1', 8080)
        . hamster_pack_bytes_u16('POST')
        . hamster_pack_bytes_u32('/submit?q=1')
        . hamster_pack_bytes_u32('/submit')
        . hamster_pack_bytes_u32('q=1')
        . hamster_pack_u16(2)
        . hamster_pack_bytes_u16('host') . hamster_pack_bytes_u32('example.test:8080')
        . hamster_pack_bytes_u16('x-binary') . hamster_pack_bytes_u32("a\0b")
        . hamster_pack_bytes_u32("body\0bytes");
}

$decoded = hamster_decode_request(request_payload());
check_protocol('request ID', $decoded['id'], 7);
check_protocol('HTTP/1.0 enum', $decoded['http_version'], 'HTTP/1.0');
check_protocol('http scheme enum', $decoded['scheme'], 'http');
check_protocol('IPv4 peer', $decoded['peer_addr'], '127.0.0.2');
check_protocol('peer port', $decoded['peer_port'], 43210);
check_protocol('IPv6 local address', $decoded['local_addr'], '2001:db8::1');
check_protocol('local port', $decoded['local_port'], 8080);
check_protocol('binary header', $decoded['headers'][1][1], "a\0b");
check_protocol('binary body', $decoded['body'], "body\0bytes");

$decodedV6 = hamster_decode_request(request_payload(2, 6));
check_protocol('HTTP/1.1 enum', $decodedV6['http_version'], 'HTTP/1.1');
check_protocol('IPv6 peer', $decodedV6['peer_addr'], '2001:db8::2');
check_protocol('https scheme enum', hamster_decode_request(request_payload(2, 4, 2))['scheme'], 'https');

foreach ([
    'unsupported HTTP version' => request_payload(3),
    'unsupported address family' => request_payload(1, 5),
    'unsupported scheme' => request_payload(1, 4, 3),
    'zero scheme' => request_payload(1, 4, 0),
    'truncated metadata' => substr(request_payload(), 0, 13),
    'trailing metadata' => request_payload() . "\0",
] as $what => $payload) {
    $rejected = false;
    try {
        hamster_decode_request($payload);
    } catch (RuntimeException) {
        $rejected = true;
    }
    check_protocol("{$what} fails closed", $rejected, true);
}

if ($failures !== 0) {
    fwrite(STDERR, "{$failures} of {$checks} checks failed\n");
    exit(1);
}
fwrite(STDOUT, "{$checks} checks passed\n");
