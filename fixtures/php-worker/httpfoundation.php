<?php

declare(strict_types=1);

/**
 * Symfony HttpFoundation request and response mapping, shared by every adapter
 * whose framework is built on it (the Laravel and Symfony HttpKernel adapters).
 * Requires adapter_support.php and an autoloader providing symfony/http-foundation.
 *
 * `Request::create()` is deliberately not used. It is a test helper: it builds
 * its cookie bag from an argument rather than the Cookie header, and it takes
 * form parameters as an argument while ignoring the body entirely. Both
 * defects are silent, and both were observed against a real application as a
 * login that always answered 419.
 */

use Symfony\Component\HttpFoundation\BinaryFileResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\StreamedResponse;
use Symfony\Component\HttpFoundation\File\UploadedFile;

/**
 * Builds a request of `$requestClass` (Request or a subclass) from a decoded
 * frame. Uploads are `$fileClass` instances in test mode, since
 * `is_uploaded_file()` answers only for the SAPI's own upload temporaries and
 * would make every upload invalid and unmovable.
 *
 * The classes are named rather than typed so the standalone PHP checks can
 * substitute stubs.
 *
 * @template T of Request
 * @param  class-string<T>  $requestClass
 * @param  class-string<UploadedFile>  $fileClass
 * @return T
 *
 * @throws HamsterBadRequest
 */
function hamster_httpfoundation_request(array $decoded, string $requestClass, string $fileClass): object
{
    $parts = hamster_request_parts(
        $decoded,
        static fn (string $name, string $content, ?string $type) => hamster_httpfoundation_file($fileClass, $name, $content, $type)
    );

    return new $requestClass(
        $parts['query'],
        $parts['parameters'],
        [],
        $parts['cookies'],
        $parts['files'],
        $parts['server'],
        $parts['body']
    );
}

/** @param class-string<UploadedFile> $fileClass */
function hamster_httpfoundation_file(string $fileClass, string $originalName, string $content, ?string $mimeType): object
{
    // An empty file input sends a part with an empty filename and no content.
    // PHP reports that as UPLOAD_ERR_NO_FILE rather than an empty upload, and
    // validation rules are written against that distinction.
    if ($originalName === '' && $content === '') {
        return new $fileClass('', '', null, UPLOAD_ERR_NO_FILE, true);
    }

    return new $fileClass(hamster_store_upload($content), $originalName, $mimeType, UPLOAD_ERR_OK, true);
}

/** @return list<array{0:string,1:string}> */
function hamster_response_headers(object $response): array
{
    $headers = [];
    foreach ($response->headers->allPreserveCase() as $name => $values) {
        if (isset(HAMSTER_FORBIDDEN_RESPONSE_HEADERS[strtolower($name)])) {
            continue;
        }
        foreach ($values as $value) {
            $headers[] = [$name, (string) $value];
        }
    }

    return $headers;
}

/**
 * The complete body. A streamed or file response is run into an output buffer:
 * the protocol carries one RESPONSE frame, so nothing can be sent before the
 * application has finished producing it.
 */
function hamster_response_body(object $response): string
{
    if (! $response instanceof StreamedResponse && ! $response instanceof BinaryFileResponse) {
        return (string) $response->getContent();
    }

    $level = ob_get_level();
    ob_start();
    try {
        $response->sendContent();

        return (string) ob_get_contents();
    } finally {
        while (ob_get_level() > $level) {
            ob_end_clean();
        }
    }
}
