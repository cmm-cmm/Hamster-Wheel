<?php

declare(strict_types=1);

if (($_GET['workload'] ?? '') === 'json') {
    header('Content-Type: application/json');
    echo '{"message":"Hello from PHP"}';
} else {
    header('Content-Type: text/plain');
    echo 'Hello from PHP';
}
