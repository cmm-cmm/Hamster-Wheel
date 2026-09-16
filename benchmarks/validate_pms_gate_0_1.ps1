param(
    [Parameter(Mandatory = $true)]
    [string]$PmsPath
)

$ErrorActionPreference = 'Stop'

$pms = (Resolve-Path -LiteralPath $PmsPath).Path
$serverExecutable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$phpExecutable = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path.Replace("'", "''")
$workerScript = (Resolve-Path -LiteralPath '.\fixtures\php-worker\laravel_worker.php').Path.Replace("'", "''")
$phpIni = (Resolve-Path -LiteralPath (Join-Path $pms 'php.ini')).Path.Replace('\', '/')
$pmsConfig = $pms.Replace("'", "''")
$tag = "hamster-gate01-validate-$PID"
$configPath = Join-Path ([IO.Path]::GetTempPath()) "$tag.toml"
$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "$tag.out"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "$tag.err"

$config = @"
[server]
listen = "127.0.0.1:8080"

[[hosts]]
name = "localhost"

[[hosts.locations]]
path = "/"
handler = "php_worker"

[hosts.locations.php]
executable = '$phpExecutable'
script = '$workerScript'
workers = 1
queue_capacity = 128

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = '$pmsConfig'
PHPRC = '$phpIni'
SESSION_DRIVER = 'file'
CACHE_STORE = 'array'
QUEUE_CONNECTION = 'sync'
APP_ENV = 'production'
APP_DEBUG = 'false'
PERF_MONITOR_ENABLED = 'false'
HAMSTER_PMS_PROBE = '1'
"@

Set-Content -LiteralPath $configPath -Value $config -Encoding UTF8
$server = $null
$phpProcesses = @()
$client = $null
$handler = $null
try {
    $server = Start-Process -FilePath $serverExecutable -ArgumentList @('--config', $configPath) `
        -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    for ($attempt = 0; $attempt -lt 500; $attempt++) {
        try {
            $probe = [Net.Sockets.TcpClient]::new('127.0.0.1', 8080)
            $probe.Dispose()
            break
        }
        catch {
            if ($attempt -eq 499) {
                $diagnostic = if (Test-Path -LiteralPath $stderrPath) { Get-Content -Raw -LiteralPath $stderrPath } else { '' }
                throw "Server did not become ready: $diagnostic"
            }
            Start-Sleep -Milliseconds 10
        }
    }

    $handler = [Net.Http.SocketsHttpHandler]::new()
    $client = [Net.Http.HttpClient]::new($handler)
    $client.BaseAddress = [Uri]'http://127.0.0.1:8080'
    $client.DefaultRequestHeaders.Host = 'localhost'
    foreach ($path in @('/__hamster/bare', '/__hamster/light', '/login')) {
        $response = $client.GetAsync($path, [Net.Http.HttpCompletionOption]::ResponseContentRead).GetAwaiter().GetResult()
        try {
            $bytes = $response.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
            [PSCustomObject]@{
                Path = $path
                Status = [int]$response.StatusCode
                HeaderLength = $response.Content.Headers.ContentLength
                ActualLength = $bytes.Length
                ContentType = $response.Content.Headers.ContentType.ToString()
                Sha256 = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes))
            } | ConvertTo-Json -Compress
        }
        finally {
            $response.Dispose()
        }
    }
}
finally {
    if ($null -ne $client) { $client.Dispose() }
    if ($null -ne $handler) { $handler.Dispose() }
    if ($null -ne $server) {
        $phpIds = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($server.Id) AND Name = 'php.exe'" -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty ProcessId)
        if (-not $server.HasExited) {
            Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
            Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
        }
        foreach ($phpId in $phpIds) {
            Stop-Process -Id $phpId -ErrorAction SilentlyContinue
            Wait-Process -Id $phpId -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -LiteralPath $configPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
}
