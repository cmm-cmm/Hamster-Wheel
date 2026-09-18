<?php

declare(strict_types=1);

namespace App\Providers;

use App\Models\Accesstokenlink;
use Illuminate\Support\Facades\Auth;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\Facades\Route;
use Illuminate\Support\ServiceProvider;
use Laravel\Octane\Events\RequestTerminated;
use Symfony\Component\HttpFoundation\Request as SymfonyRequest;

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

        // Gate 2 control: the same controller path without the `web`
        // middleware group separates kernel/routing/controller work from
        // cookie and file-session middleware. This provider is scratch-only.
        Route::get('/__hamster/bare', [HamsterPmsProbeController::class, 'light']);
        Route::get('/__hamster/metadata', [HamsterPmsProbeController::class, 'metadata']);

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

    /** @return array<string, int|string|bool|null> */
    public function metadata(): array
    {
        $forwarded = SymfonyRequest::HEADER_X_FORWARDED_FOR
            | SymfonyRequest::HEADER_X_FORWARDED_HOST
            | SymfonyRequest::HEADER_X_FORWARDED_PROTO
            | SymfonyRequest::HEADER_X_FORWARDED_PORT;
        // Trust only Hamster's immediate loopback address. A connection from
        // 127.0.0.2/.3 must therefore ignore forged forwarding headers. Reset
        // the process-global Symfony policy before returning from this
        // scratch-only probe.
        SymfonyRequest::setTrustedProxies(['127.0.0.1'], $forwarded);
        try {
            $request = request();

            return [
                'pid' => getmypid(),
                'remote_addr' => $request->server->get('REMOTE_ADDR'),
                'remote_port' => $request->server->get('REMOTE_PORT'),
                'server_addr' => $request->server->get('SERVER_ADDR'),
                'server_port' => $request->server->get('SERVER_PORT'),
                'server_protocol' => $request->server->get('SERVER_PROTOCOL'),
                'server_name' => $request->server->get('SERVER_NAME'),
                'http_host' => $request->server->get('HTTP_HOST'),
                'request_scheme' => $request->server->get('REQUEST_SCHEME'),
                'https' => $request->server->get('HTTPS'),
                'client_ip' => $request->getClientIp(),
                'host' => $request->getHost(),
                'scheme' => $request->getScheme(),
                'port' => $request->getPort(),
                'forwarded_for' => $request->headers->get('X-Forwarded-For'),
            ];
        } finally {
            SymfonyRequest::setTrustedProxies([], $forwarded);
        }
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
