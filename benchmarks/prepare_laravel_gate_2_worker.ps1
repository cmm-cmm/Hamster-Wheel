param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'

$sourceDirectory = (Resolve-Path -LiteralPath '.\fixtures\php-worker').Path
$support = (Resolve-Path -LiteralPath '.\benchmarks\support\laravel_gate_2_profile.php').Path
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) {
    throw "OutputDirectory already exists; refusing to overwrite it: $output"
}
$null = New-Item -ItemType Directory -Path $output

foreach ($name in @(
    'protocol.php', 'laravel_stdout.php', 'laravel_request.php',
    'laravel_lifecycle.php', 'laravel_octane_client.php'
)) {
    Copy-Item -LiteralPath (Join-Path $sourceDirectory $name) -Destination (Join-Path $output $name)
}
Copy-Item -LiteralPath $support -Destination (Join-Path $output 'laravel_gate_2_profile.php')

$workerSource = Join-Path $sourceDirectory 'laravel_worker.php'
$worker = Get-Content -Raw -LiteralPath $workerSource

function Replace-ExactlyOnce([string]$Text, [string]$Needle, [string]$Replacement) {
    $first = $Text.IndexOf($Needle, [StringComparison]::Ordinal)
    if ($first -lt 0 -or $Text.IndexOf($Needle, $first + $Needle.Length, [StringComparison]::Ordinal) -ge 0) {
        throw "Gate 2 worker anchor must occur exactly once: $Needle"
    }
    return $Text.Substring(0, $first) + $Replacement + $Text.Substring($first + $Needle.Length)
}

$worker = Replace-ExactlyOnce $worker @'
require __DIR__ . '/laravel_octane_client.php';
'@ @'
require __DIR__ . '/laravel_octane_client.php';
require __DIR__ . '/laravel_gate_2_profile.php';
'@

$worker = Replace-ExactlyOnce $worker @'
    Once::flush();
'@ @'
    Once::flush();
    $gate2Profiler = HamsterGate2Profiler::fromEnvironment();
    if ($gate2Profiler !== null) {
        $gate2Profiler->register($app, $worker);
    }
'@

$worker = Replace-ExactlyOnce $worker @'
    $decoded = hamster_decode_request($frame);
'@ @'
    $gate2FrameReceived = hrtime(true);
    $decoded = hamster_decode_request($frame);
    $gate2Decoded = hrtime(true);
'@

$worker = Replace-ExactlyOnce $worker @'
    }

    $stray = '';
'@ @'
    }

    $gate2DumpRequested = $gate2Profiler !== null
        && $request->headers->get('X-Hamster-Gate2-Dump') === '1';

    $stray = '';
'@

$worker = Replace-ExactlyOnce $worker @'
    $stray = '';
    $guardLevel = ob_get_level();
    ob_start();
    try {
        $worker->handle($request, new RequestContext([
'@ @'
    $gate2Reconstructed = hrtime(true);
    $stray = '';
    $guardLevel = ob_get_level();
    ob_start();
    try {
        $gate2HandleStarted = hrtime(true);
        if ($gate2Profiler !== null && ! $gate2DumpRequested) {
            $gate2Profiler->begin(
                $decoded['id'],
                $gate2FrameReceived,
                $gate2Decoded,
                $gate2Reconstructed,
                $gate2HandleStarted,
            );
        }
        $worker->handle($request, new RequestContext([
'@

$worker = Replace-ExactlyOnce $worker @'
        $lifecycle->completeRequest();
        hamster_discard_uploads();
        if (! $client->shouldStop()) {
'@ @'
        if ($gate2Profiler !== null) {
            $gate2Profiler->mark('octane_returned');
        }
        $lifecycle->completeRequest();
        hamster_discard_uploads();
        if ($gate2Profiler !== null) {
            $gate2Profiler->mark('hamster_cleanup');
        }
        if (! $client->shouldStop()) {
'@

$worker = Replace-ExactlyOnce $worker @'
            $client->markCleanupCompleted();
            $client->flushResponse();
'@ @'
            if ($gate2Profiler !== null) {
                $gate2Profiler->mark('response_commit_started');
            }
            $client->markCleanupCompleted();
            $client->flushResponse();
            if ($gate2Profiler !== null) {
                $gate2Profiler->finish();
                if ($gate2DumpRequested) {
                    $gate2Profiler->dump();
                }
            }
'@

$destination = Join-Path $output 'laravel_worker.php'
Set-Content -LiteralPath $destination -Value $worker -Encoding UTF8

$metadata = [PSCustomObject]@{
    GeneratedAt = [DateTimeOffset]::Now.ToString('o')
    ProductionWorker = $workerSource
    ProductionWorkerSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $workerSource).Hash
    ProfilerSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $support).Hash
    GeneratedWorkerSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $destination).Hash
}
$metadata | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'generation.json') -Encoding UTF8
$metadata
