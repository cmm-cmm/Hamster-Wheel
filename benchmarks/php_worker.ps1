param(
    [int]$Runs = 3,
    [int]$Requests = 20000,
    [int]$WarmupRequests = 1000,
    [ValidateSet('all', 'simple', 'php_hello', 'php_json', 'php_echo')]
    [string]$ScenarioFilter = 'all',
    # -1 leaves each scenario's own config untouched. Any other value
    # rewrites server.max_body_memory in a temporary copy of it, which is
    # how the body budget is A/B-ed without editing a checked-in config.
    [long]$MaxBodyMemory = -1,
    [int]$EchoBodyBytes = 12,
    [string]$Executable = '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe'
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Net;
using System.Net.Http;
using System.Threading.Tasks;

public sealed class PhpBenchOutput
{
    public double Seconds;
    public double[] Milliseconds = Array.Empty<double>();
}

public sealed class PhpBenchClient : IDisposable
{
    private readonly SocketsHttpHandler handler;
    private readonly HttpClient client;
    private readonly HttpStatusCode expectedStatus;

    public PhpBenchClient(string host, HttpStatusCode expectedStatus)
    {
        handler = new SocketsHttpHandler { MaxConnectionsPerServer = 1 };
        client = new HttpClient(handler) { BaseAddress = new Uri("http://127.0.0.1:8080") };
        client.DefaultRequestHeaders.Host = host;
        this.expectedStatus = expectedStatus;
    }

    private async Task Request(string path, byte[] body)
    {
        HttpResponseMessage response;
        if (body.Length == 0)
        {
            response = await client.GetAsync(path, HttpCompletionOption.ResponseContentRead);
        }
        else
        {
            using var content = new ByteArrayContent(body);
            response = await client.PostAsync(path, content);
        }
        using (response)
        {
            if (response.StatusCode != expectedStatus)
                throw new InvalidOperationException("Unexpected HTTP status: " + response.StatusCode);
        }
    }

    public async Task<double> First(string path, byte[] body)
    {
        long started = Stopwatch.GetTimestamp();
        await Request(path, body);
        return Stopwatch.GetElapsedTime(started).TotalMilliseconds;
    }

    public async Task Warmup(string path, byte[] body, int requests)
    {
        for (int i = 0; i < requests; i++) await Request(path, body);
    }

    public async Task<PhpBenchOutput> Run(string path, byte[] body, int requests)
    {
        var milliseconds = new double[requests];
        var elapsed = Stopwatch.StartNew();
        for (int i = 0; i < requests; i++)
        {
            long started = Stopwatch.GetTimestamp();
            await Request(path, body);
            milliseconds[i] = Stopwatch.GetElapsedTime(started).TotalMilliseconds;
        }
        elapsed.Stop();
        Array.Sort(milliseconds);
        return new PhpBenchOutput { Seconds = elapsed.Elapsed.TotalSeconds, Milliseconds = milliseconds };
    }

    public async Task StopWorker()
    {
        using (HttpResponseMessage response = await client.GetAsync("/exit-after-response")) { }
    }

    public void Dispose()
    {
        client.Dispose();
        handler.Dispose();
    }
}
'@

function Get-Percentile([double[]]$Values, [double]$Percentile) {
    $index = [Math]::Floor(($Values.Length - 1) * $Percentile)
    return $Values[$index]
}

function Get-Median($Values) {
    $sorted = @($Values | Sort-Object)
    return $sorted[[Math]::Floor($sorted.Count / 2)]
}

function Get-QuantumPercent([double[]]$Values) {
    $near = 0
    foreach ($value in $Values) {
        $multiple = [Math]::Round($value / 15.625)
        if ($multiple -ge 1 -and [Math]::Abs($value - ($multiple * 15.625)) -le 1.0) {
            $near++
        }
    }
    return 100.0 * $near / $Values.Length
}

function Find-PhpChild([int]$ParentId) {
    $processInfo = Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -First 1
    if ($null -eq $processInfo) { return $null }
    return Get-Process -Id $processInfo.ProcessId -ErrorAction SilentlyContinue
}

function Get-WorkerBootstrapMilliseconds([string]$StdoutPath) {
    if (-not (Test-Path -LiteralPath $StdoutPath)) { return [double]::NaN }
    $text = Get-Content -Raw -LiteralPath $StdoutPath
    $match = [regex]::Match($text, 'PHP worker \d+ ready after ([0-9.]+) ms')
    if (-not $match.Success) { return [double]::NaN }
    return [double]::Parse($match.Groups[1].Value, [Globalization.CultureInfo]::InvariantCulture)
}

$executable = (Resolve-Path -LiteralPath $Executable).Path
$emptyBody = [byte[]]::new(0)
$echoBody = if ($EchoBodyBytes -eq 12) {
    [Text.Encoding]::ASCII.GetBytes('hamster-body')
}
else {
    $filled = [byte[]]::new($EchoBodyBytes)
    for ($index = 0; $index -lt $EchoBodyBytes; $index++) { $filled[$index] = 120 }
    $filled
}

function Resolve-ScenarioConfig([string]$ConfigPath, [string]$Name) {
    if ($MaxBodyMemory -lt 0) { return $ConfigPath }
    $source = Get-Content -Raw -LiteralPath $ConfigPath
    $stripped = ($source -split "`r?`n" | Where-Object { $_ -notmatch '^\s*max_body_memory\s*=' }) -join "`n"
    $patched = $stripped -replace '(?m)^\[server\]$', "[server]`nmax_body_memory = $MaxBodyMemory"
    if ($patched -eq $stripped) { throw "config $ConfigPath has no [server] table" }
    # Beside the original and still relative. These configs resolve their own
    # relative paths against the config file's directory, and Start-Process
    # joins an ArgumentList array without quoting, so an absolute path
    # containing a space would be split into two arguments.
    $target = Join-Path (Split-Path -Parent $ConfigPath) "hamster-bench-$PID-$Name-budget.toml"
    Set-Content -LiteralPath $target -Value $patched -Encoding utf8
    return $target
}
$scenarios = @(
    [PSCustomObject]@{ Name = 'simple'; Config = '.\benchmarks\php_simple.toml'; Path = '/'; Body = $emptyBody; Status = [Net.HttpStatusCode]::NotFound; Php = $false },
    [PSCustomObject]@{ Name = 'php_hello'; Config = '.\benchmarks\php_worker.toml'; Path = '/hello'; Body = $emptyBody; Status = [Net.HttpStatusCode]::OK; Php = $true },
    [PSCustomObject]@{ Name = 'php_json'; Config = '.\benchmarks\php_worker.toml'; Path = '/json'; Body = $emptyBody; Status = [Net.HttpStatusCode]::OK; Php = $true },
    [PSCustomObject]@{ Name = 'php_echo'; Config = '.\benchmarks\php_worker.toml'; Path = '/echo'; Body = $echoBody; Status = [Net.HttpStatusCode]::Created; Php = $true }
)
foreach ($scenario in $scenarios) {
    $scenario.Config = Resolve-ScenarioConfig $scenario.Config $scenario.Name
}
if ($ScenarioFilter -ne 'all') {
    $scenarios = @($scenarios | Where-Object Name -eq $ScenarioFilter)
}

try {

foreach ($scenario in $scenarios) {
    $results = @()
    for ($run = 1; $run -le $Runs; $run++) {
        $stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-bench-$PID-$($scenario.Name)-$run.out"
        $stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-bench-$PID-$($scenario.Name)-$run.err"
        $serverProcess = Start-Process -FilePath $executable -ArgumentList @('--config', $scenario.Config) -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
        $phpProcess = $null
        $client = $null
        try {
            $ready = $false
            $startupClock = [Diagnostics.Stopwatch]::StartNew()
            for ($attempt = 0; $attempt -lt 500; $attempt++) {
                try {
                    $probe = [Net.Sockets.TcpClient]::new()
                    $probe.Connect('127.0.0.1', 8080)
                    $probe.Dispose()
                    $ready = $true
                    break
                }
                catch {
                    Start-Sleep -Milliseconds 10
                }
            }
            $startupClock.Stop()
            if (-not $ready) {
                $errorText = if (Test-Path $stderrPath) { Get-Content -Raw $stderrPath } else { '' }
                throw "Server did not become ready: $errorText"
            }

            if ($scenario.Php) {
                for ($attempt = 0; $attempt -lt 100 -and $null -eq $phpProcess; $attempt++) {
                    $phpProcess = Find-PhpChild $serverProcess.Id
                    if ($null -eq $phpProcess) { Start-Sleep -Milliseconds 10 }
                }
                if ($null -eq $phpProcess) { throw 'PHP child process was not found' }
            }

            $workerBootstrapMs = if ($scenario.Php) {
                Get-WorkerBootstrapMilliseconds $stdoutPath
            } else {
                0.0
            }
            if ($scenario.Php -and [double]::IsNaN($workerBootstrapMs)) {
                throw 'Worker READY/bootstrap timing was not found in server output'
            }

            $client = [PhpBenchClient]::new('localhost', $scenario.Status)
            $firstMs = $client.First($scenario.Path, $scenario.Body).GetAwaiter().GetResult()
            $null = $client.Warmup($scenario.Path, $scenario.Body, $WarmupRequests).GetAwaiter().GetResult()
            $serverProcess.Refresh()
            if ($null -ne $phpProcess) { $phpProcess.Refresh() }
            $serverCpuBefore = $serverProcess.TotalProcessorTime.TotalSeconds
            $phpCpuBefore = if ($null -eq $phpProcess) { 0.0 } else { $phpProcess.TotalProcessorTime.TotalSeconds }
            $result = $client.Run($scenario.Path, $scenario.Body, $Requests).GetAwaiter().GetResult()
            $serverProcess.Refresh()
            if ($null -ne $phpProcess) { $phpProcess.Refresh() }
            $serverCpu = $serverProcess.TotalProcessorTime.TotalSeconds - $serverCpuBefore
            $phpCpu = if ($null -eq $phpProcess) { 0.0 } else { $phpProcess.TotalProcessorTime.TotalSeconds - $phpCpuBefore }
            $measurement = [PSCustomObject]@{
                Run = $run
                StartupMs = $startupClock.Elapsed.TotalMilliseconds
                WorkerBootstrapMs = $workerBootstrapMs
                FirstMs = $firstMs
                WallSeconds = $result.Seconds
                Rps = $Requests / $result.Seconds
                MinMs = $result.Milliseconds[0]
                P50Ms = Get-Percentile $result.Milliseconds 0.50
                P95Ms = Get-Percentile $result.Milliseconds 0.95
                P99Ms = Get-Percentile $result.Milliseconds 0.99
                MaxMs = $result.Milliseconds[$result.Milliseconds.Length - 1]
                QuantumPct = Get-QuantumPercent $result.Milliseconds
                ServerCpuUs = $serverCpu * 1000000 / $Requests
                PhpCpuUs = $phpCpu * 1000000 / $Requests
                CombinedCpuUs = ($serverCpu + $phpCpu) * 1000000 / $Requests
                ServerWorkingMiB = $serverProcess.WorkingSet64 / 1MB
                ServerPeakMiB = $serverProcess.PeakWorkingSet64 / 1MB
                PhpWorkingMiB = if ($null -eq $phpProcess) { 0.0 } else { $phpProcess.WorkingSet64 / 1MB }
                PhpPeakMiB = if ($null -eq $phpProcess) { 0.0 } else { $phpProcess.PeakWorkingSet64 / 1MB }
            }
            $results += $measurement
            Write-Output ("PHP_BENCH scenario={0} run={1} listener_ms={2:F3} bootstrap_ms={3:F3} first_ms={4:F3} wall_s={5:F3} rps={6:F0} min_ms={7:F3} p50_ms={8:F3} p95_ms={9:F3} p99_ms={10:F3} max_ms={11:F3} quantum_pct={12:F1} server_cpu_us={13:F3} php_cpu_us={14:F3} combined_cpu_us={15:F3} server_ws_mib={16:F1} server_peak_mib={17:F1} php_ws_mib={18:F1} php_peak_mib={19:F1}" -f
                $scenario.Name, $run, $measurement.StartupMs, $measurement.WorkerBootstrapMs,
                $measurement.FirstMs, $measurement.WallSeconds, $measurement.Rps,
                $measurement.MinMs, $measurement.P50Ms, $measurement.P95Ms,
                $measurement.P99Ms, $measurement.MaxMs, $measurement.QuantumPct,
                $measurement.ServerCpuUs, $measurement.PhpCpuUs, $measurement.CombinedCpuUs,
                $measurement.ServerWorkingMiB, $measurement.ServerPeakMiB,
                $measurement.PhpWorkingMiB, $measurement.PhpPeakMiB)

            if ($scenario.Php) {
                $null = $client.StopWorker().GetAwaiter().GetResult()
            }
        }
        finally {
            if ($null -ne $client) { $client.Dispose() }
            if (-not $serverProcess.HasExited) {
                Stop-Process -Id $serverProcess.Id
                Wait-Process -Id $serverProcess.Id -ErrorAction SilentlyContinue
            }
            if ($null -ne $phpProcess) {
                $phpProcess.Refresh()
                # HasExited is a snapshot: the worker can exit between the
                # refresh and the kill, which used to abort the whole run.
                if (-not $phpProcess.HasExited) {
                    Stop-Process -Id $phpProcess.Id -ErrorAction SilentlyContinue
                    Wait-Process -Id $phpProcess.Id -ErrorAction SilentlyContinue
                }
            }
            Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
        }
    }

    Write-Output ("PHP_MEDIAN scenario={0} requests={1} concurrency=1 listener_ms={2:F3} bootstrap_ms={3:F3} first_ms={4:F3} wall_s={5:F3} rps={6:F0} min_ms={7:F3} p50_ms={8:F3} p95_ms={9:F3} p99_ms={10:F3} max_ms={11:F3} quantum_pct={12:F1} server_cpu_us={13:F3} php_cpu_us={14:F3} combined_cpu_us={15:F3} server_ws_mib={16:F1} server_peak_mib={17:F1} php_ws_mib={18:F1} php_peak_mib={19:F1}" -f
        $scenario.Name, $Requests, (Get-Median $results.StartupMs),
        (Get-Median $results.WorkerBootstrapMs), (Get-Median $results.FirstMs),
        (Get-Median $results.WallSeconds), (Get-Median $results.Rps),
        (Get-Median $results.MinMs), (Get-Median $results.P50Ms),
        (Get-Median $results.P95Ms), (Get-Median $results.P99Ms),
        (Get-Median $results.MaxMs), (Get-Median $results.QuantumPct),
        (Get-Median $results.ServerCpuUs),
        (Get-Median $results.PhpCpuUs), (Get-Median $results.CombinedCpuUs),
        (Get-Median $results.ServerWorkingMiB), (Get-Median $results.ServerPeakMiB),
        (Get-Median $results.PhpWorkingMiB), (Get-Median $results.PhpPeakMiB))
}

}
finally {
    # Runs abort on unrelated failures often enough that end-of-script cleanup
    # is not good enough; the generated configs sit beside checked-in ones.
    if ($MaxBodyMemory -ge 0) {
        Get-ChildItem -LiteralPath './benchmarks' -Filter "hamster-bench-$PID-*-budget.toml" -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
    }
}
