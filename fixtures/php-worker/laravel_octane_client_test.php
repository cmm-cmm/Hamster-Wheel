<?php

declare(strict_types=1);

namespace Illuminate\Foundation {
    class Application {}
}

namespace Illuminate\Http {
    class Request {}
}

namespace Symfony\Component\HttpFoundation {
    class HeaderBag
    {
        public function allPreserveCase(): array
        {
            return ['Content-Type' => ['text/plain']];
        }
    }

    class Response
    {
        public HeaderBag $headers;

        public function __construct(private string $content, private int $status = 200)
        {
            $this->headers = new HeaderBag();
        }

        public function getContent(): string { return $this->content; }
        public function getStatusCode(): int { return $this->status; }
    }

    class StreamedResponse extends Response {}
    class BinaryFileResponse extends Response {}
}

namespace Laravel\Octane {
    class RequestContext
    {
        public function __construct(public array $data = []) {}
        public function __get(string $key): mixed { return $this->data[$key]; }
    }

    class OctaneResponse
    {
        public function __construct(public $response, public ?string $outputBuffer = null) {}
    }
}

namespace Laravel\Octane\Contracts {
    interface Client
    {
        public function marshalRequest(\Laravel\Octane\RequestContext $context): array;
        public function respond(\Laravel\Octane\RequestContext $context, \Laravel\Octane\OctaneResponse $response): void;
        public function error(\Throwable $error, \Illuminate\Foundation\Application $app, \Illuminate\Http\Request $request, \Laravel\Octane\RequestContext $context): void;
    }

    interface StoppableClient extends Client
    {
        public function stop(): void;
    }
}

namespace {
    require __DIR__ . '/protocol.php';
    require __DIR__ . '/adapter_support.php';

    function hamster_response_headers($response): array
    {
        return [['Content-Type', 'text/plain']];
    }

    function hamster_response_body($response): string
    {
        return $response->getContent();
    }

    require __DIR__ . '/laravel_octane_client.php';

    $checks = 0;
    $failures = 0;
    function check(string $name, mixed $actual, mixed $expected): void
    {
        global $checks, $failures;
        $checks++;
        if ($actual !== $expected) {
            $failures++;
            fwrite(STDERR, "FAIL {$name}: expected " . var_export($expected, true)
                . ', got ' . var_export($actual, true) . "\n");
        }
    }

    $stream = fopen('php://memory', 'w+b');
    $client = new HamsterOctaneClient($stream);
    $context = new \Laravel\Octane\RequestContext(['id' => 41, 'request' => new \Illuminate\Http\Request()]);
    $client->respond($context, new \Laravel\Octane\OctaneResponse(
        new \Symfony\Component\HttpFoundation\Response('body'),
        'prefix-'
    ));
    check('respond stages without making bytes visible', ftell($stream), 0);
    $earlyFlushRejected = null;
    try {
        $client->flushResponse();
    } catch (RuntimeException $error) {
        $earlyFlushRejected = $error->getMessage();
    }
    check(
        'flush is rejected before cleanup completes',
        $earlyFlushRejected,
        'refusing to flush a response before cleanup completed'
    );
    check('rejected early flush emits no bytes', ftell($stream), 0);
    $client->markCleanupCompleted();
    $client->flushResponse();
    check('flush makes one frame visible', ftell($stream) > 4, true);

    rewind($stream);
    $length = unpack('Nlength', fread($stream, 4))['length'];
    $payload = fread($stream, $length);
    $offset = 1;
    check('staged response kind', ord($payload[0]), HAMSTER_RESPONSE);
    check('staged response id', hamster_u64($payload, $offset), 41);
    check('staged response status', hamster_u16($payload, $offset), 200);
    $headerCount = hamster_u16($payload, $offset);
    for ($i = 0; $i < $headerCount; $i++) {
        hamster_bytes_u16($payload, $offset);
        hamster_bytes_u32($payload, $offset);
    }
    check('ordinary output is prefixed exactly once', hamster_bytes_u32($payload, $offset), 'prefix-body');

    $retired = fopen('php://memory', 'w+b');
    $client = new HamsterOctaneClient($retired);
    $client->respond($context, new \Laravel\Octane\OctaneResponse(
        new \Symfony\Component\HttpFoundation\Response('success')
    ));
    $client->stop();
    check('retiring client leaves staged response invisible', ftell($retired), 0);
    check('stopped client retires generation', $client->shouldStop(), true);

    $large = fopen('php://memory', 'w+b');
    $client = new HamsterOctaneClient($large);
    $client->respond($context, new \Laravel\Octane\OctaneResponse(
        new \Symfony\Component\HttpFoundation\Response(str_repeat('x', HAMSTER_MAX_FRAME))
    ));
    $client->markCleanupCompleted();
    $client->flushResponse();
    rewind($large);
    $length = unpack('Nlength', fread($large, 4))['length'];
    $payload = fread($large, $length);
    $offset = 9;
    check('an oversized response is answered 500', hamster_u16($payload, $offset), 500);
    check('the replacement frame fits the protocol', $length <= HAMSTER_MAX_FRAME, true);

    if ($failures > 0) {
        fwrite(STDERR, "{$failures} of {$checks} checks failed\n");
        exit(1);
    }
    fwrite(STDOUT, "{$checks} checks passed\n");
}
