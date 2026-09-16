# Persistent Worker Cost Investigation Plan

Date: 2026-09-16

Status: proposed investigation only. This document does not select embedded
PHP, a sharded runtime host, Morph, FrankenPHP's architecture, or any other
worker design. It does not change the roadmap.

## Decision this investigation must support

Identify where the current Hamster persistent-worker path spends wall time,
CPU, and committed memory, then decide whether the next useful investment is:

1. a local optimization in the current Rust/PHP path;
2. application or Laravel-adapter work;
3. pool/admission tuning;
4. research into a different runtime placement; or
5. no worker-architecture work because the remaining opportunity is too small
   for its reliability cost.

This is not a product race against FrankenPHP. A different architecture is
interesting only when a controlled measurement locates a material cost that
the current architecture cannot remove simply and safely.

## Existing facts and why the investigation starts small

The canonical external benchmark input is
[`doc/FRANKENPHP_BENCHMARK.md`](../FRANKENPHP_BENCHMARK.md). It supersedes the
earlier separate FrankenPHP benchmark reports.

FACT: `benches/php_ipc.rs` already has a production-format timeline benchmark.
It separately records Rust encode/write/decode, combined transfer/wait, PHP
decode/handler/encode/write, total latency, and parent/child CPU. Replacing it
before using it would duplicate measurement machinery.

FACT: the earlier Windows blocking-pipe defect was visible as latency near the
15.625 ms timer quantum. After the production pipe fix, direct IPC P50 was
approximately 68 us. This proves that direct IPC can expose a large transport
defect, but it does not measure pool admission, HTTP, Laravel, or response
delivery.

FACT: the final Hamster PMS `/login` evidence reports about 302 us of Rust CPU
and 3,290 us of PHP CPU per request at W4/C16. Its observed capacity was about
233 requests/second/worker, equivalent to about 4.30 ms of service interval per
worker.

FACT: at C1, the consolidated FrankenPHP worker-versus-classic results save an
absolute 23.1-58.8 ms/request across five PMS routes, while the ratios range
from 11.0x on `/login` to 1.3x on the heavy expense report. That is evidence
about avoiding repeated application bootstrap, not a measurement of Hamster's
runtime overhead.

FACT: coarse worker normalization does not reveal a stable FrankenPHP placement
advantage. Its twelve-worker `/login` cells imply about 4.47 ms/worker at C20
and 4.10 ms/worker at C50; Hamster's four-worker C16 cell implies about
4.30 ms/worker. The comparisons differ in PMS revision, PHP build, worker
count, concurrency, process topology, environment, compression and harness.
The small difference changes sign between the available FrankenPHP cells and
cannot isolate native placement.

FACT: the expense-report route plateaus around 15.4-15.7 RPS in FrankenPHP
worker mode from C5 to C20 while latency grows sharply. This is useful evidence
of application/backend saturation, but it cannot locate Hamster transport cost.

FACT: the consolidated report also separates first-request and warm behavior.
It observes differences as large as 65x, but attributes the fully cold cases
to application/Redis state only by inference because the shared Redis cache was
not cleared. Cold behavior must not be averaged into steady-state runtime cost.

FACT: Hamster reports several PHP processes separately, while the supplied
FrankenPHP memory number is one working set containing Caddy, the Go runtime,
threads, classic capacity, and persistent workers. These memory figures are
not comparable.

INFERENCE: the existing evidence gives no reason to begin with an embedded PHP
prototype. It first calls for a bounded accounting of Hamster's current cost.

## Cost model

The investigation treats a completed request as the following buckets. It
must not label an unexplained residual as "IPC".

| Bucket | Primary measurement |
|---|---|
| HTTP accept/read, body collection, routing | Rust monotonic duration and CPU |
| Pool admission and queue wait | enqueue-to-dispatch duration |
| Dispatch and worker selection | Rust monotonic duration |
| Protocol request encode | existing Rust timeline |
| Pipe write and worker wake-up | existing Rust timeline |
| PHP frame decode and request reconstruction | PHP monotonic duration |
| Laravel request handling | PHP monotonic duration |
| Octane terminate/cleanup and response staging | PHP monotonic duration |
| PHP response encode and pipe write | PHP monotonic duration |
| Rust read, decode, response construction and copies | Rust monotonic duration |
| HTTP response delivery to the client | Rust/client duration |
| Worker bootstrap/recycle/recovery | separate event cost, never amortized invisibly into the steady-state number |

Queue wait is demand-dependent, not execution cost. Bootstrap and failure cost
are lifecycle events, not steady-state request cost. They are reported beside,
not blended into, the request-stage medians.

Timestamps from Rust and PHP are not subtracted from each other because their
clocks have no established common origin. Each process reports durations from
its own monotonic clock, correlated by request ID. End-to-end residual is
reported explicitly and remains unattributed until an experiment isolates it.

## Minimal experiment ladder

Each gate can stop the investigation. Later gates run only if an earlier gate
leaves a material and actionable uncertainty.

### Gate 0 — freeze the comparison inputs

Before measuring, record:

- Hamster commit/diff and release binary hash;
- exact PMS scratch-copy commit, Composer lock, Laravel/Octane versions, route,
  database snapshot, `.env`-relevant settings, caches, OPcache/JIT and PHP ini;
- PHP executable/build/extension list;
- OS, CPU topology, power mode, available memory, and competing processes;
- worker count, concurrency, warm-up, measured duration, request count, and
  load-generator version;
- expected response status, length and digest for every workload definition.

Record three independent temperature dimensions rather than one ambiguous
"warm" label: worker/application bootstrap, OPcache, and external application
cache (Redis/database). Gate 1 measures fully warmed steady state. Preserve
time-to-ready and the first request to each worker as separate observations,
but do not include them in steady-state percentiles. Do not clear a shared
Redis or database merely to manufacture a cold cell; isolating fully cold
application-cache behavior would require a scratch backend and is outside the
minimal runtime-cost experiment.

The load generator and server remain on the same machine because that matches
the existing evidence, but this is stated as a CPU-contention limitation.
Candidate order is alternated between trials. Every reported steady-state
cell uses at least three fresh server instances, enough requests to run for at
least ten seconds after every worker is warm, and medians plus full run ranges.
An effect smaller than twice the run-to-run spread is inconclusive.

The existing .NET drivers are closed-loop: a lane starts its next request only
after its previous response. FACT: they can measure completion capacity and
client-observed latency for these cells, but they cannot characterize overload
tail latency without coordinated-omission bias. Gate 1 therefore makes no
overload-tail claim. If queue behavior becomes the suspected bottleneck, a
fixed-rate driver belongs in Gate 2.

All responses must have status and length checked. A body digest is established
before and after every run and sampled during a run; digesting every response
inside the timed loop is permitted only after its client-CPU cost is measured.
Client CPU is recorded separately and is never included in server CPU
efficiency.

### Gate 1 — account for Hamster without runtime instrumentation

No production-runtime instrumentation or architecture change is authorized
here. Existing IPC and load machinery is reused. Small measurement-only driver
changes may validate response length/digests and collect private/system commit,
because the current drivers do not provide all of those fields.

#### 1A. Direct protocol/pipe lower bound

Run the release Windows MSVC `php_ipc` timeline and raw/variant suites with the
existing production pipe configuration. Preserve their raw output. Use the
available payload sizes around empty/small, 16 KiB (close to PMS `/login`), and
the current large-payload case.

Questions:

- Is the old timer-quantum signature absent?
- How much CPU and wall time belong to framing, copying and wake-up when the
  PHP handler does almost no work?
- Does cost grow linearly with response bytes or show a discontinuity?
- Is parent plus child CPU consistent with wall time at W1/C1?

This benchmark is a lower bound only. It cannot prove end-to-end cost.

#### 1B. HTTP plus pool lower bound

Use the existing trivial persistent fixture through the actual HTTP server and
pool. Measure W1/C1 first, then W4/C16. Run both the small hello and JSON paths.
Collect throughput and latency percentiles, Rust and total PHP CPU deltas,
private committed bytes, working set, handles, threads, and context switches.

W1/C1 minimizes queueing and exposes per-request machinery. W4/C16 shows the
same path under the production-style pool scheduler. The difference between
them is not automatically attributed to the pool; CPU saturation and client
contention must be checked.

#### 1C. Application cost ladder

On one immutable PMS scratch copy, use the existing scratch-only controller
fixture and run only:

1. a minimal controller JSON response with no database work;
2. the measured uncompressed `/login` response (19,059 bytes in this Gate 0
   scratch; do not hard-code the historical 19,160-byte observation).

Measure W1/C1 and W4/C16. These two routes are the complete core Gate 1 PMS
matrix. The consolidated FrankenPHP evidence already demonstrates that a heavy
database route can saturate the application/backend while hiding runtime cost.
Do not repeat that route to locate Hamster overhead. One database route may be
added later only as an application-saturation control; it cannot trigger a
runtime-architecture change by itself.

For each cell calculate:

```text
worker_service_interval = workers / saturated_rps
cpu_per_request          = cpu_seconds_delta / completed_requests
cpu_efficiency           = completed_requests / combined_cpu_seconds
memory_efficiency        = saturated_rps / system_commit_delta_gib
```

Primary comparisons are absolute: microseconds of wall/CPU per request and
bytes of committed memory per worker or unit of useful capacity. Ratios are
secondary presentation. A large speedup ratio caused by removing repeated
bootstrap is not recorded as Hamster runtime overhead.

Use the isolated machine's system commit-charge delta as the topology-neutral
primary resource number. Also report the sum of process-tree private bytes and
working set: private bytes help attribute ownership, while working set is a
secondary locality number. Summed child working sets are not compared with a
monolithic process working set as if they represented equal ownership.

At matched W1/C1, the difference between the minimal Laravel controller and the
trivial PHP worker is only a **combined** framework-routing, middleware and
Octane-lifecycle budget. It is not assigned to Laravel application code or to
Hamster's adapter without Gate 2 timing. If this combined budget is material,
that alone justifies Gate 2 even when Rust and pipe cost are small.

#### Gate 1 stop rule

Stop and retain the current architecture when all of the following hold:

- the entire small-response trivial-worker service interval (an upper bound on
  its generic HTTP/pool/protocol/fixture path) is no more than roughly ten
  percent of the lightweight PMS worker service interval;
- on the approximately 19 KiB PMS route, all Rust CPU plus the measured synthetic PHP
  protocol cost is no more than roughly ten percent of combined server/PHP
  CPU; this intentionally over-attributes Rust work against the architecture;
- the direct payload curve and aggregate PMS Rust CPU show no material
  size-dependent anomaly, and W1-to-W4 shows no scheduling discontinuity;
- the combined minimal-Laravel lifecycle/framework budget is below ten percent
  of the `/login` service interval; otherwise run Gate 2 rather than labelling
  it application cost;
- memory per additional worker is stable and operationally acceptable; and
- the existing lifecycle/failure suite remains clean.

The ten-percent value is a decision band, not a claim of measurement
precision. If variance overlaps it, the result is inconclusive, not a trigger
for a new architecture. Passing this stop rule means the current overhead is
below the investigation's investment sensitivity; it does not claim that
every PHP adapter stage was individually measured. PHP application versus
Octane-adapter cost remains combined unless Gate 2 is justified.

### Gate 2 — add profiling-only boundaries if the residual is material

Run this gate only when Gate 1 leaves more than about ten percent of service
time or CPU unexplained, or exposes a nonlinear payload/pool effect.

The smallest future code change would add stage timing behind the existing
`profiling` feature. It would not change the default build or wire protocol.
The measurement should emit per-stage histograms or sampled records keyed by
request ID for:

- request accepted/body complete;
- pool enqueue/worker dispatch;
- request encode/write complete;
- response read/decode complete;
- response handed to HTTP and body delivery complete;
- PHP decode/reconstruction, Laravel handle, terminate/cleanup, encode/write.

Measure durations locally in each process. Do not add synchronous hot-path
logging. First compare a release profiling build with collection disabled and
enabled against the normal release binary. If observer overhead exceeds three
percent or the normal run spread, reduce sampling or discard the method.

Gate 2 ends with a budget table whose rows sum to measured server time, plus a
named residual. It does not end merely with a flame graph or total RPS.
If queueing is material, Gate 2 also adds a fixed-rate offered-load sweep below,
at, and above measured capacity; closed-loop latency is not substituted for
this experiment.

Action follows the bucket:

| Dominant measured cost | First research target |
|---|---|
| pool queue/dispatch | admission and fixed-pool scheduling, not PHP placement |
| protocol encode/copy/write/read | current protocol implementation and buffer ownership |
| PHP reconstruction/Octane cleanup | Laravel adapter lifecycle |
| Laravel handler | application/package/database profiling |
| Rust HTTP response copies | response representation/body handoff |
| per-process private commit | worker process contents and marginal-worker model |
| no dominant Hamster bucket | stop architecture work |

### Gate 3 — controlled FrankenPHP placement comparison

Run this only if Gate 2 identifies a meaningful residual plausibly caused by
the subprocess boundary, or if marginal worker memory remains the decision
problem. This gate tests a hypothesis; it does not choose FrankenPHP.

The controlled cells are persistent Hamster versus persistent FrankenPHP only.
Classic mode may be retained as an application-bootstrap control, but it is
not evidence about placement.

Controls:

- same immutable PMS copy and database snapshot;
- same Laravel/Octane/Composer lock, route, caches, environment and PHP
  settings to the extent each runtime permits;
- separately recorded worker, OPcache and external application-cache
  temperature; only like-for-like warm steady-state cells are ranked;
- separate FrankenPHP worker-only config/process, with classic capacity and
  unrelated listener removed;
- W1/C1 and W4/C16 before any W12/C50 scaling run;
- same client, HTTP version, response validation, warm-up, duration and
  alternating trial order;
- access logging, compression and optional middleware either disabled on both
  or explicitly accounted for;
- combined server/runtime CPU, private commit, working set, peak commit,
  system commit-charge delta, handles/threads/processes and context switches;
- lifecycle failures measured separately from clean steady state.

PHP NTS in Hamster versus ZTS in FrankenPHP is an unavoidable architecture
bundle confound unless compatible builds exist. Record it; do not claim that
the experiment isolates address-space placement perfectly. PHP-side stage
timings help distinguish application work from the bundle effect.

Compare:

- absolute wall-time and CPU microseconds per request before speedup ratios;
- throughput per consumed CPU core;
- service interval per worker at matched worker count;
- latency at matched offered load and at each system's saturation point;
- idle and saturated private commit above baseline;
- marginal private commit from W1 to W4;
- response-size sensitivity;
- bootstrap, recycle and failure blast radius.

#### Gate 3 interpretation

- At most 10 percent improvement in throughput/CPU with no clear memory or
  reliability gain: current worker architecture is adequate; stop.
- 10–25 percent repeatable improvement: optimize the identified current-path
  bucket first. A runtime prototype needs additional evidence.
- More than 25 percent repeatable improvement, or a large marginal-memory
  advantage, with a located causal bucket: a bounded alternative-runtime
  prototype becomes reasonable research.

These are investment bands, not product acceptance thresholds. The measured
effect must also exceed twice the run spread. A faster result that weakens
at-most-once, cleanup failure handling, or worker isolation is rejected rather
than discounted into a score.

## Reliability and stability companion tests

Performance runs remain clean; fault tests are separate. They answer whether
an apparent efficiency gain merely moves work or risk into failure paths.

For the current Hamster configuration, and for FrankenPHP only if Gate 3 is
reached:

1. Run W1 isolation with authenticated then anonymous requests on the same
   generation.
2. Inject a PHP fatal exit before request receipt and after request receipt;
   verify recovery and no automatic replay of a possibly-started request.
3. Inject terminate/cleanup failure; verify the generation cannot silently
   return READY.
4. Recycle at a deliberately low request count under sustained load; measure
   capacity dip, p99/p99.9, failed requests, process/handle count and orphans.
5. Run a fixed-rate soak below saturation long enough to cross many recycle
   generations; sample private commit, working set, handles and per-worker
   request counts at intervals.

The soak has a concrete pass question: after warm-up and excluding active
recycle windows, do memory/handle baselines form a bounded band rather than a
positive trend, and do all retired processes disappear within the shutdown
bound? A long run without this analysis is not evidence.

At-most-once is checked with a durable per-request side-effect marker, not only
HTTP response counts. Orphans are checked from the process tree after normal
shutdown, forced worker exit, and server termination.

The Octane client ordering difference remains part of interpretation:
Hamster stages a response until lifecycle cleanup succeeds, while the inspected
FrankenPHP/Octane path can send before terminate callbacks complete. A raw RPS
comparison cannot treat those semantics as equal reliability.

## Hypotheses to falsify

HYPOTHESIS H1: framing, pipes and wake-up are already a small fraction of PMS
service time. The direct timeline plus Gate 2 can falsify this.

HYPOTHESIS H2: most W4/C16 latency above the W1 service interval is queueing
behind serialized per-worker service, not Rust transport. Matched W1/C1 and
enqueue-to-dispatch timing can falsify this. Existing capacity-derived service
intervals are not treated as measured Laravel-handler time.

HYPOTHESIS H3: response copying becomes actionable only as payload size grows.
The payload curve can falsify a constant-overhead explanation.

HYPOTHESIS H4: Hamster's multi-process topology has higher marginal private
memory than an in-process threaded runtime at equal useful capacity. Only
worker-only FrankenPHP and private-commit deltas can test it.

HYPOTHESIS H5: the existing small, direction-changing normalized difference
between Hamster and FrankenPHP is configuration/load/variance rather than
native placement. The matched Gate 3 cells can falsify this.

HYPOTHESIS H6: a fixed supervised process pool is already the best complexity/
reliability trade-off for current workloads. This survives unless a repeatable
cost bucket and a safer or equally safe alternative are demonstrated.

## Required artifacts

Every executed gate should produce a reproducible evidence bundle containing:

- exact commands and configs, source/PMS revisions, binary hashes and version
  output;
- raw per-run CSV/JSON and unaggregated latency samples;
- response validation results;
- process-tree and resource samples;
- stage definitions and profiling observer-overhead check, if Gate 2 runs;
- median, range, and derived budget calculations in a checked-in report;
- failures and rejected runs, not only the best result.

`doc/BENCHMARKS.md` receives only completed, reproducible measurements. This
plan itself is not benchmark evidence.

## Smallest owner-selectable execution

The recommended authorization is Gate 0 plus Gate 1 only. It requires no
runtime change, only reproducibility/config artifacts and any minimal
measurement-driver validation described above. It can already conclude that
the current architecture is good enough or name the unresolved budget. Gate 2
is a separate profiling change. Gate 3 is a separate controlled
external-runtime experiment. No embedded PHP, sharded host, Morph, protocol
change, or roadmap change is required to answer the first question.
