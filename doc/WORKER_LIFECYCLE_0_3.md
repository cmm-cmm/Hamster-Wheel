# Worker Lifecycle 0.3 Design Contract

## Scope and Owner Decisions

Milestone 0.3.0 adds process lifecycle recovery to the accepted fixed PHP
worker pool. `max_requests` and `max_worker_age` are optional and default OFF.
There is no proactive idle-worker stdout read probe; idle process death is
detected passively when that generation is next selected. Application Reset is
moved to the 0.4.0 Laravel Worker milestone.

This milestone does not change protocol v1, transaction cancellation safety,
bounded admission, fixed target size, or one-owner-per-generation semantics.

## Identity Model

A slot is the stable pool-capacity identity. A generation is the monotonically
increasing lifecycle identity within one slot. PID is only a property of the
currently spawned process and is never used as worker identity.

Every dispatch capability is an `IdleWorker { slot, generation, mailbox }`.
The mailbox belongs to exactly that generation. The dispatcher keeps no
slot-to-mailbox table. A stale token naturally fails `try_send` with `Closed`;
that proves the job was not delivered and is the only safe redispatch case.

## Lifecycle State Model

Slots move through `STARTING -> READY <-> BUSY`, and on unexpected failure:

```text
LIVE(slot, generation)
  -> EXITED
  -> RESPAWNING(slot, generation + 1)
  -> BOOTSTRAPPING
  -> LIVE
```

A replacement becomes dispatchable only after a valid READY handshake.
Initial startup remains all-or-nothing. A replacement bootstrap failure affects
only its slot and re-enters per-slot backoff; it does not stop the running pool.

## Idle Channel Ownership

An `idle_tx` sender may be owned only by a live generation task or a pending
respawn/backoff/bootstrap task capable of restoring service. The supervisor
must never retain a permanent keeper clone.

Therefore `idle_rx` closure means there is no live worker and no pending
replacement capable of restoring service. That closure is terminal pool
unavailability. Zero READY or even zero live generations can be transient while
a valid replacement owns the corresponding sender.

## Failure and Request Semantics

The request that discovers or causes a generation failure fails exactly once.
Once PHP may have started REQUEST, it is never retried. HTTP cancellation never
cancels a transaction; RESPONSE is consumed or the generation fails
deterministically. Replacement serves only later work.

Mailbox `Closed` is safe for redispatch because no generation received that
job. Mailbox `Full`, ambiguous delivery, I/O failure, timeout, protocol error,
or process exit never authorizes transparent retry.

## Backoff and Retirement

Failure one gets an immediate replacement. Later consecutive failures wait
100, 200, 400, 800, 1600, 3200, then at most 5000 ms. Backoff waits are
shutdown-aware. `MAX_CONSECUTIVE_FAILURES` is 10; exceeding it retires the slot.
No extra circuit breaker is added.

Failure history resets only after a generation has remained READY/alive for at
least 60 seconds and has successfully served at least one request. Planned
recycling does not increment failure history.

## Recycling

`max_requests` and `max_worker_age` are independently optional. Explicit values
must be at least one request and one second respectively. Recycling uses
stop-then-spawn, occurs only at transaction boundaries, and permits only one
voluntary recycle across the pool at a time.

A generation selected for recycling closes its mailbox and does not publish a
new idle token. Any job already buffered in that mailbox is drained before the
worker stops. Max-age retirement while idle follows the same close-then-drain
rule. Memory-threshold and spawn-then-drain recycling are not included.

## Shutdown Ordering

Global phase changes atomically from Serving to ShuttingDown before shutdown
coordination begins. After that transition no PHP process may be spawned.
Admission stops first. Respawn/backoff work is cancelled. The dispatcher closes
admission and converges after dispatching or failing accepted jobs. Worker stop
is signalled only after dispatcher convergence, preventing a wait for capacity
that can never return.

Workers finish assigned transactions, drain buffered mailbox work, send
SHUTDOWN, require SHUTDOWN_ACK, and use the existing two-second kill/reap
fallback. Bootstrap, generation, respawn, dispatcher, and supervisor tasks are
joined. Shutdown must leave no PHP child behind and must not hold lifecycle
locks across await points.

## Telemetry

`WorkerSnapshot` exposes slot, generation, PID, lifecycle state, spawn reason,
request count, spawn time, consecutive failures, and last exit reason.
`PoolSnapshot` exposes target workers, READY workers, total respawns, and all
slot snapshots. Telemetry is observational only and is never polled for
correctness or adaptive decisions.

## Race Invariants

- Shutdown phase authorization and process spawn are serialized; shutdown wins
  permanently once ShuttingDown is visible.
- Generation-specific mailboxes make stale idle tokens harmless.
- A worker never publishes idle after selecting voluntary recycle.
- A response receiver closing is not a worker failure.
- Replacement never retries the failed generation's possibly-started request.
- Terminal idle-channel closure occurs only after every live/restorable slot
  has relinquished its sender.

## Acceptance Tests

The accepted matrix is:

1. idle-crash replacement restores the target;
2. in-flight crash replacement restores the target;
3. a possibly-started request is never retried (unique execution marker);
4. W4 degrades to W3 and recovers to W4;
5. all workers can fail transiently and recover;
6. repeated crashes use the exact per-slot backoff sequence;
7. the failure limit and delay cap prevent a respawn storm;
8. `max_requests` performs a graceful generation recycle;
9. `max_worker_age` performs a graceful generation recycle;
10. shutdown cancels an active backoff;
11. shutdown interrupts and reaps replacement bootstrap;
12. shutdown while busy completes the transaction;
13. shutdown leaves no orphan PHP process;
14. generation increments while slot remains stable;
15. stale generation/channel capability cannot receive a job;
16. all 0.2 cancellation invariants remain unchanged;
17. long correctness soak includes forced failures; and
18. continuous-load random-kill stress checks recovery and unique execution.

The three 0.2 tests that asserted permanent no-respawn behavior are lifecycle
tests superseded by this contract. All 0.2 correctness-invariant tests remain
unchanged.

## Implementation Amendments

These were accepted during the independent 0.3.0 review because the shipped
implementation deliberately differs from the wording above.

Admission fails fast while no generation is live. `PhpWorkerPool::execute`
returns a controlled 503 when the live-generation count is zero, instead of
admitting the request to the bounded queue and letting it wait for the pending
replacement. "Zero live generations is transient" therefore governs the
already-accepted work the dispatcher is holding, not new admissions. An
accepted job still survives the whole replacement window and is served by the
new generation; only requests that arrive inside that window are rejected.

Voluntary recycling is threshold-based, not capped. `max_requests` and
`max_worker_age` request a recycle at the next transaction boundary; the
single pool-wide permit and replacement bootstrap time mean a generation can
overshoot its configured value. This is recorded in `doc/PHP_WORKER.md`.

The dispatcher drains idle capabilities continuously and discards capabilities
whose mailbox has closed. Publishing a capability is a blocking send, so this
is what keeps "a worker never blocks while publishing idle" true on a pool with
no traffic. Without it, `max_worker_age` stalled once `workers * 2` stale
capabilities filled the idle channel.

## Non-Goals

No adaptive pool sizing, heartbeat protocol, proactive idle read probe,
application Reset, Laravel lifecycle, memory recycle threshold, FastCGI, TLS,
reverse proxy, HTTP/2, HTTP/3, static optimization, or Hamster Morph/Cortex/
Turbo/StateFence/HamsterLink implementation belongs to 0.3.0.
