<?php

declare(strict_types=1);

$payload = chr(99);
fwrite(STDOUT, pack('N', strlen($payload)) . $payload);
fflush(STDOUT);
usleep(500_000);
