<#
.SYNOPSIS
Builds the PHP application workspace used by tests/php_applications.rs.

.DESCRIPTION
    fixtures\php-apps\prepare_php_apps.ps1 [-Target <dir>] [-Composer <composer.phar>]

Target defaults to www\php-apps (git-ignored). The workspace holds:

  php.ini        extensions for the repository PHP; used through PHPRC
  symfony-demo   symfony/symfony-demo 3.1.0, external and unmodified, prepared
                 as its README deploys it: prod container cache warmed and
                 AssetMapper assets compiled. Its SQLite database ships with
                 the package and is local to the workspace.
  slim-skeleton  slim/slim-skeleton 4.5.0, external and unmodified.
  native         fixtures\php-apps\native, a framework-free test application
                 written for Hamster (simulated, in this repository).
  dokuwiki       DokuWiki 2026-07-14c "Mort", external and unmodified, the
                 release tarball as published. A classic SAPI application for
                 the FastCGI tests in tests/php_cgi.rs; its wiki data stays in
                 the workspace.

Composer runs on the repository PHP (php\8.5\php.exe) with the generated
php.ini, so dependencies are resolved for the PHP version that serves them.
Versions are exact, not ranges: `^` would also be stripped by cmd.exe if
Composer were run through composer.bat. Network access to Packagist, GitHub
and (for Symfony Demo's importmap and Sass steps) jsdelivr and the dart-sass
release download is required.

The Laravel stand-in is built separately by
fixtures\php-worker\pms\prepare_scratch_app.ps1.
#>
param(
    [string] $Target,
    [string] $Composer
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $Target) {
    $Target = Join-Path $repoRoot 'www\php-apps'
}
$Target = [IO.Path]::GetFullPath($Target)
$php = Join-Path $repoRoot 'php\8.5\php.exe'
if (-not (Test-Path $php)) {
    throw "the repository PHP is missing: $php"
}
if (-not $Composer) {
    $command = Get-Command composer -ErrorAction SilentlyContinue
    if ($command) {
        $candidate = Join-Path (Split-Path $command.Source) 'composer.phar'
        if (Test-Path $candidate) { $Composer = $candidate }
    }
}
if (-not $Composer -or -not (Test-Path $Composer)) {
    throw 'composer.phar not found; pass -Composer <path to composer.phar>'
}
if (Test-Path $Target) {
    throw "$Target already exists; remove it to build a fresh workspace"
}
New-Item -ItemType Directory $Target | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)
$ini = Join-Path $Target 'php.ini'
$extensions = 'openssl', 'mbstring', 'fileinfo', 'curl', 'intl', 'zip', 'sodium', 'sqlite3', 'pdo_sqlite'
# extension_dir must be absolute: a relative one resolves against the working
# directory of whichever process loads this file.
[IO.File]::WriteAllText($ini, (@"
; PHPRC for the PHP applications behind Hamster. stdout carries the worker
; protocol, so errors must never be displayed there.
extension_dir = "$(Join-Path $repoRoot 'php\8.5\ext')"
memory_limit = 512M
display_errors = stderr
display_startup_errors = stderr
log_errors = Off
error_reporting = E_ALL

"@ + (($extensions | ForEach-Object { "extension=$_" }) -join "`n") + "`n"), $utf8)

function Invoke-Php([string[]] $Arguments) {
    & $php -c $ini @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "php $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

$env:COMPOSER_NO_INTERACTION = '1'
Push-Location $Target
try {
    Invoke-Php @($Composer, 'create-project', 'symfony/symfony-demo:3.1.0', 'symfony-demo', '--no-progress')
    Invoke-Php @($Composer, 'create-project', 'slim/slim-skeleton:4.5.0', 'slim-skeleton', '--no-progress')

    # The published release, pinned by name; tar.exe ships with Windows 10+.
    $release = 'dokuwiki-2026-07-14c'
    $archive = Join-Path $Target "$release.tgz"
    & curl.exe -sSfL -o $archive "https://download.dokuwiki.org/src/dokuwiki/$release.tgz"
    if ($LASTEXITCODE -ne 0) { throw "downloading $release failed" }
    & tar.exe -xzf $archive -C $Target
    if ($LASTEXITCODE -ne 0) { throw "extracting $release failed" }
    Rename-Item (Join-Path $Target $release) 'dokuwiki'
    Remove-Item $archive

    Copy-Item -Recurse (Join-Path $repoRoot 'fixtures\php-apps\native') (Join-Path $Target 'native')
    Push-Location (Join-Path $Target 'native')
    try { Invoke-Php @($Composer, 'dump-autoload') } finally { Pop-Location }

    # Deployment steps from the Symfony Demo README, for the environment the
    # tests run in. The worker boots the kernel before READY, so the container
    # cache must exist beforehand.
    Push-Location (Join-Path $Target 'symfony-demo')
    try {
        $env:APP_ENV = 'prod'
        $env:APP_DEBUG = '0'
        Invoke-Php @('bin/console', 'cache:clear')
        Invoke-Php @('bin/console', 'asset-map:compile')
    }
    finally {
        Remove-Item Env:APP_ENV, Env:APP_DEBUG -ErrorAction SilentlyContinue
        Pop-Location
    }
}
finally {
    Pop-Location
}

Write-Output @"

PHP application workspace ready: $Target

Run the application tests from the repository root with:
  `$env:HAMSTER_PHP_APPS = '$Target'
  cargo test --test php_applications -- --ignored
Set HAMSTER_LARAVEL_TEST_PATH as well to add the Laravel stand-in.
"@
