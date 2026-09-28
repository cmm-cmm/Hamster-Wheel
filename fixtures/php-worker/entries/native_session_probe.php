<?php

declare(strict_types=1);

/**
 * PSR-15 entry used only by tests: a handler that uses PHP's native session
 * the way a SAPI application would, to prove psr15_worker.php does not let
 * that session reach the next request. Borrows slim/psr7 from the
 * slim-skeleton project named by HAMSTER_APP_PATH.
 */

use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\ServerRequestInterface;
use Psr\Http\Server\RequestHandlerInterface;
use Slim\Psr7\Factory\ResponseFactory;
use Slim\Psr7\Factory\ServerRequestFactory;
use Slim\Psr7\Factory\StreamFactory;
use Slim\Psr7\Factory\UploadedFileFactory;

require getenv('HAMSTER_APP_PATH') . '/vendor/autoload.php';

ini_set('session.save_path', sys_get_temp_dir());

return [
    'handler' => new class implements RequestHandlerInterface {
        public function handle(ServerRequestInterface $request): ResponseInterface
        {
            session_start();
            $_SESSION['visits'] = ($_SESSION['visits'] ?? 0) + 1;
            $response = (new ResponseFactory())->createResponse();
            $response->getBody()->write(json_encode(['visits' => $_SESSION['visits']], JSON_THROW_ON_ERROR));

            return $response->withHeader('Content-Type', 'application/json');
        }
    },
    'request_factory' => new ServerRequestFactory(),
    'stream_factory' => new StreamFactory(),
    'uploaded_file_factory' => new UploadedFileFactory(),
];
