param(
    [int]$Requests = 100000
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Net.Http;
using System.Threading.Tasks;

public sealed class PhpSoakOutput
{
    public int Successes;
    public double Seconds;
    public string FirstState = "";
    public string LastState = "";
}

public sealed class PhpSoakClient : IDisposable
{
    private readonly SocketsHttpHandler handler = new SocketsHttpHandler { MaxConnectionsPerServer = 1 };
    private readonly HttpClient client;

    public PhpSoakClient()
    {
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

    public async Task<PhpSoakOutput> Run(int requests)
    {
        if (requests < 2) throw new ArgumentOutOfRangeException(nameof(requests));
        var output = new PhpSoakOutput();
        var elapsed = Stopwatch.StartNew();
        output.FirstState = await Get("/state");
        output.Successes++;
        for (int index = 1; index < requests - 1; index++)
        {
            string body = await Get("/hello");
            if (body != "Hello from PHP") throw new InvalidOperationException("corrupt response body");
            output.Successes++;
        }
        output.LastState = await Get("/state");
        output.Successes++;
        elapsed.Stop();
        output.Seconds = elapsed.Elapsed.TotalSeconds;
        return output;
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

function Find-PhpChild([int]$ParentId) {
    $processInfo = Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -First 1
    if ($null -eq $processInfo) { return $null }
    return Get-Process -Id $processInfo.ProcessId -ErrorAction SilentlyContinue
}

$executable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-soak-$PID.out"
$stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-php-soak-$PID.err"
$server = Start-Process -FilePath $executable -ArgumentList @('--config', '.\benchmarks\php_worker.toml') -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
$php = $null
$client = $null
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

    $server.Refresh()
    $php.Refresh()
    $serverBefore = $server.WorkingSet64 / 1MB
    $phpBefore = $php.WorkingSet64 / 1MB
    $workerPid = $php.Id

    $client = [PhpSoakClient]::new()
    $result = $client.Run($Requests).GetAwaiter().GetResult()
    $first = $result.FirstState | ConvertFrom-Json
    $last = $result.LastState | ConvertFrom-Json
    $server.Refresh()
    $php.Refresh()

    if ($result.Successes -ne $Requests) { throw "Expected $Requests successes, got $($result.Successes)" }
    if ($first.pid -ne $workerPid -or $last.pid -ne $workerPid) { throw 'Worker PID changed during soak' }
    if ($first.bootstrap -ne 1 -or $last.bootstrap -ne 1) { throw 'Application bootstrapped more than once' }
    if ($first.request -ne 1 -or $last.request -ne $Requests) { throw 'Request counter did not match soak length' }

    Write-Output ("PHP_SOAK requests={0} successes={1} wall_s={2:F3} rps={3:F0} pid={4} bootstrap={5} first_counter={6} last_counter={7} server_ws_before_mib={8:F1} server_ws_after_mib={9:F1} php_ws_before_mib={10:F1} php_ws_after_mib={11:F1} server_peak_mib={12:F1} php_peak_mib={13:F1}" -f
        $Requests, $result.Successes, $result.Seconds, ($Requests / $result.Seconds),
        $workerPid, $last.bootstrap, $first.request, $last.request,
        $serverBefore, ($server.WorkingSet64 / 1MB), $phpBefore, ($php.WorkingSet64 / 1MB),
        ($server.PeakWorkingSet64 / 1MB), ($php.PeakWorkingSet64 / 1MB))

    $null = $client.StopWorker().GetAwaiter().GetResult()
    Start-Sleep -Milliseconds 100
    $php.Refresh()
    if (-not $php.HasExited) { throw 'PHP worker did not exit after stop request' }
}
finally {
    if ($null -ne $client) { $client.Dispose() }
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
