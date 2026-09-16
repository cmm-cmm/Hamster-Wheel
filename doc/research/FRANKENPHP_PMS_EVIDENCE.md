# FrankenPHP PMS Evidence Review

Date: 2026-09-16

Status: research evidence for owner review. This note does not change the
roadmap, select a native-runtime architecture, or authorize implementation.
For this review, FastCGI is treated as removed from the product direction by
owner decision.

## Evidence examined

The canonical supplied benchmark report is
[`doc/FRANKENPHP_BENCHMARK.md`](../FRANKENPHP_BENCHMARK.md). It consolidates
the earlier `/login` report with a second round covering five PMS routes and
supersedes the separate reports. The local benchmark installation was also
inspected read-only:

- FrankenPHP 1.12.7 / Caddy 2.11.4 / PHP 8.5.10 ZTS;
- PMS commit `ee4608daf`, Laravel 12.66.0, Octane 2.19.1;
- `num_threads 24`, 12 persistent workers, and classic and worker listeners in
  one FrankenPHP process;
- the PHP `curl_multi` driver, Caddyfile, start/reload scripts and access/error
  logs;
- the exact installed Octane worker and FrankenPHP client source.

The comparison evidence is Hamster's checked-in release PMS run at
`benchmarks/results/laravel_pms_octane_0_4_20260915.{md,csv}`: PHP 8.5.10 NTS,
PMS commit `552e2752`, Laravel 12.66.0, Octane 2.19.1, four workers and
concurrency 16.

The FrankenPHP report has no checked-in raw latency samples or replicated
per-run result file. Its first-round access logs corroborate that the measured
`/login` responses were HTTP 200 and 19,160 uncompressed bytes and that the
driver retained one connection per concurrency lane during each measured
batch. The consolidated second round reports gzip response sizes, about 1,900
authenticated GETs using one admin session, and a shared LAN MySQL/Redis
backend. The report, harness and logs are enough for architectural signals but
not a release-grade product ranking.

## The result isolates application lifetime, not native placement

Two independent axes are being discussed as if they were one:

| Placement | Fresh application per request | Persistent application |
|---|---|---|
| In-process PHP SAPI | FrankenPHP classic | FrankenPHP worker |
| Supervised PHP subprocess | Hamster `fresh` probe | Hamster Octane worker |

The Hamster `fresh` probe rebuilds Laravel inside an already-running PHP
process; it is an optimistic application-bootstrap baseline, not a complete
classic SAPI request shutdown. The table separates the intended variables but
does not make the four cells operationally equivalent.

FACT: the FrankenPHP comparison keeps placement, server process, PHP build and
thread pool fixed while changing whether Laravel is bootstrapped per request.
At C1, worker mode removes an observed 23.1-58.8 ms/request across five routes;
the resulting ratios range from 11.0x on `/login` to 1.3x on the heavy expense
report. The absolute difference, not any one ratio, is evidence about
**application lifetime**.

FACT: Hamster's 0.4 comparison similarly keeps Rust HTTP, protocol-v1 IPC,
PHP processes and worker count fixed while changing only fresh versus Octane
application lifetime. It measured 1.49x on its PMS `/login` setup.

FACT: neither experiment keeps persistent Laravel, application configuration,
worker count and concurrency fixed while changing only in-process versus
subprocess placement.

INFERENCE: the new result strengthens the case for persistent application
execution. It does not establish that embedding PHP in Hamster, sharing an
address space, using ZTS, or adopting FrankenPHP's transport would save the
same absolute time or reproduce any worker/classic speedup ratio.

## Performance findings beyond headline RPS

### Execution model versus tuning

The tuning sequence at concurrency 50 was:

| Observed configuration | Worker RPS | Classic RPS | What it can support |
|---|---:|---:|---|
| Initial worker mode | 2,663 | 366 | Persistent lifetime has a large advantage before tuning. |
| Laravel caches / OPcache tuning | 2,643 | 577 | Avoided bootstrap file/config work mainly benefits fresh execution. |
| Production env/debug setting | 2,668 | 600 | Difference is too small and unreplicated to attribute. |
| JIT tracing | 2,683 | 474 | Classic regressed; worker C50 did not, while C1 reportedly regressed. Result is mixed. |
| Final uncached/non-JIT setup | 2,925 | 357 | Reproduces the large persistent/fresh gap, but also reverts several settings. |

FACT: config/route/event caches improved the observed classic result by 58
percent and changed the worker result by less than one percent in that run.
This is consistent with caches removing work from repeated bootstrap while a
persistent worker pays bootstrap once.

INFERENCE: comparing the worker with the best observed tuned classic cell
leaves roughly a 4.4-4.6x gap on this lightweight route. Thus tuning explains
part, but not all, of the observed 8x headline. Because each tuning cell is a
single short run in a fixed sequence, the exact residual is low-confidence.

FACT: in the first-round `/login` cells, worker/classic throughput ratios remain
between 7.4x and 8.2x from C1 to C50. Across the consolidated five-route C1
results, however, the ratio ranges from 11.0x to 1.3x while the absolute latency
difference stays within 23.1-58.8 ms. This is consistent with an additive
fresh-bootstrap cost being diluted by application/database work. It still does
not make the 23-59 ms a measurement of Hamster transport or placement cost.

FACT: the expense-report worker result rises only from 15.4 RPS at C5 to
15.7 RPS at C20 while average latency rises from 290.4 to 995.8 ms. This is an
application/backend saturation signal. It is unsuitable for locating a small
runtime boundary cost, though it is useful as a negative control against
overgeneralizing lightweight-route speedups.

INFERENCE: steady-state RPS alone is not a sufficient reason to remove Laravel
caches. They may affect bootstrap latency, deterministic deployment and
multi-worker startup even when they do not improve worker request throughput.
The benchmark's Windows `services.php` creation race is direct evidence that
startup preparation matters independently of steady state.

INFERENCE: `APP_DEBUG=false` and `APP_ENV=production` remain production policy
even though this successful `/login` path did not measure a throughput change.
Exception rendering, diagnostics exposure and failure-path cost were not
exercised.

HYPOTHESIS: JIT is not useful for this PMS workload. The current measurements
are insufficient to decide it: C50 worker throughput was effectively flat,
while the reported C1 latency moved in the opposite direction. Longer replicated
runs separating JIT warm-up from steady state are required.

### Worker-count normalization changes the native-runtime story

| Runtime/cell | Workers | Concurrency | Total RPS | RPS/worker | Implied service interval |
|---|---:|---:|---:|---:|---:|
| FrankenPHP round 2 `/login` | 12 | 20 | 2,682 | 223.5 | 4.47 ms |
| FrankenPHP round 1 final `/login` | 12 | 50 | 2,925 | 243.8 | 4.10 ms |
| Hamster Octane `/login` median | 4 | 16 | 931.3 | 232.8 | 4.30 ms |

FACT: the coarse FrankenPHP per-worker values straddle the Hamster value. The
previous 4.7 percent advantage is one C50 cell, not a stable placement estimate.
The service intervals are capacity-derived and include all server, adapter and
application work; they are not measured Laravel handler durations.

INFERENCE: on this Blade route, current Hamster IPC and process placement are
not shown to be the dominant cost. The normalized values are close enough to
make a native in-process rewrite an unproved optimization, not evidence of
parity or of a particular residual percentage.

The table is **not** an apples-to-apples benchmark. The PMS commits differ,
FrankenPHP used ZTS and no Laravel caches with local/debug configuration,
Hamster used NTS and an optimized production scratch, session/cache settings
differ, the consolidated run sends `Accept-Encoding`, and there is no
FrankenPHP CPU breakdown. FrankenPHP C1 latency cannot be compared with
Hamster's saturated W4/C16 p50 because no matching Hamster W1/C1 login cell was
recorded.

### Cold and warm behavior is a separate axis

FACT: the consolidated report records first-request versus warm worker latency,
including approximately 100 versus 4 ms for `/login` and 132 versus 14 ms for
`/projects/plan`. A separate fully-cold observation reached 906 versus 14 ms on
`/projects/plan` and 2,456 versus 228 ms on the expense report.

FACT: restarting FrankenPHP did not reproduce the fully-cold state, and the
shared Redis cache was not cleared. The report's attribution of the largest
gap to Redis/application cache is therefore an inference, not a controlled
measurement.

INFERENCE: worker/application bootstrap, OPcache and external application-cache
temperature must be recorded independently. First-request and deployment-warmup
costs matter operationally but must not be averaged into steady-state runtime
overhead or used to select process placement.

### Resource efficiency is currently unknown

FACT: the report records one 317 MB FrankenPHP process with 46 threads. That
process simultaneously contains Caddy, Go, 24 PHP threads, 12 persistent
Laravel workers, classic-mode capacity and shared OPcache.

FACT: Hamster recorded about 9 MiB for Rust and 232 MiB summed working set for
four PHP workers in the corresponding persistent login runs. Summed per-process
working sets can double-count shared DLL and OPcache pages; the FrankenPHP
number does not state working set versus private/commit memory.

INFERENCE: neither RPS/MB nor marginal memory per worker can be computed
honestly. FrankenPHP's thread model may share more runtime memory, but that
benefit has not been measured against the different failure isolation it buys.

The smallest required evidence is total system commit and private bytes, idle
and saturated, at W1 and W4, including bootstrap and rolling-recycle peaks.
Expand to intermediate or W12 cells only if the marginal curve is nonlinear or
a scaling decision remains. CPU time, context switches and RPS per core are
also missing from the FrankenPHP run.

## Stability and reliability findings

### The startup failure is architectural evidence

FACT: after `optimize:clear`, 12 workers concurrently tried to create
`bootstrap/cache/services.php`; Windows `rename()` contention killed workers
and aborted FrankenPHP startup. A single-process `artisan package:discover`
preflight prevented it.

INFERENCE: persistent multi-worker operation needs an explicit single-writer
preparation phase before parallel worker boot. This applies to Hamster too: a
subprocess boundary does not make application bootstrap side effects safe.
Preparation, readiness quorum, failure backoff and generation publication must
be distinct lifecycle phases.

INFERENCE: the observed FrankenPHP deployment has a broad failure domain: all
12 worker threads and the HTTP server live in one process. Hamster's current
one-process-per-worker pool costs more process state but has demonstrated
slot-local crash and cleanup-failure recovery. A native runtime design must
price failure-domain width, not only copies and syscalls.

### Response commit and cleanup semantics differ

FACT: in Octane 2.19.1, `Worker::handle()` calls the client's `respond()` before
request-handled callbacks and `ApplicationGateway::terminate()`. The installed
`FrankenPhpClient::respond()` directly calls Symfony `Response::send()`.
`FrankenPhpClient` does not implement `StoppableClient`, so Octane's
`StopWorkerIfNecessary` listener cannot retire it after a lifecycle error.

FACT: Hamster's Laravel adapter stages the response until Octane termination,
its resource boundary and upload cleanup succeed. A failure before that commit
returns no worker frame, causes a 502 without replay and retires the generation.

INFERENCE: the benchmark proves only the success path. It does not show that a
FrankenPHP worker which partially fails after sending a response is safe to
reuse. This is a more important reliability question than the small normalized
throughput difference.

HYPOTHESIS: a native streaming runtime cannot retain Hamster's current
"cleanup before response commit" rule for arbitrarily large responses without
buffering them. A future contract may need two cleanup classes: pre-commit work
whose failure withholds the response, and post-commit work whose failure
immediately quarantines the worker but cannot retract already-sent bytes. That
contract needs fault injection before transport design.

Exact upstream sources inspected:

- [Octane `Worker.php`](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/Worker.php)
- [Octane `FrankenPhpClient.php`](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/FrankenPhp/FrankenPhpClient.php)
- [Octane `StopWorkerIfNecessary.php`](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/Listeners/StopWorkerIfNecessary.php)

### The benchmark is not a lifecycle or recovery qualification

FACT: the load driver accepts any 2xx/3xx response and does not validate the
body. The first round has 400 measured requests per cell, only eight waves at
C50. The second round adds about 1,900 authenticated database-backed GETs, but
still supplies no replicated raw sample files. Worker mode runs before classic
in the documented reproduction sequence. Client and server compete on the same
machine.

FACT: login-only checks observed different CSRF tokens and cookies, while the
authenticated round used one admin session for all concurrent requests.
Neither pins authenticated then anonymous traffic to one worker. Twelve
workers can hide the same state leak that previously escaped Hamster's W4
tests, and one shared session is not a multi-user isolation test.

FACT: `MAX_REQUESTS=1000` was configured, but the reported final 1,200 measured
requests are distributed across 12 workers and do not exercise a recycle per
worker. There is no crash, disconnect, cleanup failure, malformed protocol,
orphan, graceful shutdown, rolling recycle or long memory soak result.

FACT: deployment reload uses forced process termination rather than request
drain. It is an operational script, not at-most-once or graceful-reload
evidence.

EXTERNAL EVIDENCE: FrankenPHP documents that globals, statics and `$_ENV`
persist in worker mode, that worker scripts restart with backoff, and that
`max_requests` performs a full ZTS-thread reboot. Its current internals also
retain idle auto-scaled threads because some extensions do not cleanly release
thread state. These reinforce that interpreter lifetime and extension state
are first-class ownership boundaries, not implementation details:

- [FrankenPHP worker contract](https://github.com/php/frankenphp/blob/main/docs/worker.md)
- [FrankenPHP internals](https://frankenphp.dev/docs/internals/)
- [FrankenPHP performance guidance](https://frankenphp.dev/docs/performance/)

An open upstream issue reports that request-parsing warnings can escape outside
the worker callback and produce a bogus response plus worker restart. It was
not reproduced in this PMS run and is therefore a **hypothesis to fault-inject**,
not a local finding: [FrankenPHP issue #2631](https://github.com/php/frankenphp/issues/2631).

## Product consequence of removing FastCGI

FACT: persistent Laravel is safe only through a qualified lifecycle adapter;
0.4 already disproved a generic reset for arbitrary PHP applications.

INFERENCE: removing FastCGI does not make a persistent protocol a universal PHP
compatibility boundary. Hamster must either define PHP as an opt-in
persistent-adapter workload, supply a different fresh-request execution mode,
or direct incompatible PHP applications to an external server through the
general reverse-proxy capability. Calling all three "native PHP" would hide
material lifecycle and failure differences.

HYPOTHESIS: an embedded or supervised native SAPI could offer fresh request
semantics without FastCGI, similar in purpose to FrankenPHP classic mode. The
benchmark shows the likely bootstrap cost on a light Laravel route but does not
answer whether that compatibility surface is required for Hamster 1.0. This is
a product/architecture question, not a reason to restore FastCGI implicitly.

## Candidate runtime directions exposed by this evidence

### 1. Keep supervised NTS workers and evolve the contract

Current Hamster already is a native persistent application runtime in the
sense that it owns HTTP, dispatch, protocol, lifecycle and supervision; it is
not native in-process PHP.

INFERENCE: this remains the evidence-backed reference architecture. It has a
narrow failure domain and near-FrankenPHP normalized throughput on the tested
route. Its known costs are IPC/copying, per-process memory and buffered bodies.
Those costs should be measured and attacked individually before replacing the
fault boundary.

### 2. Supervised native runtime hosts with several interpreter threads

A middle design could run a small number of separately supervised runtime-host
processes, each containing a bounded number of ZTS interpreter/worker threads.
It might share more runtime memory and reduce IPC crossings while limiting a
crash to one shard rather than the whole server.

HYPOTHESIS: this may offer a better performance/resource/isolation frontier
than either one NTS process per worker or one all-in-process PHP runtime. It is
also substantially more complex: thread-safe extension compatibility,
per-thread environment state, host restart, request ownership and ambiguous
failure after native calls all need proof.

### 3. Embed one ZTS runtime directly in Hamster

HYPOTHESIS: direct embedding could reduce copies and enable direct request/body
integration. The benchmark provides no controlled evidence for the size of
that gain. It would couple Rust HTTP, every PHP worker and native extensions in
one crash/allocator/ABI failure domain and make Windows PHP packaging part of
Hamster's core release surface.

INFERENCE: this is not currently justified as the default architecture.

### 4. Morph or adaptive topology

FACT: the experiment supplies no time-series workload, queue telemetry,
resource-pressure data, cold-start distribution or safe deoptimization result.

INFERENCE: it provides no new evidence for Morph. Worker-count normalization
and fault-domain research must precede adaptive topology.

## Next investigation

The earlier broad candidate matrix is superseded by the gated plan in
[`PERSISTENT_WORKER_COST_INVESTIGATION_PLAN.md`](PERSISTENT_WORKER_COST_INVESTIGATION_PLAN.md).
Its initial authorization requires no runtime instrumentation: direct IPC,
trivial end-to-end worker, and two lightweight PMS routes at W1/C1 and W4/C16,
with only minimal measurement-driver validation if required. It adds profiling
or a controlled FrankenPHP comparison only if a material cost remains
unexplained. Database saturation, cold-cache behavior and lifecycle
qualification remain distinct questions rather than extra cells in the core
runtime benchmark.

## Technical judgment

FACT: this is strong real-application evidence that retaining Laravel bootstrap
is valuable on lightweight PMS routes. It also confirms that classic-mode
tuning can materially reduce, but not erase, repeated-bootstrap cost.

INFERENCE: the result supports a native **persistent application runtime
product direction**, especially now that FastCGI is outside the plan. It does
not select the runtime's address-space or thread/process architecture.

INFERENCE: the small normalized throughput range, which straddles Hamster
rather than showing a stable FrankenPHP advantage, together with Hamster's much
stronger lifecycle/failure evidence, argues for treating the current supervised
worker as the reference baseline rather than sunk-cost code or a final design.
The next decision should be made on absolute decomposed cost, controlled
placement, private-memory efficiency and failure-domain evidence—not headline
RPS or worker/classic speedup.

HYPOTHESIS: the best long-term point may be a sharded native runtime host, but
there is presently insufficient evidence to prefer it over an evolved
subprocess protocol. A single embedded PHP runtime is the least justified
candidate under Hamster's stability and reliability priorities.
