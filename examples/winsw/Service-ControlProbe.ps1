# Finite, scratch-only WinSW child used to exercise the actual LocalService token.
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Deployment)
$ErrorActionPreference = 'Stop'
$controlChild = $null
$started = $false
$resultPath = Join-Path $Deployment 'state\control-result.json'
try {
    $request = Get-Content -LiteralPath (Join-Path $Deployment 'state\control-request.json') -Raw | ConvertFrom-Json
    if ($request.Command -notin @('ping', 'status', 'reload-certificates', 'stop')) { throw 'Invalid probe command.' }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = Join-Path $Deployment 'hamster-wheel.exe'
    $info.Arguments = '--config "{0}" --control {1}' -f (Join-Path $Deployment 'hamster.toml'), $request.Command
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $controlChild = New-Object Diagnostics.Process
    $controlChild.StartInfo = $info
    $started = $controlChild.Start()
    $stdout = $controlChild.StandardOutput.ReadToEndAsync()
    $stderr = $controlChild.StandardError.ReadToEndAsync()
    if (-not $controlChild.WaitForExit(7000)) { throw 'Control client exceeded its deadline.' }
    $result = @{ Command = $request.Command; Exit = $controlChild.ExitCode; Output = $stdout.Result; Error = $stderr.Result; User = $identity.User.Value; Owner = $identity.Owner.Value; Account = $identity.Name }
} catch {
    $result = @{ Exit = -1; Error = $_.Exception.Message }
} finally {
    if ($controlChild) {
        if ($started -and -not $controlChild.HasExited) { $controlChild.Kill(); $null = $controlChild.WaitForExit(5000) }
        $controlChild.Dispose()
    }
}
$temporaryResult = $resultPath + '.tmp'
[IO.File]::WriteAllText($temporaryResult, ($result | ConvertTo-Json))
[IO.File]::Move($temporaryResult, $resultPath)
# WinSW 2.11's unsolicited exit-0 path calls SignalStopped, which requests
# SCM access LocalService does not have. The harness explicitly SCM-stops this
# probe AFTER the control exchange/result completes; no transaction is killed.
# Bound the hold if the harness itself disappears. Nonzero expiry lets SCM
# observe a failed wrapper exit, rather than stranding its exit-0 callback.
Start-Sleep -Seconds 30
exit 1
