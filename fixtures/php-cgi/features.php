<?php

declare(strict_types=1);

/**
 * SAPI features a persistent CLI worker cannot provide, selected by `do`:
 * status and headers, cookies, native sessions, redirects, streaming output,
 * large output, errors, exit, and a delay for timeout and concurrency tests.
 */

$do = $_GET['do'] ?? '';
switch ($do) {
    case 'cookies':
        http_response_code(201);
        setcookie('first', '1');
        setcookie('second', 'two words', ['samesite' => 'Lax', 'httponly' => true]);
        header('X-Multi: a');
        header('X-Multi: b', false);
        echo 'cookies';
        break;
    case 'session':
        session_start();
        $_SESSION['visits'] = ($_SESSION['visits'] ?? 0) + 1;
        if (isset($_GET['name'])) {
            $_SESSION['name'] = (string) $_GET['name'];
        }
        header('Content-Type: text/plain');
        echo 'visits=', $_SESSION['visits'], "\nname=", $_SESSION['name'] ?? '', "\npid=", getmypid(), "\n";
        break;
    case 'redirect':
        header('Location: /target?from=redirect');
        break;
    case 'stream':
        header('Content-Type: text/plain');
        for ($chunk = 0; $chunk < 4; $chunk++) {
            echo "chunk {$chunk}\n";
            flush();
            usleep(300_000);
        }
        break;
    case 'big':
        header('Content-Type: application/octet-stream');
        $size = (int) ($_GET['bytes'] ?? 0);
        $block = str_repeat('0123456789abcdef', 4096);
        for ($sent = 0; $sent + strlen($block) <= $size; $sent += strlen($block)) {
            echo $block;
        }
        echo substr($block, 0, $size - $sent);
        break;
    case 'fatal':
        // Before any output, so PHP can still send its 500. After output the
        // status is already sent, under any web server.
        hamster_undefined_function();
        break;
    case 'exit':
        echo 'before exit';
        exit(3);
    case 'sleep':
        usleep(max(0, min((int) ($_GET['ms'] ?? 0), 30_000)) * 1000);
        echo 'slept pid=', getmypid();
        break;
    default:
        http_response_code(404);
        echo 'unknown';
}
