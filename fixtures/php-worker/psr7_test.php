<?php

declare(strict_types=1);

/**
 * Checks the PSR-7 mapping (psr7.php on adapter_support.php) against a real
 * PSR-7/PSR-17 implementation, slim/psr7:
 *
 *   php psr7_test.php <path to a vendor/autoload.php providing slim/psr7>
 */

require $argv[1] ?? throw new RuntimeException('pass a vendor/autoload.php that provides slim/psr7');
require __DIR__ . '/protocol.php';
require __DIR__ . '/adapter_support.php';
require __DIR__ . '/psr7.php';

use Psr\Http\Message\UploadedFileInterface;
use Slim\Psr7\Factory\ResponseFactory;
use Slim\Psr7\Factory\ServerRequestFactory;
use Slim\Psr7\Factory\StreamFactory;
use Slim\Psr7\Factory\UploadedFileFactory;

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

$binary = random_bytes(4096);
$boundary = 'hamster-boundary';
$body = "--{$boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n"
    . "--{$boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"data.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n{$binary}\r\n"
    . "--{$boundary}\r\nContent-Disposition: form-data; name=\"nested[inner]\"; filename=\"inner.txt\"\r\nContent-Type: text/plain\r\n\r\ninside\r\n"
    . "--{$boundary}\r\nContent-Disposition: form-data; name=\"empty\"; filename=\"\"\r\nContent-Type: application/octet-stream\r\n\r\n\r\n"
    . "--{$boundary}--\r\n";
$decoded = [
    'id' => 1,
    'http_version' => 'HTTP/1.0',
    'scheme' => 'https',
    'peer_addr' => '192.0.2.7',
    'peer_port' => 50123,
    'local_addr' => '127.0.0.1',
    'local_port' => 8443,
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
];
$streams = new StreamFactory();
$request = hamster_psr15_request($decoded, new ServerRequestFactory(), $streams, new UploadedFileFactory());

check('method', $request->getMethod(), 'POST');
check('protocol version', $request->getProtocolVersion(), '1.0');
check('uri scheme', $request->getUri()->getScheme(), 'https');
check('uri host', $request->getUri()->getHost(), 'app.test');
check('uri port', $request->getUri()->getPort(), 8443);
check('uri path', $request->getUri()->getPath(), '/upload');
// slim/psr7 percent-encodes the brackets, as RFC 3986 allows.
check('uri query', rawurldecode($request->getUri()->getQuery()), 'q=1&list[]=a');
check('query params', $request->getQueryParams(), ['q' => '1', 'list' => ['a']]);
check('cookie params', $request->getCookieParams(), ['theme' => 'dark', 'note' => 'two words']);
check('repeated header keeps each value', $request->getHeader('X-Repeat'), ['one', 'two']);
check('repeated header in server params, as CGI joins it', $request->getServerParams()['HTTP_X_REPEAT'], 'one, two');
check('peer in server params', $request->getServerParams()['REMOTE_ADDR'], '192.0.2.7');
check('parsed body', $request->getParsedBody(), ['title' => 'hello']);
check('raw body', (string) $request->getBody(), $body);

$files = $request->getUploadedFiles();
$doc = $files['doc'];
check('upload interface', $doc instanceof UploadedFileInterface, true);
check('upload error', $doc->getError(), UPLOAD_ERR_OK);
check('upload client name', $doc->getClientFilename(), 'data.bin');
check('upload client type', $doc->getClientMediaType(), 'application/octet-stream');
check('upload size', $doc->getSize(), strlen($binary));
check('upload stream', (string) $doc->getStream(), $binary);
check('nested upload', (string) $files['nested']['inner']->getStream(), 'inside');
check('empty file input', $files['empty']->getError(), UPLOAD_ERR_NO_FILE);
$target = sys_get_temp_dir() . '/hamster-psr7-' . getmypid() . '.bin';
$doc->moveTo($target);
check('moved under the CLI SAPI', file_get_contents($target), $binary);
unlink($target);
hamster_discard_uploads();
check('temporaries removed after the request', hamster_upload_temporaries(), []);

$urlencoded = hamster_psr15_request([
    'body' => 'a%5Bb%5D%5B%5D=1&a%5Bb%5D%5B%5D=2&c=%C3%A9',
    'headers' => [['host', 'app.test'], ['content-type', 'application/x-www-form-urlencoded']],
    'scheme' => 'http',
    'uri' => '/', 'query' => '',
] + $decoded, new ServerRequestFactory(), $streams, new UploadedFileFactory());
check('urlencoded parsed body', $urlencoded->getParsedBody(), ['a' => ['b' => ['1', '2']], 'c' => 'é']);
check('cleartext uri without a port', (string) $urlencoded->getUri(), 'http://app.test/');

$json = hamster_psr15_request([
    'body' => '{"a":1}',
    'headers' => [['host', 'app.test'], ['content-type', 'application/json']],
] + $decoded, new ServerRequestFactory(), $streams, new UploadedFileFactory());
check('json is left to the application', $json->getParsedBody(), null);

$response = (new ResponseFactory())->createResponse(201)
    ->withHeader('Set-Cookie', 'first=1')
    ->withAddedHeader('Set-Cookie', 'second=2')
    ->withHeader('X-Kept', 'yes');
check('response headers flattened in order', hamster_psr15_headers($response), [
    ['Set-Cookie', 'first=1'],
    ['Set-Cookie', 'second=2'],
    ['X-Kept', 'yes'],
]);

ini_set('session.save_path', sys_get_temp_dir());
session_start();
$_SESSION['leak'] = 'from the previous client';
fwrite(STDERR, '');
hamster_psr15_end_native_session();
check('native session closed', session_status(), PHP_SESSION_NONE);
check('native session data forgotten', $_SESSION, []);

if ($failures > 0) {
    fwrite(STDERR, "{$failures} of {$checks} checks failed\n");
    exit(1);
}
fwrite(STDOUT, "{$checks} checks passed\n");
