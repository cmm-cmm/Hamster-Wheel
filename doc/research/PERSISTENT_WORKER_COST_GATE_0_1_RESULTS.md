# Persistent Worker Cost — Gate 0–1 Results

Date: 2026-09-16

Status: completed Gate 0–1 investigation for owner review. This report does
not authorize Gate 2, Gate 3, a runtime architecture change, or a roadmap
change.

Raw evidence and per-run files are in
[`benchmarks/results/persistent_cost_gate_0_1_20260916/`](../../benchmarks/results/persistent_cost_gate_0_1_20260916/).
The environment, hashes, validation rules and rejected runs are recorded in
the directory's [`README.md`](../../benchmarks/results/persistent_cost_gate_0_1_20260916/README.md).

## Technical result

FACT: current generic persistent-worker execution costs approximately
0.185-0.194 ms/request end to end at W1/C1 for a 14-28 byte response. At
W4/C16, its saturated capacity implies 0.147-0.149 ms/worker/request. PHP
fixture CPU is only about 28-29 us/request; Rust server CPU is about 162-205
us/request depending on concurrency.

FACT: production-format direct IPC has 86.9 us P50. The production blocking
payload path is 68.6 us P50 at 16 KiB and 104.5 us at 64 KiB. There is no
payload discontinuity near the measured PMS login response. The known Windows
default-pipe failure remains a real negative control at 15.071 ms P50, but the
production blocking path has zero timer-quantum clustering.

FACT: on PMS W1/C1, the light Laravel controller costs 2.151 ms/service and
login costs 2.740 ms/service. The comparable trivial JSON service interval is
0.185 ms. The resulting 1.965 ms difference is a combined Laravel framework,
middleware, routing and Octane lifecycle budget. Gate 1 cannot assign that
budget to application code, Laravel or Hamster's adapter.

INFERENCE: the generic Rust/protocol/fixture path is not the dominant PMS cost.
On W1 light it is an 8.6 percent upper bound. Removing all direct IPC would
save at most a small fraction of the measured PMS service time; the experiment
does not show that an address-space rewrite is the highest-value investment.

FACT: Gate 1 does **not** satisfy its own stop rule. The combined minimal
Laravel lifecycle/framework budget is about 71.7 percent of W1 login service,
far above the ten-percent investigation band. Also, Rust CPU plus the measured
synthetic PHP protocol cost is about 12.4 percent of W1 login combined CPU,
though it falls to about 9.5 percent in the accepted W4 cell. The result is an
unresolved cost decomposition, not approval or rejection of the current
architecture.

## Gate 0 controls

The measured Hamster release was source commit
`fc75f705e6130beb82d6a84ffd6496361310f8e9`, release-binary SHA-256
`5E157A6045748A237A5C0C9D97911B0372CCF80CEC0ACC6890A6F2DD30DA785F`.
The PMS scratch came from commit
`552e27525b1161ed10b36bdd726d203ecfa3ccff`, Laravel 12.66.0 and Octane
2.19.1. PHP was 8.5.10 NTS with CLI OPcache. Full hashes are in the raw README.

PMS was production optimized and ran with file sessions, array cache, sync
queue, production environment and debug disabled. The two routes do not touch
the remote database or shared Redis. The machine was Windows 11 build 26200,
Core i5-12400, 32 GiB, Balanced power plan. Client and server shared the host.

Every accepted cell has three fresh server instances and at least ten measured
seconds after warm-up. The driver is closed-loop, so no open-loop overload-tail
claim is made. W4 uses one connection pool and CookieContainer per lane; this
was corrected after the original shared-cookie harness was found to represent
one concurrent user session rather than sixteen independent sessions.

FACT: one plan assumption was false. The current scratch's `/login` response
is 19,059 uncompressed bytes, not the historical 19,160 bytes. The direct
16 KiB payload cell remains a close bracket; the exact response size is now an
input measured during Gate 0 rather than a constant.

## Direct IPC evidence

### Production-format timeline

| Stage | Median |
|---|---:|
| Rust encode | 0.7 us |
| Rust write | 52.1 us |
| combined pipe transfer/wait | 26.0 us |
| PHP decode + handler + encode + write | 9.9 us |
| Rust decode | 0.4 us |
| total P50 | 86.9 us |
| combined process CPU | 117.2 us/request |

The CPU result is coarser than the stage wall timings because Windows process
CPU accounting advances in large increments over these short trials.

### Production blocking payload curve

| Payload | P50 | P95 | P99 |
|---:|---:|---:|---:|
| 0 B | 20.1 us | 40.2 us | 53.3 us |
| 128 B | 34.3 us | 67.6 us | 92.7 us |
| 1 KiB | 54.7 us | 79.5 us | 128.1 us |
| 16 KiB | 68.6 us | 79.9 us | 109.5 us |
| 64 KiB | 104.5 us | 135.9 us | 150.9 us |
| 1 MiB | 950.9 us | 1,393.5 us | 1,393.5 us |

INFERENCE: response-size copying becomes visible above 16 KiB but is smooth.
At the current roughly 19 KiB login size, there is no evidence of a threshold,
extra syscall quantum or framing cliff worth an architecture change.

## End-to-end evidence

Accepted medians:

| Workload | W/C | RPS range | median RPS | P50 | Rust CPU | PHP CPU | service interval/worker |
|---|---:|---:|---:|---:|---:|---:|---:|
| trivial hello | 1/1 | 5,061-5,371 | 5,148 | 0.182 ms | 165.2 us | 28.2 us | 0.194 ms |
| trivial JSON | 1/1 | 5,232-5,909 | 5,394 | 0.173 ms | 161.9 us | 28.4 us | 0.185 ms |
| trivial hello | 4/16 | 25,366-28,997 | 26,908 | 0.543 ms | 205.3 us | 28.7 us | 0.149 ms |
| trivial JSON | 4/16 | 24,345-27,271 | 27,134 | 0.542 ms | 200.1 us | 29.4 us | 0.147 ms |
| PMS light | 1/1 | 457-471 | 465 | 2.048 ms | 275.0 us | 1,815.6 us | 2.151 ms |
| PMS login | 1/1 | 334-368 | 365 | 2.618 ms | 321.9 us | 2,343.8 us | 2.740 ms |
| PMS light, isolated sessions | 4/16 | 1,307-1,379 | 1,361 | 11.560 ms | 318.1 us | 2,519.7 us | 2.938 ms |
| PMS login, isolated sessions | 4/16 | 1,077-1,170 | 1,095 | 14.016 ms | 332.1 us | 3,266.4 us | 3.653 ms |

FACT: PMS W4 throughput is about 2.93x W1 for light and 3.00x for login with
four workers. Combined CPU/request rises about 36 percent in both workloads.
At W4, capacity-derived service interval is within approximately 0.06-0.10 ms
of combined Rust+PHP CPU/request, so the accepted isolated-session runs do not
show a large unexplained blocking interval.

INFERENCE: the W1-to-W4 efficiency loss lives mainly in additional PHP CPU per
request under the multi-session/multi-process workload, not in generic pool
dispatch: the trivial pool scales past 4x from C1 to C16 because C1 does not
saturate the server. Gate 1 cannot distinguish Laravel session/middleware,
OPcache/process locality, allocator or Octane lifecycle cost.

HYPOTHESIS: part of the extra W4 PHP CPU comes from maintaining sixteen file
sessions rather than one session. The shared-session diagnostic used less PHP
CPU but introduced waiting and worse tails on the light route. Testing this
causally would be a separate experiment; it is not attributed here.

## Resource evidence

| Workload | W/C | process private | working set | observed RPS/GiB private |
|---|---:|---:|---:|---:|
| trivial JSON | 1/1 | 9.6 MiB | 24.0 MiB | 575,000 |
| trivial JSON | 4/16 | 36.4 MiB | 76.0 MiB | 763,000 |
| PMS light | 1/1 | 26.3 MiB | 70.1 MiB | 18,100 |
| PMS login | 1/1 | 28.5 MiB | 73.2 MiB | 13,100 |
| PMS light | 4/16 | 102.0 MiB | 235.6 MiB | 13,700 |
| PMS login | 4/16 | 110.7 MiB | 245.3 MiB | 10,100 |

The density figures describe these exact cells and are not a cross-runtime
ranking. W1 cells are latency cells rather than fully saturated capacity.

FACT: PHP private bytes scale almost exactly linearly: approximately 7.65 MiB
per trivial worker, 24.2 MiB per PMS-light worker and 26.3 MiB per PMS-login
worker. Rust private bytes rise from about 2.0 MiB at W1 to 5.1-5.5 MiB at W4.
There is no W1-to-W4 superlinear process-private growth.

INFERENCE: the application's resident worker image, not Hamster's Rust
supervisor, is the main marginal-memory cost. This supports measuring process
topology before proposing a shared runtime, but does not prove that sharing
would preserve isolation or reduce system commit by the same amount.

FACT: topology-neutral system commit could not be qualified on this shared
workstation. Per-run deltas ranged from -576 MiB to +1,037 MiB while the
process tree stayed stable. The report therefore does not claim system-wide
RPS/GiB or compare Hamster memory with FrankenPHP's 317 MB combined process.

## Anomalies and rejected evidence

### One shared cookie jar was a bad W4 control

The original client shared one CookieContainer across all concurrency lanes.
This made W4 a single-session concurrency test. After isolating sessions per
lane, light median throughput rose about 9.8 percent and tails tightened; login
throughput remained within the combined run variation while PHP CPU increased.
Both raw batches are retained, but only isolated-session cells are used for the
capacity table.

### One externally contaminated run

One original shared-session login run fell to 627.8 RPS, p95 63.9 ms and p99
121.8 ms while system commit grew by 1,037 MiB outside the stable 110 MiB
Hamster/PHP private footprint. A complete three-run repeat produced
1,114-1,147 RPS. The contaminated run remains in raw evidence and
`summary.csv`; it was not silently removed.

### Host variation limits small claims

Trivial RPS full spreads are 6-14 percent depending on the cell. The accepted
isolated-session PMS spreads are about 5.3 percent for light and 8.5 percent
for login. Effects within roughly twice those spreads are inconclusive on this
machine. This does not obscure the 10x-scale generic-versus-framework
difference, but it prevents small optimization claims.

### Startup measurement was not valid READY timing

The measurement loop timed listener readiness plus CIM child-process
enumeration. It varied between roughly 560 and 1,100 ms and did not observe the
protocol READY boundary. It is discarded. First-response latency is retained
separately: PMS medians were about 51-65 ms, with individual fresh-host
responses up to 198 ms. These are cold observations, not steady-state samples.

## Stability and reliability qualification

The existing ignored integration tests passed for:

- Laravel ownership/lifecycle checks;
- W1 authenticated-to-anonymous isolation;
- cleanup failure withholding the response and retiring the generation;
- possibly-started request not replayed;
- protocol corruption failing closed and recovering;
- client cancellation not cancelling the application transaction;
- shutdown reaping all worker processes.

No Hamster server or fixture PHP worker remained after the runs. These tests
qualify the current lifecycle semantics; they do not make the performance
measurements more precise, and the performance results do not qualify lifecycle
correctness.

## Plan assumptions after execution

### Stood up

- Production blocking pipes avoid the Windows timer quantum.
- The payload path is smooth around the current login response size.
- Generic end-to-end cost is small relative to Laravel request processing.
- Heavy/database routes are unnecessary for locating generic runtime cost.
- Lifecycle/failure qualification must remain separate from steady-state RPS.

### Falsified or downgraded

- Historical 19,160-byte login size was not stable; Gate 0 measured 19,059 B.
- System commit is not usable without an isolated host or stronger accounting.
- A single CookieContainer is not a neutral W4 application workload.
- Child-process presence is not a READY/startup measurement.
- The minimal Laravel controller is not cheap enough to let Gate 1 dismiss the
  combined framework/Octane lifecycle budget.

## Technical judgment

FACT: Gate 1 locates generic Hamster persistent-worker cost at roughly
0.15-0.20 ms/request for tiny responses, with direct production-format IPC at
roughly 0.087 ms. Rust CPU on real PMS requests is roughly 0.28-0.33 ms; PHP is
roughly 1.8-3.3 ms depending on route and concurrency.

INFERENCE: optimizing or replacing the generic worker boundary cannot explain
most current PMS time. The larger unresolved opportunity is inside the combined
Laravel framework/Octane/request lifecycle, plus the increase in PHP CPU under
W4 multi-session load. Gate 1 cannot tell whether either is Hamster-actionable.

HYPOTHESIS: a narrowly instrumented Gate 2 would determine whether Octane
adapter lifecycle is material or whether normal Laravel middleware/session work
dominates. This report does not authorize or execute that gate.

There is no evidence here that requires embedded PHP, a sharded host, Morph or
another worker architecture. There is also not enough decomposition to declare
the current adapter fully optimized. The owner can either stop because the
generic boundary is already below the desired investment threshold, or approve
a separate Gate 2 to resolve the combined Laravel/Octane budget.

## Reproduction commands

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- timeline
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- timeline_blocking
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- raw
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- raw_blocking
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- variants

./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 `
  -WarmupRequests 1000 -MinimumSeconds 10 -WorkerCounts 1 `
  -Concurrencies 1 -Workload all -OutputPath <raw.json>
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 `
  -WarmupRequests 1000 -MinimumSeconds 10 -WorkerCounts 4 `
  -Concurrencies 16 -Workload all -OutputPath <raw.json>

./benchmarks/laravel_pms_0_4.ps1 -PmsPath <prepared-scratch> `
  -Candidate persistent -Workload all -Runs 3 -Requests 5000 `
  -WarmupRequests 500 -MinimumSeconds 10 -Workers 1 -Concurrency 1 `
  -OutputPath <raw.json>
./benchmarks/laravel_pms_0_4.ps1 -PmsPath <prepared-scratch> `
  -Candidate persistent -Workload all -Runs 3 -Requests 5000 `
  -WarmupRequests 500 -MinimumSeconds 10 -Workers 4 -Concurrency 16 `
  -OutputPath <raw.json>
```
