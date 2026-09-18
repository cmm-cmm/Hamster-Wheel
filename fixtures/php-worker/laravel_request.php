<?php

declare(strict_types=1);

/**
 * Request reconstruction for the persistent Laravel worker.
 *
 * Under a real SAPI, PHP parses the request line, the cookie header and the
 * body before any application code runs, and hands the result to the framework
 * in $_GET, $_POST, $_COOKIE and $_FILES. A persistent worker never goes
 * through that path: it receives bytes on a pipe and must do the same work
 * itself. Everything Laravel later relies on -- sessions, CSRF, form input,
 * uploads -- is downstream of getting this right.
 *
 * `Request::create()` is deliberately not used. It is a test helper: it builds
 * its cookie bag from an argument rather than the Cookie header, and it takes
 * form parameters as an argument while ignoring the body entirely. Both
 * defects are silent, and both were observed against a real application as a
 * login that always answered 419.
 */

use Illuminate\Http\Request;
use Illuminate\Http\UploadedFile;

/**
 * Upper bound on multipart parts in one request.
 *
 * The protocol already caps a body at 1 MiB, so this is not a memory bound. It
 * bounds the work a body of that size can ask for: a megabyte of empty parts
 * would otherwise be a megabyte of array writes and temporary files.
 */
const HAMSTER_MAX_MULTIPART_PARTS = 1000;

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
 * Rebuilds an Illuminate request from a decoded protocol frame.
 *
 * @param  array{http_version:string,peer_addr:string,peer_port:int,local_addr:string,local_port:int,method:string,uri:string,query:string,headers:list<array{0:string,1:string}>,body:string}  $decoded
 *
 * @throws HamsterBadRequest when the client's body cannot be parsed
 */
function hamster_build_request(array $decoded): Request
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
        'REQUEST_SCHEME' => 'http',
        'HTTPS' => 'off',
    ];
    $cookies = [];
    foreach ($decoded['headers'] as [$name, $value]) {
        $key = strtoupper(str_replace('-', '_', $name));
        if ($key === 'COOKIE') {
            // Symfony builds the cookie bag from its own argument and never
            // parses HTTP_COOKIE, so passing the header alone silently gives
            // Laravel a request with no cookies: no session, no authenticated
            // user, and every POST rejected as a CSRF failure.
            $cookies += hamster_parse_cookies($value);
        }
        $server[$key === 'CONTENT_TYPE' || $key === 'CONTENT_LENGTH' ? $key : 'HTTP_' . $key] = $value;
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
        [$parameters, $files] = hamster_parse_multipart(
            $decoded['body'],
            (string) ($server['CONTENT_TYPE'] ?? '')
        );
    }

    return new Request($query, $parameters, [], $cookies, $files, $server, $decoded['body']);
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
 * File parts are written to real temporary files because that is what an
 * UploadedFile is: a path plus the client's claimed name. Every one is
 * constructed in test mode, since `is_uploaded_file()` answers for the SAPI's
 * own upload temporaries and would reject ours -- which would make Laravel's
 * `file` validation rule fail for every upload a persistent worker receives.
 *
 * @return array{0: array<string, mixed>, 1: array<string, mixed>}
 *
 * @throws HamsterBadRequest
 */
function hamster_parse_multipart(string $body, string $contentType): array
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
            hamster_uploaded_file(
                $disposition['filename'],
                $content,
                $headers['content-type'] ?? null
            )
        );
    }

    return [$parameters, $files];
}

function hamster_uploaded_file(string $originalName, string $content, ?string $mimeType): UploadedFile
{
    // An empty file input sends a part with an empty filename and no content.
    // PHP reports that as UPLOAD_ERR_NO_FILE rather than an empty upload, and
    // Laravel's validation rules are written against that distinction.
    if ($originalName === '' && $content === '') {
        return new UploadedFile('', '', null, UPLOAD_ERR_NO_FILE, true);
    }

    $path = tempnam(sys_get_temp_dir(), 'hamster-upload-');
    if ($path === false) {
        throw new HamsterBadRequest('cannot create a temporary file for an upload');
    }
    $paths = &hamster_upload_temporaries();
    $paths[] = $path;
    if (file_put_contents($path, $content) === false) {
        throw new HamsterBadRequest('cannot write an upload to its temporary file');
    }

    return new UploadedFile($path, $originalName, $mimeType, UPLOAD_ERR_OK, true);
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
 * same convention and Laravel's `$request->input('a.b')` and array validation
 * rules depend on it.
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
