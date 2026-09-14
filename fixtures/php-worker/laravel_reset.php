<?php

declare(strict_types=1);

/**
 * Request-boundary reset for a persistent Laravel worker.
 *
 * The list below is not a guess. Each entry corresponds to a probe that leaked
 * across a request boundary on a naive persistent worker; see
 * doc/LARAVEL_WORKER_0_4.md for the measurements. Two findings shaped it:
 *
 *   - Restoring the container's `instances` table wholesale is NOT sufficient
 *     and not safe. Objects shared through it by reference (the config
 *     repository, the event dispatcher, database connections, the auth manager)
 *     keep their own state, and restoring `instances` without also restoring
 *     `reboundCallbacks` leaves a stale callback pointing at a binding that was
 *     just removed, which produced an unbounded url -> router -> routes -> url
 *     resolution loop.
 *   - Laravel already re-points the url generator at the current request
 *     through `rebinding('request')`. Nothing here should duplicate that.
 *
 * The snapshot must be taken from a WARM worker. Deferred providers, the view
 * engine and the url generator's request rebinding are all resolved lazily by
 * the first request, so a snapshot taken before that records a state the worker
 * never returns to.
 */

use Carbon\Carbon;
use Illuminate\Container\Container;
use Illuminate\Database\Connection;
use Illuminate\Database\Eloquent\Model;
use Illuminate\Routing\UrlGenerator;
use Illuminate\Support\Facades\Facade;
use Illuminate\Support\Str;

/**
 * Bindings that hold request state and have no cheap way to be cleared in
 * place, so the whole instance is dropped and rebuilt on demand.
 */
const HAMSTER_REQUEST_SCOPED_BINDINGS = [
    'auth.driver',
    'session.store',
    'redirect',
    \Illuminate\Support\Defer\DeferredCallbackCollection::class,
];

/**
 * Builds the reset closure for a booted, warmed application.
 *
 * @param  string  $strategy  'targeted' for the real reset, 'none' to disable
 *                            it. 'none' exists only so the cost and the risk of
 *                            the reset can be measured against each other; it
 *                            leaks authenticated identity between requests and
 *                            must never be used to serve real traffic.
 */
function hamster_make_reset(string $strategy, $app): callable
{
    if ($strategy === 'none') {
        return static function (): void {};
    }
    if ($strategy !== 'targeted') {
        throw new RuntimeException("unknown reset strategy: {$strategy}");
    }

    $instancesProp = new ReflectionProperty(Container::class, 'instances');
    $reboundProp = new ReflectionProperty(Container::class, 'reboundCallbacks');
    $terminatingProp = new ReflectionProperty(get_class($app), 'terminatingCallbacks');

    $config = $app['config'];
    $cleanConfig = $config->all();
    $cleanLocale = $app->getLocale();

    $view = $app['view'];
    $sharedProp = new ReflectionProperty(get_class($view), 'shared');
    $cleanShared = $sharedProp->getValue($view);

    $events = $app['events'];
    $listenersProp = new ReflectionProperty(get_class($events), 'listeners');
    $cleanListeners = $listenersProp->getValue($events);

    // Eloquent keeps these on the Model class itself, so they outlive every
    // request in the process. The warm value is the baseline rather than the
    // framework default, because an application normally sets strict mode from
    // a service provider and that must survive the reset.
    $cleanUnguarded = Model::isUnguarded();
    $cleanStrict = [
        'lazyLoading' => Model::preventsLazyLoading(),
        'discarding' => Model::preventsSilentlyDiscardingAttributes(),
        'missing' => Model::preventsAccessingMissingAttributes(),
    ];

    // A forced origin or scheme rewrites every link the application generates,
    // for every later request. Route defaults are worse: setRequest() copies
    // them onto the new route generator on purpose, so Laravel itself carries
    // them across the request boundary.
    $url = $app['url'];
    $forcedRootProp = new ReflectionProperty(UrlGenerator::class, 'forcedRoot');
    $forceSchemeProp = new ReflectionProperty(UrlGenerator::class, 'forceScheme');
    $routeGeneratorProp = new ReflectionProperty(UrlGenerator::class, 'routeGenerator');
    $cleanForcedRoot = $forcedRootProp->getValue($url);
    $cleanForceScheme = $forceSchemeProp->getValue($url);
    $cleanUrlDefaults = $url->getDefaultParameters();

    // Per-connection callbacks accumulate on the connection object, which is
    // shared. A connection opened later is baselined the first time it is
    // seen, because whatever opened it registered its callbacks then.
    $beforeExecutingProp = new ReflectionProperty(Connection::class, 'beforeExecutingCallbacks');
    $cleanBeforeExecuting = [];
    if ($app->resolved('db')) {
        foreach ($app['db']->getConnections() as $name => $connection) {
            $cleanBeforeExecuting[$name] = $beforeExecutingProp->getValue($connection);
        }
    }
    // Everything the snapshot needs is resolved by now, and that ordering is
    // load bearing. Resolving a binding registers its rebinding callbacks, so a
    // reboundCallbacks snapshot taken any earlier would be restored over them
    // and delete them permanently. That is not hypothetical: taking it before
    // `$app['url']` removed the url generator's `rebinding('request')`
    // callback, and every later request generated links for the request before
    // it.
    $cleanRebound = $reboundProp->getValue($app);
    // Instance keys a warm worker holds. Anything else was bound during a
    // request and must not survive it.
    $warmInstanceKeys = array_flip(array_keys($instancesProp->getValue($app)));

    $resolvedProp = new ReflectionProperty(Container::class, 'resolved');

    return static function () use (
        $app, $instancesProp, $reboundProp, $terminatingProp, $resolvedProp, $config,
        $cleanConfig, $cleanLocale, $view, $sharedProp, $cleanShared, $events,
        $listenersProp, $cleanListeners, $cleanRebound, $warmInstanceKeys,
        $cleanUnguarded, $cleanStrict, $url, $forcedRootProp, $forceSchemeProp,
        $routeGeneratorProp, $cleanForcedRoot, $cleanForceScheme, $cleanUrlDefaults,
        $beforeExecutingProp, $cleanBeforeExecuting
    ): void {
        // 1. An abandoned transaction must never reach the next request.
        if ($app->resolved('db')) {
            foreach ($app['db']->getConnections() as $name => $connection) {
                while ($connection->transactionLevel() > 0) {
                    $connection->rollBack();
                }
                $connection->flushQueryLog();
                $connection->forgetRecordModificationState();
                if (! array_key_exists($name, $cleanBeforeExecuting)) {
                    $cleanBeforeExecuting[$name] = $beforeExecutingProp->getValue($connection);
                } elseif ($beforeExecutingProp->getValue($connection) !== $cleanBeforeExecuting[$name]) {
                    $beforeExecutingProp->setValue($connection, $cleanBeforeExecuting[$name]);
                }
            }
        }

        // 2. Clear request state inside the managers that expose a way to do
        //    it. The manager objects themselves hold nothing request specific,
        //    and keeping them resolved is measurably cheaper than rebuilding
        //    one per request.
        if ($app->resolved('auth')) {
            $app['auth']->forgetGuards();
        }
        if ($app->resolved('session')) {
            $app['session']->forgetDrivers();
        }
        if ($app->resolved('cookie')) {
            $app['cookie']->flushQueuedCookies();
        }

        // 3. Drop the bindings that have no such affordance.
        foreach (HAMSTER_REQUEST_SCOPED_BINDINGS as $binding) {
            $app->forgetInstance($binding);
            Facade::clearResolvedInstance($binding);
        }

        // 4. Drop anything the request bound into the container itself.
        $instances = $instancesProp->getValue($app);
        $resolved = $resolvedProp->getValue($app);
        $dropped = false;
        foreach ($instances as $key => $_) {
            if (! isset($warmInstanceKeys[$key])) {
                unset($instances[$key], $resolved[$key]);
                Facade::clearResolvedInstance($key);
                $dropped = true;
            }
        }
        if ($dropped) {
            $instancesProp->setValue($app, $instances);
            // Dropping an instance without its resolved flag leaves the
            // container reporting a binding as resolved when it is not, which
            // is what a probe of the queue manager surfaced.
            $resolvedProp->setValue($app, $resolved);
        }

        // 5. Configuration, including the locale that setLocale() writes there.
        $config->set($cleanConfig);
        $app->setLocale($cleanLocale);

        // 6. View data shared during a request.
        $sharedProp->setValue($view, $cleanShared);

        // 7. Listeners registered at runtime.
        $listenersProp->setValue($events, $cleanListeners);

        // 8. Bookkeeping that would otherwise grow without bound.
        $reboundProp->setValue($app, $cleanRebound);
        $terminatingProp->setValue($app, []);

        // 9. Framework statics that freeze time or accumulate.
        Carbon::setTestNow(null);
        Str::flushCache();

        // 10. Eloquent's class-level switches. A request that unguards mass
        //     assignment and never restores it leaves every later request on
        //     this worker able to fill any column. Each is compared before it
        //     is written, so the common case is four boolean reads.
        //     Model::$globalScopes is deliberately NOT restored; see the
        //     limitation in doc/LARAVEL_WORKER_0_4.md.
        if (Model::isUnguarded() !== $cleanUnguarded) {
            Model::unguard($cleanUnguarded);
        }
        if (Model::preventsLazyLoading() !== $cleanStrict['lazyLoading']) {
            Model::preventLazyLoading($cleanStrict['lazyLoading']);
        }
        if (Model::preventsSilentlyDiscardingAttributes() !== $cleanStrict['discarding']) {
            Model::preventSilentlyDiscardingAttributes($cleanStrict['discarding']);
        }
        if (Model::preventsAccessingMissingAttributes() !== $cleanStrict['missing']) {
            Model::preventAccessingMissingAttributes($cleanStrict['missing']);
        }

        // 11. The url generator's forced origin, forced scheme and route
        //     defaults. The generator survives the reset on purpose, so these
        //     have to be put back by hand.
        if ($forcedRootProp->getValue($url) !== $cleanForcedRoot) {
            $forcedRootProp->setValue($url, $cleanForcedRoot);
        }
        if ($forceSchemeProp->getValue($url) !== $cleanForceScheme) {
            $forceSchemeProp->setValue($url, $cleanForceScheme);
        }
        if ($url->getDefaultParameters() !== $cleanUrlDefaults) {
            // There is no public way to drop route defaults, and setRequest()
            // copies them forward, so the route generator is rebuilt and the
            // warm defaults are reapplied.
            $routeGeneratorProp->setValue($url, null);
            if ($cleanUrlDefaults !== []) {
                $url->defaults($cleanUrlDefaults);
            }
        }
    };
}
