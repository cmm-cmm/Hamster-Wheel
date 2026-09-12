<#
.SYNOPSIS
    Windows equivalent of scripts/check.sh: formatting, lints, and tests.
.EXAMPLE
    ./scripts/check.ps1
    $env:HAMSTER_PHP = "C:\php\8.5\php.exe"; ./scripts/check.ps1
#>
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $root
try {
    # Kept in step with scripts/_common.sh and .github/workflows/ci.yml.
    $phpVersion = '8.5'
    if (-not $env:HAMSTER_PHP) {
        $vendored = Join-Path $root "php/$phpVersion/php.exe"
        if (Test-Path $vendored) {
            $env:HAMSTER_PHP = $vendored
        }
        else {
            $onPath = Get-Command "php$phpVersion" -ErrorAction SilentlyContinue
            if (-not $onPath) { $onPath = Get-Command php -ErrorAction SilentlyContinue }
            if ($onPath) { $env:HAMSTER_PHP = $onPath.Source }
        }
    }
    if ($env:HAMSTER_PHP) {
        $build = & $env:HAMSTER_PHP -r 'printf("PHP %s %s %d-bit", PHP_VERSION, PHP_ZTS ? "ZTS" : "NTS", PHP_INT_SIZE * 8);'
        Write-Host "PHP worker tests will run against $build ($env:HAMSTER_PHP)"
        & $env:HAMSTER_PHP -r "exit((version_compare(PHP_VERSION, '$phpVersion', '>=') && !PHP_ZTS && PHP_INT_SIZE === 8) ? 0 : 1);"
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "this project targets PHP $phpVersion NTS 64-bit; found $build"
        }
    }
    else {
        Write-Warning 'no PHP CLI found; PHP worker tests will report themselves skipped'
    }

    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'cargo fmt failed' }
    cargo clippy --all-targets --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
}
finally {
    Pop-Location
}
