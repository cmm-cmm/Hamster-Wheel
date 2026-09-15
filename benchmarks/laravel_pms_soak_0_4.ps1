param(
    [Parameter(Mandatory = $true)]
    [string]$PmsPath,
    [string]$PhpIni = '',
    [int]$Pairs = 10000,
    [int]$WarmupPairs = 250,
    [int]$SampleEveryPairs = 250,
    [switch]$SkipFailureInjection,
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'

if (-not ('HamsterLaravelSoakClient' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Net;
using System.Net.Http;
using System.Threading.Tasks;

public sealed class HamsterLaravelSoakReply
{
    public int Status;
    public string Body = "";
    public string Location = "";
}

public sealed class HamsterLaravelSoakClient : IDisposable
{
    private readonly SocketsHttpHandler handler;
    private readonly HttpClient client;

    public HamsterLaravelSoakClient()
    {
        handler = new SocketsHttpHandler {
            MaxConnectionsPerServer = 1,
            CookieContainer = new CookieContainer(),
            UseCookies = true,
            AllowAutoRedirect = false
        };
        client = new HttpClient(handler) {
            BaseAddress = new Uri("http://127.0.0.1:8080"),
            Timeout = TimeSpan.FromSeconds(30)
        };
        client.DefaultRequestHeaders.Host = "localhost";
    }

    public async Task<HamsterLaravelSoakReply> Get(string path, string headerName = "", string headerValue = "")
    {
        using (var request = new HttpRequestMessage(HttpMethod.Get, path))
        {
            if (!String.IsNullOrEmpty(headerName))
                request.Headers.TryAddWithoutValidation(headerName, headerValue);
            using (HttpResponseMessage response = await client.SendAsync(request, HttpCompletionOption.ResponseContentRead))
            {
                return new HamsterLaravelSoakReply {
                    Status = (int)response.StatusCode,
                    Body = await response.Content.ReadAsStringAsync(),
                    Location = response.Headers.Location == null ? "" : response.Headers.Location.ToString()
                };
            }
        }
    }

    public void Dispose()
    {
        client.Dispose();
        handler.Dispose();
    }
}
'@
}

function Get-Worker([int]$ParentId) {
    $id = Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -First 1 -ExpandProperty ProcessId
    if ($null -eq $id) { return $null }
    return Get-Process -Id $id -ErrorAction SilentlyContinue
}

function Wait-Server([string]$ErrorPath) {
    for ($attempt = 0; $attempt -lt 1000; $attempt++) {
        try {
            $probe = [Net.Sockets.TcpClient]::new('127.0.0.1', 8080)
            $probe.Dispose()
            return
        }
        catch {
            if ($attempt -eq 999) {
                $diagnostic = if (Test-Path -LiteralPath $ErrorPath) { Get-Content -Raw -LiteralPath $ErrorPath } else { '' }
                throw "Server did not become ready: $diagnostic"
            }
            Start-Sleep -Milliseconds 10
        }
    }
}

function Get-Slope($Samples) {
    $tail = @($Samples | Select-Object -Skip ([Math]::Floor($Samples.Count / 2)))
    if ($tail.Count -lt 2) { return 0.0 }
    $meanX = ($tail.Requests | Measure-Object -Average).Average
    $meanY = ($tail.WorkingBytes | Measure-Object -Average).Average
    $numerator = 0.0
    $denominator = 0.0
    foreach ($sample in $tail) {
        $dx = $sample.Requests - $meanX
        $numerator += $dx * ($sample.WorkingBytes - $meanY)
        $denominator += $dx * $dx
    }
    if ($denominator -eq 0) { return 0.0 }
    return $numerator / $denominator
}

$pms = (Resolve-Path -LiteralPath $PmsPath).Path
if ($PhpIni -eq '') { $PhpIni = Join-Path $pms 'php.ini' }
$ini = (Resolve-Path -LiteralPath $PhpIni).Path.Replace('\', '/')
$serverExecutable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$phpExecutable = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path.Replace("'", "''")
$workerScript = (Resolve-Path -LiteralPath '.\fixtures\php-worker\laravel_worker.php').Path.Replace("'", "''")
$pmsToml = $pms.Replace("'", "''")
$tag = "$PID-$([DateTimeOffset]::Now.ToUnixTimeMilliseconds())"
$configPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pms-soak-$tag.toml"
$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pms-soak-$tag.out"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pms-soak-$tag.err"
$markerPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pms-soak-$tag.marker"
$markerToml = $markerPath.Replace('\', '/').Replace("'", "''")
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
queue_capacity = 8

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = '$pmsToml'
HAMSTER_PMS_FAILURE_MARKER = '$markerToml'
PHPRC = '$ini'
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
$auth = $null
$anonymous = $null
$workerPids = @()
$samples = @()
$clock = [Diagnostics.Stopwatch]::StartNew()
try {
    $server = Start-Process -FilePath $serverExecutable -ArgumentList @('--config', $configPath) -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    Wait-Server $stderrPath
    for ($attempt = 0; $attempt -lt 500; $attempt++) {
        $worker = Get-Worker $server.Id
        if ($null -ne $worker) { break }
        Start-Sleep -Milliseconds 10
    }
    if ($null -eq $worker) { throw 'PHP worker did not appear' }
    $initialPid = $worker.Id
    $workerPids += $initialPid
    $auth = [HamsterLaravelSoakClient]::new()
    $anonymous = [HamsterLaravelSoakClient]::new()
    $seed = $auth.Get('/__hamster/auth-seed').GetAwaiter().GetResult()
    if ($seed.Status -ne 200) { throw "Auth seed returned $($seed.Status): $($seed.Body)" }
    $userId = [string](($seed.Body | ConvertFrom-Json).id)
    if ($userId -eq '') { throw 'Auth seed did not return a user id' }

    for ($pair = -$WarmupPairs; $pair -lt $Pairs; $pair++) {
        $authenticated = $auth.Get('/__hamster/auth-state').GetAwaiter().GetResult()
        if ($authenticated.Status -ne 200 -or [string](($authenticated.Body | ConvertFrom-Json).id) -ne $userId) {
            throw "Authenticated request failed at pair ${pair}: $($authenticated.Status) $($authenticated.Body)"
        }
        $guest = $anonymous.Get('/__hamster/auth-state').GetAwaiter().GetResult()
        if ($guest.Status -ne 302 -or -not $guest.Location.EndsWith('/login')) {
            throw "Authentication leaked at pair ${pair}: $($guest.Status) $($guest.Location) $($guest.Body)"
        }
        if ($pair -ge 0 -and (($pair + 1) % $SampleEveryPairs -eq 0 -or $pair -eq $Pairs - 1)) {
            $worker = Get-Worker $server.Id
            if ($null -eq $worker) { throw "PHP worker disappeared at pair $pair" }
            $worker.Refresh()
            if ($worker.Id -ne $initialPid) { throw "Worker generation changed unexpectedly from $initialPid to $($worker.Id)" }
            $samples += [PSCustomObject]@{
                Requests = 2 * ($pair + 1)
                WorkingBytes = $worker.WorkingSet64
                PrivateBytes = $worker.PrivateMemorySize64
            }
        }
    }
    $clock.Stop()

    $lightA = $anonymous.Get('/__hamster/light').GetAwaiter().GetResult()
    $lightB = $anonymous.Get('/__hamster/light').GetAwaiter().GetResult()
    if ($lightA.Status -ne 200 -or $lightB.Status -ne 200) { throw 'Controller probe failed after soak' }
    $stateA = $lightA.Body | ConvertFrom-Json
    $stateB = $lightB.Body | ConvertFrom-Json
    if ($stateA.pid -ne $initialPid -or $stateB.pid -ne $initialPid -or $stateA.controller_calls -ne 1 -or $stateB.controller_calls -ne 1) {
        throw "Controller or PID leaked after soak: $($lightA.Body) $($lightB.Body)"
    }

    $recovery = $null
    if (-not $SkipFailureInjection) {
        $failed = $anonymous.Get('/__hamster/fail-cleanup', 'X-Hamster-Fail-Cleanup', '1').GetAwaiter().GetResult()
        if ($failed.Status -ne 502) { throw "Cleanup failure returned $($failed.Status), expected 502" }
        $markerLines = @(Get-Content -LiteralPath $markerPath)
        if ($markerLines.Count -ne 1 -or $markerLines[0] -ne 'executed') { throw "Failure request executed $($markerLines.Count) times" }
        for ($attempt = 0; $attempt -lt 2000; $attempt++) {
            $worker = Get-Worker $server.Id
            if ($null -ne $worker -and $worker.Id -ne $initialPid) { break }
            Start-Sleep -Milliseconds 10
        }
        if ($null -eq $worker -or $worker.Id -eq $initialPid) { throw 'Replacement worker did not become ready' }
        $workerPids += $worker.Id
        $healthy = $null
        for ($attempt = 0; $attempt -lt 2000; $attempt++) {
            $healthy = $anonymous.Get('/__hamster/light').GetAwaiter().GetResult()
            if ($healthy.Status -eq 200) { break }
            if ($healthy.Status -ne 503) { throw "Replacement worker returned $($healthy.Status)" }
            Start-Sleep -Milliseconds 10
        }
        if ($null -eq $healthy -or $healthy.Status -ne 200) { throw 'Replacement worker never became ready' }
        if (@(Get-Content -LiteralPath $markerPath).Count -ne 1) { throw 'Failure request was replayed' }
        $recovery = [PSCustomObject]@{ FailedStatus = $failed.Status; InitialPid = $initialPid; ReplacementPid = $worker.Id; MarkerExecutions = 1 }
    }

    $slope = Get-Slope $samples
    $summary = [PSCustomObject]@{
        Timestamp = [DateTimeOffset]::Now.ToString('o')
        Platform = [Environment]::OSVersion.VersionString
        ProcessorCount = [Environment]::ProcessorCount
        Php = (& '.\php\8.5\php.exe' -v | Select-Object -First 1)
        PmsCommit = (& git -C $pms rev-parse HEAD)
        Optimized = Test-Path -LiteralPath (Join-Path $pms 'bootstrap/cache/config.php')
        WarmupPairs = $WarmupPairs
        MeasuredPairs = $Pairs
        Requests = 2 * $Pairs
        Seconds = $clock.Elapsed.TotalSeconds
        Rps = 2 * $Pairs / $clock.Elapsed.TotalSeconds
        InitialPid = $initialPid
        MemorySlopeBytesPerRequest = $slope
        Samples = $samples
        Recovery = $recovery
    }
    Write-Output ("PMS_SOAK pairs={0} requests={1} seconds={2:F3} rps={3:F1} pid={4} samples={5} rss_first_mib={6:F2} rss_last_mib={7:F2} rss_slope_bytes_per_request={8:F2}" -f
        $Pairs, $summary.Requests, $summary.Seconds, $summary.Rps, $initialPid, $samples.Count,
        ($samples[0].WorkingBytes / 1MB), ($samples[-1].WorkingBytes / 1MB), $slope)
    if ($null -ne $recovery) {
        Write-Output "PMS_RECOVERY failed_status=502 initial_pid=$initialPid replacement_pid=$($recovery.ReplacementPid) marker_executions=1"
    }
    if ($OutputPath -ne '') { $summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $OutputPath -Encoding UTF8 }
}
finally {
    if ($null -ne $auth) { $auth.Dispose() }
    if ($null -ne $anonymous) { $anonymous.Dispose() }
    if ($null -ne $server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
        Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
    }
    foreach ($workerPid in $workerPids) {
        if ($null -ne (Get-Process -Id $workerPid -ErrorAction SilentlyContinue)) {
            Stop-Process -Id $workerPid -ErrorAction SilentlyContinue
            Wait-Process -Id $workerPid -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -LiteralPath $configPath, $stdoutPath, $stderrPath, $markerPath -Force -ErrorAction SilentlyContinue
}

foreach ($workerPid in $workerPids) {
    if ($null -ne (Get-Process -Id $workerPid -ErrorAction SilentlyContinue)) {
        throw "orphan PHP worker remains after shutdown: $workerPid"
    }
}
