[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$WinSW,
    [Parameter(Mandatory = $true)][string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
$WinSW = (Resolve-Path -LiteralPath $WinSW).ProviderPath
$expected = '31DCA0D9BD3C5AB7E119172BDB927B90C9DA935D0F4963335EAF60E7D8F7AABF'
if ((Get-FileHash -LiteralPath $WinSW -Algorithm SHA256).Hash -ne $expected) { throw 'Only pinned WinSW 2.11.0 NET461 is supported.' }
$output = [IO.Path]::GetFullPath($OutputDirectory)
$null = New-Item -ItemType Directory -Path $output -Force
$compiler = Join-Path $env:SystemRoot 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
$dll = Join-Path $output 'HamsterLifecycleExtension.dll'
# CLR resolves the compile-time WinSW.NET461 assembly name. Keep an unchanged
# reference copy beside the renamed Service executable; this copy is NOT run.
$reference = Join-Path $output 'WinSW.NET461.exe'
if ($WinSW -ine $reference) { Copy-Item -LiteralPath $WinSW -Destination $reference }
& $compiler /nologo /target:library /warnaserror+ /optimize+ "/out:$dll" "/reference:$reference" /reference:System.ServiceProcess.dll (Join-Path $PSScriptRoot 'HamsterLifecycleExtension.cs')
if ($LASTEXITCODE -ne 0) { throw "Lifecycle extension compiler failed: $LASTEXITCODE" }
Get-FileHash -LiteralPath $dll -Algorithm SHA256
