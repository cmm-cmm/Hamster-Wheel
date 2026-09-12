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
    if (-not $env:HAMSTER_PHP) {
        $vendored = Join-Path $root 'php/8.5/php.exe'
        if (Test-Path $vendored) {
            $env:HAMSTER_PHP = $vendored
        }
        else {
            $onPath = Get-Command php -ErrorAction SilentlyContinue
            if ($onPath) { $env:HAMSTER_PHP = $onPath.Source }
        }
    }
    if ($env:HAMSTER_PHP) {
        Write-Host "PHP worker tests will run against $env:HAMSTER_PHP"
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
