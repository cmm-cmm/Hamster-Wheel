<?php

declare(strict_types=1);

/**
 * Front controller of the FastCGI fixture: a classic SAPI script. It reads
 * superglobals and moves uploads with move_uploaded_file(), and reports what
 * it saw as `key=value` lines for tests/php_cgi.rs.
 */

$report = [
    'script' => basename(__FILE__),
    'sapi' => PHP_SAPI,
    'pid' => (string) getmypid(),
];
foreach ([
    'REQUEST_METHOD', 'REQUEST_URI', 'SCRIPT_NAME', 'SCRIPT_FILENAME', 'PATH_INFO', 'QUERY_STRING',
    'HTTPS', 'REQUEST_SCHEME', 'REMOTE_ADDR', 'SERVER_NAME', 'SERVER_PORT', 'HTTP_HOST',
    'HTTP_X_REPEAT', 'CONTENT_TYPE', 'CONTENT_LENGTH', 'DOCUMENT_ROOT', 'HTTP_PROXY',
] as $name) {
    $report["server.{$name}"] = isset($_SERVER[$name]) ? (string) $_SERVER[$name] : '(unset)';
}
$report['php_self'] = $_SERVER['PHP_SELF'] ?? '';

$flatten = static function (array $values, string $prefix) use (&$flatten, &$report): void {
    foreach ($values as $key => $value) {
        $name = "{$prefix}.{$key}";
        if (is_array($value)) {
            $flatten($value, $name);
        } else {
            $report[$name] = (string) $value;
        }
    }
};
$flatten($_GET, 'get');
$flatten($_POST, 'post');
$flatten($_COOKIE, 'cookie');

foreach ($_FILES as $field => $file) {
    $report["files.{$field}.name"] = (string) $file['name'];
    $report["files.{$field}.size"] = (string) $file['size'];
    $report["files.{$field}.error"] = (string) $file['error'];
    if ($file['error'] === UPLOAD_ERR_OK) {
        $report["files.{$field}.uploaded"] = is_uploaded_file($file['tmp_name']) ? 'yes' : 'no';
        $target = sys_get_temp_dir() . DIRECTORY_SEPARATOR . 'hamster-cgi-' . bin2hex(random_bytes(6));
        $report["files.{$field}.moved"] = move_uploaded_file($file['tmp_name'], $target) ? 'yes' : 'no';
        $report["files.{$field}.fnv1a64"] = hash_file('fnv1a64', $target);
        @unlink($target);
    }
}
$report['input_length'] = (string) strlen((string) file_get_contents('php://input'));

header('Content-Type: text/plain; charset=utf-8');
foreach ($report as $key => $value) {
    echo $key, '=', str_replace(["\r", "\n"], ['\r', '\n'], $value), "\n";
}
