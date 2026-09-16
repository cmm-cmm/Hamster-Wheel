# Persistent worker cost Gate 2 raw evidence

Date: 2026-09-16

This directory contains Gate 2 only. Gate 3 was not opened. The production
execution architecture and wire protocol were not changed.

## Frozen inputs

- Hamster source: `fc75f705e6130beb82d6a84ffd6496361310f8e9`, plus the
  uncommitted Gate 0-2 measurement diff recorded by `git status` in the report.
- Normal release binary SHA-256:
  `FBF71B4485DAF3B0B45C83E7EE250FC88D8229D0C7774567D8EAC31478560851`.
- `profiling` release binary SHA-256:
  `8D9C93CA54B3A70856AEE34238545DE042EA146B86472A580DCCC44D79E97BA8`.
- PMS source commit: `552e27525b1161ed10b36bdd726d203ecfa3ccff`.
- Composer lock SHA-256:
  `63C1EE0274EE87BF8B343F01D2418D6E2655EB7C4D3CF10FF9B58FCBD367B38B`.
- Laravel 12.66.0; Laravel Octane 2.19.1; the same production-optimized
  scratch, PHP ini and environment as Gate 0-1.
- PHP ini SHA-256:
  `BD5CBF26CDCF6CB4E2FDE5A083637BF16526F55FF43E18A20E509DC3FFF5447F`.
- PHP 8.5.10 NTS, CLI OPcache enabled and timestamp validation disabled.
- Rust 1.98.1, Windows MSVC release target.
- Windows 11 build 26200, Core i5-12400 (6C/12T), 32 GiB, Balanced power.
- Client and server shared the workstation. External activity caused visible
  drift; candidates/topologies were alternated and all run ranges are kept.

The original `www/pms` checkout remained clean. The external scratch received
only the scratch probe provider and regenerated Laravel caches. The new
`/__hamster/bare` control invokes the same controller as `light` without the
`web` middleware group.

## Instrumentation contract

`prepare_laravel_gate_2_worker.ps1` copies the production worker and inserts
checked anchors. `generated_worker_noio/generation.json` records input and
output hashes. The production worker has no profiling branch.

The accepted profiler samples one request ID in 64. It records local PHP
`hrtime()` boundaries and buffers records in memory. Only after throughput and
process CPU/resource counters have been captured does a separate request with
`X-Hamster-Gate2-Dump: 1` write each worker's JSON file. Every W4 accepted run
has files from all four PIDs. Clocks are never subtracted across processes.

Stage definitions:

- `frame_decode`: production protocol frame decode after the complete frame is
  already available to PHP;
- `request_reconstruction`: Symfony/Laravel request construction;
- `worker_guard_setup`: Hamster output guard setup;
- `octane_prepare`: Octane clone/gateway plus all configured
  `RequestReceived` listeners;
- `laravel_handle`: Laravel kernel, route, middleware, controller and
  `RequestHandled` listeners;
- `response_stage`: response body/header extraction and Hamster protocol
  encoding before Octane's handled callback;
- `octane_terminate`: terminating middleware and `RequestTerminated`
  listeners;
- `octane_tail`: route controller flush, sandbox flush, view resolver reset
  and restoration of the base application;
- `hamster_cleanup`: transaction/static/process-state and upload cleanup;
- `response_commit`: commit-gate transition and pipe frame write.

Each raw record's stages span `php_total_us`; the remaining rounding residual
has P50 0.4 us and P95 at most 0.7 us in accepted cells.

The stored `generated_worker_noio` is the exact accepted artifact. Its dump
request could itself be selected when its per-worker request ID was divisible
by 64, but `php_total_us` ended before the JSON file write. At most one such
same-route record per worker/run can be present. Conservatively dropping the
last sampled record from every PID moved grouped P50 by at most 10.1 us at W1
and 3.8 us at W4 and did not change attribution. The checked-in generator now
excludes dump-trigger requests from sampling.

## Accepted evidence

- `cpu-r*-w1.json`, `cpu-r*-w4.json` and their `profile-*` directories are the
  final light-route W1/C1 and W4/C16 runs. `cpu_stage_summary.csv` is the
  per-run summary.
- `bare-r*-w1.json`, `bare-r*-w4.json` and their `profile-*` directories are
  the bare-controller control. `bare_summary.csv` is the per-run summary.
- Every cell used three fresh servers, 500 sequential warm-up requests, then
  at least ten measured seconds. W1 completed at least 5,000 requests; W4 ran
  for the full time window.
- `response_validation.txt` records complete response lengths and digests for
  bare, light and login outside the timed loops.

The accepted instrumentation-overhead controls are
`w[14]-noio-r*-{normal,enabled}.json`. Median throughput changed by -1.5% at
W1 and -2.4% at W4, below the 3% observer band and below normal run spread.
The earlier profiling-binary/scaffolding-disabled cells are
`w[14]-r*-{normal,disabled}.json`; their median differences were within 1.1%.

## Rejected/anomalous evidence

The first profiler appended JSON batches inside the measured window. Its W4
median was 6.7% below production at sample rate 1/8. Reducing sampling to 1/64
but retaining in-window file creation still left a 7.8% median difference.
Those `w[14]-r*-enabled` and `w[14]-s64-r*-enabled` batches and the corresponding
`generated_worker` are retained, but rejected. Moving all filesystem output
after counter capture reduced the median difference below 3%.

`noio-smoke.json` and `profile-noio-smoke` are functional smoke evidence, not
performance cells. Absolute RPS drifted materially during this long session;
small cross-batch differences are not optimization evidence.

`quality_results.txt` records the build, lint, bench-link, unit, Laravel
lifecycle/isolation, syntax and post-run orphan checks. Linux was not run.

## Principal reproduction commands

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc --features profiling
./benchmarks/prepare_laravel_gate_2_worker.ps1 -OutputDirectory <worker-dir>

./benchmarks/laravel_pms_0_4.ps1 -PmsPath <prepared-scratch> `
  -Candidate persistent -Workload light -Runs 1 -Requests 5000 `
  -WarmupRequests 500 -MinimumSeconds 10 -Workers 1 -Concurrency 1 `
  -ServerExecutable <profiling-release.exe> `
  -PersistentScript <worker-dir>/laravel_worker.php -Gate2Profile on `
  -Gate2ProfileOutput <raw-dir> -Gate2SampleRate 64 -OutputPath <run.json>

# Repeat with -Workers 4 -Concurrency 16, then repeat both cells for
# -Workload bare. Alternate topology/order and use three fresh servers each.
./benchmarks/summarize_laravel_gate_2.ps1 `
  -ResultsDirectory ./benchmarks/results/persistent_cost_gate_2_20260916 `
  -ResultPattern 'cpu-r*.json' -OutputPath <summary.csv>
```
