<?php

declare(strict_types=1);

use Laravel\Octane\Events\RequestHandled;
use Laravel\Octane\Events\RequestReceived;
use Laravel\Octane\Events\RequestTerminated;

/**
 * Gate 2 measurement only. This file is loaded only by the generated profiling
 * worker; the production Laravel worker never includes it.
 *
 * Records are sampled by protocol request ID and buffered in memory. A
 * measurement-only request dumps them after the benchmark has already read
 * CPU/resource counters, so filesystem work cannot perturb the timed window.
 */
final class HamsterGate2Profiler
{
    /** @var array<string, int>|null */
    private ?array $active = null;

    /** @var list<array<string, int|float>> */
    private array $records = [];

    public function __construct(
        private string $outputDirectory,
        private int $sampleRate,
        private int $minimumRequestId,
    ) {}

    public static function fromEnvironment(): ?self
    {
        if (getenv('HAMSTER_GATE2_PROFILE') !== '1') {
            return null;
        }

        $output = getenv('HAMSTER_GATE2_PROFILE_OUTPUT');
        if (! is_string($output) || $output === '' || ! is_dir($output)) {
            throw new RuntimeException('HAMSTER_GATE2_PROFILE_OUTPUT must identify an existing directory');
        }

        $sampleRate = filter_var(getenv('HAMSTER_GATE2_SAMPLE_RATE') ?: '8', FILTER_VALIDATE_INT);
        $minimumId = filter_var(getenv('HAMSTER_GATE2_MIN_REQUEST_ID') ?: '1', FILTER_VALIDATE_INT);
        if (! is_int($sampleRate) || $sampleRate < 1
            || ! is_int($minimumId) || $minimumId < 1) {
            throw new RuntimeException('invalid Gate 2 profiling configuration');
        }

        return new self(rtrim($output, "/\\"), $sampleRate, $minimumId);
    }

    public function register($app, Laravel\Octane\Worker $worker): void
    {
        $events = $app->make(Illuminate\Contracts\Events\Dispatcher::class);
        // Registration occurs after boot, so these markers run after Octane's
        // configured listeners and any application listeners registered then.
        $events->listen(RequestReceived::class, fn () => $this->mark('request_prepared'));
        $events->listen(RequestHandled::class, fn () => $this->mark('request_handled'));
        $events->listen(RequestTerminated::class, fn () => $this->mark('request_terminated'));
        $worker->onRequestHandled(fn () => $this->mark('response_staged'));
    }

    public function begin(
        int $requestId,
        int $frameReceived,
        int $decoded,
        int $reconstructed,
        int $handleStarted,
    ): void {
        $this->active = null;
        if ($requestId < $this->minimumRequestId || $requestId % $this->sampleRate !== 0) {
            return;
        }

        $this->active = [
            'id' => $requestId,
            'pid' => getmypid(),
            'frame_received' => $frameReceived,
            'decoded' => $decoded,
            'reconstructed' => $reconstructed,
            'handle_started' => $handleStarted,
        ];
    }

    public function mark(string $name): void
    {
        if ($this->active !== null) {
            $this->active[$name] = hrtime(true);
        }
    }

    public function finish(): void
    {
        if ($this->active === null) {
            return;
        }

        $this->active['response_committed'] = hrtime(true);
        $required = [
            'frame_received', 'decoded', 'reconstructed', 'handle_started',
            'request_prepared', 'request_handled', 'response_staged',
            'request_terminated', 'octane_returned', 'hamster_cleanup',
            'response_commit_started', 'response_committed',
        ];
        foreach ($required as $name) {
            if (! isset($this->active[$name])) {
                throw new RuntimeException("Gate 2 profiling boundary {$name} was not observed");
            }
        }

        $pairs = [
            'frame_decode_us' => ['frame_received', 'decoded'],
            'request_reconstruction_us' => ['decoded', 'reconstructed'],
            'worker_guard_setup_us' => ['reconstructed', 'handle_started'],
            'octane_prepare_us' => ['handle_started', 'request_prepared'],
            'laravel_handle_us' => ['request_prepared', 'request_handled'],
            'response_stage_us' => ['request_handled', 'response_staged'],
            'octane_terminate_us' => ['response_staged', 'request_terminated'],
            'octane_tail_us' => ['request_terminated', 'octane_returned'],
            'hamster_cleanup_us' => ['octane_returned', 'hamster_cleanup'],
            'response_commit_us' => ['response_commit_started', 'response_committed'],
            'php_total_us' => ['frame_received', 'response_committed'],
        ];
        $record = ['id' => $this->active['id'], 'pid' => $this->active['pid']];
        foreach ($pairs as $name => [$start, $end]) {
            $record[$name] = ($this->active[$end] - $this->active[$start]) / 1000;
        }
        $this->records[] = $record;
        $this->active = null;
    }

    public function dump(): void
    {
        $path = $this->outputDirectory . DIRECTORY_SEPARATOR . 'php-' . getmypid() . '.json';
        $json = json_encode($this->records, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
        if (file_put_contents($path, $json, LOCK_EX) === false) {
            throw new RuntimeException('cannot write Gate 2 profiling records');
        }
    }
}
