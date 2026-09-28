<?php

declare(strict_types=1);

namespace NativeApp;

use RuntimeException;

/**
 * File-backed sessions shared by every worker of the pool, keyed by the `nsid`
 * cookie. PHP's native session module is not used: under the CLI SAPI it
 * cannot send its cookie, so an application that wants sessions in a worker
 * must set the cookie itself, as frameworks do.
 */
final class SessionStore
{
    public function __construct(private readonly string $directory)
    {
        // Every worker of the pool boots at once; another may create it first.
        if (! is_dir($directory) && ! @mkdir($directory, 0o777, true) && ! is_dir($directory)) {
            throw new RuntimeException("cannot create {$directory}");
        }
    }

    public function create(): string
    {
        $id = bin2hex(random_bytes(16));
        $this->write($id, []);

        return $id;
    }

    public function exists(string $id): bool
    {
        return preg_match('/^[0-9a-f]{32}$/D', $id) === 1 && is_file($this->path($id));
    }

    public function get(string $id, string $key): mixed
    {
        return $this->read($id)[$key] ?? null;
    }

    public function set(string $id, string $key, mixed $value): void
    {
        $this->update($id, static function (array $data) use ($key, $value): array {
            $data[$key] = $value;

            return $data;
        });
    }

    public function increment(string $id, string $key): int
    {
        $result = 0;
        $this->update($id, static function (array $data) use ($key, &$result): array {
            $data[$key] = $result = (int) ($data[$key] ?? 0) + 1;

            return $data;
        });

        return $result;
    }

    /** @param callable(array<string, mixed>): array<string, mixed> $change */
    private function update(string $id, callable $change): void
    {
        $handle = fopen($this->path($id), 'c+');
        if ($handle === false) {
            throw new RuntimeException('cannot open session');
        }
        try {
            flock($handle, LOCK_EX);
            $data = json_decode((string) stream_get_contents($handle), true) ?: [];
            $encoded = json_encode($change($data), JSON_THROW_ON_ERROR);
            ftruncate($handle, 0);
            rewind($handle);
            fwrite($handle, $encoded);
            fflush($handle);
        } finally {
            flock($handle, LOCK_UN);
            fclose($handle);
        }
    }

    /** @return array<string, mixed> */
    private function read(string $id): array
    {
        return json_decode((string) @file_get_contents($this->path($id)), true) ?: [];
    }

    /** @param array<string, mixed> $data */
    private function write(string $id, array $data): void
    {
        file_put_contents($this->path($id), json_encode($data, JSON_THROW_ON_ERROR), LOCK_EX);
    }

    private function path(string $id): string
    {
        return $this->directory . '/' . $id . '.json';
    }
}
