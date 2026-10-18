# Scratch-only qualification. No elevation prompt, firewall change or reboot.
[CmdletBinding()]
param(
    [ValidateSet('Prepare', 'Process', 'Service')][string]$Mode = 'Service',
    [string]$OpenSsl,
    [switch]$KeepFixture,
    # Unmodified vendor baseline: reproduce gaps, record them, then clean up.
    [switch]$LifecycleBaseline,
    # Experimental pinned integration, must be qualified as an actual Service.
    [switch]$LifecycleExtension
)
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($Mode -eq 'Service' -and -not $isAdmin) {
    Write-Error 'NOT VERIFIED: Service mode needs PowerShell Run as administrator. No Service has been installed. Use -Mode Prepare or -Mode Process without elevation.' -ErrorAction Continue
    exit 3
}
if ($LifecycleBaseline -and $LifecycleExtension) { throw 'Select baseline OR lifecycle extension, not both.' }
if ($Mode -ne 'Service' -and ($LifecycleBaseline -or $LifecycleExtension)) { throw 'Lifecycle comparison requires an actual SCM Service.' }
$stamp = [Guid]::NewGuid().ToString('N')
$base = Join-Path $repo 'target\windows-service-qualification'
$root = Join-Path $base $stamp
$name = 'HamsterQual_' + $stamp.Substring(0, 12)
$probeName = $name + '_Control'
$reportPath = Join-Path $base ($stamp + '-results.json')
$rows = [ordered]@{}
$events = [Collections.Generic.List[object]]::new()
$owned = [Collections.Generic.List[object]]::new()
$registered = @{}
$main = $null
$mainWrapper = $null
$cgi = $null
$other = $null
$failed = $false
$cleanupFailed = $false
$config = Join-Path $root 'hamster.toml'
$state = Join-Path $root 'state'
$logs = Join-Path $root 'logs'
$binary = Join-Path $root 'hamster-wheel.exe'
$php = Join-Path $root 'php\php.exe'
$wrapper = Join-Path $root ($name + '.exe')
$probeWrapper = Join-Path $root ($probeName + '.exe')
$stopClock = $null
$originalConfig = $null

function Save-Report {
    $report = [ordered]@{ Scope = $Mode; LifecycleBaseline = [bool]$LifecycleBaseline; LifecycleExtension = [bool]$LifecycleExtension; Administrator = $isAdmin; Service = $name; ProbeService = $probeName; Root = $root; Results = $rows; Evidence = $events; AtUtc = [DateTime]::UtcNow.ToString('o') }
    [IO.File]::WriteAllText($reportPath, ($report | ConvertTo-Json -Depth 8))
}
function Record([string]$Key, [string]$Status, [string]$Detail) {
    $rows[$Key] = @{ Status = $Status; Detail = $Detail }
    Write-Host "$Key : $Status : $Detail"
    Save-Report
}
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Wait-Until([string]$Description, [scriptblock]$Condition, [int]$Seconds = 30) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while (-not (& $Condition)) {
        if ($clock.Elapsed.TotalSeconds -ge $Seconds) { throw "Deadline waiting for $Description" }
        Start-Sleep -Milliseconds 50
    }
}
function Run-Tool([string]$File, [string[]]$Arguments) {
    # ProcessStartInfo preserves empty arguments (sc.exe actions="") and avoids
    # Windows PowerShell interpreting successful native stderr as an exception.
    $quoted = foreach ($argument in $Arguments) {
        $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
        '"' + $escaped + '"'
    }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $File; $info.Arguments = $quoted -join ' '
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $started = $false
    try {
        $started = $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(30000)) { throw "$File exceeded its 30-second tool deadline" }
        $out = $stdout.Result + $stderr.Result
        if ($process.ExitCode -ne 0) { throw "$File failed ($($process.ExitCode)): $out" }
        return $out
    } finally {
        if ($started -and -not $process.HasExited) { $process.Kill(); $null = $process.WaitForExit(5000) }
        $process.Dispose()
    }
}
function Escape-Xml([string]$Value) { [Security.SecurityElement]::Escape($Value) }
function New-Port {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $port
}
function Start-Owned([string]$File, [string]$Arguments, [string]$Label) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $File; $info.Arguments = $Arguments
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    if ($Label -eq 'cgi') { $info.EnvironmentVariables['PHP_FCGI_MAX_REQUESTS'] = '0' }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $null = $process.Start(); $null = $process.Handle
    $entry = @{ Process = $process; Out = $process.StandardOutput.ReadToEndAsync(); Err = $process.StandardError.ReadToEndAsync(); Label = $Label }
    $owned.Add($entry)
    return $process
}
function Service-Record([string]$Id) {
    Get-CimInstance Win32_Service -Filter "Name = '$Id'" -OperationTimeoutSec 3
}
function Assert-OwnService([string]$Id, [string]$Exe) {
    $service = Service-Record $Id
    Require ($null -ne $service) "Missing owned Service $Id"
    Require ($service.PathName.Trim('"') -ieq $Exe) "Service $Id has a different executable; refusing modification"
    return $service
}
function Service-State([string]$Id) { (Service-Record $Id).State }
function Control([string]$Command) {
    if ($Mode -eq 'Service') {
        Wait-Until 'finite control probe to stop' { (Service-State $probeName) -eq 'Stopped' }
        $resultPath = Join-Path $state 'control-result.json'
        if (Test-Path -LiteralPath $resultPath) { Remove-Item -LiteralPath $resultPath }
        [IO.File]::WriteAllText((Join-Path $state 'control-request.json'), (@{ Command = $Command } | ConvertTo-Json))
        (Get-Service -Name $probeName).Start()
        Wait-Until 'control probe response' { Test-Path -LiteralPath $resultPath }
        $answer = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
        Require ($answer.User -eq 'S-1-5-19') "Probe did not run as LocalService: $($answer | ConvertTo-Json -Compress)"
        $events.Add(@{ Control = $Command; User = $answer.User; Owner = $answer.Owner; Account = $answer.Account; Exit = $answer.Exit; Output = $answer.Output; Error = $answer.Error })
        # Stop only after the finite exchange completed. This exercises the SCM
        # path without relying on WinSW 2.11's privileged exit-0 SignalStopped.
        (Get-Service -Name $probeName).Stop()
        Wait-Until 'SCM control probe stopped' { (Service-State $probeName) -eq 'Stopped' }
        return $answer
    }
    $client = Start-Owned $binary ('--config "{0}" --control {1}' -f $config, $Command) ('control-' + $Command)
    Require ($client.WaitForExit(7000)) 'Control client exceeded its deadline'
    $entry = $owned[$owned.Count - 1]
    return [pscustomobject]@{ Exit = $client.ExitCode; Output = $entry.Out.Result; Error = $entry.Err.Result }
}
function Find-Main {
    if ($Mode -ne 'Service') { return $script:main }
    $service = Assert-OwnService $name $wrapper
    if ($service.ProcessId -eq 0) { return $null }
    $owner = [Diagnostics.Process]::GetProcessById($service.ProcessId)
    $retained = $false
    try {
      $null = $owner.Handle
      Require ($owner.MainModule.FileName -ieq $wrapper) 'SCM wrapper identity mismatch'
      foreach ($record in @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($owner.Id)" -OperationTimeoutSec 3)) {
        $candidate = $null
        try {
            $candidate = [Diagnostics.Process]::GetProcessById($record.ProcessId)
            $null = $candidate.Handle
            if ($candidate.HasExited -or $candidate.MainModule.FileName -ine $binary) { continue }
            $fresh = Get-CimInstance Win32_Process -Filter "ProcessId = $($candidate.Id)" -OperationTimeoutSec 3
            $argv = [HamsterQualificationNative]::Arguments($fresh.CommandLine)
            if ($argv.Length -ne 3 -or $argv[1] -cne '--config' -or $argv[2] -ine $config -or $fresh.ParentProcessId -ne $owner.Id -or $candidate.StartTime -lt $owner.StartTime) { continue }
            $script:mainWrapper = $owner
            $owned.Add(@{ Process = $owner; Label = 'SCM-wrapper' })
            $retained = $true
            $owned.Add(@{ Process = $candidate; Label = 'SCM-Hamster' })
            $events.Add(@{ MainPid = $candidate.Id; CreatedUtc = $candidate.StartTime.ToUniversalTime().ToString('o'); WrapperPid = $owner.Id; Image = $candidate.MainModule.FileName })
            return $candidate
        } finally {
            if ($candidate -and -not ($owned | Where-Object { $_.Process -eq $candidate })) { $candidate.Dispose() }
        }
      }
    } finally { if (-not $retained) { $owner.Dispose() } }
    return $null
}
function Start-Main {
    if ($Mode -eq 'Service') {
        $null = Assert-OwnService $name $wrapper
        (Get-Service -Name $name).Start()
        Wait-Until 'SCM RUNNING' { (Service-State $name) -eq 'Running' }
        Wait-Until 'owned Hamster child' { $script:main = Find-Main; $null -ne $script:main }
    } else {
        $script:main = Start-Owned $binary ('--config "{0}"' -f $config) 'process-Hamster'
        $events.Add(@{ MainPid = $main.Id; CreatedUtc = $main.StartTime.ToUniversalTime().ToString('o'); Image = $main.MainModule.FileName })
    }
    Wait-Until 'HTTP/application readiness' {
        try { (Http '/ready.txt') -match 'qualification-ready' } catch { $false }
    }
}
function Http([string]$Path, [string]$Pin = '') {
    $port = if ($Pin) { $script:httpsPort } else { $script:httpPort }
    [HamsterQualificationNative]::Get($port, $Path, $Pin)
}
function Capture-Workers {
    $captured = [Collections.Generic.List[Diagnostics.Process]]::new()
    foreach ($file in @(Get-ChildItem -LiteralPath $state -Filter 'worker-*.json')) {
        $pids = Get-Content -LiteralPath $file.FullName -Raw | ConvertFrom-Json
        foreach ($role in @('worker', 'descendant')) {
            $process = $null
            try {
                $process = [Diagnostics.Process]::GetProcessById($pids.$role)
                $null = $process.Handle
                if ($process.HasExited -or $process.StartTime -lt $main.StartTime -or $process.MainModule.FileName -ine $php) { continue }
                $record = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.Id)" -OperationTimeoutSec 3
                $parent = if ($role -eq 'worker') { $main.Id } else { $pids.worker }
                Require ($record.ParentProcessId -eq $parent) 'Worker/descendant ownership changed'
                $argv = [HamsterQualificationNative]::Arguments($record.CommandLine)
                $fixture = Join-Path $root 'fixtures\service_qualification.php'
                Require ($argv.Length -ge 2 -and $argv[1] -ieq $fixture) 'Unexpected worker/descendant argv'
                if ($role -eq 'worker') { Require ($argv.Length -eq 2) 'Unexpected worker argv shape' }
                else { Require ($argv.Length -eq 5 -and $argv[2] -ceq 'descendant' -and [IO.Path]::GetFullPath($argv[3]) -ieq $state -and $argv[4] -eq [string]$pids.worker) 'Unexpected descendant argv shape' }
                $owned.Add(@{ Process = $process; Label = $role })
                $events.Add(@{ Role = $role; Pid = $process.Id; CreatedUtc = $process.StartTime.ToUniversalTime().ToString('o'); ParentPid = $parent; Image = $process.MainModule.FileName })
                $captured.Add($process)
            } catch [ArgumentException] {
                # A prior generation's scratch marker is not a live identity.
            } finally {
                if ($process -and -not $captured.Contains($process)) { $process.Dispose() }
            }
        }
    }
    Require ($captured.Count -ge 2) 'Could not retain worker and descendant handles'
    return ,$captured
}
function Begin-Barrier([string]$Id) {
    $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $httpPort)
    try {
        $bytes = [Text.Encoding]::ASCII.GetBytes("GET /app/wait?id=$Id HTTP/1.1`r`nHost: localhost`r`nConnection: close`r`n`r`n")
        $client.GetStream().Write($bytes, 0, $bytes.Length)
        Wait-Until 'PHP transaction started' { Test-Path -LiteralPath (Join-Path $state "request-$Id-started") }
    } finally { $client.Dispose() }
}
function Begin-Stop {
    $lastStop = Join-Path $logs 'last-stop.log'
    if (Test-Path -LiteralPath $lastStop) { Remove-Item -LiteralPath $lastStop }
    if ($Mode -eq 'Service') {
        $null = Assert-OwnService $name $wrapper
        (Get-Service -Name $name).Stop()
        return $null
    }
    $ownerExe = [Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    return Start-Owned "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" ('-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}" -Executable "{1}" -Config "{2}" -OwnerExecutable "{3}" -GraceSeconds 12 -EvidencePath "{4}"' -f (Join-Path $root 'Stop-Hamster.ps1'), $binary, $config, $ownerExe, (Join-Path $logs 'last-stop.log')) 'stop-helper'
}
function Wait-Stopped {
    Wait-Until 'actual main exit / SCM STOPPED' {
        $exited = $main.HasExited
        if ($Mode -eq 'Service') {
            $stopped = (Service-State $name) -eq 'Stopped'
            Require (-not $stopped -or $exited) 'SCM reported STOPPED while retained Hamster handle was still live'
            return $stopped -and $exited
        }
        return $exited
    } 35
}

function Assert-Isolation {
    Require (-not $cgi.HasExited -and -not $other.HasExited) 'Other instance/operator endpoint was terminated'
    Require (([HamsterQualificationNative]::Get($otherPort, '/cgi/index.php', '')) -match 'qualification-cgi=cgi-fcgi') 'Independent PHP endpoint lost service'
}
function Wait-Recovered([Diagnostics.Process]$Previous) {
    Wait-Until 'SCM replacement main identity' {
        try {
            $next = Find-Main
            if ($next -and $next.StartTime -gt $Previous.StartTime) { $script:main = $next; return $true }
        } catch { }
        $false
    } 45
    Wait-Until 'recovered HTTP/PHP' { try { (Http '/app/') -match 'HTTP/1.1 200' } catch { $false } }
    Require ((Http '/ready.txt' $pin2) -match 'qualification-ready') 'Recovered HTTPS failed'
    Require ((Http '/cgi/index.php') -match 'qualification-cgi=cgi-fcgi') 'Recovered FastCGI failed'
    # Snapshot is diagnostic only; no termination authority comes from it.
    $matches = @(Get-CimInstance Win32_Process -Filter "Name = 'hamster-wheel.exe'" -OperationTimeoutSec 3 | Where-Object {
        $args = [HamsterQualificationNative]::Arguments($_.CommandLine)
        $args.Length -eq 3 -and $args[1] -ceq '--config' -and $args[2] -ieq $config
    })
    Require ($matches.Count -eq 1 -and $matches[0].ProcessId -eq $main.Id) 'Duplicate scratch main after recovery'
    Assert-Isolation
}
function Retain-StopHelper {
    foreach ($record in @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($mainWrapper.Id)" -OperationTimeoutSec 3)) {
        $args = [HamsterQualificationNative]::Arguments($record.CommandLine)
        if (-not ($args -contains (Join-Path $root 'Stop-Hamster.ps1'))) { continue }
        $process = [Diagnostics.Process]::GetProcessById($record.ProcessId)
        $null = $process.Handle
        Require (-not $process.HasExited -and $process.StartTime -ge $mainWrapper.StartTime) 'Stop helper identity expired'
        Require ($process.MainModule.FileName -ieq "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe") 'Unexpected stop helper image'
        $fresh = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.Id)" -OperationTimeoutSec 3
        Require ($fresh.ParentProcessId -eq $mainWrapper.Id -and $fresh.CommandLine -ceq $record.CommandLine) 'Stop helper identity changed'
        $owned.Add(@{ Process = $process; Label = 'SCM-stop-helper' })
        return $process
    }
    throw 'Could not retain exact stop helper'
}
function Test-WrapperCrash([string]$Case) {
    $previous = $main; $previousWrapper = $mainWrapper
    $children = Capture-Workers
    $stopHelper = $null
    if ($Case -in @('busy', 'stopping')) { Begin-Barrier ('wrapper' + $Case) }
    if ($Case -eq 'stopping') {
        $null = Begin-Stop
        Wait-Until 'busy stop acknowledged' {
            $path = Join-Path $logs 'last-stop.log'
            (Test-Path -LiteralPath $path) -and [IO.File]::ReadAllText($path).Contains('acknowledged=True')
        }
        Require ((Service-State $name) -eq 'Stop Pending' -and -not $main.HasExited) 'Not actually stopping a busy main'
        $stopHelper = Retain-StopHelper
    }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    [HamsterQualificationNative]::Kill($previousWrapper)
    Require ($previousWrapper.WaitForExit(5000)) 'Wrapper kill did not signal retained handle'
    Require ($previous.WaitForExit(5000)) 'Wrapper crash orphaned Hamster'
    foreach ($child in $children) { Require ($child.WaitForExit(5000)) 'Wrapper crash orphaned worker/descendant' }
    if ($stopHelper) { Require ($stopHelper.WaitForExit(5000)) 'Wrapper crash orphaned stop helper' }
    $reclaim = $clock.ElapsedMilliseconds
    Wait-Recovered $previous
    $events.Add(@{ WrapperCrashCase = $Case; WrapperPid = $previousWrapper.Id; WrapperCreatedUtc = $previousWrapper.StartTime.ToUniversalTime().ToString('o'); MainPid = $previous.Id; Children = @($children | ForEach-Object { $_.Id }); StopHelperPid = $(if ($stopHelper) { $stopHelper.Id } else { $null }); Mechanism = 'TerminateProcess retained wrapper handle'; ReclaimMs = $reclaim; RecoveryMs = $clock.ElapsedMilliseconds; NewMainPid = $main.Id; NewWrapperPid = $mainWrapper.Id; ScmState = (Service-State $name) })
    Record ('WrapperCrash_' + $Case) 'PASS' 'Retained old process tree exits; automatic SCM recovery serves PHP/TLS/CGI, singleton and independent endpoint verified'
}

try {
    $null = New-Item -ItemType Directory -Path $root -Force
    foreach ($directory in @('state', 'logs', 'public', 'cgi', 'certs', 'fixtures')) { $null = New-Item -ItemType Directory -Path (Join-Path $root $directory) }
    Record 'Administrator' $(if ($isAdmin) { 'PASS' } else { 'NOT VERIFIED' }) "account=$($identity.Name); mode=$Mode"
    foreach ($gate in @('Installation', 'ServiceAccountControl', 'CrossAccountACL', 'CrashRecovery', 'LogRotation', 'WrapperCrash')) {
        $rows[$gate] = @{ Status = 'NOT VERIFIED'; Detail = 'Not executed yet; process-only results do not qualify SCM or service-account behavior' }
    }
    $cache = Join-Path $repo 'target\tools\winsw-2.11.0\WinSW.NET461.exe'
    $expectedHash = '31DCA0D9BD3C5AB7E119172BDB927B90C9DA935D0F4963335EAF60E7D8F7AABF'
    if (-not (Test-Path -LiteralPath $cache)) {
        $null = New-Item -ItemType Directory -Path (Split-Path $cache) -Force
        Invoke-WebRequest 'https://github.com/winsw/winsw/releases/download/v2.11.0/WinSW.NET461.exe' -UseBasicParsing -OutFile $cache -TimeoutSec 60
    }
    Require ((Get-FileHash -LiteralPath $cache -Algorithm SHA256).Hash -eq $expectedHash) 'WinSW release hash mismatch'
    Require ((Get-Item -LiteralPath $cache).VersionInfo.FileVersion -eq '2.11.0.0') 'Unexpected WinSW version'
    $events.Add(@{ WinSW = '2.11.0.NET461'; Sha256 = $expectedHash; Source = 'https://github.com/winsw/winsw/releases/download/v2.11.0/WinSW.NET461.exe' })
    Copy-Item -LiteralPath $cache -Destination $wrapper
    Copy-Item -LiteralPath $cache -Destination $probeWrapper
    if ($LifecycleExtension) {
        $hash = & (Join-Path $PSScriptRoot 'Build-LifecycleExtension.ps1') -WinSW $cache -OutputDirectory $root
        $events.Add(@{ LifecycleExtensionSha256 = $hash.Hash })
    }
    Copy-Item -LiteralPath (Join-Path $repo 'target\x86_64-pc-windows-msvc\release\hamster-wheel.exe') -Destination $binary
    Copy-Item -LiteralPath (Join-Path $repo 'php\8.5') -Destination (Join-Path $root 'php') -Recurse
    foreach ($file in @('Stop-Hamster.ps1', 'Service-ControlProbe.ps1')) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination $root }
    foreach ($file in @('protocol.php', 'service_qualification.php')) { Copy-Item -LiteralPath (Join-Path $repo "fixtures\php-worker\$file") -Destination (Join-Path $root 'fixtures') }
    [IO.File]::WriteAllText((Join-Path $root 'public\ready.txt'), 'qualification-ready')
    # Location matching does not strip /cgi from the application request URI.
    $null = New-Item -ItemType Directory -Path (Join-Path $root 'cgi\cgi')
    [IO.File]::WriteAllText((Join-Path $root 'cgi\cgi\index.php'), '<?php header("Content-Type: text/plain"); echo "qualification-cgi=" . PHP_SAPI;')
    if (-not $OpenSsl) { $OpenSsl = 'C:\Program Files\Git\usr\bin\openssl.exe' }
    $OpenSsl = (Resolve-Path -LiteralPath $OpenSsl).ProviderPath
    $certs = Join-Path $root 'certs'
    $ca = Join-Path $certs 'ca.pem'; $caKey = Join-Path $certs 'ca.key'
    $null = Run-Tool $OpenSsl @('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '2', '-subj', '/CN=Hamster Qualification CA', '-keyout', $caKey, '-out', $ca, '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign')
    $extensions = Join-Path $certs 'leaf.ext'
    [IO.File]::WriteAllText($extensions, "basicConstraints=critical,CA:FALSE`nkeyUsage=critical,digitalSignature,keyEncipherment`nextendedKeyUsage=serverAuth`nsubjectAltName=DNS:localhost,IP:127.0.0.1`n")
    foreach ($number in 1..2) {
        $key = Join-Path $certs "leaf$number.key"; $csr = Join-Path $certs "leaf$number.csr"; $leaf = Join-Path $certs "leaf$number.pem"
        $null = Run-Tool $OpenSsl @('req', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=localhost', '-keyout', $key, '-out', $csr)
        $null = Run-Tool $OpenSsl @('x509', '-req', '-in', $csr, '-CA', $ca, '-CAkey', $caKey, '-CAcreateserial', '-days', '1', '-extfile', $extensions, '-out', $leaf)
        $null = Run-Tool $OpenSsl @('verify', '-CAfile', $ca, $leaf)
        $null = Run-Tool $OpenSsl @('x509', '-in', $leaf, '-outform', 'DER', '-out', (Join-Path $certs "leaf$number.der"))
        [IO.File]::WriteAllText((Join-Path $certs "chain$number.pem"), ([IO.File]::ReadAllText($leaf) + [IO.File]::ReadAllText($ca)))
    }
    Copy-Item -LiteralPath (Join-Path $certs 'chain1.pem') -Destination (Join-Path $certs 'active.pem')
    Copy-Item -LiteralPath (Join-Path $certs 'leaf1.key') -Destination (Join-Path $certs 'active.key')
    $pin1 = (Get-FileHash -LiteralPath (Join-Path $certs 'leaf1.der') -Algorithm SHA256).Hash
    $pin2 = (Get-FileHash -LiteralPath (Join-Path $certs 'leaf2.der') -Algorithm SHA256).Hash
    $script:httpPort = New-Port; $script:httpsPort = New-Port; $cgiPort = New-Port; $otherPort = New-Port
    Require ((@($httpPort, $httpsPort, $cgiPort, $otherPort) | Select-Object -Unique).Count -eq 4) 'Port reservation collision; retry preparation'
    $originalConfig = @"
[server]
listen = '127.0.0.1:$httpPort'
[server.control]
name = '$name'
[server.tls]
listen = '127.0.0.1:$httpsPort'
[[server.tls.certificates]]
cert = 'certs/active.pem'
key = 'certs/active.key'
names = ['localhost']
default = true
[[hosts]]
name = 'localhost'
[[hosts.locations]]
path = '/'
handler = 'static'
root = 'public'
[[hosts.locations]]
path = '/app'
handler = 'php_worker'
[hosts.locations.php]
executable = 'php/php.exe'
script = 'fixtures/service_qualification.php'
workers = 1
queue_capacity = 8
startup_timeout = 10
request_timeout = 60
[hosts.locations.php.env]
HAMSTER_SERVICE_TEST_STATE = '$($state.Replace('\', '/'))'
[[hosts.locations]]
path = '/cgi'
handler = 'fastcgi'
root = 'cgi'
[hosts.locations.fastcgi]
servers = ['127.0.0.1:$cgiPort']
concurrency = 1
"@
    [IO.File]::WriteAllText($config, $originalConfig)
    $powerShell = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    [xml]$xml = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'HamsterWheelService.xml')
    $xml.service.id = $name; $xml.service.name = $name
    $xml.service.startmode = 'Automatic'
    $xml.service.stoparguments = $xml.service.stoparguments.Replace('HamsterWheelService.exe', "$name.exe").Replace('-GraceSeconds 120', '-GraceSeconds 12')
    $xml.service.log.sizeThreshold = '64'; $xml.service.onfailure.delay = '3 sec'
    # Keep vendor-baseline/process modes genuinely unmodified. The qualified
    # sample now includes the shim; do not append a duplicate extension entry.
    if ($xml.service.extensions) { $null = $xml.service.RemoveChild($xml.service.extensions) }
    if ($LifecycleExtension) {
        $extensions = $xml.CreateElement('extensions')
        $extension = $xml.CreateElement('extension')
        $extension.SetAttribute('id', 'hamster-lifecycle'); $extension.SetAttribute('enabled', 'true')
        $extension.SetAttribute('className', 'Hamster.WinSW.LifecycleExtension, HamsterLifecycleExtension')
        $null = $extensions.AppendChild($extension); $null = $xml.service.AppendChild($extensions)
    }
    $xml.Save((Join-Path $root "$name.xml"))
    $probeXml = @"
<service><id>$probeName</id><name>$probeName</name><description>Finite scratch control-account qualification probe</description><startmode>Manual</startmode><stoptimeout>2 sec</stoptimeout><serviceaccount><domain>NT AUTHORITY</domain><user>LocalService</user></serviceaccount><executable>$(Escape-Xml $powerShell)</executable><arguments>-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%BASE%\Service-ControlProbe.ps1" -Deployment "%BASE%"</arguments><logpath>%BASE%\logs</logpath><log mode="roll-by-size"><sizeThreshold>64</sizeThreshold><keepFiles>8</keepFiles></log></service>
"@
    [IO.File]::WriteAllText((Join-Path $root "$probeName.xml"), $probeXml)
    foreach ($xmlPath in @((Join-Path $root "$name.xml"), (Join-Path $root "$probeName.xml"))) {
        [xml]$stagedXml = Get-Content -LiteralPath $xmlPath
        foreach ($required in @('id', 'name', 'description', 'executable')) {
            Require (-not [string]::IsNullOrWhiteSpace([string]$stagedXml.service.$required)) "Missing WinSW <$required> in $xmlPath"
        }
    }
    $null = Run-Tool $wrapper @('version')
    $null = Run-Tool $php @('-l', (Join-Path $root 'fixtures\service_qualification.php'))
    $events.Add(@{ HttpPort = $httpPort; HttpsPort = $httpsPort; CgiPort = $cgiPort; OtherPort = $otherPort })
    Record 'Preparation' 'PASS' 'Pinned WinSW, release/icon binary, real PHP copy, scratch PKI, unique IDs/loopback ports and XML prepared'
    Record 'BootStartup' 'NOT VERIFIED' 'XML Automatic prepared; SCM configuration and real reboot not yet verified; this script never reboots'
    if ($Mode -eq 'Prepare') { return }

    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Net.Security;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
public static class HamsterQualificationNative {
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr CommandLineToArgvW(string command, out int count);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr value);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateProcess(IntPtr process, uint code);
    public static string[] Arguments(string command) {
        int count; IntPtr memory = CommandLineToArgvW(command, out count);
        if (memory == IntPtr.Zero) throw new Win32Exception();
        try { var result = new string[count]; for(int i=0;i<count;i++) result[i]=Marshal.PtrToStringUni(Marshal.ReadIntPtr(memory,i*IntPtr.Size)); return result; }
        finally { LocalFree(memory); }
    }
    public static void Kill(Process process) { if (!TerminateProcess(process.Handle,1)) throw new Win32Exception(); }
    public static string Get(int port, string path, string pin) {
        using(var client = new TcpClient()) {
            var connect = client.ConnectAsync("127.0.0.1", port);
            if (!connect.Wait(3000)) throw new TimeoutException("TCP connect");
            Stream stream = client.GetStream(); stream.ReadTimeout=15000; stream.WriteTimeout=5000;
            if (!String.IsNullOrEmpty(pin)) {
                var tls = new SslStream(stream,false,(sender,cert,chain,errors) => {
                    if (cert == null || (errors & SslPolicyErrors.RemoteCertificateNameMismatch) != 0) return false;
                    using(var hash=SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(cert.GetRawCertData())).Replace("-","")==pin;
                });
                tls.ReadTimeout=15000; tls.WriteTimeout=5000;
                tls.AuthenticateAsClient("localhost"); stream=tls;
            }
            using(stream) {
                var request=Encoding.ASCII.GetBytes("GET "+path+" HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
                stream.Write(request,0,request.Length);
                using(var result=new MemoryStream()) {
                    var buffer=new byte[4096]; int count;
                    while((count=stream.Read(buffer,0,buffer.Length))>0) {
                        result.Write(buffer,0,count); if(result.Length>65536) throw new Exception("Unexpected large fixture response");
                    }
                    return Encoding.UTF8.GetString(result.ToArray());
                }
            }
        }
    }
}
'@
    if ($Mode -eq 'Service') {
        # Only new scratch ACLs are changed, never repository/runtime originals.
        $acl = Get-Acl -LiteralPath $root
        # The checkout may inherit writable Users/sandbox ACLs. Do not inherit
        # those into a Service deployment containing executable stop policy.
        $acl.SetAccessRuleProtection($true, $false)
        foreach ($sid in @($identity.User.Value, 'S-1-5-18', 'S-1-5-32-544')) {
            $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
        }
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-19'), 'ReadAndExecute', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
        Set-Acl -LiteralPath $root -AclObject $acl
        foreach ($directory in @($state, $logs)) {
            $acl = Get-Acl -LiteralPath $directory
            $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-19'), 'Modify', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
            Set-Acl -LiteralPath $directory -AclObject $acl
        }
        foreach ($deployment in @(@{ Id = $name; Exe = $wrapper }, @{ Id = $probeName; Exe = $probeWrapper })) {
            Require ($null -eq (Service-Record $deployment.Id)) "Service name collision: $($deployment.Id)"
            try { $null = Run-Tool $deployment.Exe @('install') }
            finally {
                $service = Service-Record $deployment.Id
                if ($service -and $service.PathName.Trim('"') -ieq $deployment.Exe) { $registered[$deployment.Id] = $deployment.Exe }
            }
            $service = Assert-OwnService $deployment.Id $deployment.Exe
            Require ($service.StartName -ieq 'NT AUTHORITY\LocalService') 'Unexpected Service account'
        }
        $service = Assert-OwnService $name $wrapper
        Require ($service.StartMode -eq 'Auto') 'Automatic startup not configured in SCM'
        $events.Add(@{ ScQc = (Run-Tool "$env:SystemRoot\System32\sc.exe" @('qc', $name)); ScFailure = (Run-Tool "$env:SystemRoot\System32\sc.exe" @('qfailure', $name)) })
        Record 'Installation' 'PASS' 'Only the two unique qualification Services installed; LocalService and SCM Auto verified'
        Record 'BootStartup' 'CONFIGURED, NOT VERIFIED' 'SCM StartMode=Auto; no reboot authorized or performed'
    } else { Record 'ServiceQualification' 'NOT VERIFIED' 'Process-mode smoke only; no SCM installation, service token or recovery tested' }

    $cgi = Start-Owned (Join-Path $root 'php\php-cgi.exe') "-b 127.0.0.1:$cgiPort -d display_errors=0" 'cgi'
    $otherConfig = Join-Path $root 'other.toml'
    [IO.File]::WriteAllText($otherConfig, "[server]`nlisten = '127.0.0.1:$otherPort'`n[[hosts]]`nname = 'localhost'`n[[hosts.locations]]`npath = '/'`nhandler = 'not_found'`n[[hosts.locations]]`npath = '/cgi'`nhandler = 'fastcgi'`nroot = 'cgi'`n[hosts.locations.fastcgi]`nservers = ['127.0.0.1:$cgiPort']`nconcurrency = 1`n")
    $other = Start-Owned $binary ('--config "{0}"' -f $otherConfig) 'unrelated-instance'
    Start-Main
    Require ((Http '/app/') -match '"sapi":"cli"') 'Real persistent PHP not serving'
    Require ((Http '/cgi/index.php') -match 'qualification-cgi=cgi-fcgi') 'Operator-owned php-cgi not serving'
    Require ((Http '/ready.txt' $pin1) -match 'qualification-ready') 'Pinned initial TLS handshake failed'
    Record 'HTTP_HTTPS_PHP' 'PASS' 'Cleartext, pinned localhost TLS, persistent PHP and independently owned php-cgi serve scratch responses'
    $ping = Control 'ping'; $status = Control 'status'
    Require ($ping.Exit -eq 0 -and $status.Exit -eq 0 -and $status.Output.Contains('state=serving')) 'Authorized control failed'
    if ($Mode -eq 'Service') {
        Record 'ServiceAccountControl' 'PASS' 'Actual LocalService probe token can ping/status; user/owner SIDs recorded'
        if ($identity.User.Value -ne 'S-1-5-18') {
            $denied = Start-Owned $binary ('--config "{0}" --control ping' -f $config) 'operator-denied'
            Require ($denied.WaitForExit(7000)) 'Operator denial deadline'
            $entry = $owned[$owned.Count - 1]
            Require ($denied.ExitCode -ne 0 -and $entry.Err.Result -match 'os error 5') 'Different operator token was not denied with access denied'
            $events.Add(@{ DeniedAccount = $identity.Name; DeniedUserSid = $identity.User.Value; Exit = $denied.ExitCode; Error = $entry.Err.Result })
            Record 'CrossAccountACL' 'PASS' 'Different elevated operator denied; LocalService authorized; no pipe ACL broadened'
        } else { Record 'CrossAccountACL' 'NOT VERIFIED' 'Runner is LocalSystem, explicitly allowed by the pipe ACL; ordinary operator not tested' }
    }
    Copy-Item -LiteralPath (Join-Path $certs 'chain2.pem') -Destination (Join-Path $certs 'active.pem') -Force
    Copy-Item -LiteralPath (Join-Path $certs 'leaf2.key') -Destination (Join-Path $certs 'active.key') -Force
    Require ((Control 'reload-certificates').Exit -eq 0) 'Valid TLS reload refused'
    Require ((Http '/ready.txt' $pin2) -match 'qualification-ready') 'Replacement leaf not adopted'
    Record 'TLSReload' 'PASS' 'Explicit control reload adopts the replacement pinned certificate'
    Copy-Item -LiteralPath (Join-Path $certs 'leaf1.key') -Destination (Join-Path $certs 'active.key') -Force
    $rollback = Control 'reload-certificates'
    Require ($rollback.Exit -ne 0 -and $rollback.Output.Contains('active_set=previous')) 'Invalid key did not produce rollback'
    Require ((Http '/ready.txt' $pin2) -match 'qualification-ready') 'Rollback changed the serving leaf'
    Copy-Item -LiteralPath (Join-Path $certs 'leaf2.key') -Destination (Join-Path $certs 'active.key') -Force
    Record 'TLSRollback' 'PASS' 'Mismatched disk key rejected; last-good leaf continues serving'

    if ($LifecycleBaseline -or $LifecycleExtension) {
        $children = Capture-Workers
        Begin-Barrier 'directstop'
        $receipt = Control 'stop'
        Require ($receipt.Exit -eq 0 -and $receipt.Output.Contains('stopping=true')) 'Direct stop receipt failed'
        Require (-not $main.HasExited -and (Service-State $name) -ne 'Stopped') 'Direct stop prematurely reported completion'
        [IO.File]::WriteAllText((Join-Path $state 'request-directstop-release'), 'release')
        Require ($main.WaitForExit(10000) -and $main.ExitCode -eq 0) 'Direct stop did not drain/exit 0'
        Require (Test-Path -LiteralPath (Join-Path $state 'request-directstop-completed')) 'Direct stop lost transaction'
        foreach ($child in $children) { Require ($child.WaitForExit(5000)) 'Direct stop orphaned child' }
        if ($LifecycleBaseline) {
            # Probe the actual failed state, not an XML parsing assumption.
            Require (-not $mainWrapper.WaitForExit(3000)) 'Vendor baseline unexpectedly exited cleanly'
            Require ((Service-State $name) -eq 'Running') 'Vendor baseline did not reproduce fake RUNNING'
            $wrapperLog = [IO.File]::ReadAllText((Join-Path $logs "$name.wrapper.log"))
            Require ($wrapperLog -match 'Access is denied|Access denied') 'No SCM access-denied diagnostic'
            $events.Add(@{ BaselineExitZero = @{ MainPid = $main.Id; Exit = $main.ExitCode; WrapperPid = $mainWrapper.Id; ScmState = (Service-State $name); Log = $wrapperLog } })
            Record 'DirectExitZero' 'FAIL (REPRODUCED)' 'Actual main exit 0; WinSW LocalService SCM access denied and wrapper still RUNNING'
            # The vendor also disposes its main Process after the failed
            # SignalStopped. Later custom SCM stop dereferences process.Id and
            # throws "No process is associated". Do not wait forever on that
            # known broken baseline; disable only its recovery and terminate
            # the retained wrapper AFTER the actual main/children exited.
            $null = Assert-OwnService $name $wrapper
            $null = Run-Tool "$env:SystemRoot\System32\sc.exe" @('failure', $name, 'reset=', '0', 'actions=', '')
            [HamsterQualificationNative]::Kill($mainWrapper)
            Require ($mainWrapper.WaitForExit(5000)) 'Stuck baseline wrapper did not exit'
            Wait-Stopped
            $null = Run-Tool "$env:SystemRoot\System32\sc.exe" @('failure', $name, 'reset=', '0', 'actions=', 'restart/3000')
            Start-Main
            $children = Capture-Workers; $oldMain = $main; $oldWrapper = $mainWrapper
            # Isolate baseline failure from a competing auto-recovery instance.
            $null = Run-Tool "$env:SystemRoot\System32\sc.exe" @('failure', $name, 'reset=', '0', 'actions=', '')
            [HamsterQualificationNative]::Kill($oldWrapper)
            Require ($oldWrapper.WaitForExit(5000)) 'Baseline wrapper did not die'
            Require (-not $oldMain.WaitForExit(2000)) 'Baseline wrapper unexpectedly contained main'
            Require ((Http '/app/') -match 'HTTP/1.1 200') 'Baseline orphan did not actually serve'
            $events.Add(@{ BaselineWrapperCrash = @{ WrapperPid = $oldWrapper.Id; MainPid = $oldMain.Id; Children = @($children | ForEach-Object { $_.Id }); MainAlive = $true; ScmState = (Service-State $name); RecoveryDisabledForIsolation = $true } })
            Record 'WrapperCrash' 'FAIL (REPRODUCED)' 'Wrapper dead while retained Hamster/PHP tree still live and serving'
            Assert-Isolation
            Record 'BaselineReproduction' 'PASS' 'Both vendor lifecycle gaps reproduced without SCM ACL changes'
            return
        }
        Wait-Stopped
        Require ($mainWrapper.WaitForExit(5000) -and $mainWrapper.ExitCode -eq 0) 'Exit-zero wrapper did not exit 0'
        # Observe longer than the configured 3-second failure-action delay.
        # This is a recovery-policy window, not a guessed transaction timing.
        $observation = [Diagnostics.Stopwatch]::StartNew()
        while ($observation.ElapsedMilliseconds -lt 6000) {
            $service = Assert-OwnService $name $wrapper
            Require ($service.State -eq 'Stopped' -and $service.ProcessId -eq 0) 'Intentional exit 0 incorrectly triggered recovery'
            Start-Sleep -Milliseconds 100
        }
        $events.Add(@{ DirectExitZero = @{ MainPid = $main.Id; MainExit = $main.ExitCode; WrapperPid = $mainWrapper.Id; WrapperExit = $mainWrapper.ExitCode; ScmState = (Service-State $name); TransactionComplete = $true } })
        Record 'DirectExitZero' 'PASS' 'Out-of-band LocalService stop drains transaction, actual main/wrapper exit 0 and SCM STOPPED, no privilege increase'
        Start-Main
    }

    $children = Capture-Workers
    Begin-Barrier 'grace'
    $helper = Begin-Stop
    Wait-Until 'stop acknowledgement while worker-owned request remains busy' {
        $path = Join-Path $logs 'last-stop.log'
        (Test-Path -LiteralPath $path) -and ([IO.File]::ReadAllText($path).Contains('acknowledged=True'))
    }
    Require (-not $main.HasExited) 'Stop receipt incorrectly equated with main exit'
    if ($Mode -eq 'Service') { Require ((Service-State $name) -ne 'Stopped') 'SCM STOPPED precedes transaction drain' }
    [IO.File]::WriteAllText((Join-Path $state 'request-grace-release'), 'release')
    Wait-Stopped
    Require ($main.ExitCode -eq 0) 'Normal shutdown did not exit 0'
    Require (Test-Path -LiteralPath (Join-Path $state 'request-grace-completed')) 'Disconnected-client transaction was not completed'
    foreach ($child in $children) { Require ($child.WaitForExit(5000)) 'Graceful shutdown left a retained PHP/descendant handle live' }
    $worker = $children | Where-Object { Test-Path -LiteralPath (Join-Path $state "shutdown-$($_.Id)") }
    Require ($null -ne $worker) 'No normal worker SHUTDOWN_ACK marker'
    Require ([IO.File]::ReadAllText((Join-Path $logs 'last-stop.log')).Contains('forced=false')) 'Graceful stop used forced fallback'
    if ($helper) { Require ($helper.WaitForExit(5000) -and $helper.ExitCode -eq 0) 'Stop helper did not finish normally' }
    $events.Add(@{ GracefulMainPid = $main.Id; MainExitCode = $main.ExitCode; Ack = $true; Forced = $false; ScmObserved = ($Mode -eq 'Service') })
    Record 'GracefulStop' 'PASS' "Disconnected transaction completes; worker ACK, main/child exit verified; SCM STOPPED checked only in Service mode ($Mode)"

    Start-Main
    $old = $main
    $children = Capture-Workers
    $worker = $children | Where-Object { (Get-CimInstance Win32_Process -Filter "ProcessId = $($_.Id)" -OperationTimeoutSec 3).ParentProcessId -eq $main.Id } | Select-Object -First 1
    [HamsterQualificationNative]::Kill($worker)
    Require ($worker.WaitForExit(5000)) 'Worker kill did not signal retained handle'
    $null = Http '/app/'
    Wait-Until 'replacement worker READY' { try { (Http '/app/') -match 'HTTP/1.1 200' } catch { $false } }
    $replacement = Capture-Workers
    Require (($replacement | Where-Object { $_.StartTime -gt $worker.StartTime }).Count -ge 2) 'No replacement worker/descendant generation'
    Begin-Barrier 'crash'
    $allChildren = @($children) + @($replacement)
    $clock = [Diagnostics.Stopwatch]::StartNew()
    [HamsterQualificationNative]::Kill($main)
    Require ($main.WaitForExit(5000)) 'Hamster crash handle did not signal'
    foreach ($child in $allChildren) { Require ($child.WaitForExit(5000)) 'Abnormal main death left a retained application process live' }
    $events.Add(@{ CrashPid = $old.Id; CreatedUtc = $old.StartTime.ToUniversalTime().ToString('o'); Mechanism = 'TerminateProcess retained handle'; ReclaimMs = $clock.ElapsedMilliseconds })
    Record 'ReplacementContainment' 'PASS' 'Initial and replacement PHP generations and their ordinary descendants exit after main crash'
    if ($Mode -eq 'Service') {
        Wait-Until 'SCM recovery creates a new owned main' {
            try {
                $next = Find-Main
                if ($next -and $next.StartTime -gt $old.StartTime) { $script:main = $next; return $true }
            } catch { }
            $false
        } 45
        Wait-Until 'recovered PHP readiness' { try { (Http '/app/') -match 'HTTP/1.1 200' } catch { $false } }
        Record 'CrashRecovery' 'PASS' 'SCM/WinSW automatically creates a new main instance and restores real PHP HTTP service'
    } else {
        Start-Main
        Record 'CrashRecovery' 'NOT VERIFIED' 'Process smoke restarts explicitly; not SCM recovery'
    }

    if ($Mode -eq 'Service') {
        $null = Http '/app/logs'
        Wait-Until 'WinSW stderr rotations' { @(Get-ChildItem -LiteralPath $logs -Filter "$name.*err.log").Count -ge 3 }
        $rotated = @(Get-ChildItem -LiteralPath $logs -Filter "$name.*err.log")
        Require ($rotated.Count -le 9) 'WinSW retained more than active plus eight stderr files'
        $events.Add(@{ RotatedLogs = @($rotated | ForEach-Object { @{ Name = $_.Name; Bytes = $_.Length } }); ThresholdKiB = 64; KeepFiles = 8 })
        Record 'LogRotation' 'PASS' 'Actual WinSW stderr size rotation at scratch 64 KiB, eight retained; wrapper.log is not covered by this policy'
    } else { Record 'LogRotation' 'NOT VERIFIED' 'Direct process mode has no WinSW log handler' }

    Begin-Barrier 'fallback'
    $children = Capture-Workers
    # Remove control only from the on-disk client config; the running server's
    # immutable config is unchanged. The helper must wait, not kill on CLI error.
    [IO.File]::WriteAllText($config, ($originalConfig -replace "(?m)^\[server.control\]\r?\nname = '[^']+'\r?\n", ''))
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $helper = Begin-Stop
    Wait-Stopped
    Require ($clock.Elapsed.TotalSeconds -ge 11) 'Forced fallback occurred before its grace budget'
    Require ([IO.File]::ReadAllText((Join-Path $logs 'last-stop.log')).Contains('forced=true')) 'No forced fallback evidence'
    foreach ($child in $children) { Require ($child.WaitForExit(5000)) 'Forced fallback left an application process live' }
    if ($helper) { Require ($helper.WaitForExit(5000) -and $helper.ExitCode -eq 2) 'Fallback helper did not report forced exit' }
    [IO.File]::WriteAllText($config, $originalConfig)
    $events.Add(@{ ForcedMainPid = $main.Id; ElapsedMs = $clock.ElapsedMilliseconds; GraceSeconds = 12; MainExitCode = $main.ExitCode; ScmObserved = ($Mode -eq 'Service') })
    Record 'ForcedFallback' 'PASS' "Failed control waits 12-second budget; retained main/PHP exit verified; SCM STOPPED checked only in Service mode ($Mode)"
    Require (-not $cgi.HasExited -and -not $other.HasExited) 'Another instance or operator endpoint was terminated'
    Require (([HamsterQualificationNative]::Get($otherPort, '/', '')) -match '404') 'Separate Hamster instance stopped responding'
    Require (([HamsterQualificationNative]::Get($otherPort, '/cgi/index.php', '')) -match 'qualification-cgi=cgi-fcgi') 'Operator endpoint no longer serves real PHP through the independent instance'
    Record 'Isolation' 'PASS' 'Independent Hamster and operator-owned php-cgi remain responsive after service stop/crash/fallback'
    if ($LifecycleExtension) {
        Start-Main
        foreach ($case in @('idle', 'busy', 'stopping', 'repeat1', 'repeat2')) { Test-WrapperCrash $case }
        $children = Capture-Workers
        $null = Begin-Stop; Wait-Stopped
        foreach ($child in $children) { Require ($child.WaitForExit(5000)) 'Final graceful stop orphaned child' }
        Record 'WrapperCrash' 'PASS' 'Five wrapper crash/recovery cycles: idle, busy, stopping and repeated recovery; external CGI/other instance unaffected'
        Record 'ServiceQualification' 'PASS' 'Pinned WinSW 2.11 LocalService + lifecycle extension, direct exit 0 and SCM graceful stop; main and wrapper recovery; no boot claim'
    } else {
        Record 'WrapperCrash' 'NOT VERIFIED' 'Not exercised; lifecycle extension comparison requires explicit -LifecycleExtension Service rehearsal'
        if ($Mode -eq 'Service') { Record 'ServiceQualification' 'PASS' 'Original WinSW 2.11 LocalService SCM-stop scope only; independent exit 0 and wrapper crash remain unqualified' }
    }
} catch {
    $failed = $true
    if (Test-Path -LiteralPath $base) { Record 'Failure' 'FAIL' $_.Exception.ToString() }
    Write-Error $_ -ErrorAction Continue
} finally {
    # Disable recovery only on our verified scratch Services before cleanup.
    foreach ($id in @($registered.Keys)) {
        try {
            $null = Assert-OwnService $id $registered[$id]
            $null = Run-Tool "$env:SystemRoot\System32\sc.exe" @('failure', $id, 'reset=', '0', 'actions=', '')
            $service = Get-Service -Name $id
            if ($service.Status -notin @('Stopped', 'StopPending')) { $service.Stop() }
        } catch { $events.Add(@{ CleanupError = $_.Exception.Message }); $failed = $true; $cleanupFailed = $true }
    }
    if ($originalConfig -and (Test-Path -LiteralPath $config)) { [IO.File]::WriteAllText($config, $originalConfig) }
    foreach ($entry in $owned) {
        $process = $entry.Process
        try {
            if (-not $process.HasExited -and -not $process.WaitForExit(5000)) { [HamsterQualificationNative]::Kill($process); Require ($process.WaitForExit(5000)) 'Owned process did not exit during cleanup' }
            if ($entry.ContainsKey('Out')) {
                [IO.File]::WriteAllText((Join-Path $logs ($entry.Label + '-' + $process.Id + '.stdout')), $entry.Out.Result)
                [IO.File]::WriteAllText((Join-Path $logs ($entry.Label + '-' + $process.Id + '.stderr')), $entry.Err.Result)
            }
        } catch { $events.Add(@{ CleanupError = $_.Exception.Message }); $failed = $true; $cleanupFailed = $true }
    }
    foreach ($id in @($registered.Keys)) {
        try {
            $null = Assert-OwnService $id $registered[$id]
            Wait-Until 'owned Service STOPPED during cleanup' { (Service-State $id) -eq 'Stopped' } 15
            $null = Run-Tool $registered[$id] @('uninstall')
            Wait-Until 'owned Service removal' { $null -eq (Service-Record $id) } 15
            # WinSW install creates an Event Log source; remove only our source.
            if ([Diagnostics.EventLog]::SourceExists($id)) { [Diagnostics.EventLog]::DeleteEventSource($id) }
        } catch { $events.Add(@{ CleanupError = $_.Exception.Message }); $failed = $true; $cleanupFailed = $true }
    }
    foreach ($entry in $owned) { $entry.Process.Dispose() }
    if (Test-Path -LiteralPath $root) {
        $checked = [IO.Path]::GetFullPath($root)
        Require ($checked.StartsWith([IO.Path]::GetFullPath($base) + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) 'Refusing unsafe fixture cleanup'
        Record 'Cleanup' $(if ($cleanupFailed) { 'FAIL' } else { 'PASS' }) 'Owned handles waited; only verified scratch Services uninstalled; no process-name kill'
        if (-not $KeepFixture -and -not $failed) { Remove-Item -LiteralPath $checked -Recurse -Force }
        elseif ($failed) { Write-Warning "Fixture retained for diagnosis: $root" }
    }
    Write-Host "Evidence: $reportPath"
}
if ($failed) { exit 1 }
exit 0
