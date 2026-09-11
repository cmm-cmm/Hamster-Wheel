<?php

declare(strict_types=1);

fwrite(STDERR, "intentional exit before READY\n");
exit(12);
