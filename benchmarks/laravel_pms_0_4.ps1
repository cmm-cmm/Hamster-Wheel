param(
    [Parameter(Mandatory = $true)]
    [string]$PmsPath,
    [string]$PhpIni = '',
    [ValidateSet('all', 'persistent', 'fresh')]
    [string]$Candidate = 'all',
    [ValidateSet('all', 'bare', 'light', 'login', 'dashboard')]
    [string]$Workload = 'all',
    [int]$Runs = 3,
    [int]$Requests = 5000,
    [int]$WarmupRequests = 500,
    [double]$MinimumSeconds = 4.0,
    [int]$Workers = 4,
    [int]$Concurrency = 16,
    [string]$OutputPath = '',
    [string]$ServerExecutable = '',
    [string]$PersistentScript = '',
    [ValidateSet('off', 'on')]
    [string]$Gate2Profile = 'off',
    [string]$Gate2ProfileOutput = '',
    [int]$Gate2SampleRate = 8
)

$ErrorActionPreference = 'Stop'

if (-not ('HamsterLaravelBenchClient' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Net;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class HamsterLaravelBenchOutput
{
    public double Seconds;
    public int Requests;
    public double[] Milliseconds = Array.Empty<double>();
}

public sealed class HamsterLaravelBenchClient : IDisposable
{
    private readonly SocketsHttpHandler[] handlers;
    private readonly HttpClient[] clients;

    public HamsterLaravelBenchClient(int concurrency, string cookie)
    {
        handlers = new SocketsHttpHandler[concurrency];
        clients = new HttpClient[concurrency];
        for (int slot = 0; slot < concurrency; slot++)
        {
            handlers[slot] = new SocketsHttpHandler {
                MaxConnectionsPerServer = 1,
                CookieContainer = new CookieContainer(),
                UseCookies = true
            };
            clients[slot] = new HttpClient(handlers[slot]) {
                BaseAddress = new Uri("http://127.0.0.1:8080"),
                Timeout = TimeSpan.FromSeconds(30)
            };
            clients[slot].DefaultRequestHeaders.Host = "localhost";
            if (!String.IsNullOrWhiteSpace(cookie))
                clients[slot].DefaultRequestHeaders.TryAddWithoutValidation("Cookie", cookie);
        }
    }

    private async Task Request(string path, int slot)
    {
        using (HttpResponseMessage response = await clients[slot].GetAsync(path, HttpCompletionOption.ResponseContentRead))
        {
            if (response.StatusCode != HttpStatusCode.OK)
                throw new InvalidOperationException("unexpected " + (int)response.StatusCode + " for " + path);
            if (!response.Content.Headers.ContentLength.HasValue || response.Content.Headers.ContentLength.Value <= 0)
                throw new InvalidOperationException("missing or empty content-length for " + path);
        }
    }

    private async Task DumpRequest(string path, int slot)
    {
        using (var request = new HttpRequestMessage(HttpMethod.Get, path))
        {
            request.Headers.TryAddWithoutValidation("X-Hamster-Gate2-Dump", "1");
            using (HttpResponseMessage response = await clients[slot].SendAsync(request, HttpCompletionOption.ResponseContentRead))
            {
                if (response.StatusCode != HttpStatusCode.OK)
                    throw new InvalidOperationException("unexpected dump status " + (int)response.StatusCode);
            }
        }
    }

    public async Task Dump(string path)
    {
        var tasks = new Task[clients.Length];
        for (int slot = 0; slot < clients.Length; slot++)
        {
            int dumpSlot = slot;
            tasks[slot] = Task.Run(() => DumpRequest(path, dumpSlot));
        }
        await Task.WhenAll(tasks);
    }

    public async Task<double> Probe(string path)
    {
        long started = Stopwatch.GetTimestamp();
        await Request(path, 0);
        return Stopwatch.GetElapsedTime(started).TotalMilliseconds;
    }

    public async Task Warm(string path, int requests)
    {
        for (int i = 0; i < requests; i++) await Request(path, i % clients.Length);
    }

    public async Task<HamsterLaravelBenchOutput> Run(string path, int minimumRequests, int concurrency, double minimumSeconds)
    {
        var samples = new List<double>[concurrency];
        long completed = 0;
        long deadline = Stopwatch.GetTimestamp() + (long)(minimumSeconds * Stopwatch.Frequency);
        var wall = Stopwatch.StartNew();
        var tasks = new Task[concurrency];
        for (int slot = 0; slot < concurrency; slot++)
        {
            int sampleSlot = slot;
            samples[slot] = new List<double>();
            tasks[slot] = Task.Run(async () => {
                while (Interlocked.Read(ref completed) < minimumRequests || Stopwatch.GetTimestamp() < deadline)
                {
                    long started = Stopwatch.GetTimestamp();
                    await Request(path, sampleSlot);
                    samples[sampleSlot].Add(Stopwatch.GetElapsedTime(started).TotalMilliseconds);
                    Interlocked.Increment(ref completed);
                }
            });
        }
        await Task.WhenAll(tasks);
        wall.Stop();
        int count = 0;
        foreach (var values in samples) count += values.Count;
        var milliseconds = new double[count];
        int offset = 0;
        foreach (var values in samples) {
            values.CopyTo(milliseconds, offset);
            offset += values.Count;
        }
        Array.Sort(milliseconds);
        return new HamsterLaravelBenchOutput {
            Seconds = wall.Elapsed.TotalSeconds,
            Requests = count,
            Milliseconds = milliseconds
        };
    }

    public void Dispose()
    {
        foreach (HttpClient client in clients) client.Dispose();
        foreach (SocketsHttpHandler handler in handlers) handler.Dispose();
    }
}
'@
}

function Get-Percentile([double[]]$Values, [double]$Percentile) {
    return $Values[[Math]::Floor(($Values.Length - 1) * $Percentile)]
}

function Get-Median($Values) {
    $sorted = @($Values | Sort-Object)
    return $sorted[[Math]::Floor($sorted.Count / 2)]
}

function Get-PhpChildren([int]$ParentId) {
    $ids = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -ExpandProperty ProcessId)
    return @($ids | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
}

function Get-SystemCommittedBytes {
    return [double](Get-CimInstance Win32_PerfRawData_PerfOS_Memory).CommittedBytes
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

function Stop-ProcessTree([Diagnostics.Process]$Server, $PhpProcesses) {
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -ErrorAction SilentlyContinue
        Wait-Process -Id $Server.Id -ErrorAction SilentlyContinue
    }
    foreach ($php in @($PhpProcesses)) {
        if ($null -ne (Get-Process -Id $php.Id -ErrorAction SilentlyContinue)) {
            Stop-Process -Id $php.Id -ErrorAction SilentlyContinue
            Wait-Process -Id $php.Id -ErrorAction SilentlyContinue
        }
    }
}

$pms = (Resolve-Path -LiteralPath $PmsPath).Path
if ($PhpIni -eq '') { $PhpIni = Join-Path $pms 'php.ini' }
$ini = (Resolve-Path -LiteralPath $PhpIni).Path.Replace('\', '/')
if ($ServerExecutable -eq '') { $ServerExecutable = '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe' }
$serverExecutable = (Resolve-Path -LiteralPath $ServerExecutable).Path
$phpExecutable = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path.Replace("'", "''")
if ($PersistentScript -eq '') { $PersistentScript = '.\fixtures\php-worker\laravel_worker.php' }
$persistentScript = (Resolve-Path -LiteralPath $PersistentScript).Path.Replace("'", "''")
$freshScript = (Resolve-Path -LiteralPath '.\fixtures\php-worker\laravel_fresh_worker.php').Path.Replace("'", "''")
$pmsToml = $pms.Replace("'", "''")
if ($Gate2Profile -eq 'on') {
    if ($Gate2ProfileOutput -eq '') { throw 'Gate2ProfileOutput is required when Gate2Profile is on' }
    $gate2ProfileRoot = [IO.Path]::GetFullPath($Gate2ProfileOutput)
    if (-not (Test-Path -LiteralPath $gate2ProfileRoot)) {
        $null = New-Item -ItemType Directory -Path $gate2ProfileRoot
    }
}

$candidateNames = if ($Candidate -eq 'all') { @('fresh', 'persistent') } else { @($Candidate) }
$workloads = switch ($Workload) {
    'all' { @([PSCustomObject]@{ Name = 'light'; Path = '/__hamster/light'; Auth = $false }, [PSCustomObject]@{ Name = 'login'; Path = '/login'; Auth = $false }) }
    'bare' { @([PSCustomObject]@{ Name = 'bare'; Path = '/__hamster/bare'; Auth = $false }) }
    'light' { @([PSCustomObject]@{ Name = 'light'; Path = '/__hamster/light'; Auth = $false }) }
    'login' { @([PSCustomObject]@{ Name = 'login'; Path = '/login'; Auth = $false }) }
    'dashboard' { @([PSCustomObject]@{ Name = 'dashboard'; Path = '/'; Auth = $true }) }
}

$allMeasurements = @()
foreach ($case in $workloads) {
    foreach ($candidateName in $candidateNames) {
        $measurements = @()
        for ($run = 1; $run -le $Runs; $run++) {
            $tag = "$PID-$candidateName-$($case.Name)-$run"
            $configPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-laravel-$tag.toml"
            $stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-laravel-$tag.out"
            $stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-laravel-$tag.err"
            $script = if ($candidateName -eq 'persistent') { $persistentScript } else { $freshScript }
            $gate2Environment = ''
            $gate2RunDirectory = ''
            if ($Gate2Profile -eq 'on' -and $candidateName -eq 'persistent') {
                $gate2RunDirectory = Join-Path $gate2ProfileRoot $tag
                $null = New-Item -ItemType Directory -Path $gate2RunDirectory
                $gate2TomlPath = $gate2RunDirectory.Replace("'", "''")
                $minimumProfileRequestId = $WarmupRequests + 2
                $gate2Environment = @"
HAMSTER_GATE2_PROFILE = '1'
HAMSTER_GATE2_PROFILE_OUTPUT = '$gate2TomlPath'
HAMSTER_GATE2_SAMPLE_RATE = '$Gate2SampleRate'
HAMSTER_GATE2_MIN_REQUEST_ID = '$minimumProfileRequestId'
"@
            }
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
script = '$script'
workers = $Workers
queue_capacity = 128

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = '$pmsToml'
PHPRC = '$ini'
SESSION_DRIVER = 'file'
CACHE_STORE = 'array'
QUEUE_CONNECTION = 'sync'
APP_ENV = 'production'
APP_DEBUG = 'false'
PERF_MONITOR_ENABLED = 'false'
HAMSTER_PMS_PROBE = '1'
$gate2Environment
"@
            Set-Content -LiteralPath $configPath -Value $config -Encoding UTF8
            $server = $null
            $phpProcesses = @()
            $client = $null
            try {
                $systemCommitBefore = Get-SystemCommittedBytes
                $startup = [Diagnostics.Stopwatch]::StartNew()
                $server = Start-Process -FilePath $serverExecutable -ArgumentList @('--config', $configPath) -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
                Wait-Server $stderrPath
                for ($attempt = 0; $attempt -lt 500; $attempt++) {
                    $phpProcesses = @(Get-PhpChildren $server.Id)
                    if ($phpProcesses.Count -eq $Workers) { break }
                    Start-Sleep -Milliseconds 10
                }
                if ($phpProcesses.Count -ne $Workers) { throw "Expected $Workers PHP workers, found $($phpProcesses.Count)" }
                $startup.Stop()

                $cookie = ''
                if ($case.Auth) {
                    $seedHandler = [Net.Http.HttpClientHandler]::new()
                    $seedClient = [Net.Http.HttpClient]::new($seedHandler)
                    try {
                        $seedClient.DefaultRequestHeaders.Host = 'localhost'
                        $seed = $seedClient.GetAsync('http://127.0.0.1:8080/__hamster/auth-seed').GetAwaiter().GetResult()
                        if (-not $seed.IsSuccessStatusCode) { throw "Auth seed returned $([int]$seed.StatusCode)" }
                        $cookie = $seedHandler.CookieContainer.GetCookieHeader([Uri]'http://127.0.0.1:8080')
                    }
                    finally { $seedClient.Dispose(); $seedHandler.Dispose() }
                }

                $client = [HamsterLaravelBenchClient]::new($Concurrency, $cookie)
                $firstResponseMs = $client.Probe($case.Path).GetAwaiter().GetResult()
                $null = $client.Warm($case.Path, $WarmupRequests).GetAwaiter().GetResult()
                $systemCommitWarm = Get-SystemCommittedBytes
                $server.Refresh()
                foreach ($php in $phpProcesses) { $php.Refresh() }
                $serverCpuBefore = $server.TotalProcessorTime.TotalSeconds
                $phpCpuBefore = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum
                $clientProcess = [Diagnostics.Process]::GetCurrentProcess()
                $clientProcess.Refresh()
                $clientCpuBefore = $clientProcess.TotalProcessorTime.TotalSeconds
                $result = $client.Run($case.Path, $Requests, $Concurrency, $MinimumSeconds).GetAwaiter().GetResult()
                $server.Refresh()
                foreach ($php in $phpProcesses) { $php.Refresh() }
                $clientProcess.Refresh()
                $systemCommitAfter = Get-SystemCommittedBytes
                $serverCpu = $server.TotalProcessorTime.TotalSeconds - $serverCpuBefore
                $phpCpu = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum - $phpCpuBefore
                $clientCpu = $clientProcess.TotalProcessorTime.TotalSeconds - $clientCpuBefore
                $measurement = [PSCustomObject]@{
                    Candidate = $candidateName
                    Workload = $case.Name
                    Run = $run
                    Workers = $Workers
                    Concurrency = $Concurrency
                    Requests = $result.Requests
                    WallSeconds = $result.Seconds
                    Rps = $result.Requests / $result.Seconds
                    P50Ms = Get-Percentile $result.Milliseconds 0.50
                    P95Ms = Get-Percentile $result.Milliseconds 0.95
                    P99Ms = Get-Percentile $result.Milliseconds 0.99
                    ServerCpuUsPerRequest = $serverCpu * 1000000 / $result.Requests
                    PhpCpuUsPerRequest = $phpCpu * 1000000 / $result.Requests
                    ClientCpuUsPerRequest = $clientCpu * 1000000 / $result.Requests
                    ServerWorkingMiB = $server.WorkingSet64 / 1MB
                    PhpWorkingMiB = ($phpProcesses | Measure-Object WorkingSet64 -Sum).Sum / 1MB
                    ServerPrivateMiB = $server.PrivateMemorySize64 / 1MB
                    PhpPrivateMiB = ($phpProcesses | Measure-Object PrivateMemorySize64 -Sum).Sum / 1MB
                    ServerPeakWorkingMiB = $server.PeakWorkingSet64 / 1MB
                    PhpPeakWorkingMiB = ($phpProcesses | Measure-Object PeakWorkingSet64 -Sum).Sum / 1MB
                    ServerHandles = $server.HandleCount
                    PhpHandles = ($phpProcesses | Measure-Object HandleCount -Sum).Sum
                    ServerThreads = $server.Threads.Count
                    PhpThreads = ($phpProcesses | ForEach-Object { $_.Threads.Count } | Measure-Object -Sum).Sum
                    SystemCommitWarmDeltaMiB = ($systemCommitWarm - $systemCommitBefore) / 1MB
                    SystemCommitAfterDeltaMiB = ($systemCommitAfter - $systemCommitBefore) / 1MB
                    StartupMs = $startup.Elapsed.TotalMilliseconds
                    FirstResponseMs = $firstResponseMs
                    Gate2Profile = $Gate2Profile
                    Gate2ProfileDirectory = $gate2RunDirectory
                }
                if ($Gate2Profile -eq 'on' -and $candidateName -eq 'persistent') {
                    $dumpedWorkers = @()
                    for ($dumpAttempt = 0; $dumpAttempt -lt 10; $dumpAttempt++) {
                        $null = $client.Dump($case.Path).GetAwaiter().GetResult()
                        $dumpedWorkers = @(Get-ChildItem -LiteralPath $gate2RunDirectory -Filter 'php-*.json' -File)
                        if ($dumpedWorkers.Count -eq $Workers) { break }
                    }
                    if ($dumpedWorkers.Count -ne $Workers) {
                        throw "Gate 2 profile dump reached $($dumpedWorkers.Count) of $Workers workers"
                    }
                }
                $measurements += $measurement
                $allMeasurements += $measurement
                Write-Output ("LARAVEL_RUN candidate={0} workload={1} run={2} requests={3} wall_s={4:F3} rps={5:F1} p50_ms={6:F3} p95_ms={7:F3} p99_ms={8:F3} server_cpu_us={9:F2} php_cpu_us={10:F2} server_ram_mib={11:F1} php_ram_mib={12:F1}" -f
                    $candidateName, $case.Name, $run, $measurement.Requests, $measurement.WallSeconds,
                    $measurement.Rps, $measurement.P50Ms, $measurement.P95Ms, $measurement.P99Ms,
                    $measurement.ServerCpuUsPerRequest, $measurement.PhpCpuUsPerRequest,
                    $measurement.ServerWorkingMiB, $measurement.PhpWorkingMiB)
            }
            finally {
                if ($null -ne $client) { $client.Dispose() }
                Stop-ProcessTree $server $phpProcesses
                Remove-Item -LiteralPath $configPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
            }
        }
        Write-Output ("LARAVEL_MEDIAN candidate={0} workload={1} runs={2} workers={3} concurrency={4} minimum_requests={5} minimum_seconds={6:F1} rps_min={7:F1} rps_median={8:F1} rps_max={9:F1} p50_ms={10:F3} p95_ms={11:F3} p99_ms={12:F3} server_cpu_us={13:F2} php_cpu_us={14:F2} server_ram_mib={15:F1} php_ram_mib={16:F1}" -f
            $candidateName, $case.Name, $Runs, $Workers, $Concurrency, $Requests, $MinimumSeconds,
            (($measurements.Rps | Measure-Object -Minimum).Minimum), (Get-Median $measurements.Rps),
            (($measurements.Rps | Measure-Object -Maximum).Maximum), (Get-Median $measurements.P50Ms),
            (Get-Median $measurements.P95Ms), (Get-Median $measurements.P99Ms),
            (Get-Median $measurements.ServerCpuUsPerRequest), (Get-Median $measurements.PhpCpuUsPerRequest),
            (Get-Median $measurements.ServerWorkingMiB), (Get-Median $measurements.PhpWorkingMiB))
    }
}

if ($OutputPath -ne '') {
    $metadata = [PSCustomObject]@{
        Timestamp = [DateTimeOffset]::Now.ToString('o')
        Platform = [Environment]::OSVersion.VersionString
        ProcessorCount = [Environment]::ProcessorCount
        Php = (& '.\php\8.5\php.exe' -v | Select-Object -First 1)
        PmsCommit = (& git -C $pms rev-parse HEAD)
        Optimized = Test-Path -LiteralPath (Join-Path $pms 'bootstrap/cache/config.php')
        Runs = $Runs
        MinimumRequests = $Requests
        WarmupRequests = $WarmupRequests
        MinimumSeconds = $MinimumSeconds
        Workers = $Workers
        Concurrency = $Concurrency
        Measurements = $allMeasurements
        ResponseValidation = 'HTTP 200, complete body via ResponseContentRead, positive Content-Length'
        ClientSessionModel = 'one HttpClient, connection pool, and CookieContainer per concurrency lane'
        DynamicBodyDigest = 'not compared: login CSRF tokens and light controller counters intentionally change'
        ContextSwitches = 'unavailable: stable per-PID cumulative Windows counter not exposed by the existing harness'
        PersistentScript = $persistentScript
        ServerExecutable = $serverExecutable
        Gate2Profile = $Gate2Profile
        Gate2SampleRate = $Gate2SampleRate
        Gate2MinimumRequestId = $WarmupRequests + 2
    }
    $metadata | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $OutputPath -Encoding UTF8
}
