# Execution Timeout Policy

Status: **closed by owner decision**. Implemented in the working tree; not
committed, tagged or released. Not to be extended or reopened unless new
evidence requires it. Evidence is in
`benchmarks/results/worker_request_timeout_investigation_20260917.md` (why) and
`benchmarks/results/worker_request_timeout_floor_20260917.md` (floor and hot
path).

## The problem this fixes

The transaction deadline was a fixed five-second constant, and exceeding it was
recorded as a failure of the *slot*. A slot's failure count drives
`replacement_delay`, which walks 0, 100, 200, 400, 800, 1600, 3200, 5000 ms and
then enters a 30-to-300 second cooldown band.

FACT (measured): eleven slow requests, paced to the pool's own recovery, drove
a single-worker pool into that cooldown band, and ordinary requests that never
touched the slow route were then refused for 30 151 ms.

FACT (measured): across 52 timeout-driven replacements, every one reached
READY. The backoff ladder never prevented anything; it only extended the
outage.

The ladder exists to stop a slot that *cannot start* from spawning in a tight
loop. An application that ran too long is no evidence about whether the slot
can start, so charging it to that ladder is a category error with a measured
30-second blast radius.

FACT (measured, and this is the limit of the claim): a pool with healthy
traffic interleaved did not degrade. Two workers with one slow request every
seven seconds served 629 of 629 ordinary requests with zero refusals, because
the 15-second stability reset kept clearing the history. The defect needs slow
requests arriving faster than roughly `15 s / workers`.

## What changed

One distinction, at the point where it was already known.

`PhpWorker::execute` already had exactly one place where a deadline produces a
failure: the `Err` arm of its `timeout()`. That arm now sets `timed_out` on
`TransactionFailure`, which becomes `PhpWorkerError::ExecutionTimeout`, which
`classify_generation_exit` turns into `GenerationOutcome::ExecutionTimeout`.
The supervisor gives that outcome an immediate replacement and does not call
`increment_failure`.

No new detection logic exists. The information was always there; it was being
discarded.

## What deliberately did not change

- **The worker is still killed and reaped.** Protocol v1 has no cancel frame.
  An abandoned transaction's worker would write its RESPONSE into the next
  request's read; the request-ID check would catch the mismatch, but only after
  damaging one more request. The kill is what keeps the protocol stream
  synchronized, and it is the same path that guarantees no orphan child.
- **`started` stays true.** A deadline that expires says nothing about how far
  PHP got, so the job is still never dispatched again.
  `redispatch_not_started` matches only `NotStarted`, so an execution timeout
  cannot reach it. At-most-once is untouched.
- **Infrastructure failures keep the ladder.** I/O faults, protocol faults,
  request-ID mismatches and bootstrap failures still increment the slot's
  failure count and still reach the cooldown band. Separating the timeout did
  not disarm the mechanism for the case it was built for.
- **Protocol v1.** No frame, limit or encoding changed.

## Why an unbounded respawn rate was not reintroduced

`replacement_delay` is documented as bounding the respawn rate independently of
load, and a zero-delay replacement appears to give that up. It does not,
because this failure class carries its own bound: a slot cannot produce an
execution timeout more often than once per `request_timeout` plus its
bootstrap.

FACT (measured): that model holds exactly. At a one-second timeout the observed
rate was 56.5 respawns per minute per slot against 56.6 predicted.

So the bound degrades from 4 per minute per slot to `1 / (request_timeout +
bootstrap)`, which is finite and operator-controlled rather than unbounded.
That is precisely why `request_timeout` has a validated floor, and why the
floor was derived from measurement rather than chosen.

## Observability

An execution timeout is counted, never decayed, and never used to schedule
anything:

- `WorkerSnapshot::execution_timeouts` per slot;
- `PoolSnapshot::total_execution_timeouts` per pool;
- one standard-error line per occurrence, which names the condition and states
  that the failure history was left alone:

```text
PHP worker slot 0 scheduling generation 2 immediately (execution timeout 1; slot failure history unchanged)
```

This is deliberately not a metrics subsystem. It reuses the counters and the
snapshot that already existed.

The diagnostic line is deliberately *not* rate limited, which is the opposite
of the choice the body budget made for its refusals. The reason is who pays.
A body-budget refusal costs the peer nothing, so peer-controlled traffic could
turn it into a logging workload and it needs a limiter. An execution timeout
costs the peer at least `request_timeout` of wall time on a worker it has to
occupy, so the emission rate is bounded by the same mechanism that bounds the
respawn rate.

FACT (measured, G1): the worst case this repository can construct — four
workers, a one-second timeout, every request timing out, driven concurrently
for 30.5 s — produced 95 timeouts, about three lines per second.

## HTTP status: 504 in the current Windows corrective pass

The original policy emitted `502` with the body `PHP Worker Execution Timeout`
for the reasons recorded below. The later owner instruction asks for a
cause-based gateway status: a worker that has not replied by its execution
deadline now emits `504` with the same body. Invalid protocol/process/I/O
responses remain `502`. The request still is never replayed, its worker is
killed/reaped, and replacement still bypasses infrastructure backoff.

`504 Gateway Timeout` is arguably the more literal status — RFC 9110 defines it
as a gateway not receiving a timely response from an upstream. It was
considered and rejected by owner decision, for the reason that outweighs
literal semantics here: the request is already started and may already have had
side effects. `504` reads as "the upstream was slow, try again", and clients
and proxies apply idempotent-retry heuristics more readily to it than to `502`.
Hamster refuses to replay a possibly-started request internally, so it must not
emit an HTTP-level signal that encourages a peer to replay it externally.

That was a deliberate historical choice, not an oversight. The current
contract uses [RFC 9110's timeout meaning](https://www.rfc-editor.org/rfc/rfc9110.html#section-15.6.5)
to identify the cause. A 504 does not assert that an operation is safe to
retry: clients must apply their own method/application idempotency policy.

## Configuration

`request_timeout` is per `php_worker` location, in seconds, 1 to 3600,
defaulting to 5.

The default is a compatibility choice and nothing more. Five seconds is what
the value was hard-coded to before it became configurable; no measurement in
this repository establishes it as a universal or recommended optimum, and it is
not presented as one. An operator is expected to set it from their own
application's observed request latency plus headroom, and
`doc/CONFIGURATION.md` states the one quantitative constraint that follows from
the respawn-rate bound. The validation range of 1 to 3600 seconds comes from
the measured floor derivation, not from preference.

## Regression coverage

Four tests in `tests/php_worker.rs`, the minimum needed to detect a
regression rather than the whole investigation:

- `a_configured_request_timeout_replaces_the_worker_that_exceeded_it` — the
  configured value is the one that fires, the response body identifies the
  condition, and the child is replaced;
- `execution_timeouts_never_enter_the_infrastructure_backoff_ladder` — five
  consecutive timeouts leave `consecutive_failures` at zero, increment
  `execution_timeouts`, and leave ordinary traffic working;
- `an_infrastructure_failure_still_counts_against_the_slot` — a crash is still
  charged, and is not counted as an execution timeout;
- `a_timed_out_request_is_never_executed_twice` — the at-most-once invariant,
  restated for the new class, using the existing execution-marker fixture.

The second was mutation-checked: reverting `classify_generation_exit` to the
old single-class behaviour makes it fail on round 1 with
`consecutive_failures` 1 against an expected 0.

`malformed_truncated_timeout_and_exit_fail_closed` in `src/php_worker.rs` now
asserts the `PhpWorkerError::ExecutionTimeout` variant rather than only that
the message contains "timeout", because the variant is what routes the failure
away from the ladder. The same test still asserts `PhpWorkerError::Request` for
a worker that exits mid-frame, which is what proves the split did not leak into
the infrastructure class.

## Sustained worst case

Removing the ladder for this class means a pathological application now
respawns forever instead of settling into the cooldown band. That case was
measured rather than argued (G1, four workers, one-second timeout, every
request timing out, 30.5 s):

| | |
|---|---:|
| requests answered | 2512, all `502` or `503` |
| execution timeouts | 95 |
| respawns | 95 |
| respawns per minute per slot | 46.8 |
| `consecutive_failures` per slot at the end | 0, 0, 0, 0 |
| process private memory | 2.18 MiB -> 4.22 MiB |
| leaked PHP children | 0 |
| pool state at the end | 4 of 4 READY |

FACT: only 95 of 2512 requests reached PHP at all. The rest were refused
immediately because no worker was live, which is the pre-existing admission
behaviour and makes the real churn rate lower than the arithmetic bound.

FACT: the pool was still at full strength after 95 kills, no child leaked, and
memory grew 2.04 MiB across the run rather than trending.

INFERENCE: the worst case is a fully degraded application answering `502`/`503`
at a bounded respawn rate, which is what it was before this change too. The
difference is that it is now reached only by an application that genuinely
fails every request, instead of by eleven slow requests against a healthy
one.

The raw fault-injection probes remain in `tests/timeout_probe.rs` as a
historical record. They are all `#[ignore]`d, several take minutes, and they
are instrumentation rather than assertions.
