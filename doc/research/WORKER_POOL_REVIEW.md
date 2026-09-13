# 0.2.0 Fixed PHP Worker Pool + Cancellation Safety - Design Review

Status: implemented for 0.2.0. The accepted design below remains the source of
truth; implementation notes are recorded at the end without rewriting the
review decisions.
Baseline: 0.1.2, Windows-accepted (see the platform status in
`doc/PHP_WORKER.md`).

Accepted decisions reflected in this revision:

1. No 0.1.3 hotfix. The 0.1.2 cancellation defect is recorded as a known
   limitation and fixed structurally in 0.2.0.
2. Cancellation safety is a 0.2.0 blocker with a testable invariant.
3. Respawn and advanced health management move to 0.3.0.
4. Degraded pool policy: keep serving on healthy workers; fail fast at zero.

## 1. Current 0.1.2 Architecture

The runtime already carries a worker *collection*, not a single worker:

```text
RuntimeConfig
  listen: SocketAddr
  router: Arc<HostRouter>
  php_workers: Vec<Arc<PhpWorker>>      <- already a Vec
      start_workers()    -> starts every entry, before the listener binds
      shutdown_workers() -> shuts down every entry
```

Routing reaches a worker through the handler enum:

```text
Request -> HostRouter -> LocationRouter -> Handler::PhpWorker { worker: Arc<PhpWorker> }
        -> PhpWorker::serve_http -> PhpWorker::execute
```

`PhpWorker` owns its child behind one async mutex:

```text
PhpWorker { config, timeouts, inner: Mutex<WorkerSlot> }

WorkerSlot = Starting | Running(Box<RunningWorker>) | Unavailable(String) | Stopped
RunningWorker { child, stdin, stdout, stderr_task, pid, next_request_id, state }
WorkerState  = Starting | Ready | Busy | Unavailable | Stopped
```

`execute` takes the mutex and holds it from request write through response read,
so concurrent HTTP requests serialize behind one process. Timeouts are fixed
constants: 5 s startup, 5 s request, 2 s shutdown. Configuration rejects more
than one `php_worker` location. Frame ids are validated per transaction, and any
I/O, protocol, id mismatch, or timeout failure kills the child and latches
`Unavailable`; there is no respawn.

Measured single-worker behavior (`doc/BENCHMARKS.md`): 4,808 RPS at concurrency
1, with latency growing roughly linearly with concurrency (P50 1.721 ms at C16)
because requests queue behind one serialized worker.

### 1.1 Cancellation defect (0.1.2 known limitation, not hotfixed)

Root cause, in order:

```text
client disconnects
  -> Hyper drops the connection/service future
  -> PhpWorker::execute future is dropped mid-transaction
  -> REQUEST frame already written, RESPONSE frame never consumed
  -> the protocol stream is desynchronized
  -> next request reads the previous response, id check fails
  -> worker latched Unavailable and killed; no respawn
  -> every later PHP request returns 502 until restart
```

Reproduced against the 0.1.2 release binary with `benchmarks/php_worker.toml`:

```text
GET /delay?ms=1500  -> client closes the socket after 200 ms
next request        -> "invalid protocol: response ID 1 does not match request ID 2"
three later requests -> 502, 502, 502
```

This is an architectural property present since 0.1.0, not a 0.1.2 regression,
and 0.1.2 keeps its WINDOWS ACCEPTED status. **No 0.1.3 hotfix will be made**: a
patch that merely marks or kills the worker on cancellation still lets one
disconnecting client remove the only worker, so availability would not improve.
The fix must be structural, and it is the reason cancellation safety is a
blocking requirement for 0.2.0 rather than a nice-to-have.

## 2. 0.2.0 Goals

- A fixed-size pool of `N` persistent PHP workers, with `N` fixed at startup.
- Real parallelism: up to `N` requests executing in PHP simultaneously.
- **Cancellation safety as a blocking requirement** (section 10).
- Each worker keeps 0.1.2 semantics: one bootstrap, at most one in-flight
  request, protocol v1 unchanged, Windows blocking-pipe stdin unchanged.
- Bounded queueing with explicit backpressure.
- Unavailable tracking and a degraded pool; **no respawn, no recycle, no
  adaptive sizing, no state reset** (0.3.0 and later).

Non-goal: proving a throughput multiple. Scaling is a measurement, not a claim.

## 3. Proposed Worker Pool Architecture

```text
HTTP task (per request)                  Pool internals                Workers
-----------------------                  --------------                -------
Handler::PhpWorker
  build WorkerRequest
  oneshot::channel()
  jobs_tx.try_send(Job) --->  [bounded mpsc jobs] ---> Dispatcher task
       |  Full   -> 503                                     |  pairs head of jobs
       |  Closed -> 503                                     |  with head of idle
       v                                                    v
  await oneshot rx                          [mpsc(1) worker_tx[i]] --> Worker task i
       ^                                                                owns PhpWorker i
       |                                                                runs protocol v1
       +--------------- oneshot tx <------------------------------------ to completion
                                              [mpsc idle] <------------- announces "free"
```

Three message paths, each with exactly one purpose:

- `jobs`: bounded FIFO queue of admitted requests. The only place backpressure is
  applied.
- `worker_tx[i]`: capacity-1 handoff to one specific worker.
- `idle`: ids of workers that are free. The dispatcher pairs the head of `jobs`
  with the head of `idle`.

The dispatcher performs no I/O and never touches PHP; it only matches a job to a
worker id, so it is not a global lock that serializes the pool.

## 4. Ownership Model

Each `PhpWorker` is **moved into** its worker task, which is the sole owner for
the process lifetime. The goal is an actor-owned sequential state machine: the
protocol transaction is a local, uninterruptible sequence inside one task.

```text
Worker task i owns: PhpWorker i (child, stdin, stdout, stderr drain, request ids)
Pool owns:          jobs_tx (cloneable), dispatcher handle, JoinSet of tasks
Handler owns:       Arc<PoolHandle> { jobs_tx, workers, queue_capacity }
```

Only the worker task may:

- write to worker stdin;
- read from worker stdout;
- change worker protocol state;
- mark the worker unavailable;
- shut down or kill the child.

Explicitly forbidden: `Arc<Mutex<PhpWorker>>` shared with HTTP tasks, and any
global worker mutex. HTTP tasks never touch a pipe.

Consequences:

- Exclusivity comes from ownership plus a capacity-1 mailbox, not from a lock.
  The mutex inside `PhpWorker` may remain as an implementation detail, but the
  pool must not depend on it to choose a worker.
- `next_request_id` stays per worker, so ids cannot collide across workers.
- A transaction runs inside the worker task, so dropping the HTTP task can no
  longer abandon a half-written frame. This is the structural fix for 1.1.

## 5. Scheduling

FIFO on both sides:

1. Dispatcher awaits a job (`jobs_rx.recv()`).
2. It drops jobs whose `oneshot` receiver is already closed (section 6.1).
3. It awaits an idle worker id (`idle_rx.recv()`).
4. It sends the job into `worker_tx[id]`, which is guaranteed to have room
   because a worker announces itself idle only after finishing.
5. The worker executes to completion, replies on the `oneshot`, then returns its
   id to `idle`.

If `worker_tx[id].send` fails, that worker task has died between announcing idle
and receiving work. The dispatcher must not drop the job: it takes the next idle
id and retries, and only fails the job with 503 when no idle worker can be
obtained at all (section 8.1).

Fairness: requests are served in arrival order, so no request is starved by later
arrivals. Worker selection is FIFO over idle ids, which spreads load evenly. A
LIFO idle stack would keep the most recently used worker warm; that is a
measurable alternative, not a 0.2.0 decision.

## 6. Queue, Backpressure, and Queued Cancellation

```text
workers = N            hard ceiling on concurrent PHP execution
queue_capacity = Q     hard ceiling on admitted-but-waiting requests
```

Admission uses `try_send`, never `send`:

| Condition | Behavior |
|---|---|
| Queue has room | Job admitted; HTTP task awaits its oneshot |
| Queue full (`TrySendError::Full`) | 503 Service Unavailable immediately |
| Pool closed (`TrySendError::Closed`) | 503 Service Unavailable |
| No healthy worker remains | 503 Service Unavailable |
| Worker returned a malformed response | 502 Bad Gateway (unchanged from 0.1.2) |

There is never an unbounded wait to enqueue. Keeping 503 and 502 distinct
matters: 502 means a worker answered badly, 503 means the pool could not accept
or serve the request at all.

`Retry-After` is not proposed for 0.2.0. A fixed value would be a guess, and a
computed one needs queue-drain statistics the milestone does not yet have.
Revisit once the benchmark shows how long a full queue stays full.

Memory is explicitly bounded: at most `Q + N` buffered requests, each capped by
the existing 1 MiB body limit, so worst-case queued body memory is
`(Q + N) * 1 MiB`. The configuration documentation must state this, because
`queue_capacity` is in effect a memory setting.

### 6.1 Queued cancellation: Option A vs Option B

| | Option A: always run, discard result | Option B: skip if the receiver is closed |
|---|---|---|
| Complexity | Lowest: no check at all | One `oneshot::Sender::is_closed()` call before dispatch |
| Wasted PHP work | Full transaction for a client that left | Avoided for the common case |
| Race safety | Trivially safe | Safe: the check is only a hint, and losing the race degrades to Option A |
| Queue capacity recovery | Slot frees when dispatched | Same; the slot is released by `recv`, not by execution |
| Overhead | None | One atomic load per job |

Recommendation: **Option B, with Option A as its own fallback.** It needs no new
primitive - `is_closed()` already exists on the `oneshot` sender - and it cannot
introduce a cancellation path into a started transaction, because the check is
strictly before the first byte is written. If the client disconnects one
instruction after the check, the job simply runs and its result is discarded,
which is exactly Option A.

Hard rule: **no cancellation token may ever be able to cancel a transaction after
the REQUEST frame has begun to be written.** Cancellation is permitted only in
the QUEUED / NOT STARTED state.

## 7. Startup Semantics

```text
compile configuration
  -> spawn N children concurrently (JoinSet)
  -> await READY from every worker, each under the existing 5 s startup timeout
  -> all READY?  yes -> start worker tasks and dispatcher -> bind listener -> accept
                 no  -> stop startup, shut down every already-READY worker,
                        kill and reap the rest, report the failing worker index,
                        exit non-zero before binding
```

- Concurrent spawn matters: startup measures about 62-80 ms per worker, so eight
  sequential spawns would add roughly half a second to boot.
- All-or-nothing startup is retained from 0.1.2: the listener must not bind while
  the pool is short of workers, because a partially started pool would silently
  serve at reduced capacity.
- The failure path must reap every child it created, including ones still inside
  their handshake. `kill_on_drop` remains the last-resort guard, not the plan.
- No respawn during startup: a failed worker fails the server.

## 8. Failure Behavior and Degraded Pool

Per-worker failure keeps 0.1.2 semantics inside the worker task:

```text
READY / BUSY -> (I/O, protocol, id mismatch, request timeout, child exit)
             -> kill and reap the child
             -> UNAVAILABLE (latched)
             -> worker task exits without returning its id to idle
```

While `healthy_workers > 0` the pool keeps serving on the remaining workers, and
the dispatcher can never assign work to an unavailable worker because an
unavailable worker never re-announces itself as idle. The healthy count is a
reporting value, not the mechanism (see 8.1), which keeps it off the correctness
path.

Rationale for continuing degraded: failing the whole pool on one bad worker would
make 0.2.0 strictly less available than 0.1.2 for the same fault, and the fault
is usually specific to one process. The cost is that a systemic fault degrades
quietly, so every transition must be reported.

### 8.1 Zero healthy workers, without a counter race

The dangerous interleaving is "healthy count goes 1 -> 0 while the dispatcher is
already parked on `idle_rx.recv()`, holding a job, with more jobs arriving". A
counter checked before parking cannot close this race on its own.

Channel closure solves it without polling, spinning, or an unbounded wait:

- Each worker task holds one clone of `idle_tx`; the dispatcher holds **only**
  `idle_rx` and never keeps a sender clone.
- When a worker task exits, its `idle_tx` clone drops.
- When the last worker task exits, every sender is gone, so a parked
  `idle_rx.recv()` returns `None` **immediately**. There is no wait to time out
  and no state to sample.

On `None` the dispatcher: resolves the job it is holding with 503, drains
`jobs_rx` and resolves every queued job with 503, then drops `jobs_rx` so every
later `try_send` fails with `Closed` and new requests get 503 at admission. The
transition is edge-triggered by the runtime rather than observed by a reader, so
there is no window in which a job can be admitted into a pool that can never
serve it.

No automatic respawn in 0.2.0. Health management, respawn, recycle, and
repeated-failure backoff are 0.3.0.

## 9. Shutdown Semantics

```text
Ctrl+C
  -> stop accepting (existing server behavior)
  -> stop admission: no new jobs are accepted
  -> queued jobs are dispatched (documented policy, see below)
  -> in-flight worker transactions run to completion
  -> each worker task: SHUTDOWN -> await SHUTDOWN_ACK -> close stdin
                       -> wait up to 2 s -> kill fallback -> reap
  -> join every worker task (JoinSet)
  -> join the dispatcher
```

Policy for queued jobs: **dispatch them.** They belong to connections that are
still draining and clients that are still waiting; rejecting them would turn a
graceful shutdown into visible errors. The bound is at most `Q` requests.

Shutdown must be driven by an explicit signal, not by drop order. The router is
cloned into every connection task, so relying on "the last `jobs_tx` clone drops"
would couple shutdown to Hyper's teardown order. An explicit `pool.shutdown()`
awaited after `serve_with_router` returns keeps the sequence readable and
testable.

Cancellation during shutdown changes nothing about the protocol: a client that
disappears while its transaction is draining still gets its transaction
completed and its result discarded. Every child must be reaped on every path,
including the kill fallback and the startup failure path.

## 10. Cancellation Safety (blocking requirement)

### 10.1 The invariant

> Once a REQUEST frame has begun to be written to a worker, that protocol
> transaction must reach exactly one deterministic end:
> **(A)** the RESPONSE frame is fully consumed and validated; **(B)** a protocol,
> timeout, or process failure is handled and the worker is marked unavailable; or
> **(C)** controlled shutdown. HTTP client cancellation must never be one of
> these ends.

Corollaries that make it testable rather than aspirational:

- A dropped `oneshot` receiver is a normal, expected outcome. It discards a
  result; it never aborts a transaction and never marks a healthy worker
  unavailable.
- A worker that completes a transaction whose client has gone must return to
  READY and accept the next job.
- No `select!`, timeout, or abort handle in the HTTP task may wrap the worker
  transaction.

### 10.2 Behavior at every cancellation point

| # | When the client disconnects | Behavior | Worker outcome |
|---|---|---|---|
| 1 | Before enqueue (during body read) | Body read fails; 400/aborted connection. No job is created. | Untouched |
| 2 | While queued | Dispatcher sees `is_closed()` and drops the job before assignment (6.1). Slot already freed by `recv`. | Untouched |
| 3 | After assignment, before the first write | Worker task re-checks `is_closed()` before writing; if closed, it skips the transaction and returns to idle. | READY |
| 4 | While the REQUEST frame is being written | **Not cancellable.** The write completes, the response is consumed, the result is discarded. | READY |
| 5 | While awaiting the RESPONSE frame | **Not cancellable.** The read completes or fails on its own terms; a 5 s request timeout still applies and still marks the worker unavailable. | READY, or UNAVAILABLE on genuine protocol/timeout failure |
| 6 | After RESPONSE, before the HTTP reply is sent | `oneshot` send fails; the result is dropped. | READY |
| 7 | Concurrently with shutdown | Transaction runs to completion, then normal SHUTDOWN/ACK/kill-fallback proceeds. | Clean exit |

Points 3 is the only place where a "skip" is allowed, and it is before any byte
is written. Points 4 and 5 are where 0.1.2 breaks today; in the pool they are
inside the worker task and are unreachable from the HTTP task's lifetime.

### 10.3 What the HTTP task does instead

The HTTP task awaits the `oneshot`. If the sender is dropped because the worker
task itself died mid-transaction, that is a 502, not a hang. The HTTP task never
imposes its own deadline on a started transaction; the request timeout lives in
the worker task, where it can terminate the protocol correctly.

## 11. Tokio Primitives

| Primitive | Use | Why this one |
|---|---|---|
| `mpsc::channel(Q)` bounded | job queue | Backpressure and FIFO in one place; `try_send` gives a non-blocking admission decision |
| `oneshot::channel` | response return | Exactly one value; `is_closed` detects client cancellation for free |
| `mpsc::channel(1)` per worker | dispatcher to worker handoff | Capacity 1 encodes "at most one in-flight request" in the type, not in a comment |
| `mpsc::channel(N)` | idle worker ids | Never blocks; its closure is the zero-healthy-worker signal (8.1) |
| `JoinSet` | worker tasks and startup | Structured joining and failure reporting for a fixed set of tasks |
| `Arc<PoolHandle>` | handler access | The handler only needs a cloneable sender and metadata |

Deliberately rejected:

- **Global `Mutex` over the pool**: would reintroduce the 0.1.2 serialization the
  milestone exists to remove.
- **`Semaphore` for admission**: no bounded wait queue and no way to ask how many
  waiters exist, so the 503 decision and queue-depth metric become awkward.
- **Unbounded channel**: removes the memory bound.
- **`Notify`**: needs a shared ready-set plus a lock, and missed-notification
  bugs are easy to write and hard to test.
- **`Arc<Mutex<mpsc::Receiver>>` shared by all worker tasks**: `recv().await`
  while holding the lock blocks every other worker from even waiting.
- **`CancellationToken` around a transaction**: directly violates 10.1.
- **Any spin or poll loop**, and any lock held across an await that is not needed
  for correctness.

## 12. Configuration Proposal

```toml
[[hosts.locations]]
path = "/php"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/worker.php"
workers = 4            # optional, default 1 -> behavior compatible with 0.1.2
queue_capacity = 32    # optional, default workers * 8
```

Validation rules to add, in the existing fail-fast style with the field path and
host/location index in the message:

- `workers` >= 1 with a sane upper bound (proposal: 64), so a typo cannot spawn
  hundreds of PHP processes.
- `queue_capacity` >= 1, upper bound stated together with the memory formula from
  section 6.
- Defaults chosen so an unchanged 0.1.2 configuration keeps 0.1.2 behavior.
- 0.2.0 still permits exactly one `php_worker` location; the milestone widens the
  worker count, not the number of pools. The existing error message naming
  "milestone 0.1.0" should be reworded.

No `min_workers`/`max_workers`: adaptive sizing is out of scope.

## 13. Race and Deadlock Analysis

**13.1 No lock is held across an await in the pool path.** Scheduling state lives
in channels, so there is no lock ordering to get wrong and no cross-task cycle.

**13.2 Dispatcher parked forever (resolved by design).** Handled by 8.1: the
dispatcher holds no `idle_tx` clone, so the last worker's exit closes the channel
and `recv()` returns `None` instead of parking. Without that ownership rule this
is a real deadlock, so it needs an explicit test (Test 6) rather than a comment.

**13.3 Lost worker id.** If a worker task panics after taking a job but before
returning its id, capacity shrinks silently. Mitigation: the `JoinSet` observes
task termination, and a job whose worker vanished is retried against the next
idle worker (section 5). A panicking worker task must be treated as an
unavailable worker, not as a lost job.

**13.4 Capacity-1 mailbox invariant.** Sending to `worker_tx[i]` must never
block. This holds only while "a worker announces idle exactly once per completed
job" is true; it deserves a debug assertion, treating `Full` as a programming
error rather than a runtime condition.

**13.5 Request id collisions.** None: ids are per worker and each worker has one
channel; no two tasks share a pipe.

**13.6 Response delivery race.** `oneshot::Sender::send` returning `Err` is
normal (client gone) and must not mark the worker unhealthy - this is the most
likely place to reintroduce defect 1.1 by accident.

**13.7 Starvation.** FIFO on the job queue makes request starvation impossible;
idle ids are consumed in order.

**13.8 Shutdown race.** Closing admission while the dispatcher is mid-pairing must
not drop a job a client still awaits; the dispatcher completes the current pairing
before observing the closed channel.

**13.9 Cancellation versus failure attribution.** A client disconnect and a
genuine worker failure can happen in the same transaction. The rule is that only
protocol, timeout, and process outcomes decide worker health; client presence is
never an input to that decision.

## 14. Correctness Test Plan

### 14.1 Required cancellation tests

Test 1 - **the 1.1 regression test** (`workers = 1`): request A is
`GET /delay?ms=...`; the client disconnects while A is in flight; after A
completes, request B on a new connection must return 200, from the same worker
PID, with the protocol still synchronized. No 502, no id mismatch, no unavailable
worker.

To avoid timing guesswork, the fixture should signal that it has entered the
handler - for example by creating a marker file, which the test waits for before
disconnecting - so the disconnect provably happens after the REQUEST arrived at
PHP and while PHP is still working. A fixed `sleep` that hopes to land inside the
transaction is exactly the flaky pattern to avoid.

Test 2 - many concurrent clients disconnect mid-transaction; afterwards every
worker is still healthy and serving.

Test 3 - a client disconnects while its job is queued; no worker is touched and
no worker is corrupted.

Test 4 - a client disconnects after assignment; the worker transaction still
completes and the worker returns to READY.

Test 5 - a worker really dies during a transaction; that worker becomes
unavailable, the pool continues at `N-1`, and other in-flight requests are
unaffected.

Test 6 - every worker unavailable; queued and new requests resolve with 503
promptly instead of hanging. This is the regression test for 13.2.

Test 7 - Ctrl+C with both queued and in-flight work; clean shutdown, zero orphan
`php.exe`.

### 14.2 Pool behavior

- `workers = 1` reproduces 0.1.2 behavior for every existing PHP worker test.
- `workers = 2`, `4`, `8` start, serve, and shut down cleanly.
- Concurrent requests greater than the worker count complete in bounded time.
- Each worker bootstraps exactly once; per-worker counters advance independently;
  the observed PID set has size `N`.
- No cross-talk: every response body matches its request, including binary bodies
  and repeated headers.
- Keep-alive on one connection and many concurrent connections both work.
- Queue full returns 503 while in-flight requests still succeed; 502 and 503 are
  never interchanged.
- Partial startup failure (worker `k` fails) leaves no child alive and exits
  non-zero before binding.

## 15. Benchmark Plan

Matrix, release build, Windows, existing .NET driver style:

- workers: 1, 2, 4, 8
- concurrency: 1, 2, 4, 8, 16, 32
- workloads: PHP Hello, PHP JSON

Metrics per cell: RPS; P50/P95/P99; Hamster CPU per request; total PHP CPU across
all worker processes; combined CPU per request; Hamster working set; total PHP
working set; and queue wait time only if it can be measured without instrumenting
the hot path.

Required comparisons:

- `workers = 1` against the 0.1.2 baseline, to isolate the overhead of the queue,
  dispatcher, oneshot, and worker task. Any regression is a design cost to be
  reported, not averaged away.
- Scaling efficiency per worker count, stated as measured ratios, never as a
  claim of linear scaling.
- The saturation point where added concurrency stops increasing RPS, and the
  concurrency at which the queue begins to fill.

**Cancellation is never a throughput case.** The cancellation work is a
correctness and stability property, and no performance number may be attributed
to it. Cancellation appears in the test plan and the soak plan, not in the
benchmark matrix.

Methodology stays as recorded: release build only, medians of at least three
runs, conditions written down, results appended to `doc/BENCHMARKS.md` without
touching historical numbers.

## 16. Soak Plan

Normal soak: at least 100,000 requests at a representative pool size
(`workers = 4`). Track the same `N` PIDs throughout, bootstrap count exactly one
per worker, request distribution across workers, memory before and after, and the
absence of timeouts, crashes, protocol corruption, deadlocks, and orphans.

Cancellation stress (separate run, mandatory for this milestone):

- a large share of requests deliberately disconnect mid-transaction;
- afterwards, a verification pass of ordinary requests must succeed on every
  worker;
- acceptance: no protocol desync, no request id mismatch, no unexpected
  unavailable worker, no orphan process, and the healthy count still `N`.

A 1,000,000-request soak is worth running only if the 100,000 run is clean and
the distribution is even.

## 17. Security Implications

- **Availability (present in 0.1.2).** The 1.1 defect is remotely triggerable:
  one client that disconnects mid-request disables the PHP handler for everyone
  until restart. It is accepted as a known limitation for 0.1.2 and removed
  structurally by this design, which is why cancellation safety blocks 0.2.0.
- **Resource bounds.** `workers` and `queue_capacity` are the only limits on PHP
  process count and queued memory. Both need validated upper bounds, and the
  `(Q + N) * 1 MiB` worst case must be documented rather than discovered.
- **Fault isolation improves.** `N` processes mean one poisoned worker no longer
  takes down all PHP traffic, provided section 8 is implemented.
- **State isolation does not improve.** Persistent workers still leak globals,
  statics, and singletons across requests, and a client can no longer predict
  which worker it lands on. State reset stays a later milestone and must not be
  assumed by anything built on 0.2.0.
- **Diagnostics.** Per-worker stderr must stay drained and must never log request
  bodies or headers.
- Unchanged: executable and script remain trusted startup configuration, spawned
  directly, never through a shell, never resolved from `PATH`.

## 18. Out of Scope for 0.2.0

Automatic respawn, worker recycle policy, repeated-failure backoff, advanced
health management, adaptive pool sizing, Laravel worker mode, state reset,
FastCGI, TLS, HTTP/2, static-file optimization, mmap, TransmitFile, named-pipe
transport, io_uring, and CPU affinity.

Backlog items discovered during review:

1. `Retry-After` and/or `queue_timeout`, once queue-fill statistics exist.
2. LIFO idle selection for cache warmth, to be measured, not assumed.
3. Location prefixes are not stripped before the request reaches PHP, so a worker
   mounted at `/php` sees `/php/state`. Harmless today; must be settled before
   framework integration.
4. The configuration error text still says "milestone 0.1.0 permits exactly one
   php_worker location" and needs rewording.
5. Whether queue wait time should count toward a request deadline (see 19.3).

## 19. Remaining Design Questions

These do not block implementation; each has a documented default.

1. **Degraded floor.** Section 8 continues at any non-zero healthy count. Should
   there be a floor - for example, refuse to keep serving below half the
   configured workers - or is any non-zero count acceptable? Default: any
   non-zero count.
2. **Observability surface.** 0.2.0 needs queue depth, wait time, and per-worker
   utilization for its own benchmarks. Are these benchmark-only measurements, or
   the first step of the "Observable" pillar with a real interface? Default:
   benchmark-only.
3. **Timeout semantics under queueing.** The 5 s request timeout currently covers
   execution only, so a queued request can wait and then still get a full 5 s.
   Should queue wait count toward a total deadline? Default: keep execution-only
   for 0.2.0 and revisit with queue-wait data.
4. **Worker identity in diagnostics.** Should responses or logs carry a worker
   index to make distribution debuggable, given it slightly exposes internals?
   Default: logs only, never a response header.

## 20. Recommended Architecture

Bounded FIFO job queue, one dispatcher task that pairs jobs with idle workers, and
`N` worker tasks that each exclusively own one `PhpWorker`, with results returned
over `oneshot` channels, and the zero-healthy-worker condition signalled by idle
channel closure rather than by a sampled counter.

It is recommended because it: removes the global serialization point without
introducing a global lock; makes "one request per worker" a structural property
of a capacity-1 mailbox; puts backpressure in exactly one place with an explicit
memory bound; keeps every started transaction inside a task the client cannot
cancel, which satisfies the 10.1 invariant structurally rather than by discipline;
turns the most dangerous race into a runtime-provided edge signal; and reuses the
existing `Vec<Arc<PhpWorker>>`, startup-before-bind, and SHUTDOWN/ACK machinery.
Protocol v1, the Windows blocking-pipe path, the per-worker state machine, and the
timeout constants are untouched.

Known costs, stated rather than hidden: one extra task and one extra handoff per
request compared with a direct-checkout design (to be quantified by the
`workers = 1` benchmark), and a dispatcher whose ownership rule in 8.1 must be
preserved - it is easy to break by "helpfully" keeping an `idle_tx` clone.

## Verdict

**READY FOR CODEX IMPLEMENTATION**

The two questions that blocked the previous revision are answered: no 0.1.3
hotfix, with the 0.1.2 limitation documented; and respawn moves to 0.3.0. The
degraded pool policy is fixed, cancellation safety has a testable invariant with
defined behavior at all seven cancellation points, and the zero-healthy-worker
deadlock has a mechanism rather than a caveat. The open items in section 19 all
have documented defaults and can be settled during implementation without
changing the architecture.

Acceptance for 0.2.0 must include Test 1 (the 1.1 regression test) and Test 6
(the zero-healthy-worker test) passing, plus the cancellation stress soak in
section 16.

## 21. Implementation Notes

The implementation follows the recommended actor topology: every mutable
`PhpWorker` is moved into exactly one task, the bounded admission channel uses
`try_send`, and HTTP tasks await only a response oneshot. The dispatcher owns
`idle_rx` but no `idle_tx`; only worker tasks own idle senders, so the last
worker exit closes the channel and resolves held, queued, and future requests
with 503 instead of parking.

The worker task checks a closed response sender before the first protocol write.
After `PhpWorker::execute` begins, it always consumes a matching response or
handles a genuine timeout/process/protocol failure, regardless of HTTP client
cancellation. A failed oneshot send is normal and returns the worker to READY.
No transaction is retried after worker ownership begins; only a mailbox send to
an already-closed worker task is retried, which proves no request byte was
written.

Startup creates all children concurrently and exposes admission only after all
READY frames validate. A partial failure shuts down every successful child.
Shutdown explicitly closes admission, drains accepted jobs, queues SHUTDOWN
behind in-flight work, validates SHUTDOWN_ACK, applies the existing timeout/kill
fallback, and joins worker and dispatcher tasks. Automatic respawn, recycle,
and backoff remain deferred to 0.3.0.

The implementation caps `queue_capacity` at 128. This bounds admitted job
count, not total request bytes: HTTP bodies are buffered before admission, so a
full queue can retain about 128 MiB of payload and total request memory can be
higher. A total-byte budget or body streaming requires separate design work and
is not part of 0.2.0.

The configuration backlog item about the old "milestone 0.1.0" error text is
resolved; validation now reports the 0.2.0 one-pool rule.
