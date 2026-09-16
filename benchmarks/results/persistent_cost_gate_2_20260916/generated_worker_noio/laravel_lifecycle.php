<?php

declare(strict_types=1);

use Carbon\Carbon;
use Illuminate\Database\Eloquent\Model;
use Illuminate\Foundation\Application;

/**
 * Process-global and external-resource cleanup which is outside Octane's
 * application sandbox.
 *
 * Container ownership belongs to Octane. This class deliberately does not
 * inspect or restore the container's instance table. It only handles state
 * that cloning a container cannot own: open database transactions, framework
 * static switches, Carbon's process-global test clock, process cwd/timezone,
 * and the initial scoped instances which may have been resolved while the base
 * application booted.
 */
final class HamsterLaravelLifecycle
{
    /** @var array{unguarded:bool,lazy_loading:bool,discarding:bool,missing:bool} */
    private array $modelBaseline;

    private mixed $carbonBaseline;

    private string $timezoneBaseline;

    private string $workingDirectoryBaseline;

    public function __construct(private Application $app)
    {
        $this->modelBaseline = [
            'unguarded' => Model::isUnguarded(),
            'lazy_loading' => Model::preventsLazyLoading(),
            'discarding' => Model::preventsSilentlyDiscardingAttributes(),
            'missing' => Model::preventsAccessingMissingAttributes(),
        ];
        $testNow = Carbon::getTestNow();
        $this->carbonBaseline = is_object($testNow) && ! $testNow instanceof Closure
            ? clone $testNow
            : $testNow;
        $this->timezoneBaseline = date_default_timezone_get();
        $workingDirectory = getcwd();
        if ($workingDirectory === false) {
            throw new RuntimeException('cannot record the worker working directory');
        }
        $this->workingDirectoryBaseline = $workingDirectory;

        // Octane normally forgets scoped instances when an operation ends.
        // There is no preceding operation before request one, so a scoped
        // binding resolved by a booting provider would otherwise enter that
        // first request from the base application.
        $this->app->forgetScopedInstances();
    }

    /**
     * Complete the request boundary before its response becomes visible to
     * Rust. Any exception escapes and poisons the worker generation.
     */
    public function completeRequest(): void
    {
        if ($this->app->resolved('db')) {
            foreach ($this->app->make('db')->getConnections() as $connection) {
                while ($connection->transactionLevel() > 0) {
                    $connection->rollBack();
                }
            }
        }

        // This is idempotent with Octane's OperationTerminated listener and
        // protects the contract even when a scope was created during a late
        // terminating callback.
        $this->app->forgetScopedInstances();

        if (getcwd() !== $this->workingDirectoryBaseline
            && ! chdir($this->workingDirectoryBaseline)) {
            throw new RuntimeException('cannot restore the worker working directory');
        }
        if (date_default_timezone_get() !== $this->timezoneBaseline
            && ! date_default_timezone_set($this->timezoneBaseline)) {
            throw new RuntimeException('cannot restore the worker default timezone');
        }
        Carbon::setTestNow($this->carbonBaseline);

        if (Model::isUnguarded() !== $this->modelBaseline['unguarded']) {
            Model::unguard($this->modelBaseline['unguarded']);
        }
        if (Model::preventsLazyLoading() !== $this->modelBaseline['lazy_loading']) {
            Model::preventLazyLoading($this->modelBaseline['lazy_loading']);
        }
        if (Model::preventsSilentlyDiscardingAttributes() !== $this->modelBaseline['discarding']) {
            Model::preventSilentlyDiscardingAttributes($this->modelBaseline['discarding']);
        }
        if (Model::preventsAccessingMissingAttributes() !== $this->modelBaseline['missing']) {
            Model::preventAccessingMissingAttributes($this->modelBaseline['missing']);
        }
    }
}

/**
 * True when every required listener occurs in the configured order.
 *
 * Custom listeners may be interleaved, but reordering Octane's state setup is
 * unsafe: later listeners consume application/config/request objects prepared
 * by earlier listeners.
 *
 * @param list<class-string> $configured
 * @param list<class-string> $required
 */
function hamster_has_ordered_listeners(array $configured, array $required): bool
{
    $next = 0;
    foreach ($configured as $listener) {
        if (isset($required[$next]) && $listener === $required[$next]) {
            $next++;
        }
    }

    return $next === count($required);
}

/**
 * Request preparation must run before any application-defined listener can
 * observe the shallow sandbox and mutate still-shared framework objects.
 *
 * @param list<class-string> $configured
 * @param list<class-string> $required
 */
function hamster_starts_with_listeners(array $configured, array $required): bool
{
    return array_slice($configured, 0, count($required)) === $required;
}

/** Fail closed if published Octane config weakens its lifecycle contract. */
function hamster_validate_octane_lifecycle(Application $app): void
{
    $listeners = $app->make('config')->get('octane.listeners', []);
    $requestReceived = $listeners[Laravel\Octane\Events\RequestReceived::class] ?? [];
    $operationTerminated = $listeners[Laravel\Octane\Contracts\OperationTerminated::class] ?? [];
    $workerErrors = $listeners[Laravel\Octane\Events\WorkerErrorOccurred::class] ?? [];

    $requiredReceived = [
        ...Laravel\Octane\Octane::prepareApplicationForNextOperation(),
        ...Laravel\Octane\Octane::prepareApplicationForNextRequest(),
    ];
    if (! hamster_starts_with_listeners($requestReceived, $requiredReceived)) {
        throw new RuntimeException(
            'Octane RequestReceived lifecycle must begin with the complete default listener sequence'
        );
    }

    $requiredTerminated = [
        Laravel\Octane\Listeners\FlushOnce::class,
        Laravel\Octane\Listeners\FlushTemporaryContainerInstances::class,
    ];
    if (! hamster_has_ordered_listeners($operationTerminated, $requiredTerminated)) {
        throw new RuntimeException(
            'Octane OperationTerminated lifecycle listeners are missing or out of order'
        );
    }

    if (! in_array(
        Laravel\Octane\Listeners\StopWorkerIfNecessary::class,
        $workerErrors,
        true
    )) {
        throw new RuntimeException('Octane must stop the worker after a lifecycle error');
    }
}
