param(
    [int]$Requests = 2000,
    [int]$WarmupRequests = 100
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class PhpConcurrencyOutput
{
    public int Successes;
    public double Seconds;
    public double[] Milliseconds = Array.Empty<double>();
}

public sealed class PhpConcurrencyClient : IDisposable
{
    private readonly SocketsHttpHandler handler;
    private readonly HttpClient client;

    public PhpConcurrencyClient(int concurrency)
    {
        handler = new SocketsHttpHandler { MaxConnectionsPerServer = concurrency };
        client = new HttpClient(handler) {
            BaseAddress = new Uri("http://127.0.0.1:8080"),
            Timeout = TimeSpan.FromSeconds(10)
        };
        client.DefaultRequestHeaders.Host = "localhost";
    }

    private async Task<string> Get(string path)
    {
        using (HttpResponseMessage response = await client.GetAsync(path, HttpCompletionOption.ResponseContentRead))
        {
            response.EnsureSuccessStatusCode();
            return await response.Content.ReadAsStringAsync();
        }
    }

    public async Task Warmup(int requests)
    {
        for (int index = 0; index < requests; index++)
            if (await Get("/hello") != "Hello from PHP") throw new InvalidOperationException("corrupt warm-up response");
    }

    public async Task<PhpConcurrencyOutput> Run(int requests, int concurrency)
    {
        var milliseconds = new double[requests];
        int next = -1;
        int successes = 0;
        var elapsed = Stopwatch.StartNew();
        var tasks = new Task[concurrency];
        for (int worker = 0; worker < concurrency; worker++)
        {
            tasks[worker] = Task.Run(async () => {
                while (true)
                {
                    int index = Interlocked.Increment(ref next);
                    if (index >= requests) break;
                    long started = Stopwatch.GetTimestamp();
                    string body = await Get("/hello");
                    milliseconds[index] = Stopwatch.GetElapsedTime(started).TotalMilliseconds;
                    if (body != "Hello from PHP") throw new InvalidOperationException("corrupt response body");
                    Interlocked.Increment(ref successes);
                }
            });
        }
        await Task.WhenAll(tasks);
        elapsed.Stop();
        Array.Sort(milliseconds);
        return new PhpConcurrencyOutput { Successes = successes, Seconds = elapsed.Elapsed.TotalSeconds, Milliseconds = milliseconds };
    }

    public Task<string> State() { return Get("/state"); }
    public async Task StopWorker() { using (HttpResponseMessage response = await client.GetAsync("/exit-after-response")) { } }

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

function Find-PhpChild([int]$ParentId) {
    $processInfo = Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -First 1
    if ($null -eq $processInfo) { return $null }
    return Get-Process -Id $processInfo.ProcessId -ErrorAction SilentlyContinue
}

$executable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-concurrency-$PID.out"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-concurrency-$PID.err"
$server = Start-Process -FilePath $executable -ArgumentList @('--config', '.\benchmarks\php_worker.toml') -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
$php = $null
$cleanupClient = $null
try {
    for ($attempt = 0; $attempt -lt 500; $attempt++) {
        try {
            $probe = [Net.Sockets.TcpClient]::new('127.0.0.1', 8080)
            $probe.Dispose()
            break
        }
        catch {
            if ($attempt -eq 499) { throw 'Server did not become ready' }
            Start-Sleep -Milliseconds 10
        }
    }
    for ($attempt = 0; $attempt -lt 100 -and $null -eq $php; $attempt++) {
        $php = Find-PhpChild $server.Id
        if ($null -eq $php) { Start-Sleep -Milliseconds 10 }
    }
    if ($null -eq $php) { throw 'PHP child process was not found' }

    $cleanupClient = [PhpConcurrencyClient]::new(1)
    $first = $cleanupClient.State().GetAwaiter().GetResult() | ConvertFrom-Json
    foreach ($concurrency in 1, 2, 4, 8, 16) {
        $client = [PhpConcurrencyClient]::new($concurrency)
        try {
            $null = $client.Warmup($WarmupRequests).GetAwaiter().GetResult()
            $result = $client.Run($Requests, $concurrency).GetAwaiter().GetResult()
            if ($result.Successes -ne $Requests) { throw "Concurrency $concurrency returned only $($result.Successes) successes" }
            Write-Output ("PHP_CONCURRENCY concurrency={0} requests={1} successes={2} wall_s={3:F3} rps={4:F0} min_ms={5:F3} p50_ms={6:F3} p95_ms={7:F3} p99_ms={8:F3} max_ms={9:F3}" -f
                $concurrency, $Requests, $result.Successes, $result.Seconds,
                ($Requests / $result.Seconds), $result.Milliseconds[0],
                (Get-Percentile $result.Milliseconds 0.50),
                (Get-Percentile $result.Milliseconds 0.95),
                (Get-Percentile $result.Milliseconds 0.99),
                $result.Milliseconds[$result.Milliseconds.Length - 1])
        }
        finally {
            $client.Dispose()
        }
    }

    $last = $cleanupClient.State().GetAwaiter().GetResult() | ConvertFrom-Json
    $expectedLast = 2 + (5 * ($WarmupRequests + $Requests))
    if ($first.pid -ne $php.Id -or $last.pid -ne $php.Id) { throw 'Worker PID changed during concurrency smoke' }
    if ($last.bootstrap -ne 1) { throw 'Application bootstrapped more than once' }
    if ($last.request -ne $expectedLast) { throw "Expected request counter $expectedLast, got $($last.request)" }
    Write-Output "PHP_CONCURRENCY_STATE pid=$($last.pid) bootstrap=$($last.bootstrap) final_counter=$($last.request)"

    $null = $cleanupClient.StopWorker().GetAwaiter().GetResult()
    Start-Sleep -Milliseconds 100
    $php.Refresh()
    if (-not $php.HasExited) { throw 'PHP worker did not exit after stop request' }
}
finally {
    if ($null -ne $cleanupClient) { $cleanupClient.Dispose() }
    if (-not $server.HasExited) {
        Stop-Process -Id $server.Id
        Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
    }
    if ($null -ne $php) {
        $php.Refresh()
        if (-not $php.HasExited) {
            Stop-Process -Id $php.Id
            Wait-Process -Id $php.Id -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
}
