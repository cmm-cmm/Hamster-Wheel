# AI Handoff

## Current Milestone

0.2.0 — Fixed PHP Worker Pool + Cancellation Safety. Reviewed and accepted;
closed for technical change.

The next milestone is 0.3.0 — Worker Lifecycle & Recovery. Do not start 0.3.0
work until it is explicitly opened. Automatic respawn, adaptive sizing, recycle,
health polling/backoff, protocol changes, Laravel lifecycle, FastCGI, TLS,
HTTP/2, and static-file optimization remain outside the current scope.

## Current Status

0.2.0 FINAL REVIEW: **ACCEPTED**. Blocking findings: **NONE**.

The implementation and the corrective pass are committed on `dev` at `c9810bc`,
which matches `origin/dev`. This milestone-close documentation pass is the only
uncommitted work in the tree. Do not commit, push, merge, tag, or discard
changes without an explicit request.

Every adversarial-review finding was verified as fixed against the code rather
than against the report: the queue envelope (`MAX_QUEUE_CAPACITY = 128`, default
`min(workers * 8, 128)`, validated and tested), benchmark methodology and
variance wording, Ctrl+C console detection, pool-soak wording, the dispatcher
RAII availability guard, the non-panicking `WorkerPidGuard::drop`, and the
documented `queue_capacity = 128` benchmark driver. The long-duration benchmark
matrix and all Windows quality gates are complete.

## Verified Platform

The accepted native platform is Windows `x86_64-pc-windows-msvc` with the local
PHP 8.5 NTS executable at `php/8.5/php.exe`. Windows native gates: **PASS**.

Linux: **NOT NATIVE VERIFIED**. It is compile-checked only, via cross-target
`cargo check`/`clippy` from Windows. That is not LINUX VERIFIED, not LINUX
SUPPORTED, and not NATIVE LINUX TESTED. Do not upgrade the claim, and do not
report cross-target checking as a native run.

## Architecture Summary

HTTP tasks buffer and validate a request, call `PhpWorkerPool::execute`, attempt
a non-blocking send to a bounded jobs channel, and await a response oneshot.
One dispatcher pairs FIFO jobs with idle worker identities. Each dedicated
worker task exclusively owns one mutable `PhpWorker`, child process, pipes,
protocol state, and request-ID sequence.

Worker concurrency comes from fixed PHP process count; a single process never
multiplexes requests. Failed workers become unavailable and exit. Surviving
workers continue in degraded mode. There is no respawn in 0.2.0.

## Critical Invariants

- One task owns each PHP worker and its stdin/stdout for its full lifetime.
- HTTP tasks never hold `Arc<Mutex<PhpWorker>>`; there is no global worker lock.
- Only worker tasks own `idle_tx`; the dispatcher owns no sender clone.
- Closure of `idle_rx` is the correctness signal for zero healthy workers.
- A REQUEST that may have started is never retried.
- Dropped HTTP clients are not worker failures.
- Queued cancellation may skip work only before transaction execution starts.
- Once execution starts, RESPONSE is consumed or the worker is made unavailable.
- Protocol v1 framing and READY/REQUEST/RESPONSE/SHUTDOWN/SHUTDOWN_ACK are unchanged.
- Startup is all-or-nothing; shutdown explicitly drains, stops, kills if needed,
  reaps, and joins.

## Known Limitations

`queue_capacity` is limited to 1-128 and defaults to
`min(workers * 8, 128)`. The queue bounds admitted job count, not total HTTP
memory. Request bodies are buffered before admission. A full queue can retain
about 128 MiB of payload at the 1 MiB body limit, before metadata, allocator
overhead, dispatcher/in-flight jobs, and concurrently buffering HTTP requests.

Total-byte admission and body streaming require a later design pass. Worker
respawn remains deferred to 0.3.0. A pool can therefore remain permanently
degraded until restart, and all PHP traffic fails fast with 503 after the last
worker becomes unavailable.

## Current Findings

The original 0.2.0 pool benchmark cells were too short and used a different
driver from the immutable 0.1.2 baseline. They are retained only as
NON-COMPARABLE / INVESTIGATION DATA. Their W1 regression and peak-based scaling
claims have been withdrawn.

The corrected driver ran each W1/W2/W4/W8 by C1/C2/C4/C8/C16/C32 Hello/JSON
cell for at least eight seconds, three fresh process sets. Best observed median
configurations were W2/C16 Hello at 10,866 RPS and W4/C8 JSON at 10,620 RPS,
but W4/W8 did not improve consistently at the same concurrency. W4 server
CPU/request was elevated at C16-C32 versus W2 and remains an investigation item.

Dispatcher exit drops its jobs receiver and any locally held job, closing the
corresponding oneshots. An RAII guard now also marks admission unavailable on
normal return, panic, or abort; channel loss maps to controlled pool-unavailable
behavior. A regression test verifies pending and future callers do not hang.

The Windows Ctrl+C integration test checks for an attached console. It performs
the real signal/reap assertions when available and prints a clear SKIPPED reason
when the prerequisite is absent; production signal behavior is unchanged.

The 100,000-request direct `pool.execute()` test is a PHP worker pool-level
correctness soak, not an end-to-end HTTP benchmark. It did not sample working
set start/end and makes no memory-stability claim.

## Quality Gates at Milestone Close

Re-run independently during the final review, all on `x86_64-pc-windows-msvc`,
all passing: `cargo fmt --all -- --check`; Clippy with all targets, all
features, and warnings denied; the normal test suite (44 tests:
21 library, 14 configuration, 4 routing, 5 static); the ignored PHP suite (24
tests: 8 PHP-worker unit, 16 PHP HTTP/process); the release build; and
`cargo bench --no-run --features profiling`, which linked eight bench
executables. Ctrl+C had an attached console and ran the real signal/reap
assertion rather than skipping. All nine PHP worker fixtures passed `php -l`,
and no orphan `php.exe` or `hamster-wheel` process remained afterwards.

No Linux verification was performed. See Verified Platform above.

## Backlog

Deferred work, none of which blocks 0.2.0:

- HTTP body byte-budget admission and body streaming.
- W4/W8 server CPU/request investigation at C16-C32.
- W1 benchmark re-measurement on a quieter host; the current same-methodology
  batches disagree too much to state a pool-overhead percentage.
- Native Linux qualification.
- 0.3.0 worker lifecycle and recovery: respawn, replacement, backoff, recycling,
  adaptive sizing.
- Laravel persistent lifecycle.
- Hamster Morph / Request-Graph JIT research, later.

## Next Action

0.2.0 is closed for technical change. The next action is the project owner's
decision to commit and tag the milestone-close documentation pass.

Do not reopen 0.2.0 for optimization, and do not begin 0.3.0 until it is
explicitly opened. `doc/HAMSTER_MORPH_REQUEST_GRAPH_JIT.md` is untracked
research for a later milestone and must stay out of the 0.2.0 commit and tag.
