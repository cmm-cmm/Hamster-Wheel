param(
    [int[]]$ConnectionCounts = @(4, 8, 16, 32),
    [int]$QueueCapacity = 128,
    [int]$WorkerCount = 1,
    [int]$BodyMiB = 1,
    [double]$BodyFractionBeforeHold = 1.0,
    [int]$DelayMilliseconds = 10000,
    [long]$MaxBodyMemory = 0,
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Net.Sockets;
using System.Text;
using System.Threading.Tasks;

public static class BodyFloodClient
{
    // Counted so a run can report how the server answered the flood, not only
    // how much memory it used while answering it.
    public static int Refused;
    public static int Served;

    private static async Task SendAndHold(int port, byte[] header, byte[] body, int bodyBytes)
    {
        using var client = new TcpClient();
        await client.ConnectAsync("127.0.0.1", port).ConfigureAwait(false);
        using NetworkStream stream = client.GetStream();
        await stream.WriteAsync(header).ConfigureAwait(false);
        await stream.WriteAsync(body, 0, bodyBytes).ConfigureAwait(false);
        byte[] buffer = new byte[4096];
        bool classified = false;
        try
        {
            int count;
            while ((count = await stream.ReadAsync(buffer).ConfigureAwait(false)) != 0)
            {
                if (classified) continue;
                classified = true;
                string head = Encoding.ASCII.GetString(buffer, 0, Math.Min(count, 16));
                if (head.StartsWith("HTTP/1.1 503")) System.Threading.Interlocked.Increment(ref Refused);
                else System.Threading.Interlocked.Increment(ref Served);
            }
        }
        catch (SocketException) { }
        catch (ObjectDisposedException) { }
    }

    public static Task[] Start(int port, int connections, int bodyBytes, int initialBodyBytes, int delayMilliseconds)
    {
        Refused = 0;
        Served = 0;
        byte[] body = new byte[bodyBytes];
        string request = $"POST /delay?ms={delayMilliseconds} HTTP/1.1\r\n" +
            "Host: localhost\r\n" +
            "Connection: close\r\n" +
            $"Content-Length: {bodyBytes}\r\n\r\n";
        byte[] header = Encoding.ASCII.GetBytes(request);
        var tasks = new Task[connections];
        for (int index = 0; index < connections; index++)
            tasks[index] = SendAndHold(port, header, body, initialBodyBytes);
        return tasks;
    }
}
'@

function Get-ChildPhp([int]$ParentId) {
    @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentId AND Name = 'php.exe'" |
        Select-Object -ExpandProperty ProcessId |
        ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
}

function Wait-Ready([int]$Port, [System.Diagnostics.Process]$Process) {
    for ($attempt = 0; $attempt -lt 500; $attempt++) {
        if ($Process.HasExited) {
            throw "server exited before readiness (exit $($Process.ExitCode))"
        }
        try {
            $probe = [Net.Sockets.TcpClient]::new('127.0.0.1', $Port)
            $probe.Dispose()
            return
        }
        catch {
            Start-Sleep -Milliseconds 10
        }
    }
    throw 'server did not become ready'
}

$executable = (Resolve-Path -LiteralPath '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe').Path
$phpExecutable = (Resolve-Path -LiteralPath '.\php\8.5\php.exe').Path.Replace("'", "''")
$workerScript = (Resolve-Path -LiteralPath '.\fixtures\php-worker\worker.php').Path.Replace("'", "''")
$bodyBytes = $BodyMiB * 1MB
if ($BodyFractionBeforeHold -lt 0 -or $BodyFractionBeforeHold -gt 1) {
    throw 'BodyFractionBeforeHold must be between 0 and 1'
}
$rows = @()

foreach ($connections in $ConnectionCounts) {
    $tag = "$PID-$connections-$QueueCapacity"
    $configPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-body-$tag.toml"
    $stdoutPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-body-$tag.out"
    $stderrPath = Join-Path ([IO.Path]::GetTempPath()) "hamster-body-$tag.err"
    $config = @"
[server]
listen = "127.0.0.1:8080"
max_connections = $connections
body_read_timeout = 30
max_body_memory = $MaxBodyMemory

[[hosts]]
name = "localhost"

[[hosts.locations]]
path = "/"
handler = "php_worker"

[hosts.locations.php]
executable = '$phpExecutable'
script = '$workerScript'
workers = $WorkerCount
queue_capacity = $QueueCapacity
"@
    Set-Content -LiteralPath $configPath -Value $config -Encoding utf8
    Write-Host "launching connections=$connections"
    $server = Start-Process -FilePath $executable -ArgumentList @('--config', $configPath) -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    Write-Host "launched pid=$($server.Id)"
    $tasks = $null
    try {
        Wait-Ready 8080 $server
        Write-Host "ready connections=$connections"
        Start-Sleep -Milliseconds 250
        $server.Refresh()
        $php = @(Get-ChildPhp $server.Id)
        $beforePrivate = [double]$server.PrivateMemorySize64 / 1MB
        $beforeWorking = [double]$server.WorkingSet64 / 1MB
        $beforePhpPrivate = if ($php.Count -eq 0) { 0.0 } else { ($php | Measure-Object -Property PrivateMemorySize64 -Sum).Sum / 1MB }
        $beforePhpWorking = if ($php.Count -eq 0) { 0.0 } else { ($php | Measure-Object -Property WorkingSet64 -Sum).Sum / 1MB }

        $initialBodyBytes = [int][Math]::Floor($bodyBytes * $BodyFractionBeforeHold)
        $tasks = [BodyFloodClient]::Start(8080, $connections, $bodyBytes, $initialBodyBytes, $DelayMilliseconds)
        Write-Host "flood started connections=$connections"
        Start-Sleep -Milliseconds 1000
        $server.Refresh()
        $php = @(Get-ChildPhp $server.Id)
        $afterPrivate = [double]$server.PrivateMemorySize64 / 1MB
        $afterWorking = [double]$server.WorkingSet64 / 1MB
        $afterPhpPrivate = if ($php.Count -eq 0) { 0.0 } else { ($php | Measure-Object -Property PrivateMemorySize64 -Sum).Sum / 1MB }
        $afterPhpWorking = if ($php.Count -eq 0) { 0.0 } else { ($php | Measure-Object -Property WorkingSet64 -Sum).Sum / 1MB }
        Write-Host "sampled connections=$connections"
        $rows += [PSCustomObject]@{
            Connections = $connections
            QueueCapacity = $QueueCapacity
            BodyMiB = $BodyMiB
            MaxBodyMemory = $MaxBodyMemory
            Refused = [BodyFloodClient]::Refused
            Served = [BodyFloodClient]::Served
            ServerPrivateBeforeMiB = [Math]::Round($beforePrivate, 2)
            ServerPrivateAfterMiB = [Math]::Round($afterPrivate, 2)
            ServerPrivateDeltaMiB = [Math]::Round($afterPrivate - $beforePrivate, 2)
            ServerWorkingBeforeMiB = [Math]::Round($beforeWorking, 2)
            ServerWorkingAfterMiB = [Math]::Round($afterWorking, 2)
            ServerWorkingDeltaMiB = [Math]::Round($afterWorking - $beforeWorking, 2)
            PhpPrivateBeforeMiB = [Math]::Round($beforePhpPrivate, 2)
            PhpPrivateAfterMiB = [Math]::Round($afterPhpPrivate, 2)
            PhpPrivateDeltaMiB = [Math]::Round($afterPhpPrivate - $beforePhpPrivate, 2)
            PhpWorkingBeforeMiB = [Math]::Round($beforePhpWorking, 2)
            PhpWorkingAfterMiB = [Math]::Round($afterPhpWorking, 2)
            PhpWorkingDeltaMiB = [Math]::Round($afterPhpWorking - $beforePhpWorking, 2)
        }
        $rows[-1] | Format-List
    }
    finally {
        Write-Host "stopping connections=$connections"
        if ($server.HasExited -and (Test-Path -LiteralPath $stderrPath)) {
            $diagnostic = Get-Content -Raw -LiteralPath $stderrPath
            if ($diagnostic -ne '') { Write-Warning $diagnostic }
        }
        if ($null -ne $server -and -not $server.HasExited) {
            Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
            Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
        }
        Remove-Item -LiteralPath $configPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
    }
}

if ($OutputPath -ne '') {
    $rows | Export-Csv -LiteralPath $OutputPath -NoTypeInformation
}
$rows | Format-Table -AutoSize
