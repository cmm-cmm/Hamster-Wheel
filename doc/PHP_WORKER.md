# PHP Worker Prototype

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

The worker is a single serialized process with buffered messages, fixed timeout
and size limits, simple stderr diagnostics, no health probe, no cancellation
frame, no streaming, no respawn, and no state reset. It is a protocol/lifecycle
proof, not a throughput architecture or a claim against Node.js. Milestone 0.2.0
may introduce a measured worker pool, queue policy, health and respawn behavior;
FastCGI and Laravel integration remain separate later work.
