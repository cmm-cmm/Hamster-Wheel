# WinSW 2.11 custom stop waits are unbounded. This helper owns the deadline.
# Run only as WinSW's direct child (or the process-level rehearsal owner).
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Executable,
    [Parameter(Mandatory = $true)][string]$Config,
    [Parameter(Mandatory = $true)][string]$OwnerExecutable,
    # Milliseconds must fit Process.WaitForExit's signed 32-bit argument.
    [ValidateRange(1, 2147483)][int]$GraceSeconds = 120,
    [string]$EvidencePath
)

$ErrorActionPreference = 'Stop'
$stopClock = [Diagnostics.Stopwatch]::StartNew()
$mainProcess = $null
$ownerProcess = $null
$controlProcess = $null
$controlStarted = $false
$stopExitCode = 1

function Write-Evidence([string]$Message) {
    $line = '{0:o} elapsed_ms={1} {2}' -f [DateTime]::UtcNow, $stopClock.ElapsedMilliseconds, $Message
    try { [Console]::Out.WriteLine($line) } catch {}
    if ($EvidencePath) {
        try { [IO.File]::AppendAllText($EvidencePath, $line + [Environment]::NewLine) } catch {}
    }
}

try {
    $Executable = (Resolve-Path -LiteralPath $Executable).ProviderPath
    $Config = (Resolve-Path -LiteralPath $Config).ProviderPath
    $OwnerExecutable = (Resolve-Path -LiteralPath $OwnerExecutable).ProviderPath
    if ($EvidencePath) {
        try { [IO.File]::WriteAllText($EvidencePath, '') } catch {}
    }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
public static class HamsterStopNative {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder name, ref uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr process, out long created, out long exited, out long kernel, out long user);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("shell32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CommandLineToArgvW(string command, out int count);
    [DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr memory);
    public static string Image(Process process) {
        var name = new StringBuilder(32768); uint size = (uint)name.Capacity;
        if (!QueryFullProcessImageName(process.Handle, 0, name, ref size)) throw new Win32Exception();
        return name.ToString();
    }
    public static long Created(Process process) {
        long created, exited, kernel, user;
        if (!GetProcessTimes(process.Handle, out created, out exited, out kernel, out user)) throw new Win32Exception();
        return created;
    }
    public static void Terminate(Process process) {
        // Use the retained handle, never reopen a PID to terminate it.
        if (!TerminateProcess(process.Handle, 1)) throw new Win32Exception();
    }
    public static string[] Arguments(string command) {
        int count; IntPtr memory = CommandLineToArgvW(command, out count);
        if (memory == IntPtr.Zero) throw new Win32Exception();
        try {
            var result = new string[count];
            for (int i = 0; i < count; i++) result[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(memory, i * IntPtr.Size));
            return result;
        } finally { LocalFree(memory); }
    }
}
'@

    # PID snapshots are discovery only. Verify the running owner instance,
    # direct parent relationship, executable, config argv and creation times.
    $selfRecord = Get-CimInstance Win32_Process -Filter "ProcessId = $PID" -OperationTimeoutSec 3
    $ownerProcess = [Diagnostics.Process]::GetProcessById($selfRecord.ParentProcessId)
    $null = $ownerProcess.Handle
    $ownerCreated = [HamsterStopNative]::Created($ownerProcess)
    if ([HamsterStopNative]::Image($ownerProcess) -ine $OwnerExecutable -or $ownerProcess.HasExited) {
        throw 'The stop helper is not a child of the expected live wrapper.'
    }
    $records = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($ownerProcess.Id)" -OperationTimeoutSec 3)
    foreach ($record in $records) {
        $candidate = $null
        try {
            if (-not $record.CommandLine) { continue }
            $argv = [HamsterStopNative]::Arguments($record.CommandLine)
            if ($argv.Count -ne 3 -or $argv[1] -cne '--config' -or [IO.Path]::GetFullPath($argv[2]) -ine $Config) { continue }
            $candidate = [Diagnostics.Process]::GetProcessById($record.ProcessId)
            $null = $candidate.Handle
            if ([HamsterStopNative]::Image($candidate) -ine $Executable) { continue }
            $created = [HamsterStopNative]::Created($candidate)
            $fresh = Get-CimInstance Win32_Process -Filter "ProcessId = $($candidate.Id)" -OperationTimeoutSec 3
            if ($created -lt $ownerCreated -or $fresh.ParentProcessId -ne $ownerProcess.Id -or $candidate.HasExited -or $ownerProcess.HasExited) { continue }
            # Recheck argv after acquiring the handle: the discovery PID may
            # have been reused before it was opened. A retained live handle
            # and a fresh matching deployment record are both required.
            if (-not $fresh.CommandLine) { continue }
            $freshArgv = [HamsterStopNative]::Arguments($fresh.CommandLine)
            if ($freshArgv.Count -ne 3 -or $freshArgv[1] -cne '--config' -or [IO.Path]::GetFullPath($freshArgv[2]) -ine $Config) { continue }
            if ($mainProcess) { throw 'More than one owned Hamster matches this deployment; refusing termination.' }
            $mainProcess = $candidate
            $candidate = $null
            Write-Evidence "owned_pid=$($mainProcess.Id) created_filetime=$created owner_pid=$($ownerProcess.Id) image=$Executable"
        } finally {
            if ($candidate) { $candidate.Dispose() }
        }
    }
    if (-not $mainProcess) { throw 'No verified live Hamster child matches this wrapper and configuration.' }

    try {
        $controlInfo = New-Object Diagnostics.ProcessStartInfo
        $controlInfo.FileName = $Executable
        $controlInfo.Arguments = '--config "{0}" --control stop' -f $Config
        $controlInfo.UseShellExecute = $false
        $controlInfo.CreateNoWindow = $true
        $controlInfo.RedirectStandardOutput = $true
        $controlInfo.RedirectStandardError = $true
        $controlProcess = New-Object Diagnostics.Process
        $controlProcess.StartInfo = $controlInfo
        $null = $controlProcess.Start()
        $controlStarted = $true
        $controlOutput = $controlProcess.StandardOutput.ReadToEndAsync()
        $controlErrors = $controlProcess.StandardError.ReadToEndAsync()
        $remaining = [Math]::Max(0, $GraceSeconds * 1000 - $stopClock.ElapsedMilliseconds)
        if ($controlProcess.WaitForExit([int][Math]::Min(7000, $remaining))) {
            # This is receipt only. The retained main process is waited below.
            Write-Evidence "control_exit=$($controlProcess.ExitCode) acknowledged=$($controlOutput.Result.Contains('stopping=true'))"
        } else {
            $controlProcess.Kill()
            $null = $controlProcess.WaitForExit(5000)
            Write-Evidence 'control helper exceeded its budget; waiting for the owned main process'
        }
    } catch {
        Write-Evidence "control_failed=true message=$($_.Exception.Message)"
    }
    $remaining = [Math]::Max(0, $GraceSeconds * 1000 - $stopClock.ElapsedMilliseconds)
    if ($mainProcess.WaitForExit([int]$remaining)) {
        Write-Evidence "main_exited=true forced=false exit_code=$($mainProcess.ExitCode)"
        $stopExitCode = 0
    } else {
        Write-Evidence "grace_expired=true forcing_owned_pid=$($mainProcess.Id)"
        [HamsterStopNative]::Terminate($mainProcess)
        if (-not $mainProcess.WaitForExit(5000)) { throw 'The retained process handle did not signal after forced termination.' }
        Write-Evidence "main_exited=true forced=true exit_code=$($mainProcess.ExitCode)"
        $stopExitCode = 2
    }
} catch {
    Write-Evidence "stop_failed=true message=$($_.Exception.Message)"
} finally {
    if ($controlProcess) {
        if ($controlStarted -and -not $controlProcess.HasExited) { $controlProcess.Kill(); $null = $controlProcess.WaitForExit(5000) }
        $controlProcess.Dispose()
    }
    if ($mainProcess) { $mainProcess.Dispose() }
    if ($ownerProcess) { $ownerProcess.Dispose() }
}
exit $stopExitCode
