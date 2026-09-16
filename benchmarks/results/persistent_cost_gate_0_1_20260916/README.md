# Persistent Worker Gate 0–1 Raw Evidence

Date: 2026-09-16 (Asia/Bangkok)

Scope: Gate 0–1 only from
`doc/research/PERSISTENT_WORKER_COST_INVESTIGATION_PLAN.md`. No Gate 2 stage
instrumentation and no Gate 3 FrankenPHP comparison were run.

## Frozen inputs

- Hamster source commit: `fc75f705e6130beb82d6a84ffd6496361310f8e9`.
- Working tree contained documentation/research work plus the measurement-only
  harness changes used by this run; no production runtime source changed.
- Release binary SHA-256:
  `5E157A6045748A237A5C0C9D97911B0372CCF80CEC0ACC6890A6F2DD30DA785F`.
- Rust: `rustc 1.98.1 (48a229cea 2026-09-01)`, MSVC target.
- PHP: 8.5.10 NTS x64, SHA-256
  `32DA1562C56DFE511A621627E3BBD9721058E855A6F14225C5803BC19BA623E9`.
- PMS source commit: `552e27525b1161ed10b36bdd726d203ecfa3ccff`.
- Scratch Composer lock SHA-256:
  `63C1EE0274EE87BF8B343F01D2418D6E2655EB7C4D3CF10FF9B58FCBD367B38B`.
- Scratch PHP ini SHA-256:
  `BD5CBF26CDCF6CB4E2FDE5A083637BF16526F55FF43E18A20E509DC3FFF5447F`.
- Laravel 12.66.0; Octane 2.19.1; optimized config/events/routes/views;
  OPcache CLI enabled and timestamp validation disabled.
- PMS runtime overrides: `SESSION_DRIVER=file`, `CACHE_STORE=array`,
  `QUEUE_CONNECTION=sync`, `APP_ENV=production`, `APP_DEBUG=false`, probe
  provider enabled. The measured routes do not use the shared Redis or remote
  database.
- Windows 11 Pro build 26200; Core i5-12400, 6 cores / 12 logical; 32 GiB RAM;
  Balanced power scheme.
- Client and server ran on the same workstation. Two unrelated PHP development
  server processes were present but showed zero CPU over the sampled three
  seconds before measurement. The workstation was not otherwise isolated.

The PHP 8.5 platform requirement was ignored while preparing the scratch to
retain the PMS dependency graph, as in the 0.4 evidence. This is a compatibility
qualification gap, not a performance tuning choice.

## Method

- Release build only.
- Three fresh server instances per accepted cell.
- W1/C1 and W4/C16 only.
- Minimum ten measured seconds per run. Trivial worker used at least 20,000
  requests and 1,000 warm-up requests. PMS used at least 5,000 requests and
  500 warm-up requests.
- Closed-loop `.NET HttpClient`, same host, HTTP/1.1. W4 PMS uses one client,
  connection pool and CookieContainer per concurrency lane. These results
  measure completion capacity and client-observed latency, not open-loop
  overload tail.
- Every timed response required the expected status, a completely received
  body via `ResponseContentRead`, and a positive Content-Length. Trivial bodies
  additionally required exact lengths: 14 bytes for hello and 28 for JSON.
- PMS bodies are intentionally dynamic (CSRF token and controller counter), so
  their digests are not compared. A separate structural probe recorded:
  - `/__hamster/light`: 200, JSON, 33 bytes;
  - `/login`: 200, HTML, 19,059 uncompressed bytes.
- CPU is process CPU delta from Windows `GetProcessTimes`, divided by completed
  requests. Working set, private bytes, handles and threads are process-tree
  samples after the measured interval.
- System commit was sampled but rejected as a resource-efficiency metric: its
  per-run delta ranged from -576 MiB to +1,037 MiB while Hamster/PHP private
  bytes stayed stable. This is external workstation activity, not process-tree
  allocation.
- Context-switch counts are unavailable because the harness did not have a
  stable per-PID cumulative Windows counter.

## Direct IPC

Production-format `timeline_blocking`, median of three trials:

| Stage | Median |
|---|---:|
| Rust encode | 0.7 us |
| Rust write | 52.1 us |
| combined transfer/wait | 26.0 us |
| PHP decode | 5.5 us |
| PHP handler | 0.2 us |
| PHP encode | 2.1 us |
| PHP write | 2.1 us |
| Rust decode | 0.4 us |
| total P50 | 86.9 us |
| combined process CPU | 117.2 us/request |

The production blocking path had zero timer-quantum samples. The default PHP
negative control reproduced the known failure: 15,071.2 us P50 and about 96
percent late-window quantum clustering.

Production `pipe_blocking` raw payload sweep:

| Payload | P50 | P95 | P99 |
|---:|---:|---:|---:|
| 0 B | 20.1 us | 40.2 us | 53.3 us |
| 16 B | 36.4 us | 67.9 us | 93.2 us |
| 128 B | 34.3 us | 67.6 us | 92.7 us |
| 1 KiB | 54.7 us | 79.5 us | 128.1 us |
| 16 KiB | 68.6 us | 79.9 us | 109.5 us |
| 64 KiB | 104.5 us | 135.9 us | 150.9 us |
| 1 MiB | 950.9 us | 1,393.5 us | 1,393.5 us |

Short direct cases are below Windows process-CPU accounting resolution; zero
child CPU samples are not evidence of free execution. Wall latency is the
usable payload-curve metric.

## End-to-end medians

See `summary.csv` for accepted and rejected cells. Headline accepted medians:

| Workload | W/C | RPS | P50 | Rust CPU | PHP CPU | service interval/worker |
|---|---:|---:|---:|---:|---:|---:|
| trivial hello | 1/1 | 5,148.0 | 0.182 ms | 165.2 us | 28.2 us | 0.194 ms |
| trivial JSON | 1/1 | 5,394.2 | 0.173 ms | 161.9 us | 28.4 us | 0.185 ms |
| trivial hello | 4/16 | 26,908.1 | 0.543 ms | 205.3 us | 28.7 us | 0.149 ms |
| trivial JSON | 4/16 | 27,134.0 | 0.542 ms | 200.1 us | 29.4 us | 0.147 ms |
| PMS light JSON | 1/1 | 465.0 | 2.048 ms | 275.0 us | 1,815.6 us | 2.151 ms |
| PMS login | 1/1 | 365.0 | 2.618 ms | 321.9 us | 2,343.8 us | 2.740 ms |
| PMS light JSON | 4/16 | 1,361.3 | 11.560 ms | 318.1 us | 2,519.7 us | 2.938 ms |
| PMS login | 4/16 | 1,094.9 | 14.016 ms | 332.1 us | 3,266.4 us | 3.653 ms |

Resource medians:

| Workload | W/C | process private | working set | handles | threads |
|---|---:|---:|---:|---:|---:|
| trivial JSON | 1/1 | 9.6 MiB | 24.0 MiB | 279 | 24 |
| trivial JSON | 4/16 | 36.4 MiB | 76.0 MiB | 839 | 63 |
| PMS light | 1/1 | 26.3 MiB | 70.1 MiB | 300 | 23 |
| PMS login | 1/1 | 28.5 MiB | 73.2 MiB | 300 | 23 |
| PMS light | 4/16 | 102.0 MiB | 235.6 MiB | 917 | 50 |
| PMS login | 4/16 | 110.7 MiB | 245.3 MiB | 917 | 49 |

The PHP component accounts for about 7.65 MiB private per trivial worker and
26.3 MiB private per PMS login worker. Rust private memory was about 2.0 MiB at
W1 and 5.1-5.5 MiB at W4.

## Rejected run and anomalies

The original PMS client used one CookieContainer for all 16 lanes, so every
concurrent request shared one Laravel session. That is a workload-specific
contention control, not a general multi-user capacity cell. With isolated
per-lane sessions, light throughput rose from 1,239.9 to 1,361.3 RPS and tails
tightened materially; login throughput stayed in the same broad range while
PHP CPU/request increased because each lane maintained independent session
state. The shared-session batches remain as diagnostics rather than accepted
capacity cells.

The first original shared-session login W4/C16 run produced only 627.8 RPS with
63.9 ms p95 and 121.8 ms p99. During the same ten seconds, system commit increased by
1,037 MiB while the Hamster/PHP process tree stayed at 110 MiB private. The
other two original runs reached 1,086.4 and 1,137.0 RPS. The complete cell was
repeated rather than silently deleting the outlier; the repeat range was
1,114.3-1,146.8 RPS. The original batch remains in `pms_w4_c16.json` and
`pms_w4_c16.stdout.txt` and is marked rejected in `summary.csv`.

Trivial throughput also has material host variance: up to 6.1 percent for
hello W1, 12.6 percent for JSON W1, 13.5 percent for hello W4 and 10.8 percent
for JSON W4. Absolute RPS claims below these ranges are not defensible. The
generic-versus-PMS order-of-magnitude difference is much larger than this
variance.

First-response observations were separate from steady state. Median first
response was about 1-7 ms for the trivial fixture, 51 ms for PMS light and
63 ms for PMS login. Some first fresh-host runs were materially slower (up to
198 ms for login). Startup timing was discarded: its process-enumeration loop
varied between about 560 and 1,100 ms and did not observe the protocol READY
boundary.

## Reliability companion checks

The following existing ignored integration tests passed against the prepared
scratch/PHP distribution:

- Laravel ownership contract (25 checks);
- W1 authenticated-to-anonymous isolation plus cleanup-failure retirement;
- response staging until cleanup commit;
- possibly-started request is not replayed;
- protocol corruption fails closed and replaces the generation;
- cancellation preserves the application transaction;
- shutdown leaves no orphan PHP worker.

After benchmarks and tests, there were zero `hamster-wheel` processes and zero
PHP processes whose command line referenced Hamster worker fixtures.

## Raw files

- `php_ipc_timeline*.{stdout,stderr}.txt`
- `php_ipc_raw*.{stdout,stderr}.txt`
- `php_ipc_variants.{stdout,stderr}.txt`
- `pool_w1_c1.json`, `.stdout.txt`, `.stderr.txt`
- `pool_w4_c16.json`, `.stdout.txt`, `.stderr.txt`
- `pms_w1_c1.json`, `.stdout.txt`, `.stderr.txt`
- `pms_w4_c16.json`, `.stdout.txt`, `.stderr.txt`
- `pms_w4_c16_login_repeat.json`, `.stdout.txt`, `.stderr.txt`
- `pms_w4_c16_isolated_sessions.json`, `.stdout.txt`, `.stderr.txt`
- `reliability_results.txt`

Commands and parameter values are preserved in the top-level research report.
