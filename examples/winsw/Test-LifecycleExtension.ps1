# NON-SERVICE smoke checks only. Never interpret these as SCM qualification.
[CmdletBinding()]
param([string]$WinSW, [string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
if ([IntPtr]::Size -ne 8) { throw 'This smoke check qualifies only the x64 deployment.' }
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if (-not $WinSW) { $WinSW = Join-Path $repo 'target\tools\winsw-2.11.0\WinSW.NET461.exe' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repo 'target\tools\hamster-lifecycle' }
$hash = & (Join-Path $PSScriptRoot 'Build-LifecycleExtension.ps1') -WinSW $WinSW -OutputDirectory $OutputDirectory
$vendor = [Reflection.Assembly]::LoadFrom((Resolve-Path -LiteralPath $WinSW).ProviderPath)
$shim = [Reflection.Assembly]::LoadFrom((Join-Path ([IO.Path]::GetFullPath($OutputDirectory)) 'HamsterLifecycleExtension.dll'))
$type = $shim.GetType('Hamster.WinSW.LifecycleExtension', $true)
$instance = [Activator]::CreateInstance($type)
$flags = [Reflection.BindingFlags]'Static,NonPublic'
$job = $type.GetField('lifetimeJob', $flags)
try {
    $instance.OnWrapperStarted()
    throw 'Extension unexpectedly started outside a real Service.'
} catch {
    if ($_.Exception.ToString() -notmatch 'Unsupported WinSW service status layout') { throw }
}
if ($null -ne $job.GetValue($null)) { throw 'Rejected startup created a containment job.' }
foreach ($pair in @(@('ExtendedLimits', 144), @('ServiceStatus', 28))) {
    $layout = $type.GetNestedType($pair[0], [Reflection.BindingFlags]::NonPublic)
    $value = [Activator]::CreateInstance($layout)
    if ([Runtime.InteropServices.Marshal]::SizeOf($value) -ne $pair[1]) { throw "Unexpected x64 Win32 layout: $($pair[0])" }
}
# The exception hook MUST NOT act on an access-denied exception from an
# unrelated caller (including installation/CLI/tool exceptions).
$callback = $type.GetMethod('OnFirstChanceException', $flags)
$commandType = $vendor.GetType('WinSW.CommandException', $true)
$constructor = $commandType.GetConstructor([Reflection.BindingFlags]'Instance,Public,NonPublic', $null, @([string], [Exception]), $null)
if (-not $constructor) { throw 'Unexpected pinned CommandException constructor.' }
foreach ($code in @(5, 6)) {
    $exception = $constructor.Invoke(@('test-only unrelated caller', [ComponentModel.Win32Exception]::new($code)))
    $event = [Runtime.ExceptionServices.FirstChanceExceptionEventArgs]::new($exception)
    $null = $callback.Invoke($null, @($null, $event))
}
if ($type.GetField('reportingStopped', $flags).GetValue($null) -ne 0) { throw 'Unrelated exception was mistaken for an exit-zero stop.' }
Write-Host "PASS NON-SERVICE: strict compilation, x64 native layouts, fail-closed outside SCM, unrelated exception ignored. SHA256=$($hash.Hash)"
Write-Host 'NOT TESTED BY THIS SMOKE SCRIPT: owned SCM status handle, actual direct exit 0, wrapper crash/recovery/stop races. See the elevated Invoke-LifecycleQualification.ps1 reports for Service evidence.'
