param(
    [Parameter(Mandatory = $true)]
    [string]$ResultsDirectory,
    [string]$ResultPattern = '*.json',
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath $ResultsDirectory).Path
if ($OutputPath -eq '') { $OutputPath = Join-Path $root 'summary.csv' }

function Get-Percentile($Values, [double]$Percentile) {
    if ($Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    return [double]$sorted[[Math]::Floor(($sorted.Count - 1) * $Percentile)]
}

$stages = @(
    'frame_decode_us', 'request_reconstruction_us', 'worker_guard_setup_us',
    'octane_prepare_us', 'laravel_handle_us', 'response_stage_us',
    'octane_terminate_us', 'octane_tail_us', 'hamster_cleanup_us',
    'response_commit_us', 'php_total_us'
)

$rows = @()
foreach ($resultFile in Get-ChildItem -LiteralPath $root -Filter $ResultPattern -File | Sort-Object Name) {
    $result = Get-Content -Raw -LiteralPath $resultFile.FullName | ConvertFrom-Json
    foreach ($measurement in @($result.Measurements)) {
        $records = @()
        if ($measurement.Gate2Profile -eq 'on') {
            $profileDirectory = [string]$measurement.Gate2ProfileDirectory
            if (-not (Test-Path -LiteralPath $profileDirectory)) {
                throw "Missing profile directory recorded in $($resultFile.Name): $profileDirectory"
            }
            foreach ($profileFile in Get-ChildItem -LiteralPath $profileDirectory -Filter 'php-*.json' -File) {
                $records += @(Get-Content -Raw -LiteralPath $profileFile.FullName | ConvertFrom-Json)
            }
        }

        $row = [ordered]@{
            File = $resultFile.Name
            Workers = [int]$measurement.Workers
            Concurrency = [int]$measurement.Concurrency
            Requests = [int]$measurement.Requests
            Rps = [double]$measurement.Rps
            P50Ms = [double]$measurement.P50Ms
            P95Ms = [double]$measurement.P95Ms
            P99Ms = [double]$measurement.P99Ms
            ServerCpuUsPerRequest = [double]$measurement.ServerCpuUsPerRequest
            PhpCpuUsPerRequest = [double]$measurement.PhpCpuUsPerRequest
            ClientCpuUsPerRequest = [double]$measurement.ClientCpuUsPerRequest
            Profile = [string]$measurement.Gate2Profile
            ProfileSamples = $records.Count
            ProfileWorkers = @($records.pid | Sort-Object -Unique).Count
        }
        foreach ($stage in $stages) {
            $values = @($records | ForEach-Object { [double]$_.$stage })
            $row["${stage}_p50"] = Get-Percentile $values 0.50
            $row["${stage}_p95"] = Get-Percentile $values 0.95
        }
        $rows += [PSCustomObject]$row
    }
}

$rows | Export-Csv -LiteralPath $OutputPath -NoTypeInformation -Encoding UTF8
$rows | Format-Table File, Workers, Concurrency, Requests, Rps, PhpCpuUsPerRequest, Profile, ProfileSamples, ProfileWorkers
