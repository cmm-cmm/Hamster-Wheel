<?php

declare(strict_types=1);

/**
 * Checks the HttpFoundation mapping (httpfoundation.php on adapter_support.php)
 * against the real symfony/http-foundation of an application:
 *
 *   php httpfoundation_test.php <path to an application's vendor/autoload.php>
 *
 * What a Symfony application reads from the request, and what reaches the
 * RESPONSE frame, for the cases a worker must reconstruct itself.
 */

require $argv[1] ?? throw new RuntimeException('pass the vendor/autoload.php of a Symfony application');
require __DIR__ . '/protocol.php';
require __DIR__ . '/adapter_support.php';
require __DIR__ . '/httpfoundation.php';

use Symfony\Component\HttpFoundation\BinaryFileResponse;
use Symfony\Component\HttpFoundation\Cookie;
use Symfony\Component\HttpFoundation\File\UploadedFile;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpFoundation\StreamedResponse;

$checks = 0;
$failures = 0;
function check(string $name, mixed $actual, mixed $expected): void
{
    global $checks, $failures;
    $checks++;
    if ($actual !== $expected) {
        $failures++;
        fwrite(STDERR, "FAIL {$name}: expected " . var_export($expected, true) . ', got ' . var_export($actual, true) . "\n");
    }
}

function frame(array $overrides): array
{
    return $overrides + [
        'id' => 1,
        'http_version' => 'HTTP/1.1',
        'scheme' => 'https',
        'peer_addr' => '192.0.2.7',
        'peer_port' => 50123,
        'local_addr' => '127.0.0.1',
        'local_port' => 8443,
        'method' => 'GET',
        'uri' => '/',
        'path' => '/',
        'query' => '',
        'headers' => [['host', 'app.test:8443']],
        'body' => '',
    ];
}

$binary = random_bytes(4096);
$boundary = 'hamster-boundary';
$body = "--{$boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n"
    . "--{$boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"data.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n{$binary}\r\n"
    . "--{$boundary}\r\nContent-Disposition: form-data; name=\"empty\"; filename=\"\"\r\nContent-Type: application/octet-stream\r\n\r\n\r\n"
    . "--{$boundary}--\r\n";
$request = hamster_httpfoundation_request(frame([
    'method' => 'POST',
    'uri' => '/upload?q=1&list[]=a',
    'path' => '/upload',
    'query' => 'q=1&list[]=a',
    'headers' => [
        ['host', 'app.test:8443'],
        ['content-type', "multipart/form-data; boundary={$boundary}"],
        ['cookie', 'theme=dark; note=two%20words'],
        ['x-repeat', 'one'],
        ['x-repeat', 'two'],
    ],
    'body' => $body,
]), Request::class, UploadedFile::class);

check('request class', get_class($request), Request::class);
check('method', $request->getMethod(), 'POST');
check('uri', $request->getUri(), 'https://app.test:8443/upload?list%5B0%5D=a&q=1');
check('scheme and host', $request->getSchemeAndHttpHost(), 'https://app.test:8443');
check('secure', $request->isSecure(), true);
check('port', $request->getPort(), 8443);
check('client ip is the socket peer', $request->getClientIp(), '192.0.2.7');
check('path info', $request->getPathInfo(), '/upload');
check('query', $request->query->all(), ['q' => '1', 'list' => ['a']]);
check('form field', $request->request->get('title'), 'hello');
check('cookie', $request->cookies->get('note'), 'two words');
// As under a CGI server: one variable holding every value.
check('repeated header', $request->headers->all('x-repeat'), ['one, two']);
check('raw body', $request->getContent(), $body);

$file = $request->files->get('doc');
check('upload class', $file instanceof UploadedFile, true);
check('upload is valid', $file->isValid(), true);
check('upload client name', $file->getClientOriginalName(), 'data.bin');
check('upload size', $file->getSize(), strlen($binary));
check('empty file input', $request->files->get('empty')?->getError(), UPLOAD_ERR_NO_FILE);
$target = sys_get_temp_dir() . '/hamster-httpfoundation-' . getmypid();
@mkdir($target);
$moved = $file->move($target, 'moved.bin');
check('moved content', file_get_contents($moved->getPathname()), $binary);
unlink($moved->getPathname());
rmdir($target);
$unmoved = hamster_httpfoundation_request(frame([
    'method' => 'POST',
    'headers' => [['host', 'app.test'], ['content-type', "multipart/form-data; boundary={$boundary}"]],
    'body' => $body,
]), Request::class, UploadedFile::class)->files->get('doc')->getPathname();
check('unmoved upload exists during the request', is_file($unmoved), true);
hamster_discard_uploads();
check('unmoved upload removed after the request', is_file($unmoved), false);

$plain = hamster_httpfoundation_request(frame(['scheme' => 'http', 'local_port' => 8080, 'headers' => [['host', 'app.test']]]), Request::class, UploadedFile::class);
check('cleartext scheme', $plain->isSecure(), false);
check('host without a port falls back to the scheme default', $plain->getSchemeAndHttpHost(), 'http://app.test');

$response = new Response('hello', 201, ['Content-Length' => '999', 'Connection' => 'close', 'X-Kept' => 'yes']);
$response->headers->setCookie(Cookie::create('first', '1'));
$response->headers->setCookie(Cookie::create('second', 'two'));
$headers = hamster_response_headers($response);
$names = array_map(static fn (array $pair): string => strtolower($pair[0]), $headers);
check('owned headers dropped', in_array('content-length', $names, true) || in_array('connection', $names, true), false);
check('both cookies kept', count(array_keys($names, 'set-cookie', true)), 2);
check('other headers kept', in_array('x-kept', $names, true), true);

$streamed = new StreamedResponse(static function (): void {
    echo 'part one, ';
    flush();
    echo 'part two';
});
check('streamed body is collected', hamster_response_body($streamed), 'part one, part two');
$path = tempnam(sys_get_temp_dir(), 'hamster-binary-');
file_put_contents($path, $binary);
check('file body is collected', hamster_response_body(new BinaryFileResponse($path)), $binary);
unlink($path);

if ($failures > 0) {
    fwrite(STDERR, "{$failures} of {$checks} checks failed\n");
    exit(1);
}
fwrite(STDOUT, "{$checks} checks passed\n");
