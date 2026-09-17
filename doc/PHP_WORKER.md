# PHP Worker Pool

## 0.3.0 Lifecycle Supervisor

Milestone 0.3.0 keeps the 0.2 actor-owned transaction path and adds permanent
worker slots managed by a lifecycle supervisor. A slot is stable pool capacity;
its generation increments whenever the process is replaced, while PID remains
only a process property. Each idle capability carries
`{ slot, generation, mailbox }`, and the mailbox belongs to that exact
generation. The dispatcher has no worker lookup table.

Unexpected process, pipe, or protocol failure fails the affected request once
and schedules a replacement. A possibly-started request is never retried. The
first replacement is immediate; subsequent failures back off for
100, 200, 400, 800, 1600, 3200, then at most 5000 ms. After the tenth failure
the slot moves into a bounded cooldown band instead of being retired: 30, 60,
120, 240, then 300 seconds, repeating at that ceiling. **Slots are never
permanently retired**, so a pool that lost every worker to a transient host
problem restores itself without an operator restart. Failure history resets
only after a generation has stayed READY for 15 seconds and completed a
request successfully; the 15 seconds are measured from READY, so bootstrap
time does not count toward it.

An application execution timeout is deliberately outside that ladder. It also
kills and reaps the worker, and its request is also never retried, but it does
not increment the slot's failure history and its replacement is immediate no
matter how often it happens. A slot whose replacement reaches READY has not
demonstrated that it cannot start, which is the only thing the ladder protects
against; charging it there was measured to cost 30 seconds of total outage for
eleven slow requests. The respawn rate stays bounded because the deadline
itself bounds it, at `1 / (request_timeout + bootstrap)`. See
`doc/EXECUTION_TIMEOUT_POLICY.md`.

Stability is time based on purpose. Treating successful request *count* as
proof of health would tie the respawn rate to application throughput, because
a crash loop reaches any request threshold sooner the faster the pool serves.
The time threshold bounds the worst case at four spawns per minute per slot at
any load, and a permanently broken slot at roughly 0.2 spawns per minute.

Idle process death is detected passively when its generation is next selected;
there is no proactive stdout probe in this milestone.

Only live generation tasks or pending backoff/bootstrap tasks own `idle_tx`.
The supervisor never keeps a permanent clone. Zero READY workers is therefore
transient while a replacement is possible. Because a slot always has a pending
replacement, `idle_rx` no longer closes through exhausted retries; admission
still refuses immediately rather than blocking, because `execute` checks
`live_workers` before queueing. New requests receive controlled 503 while no
process is live, and service resumes after replacement READY.

Optional `max_requests` and `max_worker_age` policies default OFF. Recycling is
stop-then-spawn, happens only at a transaction boundary, and is serialized to
one voluntary recycle across the pool. A recycling worker closes its mailbox,
drains an already-buffered job, and does not publish another idle token.
Planned recycle does not increment failure history.

The dispatcher drains idle capabilities continuously, not only while it holds
a job, and discards capabilities whose generation has already closed its
mailbox. This is a correctness requirement of the recycle path, not an
optimization: a generation publishes its capability with a blocking send, so a
pool with no traffic would otherwise fill the `workers * 2` idle channel with
stale capabilities, block the next generation inside its publish before it
could arm its own age deadline, and stall `max_worker_age` at generation three.
`max_worker_age_recycles_a_pool_with_no_traffic` and
`dispatcher_drains_idle_capabilities_without_a_pending_job` are the regression
tests.

### Known 0.3.0 recycling limitation

`max_requests` is a soft threshold, not a cap. Voluntary recycling holds a
single pool-wide permit; a generation that cannot take the permit keeps serving
and retries at the next transaction boundary. Measured: `workers = 2`,
`max_requests = 1`, 200 requests produced 8 recycles. Overshoot is bounded by
permit contention and replacement bootstrap time, not by the configured value.
This does not violate a correctness invariant and only affects an optional
policy that defaults OFF.

Shutdown changes the global phase before signalling tasks, permanently closing
spawn authorization. Admission stops, backoff/bootstrap work is cancelled, the
dispatcher converges, in-flight and accepted work resolves, then live workers
receive SHUTDOWN. ACK validation, the two-second kill fallback, child reaping,
and task joins remain mandatory. Application Reset is deferred to the 0.4.0
Laravel Worker milestone. The full contract is in
`doc/WORKER_LIFECYCLE_0_3.md`.

`PoolSnapshot` reports target and READY counts, total respawns, and per-slot
`WorkerSnapshot` values: slot, generation, PID, lifecycle state, spawn reason,
request count, spawn time, consecutive failures, execution timeouts, and last
exit reason.

`live_workers` is the pool's health signal and the counter `execute` gates its
503 on. `ready_workers` counts workers that are READY *and idle*, because
`mark_busy` decrements it, so it measures spare capacity rather than health: a
four-worker pool that served 23,072 requests with no failure at all reported
zero ready workers in 20% of samples taken during that load. A degradation
check built on `ready_workers` would fire constantly under ordinary traffic.
Compare `live_workers` against `target_workers` instead.

`live_workers` is only as fresh as the last dispatch. An idle worker's death is
detected passively on its next assignment, so a pool with no traffic reports
workers that are already gone until something is dispatched to them. `consecutive_failures` decays and schedules replacements;
`execution_timeouts` does neither, so it stays a faithful count of how often
the application ran past its deadline on that slot. `PoolSnapshot` carries the
pool-wide `total_execution_timeouts` and `live_workers` beside
`total_respawns`.

Every pool-scoped diagnostic names its pool, as `PHP worker pool <host><path>`,
including the child's own drained standard error. Only one `php_worker`
location is permitted today, so nothing is currently ambiguous without it; it
exists so the diagnostics do not have to be revisited when more than one pool
becomes possible, and so an operator can grep one pool's lines by a string they
already wrote in their configuration.

A pool that loses its last live worker reports that, and reports again when one
returns:

```text
PHP worker pool localhost/ has no live worker after 1000 ms; requests are refused until one is READY
PHP worker pool localhost/ recovered: 1 of 1 workers live after 1636 ms with none
```

Both lines use the same threshold, `POOL_OUTAGE_NOTICE_AFTER`, so they always
appear in pairs. The threshold exists because every ordinary replacement passes
through zero live workers for as long as a bootstrap takes -- 38 ms for the
fixture, 58 to 403 ms for real Laravel applications -- and announcing those
produced far more outage lines than recoveries, which reads as a pile of
unresolved outages. One second clears the worst measured bootstrap about 2.5
times over while still reporting anything a client could notice, since the
backoff ladder holds a slot down for up to 5 s and the cooldown band for 30 to
300 s. Reporting is suppressed during shutdown, where losing every worker is
the goal rather than a fault.

Partial degradation is deliberately not reported. A pool that is serving on two
of four workers still answers every request, and the measured user-visible
failure was total refusal. Adding a second threshold for partial capacity would
need its own evidence. These values are
observational; correctness does not poll them.

## 0.2.0 Architecture

Milestone 0.2.0 replaces the HTTP-owned single-worker transaction with a fixed
pool of actor-owned workers:

```text
HTTP task -> bounded jobs queue -> dispatcher -> per-worker mailbox
                                             -> dedicated worker task
                                             -> response oneshot
```

Each worker task exclusively owns one mutable `PhpWorker`, including its child,
stdin/stdout, request IDs, protocol state, shutdown, kill, and reap operations.
There is no global worker mutex and no task shares a process. One worker still
executes at most one request at a time; concurrency comes only from multiple PHP
processes. Protocol v1 and the 8 MiB frame, 1 MiB request body, and 128-header
limits remain byte-for-byte unchanged.

Admission uses bounded `mpsc::channel(queue_capacity)` and non-blocking
`try_send`. A full or closed queue returns 503 immediately. Accepted jobs are
paired FIFO with idle worker IDs. Only worker tasks hold `idle_tx`; therefore,
when the last healthy worker exits, `idle_rx` closes and the dispatcher resolves
its held job and every queued job with 503, closes admission, and exits without
polling or a healthy-counter race. One worker failure leaves the pool serving in
degraded mode; there is no respawn in 0.2.0.

## Cancellation Safety

The transaction lifetime is independent of the HTTP future. A queued job whose
oneshot receiver has closed is skipped before assignment, and the worker checks
again before writing. Once REQUEST writing begins, the worker task always reads
and validates the complete RESPONSE or handles a genuine timeout, process, I/O,
or protocol failure. Client disconnect is never a worker-health input. If the
receiver disappears after assignment, response delivery fails harmlessly and a
valid worker returns to READY.

This structurally fixes the known 0.1.2 cancellation availability limitation;
it does not treat cancellation as worker failure and does not retry a possibly
started transaction.

### Provably unstarted redispatch (0.4)

There are exactly two cases where a job may be dispatched again without
weakening that invariant, and both are proofs that PHP never received the
request rather than guesses that it probably did not.

1. **A closed generation mailbox.** The dispatcher's `TrySendError::Closed`
   means the generation task is gone and never took the job. This has existed
   since 0.2.
2. **A worker that was already dead, or that refused the first byte.** `execute`
   now returns `PhpWorkerError::NotStarted` when `try_wait` reaps the process
   before a byte of the frame is written, or when the frame write fails having
   transferred **zero** bytes. `write_frame_tracked` exists to answer that
   question, because `write_all` cannot.

Everything else stays a `502`. A write that moved any number of bytes may have
been read by PHP; a timeout says nothing about how far PHP got. Both are treated
as started.

A job carries a `redispatches` counter and `MAX_NOT_STARTED_REDISPATCHES` is 1.
One retry covers the case this exists for -- a worker killed while idle, found
by the next request -- while keeping a crash-looping pool from bouncing one job
between dead generations. A requeue that cannot be admitted, because the queue
is full or admission has closed, fails the job exactly as before.

The visible change is that killing an idle worker no longer costs a client a
`502`. `respawn_after_idle_crash_restores_target` asserts the `200` and, through
the fixture's execution marker, that the request body ran exactly once.
`possibly_started_request_is_never_retried` is untouched and still passes.

## Pool Configuration

```toml
[[hosts.locations]]
path = "/php"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/worker.php"
workers = 4
queue_capacity = 32
```

`workers` defaults to 1 and is validated in 1-64. `queue_capacity` defaults to
`min(workers * 8, 128)` and is validated in 1-128. At most one PHP worker
location is still permitted. The 128 hard maximum caps queued request payload
at approximately 128 MiB when every admitted job carries the 1 MiB maximum,
before request metadata and allocator overhead.

This bounded queue limits the number of jobs admitted to the PHP pool; it is
not complete memory backpressure for the HTTP server. HTTP bodies are buffered
before `pool.execute()` attempts admission, and memory can also include the job
held by the dispatcher, in-flight worker jobs, and concurrently buffering HTTP
requests. Total-byte admission and body streaming remain backlog items outside
0.2.0. Deployments should choose a lower `queue_capacity` for a tighter memory
envelope.

Startup is all-or-nothing: children start concurrently, all must validate READY,
and admission is exposed only afterwards. Any partial failure shuts down every
child that reached READY and kills/reaps failed handshakes. Explicit shutdown
stops admission, drains accepted jobs, lets in-flight transactions finish, sends
SHUTDOWN to healthy workers, validates SHUTDOWN_ACK, applies the two-second
kill/reap fallback, and joins the worker and dispatcher tasks. Windows Ctrl+C
uses the same coordinated shutdown path and has a process-level regression test.

## 0.1.x Historical Architecture

## Architecture

Milestone 0.1.0 adds one persistent PHP CLI process without changing the
existing vector routers or handler enum design:

```text
HTTP Request
    -> Handler::PhpWorker
    -> PhpWorker transaction mutex
    -> persistent php.exe worker.php
    -> bootstrapped request loop
    -> validated HTTP Response
```

The executable and script are trusted startup configuration. Hamster Wheel
spawns the executable directly with the script as its only argument; no shell,
`php-cgi`, PHP-FPM, FastCGI, or command-string expansion is involved. Standard
input and standard output carry only the protocol. Standard error is a separate
pipe and is drained independently so diagnostics cannot corrupt framing.

The optional `php.env` table adds environment variables to the child on top of
the server's own environment, and every generation receives them, replacements
included. The pool never reads them. They exist so a worker script can be told
what application it serves without the server needing to know what kind of
application that is, which is what keeps framework-specific logic out of Rust
entirely; see `doc/LARAVEL_WORKER_0_4.md`.

Because standard output is the frame stream, a worker script owns the whole of
it. PHP's own diagnostics must be redirected to standard error before anything
is loaded: PHP 8.5 emits deprecation notices for ordinary application code, and
a single such line printed ahead of the first frame is read as a frame length.
The shipped Laravel worker sets `display_errors = stderr` at its top and buffers
both bootstrap and request handling for this reason.

## Lifecycle and Process Management

The prototype state machine is `STARTING -> BOOTSTRAPPING -> READY -> BUSY ->
READY`. `PhpWorker::start` spawns one child with `kill_on_drop`, waits for a
validated `READY` frame, and refuses traffic until the handshake succeeds.
Exit, EOF, malformed framing, a wrong protocol version, or the fixed five-second
startup timeout fails server startup before the listener binds.

There is exactly one worker in a runtime configuration. Every transaction holds
one Tokio async mutex from request write through response read, so concurrent
HTTP requests queue and protocol bytes cannot interleave. This serialization is
intentional correctness scaffolding, not the 0.2.0 pool architecture.

An I/O, protocol, ID, or request-timeout failure marks the worker
unavailable and terminates it. The failed and later requests receive controlled
`502 Bad Gateway` responses; 0.1.0 never respawns automatically.

## Binary Framing Protocol

Every message is `[payload_length: u32 big-endian][payload]`. The length excludes
the four-byte prefix and cannot exceed 8 MiB. The first payload byte is its kind;
all integers are unsigned big-endian and every variable byte string carries an
explicit length. Readers reject oversized, truncated, trailing, or structurally
invalid frames.

Message kinds are:

- `READY (1)`: protocol version `u16`, PHP PID `u32`.
- `REQUEST (2)`: request ID `u64`; method (`u16 + bytes`); URI, path, and query
  (`u32 + bytes` each); header count `u16` followed by name (`u16 + bytes`) and
  value (`u32 + bytes`) pairs; body (`u32 + bytes`).
- `RESPONSE (3)`: matching request ID `u64`; status `u16`; header pairs in the
  same encoding; body (`u32 + bytes`).
- `SHUTDOWN (4)`: no fields.
- `SHUTDOWN_ACK (5)`: no fields.

The complete request or response is buffered in one frame. Header order and
duplicates are preserved, and values and bodies are binary-safe. The prototype
accepts at most 128 headers and a 1 MiB HTTP request body. There is no streaming,
chunking, multiplexing, or compression.

## HTTP Mapping and Validation

Rust serializes the HTTP method, complete URI, path, query without `?`, every
header occurrence, and the buffered body. Body-limit violations return `413`
without touching the worker. The PHP response status must be a valid HTTP status;
header names and values must pass Hyper's parsers. Hop-by-hop headers and worker
supplied `Content-Length` or `Server` are rejected so framing and server-owned
headers cannot conflict. Any malformed response yields `502`, never a panic.

## Configuration

```toml
[[hosts.locations]]
path = "/php"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/worker.php"
```

Both paths may be absolute or relative to the directory containing
`hamster.toml`. Startup validation requires each path to exist and be a regular
file, then canonicalizes it once. `php` is required only for `php_worker` and is
rejected for other handlers. More than one PHP worker location is invalid in
this milestone. The development setup uses a locally supplied PHP 8.5 NTS at
`php/8.5/php.exe`; the `php/` directory is intentionally ignored by Git. Place
the PHP distribution at that path before using the checked-in `hamster.toml`,
which exposes the fixture at `/php`. PHP is never searched from `PATH`.

## Shutdown

Ctrl+C first stops accepts and lets current connection tasks drain under the
existing server behavior. Shutdown then waits for any transaction holding the
worker mutex, sends `SHUTDOWN`, requires `SHUTDOWN_ACK`, closes stdin, and waits
up to two seconds for process exit. Timeout or protocol failure invokes direct
child kill and wait. On Windows, a console Ctrl+C can also terminate the child
before Rust sends `SHUTDOWN`; an already-exited child is treated as successfully
cleaned up. `kill_on_drop` is a final orphan-prevention fallback.

## Fixture and Persistent-State Proof

`fixtures/php-worker/bootstrap.php` initializes one process object and increments
the bootstrap counter once. `worker.php` keeps it plus a request counter alive
inside its loop. State responses expose PID, bootstrap count, and request count;
100 sequential HTTP requests must retain one PID, report bootstrap count one,
and advance the request count from one through 100.

This intentionally demonstrates the principal risk: a persistent runtime can
leak globals, statics, singleton objects, request references, transactions, or
other state across requests. The prototype does not reset Laravel containers,
globals, sessions, database state, or scoped services. State reset belongs to a
later lifecycle milestone.

## Known Limitations and Roadmap

### 0.1.2 cancellation limitation (fixed in 0.2.0)

`PhpWorker::execute` runs inside the HTTP connection future and holds its guard
across await points. If a client disconnects while a PHP transaction is in
flight, that future is dropped after the REQUEST frame has been written but
before the RESPONSE frame is consumed, so the protocol stream is left out of
sync. The next request reads the previous response, the id check fails, and the
worker is latched unavailable; with one worker and no respawn, every later PHP
request returns 502 until the server restarts.

This was an architectural property present since 0.1.0, not a 0.1.2 regression.
It was deliberately not hotfixed because marking or killing the worker on
cancellation would still let one disconnecting client remove the only worker.
0.2.0 fixes it structurally: the transaction now runs inside a dedicated worker
task whose lifetime is independent of the HTTP request. See
`doc/research/WORKER_POOL_REVIEW.md`.

### Other limitations

Workers still use buffered messages, fixed timeout and size limits, and simple
stderr diagnostics. There is no health polling, cancellation frame, streaming,
or adaptive sizing. FastCGI remains separate later work.

### Laravel worker limitations (0.4)

`fixtures/php-worker/laravel_worker.php` adapts protocol v1 to Laravel Octane's
application sandbox. `laravel_request.php` rebuilds the request, including
cookies, urlencoded bodies and `multipart/form-data` uploads, because the SAPI
layer that normally does that is not in the path. `laravel_lifecycle.php` owns
only external/process-global cleanup which a cloned application cannot own.
The response is staged until termination and cleanup succeed, so a contaminated
generation is never made reusable by an early response frame. The adapter also
reserves a private duplicate of stdout for protocol frames and redirects fd 1
to the null device after bootstrap. Output buffering alone cannot contain
`fwrite(STDOUT, ...)`; after READY, direct application or cleanup writes no
longer reach the protocol pipe.

If Octane reports an uncaught lifecycle error, the staged response is not
written. PHP exits, Rust receives EOF, returns 502 for the possibly-started
request without replaying it, marks the generation unavailable and starts a
replacement. Sending a synthetic 500 before exit was tested and rejected
because Rust briefly advertised the dead generation as READY after accepting
the valid frame.

What the worker does not do:

- Request bodies are capped at the protocol's 1 MiB, uploads included. A larger
  upload is `413` before the worker sees it.
- Userland globals/statics, runtime listeners and macros remain process state.
  Request-sensitive data must use scoped bindings or an explicit compatible
  Octane lifecycle hook.
- A singleton resolved in the base application or listed in `octane.warm` is
  worker-owned. It must not capture request, user or tenant state. A legacy
  singleton that must be rebuilt belongs in `octane.flush`.
- Streamed/file responses are captured into one frame; this is response-semantic
  support, not transport streaming.
- The protocol does not carry typed peer address metadata, so Laravel currently
  sees `REMOTE_ADDR=127.0.0.1`. Rust receives the address from `accept()` but
  drops it before routing. A reserved server-owned pseudo-header can fit the
  existing header list without changing framing; protocol v2 would provide a
  typed metadata field. Neither owner-reserved option is implemented.

The lifecycle, falsification evidence and current blockers are in
`doc/LARAVEL_WORKER_0_4.md` and
`doc/research/LARAVEL_LIFECYCLE_0_4_REVIEW.md`.

## 0.1.1 Windows IPC Investigation

The investigation is isolated in `benches/php_ipc.rs` and the
`ipc_echo.php`/`ipc_timeline.php` fixtures. It does not change the production
worker, protocol, router, configuration, or timeout behavior. Run individual
release suites with:

```powershell
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- raw
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- variants
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- sync
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- stderr
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- timeline
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- timeline_blocking
```

Omitting `-- <suite>` runs every suite. Cargo appends its own `--bench` flag
after anything passed behind `--`, so the suite is taken from the first
positional argument rather than `argv[1]`; an unknown or second suite name
fails with the accepted list. `cargo bench --no-run --features profiling` is the
gate that actually links every bench: `cargo clippy --all-targets` only checks
them and therefore cannot catch a linker error.

### Current Handoff Shape

For a small request, Rust's production `write_frame` makes two `write_all`
calls (four-byte prefix and payload) and one `flush`. PHP normally completes
two `fread` calls (prefix and payload), constructs one contiguous response
frame, performs one `fwrite`, and calls `fflush` once. Rust consumes the
response through two `read_exact` calls. PHP counters observed 1,841 `fread`
calls for 920 128-byte requests: exactly two per request plus the final EOF
probe. Larger payloads require more PHP reads (normally 8 KiB chunks).

On Windows, Tokio 1.53.1 implements child stdio by wrapping ordinary blocking
file handles in its blocking-I/O adapter. Each underlying read/write is handed
to a blocking task; `ChildStdin`/`ChildStdout` are not native overlapped named
pipes. The Rust-child control nevertheless completes in tens of microseconds,
so this adapter is measurable but cannot explain the PHP-only 15 ms plateau.

### PHP Windows Pipe Behavior

PHP 8.5.10 opens `php://stdin` as a non-blocking Windows pipe by default. Its
stdio stream implementation uses `PeekNamedPipe`; when no bytes are available,
it calls `usleep(10)` and polls again. The observed sleep is quantized near the
Windows 15.625 ms timer interval. This behavior is distinct from PHP output
buffering: `ob_get_level()` was zero, omitting `fflush` did not help, and the
write-buffer setting was unsupported.

`stream_set_blocking($stdin, true)` returned `false` and did not change latency.
A profiling-only open using a stream context with
`['pipe' => ['blocking' => true]]` bypassed the polling path and reduced the
production-format median from 15.137 ms to 67.9 us. This is causal experiment
evidence, not an accepted production change.

### Timing Semantics and Limits

Rust records encode, write completion, complete response receipt, and response
decode with one `Instant` domain. PHP reports only internal durations from
`hrtime`: decode, fixture handler, response encode, and response write/flush.
No absolute timestamp is subtracted across processes. The residual after
subtracting PHP internal duration from Rust's external wait is the combined
Rust-to-PHP and PHP-to-Rust transfer/wakeup time; the two directions cannot be
safely separated with this protocol.

The timeline fixture writes one diagnostic stderr line after each response.
That adds profiling overhead before the next request and is excluded from
production. Process CPU accounting is coarse on this Windows host, especially
for short high-throughput trials. Full results and ranges are recorded in
`Doc/BENCHMARKS.md`.

### Decision

Conclusion A applies: PHP's default Windows anonymous-pipe polling/buffering
behavior is the primary cause. Before any production edit, separately review a
minimal `pipe.blocking` fixture change for shutdown/timeout implications and
rerun every 0.1.0 failure and HTTP integration test plus the same benchmark.
Do not begin a worker pool to hide this latency.

## 0.1.2 Platform Verification Status

| Target | Status |
|---|---|
| `x86_64-pc-windows-msvc` | VERIFIED |
| `x86_64-unknown-linux-gnu` | COMPILE-CHECKED ONLY / NATIVE VERIFICATION DEFERRED |

Windows is the fully verified target for 0.1.2: formatting, clippy, unit and
integration tests, the PHP worker integration tests, the release build, the
benchmark link gate, PHP fixture lint, and a production regression run all pass
on it.

The non-Windows `cfg` paths type-check: `cargo check` and `cargo clippy` for
`x86_64-unknown-linux-gnu` were run as cross-target commands from the Windows
host and pass. That is not native verification. No native Linux linker, test
run, benchmark, or PHP worker execution has happened, and a cross-target check
must not be reported as one. Linux native qualification is a deferred platform
gate to be run when a Linux CI job or host exists; it is not a defect in 0.1.2
and does not block the milestone. This milestone therefore claims a verified
Windows target, not Linux support.

## 0.1.2 Production Blocking Pipe

The canonical production fixture now opens `php://stdin` on Windows with the
supported stream context `['pipe' => ['blocking' => true]]`. The context is
created and the stream is opened before `READY`; context creation, open, or
STDIO metadata failure throws and therefore fails Rust's startup handshake.
There is deliberately no fallback to the default polling stream. Non-Windows
workers retain the ordinary `php://stdin` open because the Windows pipe option
does not apply there.

PHP does not expose the internal Windows `pipe.blocking` flag through
`stream_context_get_options`, and `stream_get_meta_data()['blocked']` is `true`
for both the default and context paths. Startup can consequently verify a
successfully opened blocking STDIO stream but cannot query the internal flag
directly. The actual Windows behavior is guarded by the production HTTP
latency test: the accepted run has no samples clustered around 15.625 ms and
reduces P50 from about 15.2 ms to 0.13-0.14 ms. A return of that quantum is a
release blocker, not a condition for silent degradation.

The wire protocol remains version 1 and is byte-for-byte unchanged: one binary
length-prefixed frame per message, repeated headers, binary bodies, `READY`,
`SHUTDOWN`, and `SHUTDOWN_ACK`. The 8 MiB frame, 1 MiB request-body, and 128
header limits are unchanged. No profiling fixture is called by production.

Blocking read changes only how PHP waits while READY. Closing stdin, sending
`SHUTDOWN`, or process termination still wakes the read. Programmatic shutdown
waits for the transaction mutex, drains the current BUSY request, then performs
the existing two-second graceful handshake/kill fallback. On Windows, console
Ctrl+C may terminate the child while BUSY; Rust detects EOF, returns a
controlled 502 for that request, and completes cleanup. READY and BUSY Ctrl+C
checks left no orphan `php.exe`.

The default polling loop, timer-resolution changes, busy waiting, named pipes,
protocol redesign, respawn, and worker pooling were rejected for this
milestone. The single async mutex and one-worker state model remain intact.
