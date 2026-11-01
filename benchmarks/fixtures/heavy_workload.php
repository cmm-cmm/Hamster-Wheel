<?php

declare(strict_types=1);

// Shared by both server adapters: bounded synthetic CPU work, not an app claim.
function benchmark_heavy_payload(string $path, string $body): string
{
    if ($path === '/cpu') {
        $digest = str_repeat('a', 64);
        for ($round = 0; $round < 20_000; $round++) {
            $digest = hash('sha256', $digest);
        }
        return $digest;
    }
    if ($path === '/body') {
        return hash('sha256', $body);
    }
    return 'Hello from PHP';
}
