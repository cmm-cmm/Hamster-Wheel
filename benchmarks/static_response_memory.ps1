param(
    [int[]]$ConnectionCounts = @(1, 4, 16, 32),
    [int]$FileMiB = 256,
    [int]$SampleMilliseconds = 2500,
    [int]$HeaderReadTimeoutSeconds = 30,
    [string]$Executable = '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe'
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Net.Sockets;
using System.Text;

public static class HamsterStaticHoldClients
{
    public static TcpClient[] Start(string host, int port, int count, string path)
    {
        var clients = new List<TcpClient>(count);
        byte[] request = Encoding.ASCII.GetBytes(
            "GET " + path + " HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        try
        {
            for (int i = 0; i < count; i++)
            {
                var client = new TcpClient();
                client.ReceiveBufferSize = 512;
                client.Connect(host, port);
                NetworkStream stream = client.GetStream();
                stream.Write(request, 0, request.Length);
                clients.Add(client);
            }
            return clients.ToArray();
        }
        catch
        {
            foreach (TcpClient client in clients) client.Dispose();
            throw;
        }
    }

    public static void Stop(TcpClient[] clients)
    {
        foreach (TcpClient client in clients) client.Dispose();
    }
}
'@

function Get-FreePort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try { return ([Net.IPEndPoint]$listener.LocalEndpoint).Port }
    finally { $listener.Stop() }
}

function Wait-Ready([int]$Port, [Diagnostics.Process]$Process) {
    for ($attempt = 0; $attempt -lt 200; $attempt++) {
        if ($Process.HasExited) { throw "Server exited before readiness with code $($Process.ExitCode)" }
        try {
            $probe = [Net.Sockets.TcpClient]::new()
            $probe.Connect('127.0.0.1', $Port)
            $probe.Dispose()
            return
        }
        catch { Start-Sleep -Milliseconds 20 }
    }
    throw 'Server did not become ready'
}

$executablePath = (Resolve-Path -LiteralPath $Executable).Path
$runRoot = Join-Path ([IO.Path]::GetTempPath()) ("hamster-static-memory-" + [Guid]::NewGuid().ToString('N'))
$documentRoot = Join-Path $runRoot 'public'
$largeFile = Join-Path $documentRoot 'large.bin'
[IO.Directory]::CreateDirectory($documentRoot) | Out-Null
$fixture = [IO.File]::Open($largeFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
try { $fixture.SetLength([int64]$FileMiB * 1MB) }
finally { $fixture.Dispose() }

try {
    foreach ($connectionCount in $ConnectionCounts) {
        $port = Get-FreePort
        $configPath = Join-Path $runRoot "hamster-$connectionCount.toml"
        $stdoutPath = Join-Path $runRoot "stdout-$connectionCount.txt"
        $stderrPath = Join-Path $runRoot "stderr-$connectionCount.txt"
        $tomlRoot = $documentRoot.Replace('\', '\\')
        $config = @"
[server]
listen = "127.0.0.1:$port"
max_connections = $($connectionCount + 8)
header_read_timeout = $HeaderReadTimeoutSeconds

[[hosts]]
name = "127.0.0.1"

[[hosts.locations]]
path = "/"
handler = "static"
root = "$tomlRoot"
"@
        [IO.File]::WriteAllText($configPath, $config)

        $server = Start-Process -FilePath $executablePath -ArgumentList @('--config', $configPath) `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath `
            -WindowStyle Hidden -PassThru
        $clients = $null
        try {
            Wait-Ready $port $server
            Start-Sleep -Milliseconds 250
            $server.Refresh()
            $idlePrivate = $server.PrivateMemorySize64
            $idleWorking = $server.WorkingSet64

            $clients = [HamsterStaticHoldClients]::Start('127.0.0.1', $port, $connectionCount, '/large.bin')
            $peakPrivate = $idlePrivate
            $peakWorking = $idleWorking
            $deadline = [Diagnostics.Stopwatch]::StartNew()
            while ($deadline.ElapsedMilliseconds -lt $SampleMilliseconds) {
                Start-Sleep -Milliseconds 20
                $server.Refresh()
                $peakPrivate = [Math]::Max($peakPrivate, $server.PrivateMemorySize64)
                $peakWorking = [Math]::Max($peakWorking, $server.WorkingSet64)
            }
            $heldPrivate = $server.PrivateMemorySize64

            [HamsterStaticHoldClients]::Stop($clients)
            $clients = $null
            Start-Sleep -Milliseconds 750
            $server.Refresh()
            $recoveredPrivate = $server.PrivateMemorySize64

            [PSCustomObject]@{
                Connections = $connectionCount
                FileMiB = $FileMiB
                IdlePrivateMiB = [Math]::Round($idlePrivate / 1MB, 2)
                PeakPrivateDeltaMiB = [Math]::Round(($peakPrivate - $idlePrivate) / 1MB, 2)
                PeakWorkingDeltaMiB = [Math]::Round(($peakWorking - $idleWorking) / 1MB, 2)
                HeldPrivateDeltaMiB = [Math]::Round(($heldPrivate - $idlePrivate) / 1MB, 2)
                RecoveredPrivateDeltaMiB = [Math]::Round(($recoveredPrivate - $idlePrivate) / 1MB, 2)
            }
        }
        finally {
            if ($null -ne $clients) { [HamsterStaticHoldClients]::Stop($clients) }
            if (-not $server.HasExited) {
                Stop-Process -Id $server.Id
                Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
            }
            $server.Dispose()
        }
    }
}
finally {
    if ([IO.Directory]::Exists($runRoot)) {
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            try {
                [IO.Directory]::Delete($runRoot, $true)
                break
            }
            catch {
                if ($attempt -eq 19) { throw }
                Start-Sleep -Milliseconds 50
            }
        }
    }
}
