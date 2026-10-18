param(
    [int]$Runs = 3,
    [int]$Requests = 500000,
    [int]$Concurrency = 32,
    [int]$WarmupRequests = 1000,
    [string]$RequestPath = '/',
    [int]$ExpectedStatus = 200,
    [string]$Executable = '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe',
    # Omitted means the built-in configuration, which every earlier run
    # used. Pass one to measure a configured feature against the same
    # workload.
    [string]$ConfigPath = ''
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Net;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class StaticBenchOutput
{
    public double Seconds;
    public double[] Milliseconds = Array.Empty<double>();
}

public sealed class StaticBenchClient : IDisposable
{
    private sealed class Counter { public int Value; }
    private readonly SocketsHttpHandler handler;
    private readonly HttpClient client;
    private readonly string requestPath;
    private readonly HttpStatusCode expectedStatus;

    public StaticBenchClient(string url, string requestPath, int expectedStatus, int concurrency)
    {
        handler = new SocketsHttpHandler { MaxConnectionsPerServer = concurrency };
        client = new HttpClient(handler) { BaseAddress = new Uri(url) };
        this.requestPath = requestPath;
        this.expectedStatus = (HttpStatusCode)expectedStatus;
    }

    public async Task Warmup(int requests)
    {
        for (int i = 0; i < requests; i++)
        {
            using (HttpResponseMessage response = await client.GetAsync(requestPath))
                if (response.StatusCode != expectedStatus)
                    throw new InvalidOperationException("Unexpected HTTP status: " + response.StatusCode);
        }
    }

    private async Task Worker(int count, double[] milliseconds, Counter counter)
    {
        while (true)
        {
            int index = Interlocked.Increment(ref counter.Value) - 1;
            if (index >= count) return;
            long started = Stopwatch.GetTimestamp();
            using (HttpResponseMessage response = await client.GetAsync(requestPath, HttpCompletionOption.ResponseContentRead))
            {
                if (response.StatusCode != expectedStatus)
                    throw new InvalidOperationException("Unexpected HTTP status: " + response.StatusCode);
            }
            milliseconds[index] = Stopwatch.GetElapsedTime(started).TotalMilliseconds;
        }
    }

    public async Task<StaticBenchOutput> Run(int requests, int concurrency)
    {
        var milliseconds = new double[requests];
        var counter = new Counter();
        var workers = new Task[concurrency];
        var elapsed = Stopwatch.StartNew();
        for (int i = 0; i < concurrency; i++)
            workers[i] = Worker(requests, milliseconds, counter);
        await Task.WhenAll(workers);
        elapsed.Stop();
        Array.Sort(milliseconds);
        return new StaticBenchOutput { Seconds = elapsed.Elapsed.TotalSeconds, Milliseconds = milliseconds };
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

$executable = (Resolve-Path -LiteralPath $Executable).Path
$logicalProcessors = [Environment]::ProcessorCount
$results = @()

$serverArguments = @()
if ($ConfigPath -ne '') {
    $serverArguments = @('--config', (Resolve-Path -LiteralPath $ConfigPath).Path)
}

for ($run = 1; $run -le $Runs; $run++) {
    $serverProcess = if ($serverArguments.Count -gt 0) {
        Start-Process -FilePath $executable -ArgumentList $serverArguments -PassThru -WindowStyle Hidden
    }
    else {
        Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
    }
    try {
        $ready = $false
        for ($attempt = 0; $attempt -lt 100; $attempt++) {
            try {
                $probe = [Net.Sockets.TcpClient]::new()
                $probe.Connect('127.0.0.1', 8080)
                $probe.Dispose()
                $ready = $true
                break
            }
            catch {
                Start-Sleep -Milliseconds 20
            }
        }
        if (-not $ready) { throw 'Server did not become ready' }

        $client = [StaticBenchClient]::new('http://127.0.0.1:8080', $RequestPath, $ExpectedStatus, $Concurrency)
        try {
            $null = $client.Warmup($WarmupRequests).GetAwaiter().GetResult()
            $serverProcess.Refresh()
            $cpuBefore = $serverProcess.TotalProcessorTime.TotalSeconds
            $result = $client.Run($Requests, $Concurrency).GetAwaiter().GetResult()
            $serverProcess.Refresh()
            $cpuSeconds = $serverProcess.TotalProcessorTime.TotalSeconds - $cpuBefore
            $effectiveCores = $cpuSeconds / $result.Seconds

            $measurement = [PSCustomObject]@{
                Run = $run
                WallSeconds = $result.Seconds
                Rps = $Requests / $result.Seconds
                P50Ms = Get-Percentile $result.Milliseconds 0.50
                P95Ms = Get-Percentile $result.Milliseconds 0.95
                P99Ms = Get-Percentile $result.Milliseconds 0.99
                CpuSeconds = $cpuSeconds
                EffectiveCores = $effectiveCores
                NormalizedCpuPercent = $effectiveCores / $logicalProcessors * 100
                CpuMicrosecondsPerRequest = $cpuSeconds * 1000000 / $Requests
                WorkingSetMiB = $serverProcess.WorkingSet64 / 1MB
                PeakWorkingSetMiB = $serverProcess.PeakWorkingSet64 / 1MB
            }
            $results += $measurement
            Write-Output ("RUN={0} WALL_S={1:F3} RPS={2:F0} P50_MS={3:F3} P95_MS={4:F3} P99_MS={5:F3} CPU_S={6:F3} EFFECTIVE_CORES={7:F3} CPU_NORMALIZED_PERCENT={8:F1} CPU_US_PER_REQUEST={9:F3} WORKING_MIB={10:F1} PEAK_MIB={11:F1}" -f
                $measurement.Run, $measurement.WallSeconds, $measurement.Rps, $measurement.P50Ms,
                $measurement.P95Ms, $measurement.P99Ms, $measurement.CpuSeconds,
                $measurement.EffectiveCores, $measurement.NormalizedCpuPercent,
                $measurement.CpuMicrosecondsPerRequest, $measurement.WorkingSetMiB,
                $measurement.PeakWorkingSetMiB)
        }
        finally {
            if ($null -ne $client) { $client.Dispose() }
        }
    }
    finally {
        if (-not $serverProcess.HasExited) {
            Stop-Process -Id $serverProcess.Id
            Wait-Process -Id $serverProcess.Id -ErrorAction SilentlyContinue
        }
    }
}

$medianRps = Get-Median $results.Rps
$medianP99 = Get-Median $results.P99Ms
$rpsRangePercent = (($results.Rps | Measure-Object -Maximum).Maximum - ($results.Rps | Measure-Object -Minimum).Minimum) / $medianRps * 100
$p99RangePercent = (($results.P99Ms | Measure-Object -Maximum).Maximum - ($results.P99Ms | Measure-Object -Minimum).Minimum) / $medianP99 * 100

Write-Output ("MEDIAN PATH={0} EXPECTED_STATUS={1} CONCURRENCY={2} REQUESTS={3} WALL_S={4:F3} RPS={5:F0} P50_MS={6:F3} P95_MS={7:F3} P99_MS={8:F3} CPU_S={9:F3} EFFECTIVE_CORES={10:F3} CPU_NORMALIZED_PERCENT={11:F1} CPU_US_PER_REQUEST={12:F3} WORKING_MIB={13:F1} PEAK_MIB={14:F1} RPS_RANGE_PERCENT={15:F1} P99_RANGE_PERCENT={16:F1}" -f
    $RequestPath, $ExpectedStatus, $Concurrency, $Requests, (Get-Median $results.WallSeconds), $medianRps,
    (Get-Median $results.P50Ms), (Get-Median $results.P95Ms),
    $medianP99, (Get-Median $results.CpuSeconds),
    (Get-Median $results.EffectiveCores), (Get-Median $results.NormalizedCpuPercent),
    (Get-Median $results.CpuMicrosecondsPerRequest), (Get-Median $results.WorkingSetMiB),
    (Get-Median $results.PeakWorkingSetMiB), $rpsRangePercent, $p99RangePercent)
