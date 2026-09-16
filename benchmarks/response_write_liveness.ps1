param(
    [int]$FileMiB = 256,
    [ValidateRange(1, 3600)]
    [int]$WriteProgressTimeoutSeconds = 2,
    [int]$ProgressDurationSeconds = 6,
    [int]$ProgressReadBytes = 4096,
    [int]$ProgressIntervalMilliseconds = 100,
    [string]$Executable = '.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe'
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Net.Sockets;
using System.Text;
using System.Threading;

public sealed class ResponseLivenessClient : IDisposable
{
    private readonly TcpClient client;
    private readonly NetworkStream stream;

    public ResponseLivenessClient(string host, int port)
    {
        client = new TcpClient();
        client.ReceiveBufferSize = 512;
        client.ReceiveTimeout = 10000;
        client.SendTimeout = 10000;
        client.Connect(host, port);
        stream = client.GetStream();
    }

    public void Request(string path, bool close)
    {
        byte[] request = Encoding.ASCII.GetBytes(
            "GET " + path + " HTTP/1.1\r\nHost: 127.0.0.1\r\n" +
            (close ? "Connection: close\r\n" : "") + "\r\n");
        stream.Write(request, 0, request.Length);
    }

    public string ReadHead()
    {
        using (var bytes = new MemoryStream())
        {
            int matched = 0;
            byte[] delimiter = new byte[] { 13, 10, 13, 10 };
            while (matched < delimiter.Length)
            {
                int value = stream.ReadByte();
                if (value < 0) throw new EndOfStreamException("closed before response head");
                bytes.WriteByte((byte)value);
                matched = value == delimiter[matched] ? matched + 1 : (value == delimiter[0] ? 1 : 0);
            }
            return Encoding.ASCII.GetString(bytes.ToArray());
        }
    }

    public long ReadSlowly(TimeSpan duration, int bytesPerRead, int intervalMilliseconds)
    {
        byte[] buffer = new byte[bytesPerRead];
        long total = 0;
        var elapsed = Stopwatch.StartNew();
        while (elapsed.Elapsed < duration)
        {
            Thread.Sleep(intervalMilliseconds);
            int read = stream.Read(buffer, 0, buffer.Length);
            if (read == 0) throw new EndOfStreamException("server closed a progressing response");
            total += read;
        }
        return total;
    }

    public string ReadToEnd()
    {
        using (var bytes = new MemoryStream())
        {
            byte[] buffer = new byte[4096];
            int read;
            while ((read = stream.Read(buffer, 0, buffer.Length)) != 0)
                bytes.Write(buffer, 0, read);
            return Encoding.ASCII.GetString(bytes.ToArray());
        }
    }

    public void Dispose()
    {
        stream.Dispose();
        client.Dispose();
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

function Start-ProbeServer([string]$Name) {
    $port = Get-FreePort
    $configPath = Join-Path $runRoot "$Name.toml"
    $stdoutPath = Join-Path $runRoot "$Name.stdout.txt"
    $stderrPath = Join-Path $runRoot "$Name.stderr.txt"
    $tomlRoot = $documentRoot.Replace('\', '\\')
    $config = @"
[server]
listen = "127.0.0.1:$port"
max_connections = 1
header_read_timeout = 30
write_progress_timeout = $WriteProgressTimeoutSeconds

[[hosts]]
name = "127.0.0.1"

[[hosts.locations]]
path = "/"
handler = "static"
root = "$tomlRoot"
"@
    [IO.File]::WriteAllText($configPath, $config)
    $process = Start-Process -FilePath $executablePath -ArgumentList @('--config', $configPath) `
        -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath `
        -WindowStyle Hidden -PassThru
    Wait-Ready $port $process
    return [PSCustomObject]@{
        Port = $port
        Process = $process
        StderrPath = $stderrPath
    }
}

function Stop-ProbeServer($Server) {
    if (-not $Server.Process.HasExited) {
        Stop-Process -Id $Server.Process.Id
        Wait-Process -Id $Server.Process.Id -ErrorAction SilentlyContinue
    }
    $Server.Process.Dispose()
}

$executablePath = (Resolve-Path -LiteralPath $Executable).Path
$runRoot = Join-Path ([IO.Path]::GetTempPath()) ("hamster-response-liveness-" + [Guid]::NewGuid().ToString('N'))
$documentRoot = Join-Path $runRoot 'public'
[IO.Directory]::CreateDirectory($documentRoot) | Out-Null
$largeFile = Join-Path $documentRoot 'large.bin'
$fixture = [IO.File]::Open($largeFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
try { $fixture.SetLength([int64]$FileMiB * 1MB) }
finally { $fixture.Dispose() }
[IO.File]::WriteAllText((Join-Path $documentRoot 'healthy.txt'), 'healthy')

try {
    $server = Start-ProbeServer 'zero-progress'
    $stalled = $null
    $waiting = $null
    try {
        $stalled = [ResponseLivenessClient]::new('127.0.0.1', $server.Port)
        $stalled.Request('/large.bin', $false)
        $head = $stalled.ReadHead()
        if (-not $head.StartsWith('HTTP/1.1 200')) { throw "unexpected stalled response: $head" }

        $waiting = [ResponseLivenessClient]::new('127.0.0.1', $server.Port)
        $waiting.Request('/healthy.txt', $true)
        $elapsed = [Diagnostics.Stopwatch]::StartNew()
        $response = $waiting.ReadToEnd()
        $elapsed.Stop()
        if (-not $response.StartsWith('HTTP/1.1 200') -or -not $response.EndsWith('healthy')) {
            throw "waiting connection did not recover: $response"
        }
        Start-Sleep -Milliseconds 100
        $stderr = ''
        if (Test-Path -LiteralPath $server.StderrPath) {
            $stderrFile = [IO.File]::Open(
                $server.StderrPath,
                [IO.FileMode]::Open,
                [IO.FileAccess]::Read,
                [IO.FileShare]::ReadWrite)
            try {
                $stderrReader = [IO.StreamReader]::new($stderrFile)
                try { $stderr = $stderrReader.ReadToEnd() }
                finally { $stderrReader.Dispose() }
            }
            finally { $stderrFile.Dispose() }
        }
        Write-Output ("CASE=zero_progress TIMEOUT_S={0} WAITING_RESPONSE_MS={1:F1} PEER_CONTROLLED_STDERR_BYTES={2}" -f
            $WriteProgressTimeoutSeconds,
            $elapsed.Elapsed.TotalMilliseconds,
            [Text.Encoding]::UTF8.GetByteCount($stderr))
    }
    finally {
        if ($null -ne $waiting) { $waiting.Dispose() }
        if ($null -ne $stalled) { $stalled.Dispose() }
        Stop-ProbeServer $server
    }

    $server = Start-ProbeServer 'slow-progress'
    $slow = $null
    try {
        $slow = [ResponseLivenessClient]::new('127.0.0.1', $server.Port)
        $slow.Request('/large.bin', $false)
        $head = $slow.ReadHead()
        if (-not $head.StartsWith('HTTP/1.1 200')) { throw "unexpected slow response: $head" }
        $read = $slow.ReadSlowly(
            [TimeSpan]::FromSeconds($ProgressDurationSeconds),
            $ProgressReadBytes,
            $ProgressIntervalMilliseconds)
        Write-Output ("CASE=slow_progress TIMEOUT_S={0} OBSERVED_S={1} BYTES_READ={2} READ_BYTES_PER_S={3:F1}" -f
            $WriteProgressTimeoutSeconds,
            $ProgressDurationSeconds,
            $read,
            ($read / $ProgressDurationSeconds))
    }
    finally {
        if ($null -ne $slow) { $slow.Dispose() }
        Stop-ProbeServer $server
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
