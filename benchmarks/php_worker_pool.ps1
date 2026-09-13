param(
    [int]$Runs = 3,
    [int]$Requests = 20000,
    [int]$WarmupRequests = 1000,
    [double]$MinimumSeconds = 8.0,
    [int[]]$WorkerCounts = @(1, 2, 4, 8),
    [int[]]$Concurrencies = @(1, 2, 4, 8, 16, 32),
    [ValidateSet('all', 'hello', 'json')]
    [string]$Workload = 'all'
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class PoolBenchOutput
{
    public double Seconds;
    public int Requests;
    public double[] Milliseconds = Array.Empty<double>();
}

public sealed class PoolBenchClient : IDisposable
{
    private readonly SocketsHttpHandler handler;
    private readonly HttpClient client;

    public PoolBenchClient(int concurrency)
    {
        handler = new SocketsHttpHandler { MaxConnectionsPerServer = concurrency };
        client = new HttpClient(handler) {
            BaseAddress = new Uri("http://127.0.0.1:8080"),
            Timeout = TimeSpan.FromSeconds(10)
        };
        client.DefaultRequestHeaders.Host = "localhost";
    }

    private async Task Request(string path)
    {
        using (HttpResponseMessage response = await client.GetAsync(path, HttpCompletionOption.ResponseContentRead))
        {
            response.EnsureSuccessStatusCode();
        }
    }

    public async Task RunWarmup(string path, int requests)
    {
        for (int index = 0; index < requests; index++) await Request(path);
    }

    public async Task<PoolBenchOutput> Run(string path, int minimumRequests, int concurrency, double minimumSeconds)
    {
        var samples = new List<double>[concurrency];
        long completed = 0;
        var elapsed = Stopwatch.StartNew();
        long deadline = Stopwatch.GetTimestamp() + (long)(minimumSeconds * Stopwatch.Frequency);
        var tasks = new Task[concurrency];
        for (int slot = 0; slot < concurrency; slot++)
        {
            int sampleSlot = slot;
            samples[sampleSlot] = new List<double>();
            tasks[slot] = Task.Run(async () => {
                while (true)
                {
                    if (Interlocked.Read(ref completed) >= minimumRequests && Stopwatch.GetTimestamp() >= deadline)
                        break;
                    long started = Stopwatch.GetTimestamp();
                    await Request(path);
                    samples[sampleSlot].Add(Stopwatch.GetElapsedTime(started).TotalMilliseconds);
                    Interlocked.Increment(ref completed);
                }
            });
        }
        await Task.WhenAll(tasks);
        elapsed.Stop();
        int requests = 0;
        for (int slot = 0; slot < samples.Length; slot++) requests += samples[slot].Count;
        var milliseconds = new double[requests];
        int offset = 0;
        for (int slot = 0; slot < samples.Length; slot++)
        {
            samples[slot].CopyTo(milliseconds, offset);
            offset += samples[slot].Count;
        }
        Array.Sort(milliseconds);
        return new PoolBenchOutput {
            Seconds = elapsed.Elapsed.TotalSeconds,
            Requests = requests,
            Milliseconds = milliseconds
        };
    }

    public void Dispose()
    {
        client.Dispose();
        handler.Dispose();
    }
}
'@

function Get-Percentile([double[]]$Values, [double]$Percentile) {
    return $Values[[Math]::Floor(($Values.Length - 1) * $Percentile)]
}

function Get-Median($Values) {
    $sorted = @($Values | Sort-Object)
    return $sorted[[Math]::Floor($sorted.Count / 2)]
}

function Get-Minimum($Values) {
    return ($Values | Measure-Object -Minimum).Minimum
}

function Get-Maximum($Values) {
    return ($Values | Measure-Object -Maximum).Maximum
}

function Get-PhpChildren([int]$ParentId) {
    $ids = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -ExpandProperty ProcessId)
    return @($ids | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
}

function Stop-ProcessSet($Processes) {
    foreach ($process in @($Processes)) {
        if ($null -eq $process) { continue }
        $running = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
        if ($null -ne $running) {
            Stop-Process -Id $running.Id -ErrorAction SilentlyContinue
            Wait-Process -Id $running.Id -ErrorAction SilentlyContinue
        }
    }
}

$executable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$phpExecutable = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path.Replace("'", "''")
$workerScript = (Resolve-Path -LiteralPath '.\fixtures\php-worker\worker.php').Path.Replace("'", "''")
$workloads = if ($Workload -eq 'all') {
    @([PSCustomObject]@{ Name = 'hello'; Path = '/hello' }, [PSCustomObject]@{ Name = 'json'; Path = '/json' })
} else {
    @([PSCustomObject]@{ Name = $Workload; Path = "/$Workload" })
}

foreach ($workers in $WorkerCounts) {
    if ($workers -notin 1, 2, 4, 8) { throw "Unsupported worker count $workers; expected 1, 2, 4, or 8" }
    foreach ($concurrency in $Concurrencies) {
        foreach ($case in $workloads) {
            $measurements = @()
            for ($run = 1; $run -le $Runs; $run++) {
                $tag = "$PID-$workers-$concurrency-$($case.Name)-$run"
                $configPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pool-$tag.toml"
                $stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pool-$tag.out"
                $stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-pool-$tag.err"
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
workers = $workers
queue_capacity = 128
"@
                Set-Content -LiteralPath $configPath -Value $config -Encoding UTF8
                $server = Start-Process -FilePath $executable -ArgumentList @('--config', $configPath) -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
                $phpProcesses = @()
                $client = $null
                try {
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
                    for ($attempt = 0; $attempt -lt 100; $attempt++) {
                        $phpProcesses = @(Get-PhpChildren $server.Id)
                        if ($phpProcesses.Count -eq $workers) { break }
                        Start-Sleep -Milliseconds 10
                    }
                    if ($phpProcesses.Count -ne $workers) {
                        throw "Expected $workers PHP children, found $($phpProcesses.Count)"
                    }

                    $client = [PoolBenchClient]::new($concurrency)
                    $null = $client.RunWarmup($case.Path, $WarmupRequests).GetAwaiter().GetResult()
                    $server.Refresh()
                    foreach ($php in $phpProcesses) { $php.Refresh() }
                    $serverCpuBefore = $server.TotalProcessorTime.TotalSeconds
                    $phpCpuBefore = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum
                    $result = $client.Run($case.Path, $Requests, $concurrency, $MinimumSeconds).GetAwaiter().GetResult()
                    $server.Refresh()
                    foreach ($php in $phpProcesses) { $php.Refresh() }
                    $serverCpu = $server.TotalProcessorTime.TotalSeconds - $serverCpuBefore
                    $phpCpuAfter = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum
                    $phpCpu = $phpCpuAfter - $phpCpuBefore
                    $measurement = [PSCustomObject]@{
                        Requests = $result.Requests
                        WallSeconds = $result.Seconds
                        Rps = $result.Requests / $result.Seconds
                        P50 = Get-Percentile $result.Milliseconds 0.50
                        P95 = Get-Percentile $result.Milliseconds 0.95
                        P99 = Get-Percentile $result.Milliseconds 0.99
                        ServerCpuUs = $serverCpu * 1000000 / $result.Requests
                        PhpCpuUs = $phpCpu * 1000000 / $result.Requests
                        CombinedCpuUs = ($serverCpu + $phpCpu) * 1000000 / $result.Requests
                        ServerRamMiB = $server.WorkingSet64 / 1MB
                        PhpRamMiB = ($phpProcesses | Measure-Object WorkingSet64 -Sum).Sum / 1MB
                    }
                    $measurements += $measurement
                    Write-Output ("PHP_POOL_RUN workload={0} workers={1} concurrency={2} run={3} requests={4} wall_s={5:F3} rps={6:F0} p50_ms={7:F3} p95_ms={8:F3} p99_ms={9:F3} server_cpu_us={10:F3} php_cpu_us={11:F3} combined_cpu_us={12:F3} server_ram_mib={13:F1} php_ram_mib={14:F1} pids={15}" -f
                        $case.Name, $workers, $concurrency, $run, $measurement.Requests,
                        $measurement.WallSeconds, $measurement.Rps,
                        $measurement.P50, $measurement.P95, $measurement.P99,
                        $measurement.ServerCpuUs, $measurement.PhpCpuUs, $measurement.CombinedCpuUs,
                        $measurement.ServerRamMiB, $measurement.PhpRamMiB,
                        (($phpProcesses.Id | Sort-Object) -join ':'))
                }
                finally {
                    if ($null -ne $client) { $client.Dispose() }
                    if (-not $server.HasExited) {
                        Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
                        Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
                    }
                    Stop-ProcessSet $phpProcesses
                    Remove-Item -LiteralPath $configPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
                }
            }
            Write-Output ("PHP_POOL_MEDIAN workload={0} workers={1} concurrency={2} runs={3} minimum_requests={4} minimum_seconds={5:F1} queue_capacity=128 measured_requests={6:F0} wall_s={7:F3} rps_min={8:F0} rps_median={9:F0} rps_max={10:F0} p50_ms={11:F3} p95_ms={12:F3} p99_ms={13:F3} server_cpu_us={14:F3} php_cpu_us={15:F3} combined_cpu_us={16:F3} server_ram_mib={17:F1} php_ram_mib={18:F1}" -f
                $case.Name, $workers, $concurrency, $Runs, $Requests, $MinimumSeconds,
                (Get-Median $measurements.Requests), (Get-Median $measurements.WallSeconds),
                (Get-Minimum $measurements.Rps), (Get-Median $measurements.Rps),
                (Get-Maximum $measurements.Rps), (Get-Median $measurements.P50),
                (Get-Median $measurements.P95), (Get-Median $measurements.P99),
                (Get-Median $measurements.ServerCpuUs), (Get-Median $measurements.PhpCpuUs),
                (Get-Median $measurements.CombinedCpuUs), (Get-Median $measurements.ServerRamMiB),
                (Get-Median $measurements.PhpRamMiB))
        }
    }
}
