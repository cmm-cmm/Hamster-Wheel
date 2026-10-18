<?php

declare(strict_types=1);

/**
 * Framework-neutral support for persistent-worker adapters.
 *
 * Under a real SAPI, PHP parses the request line, the cookie header and the
 * body before any application code runs, and hands the result to the
 * application in $_SERVER, $_GET, $_POST, $_COOKIE and $_FILES. A persistent
 * worker never goes through that path: it receives a decoded protocol frame and
 * must do the same work itself. This file does that work once, returning plain
 * arrays, so each framework adapter only wraps the result in its own request
 * type (HttpFoundation, PSR-7, or none). It also holds the response-side rules
 * every adapter must obey: headers Hamster owns, output that must never reach
 * the protocol pipe, and the frame size limit.
 *
 * Nothing here resets application state. What a framework keeps between
 * requests is the adapter's responsibility.
 */

/**
 * Upper bound on multipart parts in one request.
 *
 * The protocol already caps a body at 1 MiB, so this is not a memory bound. It
 * bounds the work a body of that size can ask for: a megabyte of empty parts
 * would otherwise be a megabyte of array writes and temporary files.
 */
const HAMSTER_MAX_MULTIPART_PARTS = 1000;

/**
 * Response headers Hamster owns. The server rejects a RESPONSE frame carrying
 * any of them (502), so an adapter drops them from whatever the framework set.
 */
const HAMSTER_FORBIDDEN_RESPONSE_HEADERS = [
    'connection' => true,
    'content-length' => true,
    'keep-alive' => true,
    'proxy-authenticate' => true,
    'proxy-authorization' => true,
    'server' => true,
    'te' => true,
    'trailer' => true,
    'transfer-encoding' => true,
    'upgrade' => true,
];

/** Raised for a request Hamster can decode but a client has malformed. */
class HamsterBadRequest extends RuntimeException {}

/**
 * Temporary files created for this request's uploads.
 *
 * PHP's own upload temporaries are cleaned up when the SAPI ends the request.
 * Nothing ends here, so the worker owns that job; see
 * hamster_discard_uploads().
 *
 * @return array<int, string>
 */
function &hamster_upload_temporaries(): array
{
    static $paths = [];

    return $paths;
}

/**
 * Removes any upload temporary the application did not move away.
 *
 * The optional remover is a fault-injection seam for the standalone test.
 * Production callers always use unlink.
 */
function hamster_discard_uploads(?callable $remove = null): void
{
    $paths = &hamster_upload_temporaries();
    $failures = [];
    foreach ($paths as $path) {
        // A moved upload is renamed, so a missing file is the normal case.
        $removed = ! is_file($path)
            || ($remove === null ? @unlink($path) : $remove($path));
        if (! $removed && is_file($path)) {
            $failures[] = $path;
        }
    }
    $paths = $failures;
    if ($failures !== []) {
        throw new RuntimeException(
            'failed to remove ' . count($failures) . ' upload temporary file(s)'
        );
    }
}

/**
 * The SAPI-equivalent parts of a decoded request frame.
 *
 * `$makeFile` receives each file part as ($clientName, $content, $clientType)
 * and returns whatever the framework calls an uploaded file; the returned
 * values are arranged under their PHP-style field names in `files`.
 *
 * @param  array{http_version:string,scheme:string,peer_addr:string,peer_port:int,local_addr:string,local_port:int,method:string,uri:string,query:string,headers:list<array{0:string,1:string}>,body:string}  $decoded
 * @return array{server: array<string, string>, query: array<string, mixed>, parameters: array<string, mixed>, cookies: array<string, string>, files: array<string, mixed>, body: string}
 *
 * @throws HamsterBadRequest when the client's body cannot be parsed
 */
function hamster_request_parts(array $decoded, callable $makeFile): array
{
    $server = [
        'REQUEST_METHOD' => $decoded['method'],
        'REQUEST_URI' => $decoded['uri'],
        'QUERY_STRING' => $decoded['query'],
        'SERVER_PROTOCOL' => $decoded['http_version'],
        'REMOTE_ADDR' => $decoded['peer_addr'],
        'REMOTE_PORT' => (string) $decoded['peer_port'],
        'SERVER_ADDR' => $decoded['local_addr'],
        'SERVER_PORT' => (string) $decoded['local_port'],
        'REQUEST_SCHEME' => $decoded['scheme'],
        'HTTPS' => $decoded['scheme'] === 'https' ? 'on' : 'off',
        'REQUEST_TIME' => (string) time(),
        'REQUEST_TIME_FLOAT' => (string) microtime(true),
    ];
    $cookies = [];
    foreach ($decoded['headers'] as [$name, $value]) {
        $key = strtoupper(str_replace('-', '_', $name));
        if ($key === 'COOKIE') {
            // Symfony builds the cookie bag from its own argument and never
            // parses HTTP_COOKIE, so passing the header alone silently gives
            // the application a request with no cookies: no session, no
            // authenticated user, and every POST rejected as a CSRF failure.
            $cookies += hamster_parse_cookies($value);
        }
        if ($key === 'CONTENT_TYPE' || $key === 'CONTENT_LENGTH') {
            $server[$key] = $value;
            continue;
        }
        // A field sent more than once becomes one variable holding every
        // value, joined as a CGI server joins them (RFC 3875 4.1.18): with
        // ", ", or "; " for Cookie. Assigning each occurrence in turn kept
        // only the last, so an application saw one of several Accept,
        // Forwarded or X-Forwarded-For values.
        $variable = 'HTTP_' . $key;
        $server[$variable] = isset($server[$variable])
            ? $server[$variable] . ($key === 'COOKIE' ? '; ' : ', ') . $value
            : $value;
    }
    if (! isset($server['HTTP_HOST']) || $server['HTTP_HOST'] === '') {
        throw new HamsterBadRequest('request has no effective Host');
    }
    $server['SERVER_NAME'] = hamster_authority_hostname($server['HTTP_HOST']);

    parse_str($decoded['query'], $query);

    $parameters = [];
    $files = [];
    $contentType = strtolower($server['CONTENT_TYPE'] ?? '');
    if (str_starts_with($contentType, 'application/x-www-form-urlencoded')) {
        parse_str($decoded['body'], $parameters);
    } elseif (str_starts_with($contentType, 'multipart/form-data')) {
        [$parameters, $files] = hamster_parse_multipart_with(
            $decoded['body'],
            (string) ($server['CONTENT_TYPE'] ?? ''),
            $makeFile
        );
    }

    return [
        'server' => $server,
        'query' => $query,
        'parameters' => $parameters,
        'cookies' => $cookies,
        'files' => $files,
        'body' => $decoded['body'],
    ];
}

/**
 * Returns the hostname component of the already validated effective authority.
 *
 * Rust owns authority validation and rewrites Host before routing. This parser
 * deliberately has no fallback identity: a missing or structurally invalid
 * authority fails the request instead of manufacturing SERVER_NAME.
 */
function hamster_authority_hostname(string $authority): string
{
    if (str_starts_with($authority, '[')) {
        $close = strpos($authority, ']');
        if ($close === false || $close === 1) {
            throw new HamsterBadRequest('invalid effective Host');
        }
        $remainder = substr($authority, $close + 1);
        if ($remainder !== '' && preg_match('/^:[0-9]+$/D', $remainder) !== 1) {
            throw new HamsterBadRequest('invalid effective Host');
        }

        return substr($authority, 1, $close - 1);
    }

    $separator = strrpos($authority, ':');
    $hostname = $separator === false ? $authority : substr($authority, 0, $separator);
    $port = $separator === false ? null : substr($authority, $separator + 1);
    if ($hostname === '' || ($port !== null && ($port === '' || ! ctype_digit($port)))) {
        throw new HamsterBadRequest('invalid effective Host');
    }

    return $hostname;
}

/** @return array<string, string> */
function hamster_parse_cookies(string $header): array
{
    $cookies = [];
    foreach (explode(';', $header) as $pair) {
        $pair = trim($pair);
        if ($pair === '') {
            continue;
        }
        $split = strpos($pair, '=');
        if ($split === false) {
            $cookies[urldecode($pair)] = '';
            continue;
        }
        $cookies[urldecode(substr($pair, 0, $split))] = urldecode(substr($pair, $split + 1));
    }

    return $cookies;
}

/**
 * Splits a multipart/form-data body into request parameters and uploads.
 *
 * File parts become whatever `$makeFile` returns for ($clientName, $content,
 * $clientType); hamster_store_upload() gives it a real temporary file that
 * hamster_discard_uploads() removes after the request.
 *
 * @return array{0: array<string, mixed>, 1: array<string, mixed>}
 *
 * @throws HamsterBadRequest
 */
function hamster_parse_multipart_with(string $body, string $contentType, callable $makeFile): array
{
    $boundary = hamster_multipart_boundary($contentType);
    if ($boundary === null) {
        throw new HamsterBadRequest('multipart/form-data without a boundary');
    }

    $parameters = [];
    $files = [];
    // A boundary prefix inside file content is not a delimiter unless the
    // remainder of its line is valid too. Splitting on the prefix alone would
    // silently truncate an upload containing, for example,
    // "\r\n--boundary--not-a-close". Match complete delimiter lines and use
    // their offsets so the CRLF before each delimiter stays out of the part.
    $delimiter = '/(?:\A|\r\n)--' . preg_quote($boundary, '/')
        . '(?:(--)[\x20\t]*(?:\r\n|\z)|[\x20\t]*\r\n)/';
    $matched = preg_match_all(
        $delimiter,
        $body,
        $matches,
        PREG_OFFSET_CAPTURE | PREG_UNMATCHED_AS_NULL
    );
    if ($matched === false || $matched === 0) {
        throw new HamsterBadRequest('multipart body has no parts');
    }

    $segments = [];
    $contentStart = null;
    $closed = false;
    foreach ($matches[0] as $index => [$wholeDelimiter, $offset]) {
        $closing = $matches[1][$index][0] !== null;
        if ($contentStart === null) {
            if ($closing) {
                break;
            }
            $contentStart = $offset + strlen($wholeDelimiter);
            continue;
        }
        $segments[] = substr($body, $contentStart, $offset - $contentStart);
        if ($closing) {
            $closed = true;
            break;
        }
        $contentStart = $offset + strlen($wholeDelimiter);
    }
    if ($contentStart === null) {
        throw new HamsterBadRequest('multipart body has no parts');
    }
    if (! $closed) {
        throw new HamsterBadRequest('multipart body has no closing boundary');
    }
    if ($segments === []) {
        throw new HamsterBadRequest('multipart body has no parts');
    }

    $parsed = 0;
    foreach ($segments as $segment) {
        if (++$parsed > HAMSTER_MAX_MULTIPART_PARTS) {
            throw new HamsterBadRequest('multipart body has too many parts');
        }
        $split = strpos($segment, "\r\n\r\n");
        if ($split === false) {
            throw new HamsterBadRequest('multipart part has no header block');
        }
        $headers = hamster_multipart_headers(substr($segment, 0, $split));
        $content = substr($segment, $split + 4);

        $disposition = hamster_content_disposition($headers['content-disposition'] ?? '');
        $name = $disposition['name'] ?? null;
        if ($name === null || $name === '') {
            continue;
        }

        if (! array_key_exists('filename', $disposition)) {
            hamster_assign_nested($parameters, $name, $content);
            continue;
        }
        hamster_assign_nested(
            $files,
            $name,
            $makeFile($disposition['filename'], $content, $headers['content-type'] ?? null)
        );
    }

    return [$parameters, $files];
}

/**
 * Writes one upload's content to a temporary file owned by this request.
 *
 * @throws HamsterBadRequest
 */
function hamster_store_upload(string $content): string
{
    $path = tempnam(sys_get_temp_dir(), 'hamster-upload-');
    if ($path === false) {
        throw new HamsterBadRequest('cannot create a temporary file for an upload');
    }
    $paths = &hamster_upload_temporaries();
    $paths[] = $path;
    if (file_put_contents($path, $content) === false) {
        throw new HamsterBadRequest('cannot write an upload to its temporary file');
    }

    return $path;
}

function hamster_multipart_boundary(string $contentType): ?string
{
    if (preg_match('/boundary="?([^";]+)"?/i', $contentType, $matches) !== 1) {
        return null;
    }
    $boundary = trim($matches[1]);

    return $boundary === '' ? null : $boundary;
}

/** @return array<string, string> */
function hamster_multipart_headers(string $block): array
{
    $headers = [];
    foreach (explode("\r\n", $block) as $line) {
        $split = strpos($line, ':');
        if ($split === false) {
            continue;
        }
        $headers[strtolower(trim(substr($line, 0, $split)))] = trim(substr($line, $split + 1));
    }

    return $headers;
}

/**
 * Reads `name` and `filename` from a Content-Disposition header.
 *
 * `filename` is reported as present-but-empty rather than absent when the
 * client sent it empty, because that is what separates a form field from an
 * empty file input.
 *
 * @return array<string, string>
 */
function hamster_content_disposition(string $header): array
{
    $values = [];
    foreach (['name', 'filename'] as $attribute) {
        if (preg_match('/(?<![a-z*])' . $attribute . '="((?:[^"\\\\]|\\\\.)*)"/i', $header, $matches) === 1) {
            $values[$attribute] = stripslashes($matches[1]);
        } elseif (preg_match('/(?<![a-z*])' . $attribute . '=([^;]*)/i', $header, $matches) === 1) {
            $values[$attribute] = trim($matches[1]);
        }
    }

    return $values;
}

/**
 * Stores a value under a PHP-style field name such as `a`, `a[b]` or `a[]`.
 *
 * `parse_str` does this for urlencoded bodies; multipart field names obey the
 * same convention and frameworks' nested input and array validation depend on
 * it.
 */
function hamster_assign_nested(array &$target, string $name, mixed $value): void
{
    $open = strpos($name, '[');
    if ($open === false) {
        // PHP normalizes dots and spaces in the top-level variable name. Keys
        // inside brackets are deliberately left alone.
        $target[strtr($name, ['.' => '_', ' ' => '_'])] = $value;

        return;
    }

    $keys = [strtr(substr($name, 0, $open), ['.' => '_', ' ' => '_'])];
    $rest = substr($name, $open);
    while ($rest !== '' && $rest[0] === '[') {
        $close = strpos($rest, ']');
        if ($close === false) {
            // `parse_str` treats an unmatched opening bracket as part of the
            // top-level name and normalizes it to an underscore.
            $target[strtr($name, ['.' => '_', ' ' => '_', '[' => '_'])] = $value;

            return;
        }
        $keys[] = substr($rest, 1, $close - 1);
        $rest = substr($rest, $close + 1);
    }

    $cursor = &$target;
    foreach ($keys as $index => $key) {
        $last = $index === array_key_last($keys);
        if ($key === '') {
            if ($last) {
                $cursor[] = $value;

                return;
            }
            $cursor[] = [];
            $key = array_key_last($cursor);
        }
        if ($last) {
            $cursor[$key] = $value;

            return;
        }
        if (! isset($cursor[$key]) || ! is_array($cursor[$key])) {
            $cursor[$key] = [];
        }
        $cursor = &$cursor[$key];
    }
}

/** Run code behind an output guard and return bytes it attempted to print. */
function hamster_guard_output(callable $operation): string
{
    $level = ob_get_level();
    ob_start();
    try {
        $operation();
    } finally {
        $chunks = [];
        while (ob_get_level() > $level) {
            $chunk = ob_get_clean();
            if (is_string($chunk) && $chunk !== '') {
                $chunks[] = $chunk;
            }
        }
    }

    return implode('', array_reverse($chunks));
}

/**
 * Encodes a RESPONSE frame the server will accept.
 *
 * Headers Hamster owns are dropped. A response whose frame would exceed the
 * protocol's 8 MiB limit cannot be delivered: writing it would make the server
 * treat this worker as broken, kill it and answer 502, and the next request
 * would pay for a replacement. The adapter answers 500 instead, says so on
 * stderr, and keeps the worker.
 *
 * @param  list<array{0:string,1:string}>  $headers
 */
function hamster_response_frame(int $id, int $status, array $headers, string $body, string $adapter): string
{
    $kept = [];
    foreach ($headers as [$name, $value]) {
        if (! isset(HAMSTER_FORBIDDEN_RESPONSE_HEADERS[strtolower($name)])) {
            $kept[] = [$name, $value];
        }
    }
    try {
        $frame = hamster_encode_response(['id' => $id, 'status' => $status, 'headers' => $kept, 'body' => $body]);
    } catch (RuntimeException $error) {
        // Too many headers, or one name longer than the u16 length field.
        fwrite(STDERR, "{$adapter}: response cannot be encoded: {$error->getMessage()}\n");

        return hamster_plain_response_frame($id, 500, 'Response Not Representable');
    }
    if (strlen($frame) > HAMSTER_MAX_FRAME) {
        fwrite(STDERR, "{$adapter}: response of " . strlen($body) . " bytes exceeds the protocol frame limit\n");

        return hamster_plain_response_frame($id, 500, 'Response Too Large');
    }

    return $frame;
}

function hamster_plain_response_frame(int $id, int $status, string $body): string
{
    return hamster_encode_response([
        'id' => $id,
        'status' => $status,
        'headers' => [['Content-Type', 'text/plain; charset=UTF-8']],
        'body' => $body,
    ]);
}
