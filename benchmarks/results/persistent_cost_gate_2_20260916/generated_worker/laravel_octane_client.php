<?php

declare(strict_types=1);

use Illuminate\Foundation\Application;
use Illuminate\Http\Request;
use Laravel\Octane\Contracts\Client;
use Laravel\Octane\Contracts\StoppableClient;
use Laravel\Octane\OctaneResponse;
use Laravel\Octane\RequestContext;

/**
 * Hamster protocol adapter for Laravel Octane.
 *
 * Responses are encoded but not written from respond(). Octane still has to
 * run terminating middleware and lifecycle listeners after that call. Rust
 * must not make this generation reusable until those operations and Hamster's
 * resource boundary have succeeded, so the worker explicitly flushes the
 * staged frame afterwards.
 */
final class HamsterOctaneClient implements Client, StoppableClient
{
    private ?string $pendingFrame = null;
    private bool $stopped = false;
    private bool $cleanupCompleted = false;

    /** @param resource $stdout */
    public function __construct(private $stdout) {}

    public function marshalRequest(RequestContext $context): array
    {
        return [$context->request, $context];
    }

    public function respond(RequestContext $context, OctaneResponse $octaneResponse): void
    {
        $response = $octaneResponse->response;
        $body = hamster_response_body($response);
        if ($octaneResponse->outputBuffer !== null
            && $octaneResponse->outputBuffer !== ''
            && ! $response instanceof Symfony\Component\HttpFoundation\StreamedResponse
            && ! $response instanceof Symfony\Component\HttpFoundation\BinaryFileResponse) {
            $body = $octaneResponse->outputBuffer . $body;
        }

        $this->stage($context->id, $response->getStatusCode(), hamster_response_headers($response), $body);
    }

    public function error(
        \Throwable $error,
        Application $app,
        Request $request,
        RequestContext $context
    ): void {
        $this->stopped = true;
        $this->stage($context->id, 500, [['Content-Type', 'text/plain; charset=UTF-8']], 'Internal Server Error');
    }

    public function stop(): void
    {
        $this->stopped = true;
    }

    public function shouldStop(): bool
    {
        return $this->stopped;
    }

    /** Open the protocol commit gate after every request cleanup succeeded. */
    public function markCleanupCompleted(): void
    {
        if ($this->pendingFrame === null) {
            throw new RuntimeException('cannot complete cleanup without a staged response');
        }
        $this->cleanupCompleted = true;
    }

    public function flushResponse(): void
    {
        if ($this->pendingFrame === null) {
            throw new RuntimeException('Octane completed without producing a response');
        }
        if (! $this->cleanupCompleted) {
            throw new RuntimeException('refusing to flush a response before cleanup completed');
        }
        $frame = $this->pendingFrame;
        $this->pendingFrame = null;
        $this->cleanupCompleted = false;
        hamster_write_frame($this->stdout, $frame);
    }

    /** @param list<array{0:string,1:string}> $headers */
    private function stage(int $id, int $status, array $headers, string $body): void
    {
        if ($this->pendingFrame !== null) {
            throw new RuntimeException('Octane attempted to produce more than one response');
        }
        $this->cleanupCompleted = false;
        $this->pendingFrame = hamster_encode_response([
            'id' => $id,
            'status' => $status,
            'headers' => $headers,
            'body' => $body,
        ]);
    }
}
