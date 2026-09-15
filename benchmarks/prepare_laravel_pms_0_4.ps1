param(
    [Parameter(Mandatory = $true)]
    [string]$SourcePmsPath,
    [Parameter(Mandatory = $true)]
    [string]$ScratchPath,
    [string]$ComposerPhar = '',
    [string]$Branch = 'dev',
    [switch]$IgnorePhpPlatformRequirement,
    [string]$EnvironmentFile = ''
)

$ErrorActionPreference = 'Stop'

$source = (Resolve-Path -LiteralPath $SourcePmsPath).Path
$scratch = [IO.Path]::GetFullPath($ScratchPath)
if (Test-Path -LiteralPath $scratch) {
    throw "ScratchPath already exists; refusing to overwrite it: $scratch"
}
if ($ComposerPhar -eq '') {
    $composerCommand = Get-Command composer -ErrorAction Stop
    $ComposerPhar = Join-Path (Split-Path -Parent $composerCommand.Source) 'composer.phar'
}
$composer = (Resolve-Path -LiteralPath $ComposerPhar).Path
$php = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path
$extensionDirectory = (Resolve-Path -LiteralPath '.\php\8.5\ext').Path.Replace('\', '/')
$probe = (Resolve-Path -LiteralPath '.\fixtures\php-worker\pms\HamsterPmsProbeServiceProvider.php').Path

& git clone --local --no-hardlinks --branch $Branch --single-branch -- $source $scratch
if ($LASTEXITCODE -ne 0) { throw 'PMS scratch clone failed' }

if ($EnvironmentFile -ne '') {
    $environment = (Resolve-Path -LiteralPath $EnvironmentFile).Path
    Copy-Item -LiteralPath $environment -Destination (Join-Path $scratch '.env')
}

$phpIni = Join-Path $scratch 'php.ini'
$phpIniContents = @"
extension_dir="$extensionDirectory"
extension=php_openssl.dll
extension=php_mbstring.dll
extension=php_curl.dll
extension=php_fileinfo.dll
extension=php_gd.dll
extension=php_pdo_mysql.dll
extension=php_mysqli.dll
extension=php_intl.dll
extension=php_zip.dll
extension=php_sodium.dll
opcache.enable=1
opcache.enable_cli=1
opcache.validate_timestamps=0
display_errors=stderr
log_errors=0
"@
Set-Content -LiteralPath $phpIni -Value $phpIniContents -Encoding UTF8

$requireArguments = @(
    '-c', $phpIni,
    $composer,
    "--working-dir=$scratch",
    'require', 'laravel/octane:^2',
    '--no-interaction', '--no-scripts'
)
if ($IgnorePhpPlatformRequirement) { $requireArguments += '--ignore-platform-req=php' }
& $php @requireArguments
if ($LASTEXITCODE -ne 0) {
    throw 'Composer could not install Octane. Do not use --ignore-platform-req=php unless the incompatible package has been assessed separately.'
}
& $php -c $phpIni $composer "--working-dir=$scratch" dump-autoload --no-scripts --optimize
if ($LASTEXITCODE -ne 0) { throw 'Composer autoload generation failed' }

$providerDestination = Join-Path $scratch 'app/Providers/HamsterPmsProbeServiceProvider.php'
Copy-Item -LiteralPath $probe -Destination $providerDestination
$providerManifest = Join-Path $scratch 'bootstrap/providers.php'
$providers = Get-Content -Raw -LiteralPath $providerManifest
if (-not $providers.Contains('HamsterPmsProbeServiceProvider')) {
    $needle = '    App\Providers\AppServiceProvider::class,'
    if (-not $providers.Contains($needle)) { throw 'Cannot locate AppServiceProvider in scratch provider manifest' }
    $providers = $providers.Replace($needle, "$needle`r`n    App\Providers\HamsterPmsProbeServiceProvider::class,")
    Set-Content -LiteralPath $providerManifest -Value $providers -Encoding UTF8
}

$environmentNames = @('SESSION_DRIVER', 'CACHE_STORE', 'QUEUE_CONNECTION', 'APP_ENV', 'APP_DEBUG', 'PERF_MONITOR_ENABLED', 'HAMSTER_PMS_PROBE')
$savedEnvironment = @{}
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    $env:SESSION_DRIVER = 'file'
    $env:CACHE_STORE = 'array'
    $env:QUEUE_CONNECTION = 'sync'
    $env:APP_ENV = 'production'
    $env:APP_DEBUG = 'false'
    $env:PERF_MONITOR_ENABLED = 'false'
    $env:HAMSTER_PMS_PROBE = '1'
    & $php -c $phpIni (Join-Path $scratch 'artisan') package:discover --ansi
    if ($LASTEXITCODE -ne 0) { throw 'Laravel package discovery failed' }
    & $php -c $phpIni (Join-Path $scratch 'artisan') optimize
    if ($LASTEXITCODE -ne 0) { throw 'Laravel production optimization failed' }
}
finally {
    foreach ($name in $environmentNames) {
        if ($null -eq $savedEnvironment[$name]) {
            Remove-Item "Env:$name" -ErrorAction SilentlyContinue
        }
        else {
            [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
        }
    }
}

Write-Output "PMS scratch prepared at $scratch"
Write-Output "PHP ini: $phpIni"
Write-Output 'The source PMS checkout was not modified.'
