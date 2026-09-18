<?php

declare(strict_types=1);

// Deliberately old startup frame. This fixture must not include protocol.php:
// that module tracks the current protocol version by design.
$payload = chr(1) . pack('n', 1) . pack('N', getmypid());
$frame = pack('N', strlen($payload)) . $payload;
fwrite(STDOUT, $frame);
fflush(STDOUT);
sleep(30);
