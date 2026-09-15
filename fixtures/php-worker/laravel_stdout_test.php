<?php

declare(strict_types=1);

require __DIR__ . '/laravel_stdout.php';

$checks = [];
$protocol = hamster_open_protocol_stdout();
$checks['stdout remains valid during bootstrap'] = is_resource(STDOUT);
hamster_disable_application_stdout();

$checks['protocol duplicate remains writable'] = fwrite($protocol, '') === 0;
$reopened = @fopen('php://stdout', 'wb');
$checks['application stdout reopens only onto the null sink'] = is_resource($reopened)
    && fwrite($reopened, 'reopened-corruption') === strlen('reopened-corruption');

$directWriteRejected = false;
try {
    fwrite(STDOUT, 'corruption');
} catch (TypeError) {
    $directWriteRejected = true;
}
$checks['application cannot write the STDOUT constant'] = $directWriteRejected;

$failed = array_keys(array_filter($checks, static fn (bool $passed): bool => ! $passed));
if ($failed !== []) {
    foreach ($failed as $name) {
        fwrite(STDERR, "FAIL {$name}\n");
    }
    exit(1);
}

fwrite($protocol, count($checks) . " checks passed\n");
