# AI Handoff

## Current Milestone

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
PHP responses remain buffered protocol-v1 frames. `header_read_timeout` still
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
needed an explicit platform-ignore. Protocol v1 also lacks typed peer metadata,
leaving `REMOTE_ADDR=127.0.0.1`; a stripped server-owned pseudo-header could
carry it without changing framing, while a typed solution requires v2. This is
an owner decision. See
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

Owner review of 0.6.1 Concrete Streaming Response. The measured response-memory
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
  that work. This is a protocol change and needs a milestone that owns the
  protocol; do not change protocol v1 outside one.
- Exercise the supplied application's own import endpoints. Not done here
  because they write to the owner's database.
- Native Linux verification, still deferred.
- Strengthen the `ctrl_c_stops_server_and_reaps_pool` console guard so a ConPTY
  session skips instead of failing. It is now the only test that fails under
  ConPTY.

Roadmap decisions, due at their relevant design gates:

- select the protocol-v1 compatibility and typed transport-metadata policy
  before any backend wire mapping ships;
- choose managed versus externally managed Windows FastCGI before 0.6 is
  finalized;
- choose edge-capable versus origin-only and the HTTP/2 gate before 0.9;
- identify 1.0-stable config/protocol surfaces before the release candidate.
