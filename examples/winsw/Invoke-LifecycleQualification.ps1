# Owner-authorized scratch Services only; no automatic elevation or reboot.
[CmdletBinding()]
param([switch]$RunnerSelfTest)
$ErrorActionPreference = 'Stop'
function Invoke-QualificationChild([string[]]$Arguments, [string]$Log) {
    # Windows PowerShell's native stderr adaptation plus ErrorAction=Stop can
    # terminate a native child before its finally cleanup. Capture streams with
    # ProcessStartInfo instead; the child's error exit is data, not a PS error.
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $quoted = foreach ($argument in $Arguments) {
        $escaped = [regex]::Replace($argument, '(\\*)"', '$1$1\"')
        $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
        '"' + $escaped + '"'
    }
    $info.Arguments = $quoted -join ' '
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    try {
        $null = $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        [IO.File]::WriteAllText($Log, $stdout.Result + $stderr.Result)
        return $process.ExitCode
    } finally { $process.Dispose() }
}
if ($RunnerSelfTest) {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('hamster-runner-' + [Guid]::NewGuid().ToString('N') + '.log')
    try {
        $code = Invoke-QualificationChild @('-NoProfile', '-NonInteractive', '-Command', 'try { Write-Error "expected runner error" -ErrorAction Continue } finally { Start-Sleep -Milliseconds 100; Write-Output "finally completed" }; exit 1') $scratch
        $log = [IO.File]::ReadAllText($scratch)
        if ($code -ne 1 -or -not $log.Contains('finally completed') -or -not $log.Contains('expected runner error')) { throw 'Native stderr interrupted child cleanup.' }
        Write-Host 'PASS: stderr/error exit preserved and child finally completed.'
    } finally { if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch } }
    exit 0
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'Run this complete baseline/fixed rehearsal as Administrator.' -ErrorAction Continue
    exit 3
}
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$base = Join-Path $repo 'target\windows-service-qualification'
$null = New-Item -ItemType Directory -Path $base -Force
$stamp = [Guid]::NewGuid().ToString('N')
$stages = [Collections.Generic.List[object]]::new()
foreach ($stage in @('baseline', 'fixed')) {
    $log = Join-Path $base ($stamp + '-' + $stage + '.log')
    $arguments = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $PSScriptRoot 'Invoke-ServiceQualification.ps1'), '-Mode', 'Service')
    if ($stage -eq 'baseline') { $arguments += '-LifecycleBaseline' }
    else { $arguments += '-LifecycleExtension' }
    $code = Invoke-QualificationChild $arguments $log
    $stages.Add(@{ Stage = $stage; Exit = $code; Log = $log })
    [IO.File]::WriteAllText((Join-Path $base ($stamp + '-matrix.json')), ($stages | ConvertTo-Json))
    if ($code -ne 0) { exit $code }
}
exit 0
