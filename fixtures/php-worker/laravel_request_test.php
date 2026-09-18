<?php

declare(strict_types=1);

/**
 * Self-checking tests for the worker's request reconstruction.
 *
 * The repository deliberately does not vendor a Laravel application, so this
 * covers the part that can be tested without one: cookie parsing, multipart
 * splitting, PHP-style field names, and the Content-Disposition grammar. A
 * stub UploadedFile is declared before the module is loaded, which is enough
 * because the module only names the class at call time.
 *
 * Run directly: php fixtures/php-worker/laravel_request_test.php
 * Exit status 0 means every case passed.
 */

namespace Illuminate\Http {
    /** Records the arguments the worker would hand to Laravel. */
    class UploadedFile
    {
        public function __construct(
            public string $path,
            public string $originalName,
            public ?string $mimeType = null,
            public ?int $error = null,
            public bool $test = false
        ) {}

        public function contents(): string
        {
            return $this->path === '' ? '' : (string) file_get_contents($this->path);
        }
    }

    class Request
    {
        public function __construct(
            public array $query = [],
            public array $request = [],
            public array $attributes = [],
            public array $cookies = [],
            public array $files = [],
            public array $server = [],
            public mixed $content = null
        ) {}
    }
}

namespace {
    require __DIR__ . '/laravel_request.php';

    $failures = 0;
    $checks = 0;

    function check(string $what, mixed $actual, mixed $expected): void
    {
        global $failures, $checks;
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

    // --- cookies ---------------------------------------------------------
    check(
        'cookies are split, trimmed and url-decoded',
        hamster_parse_cookies('a=1; b=two%20words;c=; flag'),
        ['a' => '1', 'b' => 'two words', 'c' => '', 'flag' => '']
    );
    check(
        'a cookie value keeps its own equals signs',
        hamster_parse_cookies('token=abc=def=='),
        ['token' => 'abc=def==']
    );

    $built = hamster_build_request([
        'http_version' => 'HTTP/1.0',
        'peer_addr' => '127.0.0.2',
        'peer_port' => 43210,
        'local_addr' => '127.0.0.1',
        'local_port' => 8080,
        'method' => 'POST',
        'uri' => '/submit?q=two%20words',
        'query' => 'q=two%20words',
        'headers' => [
            ['content-type', 'application/x-www-form-urlencoded'],
            ['cookie', 'session=abc%3Ddef; theme=dark'],
            ['x-review', 'present'],
            ['host', 'example.test:8080'],
        ],
        'body' => 'name=Alice+Example&nested%5Ba%5D=1',
    ]);
    check('the request builder populates the query bag', $built->query, ['q' => 'two words']);
    check('the request builder populates the form bag', $built->request, [
        'name' => 'Alice Example',
        'nested' => ['a' => '1'],
    ]);
    check('the request builder populates the cookie bag', $built->cookies, [
        'session' => 'abc=def',
        'theme' => 'dark',
    ]);
    check('the request builder preserves the raw body', $built->content, 'name=Alice+Example&nested%5Ba%5D=1');
    check('the request builder maps content type to the SAPI key', $built->server['CONTENT_TYPE'], 'application/x-www-form-urlencoded');
    check('the request builder maps ordinary headers to HTTP keys', $built->server['HTTP_X_REVIEW'], 'present');
    check('the request builder preserves the HTTP version', $built->server['SERVER_PROTOCOL'], 'HTTP/1.0');
    check('the request builder maps the immediate peer address', $built->server['REMOTE_ADDR'], '127.0.0.2');
    check('the request builder maps the immediate peer port', $built->server['REMOTE_PORT'], '43210');
    check('the request builder maps the local address', $built->server['SERVER_ADDR'], '127.0.0.1');
    check('the request builder maps the local port', $built->server['SERVER_PORT'], '8080');
    check('the request builder keeps the effective authority', $built->server['HTTP_HOST'], 'example.test:8080');
    check('SERVER_NAME is the effective hostname without its port', $built->server['SERVER_NAME'], 'example.test');
    check('cleartext requests use the http scheme', $built->server['REQUEST_SCHEME'], 'http');
    check('cleartext requests do not set HTTPS', $built->server['HTTPS'], 'off');
    check('an IPv6 effective authority yields an unbracketed SERVER_NAME', hamster_authority_hostname('[2001:db8::1]:8080'), '2001:db8::1');

    // --- field names -----------------------------------------------------
    $target = [];
    hamster_assign_nested($target, 'plain', 'v');
    hamster_assign_nested($target, 'a[b]', 'v2');
    hamster_assign_nested($target, 'a[c][d]', 'v3');
    hamster_assign_nested($target, 'list[]', 'first');
    hamster_assign_nested($target, 'list[]', 'second');
    hamster_assign_nested($target, 'rows[][name]', 'r0');
    hamster_assign_nested($target, 'rows[][name]', 'r1');
    check('nested field names match parse_str', $target, [
        'plain' => 'v',
        'a' => ['b' => 'v2', 'c' => ['d' => 'v3']],
        'list' => ['first', 'second'],
        'rows' => [['name' => 'r0'], ['name' => 'r1']],
    ]);

    $malformed = [];
    hamster_assign_nested($malformed, 'broken[', 'v');
    check('an unterminated bracket follows parse_str normalization', $malformed, ['broken_' => 'v']);

    foreach (['a.b', 'space name', 'n.d[e.f]', 'a[b]ignored'] as $phpName) {
        $actual = [];
        hamster_assign_nested($actual, $phpName, 'v');
        parse_str(rawurlencode($phpName) . '=v', $expected);
        check("field name {$phpName} matches parse_str", $actual, $expected);
    }

    // --- content disposition --------------------------------------------
    check(
        'name and filename are read from a quoted disposition',
        hamster_content_disposition('form-data; name="avatar"; filename="my photo.png"'),
        ['name' => 'avatar', 'filename' => 'my photo.png']
    );
    check(
        'a field without a filename reports no filename at all',
        hamster_content_disposition('form-data; name="note"'),
        ['name' => 'note']
    );
    check(
        'an empty filename is present, which is how an empty file input looks',
        hamster_content_disposition('form-data; name="slot"; filename=""'),
        ['name' => 'slot', 'filename' => '']
    );
    check(
        'filename* does not masquerade as filename',
        hamster_content_disposition("form-data; name=\"f\"; filename*=UTF-8''a.txt"),
        ['name' => 'f']
    );

    // --- boundaries ------------------------------------------------------
    check(
        'a quoted boundary is unquoted',
        hamster_multipart_boundary('multipart/form-data; boundary="a-b-c"'),
        'a-b-c'
    );
    check(
        'a boundary is found after other parameters',
        hamster_multipart_boundary('multipart/form-data; charset=utf-8; boundary=xyz'),
        'xyz'
    );
    check(
        'a missing boundary is reported as missing',
        hamster_multipart_boundary('multipart/form-data'),
        null
    );

    // --- whole bodies ----------------------------------------------------
    $boundary = 'B';
    $binary = "line\r\nwith\x00nulls\xff and --B-ish text";
    $body = "--B\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhello\r\n"
        . "--B\r\nContent-Disposition: form-data; name=\"meta[a]\"\r\n\r\nnested\r\n"
        . "--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n"
        . "Content-Type: application/octet-stream\r\n\r\n{$binary}\r\n"
        . "--B\r\nContent-Disposition: form-data; name=\"slot\"; filename=\"\"\r\n\r\n\r\n"
        . "--B--\r\n";

    [$parameters, $files] = hamster_parse_multipart($body, "multipart/form-data; boundary={$boundary}");
    check('multipart fields become request parameters', $parameters, [
        'note' => 'hello',
        'meta' => ['a' => 'nested'],
    ]);
    check('every file part becomes an upload', array_keys($files), ['file', 'slot']);
    check('a file part keeps its bytes exactly', $files['file']->contents(), $binary);
    check('a file part keeps the client name', $files['file']->originalName, 'a.bin');
    check('a file part keeps the client type', $files['file']->mimeType, 'application/octet-stream');
    check('an upload is built in test mode', $files['file']->test, true);
    check('an upload reports no error', $files['file']->error, UPLOAD_ERR_OK);
    check('an empty file input is UPLOAD_ERR_NO_FILE', $files['slot']->error, UPLOAD_ERR_NO_FILE);
    check('an empty file input has no path', $files['slot']->path, '');

    $looksLikeClosingDelimiter = "alpha\r\n--B--not-a-close\r\nomega";
    $body = "--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"boundary.bin\"\r\n"
        . "Content-Type: application/octet-stream\r\n\r\n{$looksLikeClosingDelimiter}\r\n"
        . "--B--\r\n";
    [, $filesWithBoundaryPrefix] = hamster_parse_multipart(
        $body,
        'multipart/form-data; boundary=B'
    );
    check(
        'a boundary prefix with an invalid suffix remains file content',
        $filesWithBoundaryPrefix['file']->contents(),
        $looksLikeClosingDelimiter
    );

    $temporaries = hamster_upload_temporaries();
    check('one temporary exists per real upload', count($temporaries), 2);
    hamster_discard_uploads();
    check('cleanup removes every temporary', is_file($temporaries[0]), false);
    check('cleanup empties the list', hamster_upload_temporaries(), []);

    $locked = tempnam(sys_get_temp_dir(), 'hamster-locked-upload-');
    if ($locked === false) {
        throw new RuntimeException('cannot create upload cleanup failure probe');
    }
    $paths = &hamster_upload_temporaries();
    $paths[] = $locked;
    $cleanupRejected = null;
    try {
        hamster_discard_uploads(static fn (string $path): bool => false);
    } catch (RuntimeException $error) {
        $cleanupRejected = $error->getMessage();
    }
    check(
        'an upload deletion failure makes cleanup fail closed',
        $cleanupRejected,
        'failed to remove 1 upload temporary file(s)'
    );
    check('failed upload cleanup retains ownership for retry', hamster_upload_temporaries(), [$locked]);
    hamster_discard_uploads();
    check('a retried upload cleanup removes the file', is_file($locked), false);

    // An empty preamble and a CRLF-prefixed first delimiter are both legal.
    [$parameters] = hamster_parse_multipart(
        "preamble\r\n--B\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1\r\n--B--\r\n",
        'multipart/form-data; boundary=B'
    );
    check('a preamble before the first delimiter is ignored', $parameters, ['x' => '1']);

    $rejected = null;
    try {
        hamster_parse_multipart('whatever', 'multipart/form-data');
    } catch (HamsterBadRequest $error) {
        $rejected = $error->getMessage();
    }
    check('a multipart body without a boundary is rejected', $rejected, 'multipart/form-data without a boundary');

    $rejected = null;
    try {
        hamster_parse_multipart(
            "--B\r\nno header terminator\r\n--B--\r\n",
            'multipart/form-data; boundary=B'
        );
    } catch (HamsterBadRequest $error) {
        $rejected = $error->getMessage();
    }
    check('a part without a header block is rejected', $rejected, 'multipart part has no header block');

    $rejected = null;
    try {
        hamster_parse_multipart(
            "--B\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1",
            'multipart/form-data; boundary=B'
        );
    } catch (HamsterBadRequest $error) {
        $rejected = $error->getMessage();
    }
    check(
        'a multipart body without its closing boundary is rejected',
        $rejected,
        'multipart body has no closing boundary'
    );

    hamster_discard_uploads();

    if ($failures > 0) {
        fwrite(STDERR, "{$failures} of {$checks} checks failed\n");
        exit(1);
    }
    fwrite(STDOUT, "{$checks} checks passed\n");
}
