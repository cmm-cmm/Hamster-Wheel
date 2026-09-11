<?php

declare(strict_types=1);

final class FixtureApplication
{
    public int $bootstrapCount = 1;
    public int $requestCount = 0;
}

return new FixtureApplication();
