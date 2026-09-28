<?php

declare(strict_types=1);

/**
 * Hamster entry file for an unmodified slim/slim-skeleton application.
 *
 * It builds the application exactly as the skeleton's public/index.php does
 * and returns it to psr15_worker.php instead of handling one request from
 * superglobals. Two parts of index.php cannot carry over, and this file says
 * so rather than hide it:
 *
 * - ResponseEmitter::emit() adds CORS and cache headers while emitting. The
 *   worker never calls an emitter, so the same headers are added by the
 *   wrapping handler below.
 * - ShutdownHandler turns a fatal error into a 500 page from a shutdown
 *   function. A fatal error ends a worker process, and the server answers 502.
 *
 * HAMSTER_APP_PATH names the skeleton's project directory.
 */

use App\Application\Handlers\HttpErrorHandler;
use App\Application\Settings\SettingsInterface;
use DI\ContainerBuilder;
use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\ServerRequestInterface;
use Psr\Http\Server\RequestHandlerInterface;
use Slim\Factory\AppFactory;
use Slim\Psr7\Factory\ServerRequestFactory;
use Slim\Psr7\Factory\StreamFactory;
use Slim\Psr7\Factory\UploadedFileFactory;

$root = getenv('HAMSTER_APP_PATH');
if ($root === false || ! is_file($root . '/vendor/autoload.php')) {
    throw new RuntimeException('HAMSTER_APP_PATH must name a slim-skeleton project');
}
require $root . '/vendor/autoload.php';

$containerBuilder = new ContainerBuilder();
(require $root . '/app/settings.php')($containerBuilder);
(require $root . '/app/dependencies.php')($containerBuilder);
(require $root . '/app/repositories.php')($containerBuilder);
$container = $containerBuilder->build();

AppFactory::setContainer($container);
$app = AppFactory::create();
$callableResolver = $app->getCallableResolver();
(require $root . '/app/middleware.php')($app);
(require $root . '/app/routes.php')($app);

/** @var SettingsInterface $settings */
$settings = $container->get(SettingsInterface::class);
$errorHandler = new HttpErrorHandler($callableResolver, $app->getResponseFactory());
$app->addRoutingMiddleware();
$app->addBodyParsingMiddleware();
$app->addErrorMiddleware(
    $settings->get('displayErrorDetails'),
    $settings->get('logError'),
    $settings->get('logErrorDetails')
)->setDefaultErrorHandler($errorHandler);

$emitterHeaders = new class ($app) implements RequestHandlerInterface {
    public function __construct(private RequestHandlerInterface $app) {}

    public function handle(ServerRequestInterface $request): ResponseInterface
    {
        // The skeleton's ResponseEmitter::emit(), minus the emitting.
        return $this->app->handle($request)
            ->withHeader('Access-Control-Allow-Credentials', 'true')
            ->withHeader('Access-Control-Allow-Origin', $request->getHeaderLine('Origin'))
            ->withHeader(
                'Access-Control-Allow-Headers',
                'X-Requested-With, Content-Type, Accept, Origin, Authorization',
            )
            ->withHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, PATCH, DELETE, OPTIONS')
            ->withHeader('Cache-Control', 'no-store, no-cache, must-revalidate, max-age=0')
            ->withAddedHeader('Cache-Control', 'post-check=0, pre-check=0')
            ->withHeader('Pragma', 'no-cache');
    }
};

return [
    'handler' => $emitterHeaders,
    'request_factory' => new ServerRequestFactory(),
    'stream_factory' => new StreamFactory(),
    'uploaded_file_factory' => new UploadedFileFactory(),
];
