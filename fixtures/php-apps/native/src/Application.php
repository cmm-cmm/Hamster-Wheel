<?php

declare(strict_types=1);

namespace NativeApp;

use Throwable;

/**
 * The application: a front controller with its own routing table, booted once
 * per worker process. Handlers return [status, headers, body].
 */
final class Application
{
    private int $requests = 0;
    private readonly float $bootedAt;
    /** @var array<string, string> worker-local, deliberately not shared */
    private array $memory = [];
    private readonly SessionStore $sessions;
    private bool $flat = false;

    public function __construct(string $varDirectory)
    {
        $this->bootedAt = microtime(true);
        $this->sessions = new SessionStore($varDirectory . '/sessions');
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    public function handle(Request $request): array
    {
        $this->requests++;
        // Tests ask for a flat `key=value` rendering they can read without a
        // JSON parser; people get JSON.
        $this->flat = str_starts_with((string) $request->header('Accept'), 'text/plain');
        try {
            return match ($request->path) {
                '/' => $this->html('<h1>native app</h1><p>pid ' . getmypid() . '</p>'),
                '/state' => $this->json($this->state()),
                '/echo' => $this->json($this->echo($request)),
                '/cookies' => $this->cookies(),
                '/bytes' => $this->bytes((int) ($request->query['n'] ?? 0)),
                '/kv' => $this->json($this->kv($request)),
                '/session' => $this->session($request),
                '/sleep' => $this->sleep((int) ($request->query['ms'] ?? 0)),
                '/stdout' => $this->stdout(),
                '/error' => throw new \RuntimeException('handled failure'),
                '/exit' => exit(9),
                '/oom' => $this->exhaustMemory(),
                default => [404, [['Content-Type', 'text/plain']], 'no route for ' . $request->path],
            };
        } catch (Throwable $error) {
            return [500, [['Content-Type', 'text/plain']], 'application error: ' . $error->getMessage()];
        }
    }

    /** @return array<string, mixed> */
    private function state(): array
    {
        return [
            'pid' => getmypid(),
            'requests' => $this->requests,
            'uptime_ms' => (int) ((microtime(true) - $this->bootedAt) * 1000),
            'memory' => memory_get_usage(),
        ];
    }

    /** @return array<string, mixed> */
    private function echo(Request $request): array
    {
        $files = [];
        $this->collectFiles($request->parts['files'], '', $files);

        return [
            'method' => $request->method,
            'uri' => $request->decoded['uri'],
            'scheme' => $request->decoded['scheme'],
            'host' => $request->parts['server']['HTTP_HOST'],
            'server_name' => $request->parts['server']['SERVER_NAME'],
            'remote_addr' => $request->parts['server']['REMOTE_ADDR'],
            'query' => $request->query,
            'headers' => $request->headers,
            'cookies' => $request->parts['cookies'],
            'fields' => $request->parts['parameters'],
            'files' => $files,
            'body_length' => strlen($request->parts['body']),
            'body_fnv1a64' => hash('fnv1a64', $request->parts['body']),
        ];
    }


    /** @param array<string, mixed> $tree @param list<array<string, mixed>> $files */
    private function collectFiles(array $tree, string $prefix, array &$files): void
    {
        foreach ($tree as $key => $value) {
            $field = $prefix === '' ? (string) $key : "{$prefix}[{$key}]";
            if (is_array($value) && array_key_exists('tmp_name', $value)) {
                $files[] = [
                    'field' => $field,
                    'name' => $value['name'],
                    'type' => $value['type'],
                    'error' => $value['error'],
                    'size' => $value['size'],
                    'fnv1a64' => $value['error'] === UPLOAD_ERR_OK ? hash_file('fnv1a64', $value['tmp_name']) : null,
                ];
            } elseif (is_array($value)) {
                $this->collectFiles($value, $field, $files);
            }
        }
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function cookies(): array
    {
        return [200, [
            ['Content-Type', 'text/plain'],
            ['Set-Cookie', 'first=1; Path=/; HttpOnly'],
            ['Set-Cookie', 'second=two%20words; Path=/; SameSite=Lax'],
            ['X-Multi', 'a'],
            ['X-Multi', 'b'],
            // Hamster owns these; the adapter drops them.
            ['Content-Length', '999'],
            ['Connection', 'close'],
        ], 'cookies set'];
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function bytes(int $count): array
    {
        $count = max(0, min($count, 64 * 1024 * 1024));
        $pattern = implode('', array_map('chr', range(0, 255)));

        return [200, [['Content-Type', 'application/octet-stream']], substr(str_repeat($pattern, intdiv($count, 256) + 1), 0, $count)];
    }

    /** @return array<string, mixed> */
    private function kv(Request $request): array
    {
        $key = (string) ($request->query['key'] ?? '');
        if (isset($request->query['set'])) {
            $this->memory[$key] = (string) $request->query['set'];
        }

        return ['pid' => getmypid(), 'key' => $key, 'value' => $this->memory[$key] ?? null];
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function session(Request $request): array
    {
        $id = $request->cookie('nsid');
        $cookie = [];
        if ($id === null || ! $this->sessions->exists($id)) {
            $id = $this->sessions->create();
            $cookie[] = ['Set-Cookie', "nsid={$id}; Path=/; HttpOnly; SameSite=Lax"];
        }
        $visits = $this->sessions->increment($id, 'visits');
        // Holding the worker lets a test make concurrent requests land on
        // different workers of the pool.
        usleep(max(0, min((int) ($request->query['hold'] ?? 0), 2_000)) * 1000);
        if (isset($request->query['name'])) {
            $this->sessions->set($id, 'name', (string) $request->query['name']);
        }

        [$status, $rendered, $body] = $this->json([
            'pid' => getmypid(),
            'visits' => $visits,
            'name' => $this->sessions->get($id, 'name'),
        ]);

        return [$status, array_merge($rendered, $cookie), $body];
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function sleep(int $milliseconds): array
    {
        usleep(max(0, min($milliseconds, 30_000)) * 1000);

        return [200, [['Content-Type', 'text/plain']], 'slept'];
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function stdout(): array
    {
        // Everything an application might do to standard output. None of it
        // may reach the protocol pipe.
        $attempts = [
            'echo' => static function (): void { echo 'echoed'; },
            'print' => static function (): void { print 'printed'; },
            'php://output' => static function (): void { file_put_contents('php://output', 'output-wrapper'); },
            'php://stdout' => static function (): void {
                $stream = fopen('php://stdout', 'wb');
                fwrite($stream, 'reopened');
                fclose($stream);
            },
            'STDOUT constant' => static function (): void { fwrite(STDOUT, 'direct'); },
        ];
        $outcome = [];
        foreach ($attempts as $name => $attempt) {
            try {
                $attempt();
                $outcome[$name] = 'ok';
            } catch (Throwable $error) {
                $outcome[$name] = get_class($error) . ': ' . $error->getMessage();
            }
        }

        return $this->json($outcome);
    }

    /** @return never */
    private function exhaustMemory(): array
    {
        ini_set('memory_limit', '64M');
        $chunks = [];
        while (true) {
            $chunks[] = str_repeat('x', 1024 * 1024);
        }
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function html(string $body): array
    {
        return [200, [['Content-Type', 'text/html; charset=utf-8']], $body];
    }

    /** @return array{0:int, 1:list<array{0:string,1:string}>, 2:string} */
    private function json(array $data): array
    {
        if (! $this->flat) {
            return [200, [['Content-Type', 'application/json']], json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES)];
        }
        $lines = [];
        $flatten = static function (array $values, string $prefix) use (&$flatten, &$lines): void {
            foreach ($values as $key => $value) {
                $name = $prefix === '' ? (string) $key : "{$prefix}.{$key}";
                if (is_array($value)) {
                    $flatten($value, $name);
                } else {
                    $lines[] = $name . '=' . match (true) {
                        $value === null => 'null',
                        is_bool($value) => $value ? 'true' : 'false',
                        default => str_replace(["\r", "\n"], ['\r', '\n'], (string) $value),
                    };
                }
            }
        };
        $flatten($data, '');

        return [200, [['Content-Type', 'text/plain; charset=utf-8']], implode("\n", $lines) . "\n"];
    }
}
