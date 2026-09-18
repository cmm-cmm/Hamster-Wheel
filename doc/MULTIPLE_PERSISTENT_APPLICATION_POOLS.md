# Multiple Persistent Application Pools

Status: implementation scope closed by owner on 2026-09-18. The working tree
is not committed, tagged or released.

This document is the authoritative contract for multiple persistent
application pools. Historical 0.1-0.4 documents may describe the one-location
restriction that existed at those milestones; that restriction is no longer
the current configuration contract.

## Final architecture — FACT

Every `php_worker` location compiles to one independent `PhpWorkerPool`. The
pool still exclusively owns its processes, queues, supervisor, spawn gate,
slot/generation state, recovery ladder, timeouts and health counters. This
change does not add a shared supervisor or a generic backend abstraction.

The only cross-pool resource is the existing process-wide `BodyBudget`. That is
intentional: it bounds memory held by the Rust process, not memory assigned to
one application.

The router holds the exact pool compiled for each location. Two locations may
use the same executable and script but still own distinct pool instances and
lifecycle state. Existing single-pool configurations keep their prior handler
and runtime path.

Protocol v1, at-most-once dispatch, application execution-timeout policy,
worker slot/generation lifecycle, pool health semantics and framework adapter
ownership are unchanged.

## Startup transaction — FACT

Pools start sequentially in configuration order. The listener is created only
after every pool completed every READY handshake. If pool N fails:

1. pool N cleans up any of its workers that reached READY;
2. the runtime shuts pools 0 through N-1 down in reverse order;
3. every rollback target is attempted even after a cleanup error;
4. `server::run` performs a final all-pool convergence pass;
5. startup returns failure without binding or accepting traffic.

The failure from pool N remains the primary error and the error-chain source.
Rollback failures are retained as pool-labelled additional evidence. A cleanup
failure therefore cannot make a broken startup look like a shutdown failure.

If listener bind or shutdown-signal setup fails after all pools reach READY,
that server I/O error remains the causal source. Worker shutdown failures are
reported as additional pool-labelled evidence, and every pool is still
attempted.

Startup remains all-or-nothing. A pool that fails initial bootstrap has no
supervisor and no retry path, so serving other pools would create a permanently
stopped application. Degraded startup and startup retry are out of scope.

## Shutdown — FACT

Normal shutdown walks every pool even if an earlier pool reports an error. Each
pool's existing bounded shutdown/kill/reap behavior is unchanged. When multiple
pools fail, the first failure is primary and later failures are additional
pool-labelled evidence. A healthy pool between two failing pools is still
drained, shut down and reaped.

## Known limitations — FACT

Pool startup and shutdown are currently sequential. This is the smallest
correctness design and preserves existing pool behavior, but worst-case total
shutdown latency can scale with the number of pools. No evidence in this slice
justifies concurrent pool orchestration; it remains a measurable operational
hypothesis rather than a hidden guarantee.

Configuration still limits workers per pool (1-64), not the process-wide sum
of workers or number of pools. Configuration is trusted startup input and
startup remains fail-fast, but an operator can request an impractically large
process population.

Direct cancellation or abort of the startup future and concurrent calls into
startup/shutdown are not a qualified public lifecycle contract. The production
`server::run` ownership and its returned error paths are qualified.

PMS multi-pool operation was not verified because no prepared external PMS
environment was available. Generic persistent-worker multi-pool behavior and
the existing single-pool Laravel adapter were verified separately; that is not
equivalent evidence.

Linux was not natively verified. Windows results and cross-platform source
reasoning must not be reported as native Linux support.

These are recorded limitations, not closure blockers supported by current
evidence. Do not implement concurrent orchestration, a global ceiling, startup
retry or degraded startup without new evidence and owner scope.

## Verification evidence — FACT

`tests/configuration.rs` verifies two locations compile into separately labelled
pools and route independently. PHP-dependent permanent tests in
`tests/multipool_probe.rs` verify:

- two healthy pools start, serve and stop;
- repeatedly crashing all generations in pool A does not disturb pool B;
- a later startup failure reaps exact PIDs from an earlier healthy pool;
- the server refuses connections while a later pool is still bootstrapping and
  exits without ever accepting traffic when it fails;
- a shutdown failure in the first pool does not prevent two later pools from
  shutting down and all five exact child PIDs are reaped;
- two separate shutdown failures are both retained with the correct pool
  labels while a healthy pool between them still shuts down;
- a startup failure remains primary when rollback also reports failure;
- an occupied listen address remains the primary server error when worker
  cleanup also fails, and every started pool is still reaped;
- the process-wide body budget is visible across pools, body-free traffic in a
  healthy pool remains available under pressure from another pool, and a
  disconnect releases the reservation for the other pool.

An ignored unit regression also covers the narrower within-pool case: one
worker reaches READY, another sends malformed READY, and the first worker then
fails its shutdown handshake. The malformed startup remains the cause and the
forced-kill cleanup is additional evidence.

The final Windows verification was:

- nine of nine multi-pool process/HTTP regressions passed sequentially;
- the default all-feature suite passed 92 non-ignored tests;
- strict clippy, formatting and the Windows MSVC release build passed;
- PHP fixture syntax and the benchmark link gate passed in the implementation
  pass;
- the broader PHP lifecycle matrix passed every locally runnable non-PMS case,
  including cancellation, at-most-once, crash/recovery, random-kill and soak;
- five PMS/Laravel cases remained unrun because their prepared external
  environment variable was unavailable, not because a product assertion
  failed;
- no repository PHP child remained after the fault suites.

No throughput benchmark was run for this slice. Request routing still reaches
the already-compiled pool directly and no request/protocol hot-path operation
was added; startup, shutdown and error handling are the changed paths.

## Assumptions falsified — FACT

- “A vector of pools means multi-pool ownership is already correct” was false.
  The old loops returned on the first startup/shutdown error and did not own
  rollback transactionally.
- “Process exit eventually kills children, so partial startup cleanup is good
  enough” was false. It skipped protocol shutdown and application cleanup, and
  made an embedded `server::run` ownership path unsafe.
- “Checking two routes have `HandlerKind::PhpWorker` proves distinct pools” was
  false. The permanent config regression now checks concrete `Arc` identity.
- “One forced shutdown failure proves aggregation” was too weak. A regression
  with two failing pools verifies both errors survive with stable identities.
- “Source placement alone sufficiently protects the shared BodyBudget” was too
  weak. An HTTP regression now proves cross-pool admission and disconnect
  release behavior.
- The historical E7 timeout probe assumed execution timeouts enter the
  infrastructure cooldown ladder. That contradicts the accepted timeout
  policy; the probe is now bounded and reports the case as not applicable.

## Engineering judgment — INFERENCE

The verified architecture meets the closed slice contract: resources with
application ownership stay pool-local, the one process-owned request-body
resource stays shared, and every returned startup/shutdown failure path
converges owned processes without losing its primary cause. The known limits do
not invalidate this result at the deployment scale represented by evidence.

## Future questions — HYPOTHESIS

- Sequential shutdown may become operationally expensive with many pools or
  long per-pool request timeouts.
- A process-wide worker/pool ceiling may prevent configuration mistakes at
  scales not represented by current deployments.
- PMS may expose adapter/application interactions not covered by generic
  protocol fixtures when multiple Laravel applications coexist.

These hypotheses require new evidence and explicit scope. They are not tasks
implicitly carried into the next milestone.

## Compatibility

Protocol v1, at-most-once dispatch, execution timeout policy, lifecycle
supervision, pool health semantics, Laravel adapter behavior and HTTP resource
bounds are unchanged. The public orchestration error type is richer so it can
identify a pool and preserve multiple failures.
