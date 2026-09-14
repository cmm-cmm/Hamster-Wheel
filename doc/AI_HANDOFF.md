# AI Handoff

## Current Milestone

0.4.0 — Laravel Persistent Worker. Research is complete and the implementation
is uncommitted on top of `616e4ee`, awaiting owner review. The findings,
measurements and rejected alternatives are in `doc/LARAVEL_WORKER_0_4.md`.

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
proactive stdout probe, adaptive sizing, memory-threshold recycle, or
byte-based admission. Permanent slot retirement no longer exists: prolonged
failure settles into a 300-second cooldown ceiling instead. HTTP bodies
still buffer before pool admission. `max_requests` and `max_worker_age` default
OFF; recycling is stop-then-spawn and permits one voluntary recycle at a time.
Both recycle policies are thresholds rather than caps; see the recycling
limitation in `doc/PHP_WORKER.md`. Application Reset is implemented in 0.4.0
for Laravel, in PHP, and so is `multipart/form-data` with file uploads, bounded
by the protocol's 1 MiB body limit.

## Current Findings

Windows quality gates: `fmt --check`, clippy with `-D warnings`, the default
suite (34 unit, 16 configuration, 4 routing, 5 static), a release build and
`cargo bench --no-run --features profiling` all pass. The ignored suite run
single-threaded is 38 passed, 1 failed, and the one failure is
`ctrl_c_stops_server_and_reaps_pool` under ConPTY, which is the documented
harness gap that also fails on the unmodified tree. The complete ignored suite
includes the 18-test lifecycle matrix, the existing 0.2 cancellation/shutdown
regressions, the 100,000-request pool-level soak, a 10,000-request
forced-failure HTTP soak, the 60-second W4 continuous-load random-kill stress,
and the Laravel request-reconstruction checks. PHP lint over all 14 fixtures and
post-test orphan checks pass.

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

## 0.3.1 Retirement Hardening (uncommitted, awaiting review)

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

## 0.4.0 Laravel Persistent Worker (uncommitted, awaiting review)

The server learned one generic thing: `php.env`, a `BTreeMap<String, String>`
carried through all three configuration stages into `PhpWorkerConfig` and
applied with `Command::envs`. Every generation, replacements included, receives
it. All Laravel knowledge lives in PHP, in
`fixtures/php-worker/laravel_worker.php` and `laravel_reset.php`.
`fixtures/php-worker/env_worker.php` exists so the environment can be observed
from an HTTP response.

Verified on a Laravel 12 skeleton and on a real 305-route application with JWT,
Redis sessions and remote MySQL. Persistent bootstrap is worth 19.4x on a
trivial route, 10.8x on a Blade login page, and 1.31x on a 2.2 MB
database-bound dashboard: the gain is inversely proportional to how much real
work a request does. The request-boundary reset costs 0.018 ms.

The reset is not optional. With one worker and no reset, an anonymous client
sending no cookie at all was served another user's authenticated 2.2 MB
dashboard. A four-worker pool hides this, so any isolation test must pin
`workers = 1`.

Two request-reconstruction defects were found by testing against the real
application and fixed: `Request::create()` never parses `HTTP_COOKIE` into its
cookie bag, and it ignores the body entirely, so sessions, authentication and
every form POST were broken. The worker now builds the request directly.

`multipart/form-data` and file uploads are implemented in
`fixtures/php-worker/laravel_request.php`: the worker splits the body, applies
PHP's `a[b][]` field-name rules, writes each file to a temporary and builds
`UploadedFile` objects in test mode, which is required or every Laravel `file`
validation rule rejects the upload. The worker deletes its own temporaries,
because Laravel's `store()` copies rather than moves and relies on a SAPI
request shutdown that never happens. Verified byte-exact over HTTP, and 600
uploads left zero temporaries behind. Bodies are still capped at the protocol's
1 MiB.

A production-tuned measurement of the supplied application was taken without
modifying it, by copying it and running `artisan optimize` against the copy. It
reframes the headline: the persistent gain on a trivial route is 3.3x rather
than 20x, and 7.6x on a Blade page. `artisan optimize` also made the persistent
worker slower on `/up` and leaked 378 B/request there, because
`withRouting(health: ...)` is a closure route and the route cache makes every
dispatch unserialize it.

Eight further probes covered Eloquent statics, the queue manager, forced URL
origins, macros and connection callbacks. Four are now isolated by the reset.
Three are deliberately not: global scopes cannot be restored without
permanently unfiltering soft deletes, which is demonstrated rather than
asserted, and macros cannot be enumerated.

Known gaps: userland statics leak and cannot be fixed by a server; global
scopes and macros registered during a request leak by design; cached closure
routes leak memory inside Laravel itself.

## Next Action

Owner review of the uncommitted 0.4 worker. The three items previously listed
here are done: `multipart/form-data`, the production-tuned measurement, and the
`NotStarted` redispatch. The highest-value remaining items, in order:

- Raise the 1 MiB request body limit, or stream bodies, so uploads larger than
  that work. This is a protocol change and needs a milestone that owns the
  protocol; do not change protocol v1 outside one.
- Exercise the supplied application's own import endpoints. Not done here
  because they write to the owner's database.
- Native Linux verification, still deferred.
- Strengthen the `ctrl_c_stops_server_and_reaps_pool` console guard so a ConPTY
  session skips instead of failing. It is now the only test that fails under
  ConPTY.
