<#
.SYNOPSIS
Build an isolated FlatPress 1.5.1 application for FastCGI tests.

.DESCRIPTION
Downloads the published GitHub source tag into a new target directory. The
source tree is left unmodified; setup runs over HTTP in tests/php_cgi.rs.
#>
param([Parameter(Mandatory = $true)][string] $Target)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Target = [IO.Path]::GetFullPath($Target)
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (Test-Path -LiteralPath $Target) {
    throw "target already exists: $Target"
}
New-Item -ItemType Directory -Path $Target | Out-Null
$archive = Join-Path $Target 'flatpress-1.5.1.zip'
$unpack = Join-Path $Target 'unpack'
$url = 'https://github.com/flatpressblog/flatpress/archive/refs/tags/1.5.1.zip'
curl.exe -sSfL -o $archive $url
if ($LASTEXITCODE -ne 0) { throw "FlatPress download failed: $url" }
$expected = 'D292443F5B2C0E5900F15D2B541D71462A618989074F0E4DF7B14ABE73A192EF'
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash
if ($actual -ne $expected) { throw "FlatPress archive SHA256 mismatch: $actual" }
Expand-Archive -LiteralPath $archive -DestinationPath $unpack
$source = Join-Path $unpack 'flatpress-1.5.1'
if (-not (Test-Path -LiteralPath (Join-Path $source 'setup.php'))) {
    throw "FlatPress release layout changed: $source"
}
Move-Item -LiteralPath $source -Destination (Join-Path $Target 'app')
Remove-Item -LiteralPath $archive
Remove-Item -LiteralPath $unpack -Recurse
$extensions = Join-Path $repoRoot 'php\8.5\ext'
$ini = Join-Path $Target 'php.ini'
$iniText = @"
extension_dir = "$extensions"
extension=mbstring
extension=fileinfo
extension=openssl
extension=intl
extension=gd
extension=curl
extension=zip
display_errors=stderr
display_startup_errors=stderr
log_errors=Off
error_reporting=E_ALL
"@
[IO.File]::WriteAllText($ini, $iniText, [Text.UTF8Encoding]::new($false))
Write-Output "FlatPress 1.5.1 ready: $(Join-Path $Target 'app')"
