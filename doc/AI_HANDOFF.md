# AI Handoff

## Current Milestone

0.2.0 — Fixed PHP Worker Pool + Cancellation Safety.

Do not start 0.3.0 work. Automatic respawn, adaptive sizing, recycle, health
polling/backoff, protocol changes, Laravel lifecycle, FastCGI, TLS, HTTP/2, and
static-file optimization are outside the current scope.

## Current Status

The implementation and corrective pass are present as uncommitted changes on
`dev`. The branch is three documentation/maintenance commits ahead of
`origin/dev`. Do not commit, push, merge, or discard existing changes without
an explicit request.

Claude's adversarial-review findings are being corrected. The queue envelope,
benchmark methodology, Ctrl+C environment detection, pool-soak wording,
dispatcher abnormal-exit propagation, and poisoned PID-lock drop path have been
addressed in code or documentation. The long-duration benchmark matrix is
complete; final quality gates remain to be completed.

## Verified Platform

The accepted native platform is Windows `x86_64-pc-windows-msvc` with the local
PHP 8.5 NTS executable at `php/8.5/php.exe`. Linux has not been natively
qualified. Do not report cross-target checking as native Linux verification.

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

## Next Action

Current final gate result: `cargo fmt --all -- --check`, Windows-target Clippy
with all targets/features and warnings denied, normal Windows tests, ignored PHP
tests, Windows release build, and the profiling bench link gate all pass. The
normal suite passed 21 library, 14 configuration, 4 routing, and 5 static tests.
The ignored suite passed 8 PHP-worker unit tests and 16 PHP HTTP/process tests.
Ctrl+C had an attached console and ran the real assertion rather than skipping.
All nine PHP worker fixtures passed `php -l`, and no orphan `php.exe` remained.

The next action is final Claude review of the uncommitted 0.2.0 diff. Do not
commit, push, merge, or begin 0.3.0 during that review.
