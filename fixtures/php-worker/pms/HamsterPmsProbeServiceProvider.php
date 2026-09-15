<?php

declare(strict_types=1);

namespace App\Providers;

use App\Models\Accesstokenlink;
use Illuminate\Support\Facades\Auth;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Route;
use Illuminate\Support\ServiceProvider;
use Laravel\Octane\Events\RequestTerminated;

/**
 * Scratch-only PMS routes used by Hamster's ignored end-to-end tests.
 *
 * This provider must never be registered in the production PMS checkout. The
 * seed endpoint selects an existing account read-only and writes only to the
 * scratch application's configured session store.
 */
final class HamsterPmsProbeServiceProvider extends ServiceProvider
{
    public function boot(): void
    {
        if (getenv('HAMSTER_PMS_PROBE') !== '1') {
            throw new \RuntimeException('scratch PMS probe provider is disabled');
        }

        Route::middleware('web')->group(function (): void {
            Route::get('/__hamster/auth-seed', [HamsterPmsProbeController::class, 'seed']);
            Route::get('/__hamster/auth-state', [HamsterPmsProbeController::class, 'authState'])
                ->middleware('auth');
            Route::get('/__hamster/light', [HamsterPmsProbeController::class, 'light']);
            Route::get('/__hamster/fail-cleanup', [HamsterPmsProbeController::class, 'failCleanup']);
        });

        Event::listen(RequestTerminated::class, static function (RequestTerminated $event): void {
            if ($event->request->headers->get('X-Hamster-Fail-Cleanup') === '1') {
                throw new \RuntimeException('injected PMS lifecycle cleanup failure');
            }
        });
    }
}

final class HamsterPmsProbeController
{
    private int $calls = 0;

    /** @return array{id:int|string|null} */
    public function seed(): array
    {
        $user = Accesstokenlink::query()
            ->whereHas('taikhoan')
            ->where(static function ($query): void {
                $query->whereNull('AccessLife')->orWhere('AccessLife', '>=', now());
            })
            ->firstOrFail();
        Auth::login($user, false);

        return ['id' => Auth::id()];
    }

    /** @return array{id:int|string|null} */
    public function authState(): array
    {
        return ['id' => Auth::id()];
    }

    /** @return array{pid:int,controller_calls:int} */
    public function light(): array
    {
        $delayMilliseconds = request()->headers->get('X-Hamster-Probe-Delay-Ms');
        if (is_string($delayMilliseconds) && ctype_digit($delayMilliseconds)) {
            usleep(min((int) $delayMilliseconds, 250) * 1000);
        }

        return ['pid' => getmypid(), 'controller_calls' => ++$this->calls];
    }

    /** @return array{executions:int} */
    public function failCleanup(): array
    {
        $marker = getenv('HAMSTER_PMS_FAILURE_MARKER');
        if (! is_string($marker) || $marker === '') {
            throw new \RuntimeException('HAMSTER_PMS_FAILURE_MARKER is required');
        }
        $written = file_put_contents($marker, "executed\n", FILE_APPEND | LOCK_EX);
        if ($written === false) {
            throw new \RuntimeException('cannot write the PMS failure marker');
        }

        return ['executions' => 1];
    }
}
