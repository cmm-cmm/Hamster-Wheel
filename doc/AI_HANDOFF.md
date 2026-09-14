# AI Handoff

## Current Milestone

0.3.1 — PHP Worker Lifecycle, retirement hardening. `v0.3.0` and `v0.3.1`
are tagged on `dev`; the retirement-hardening pass sits uncommitted on top of
`17af4d1` and is awaiting owner review. The accepted contract is
`doc/WORKER_LIFECYCLE_0_3.md`, including its implementation amendments and the
revised Backoff and Cooldown section. Milestone 0.2.0 is closed; its
transaction-safety invariants remain mandatory.

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

Idle worker death is detected passively on its next assignment. There is no
proactive stdout probe, adaptive sizing, memory-threshold recycle, or
byte-based admission. Permanent slot retirement no longer exists: prolonged
failure settles into a 300-second cooldown ceiling instead. HTTP bodies
still buffer before pool admission. `max_requests` and `max_worker_age` default
OFF; recycling is stop-then-spawn and permits one voluntary recycle at a time.
Both recycle policies are thresholds rather than caps; see the recycling
limitation in `doc/PHP_WORKER.md`. Application Reset moved to 0.4.0 Laravel
Worker.

## Current Findings

All Windows quality gates pass. The complete ignored suite includes the 18-test
lifecycle matrix, the existing 0.2 cancellation/shutdown regressions, the
100,000-request pool-level soak, a 10,000-request forced-failure HTTP soak, and
the 60-second W4 continuous-load random-kill stress. PHP lint, protocol-v1
diff, and post-test orphan checks pass.

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
- Distinguish "the process exited before REQUEST was written" from a
  possibly-started failure. That case proves PHP never received the frame, so
  it may be redispatched without weakening the never-retry invariant.
- `all_workers_crash_then_pool_recovers` asserts all four concurrent requests
  receive 502; a fast replacement can answer one with 200. Make it
  deterministic.
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

## Next Action

Owner review of the uncommitted retirement-hardening pass. After that, either
close 0.3 or open the 0.4.0 Laravel Worker milestone. `NotStarted` redispatch
for provably-before-first-write failures remains deliberately deferred, and is
the highest-value remaining lifecycle item. Do not change protocol v1 outside a
milestone that owns it.
