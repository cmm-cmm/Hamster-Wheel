<?php

declare(strict_types=1);

namespace NativeApp;

/** One request, as the worker decoded it. Read-only. */
final class Request
{
    public readonly string $method;
    public readonly string $path;
    /** @var array<string, mixed> */
    public readonly array $query;
    /** @var list<array{0:string,1:string}> */
    public readonly array $headers;

    /**
     * @param array<string, mixed> $decoded
     * @param array<string, mixed> $parts
     */
    public function __construct(public readonly array $decoded, public readonly array $parts)
    {
        $this->method = $decoded['method'];
        $this->path = $decoded['path'];
        $this->query = $parts['query'];
        $this->headers = $decoded['headers'];
    }

    public function header(string $name): ?string
    {
        foreach ($this->headers as [$field, $value]) {
            if (strcasecmp($field, $name) === 0) {
                return $value;
            }
        }

        return null;
    }

    public function cookie(string $name): ?string
    {
        return $this->parts['cookies'][$name] ?? null;
    }
}
