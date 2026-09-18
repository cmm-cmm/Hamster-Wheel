# AI Handoff

## Closed Scope — Multiple Persistent Application Pools

The owner closed the implementation scope on 2026-09-18. Multiple
`php_worker` locations are implemented in the working tree; they are not yet
committed, tagged or released. The former validation guard was removed only
after orchestration gained transactional ownership semantics.

FACT: every location compiles to a distinct `Arc<PhpWorkerPool>` and keeps its
own processes, queue, supervisor, spawn gate, slot/generation state, health and
recovery counters. Request dispatch, recovery and worker lifecycle paths are
unchanged. The one intentional cross-pool resource is the process-wide
`BodyBudget`, because it bounds Rust process memory rather than an application.

FACT: startup policy is all-or-nothing. `RuntimeConfig::start_workers` starts pools
in configuration order and, on failure, shuts already-started pools down in
reverse order before returning. `server::run` also converges every pool on its
startup-error path and never binds the listener first. There is deliberately no
degraded startup or startup retry.

FACT: `shutdown_workers` attempts every pool before returning an aggregate error.
`PhpPoolsError` keeps the initiating pool failure as its error source and lists
cleanup failures as additional, pool-labelled evidence. The same causal rule
now covers partial startup inside a pool when one worker reaches READY and
another fails.

FACT: nine permanent ignored regressions in `tests/multipool_probe.rs` cover healthy
multi-pool routing, cross-pool crash isolation, partial-startup rollback,
startup failure before bind, best-effort shutdown after an earlier pool fails,
multiple simultaneous shutdown failures, bind failure plus cleanup failure,
cross-pool body-budget ownership, PID reaping, and primary-versus-cleanup
diagnostics. The test-only
`record_pid_worker.php` records exact child PIDs, avoiding repository-wide
process counts that can be contaminated by parallel tests.

FACT: the final falsification pass found no new production correctness defect.
The nine-test multi-pool suite, 92-test default suite, formatting, strict
clippy and Windows MSVC release build passed; no repository PHP process
remained. Linux was not natively verified. PMS multi-pool verification was not
run because the prepared external environment was unavailable.

Known limitations, not closure blockers without new evidence:

- FACT: pool startup and shutdown are sequential; shutdown worst-case can
  scale with pool count.
- FACT: 64 workers is a per-pool limit; there is no process-wide worker or
  pool ceiling.
- FACT: direct cancellation/abort of the startup future and concurrent
  lifecycle calls are not a qualified public contract.
- FACT: PMS multi-pool was not verified because the prepared external
  environment was unavailable.
- FACT: Linux was not natively verified; Windows evidence cannot be promoted
  to a Linux support claim.
- HYPOTHESIS: concurrent shutdown or a global ceiling may become useful at a
  deployment scale not represented by current evidence. Do not implement them
  pre-emptively.

INFERENCE: the verified ownership and failure paths meet this slice's contract;
the limitations above do not invalidate it. See the authoritative record in
`doc/MULTIPLE_PERSISTENT_APPLICATION_POOLS.md`.

## Previous Milestone — Pool Identity + Degradation Visibility

Pool Identity + Degradation Visibility is implemented in the working tree for
owner review, on top of the closed Execution Timeout Policy slice. It is not
committed, tagged or released.

The investigation challenged its own premise and the premise was half wrong.
At that milestone only one `php_worker` location was permitted, so pool identity
had no immediate disambiguation value and the label was added in preparation
for a later multi-pool capability. That statement is historical: the current
contract permits multiple independently owned pools. The real defect
was the opposite of the stated one. Degradation was already *over*-reported at
three stderr lines per event, while recovery was reported not at all: a
measured run ended on four consecutive failure lines while the pool was serving
`200 200 200 200 200`. Worse, the one aggregate `PoolSnapshot` exposed,
`ready_workers`, is not a health signal -- `mark_busy` decrements it, and it
read zero in 20% of samples on a four-worker pool that served 23,072 requests
without a single failure. The counter that answers the question, `live_workers`,
was not exposed at all despite being what `execute` gates its 503 on.

The slice therefore exposes `live_workers`, documents `ready_workers` as spare
capacity, names every pool-scoped diagnostic (including drained application
stderr) after its pool, and reports an outage and its recovery as a matched
pair once the pool has had no live worker for longer than
`POOL_OUTAGE_NOTICE_AFTER` (1 s, about 2.5x the worst measured bootstrap of
403 ms). Ordinary replacement churn produces no outage lines. A first
implementation that reported every drop to zero produced seven alarms against
two all-clears, which was the same asymmetry in a new form and was fixed.
Evidence: `benchmarks/results/pool_visibility_20260917.md`.

Known limitations carried out of this slice:

- FACT: `live_workers` is only as fresh as the last dispatch. An idle worker's
  death is detected passively on its next assignment, so an idle pool reports
  workers that are already gone and the outage notice cannot fire for one.
- FACT: partial degradation is not reported. A pool serving on two of four
  workers still answers every request; the measured user-visible failure was
  total refusal. A partial-capacity threshold would need its own evidence.
- FACT: a per-request failure still emits its own line, so a failing generation
  produces one line per queued request on top of the two slot lines. Reducing
  that duplication was left alone as outside the scope.
- HYPOTHESIS: the 1 s threshold is derived from bootstrap times measured on
  this host with this fixture and the recorded Laravel figures. A materially
  slower application bootstrap would make ordinary replacements visible as
  outages.

Execution Timeout Policy is **closed by owner decision**. It is implemented in
the working tree on top of the uncommitted 0.6.x response work and is not
committed, tagged or released. Do not extend or reopen this slice unless new
evidence requires it. An application execution
timeout is now its own failure class: it still kills and reaps the worker and
its request is still never retried, but it no longer increments the slot's
infrastructure failure history, so repeated slow requests cannot walk a pool
into the replacement backoff ladder or its 30-to-300 second cooldown band.
`request_timeout` is configurable per `php_worker` location, 1 to 3600 seconds,
defaulting to the historical 5. Fault injection measured the failure this
fixes: eleven paced slow requests put a one-worker pool into the cooldown band
and refused ordinary traffic for 30 151 ms, while 52 of 52 timeout-driven
replacements reached READY, so the ladder never prevented anything. Mixed
traffic with headroom never degraded (629 of 629 ordinary requests served). The
HTTP status is `502` with the body `PHP Worker Execution Timeout`, by owner
decision: the request is possibly-started, so Hamster must not emit a status
that encourages an external retry it cannot make safe. Evidence:
`benchmarks/results/worker_request_timeout_investigation_20260917.md` and
`benchmarks/results/worker_request_timeout_floor_20260917.md`. The policy, what
deliberately did not change, and the rejected alternatives are in
`doc/EXECUTION_TIMEOUT_POLICY.md`.

Known limitations carried out of the slice, and the hypotheses it did not
test:

- HYPOTHESIS: the same escalation still applies to every failure that happens
  after the REQUEST frame is written -- I/O and protocol faults -- because they
  share the `GenerationOutcome::Failed` path. The slice deliberately left their
  recovery policy unchanged, and no probe drove them at rate.
- HYPOTHESIS: a real Laravel bootstrap is 58-403 ms against the fixture's
  ~60 ms, so the sustained-churn duty cycle in production is higher than the
  measured G1 worst case. The arithmetic is in the floor derivation; it was
  never measured against a real application because that needs
  `HAMSTER_LARAVEL_TEST_PATH`.
- FACT: five seconds is a compatibility default and nothing more. Nothing in
  this repository establishes it as correct for any application.
- FACT: `tests/timeout_probe.rs` is historical instrumentation, not a
  regression suite. All of it is `#[ignore]`d and several probes take minutes.
  The four permanent regressions live in `tests/php_worker.rs`.

0.6.2 — Response Liveness is implemented in the working tree for owner review,
on top of the uncommitted 0.6.1 streaming response. A lazy per-connection timer
now covers only a backpressured socket write with zero byte progress; it does
not time request reads, keep-alive idle, application execution or total response
duration. The two-second release probe returned the sole admission slot in
2008.3 ms, while a progressing 36 KiB/s client survived three timeout
intervals. See `doc/RESPONSE_LIVENESS_0_6_2.md` and
`benchmarks/results/response_liveness_0_6_2_20260916.md`.

0.4.0 — Laravel Persistent Worker. The investigation and Octane adapter are
committed on `dev` at `6e961d5`. They are not tagged or released, and milestone
closure remains an owner decision. Findings, measurements and rejected
alternatives are in `doc/LARAVEL_WORKER_0_4.md`.

0.3.1 is closed: `v0.3.0` and `v0.3.1` are tagged on `dev` and the retirement
hardening is committed as `616e4ee`. The accepted lifecycle contract remains
`doc/WORKER_LIFECYCLE_0_3.md` and 0.4 reuses it unchanged. Milestone 0.2.0 is
closed; its transaction-safety invariants remain mandatory.

## Current Status

Identity, lifecycle supervisor, automatic replacement, per-slot backoff,
optional recycling, shutdown coordination, telemetry, config validation, and
the accepted 18-test lifecycle matrix are implemented. The three old tests
that encoded permanent no-respawn behavior were explicitly reclassified as
0.3-superseded lifecycle tests and updated. Independent review closed with
ACCEPT and no release blocker; its non-blocking follow-ups are in the backlog
below.

## Verified Platform

Windows `x86_64-pc-windows-msvc` with local PHP 8.5 NTS at
`php/8.5/php.exe`. Do not claim native Linux verification. A Windows-host
cross-target check is COMPILE-CHECKED ONLY.

## Architecture Summary

The fixed-size pool has stable slots. Each replacement increments that slot's
generation; PID is process metadata only. A generation task exclusively owns
one `PhpWorker`, process, pipes, protocol state, and capacity-1 mailbox.
`IdleWorker { slot, generation, mailbox }` is the dispatcher capability, so a
stale token naturally returns `Closed` and is the sole safe redispatch case.

The lifecycle supervisor owns the generation `JoinSet`, replacement/backoff
work, dispatcher coordination, and shutdown. `idle_tx` belongs only to a live
generation or pending replacement; the supervisor has no permanent keeper.
Unexpected failure schedules an immediate first replacement, then 100, 200,
400, 800, 1600, 3200, and capped 5000 ms delays. More than 10 consecutive
failures moves the slot into a bounded cooldown band (30 s doubling to a 300 s
ceiling); no slot is ever permanently retired. Stability reset requires 15
seconds READY, measured from READY rather than from spawn, plus a successful
request. Stability is deliberately time based: request-count stability would
make the respawn rate proportional to throughput.

## Critical Invariants

- Never retry once PHP may have started REQUEST.
- Client cancellation never cancels a worker transaction.
- Consume RESPONSE or handle worker failure deterministically.
- One task owns one generation; no worker/global execution mutex.
- Protocol v1 and request/frame/header limits remain unchanged.
- Admission is bounded and non-blocking.
- Zero READY is transient: a slot always has a live or pending generation, so
  `idle_rx` no longer closes through exhausted retries. Admission still refuses
  immediately on `live_workers == 0` rather than blocking on distant capacity.
- Once shutdown enters ShuttingDown, the spawn gate permanently forbids new
  PHP processes.
- Dispatcher converges before workers receive shutdown; every child is reaped.

## Known Limitations

Idle worker death is detected passively on its next assignment, but a request
that finds a dead worker is now queued again rather than failed, because a
process reaped before the frame write proves PHP never received it. There is no
proactive stdout probe, adaptive sizing or memory-threshold recycle.
Byte-based admission now exists: the
`server.max_body_memory` budget bounds request-body bytes buffered process-wide
and refuses over-budget requests with 503 before pool admission, with no
waiting. It defaults to 64 MiB, reports its configured value at startup and its
refusals on standard error at most once per minute, and `0` is an explicit
unlimited opt-out. Static response bodies now stream in bounded 128 KiB frames;
32 slow clients serving 256 MiB files used 9.39 MiB additional private memory.
PHP responses remain buffered protocol-v2 frames; their response-frame layout
is unchanged. `header_read_timeout` still
does not cover response writes; `write_progress_timeout` now releases an active
response after its socket write makes zero progress for a configured interval.
This is deliberately not a total-duration or minimum-rate policy, so a peer
that makes deliberate trickle progress can still hold a slot indefinitely.
Graceful shutdown retains its separate finite drain deadline. Charging a
declared `Content-Length` before reading also lets a slow
client hold budget it never fills; four such clients made every other POST 503
for `body_read_timeout` while GET kept serving. Both are documented in
`doc/CONFIGURATION.md` and `doc/HTTP_TRANSPORT_0_5.md` and measured in
`benchmarks/results/review_0_5_20260916.md`. Permanent slot
retirement no longer exists: prolonged
failure settles into a 300-second cooldown ceiling instead. HTTP bodies
still buffer before pool admission. `max_requests` and `max_worker_age` default
OFF; recycling is stop-then-spawn and permits one voluntary recycle at a time.
Both recycle policies are thresholds rather than caps; see the recycling
limitation in `doc/PHP_WORKER.md`. Laravel request ownership is implemented in
0.4.0 with Octane's sandbox/lifecycle plus a narrow Hamster resource boundary.
`multipart/form-data` with file uploads remains bounded by the protocol's 1 MiB
body limit.

## Current Findings

Windows quality gates: `fmt --check`, clippy with `-D warnings`, the default
suite (34 unit, 16 configuration, 4 routing, 5 static), a release build and
`cargo bench --no-run --features profiling` all pass. The final Octane
regression pass ran the complete ignored suite in its default parallel mode:
eight PHP pool unit tests and 47 process/HTTP integration tests passed. It
includes the accepted worker lifecycle and 0.2 cancellation/shutdown
regressions, the 100,000-request pool-level soak, a 10,000-request
forced-failure HTTP soak, the 60-second W4 continuous-load random-kill stress,
and all Laravel/PMS paths. A new real-PMS W4 regression forces all workers to
serve fresh controllers, injects one cleanup failure, and proves only that slot
advances generation before W4 recovers. No project PHP process remains after
the suite.

The first complete run exposed a deterministic harness regression: two
external Laravel tests omitted the scratch application's `PHPRC`, so repository
PHP lacked `mbstring`. The tests now derive the ini path themselves; both pass
with the parent shell's `PHPRC` removed and the complete matrix passes. This
was configuration dependence in the new tests, not a product race.

Protocol v1 is unchanged on the wire. `src/php_protocol.rs` differs from
`v0.2.0` only inside the old `write_frame` body, which now delegates to
`write_frame_tracked`; the constants, encoders and decoders are untouched and
`fixtures/php-worker/protocol.php` is byte-identical.

The Linux cross-target check could not be run on this machine: the
`x86_64-unknown-linux-gnu` target is not installed in this rustup toolchain.
Linux remains unverified, as before.

`ctrl_c_stops_server_and_reaps_pool` needs a real conhost console. Its
`GetConsoleCP` guard does not detect a ConPTY or pseudo-console session, so
under those terminals the test hard-fails at the five-second wait instead of
skipping. The same failure reproduces on the `v0.2.0` tree, so it is a test
harness gap, not a lifecycle regression. Ctrl+C was verified on a real Windows
console only.

The limited steady-state benchmark detected no greater-than-10% regression in
W1/C1, W4/C16, or W4/C32 for Hello or JSON. Several cells moved positively by
more than 30%, so host variance dominates and the data can only fail to detect
a regression, not prove there is none. Exact data and high-variance notes are
appended to `doc/BENCHMARKS.md`. Shutdown during replacement bootstrap
interrupts startup and kill/reaps the child rather than waiting for the
five-second READY timeout.

## Backlog

Non-blocking follow-ups from the independent 0.3.0 review, highest value first.
The first item is done on `dev` after the `v0.3.0` tag.

- DONE: the dispatcher drains idle capabilities continuously and discards
  closed mailboxes, so `max_worker_age` advances on a pool with no traffic.
  Regression tests are `max_worker_age_recycles_a_pool_with_no_traffic` and
  `dispatcher_drains_idle_capabilities_without_a_pending_job`; both fail
  against the `v0.3.0` tree.
- DONE: end-to-end coverage that the supervisor actually applies the delay
  ladder. `shutdown_during_prolonged_failure_cooldown`,
  `prolonged_failure_never_retires_a_slot_permanently` and
  `one_bad_slot_does_not_disturb_healthy_slots` drive a real slot past
  `MAX_CONSECUTIVE_FAILURES` and assert the resulting state.
- Strengthen the `ctrl_c_stops_server_and_reaps_pool` console guard so a
  ConPTY session skips instead of failing.
- DONE: "the process exited before REQUEST was written" is distinguished from a
  possibly-started failure. `PhpWorkerError::NotStarted` is returned when
  `try_wait` reaps the process first or when the frame write transfers zero
  bytes, and such a job is queued again once. `write_frame_tracked` reports the
  transferred count, which `write_all` cannot.
  `respawn_after_idle_crash_restores_target` now asserts a 200 and exactly one
  execution.
- DONE: `all_workers_crash_then_pool_recovers` is deterministic. It no longer
  asserts a particular status, which was never knowable, and instead asserts
  that every request is answered within ten seconds with 200, 502 or 503 and
  that the number of executions equals the number of requests served, with no
  duplicates. Five consecutive runs pass.
- Smaller debt: `WorkerState` and `WorkerLifecycleState` duplicate each other;
  the pre-request exit path drops the worker without awaiting its stderr
  drain; `total_respawns` also counts planned recycles; the dispatcher's
  full-mailbox branch lost the 0.2 `debug_assert`; `SpawnGate` holds a std
  mutex across `Command::spawn`.
- DONE: `IdleWorker` no longer derives `Clone`. One capability per generation
  is what bounds the capacity-1 mailbox to a single buffered job, and that
  bound was held only by the shape of `run_worker_generation`. Cloning is now
  a type error rather than a silent way to break it.

## 0.3.1 Retirement Hardening (committed at `616e4ee`)

`STABILITY_RESET_PERIOD` is 15 s (was 60 s), measured from READY.
`WorkerLifecycleState::Retired` and `retire_slot` are gone; `replacement_delay`
returns a `Duration` rather than `Option`, moving into `COOLDOWN_BASE` 30 s ..
`COOLDOWN_MAX` 300 s past `MAX_CONSECUTIVE_FAILURES`.

Verified on Windows with real php.exe: permanent bootstrap failure settles at
the 300 s ceiling using 0.08 s CPU over 423 s with stable RSS; a W4 pool driven
into the cooldown band on all four slots returned to W4 by itself 46 s after
the fault stopped; a generation living 20.1 s resets failure history while
generations killed faster still accumulate; one slot driven into cooldown left
the other three at zero failures and serving; shutdown during a cooldown
returns in under two seconds with no child left behind.

At-most-once semantics are untouched: no transaction-ownership code changed and
`possibly_started_request_is_never_retried` still passes. Protocol v1 is
byte-identical to `v0.2.0`.

Known test flakiness, not a regression: `ctrl_c_stops_server_and_reaps_pool`
fails under ConPTY and also fails that way on the unmodified tree.
`no_orphan_php_processes` failed once under a ConPTY run and passed four out of
four under a real console; it compares raw PIDs, and the new tests recycle PIDs
much faster. Repository php.exe count was zero after every run, so no orphan
was ever present.

## 0.4.0 Laravel Persistent Worker (implemented at `6e961d5`)

The independent review rejected the in-place targeted reset. Its
`warmInstanceKeys` snapshot made binding lifetime depend on warm-up resolution,
retained a bootstrap-resolved scoped binding, and could not see a controller
cached on a shared `Route`. More reflection/list entries would not establish a
real ownership boundary.

The PHP worker now requires Laravel Octane 2.x and uses its application factory,
per-request sandbox, manager rewiring and lifecycle listeners. Hamster supplies
only the protocol-v1 client and a narrow resource boundary: abandoned DB
transactions, Eloquent guard/strict statics, Carbon test time, cwd/default
timezone and worker-created upload temporaries. The old `none` and `targeted`
modes fail before READY.

Responses are staged in PHP until kernel termination, Octane cleanup and the
Hamster resource boundary all succeed. A terminating-listener fault injection
initially exposed that sending a synthetic 500 made Rust briefly mark an exited
generation READY. The final worker emits no frame on lifecycle failure: Rust
receives EOF for a possibly-started request, returns 502 without replay,
immediately marks the generation unavailable, boots generation 2 and returns
200 there. The handler marker remains exactly one.

Checked-in probes now cover bootstrap-resolved scoped state, scoped separation,
lazy versus warm singleton ownership, cloned config, cached controllers,
deferred providers, listener ordering, framework/runtime baselines, transaction
and upload cleanup failure, response commit ordering, and stdout descriptor
isolation. The real lifecycle test deliberately needs an Octane-enabled Laravel
app and W1. A PMS-specific checked-in test drives 50 authenticated/anonymous
pairs through the Rust server on one PID, checks cached-controller ownership,
then fault-injects termination and requires exact-once recovery.

The actual Octane 2.19.1 package was installed in a no-hardlink scratch PMS,
with package discovery and `artisan optimize`; no source injection was used.
The ownership suite passed 25 checks. A release same-HTTP/IPC fresh-bootstrap
comparison measured 1.51x on controller JSON, 1.49x on Blade login and 1.08x
on the remote-MySQL dashboard. A W1 soak passed 20,000 alternating auth/guest
requests on one PID and measured 0 B/request RSS slope over the second half.
Raw results and reproduction scripts are under `benchmarks/`.

The original PMS remains untouched and does not declare Octane. Its pinned
PhpSpreadsheet 1.30.6 declares PHP `<8.5`, so the scratch dependency install
needed an explicit platform-ignore. At 0.4 closure, protocol v1 still lacked
typed peer metadata and left `REMOTE_ADDR=127.0.0.1`; that historical decision
was subsequently resolved by strict typed protocol v2. See the current handoff
section below and
`doc/research/LARAVEL_LIFECYCLE_0_4_REVIEW.md`.

## Accepted 0.5-1.0 Production Spine direction

The old post-0.4 order is rejected: adaptive worker management and Morph no
longer precede basic compatibility and production operation. Hamster Wheel is a
general-purpose Web/Application Server; Laravel Octane remains confined to the
Laravel persistent-worker adapter.

The dependency order is:

1. 0.5 HTTP/transport identity, bounded request/response policy, streaming
   static consumer, minimum instrumentation and native Linux bring-up;
2. 0.6 FastCGI on Windows/Linux, multiple PHP backends and the explicit generic
   persistent-adapter contract;
3. 0.7 minimal reliable reverse proxy with streaming, backpressure, forwarding
   trust and an at-most-once retry boundary;
4. 0.8 transactional reload/drain, logs/metrics/health, Windows Service, systemd
   and operational CLI;
5. 0.9 TLS/SNI/certificate reload, optional HTTP/2 product gate and full release
   qualification;
6. 1.0 stable Web/Application Server contract.

No generic backend trait/plugin system is authorized in 0.5. FastCGI and proxy
must first provide real additional backend shapes. Adaptive sizing and Morph
are post-1.0 research unless production evidence changes their dependency.
`doc/ROADMAP_0_5_TO_1_0.md` contains milestone exits, alternatives, hypotheses
and owner decisions.

The first 0.5.x working-tree slice now preserves the immediate peer returned by
`accept()`, queries the accepted stream's concrete local endpoint, and passes
immutable `TransportInfo` through routing to every handler. It deliberately
does not copy request-owned HTTP version/authority into a second context, trust
forwarding headers, or alter protocol v1. A real-socket unit test covers a
wildcard listener, keep-alive, forged proxy headers and 32 concurrent clients.
See `doc/HTTP_TRANSPORT_0_5.md`. The change awaits owner review and has not been
committed or released.

## Next Action

Owner review of the Execution Timeout Policy slice, then of 0.6.1 Concrete
Streaming Response. The measured response-memory
failure is closed and no static or PHP performance regression was detected.
The pass also falsified the 0.5 inference that `header_read_timeout` reclaims an
active response write: a streaming slow reader remains active past it. Graceful
shutdown has its own bounded drain using that configured interval, but the
normal-operation response-write timeout policy remains a documented gap and is
not silently folded into 0.6.1.

Nothing in 0.6.1 is committed, tagged, pushed, merged or released. Do not start
the next 0.6.x slice until the owner decides closure.
Remaining 0.4 decisions and out-of-scope qualifications:

- Decide how the external PMS adopts Octane and resolves its PhpSpreadsheet
  PHP 8.5 constraint. The disposable scratch proves lifecycle compatibility,
  not that ignoring Composer platform requirements is a production policy.
- Decide whether correct client IP reconstruction is a 0.4 blocker, and if a
  reserved v1 metadata header or typed protocol-v2 field is the accepted
  contract. Do not infer it from an untrusted forwarded header.

- Raise the 1 MiB request body limit, or stream bodies, so uploads larger than
  that work. This remains a protocol change and needs a milestone that owns the
  current protocol v2 contract.
- Exercise the supplied application's own import endpoints. Not done here
  because they write to the owner's database.
- Native Linux verification, still deferred.
- Strengthen the `ctrl_c_stops_server_and_reaps_pool` console guard so a ConPTY
  session skips instead of failing. It is now the only test that fails under
  ConPTY.

Roadmap decisions, due at their relevant design gates:

- DONE: strict typed protocol v2 owns transport metadata; protocol v1 workers
  fail startup before listener bind and there is no fallback;
- choose managed versus externally managed Windows FastCGI before 0.6 is
  finalized;
- choose edge-capable versus origin-only and the HTTP/2 gate before 0.9;
- identify 1.0-stable config/protocol surfaces before the release candidate.

## Current working-tree handoff: Application Request Metadata (2026-09-18)

This section supersedes the older `Next Action` and protocol-v1 decision notes
above. Those passages are retained as historical context, not current work.

FACT: the owner selected typed protocol v2. The PHP handler now passes the
accepted connection's immutable `TransportInfo` plus the actual HTTP version
into each independently owned `WorkerRequest`. V2 encodes HTTP/1.0 versus 1.1,
peer IP/port and local IP/port before the existing request fields. IPv4 and
IPv6 use family-tagged fixed-width bytes. READY compatibility is strict v2;
there is no v1 fallback or optional tail.

FACT: Laravel's Symfony request server bag now receives `REMOTE_ADDR/PORT`,
`SERVER_ADDR/PORT`, actual `SERVER_PROTOCOL`, cleartext
`REQUEST_SCHEME=http`/`HTTPS=off`, effective `HTTP_HOST`, and `SERVER_NAME`
derived from that same effective authority hostname. No PHP `$_SERVER` globals
are hydrated. Forwarded headers remain ordinary untrusted client input.

FACT: checked-in regressions cover PHP decoder corruption, old-worker startup
mismatch before listener bind, `.2`/`.3` real loopback peers and ports,
HTTP/1.0, absolute-form authority conflict, keep-alive request separation,
W1 reuse and independent multi-pool delivery. The checked-in scratch PMS probe
configures Symfony to trust only `127.0.0.1`; an actual `.2` peer with forged
forwarded client/host/scheme/port values remains `.2`/`localhost`/`http`, and a
following `.3` request replaces all request-owned metadata on the same W1 PID.

FACT: Windows verification passed the 100,000-request protocol soak, the full
nine-test multi-pool fault matrix, PHP decoder/request reconstruction checks,
PMS W1/W4 lifecycle and cleanup recovery, cancellation stress, crash recovery,
at-most-once execution timeout and shutdown drain. The temporary prepared PMS
copy was updated for the probe; the original PMS was not modified.

INFERENCE: a four-pair alternating release comparison demonstrates no material
trivial-worker regression, but its spread cannot exclude a low-single-digit
cost. Paired RPS and CPU signs split two/two. Do not claim v2 is zero-cost.
Raw samples and exact contract are in
`doc/APPLICATION_REQUEST_METADATA.md`.

Known limits are not blockers without new evidence: PHP globals and duplicate
header semantics remain separate adapter compatibility work; TLS/trusted proxy
implementation was not opened; IPv6 was binary-decoder tested but not exercised
end to end; IPv6 scope IDs are not represented; Linux remains not natively
verified. The working tree is not committed, tagged, pushed, merged or released.
Owner decides closure and repository actions.
