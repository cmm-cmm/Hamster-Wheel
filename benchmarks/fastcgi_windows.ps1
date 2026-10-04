param(
    [int] $Runs = 3,
    [int] $Seconds = 6,
    [int] $Warmup = 200,
    [int] $Endpoints = 2,
    [int[]] $Concurrencies = @(1, 4, 16, 32),
    [ValidateSet('hello', 'json')]
    [string[]] $Workloads = @('hello', 'json'),
    [string] $OutputPath = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -TypeDefinition @'
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class FastCgiBenchResult {
    public double Seconds;
    public int Successes;
    public int Errors;
    public double[] Latencies = Array.Empty<double>();
}

public sealed class FastCgiBenchClient : IDisposable {
    readonly SocketsHttpHandler handler;
    readonly HttpClient client;
    public FastCgiBenchClient(int concurrency, int port) {
        handler = new SocketsHttpHandler { MaxConnectionsPerServer = concurrency };
        client = new HttpClient(handler) {
            BaseAddress = new Uri("http://127.0.0.1:" + port),
            Timeout = TimeSpan.FromSeconds(10)
        };
        client.DefaultRequestHeaders.Host = "localhost";
    }
    async Task<bool> Request(string path) {
        using (var response = await client.GetAsync(path, HttpCompletionOption.ResponseContentRead)) {
            if ((int)response.StatusCode != 200) return false;
            string body = await response.Content.ReadAsStringAsync();
            return body == (path.EndsWith("json") ? "{\"message\":\"Hello from PHP\"}" : "Hello from PHP");
        }
    }
    public async Task Warmup(string path, int count) {
        for (int i = 0; i < count; i++)
            if (!await Request(path)) throw new Exception("invalid FastCGI warm-up response");
    }
    public async Task<FastCgiBenchResult> Run(string path, int concurrency, int seconds) {
        var samples = new ConcurrentBag<double>();
        int errors = 0;
        var elapsed = Stopwatch.StartNew();
        long deadline = Stopwatch.GetTimestamp() + (long)seconds * Stopwatch.Frequency;
        var tasks = new Task[concurrency];
        for (int i = 0; i < tasks.Length; i++) {
            tasks[i] = Task.Run(async () => {
                while (Stopwatch.GetTimestamp() < deadline) {
                    long started = Stopwatch.GetTimestamp();
                    try {
                        if (await Request(path)) samples.Add(Stopwatch.GetElapsedTime(started).TotalMilliseconds);
                        else Interlocked.Increment(ref errors);
                    } catch { Interlocked.Increment(ref errors); }
                }
            });
        }
        await Task.WhenAll(tasks);
        elapsed.Stop();
        var latencies = samples.ToArray();
        Array.Sort(latencies);
        return new FastCgiBenchResult {
            Seconds = elapsed.Elapsed.TotalSeconds, Successes = latencies.Length,
            Errors = errors, Latencies = latencies
        };
    }
    public void Dispose() { client.Dispose(); handler.Dispose(); }
}
'@

function Free-Port {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try { return $listener.LocalEndpoint.Port } finally { $listener.Stop() }
}

function Wait-Listener([int] $Port, [Diagnostics.Process] $Process) {
    for ($attempt = 0; $attempt -lt 200; $attempt++) {
        if ($Process.HasExited) { throw "process $($Process.Id) exited before listening on $Port" }
        try {
            $socket = [Net.Sockets.TcpClient]::new('127.0.0.1', $Port)
            $socket.Dispose()
            return
        } catch { Start-Sleep -Milliseconds 25 }
    }
    throw "process $($Process.Id) did not listen on $Port"
}

function Percentile([double[]] $Values, [double] $Fraction) {
    if ($Values.Length -eq 0) { return [double]::NaN }
    return $Values[[Math]::Floor(($Values.Length - 1) * $Fraction)]
}

function Median($Values) {
    $sorted = @($Values | Sort-Object)
    return $sorted[[Math]::Floor($sorted.Count / 2)]
}

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$serverExe = (Resolve-Path (Join-Path $repo 'target/x86_64-pc-windows-msvc/release/hamster-wheel.exe')).Path
$phpExe = (Resolve-Path (Join-Path $repo 'php/8.5/php-cgi.exe')).Path
$root = (Resolve-Path (Join-Path $repo 'fixtures/php-cgi')).Path.Replace("'", "''")
$results = @()

foreach ($concurrency in $Concurrencies) {
    foreach ($workload in $Workloads) {
        $cell = @()
        for ($run = 1; $run -le $Runs; $run++) {
            $scratch = Join-Path ([IO.Path]::GetTempPath()) "hamster-fcgi-bench-$PID-$concurrency-$workload-$run"
            if (Test-Path -LiteralPath $scratch) { throw "scratch path already exists: $scratch" }
            New-Item -ItemType Directory -Path $scratch | Out-Null
            $phpProcesses = @()
            $server = $null
            $client = $null
            $savedMaxRequests = [Environment]::GetEnvironmentVariable('PHP_FCGI_MAX_REQUESTS')
            try {
                [Environment]::SetEnvironmentVariable('PHP_FCGI_MAX_REQUESTS', '0')
                $ports = @()
                for ($index = 0; $index -lt $Endpoints; $index++) {
                    $port = Free-Port
                    $ports += $port
                    $php = Start-Process -FilePath $phpExe -ArgumentList @('-b', "127.0.0.1:$port", '-d', 'display_errors=0') -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $scratch "php-$index.err")
                    $phpProcesses += $php
                    Wait-Listener $port $php
                }
                $listenPort = Free-Port
                $addresses = ($ports | ForEach-Object { '"127.0.0.1:' + $_ + '"' }) -join ', '
                $config = @"
[server]
listen = "127.0.0.1:$listenPort"

[[hosts]]
name = "localhost"

[[hosts.locations]]
path = "/"
handler = "fastcgi"
root = '$root'

[hosts.locations.fastcgi]
servers = [$addresses]
concurrency = 1
connect_timeout = 2
response_timeout = 10
"@
                $configPath = Join-Path $scratch 'hamster.toml'
                [IO.File]::WriteAllText($configPath, $config)
                $server = Start-Process -FilePath $serverExe -ArgumentList @('--config', $configPath) -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $scratch 'hamster.err')
                Wait-Listener $listenPort $server
                $path = "/bench.php?workload=$workload"
                $client = [FastCgiBenchClient]::new($concurrency, $listenPort)
                $null = $client.Warmup($path, $Warmup).GetAwaiter().GetResult()
                $server.Refresh()
                foreach ($php in $phpProcesses) { $php.Refresh() }
                $serverCpuBefore = $server.TotalProcessorTime.TotalSeconds
                $phpCpuBefore = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum
                $measurement = $client.Run($path, $concurrency, $Seconds).GetAwaiter().GetResult()
                $server.Refresh()
                foreach ($php in $phpProcesses) { $php.Refresh() }
                $serverCpu = $server.TotalProcessorTime.TotalSeconds - $serverCpuBefore
                $phpCpu = ($phpProcesses | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum - $phpCpuBefore
                $row = [PSCustomObject]@{
                    Concurrency = $concurrency; Workload = $workload; Run = $run; Endpoints = $Endpoints
                    Requests = $measurement.Successes; Errors = $measurement.Errors
                    Seconds = $measurement.Seconds; Rps = $measurement.Successes / $measurement.Seconds
                    P50Ms = Percentile $measurement.Latencies 0.50
                    P95Ms = Percentile $measurement.Latencies 0.95
                    P99Ms = Percentile $measurement.Latencies 0.99
                    ServerCpuUs = $serverCpu * 1e6 / [Math]::Max($measurement.Successes, 1)
                    PhpCpuUs = $phpCpu * 1e6 / [Math]::Max($measurement.Successes, 1)
                    ServerRamMiB = $server.WorkingSet64 / 1MB
                    PhpRamMiB = ($phpProcesses | Measure-Object WorkingSet64 -Sum).Sum / 1MB
                }
                $cell += $row
                $results += $row
                Write-Output ("FASTCGI_RUN {0}" -f ($row | ConvertTo-Json -Compress))
                if ($row.Errors -ne 0) { throw "benchmark observed $($row.Errors) failed requests" }
            }
            finally {
                if ($null -ne $client) { $client.Dispose() }
                foreach ($process in @($server) + $phpProcesses) {
                    if ($null -eq $process) { continue }
                    $process.Refresh()
                    if (-not $process.HasExited) {
                        Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
                        Wait-Process -Id $process.Id -ErrorAction SilentlyContinue
                    }
                }
                [Environment]::SetEnvironmentVariable('PHP_FCGI_MAX_REQUESTS', $savedMaxRequests)
                if ((Resolve-Path -LiteralPath $scratch).Path.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase)) {
                    Remove-Item -LiteralPath $scratch -Recurse -Force
                }
            }
        }
        Write-Output ("FASTCGI_CELL workload={0} concurrency={1} endpoints={2} rps_min={3:F0} rps_median={4:F0} rps_max={5:F0} p50_ms={6:F3} p95_ms={7:F3} p99_ms={8:F3} server_cpu_us={9:F1} php_cpu_us={10:F1} server_ram_mib={11:F1} php_ram_mib={12:F1} errors={13}" -f
            $workload, $concurrency, $Endpoints, ($cell.Rps | Measure-Object -Minimum).Minimum,
            (Median $cell.Rps), ($cell.Rps | Measure-Object -Maximum).Maximum,
            (Median $cell.P50Ms), (Median $cell.P95Ms), (Median $cell.P99Ms),
            (Median $cell.ServerCpuUs), (Median $cell.PhpCpuUs),
            (Median $cell.ServerRamMiB), (Median $cell.PhpRamMiB),
            ($cell.Errors | Measure-Object -Sum).Sum)
    }
}

if ($OutputPath) {
    [PSCustomObject]@{
        Timestamp = [DateTimeOffset]::Now.ToString('o')
        Platform = [Environment]::OSVersion.VersionString
        Runs = $Runs; Seconds = $Seconds; Warmup = $Warmup
        Endpoints = $Endpoints; Concurrencies = $Concurrencies; Workloads = $Workloads
        Results = $results
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $OutputPath -Encoding UTF8
}
