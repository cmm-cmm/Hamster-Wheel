# PHP Worker Pool

## 0.3.0 Lifecycle Supervisor

Milestone 0.3.0 keeps the 0.2 actor-owned transaction path and adds permanent
worker slots managed by a lifecycle supervisor. A slot is stable pool capacity;
its generation increments whenever the process is replaced, while PID remains
only a process property. Each idle capability carries
`{ slot, generation, mailbox }`, and the mailbox belongs to that exact
generation. The dispatcher has no worker lookup table.

Unexpected process, pipe, timeout, or protocol failure fails the affected
request once and schedules a replacement. A possibly-started request is never
retried. The first replacement is immediate; subsequent failures back off for
100, 200, 400, 800, 1600, 3200, then at most 5000 ms. A slot is retired after
the tenth replacement also fails. Failure history resets only after a
generation has lived for 60 seconds and completed a request successfully.
Idle process death is detected passively when its generation is next selected;
there is no proactive stdout probe in this milestone.

Only live generation tasks or pending backoff/bootstrap tasks own `idle_tx`.
The supervisor never keeps a permanent clone. Zero READY workers is therefore
transient while a replacement is possible; `idle_rx` closes only when no live
or pending generation can restore service. New requests receive controlled 503
while no process is live, and service resumes after replacement READY.

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
request count, spawn time, consecutive failures, and last exit reason. These
values are observational; correctness does not poll them.

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

An I/O, protocol, ID, or five-second request-timeout failure marks the worker
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
respawn, recycle, adaptive sizing, or state reset. Respawn and advanced health
management remain deferred to 0.3.0; FastCGI and Laravel integration remain
separate later work.

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
