<?php

declare(strict_types=1);

/**
 * Request reconstruction for the persistent Laravel worker.
 *
 * The SAPI-equivalent work (server bag, cookies, form bodies, multipart and
 * upload temporaries) is framework-neutral and lives in adapter_support.php;
 * the HttpFoundation mapping is shared with the Symfony adapter in
 * httpfoundation.php. This file only names Laravel's own request and upload
 * classes. Everything Laravel later relies on -- sessions, CSRF, form input,
 * uploads -- is downstream of getting that work right.
 */

require_once __DIR__ . '/adapter_support.php';
require_once __DIR__ . '/httpfoundation.php';

use Illuminate\Http\Request;
use Illuminate\Http\UploadedFile;

/**
 * Rebuilds an Illuminate request from a decoded protocol frame.
 *
 * @param  array{http_version:string,scheme:string,peer_addr:string,peer_port:int,local_addr:string,local_port:int,method:string,uri:string,query:string,headers:list<array{0:string,1:string}>,body:string}  $decoded
 *
 * @throws HamsterBadRequest when the client's body cannot be parsed
 */
function hamster_build_request(array $decoded): Request
{
    return hamster_httpfoundation_request($decoded, Request::class, UploadedFile::class);
}

/**
 * Multipart parsing with Laravel upload objects.
 *
 * @return array{0: array<string, mixed>, 1: array<string, mixed>}
 *
 * @throws HamsterBadRequest
 */
function hamster_parse_multipart(string $body, string $contentType): array
{
    return hamster_parse_multipart_with($body, $contentType, 'hamster_uploaded_file');
}

function hamster_uploaded_file(string $originalName, string $content, ?string $mimeType): UploadedFile
{
    return hamster_httpfoundation_file(UploadedFile::class, $originalName, $content, $mimeType);
}
