<?php

declare(strict_types=1);

/**
 * Adversarial lifecycle test for an Octane-enabled Laravel application.
 *
 * Set HAMSTER_LARAVEL_TEST_PATH to the application root. This intentionally
 * runs on one process/worker so scheduling cannot hide a leak.
 */

$appPath = getenv('HAMSTER_LARAVEL_TEST_PATH');
if (! is_string($appPath) || $appPath === '') {
    throw new RuntimeException('HAMSTER_LARAVEL_TEST_PATH is required');
}
require rtrim($appPath, "/\\") . '/vendor/autoload.php';
if (! class_exists(Laravel\Octane\Worker::class)) {
    throw new RuntimeException('the test application must require laravel/octane');
}

require __DIR__ . '/laravel_lifecycle.php';

final class HamsterLifecycleProbeState
{
    public ?string $value = null;
}

final class HamsterLifecycleProbeController
{
    private int $calls = 0;

    public function __invoke(): array
    {
        return ['controller_calls' => ++$this->calls];
    }
}

function hamster_test_now_value(): ?string
{
    $value = Carbon\Carbon::getTestNow();
    if ($value === null) {
        return null;
    }
    if ($value instanceof Closure) {
        return 'closure:' . spl_object_id($value);
    }

    return $value->format('Y-m-d H:i:s.uP');
}

final class HamsterLifecycleCollector implements Laravel\Octane\Contracts\Client, Laravel\Octane\Contracts\StoppableClient
{
    public array $responses = [];
    public array $httpResponses = [];
    public array $errors = [];
    public bool $stopped = false;

    public function marshalRequest(Laravel\Octane\RequestContext $context): array
    {
        return [$context->request, $context];
    }

    public function respond(Laravel\Octane\RequestContext $context, Laravel\Octane\OctaneResponse $response): void
    {
        $this->httpResponses[] = $response->response;
        $this->responses[] = $response->response->original ?? $response->response->getContent();
    }

    public function error(
        Throwable $error,
        Illuminate\Foundation\Application $app,
        Illuminate\Http\Request $request,
        Laravel\Octane\RequestContext $context
    ): void {
        $this->errors[] = get_class($error) . ': ' . $error->getMessage();
    }

    public function stop(): void
    {
        $this->stopped = true;
    }
}

$client = new HamsterLifecycleCollector();
$worker = new Laravel\Octane\Worker(new Laravel\Octane\ApplicationFactory($appPath), $client);
$worker->boot();
$app = $worker->application();

$app->scoped('hamster.probe.scoped', static fn () => new HamsterLifecycleProbeState());
$app->singleton('hamster.probe.lazy', static fn () => new HamsterLifecycleProbeState());
$app->singleton('hamster.probe.warm', static fn () => new HamsterLifecycleProbeState());
$app->make('hamster.probe.scoped')->value = 'bootstrap';
$app->make('hamster.probe.warm');

$lifecycle = new HamsterLaravelLifecycle($app);
hamster_validate_octane_lifecycle($app);
$app['router']->get('/__hamster/ownership/{value}', function (
    Illuminate\Foundation\Application $current,
    string $value
): array {
    $scoped = $current->make('hamster.probe.scoped');
    $lazy = $current->make('hamster.probe.lazy');
    $warm = $current->make('hamster.probe.warm');
    $before = [
        'scoped' => $scoped->value,
        'lazy' => $lazy->value,
        'warm' => $warm->value,
        'config' => $current['config']->get('hamster.probe'),
        'model' => [
            'unguarded' => Illuminate\Database\Eloquent\Model::isUnguarded(),
            'lazy' => Illuminate\Database\Eloquent\Model::preventsLazyLoading(),
            'discarding' => Illuminate\Database\Eloquent\Model::preventsSilentlyDiscardingAttributes(),
            'missing' => Illuminate\Database\Eloquent\Model::preventsAccessingMissingAttributes(),
        ],
        'carbon' => hamster_test_now_value(),
        'timezone' => date_default_timezone_get(),
        'cwd' => getcwd(),
    ];
    $scoped->value = $value;
    $lazy->value = $value;
    $warm->value = $value;
    $current['config']->set('hamster.probe', $value);
    if ($value === 'alice') {
        Illuminate\Database\Eloquent\Model::unguard(! $before['model']['unguarded']);
        Illuminate\Database\Eloquent\Model::preventLazyLoading(! $before['model']['lazy']);
        Illuminate\Database\Eloquent\Model::preventSilentlyDiscardingAttributes(
            ! $before['model']['discarding']
        );
        Illuminate\Database\Eloquent\Model::preventAccessingMissingAttributes(
            ! $before['model']['missing']
        );
        Carbon\Carbon::setTestNow('2040-01-02 03:04:05');
        date_default_timezone_set('Pacific/Honolulu');
        chdir(sys_get_temp_dir());
    }

    return $before;
});
$app['router']->get('/__hamster/controller', HamsterLifecycleProbeController::class);

foreach (['/__hamster/ownership/alice', '/__hamster/ownership/bob', '/__hamster/controller', '/__hamster/controller'] as $uri) {
    $request = Illuminate\Http\Request::create($uri, 'GET');
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();
}

$checks = [
    'first scoped instance is not inherited from bootstrap' => $client->responses[0]['scoped'] === null,
    'scoped binding is new for request B' => $client->responses[1]['scoped'] === null,
    'lazy singleton is sandbox-local for request A' => $client->responses[0]['lazy'] === null,
    'lazy singleton is sandbox-local for request B' => $client->responses[1]['lazy'] === null,
    'warmed singleton is explicitly worker-owned' => $client->responses[1]['warm'] === 'alice',
    'configuration is cloned per request' => $client->responses[1]['config'] === null,
    'Eloquent process-global switches return to the boot baseline' =>
        $client->responses[1]['model'] === $client->responses[0]['model'],
    'Carbon test clock returns to the boot baseline' =>
        $client->responses[1]['carbon'] === $client->responses[0]['carbon'],
    'default timezone returns to the boot baseline' =>
        $client->responses[1]['timezone'] === $client->responses[0]['timezone'],
    'working directory returns to the boot baseline' =>
        $client->responses[1]['cwd'] === $client->responses[0]['cwd'],
    'controller instance is new for request A' => $client->responses[2]['controller_calls'] === 1,
    'controller instance is new for request B' => $client->responses[3]['controller_calls'] === 1,
    'all deferred providers were loaded before traffic' => $app->getDeferredServices() === [],
    'worker did not encounter a lifecycle error' => $client->errors === [] && ! $client->stopped,
];

$originalListeners = $app['config']->get('octane.listeners');
$listeners = $originalListeners;
$received = &$listeners[Laravel\Octane\Events\RequestReceived::class];
[$received[0], $received[1]] = [$received[1], $received[0]];
$app['config']->set('octane.listeners', $listeners);
$reorderedRejected = false;
try {
    hamster_validate_octane_lifecycle($app);
} catch (RuntimeException) {
    $reorderedRejected = true;
}
$checks['reordered required listeners fail startup validation'] = $reorderedRejected;
$listeners = $originalListeners;
array_unshift(
    $listeners[Laravel\Octane\Events\RequestReceived::class],
    HamsterLifecycleProbeState::class
);
$app['config']->set('octane.listeners', $listeners);
$prependedRejected = false;
try {
    hamster_validate_octane_lifecycle($app);
} catch (RuntimeException) {
    $prependedRejected = true;
}
$checks['a custom listener before sandbox preparation fails startup validation'] = $prependedRejected;
$app['config']->set('octane.listeners', $originalListeners);

final class HamsterLifecycleFakeConnection
{
    public int $level = 2;

    public function transactionLevel(): int
    {
        return $this->level;
    }

    public function rollBack(): void
    {
        $this->level--;
    }
}

final class HamsterLifecycleFakeDatabaseManager
{
    public function __construct(private HamsterLifecycleFakeConnection $connection) {}

    public function getConnections(): array
    {
        return ['probe' => $this->connection];
    }
}

$cleanupApp = new Illuminate\Foundation\Application();
$connection = new HamsterLifecycleFakeConnection();
$cleanupApp->instance('db', new HamsterLifecycleFakeDatabaseManager($connection));
(new HamsterLaravelLifecycle($cleanupApp))->completeRequest();
$checks['all leaked database transaction levels are rolled back'] = $connection->level === 0;

// PMS resolves tymon/jwt-auth's parser during provider boot. The singleton is
// therefore worker-owned and keeps a Request property, but the package also
// registers a request rebinding callback. Exercise that real package pattern
// when present: a token from A must not remain visible to anonymous B.
if ($app->bound('tymon.jwt.parser')) {
    $app['router']->get('/__hamster/jwt', static fn () => [
        'token' => app('tymon.jwt.parser')->parseToken(),
    ]);
    $jwtStart = count($client->responses);
    $request = Illuminate\Http\Request::create(
        '/__hamster/jwt',
        'GET',
        [],
        [],
        [],
        ['HTTP_AUTHORIZATION' => 'Bearer alice-token']
    );
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();
    $request = Illuminate\Http\Request::create('/__hamster/jwt', 'GET');
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();
    $checks['warmed JWT parser sees request A token'] =
        $client->responses[$jwtStart]['token'] === 'alice-token';
    $checks['warmed JWT parser does not leak token to request B'] =
        $client->responses[$jwtStart + 1]['token'] === null;
}

// On PMS, exercise the actual session guard and web middleware. The seed
// request reads an existing valid user and creates only a scratch session; it
// does not update the user or issue a remember token. Request A presents that
// cookie, while request B is anonymous on the same process and generation.
if (class_exists(App\Models\Accesstokenlink::class)) {
    $app['router']->get('/__hamster/auth-seed', static function (): array {
        $user = App\Models\Accesstokenlink::query()
            ->whereHas('taikhoan')
            ->where(static function ($query): void {
                $query->whereNull('AccessLife')->orWhere('AccessLife', '>=', now());
            })
            ->firstOrFail();
        Illuminate\Support\Facades\Auth::login($user, false);

        return ['id' => Illuminate\Support\Facades\Auth::id()];
    })->middleware('web');
    $app['router']->get('/__hamster/auth-state', static fn (): array => [
        'id' => Illuminate\Support\Facades\Auth::id(),
    ])->middleware(['web', 'auth']);

    $authStart = count($client->responses);
    $request = Illuminate\Http\Request::create('/__hamster/auth-seed', 'GET');
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();
    $authenticatedId = $client->responses[$authStart]['id'];
    $cookieName = $app['config']->get('session.cookie');
    $sessionCookie = null;
    foreach ($client->httpResponses[$authStart]->headers->getCookies() as $cookie) {
        if ($cookie->getName() === $cookieName) {
            $sessionCookie = $cookie->getValue();
            break;
        }
    }

    if (! is_string($sessionCookie) || $sessionCookie === '') {
        throw new RuntimeException('PMS auth seed did not issue a session cookie');
    }
    $request = Illuminate\Http\Request::create(
        '/__hamster/auth-state',
        'GET',
        [],
        [$cookieName => $sessionCookie]
    );
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();

    $request = Illuminate\Http\Request::create('/__hamster/auth-state', 'GET');
    $worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
    $lifecycle->completeRequest();

    $checks['PMS auth seed logs in an existing user without remember state'] =
        is_int($authenticatedId) || ctype_digit((string) $authenticatedId);
    $checks['PMS authenticated request restores the session user'] =
        (string) $client->responses[$authStart + 1]['id'] === (string) $authenticatedId;
    $checks['PMS anonymous request is not served authenticated content'] =
        $client->httpResponses[$authStart + 2]->getStatusCode() === 302;
    $checks['PMS anonymous request redirects to login'] = str_ends_with(
        $client->httpResponses[$authStart + 2]->headers->get('Location', ''),
        '/login'
    );
}

// A terminating callback runs after Octane has produced a response. The
// generation must still be retired; the protocol-adapter test separately
// proves that its staged success is replaced before any bytes become visible.
$app['events']->listen(
    Laravel\Octane\Events\RequestTerminated::class,
    static function (): void {
        throw new RuntimeException('injected terminating callback failure');
    }
);
$responsesBeforeFailure = count($client->responses);
$request = Illuminate\Http\Request::create('/__hamster/controller', 'GET');
$worker->handle($request, new Laravel\Octane\RequestContext(['request' => $request]));
$lifecycle->completeRequest();
$checks['request executed before injected termination failure'] = count($client->responses) === $responsesBeforeFailure + 1;
$checks['termination failure retires the worker'] = $client->stopped;

$failed = array_keys(array_filter($checks, static fn (bool $passed): bool => ! $passed));
if ($failed !== []) {
    foreach ($failed as $name) {
        fwrite(STDERR, "FAIL {$name}\n");
    }
    fwrite(STDERR, json_encode($client->responses, JSON_PRETTY_PRINT) . "\n");
    exit(1);
}

fwrite(STDOUT, count($checks) . " checks passed\n");
