<?php

declare(strict_types=1);

/**
 * worker.php after a bootstrap that takes HAMSTER_READY_DELAY_MS, standing in
 * for an application whose first start is slow (a cold file cache, on-access
 * scanning). Used to test the pool's startup_timeout.
 */

usleep(max(0, (int) getenv('HAMSTER_READY_DELAY_MS')) * 1000);
require __DIR__ . '/worker.php';