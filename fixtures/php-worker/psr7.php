<?php

declare(strict_types=1);

/**
 * PSR-7 request and response mapping for psr15_worker.php, in its own file so
 * the standalone checks (psr7_test.php) can exercise it with a real PSR-7
 * implementation. Requires adapter_support.php and an autoloader providing
 * psr/http-message.
 */

use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\ServerRequestFactoryInterface;
use Psr\Http\Message\ServerRequestInterface;
use Psr\Http\Message\StreamFactoryInterface;
use Psr\Http\Message\UploadedFileFactoryInterface;

function hamster_psr15_request(
    array $decoded,
    ServerRequestFactoryInterface $requests,
    StreamFactoryInterface $streams,
    UploadedFileFactoryInterface $uploads
): ServerRequestInterface {
    $parts = hamster_request_parts(
        $decoded,
        static function (string $name, string $content, ?string $type) use ($streams, $uploads) {
            if ($name === '' && $content === '') {
                return $uploads->createUploadedFile($streams->createStream(''), 0, UPLOAD_ERR_NO_FILE, '', null);
            }
            $path = hamster_store_upload($content);

            return $uploads->createUploadedFile(
                $streams->createStreamFromFile($path, 'r'),
                strlen($content),
                UPLOAD_ERR_OK,
                $name,
                $type
            );
        }
    );
    $server = $parts['server'];
    $uri = $decoded['scheme'] . '://' . $server['HTTP_HOST'] . $decoded['uri'];

    $request = $requests->createServerRequest($decoded['method'], $uri, $server)
        ->withProtocolVersion(substr($decoded['http_version'], 5))
        ->withCookieParams($parts['cookies'])
        ->withQueryParams($parts['query'])
        ->withUploadedFiles($parts['files'])
        ->withBody($streams->createStream($parts['body']));
    // A factory may already have derived headers from the server parameters;
    // the first occurrence replaces, later ones append, so the application
    // sees each field exactly as the client sent it.
    $seen = [];
    foreach ($decoded['headers'] as [$name, $value]) {
        $key = strtolower($name);
        $request = isset($seen[$key]) ? $request->withAddedHeader($name, $value) : $request->withHeader($name, $value);
        $seen[$key] = true;
    }
    if ($parts['parameters'] !== [] || $parts['files'] !== []) {
        $request = $request->withParsedBody($parts['parameters']);
    }

    return $request;
}

/**
 * Ends a native PHP session the application left open.
 *
 * Under the CLI SAPI session_start() works but can never send its cookie, and
 * an active session outlives the request: the next request, from any client,
 * would find it already started and read the same $_SESSION. That is a data
 * leak between clients, so the session is written, closed and forgotten after
 * every request, and the first occurrence is reported once.
 */
function hamster_psr15_end_native_session(): void
{
    static $reported = false;
    if (session_status() !== PHP_SESSION_ACTIVE) {
        return;
    }
    session_write_close();
    $_SESSION = [];
    session_id('');
    if (! $reported) {
        $reported = true;
        fwrite(STDERR, "psr-15 worker: the application started a native PHP session; it cannot set its cookie in a worker and was closed after the request\n");
    }
}

/** @return list<array{0:string,1:string}> */
function hamster_psr15_headers(ResponseInterface $response): array
{
    $headers = [];
    foreach ($response->getHeaders() as $name => $values) {
        foreach ($values as $value) {
            $headers[] = [(string) $name, (string) $value];
        }
    }

    return $headers;
}
