<?php

declare(strict_types=1);

require __DIR__ . '/heavy_workload.php';
$handler = static function (): void {
    header('Content-Type: text/plain; charset=utf-8');
    echo benchmark_heavy_payload(
        (string) parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH),
        (string) file_get_contents('php://input')
    );
};
while (frankenphp_handle_request($handler)) {
}
