# Runtime Configuration

## Design Goals

Milestone 0.0.4 parses, validates, and compiles configuration once before the
listener accepts requests. The resulting `RuntimeConfig` is immutable and is
shared as `Arc<RuntimeConfig>`. Request handling uses the already-compiled
`HostRouter -> LocationRouter -> Handler` graph; it never reads TOML, resolves
paths, or rebuilds routing data.

## Schema

```toml
[server]
listen = "127.0.0.1:8080"
max_connections = 1024
header_read_timeout = 30
body_read_timeout = 30
write_progress_timeout = 30
max_body_memory = 67108864
http1_max_buffer_size = 65536

[[hosts]]
name = "localhost"
aliases = ["127.0.0.1"]

[[hosts.locations]]
path = "/api"
handler = "not_found"

[[hosts.locations]]
path = "/"
handler = "static"
root = "public"
```

An optional PHP worker location configures its executable, script, fixed pool,
and bounded queue:

```toml
[[hosts.locations]]
path = "/php"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/worker.php"
workers = 4
queue_capacity = 32
# Ceiling on one application transaction, in seconds. Defaults to 5.
request_timeout = 30
# Optional lifecycle policies; omitted means OFF.
max_requests = 10000
max_worker_age = 3600

# Optional extra environment for the worker process.
[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = "www/pms"
```

`server.listen` must be a numeric IP socket address. `max_connections` bounds
accepted connections owned by the process and defaults to 1024 (valid range
1-65536). At the bound, Hamster stops accepting until a connection task exits;
it does not create an application queue or reject an already accepted request.
`header_read_timeout` is the maximum number of seconds to receive each complete
HTTP/1 request head, including the wait for a next keep-alive request. It
defaults to 30 and must be 1-3600. `http1_max_buffer_size` caps Hyper's adaptive
per-connection HTTP/1 read/write buffer. It defaults to 65536 bytes and must be
8192-1048576 bytes.

`body_read_timeout` is the maximum total time spent reconstructing a PHP request
body after its head has been accepted. It defaults to 30 seconds and must be
1-3600. The deadline is applied before PHP pool admission, so an incomplete or
slow body cannot occupy a worker queue slot or execute application code. It is a
total read deadline, not an inactivity timer reset after each chunk. A timeout
returns HTTP 408 and closes the incomplete HTTP/1 exchange; it does not cancel a
PHP request because PHP has not received one. The existing 1 MiB PHP body limit
remains independent of this timer.

`write_progress_timeout` is the maximum number of seconds an HTTP response
socket write may remain backpressured without writing at least one byte. It
defaults to 30 and must be 1-3600. The timer is lazy: it starts only when the
underlying TCP write returns `Pending`, is cleared by a positive socket write,
and does not run during request reads, keep-alive idle time, or application/PHP
execution. It is an inactivity interval, not a total response deadline.

`request_timeout` is the maximum number of seconds one application
transaction may take, measured from the REQUEST frame write through the
RESPONSE frame read. It is configured per `php_worker` location, defaults to 5
and must be between 1 and 3600.

The default is a compatibility choice, not a recommendation. Five seconds is
what the value was hard-coded to before it became configurable; nothing
establishes it as a universal or recommended optimum. Set it from the
application's own observed request latency plus headroom: measure the routes
this location actually serves, take a high percentile rather than an average,
and leave room for a cold cache and a slow dependency. A value below the
application's real p99 converts ordinary slow requests into killed workers and
`502`s.

Exceeding it is not an ordinary request failure. Protocol v1 has no cancel
frame, so the only way to reclaim a worker that is still mid-transaction is to
kill it; the request is answered `502` with the body
`PHP Worker Execution Timeout`, the child is killed and reaped, and a
replacement is started immediately. The request is never dispatched again,
because a deadline that expires says nothing about how far PHP already got.

Because exceeding it costs a worker, the value also bounds a slot's worst-case
respawn rate at `1 / (request_timeout + bootstrap)`. That relationship was
measured and holds exactly. The practical consequence is that a slot spends
`bootstrap / (request_timeout + bootstrap)` of its time bootstrapping while
every request is timing out, so **set `request_timeout` to at least ten times
the application's bootstrap time** to keep that under 10%. This repository has
measured 58 ms median and 403 ms worst bootstrap for real Laravel
applications. The 1-second floor exists for the same reason, and because the
deadline also covers Hamster's own protocol I/O: the worst legal payload, a
1 MiB body echoed back, round-trips in 18.1 ms, so one second still leaves the
application over 98% of the smallest allowed deadline.

An execution timeout is deliberately *not* charged to the slot's
infrastructure failure history, so repeated slow requests cannot walk a slot
into the replacement backoff ladder or its cooldown band. Crashes, I/O faults
and protocol faults still are. Both counts are visible in the pool snapshot,
and each timeout writes one line to standard error naming the condition. The
status is deliberately `502` rather than `504`: the request is already started
and may already have had side effects, and `504` more readily invites the
external retry that Hamster itself refuses to perform. See
`doc/EXECUTION_TIMEOUT_POLICY.md` for the measured failure this separation
fixes.

`max_body_memory` is the total number of request-body bytes Hamster may have
buffered process-wide at one time. It defaults to 67108864 (64 MiB) and must be
0 or between 1048576 and 68719476736. Zero disables the budget and restores the
previous behavior exactly, where the only aggregate bound is
`max_connections` times the 1 MiB per-request body limit. Values below one
maximum body are rejected, because they would make some legal requests
impossible even on a completely idle server.

Every PHP worker pool compiled from one configuration shares one budget, since
the resource being protected is this process's request-body memory rather than a
pool's queue. It protects that one term and no other; see *Response bodies are
not bounded* below before using it to size a machine. A
request whose body does not fit the remaining budget is refused immediately with
HTTP 503 and never waits for another request to finish, so the budget adds no
queue and no new way for one client to block another. The reservation is taken
before the memory it covers is allocated and is released when the request's
bytes are dropped, including on timeout, client disconnect, queue-full and
worker-failure paths. A declared `Content-Length` is charged in full before the
first body byte is read, so sending slowly is not a cheaper way past the
ceiling. That charge is what makes the aggregate bound hold and it has a cost,
measured in *What a declared Content-Length reserves, and what that costs*
below. Like the deadlines, the check happens before PHP pool admission and
therefore cannot cancel a possibly-started application transaction.

The budget reports itself three ways. At startup the server prints
`Request body memory budget <n> bytes`, or
`Request body memory budget disabled (server.max_body_memory = 0)`. When it
actually refuses a request it writes one line to standard error naming the
cumulative refusal count and the configured value; the first refusal of a run
reports immediately and later ones at most once every 60 seconds, so
peer-controlled traffic cannot turn refusals into a logging workload. Because a
PHP location can answer 503 for a full worker queue, an unavailable pool or this
budget, and Hyper writes the canonical reason phrase for the status, the
distinguishing signal is the response body, which reads
`Request Body Memory Budget`. The high-water mark is deliberately not reported:
it is only useful for proactive sizing, which needs periodic sampling, and that
is later observability work rather than part of this limit.

These six limits bound connection tasks, slow/idle request heads, HTTP/1 parser
buffering, PHP body reconstruction time and buffered request-body bytes. They do
not add up to a process-memory budget, and 0.5 does not create one. Two terms sit
outside every limit above, and the larger of them is not small. The header and
body deadlines both stop before PHP pool admission, so neither can cancel a
possibly-started application transaction.

### Response-body memory

Static GET responses stream from one open file in 128 KiB application frames.
The same limit is applied to Tokio's internal file buffer, and Hyper only polls
another frame while the connection can accept it. A 256 MiB file therefore no
longer creates a 256 MiB application allocation per slow client.

FACT: on a Windows MSVC release build, 32 clients requesting a 256 MiB file and
then reading nothing held 9.39 MiB of additional process-private memory. The old
fully buffered path used 1026.36 MiB with only four clients. After disconnect,
the 32-client process returned to within 1.20 MiB of idle.

This is a per-stream bound, not a configured aggregate response budget. The
observed slope was about 0.29 MiB per slow connection after the emitted frame,
Tokio, Hyper/socket and allocator overhead were included. `max_connections`
bounds how many such streams the process owns. `max_body_memory` continues to
count request bodies only.

PHP responses remain buffered protocol-v2 frames; the response frame shape is
unchanged. Their frame
limit, rather than the static file size, bounds one response.

`header_read_timeout` is not a response-write timeout. `write_progress_timeout`
owns that separate phase: after socket backpressure begins, no accepted bytes
for the configured interval closes the partial response and releases its
connection slot. Producing another body frame is deliberately not progress,
because Hyper may buffer it before the socket accepts it. A client that keeps
causing positive writes is not cut by this inactivity policy, even when its
total transfer is slow. Consequently this is not an absolute response-lifetime
bound; enforcing one would require a declared minimum rate or total deadline.
Graceful shutdown retains its independent finite drain deadline.

### What a declared Content-Length reserves, and what that costs

A request that declares `Content-Length` is charged that many bytes of
`max_body_memory` before its first body byte is read. That is what keeps a slow
sender from being the cheap way past the ceiling, and it is why the aggregate
bound holds at all. It has a price, and the price is not theoretical.

FACT: with `max_body_memory = 4194304` and `body_read_timeout = 20`, four
clients that declared 1 MiB each and then sent a single byte held the entire
budget. Every other POST received 503 for the duration, while GET requests
continued to be answered 200. At the 64 MiB default the same denial costs an
attacker 64 connections.

A slow client therefore holds budget it has not used, for up to
`body_read_timeout`. Configuration that affects this:

| Setting | Effect |
|---|---|
| `body_read_timeout` | The dominant lever. It is the length of the hold, so lowering it shortens every stalled reservation. |
| `max_body_memory` | Raising it raises the number of concurrent stalled declarations needed, at the cost of a higher memory ceiling. |
| `max_connections` | Caps how many stalled declarations can exist at once, but only when it is set below `max_body_memory` divided by the 1 MiB body limit. |
| `max_body_memory = 0` | Removes the denial entirely, and with it the aggregate memory bound. |

This 503 is not the per-request 413 and should not be read as one:

| | 413 Payload Too Large | 503 Request Body Memory Budget |
|---|---|---|
| Scope | This one request | The whole process right now |
| Cause | The body exceeds the 1 MiB protocol limit | The body is individually legal, but there is no room left |
| Determinism | The same request always fails | The same request may succeed a moment later |
| What to change | The client, or the protocol limit | `max_body_memory`, `body_read_timeout`, or the traffic |

A client that sees 503 here should retry; a client that sees 413 should not.


Host names and aliases use
the router's restricted DNS-name grammar, do not include a port, and match
requests exactly and ASCII case-insensitively. Handler names are case-sensitive;
only `static`, `not_found`, and `php_worker` are supported. A static location
requires `root`; a PHP worker location requires `php.executable` and
`php.script`. `workers` defaults to 1 and must be 1-64. `queue_capacity`
defaults to `min(workers * 8, 128)` and must be 1-128. Handler-specific fields
on another handler are errors. `max_requests` is an optional positive request
count. `max_worker_age` is an optional integer number of seconds and must be at
least 1. Both default OFF. Each `php_worker` location owns an independent fixed
pool. Multiple locations may coexist on one host or across virtual hosts;
diagnostics identify a pool as `<host><normalized-location-path>`. Unknown
fields are errors.

All configured pools must complete their READY handshakes before the listener
binds. If any pool fails startup, pools that already started are shut down in
reverse startup order and the server fails startup. There is no degraded
startup or startup retry. Pool queues, processes, lifecycle state, health and
recycling policy are independent. `server.max_body_memory` is intentionally
different: one process-wide `BodyBudget` is shared by every pool because it
bounds Rust process memory rather than an application's queue.

The complete ownership, rollback, diagnostics and shutdown contract is recorded
in `doc/MULTIPLE_PERSISTENT_APPLICATION_POOLS.md`.

`php.env` is an optional table of extra environment variables for the worker
process, applied on top of the server's own environment and given to every
generation including replacements. The server never interprets these; they exist
so a worker script can be told what application it serves without the server
needing to know what kind of application that is. A variable name must be
non-empty and must not contain `=` or NUL, and no value may contain NUL. Values
are redacted from the configuration's `Debug` output, because credentials
belong here and logs do not. `doc/LARAVEL_WORKER_0_4.md` describes the Laravel
adapter. It reads `HAMSTER_LARAVEL_PATH` and requires the application to install
Laravel Octane. The former `none` and `targeted` reset modes are rejected;
warm and flush bindings are configured through Octane's application config.

Both recycle policies are thresholds, not hard caps. Voluntary recycling holds
one pool-wide permit, so a generation that reaches `max_requests` while another
slot is recycling keeps serving and retries at its next transaction boundary.
See the recycling limitation in `doc/PHP_WORKER.md`.

The queue bound limits the number of jobs admitted to the PHP pool, not total
HTTP request memory. PHP request bodies are buffered before pool admission. A
full queue plus busy workers can retain up to
`min(max_connections, sum(workers + queue_capacity for every pool)) * 1 MiB`
of completed payload,
but those jobs originated from the already accepted connection set; it does not
add another independent `max_connections` population. Bodies still being read
can approach `max_connections * 1 MiB` under coordinated slow uploads, before
metadata, parser/frame copies and allocator overhead. A total-byte admission
budget and body streaming are deferred; configure `max_connections`, body
limits, and queue capacity to fit the deployment's memory envelope.

The sample path expects a locally supplied PHP 8.5 NTS distribution. The
`php/` directory is deliberately excluded from Git and must be populated before
starting the checked-in development configuration.

## Path Resolution

An absolute static root is used as written. A relative root is resolved against
the directory containing the selected `hamster.toml`, not the process working
directory. For example, `D:\project\hamster.toml` with `root = "public"`
resolves to `D:\project\public`. PHP executable and script paths use the same
base-directory rule. The static root must be a directory; PHP paths must be
files. All are canonicalized once while compiling the runtime configuration.

The explicit built-in development configuration is used only when no CLI
arguments are supplied. It listens on `127.0.0.1:8080`, maps `localhost` and
`127.0.0.1`, and resolves `public` against the startup working directory once.
`hamster-wheel --config PATH` always uses that file; missing, malformed, or
invalid explicit files never fall back to the built-in configuration.

## Validation Rules

Configuration fails fast for an invalid listen address or HTTP bound; empty or invalid host
names; case-insensitive primary/alias collisions; absent hosts or locations;
invalid or duplicate normalized location paths; unknown handlers; missing or
unexpected handler fields; invalid pool bounds; static roots that cannot be
canonicalized or are not directories; and PHP executable/script paths that are
missing or not files. Errors identify the field plus the host/location index or
name where applicable. No dangerous input is silently repaired, and PHP is not
searched from `PATH`.

## Startup Lifecycle

`RawConfig` mirrors TOML using strings and paths. Validation parses the listen
address, checks global host uniqueness, normalizes location definitions, and
resolves roots against a stable base directory. Compilation canonicalizes static
roots and executable/script paths and constructs the existing vector-based
routers and handler enum. Every compiled `PhpWorkerPool` starts and completes
all READY handshakes before the listener binds. Partial startup is rolled back;
only the completely started `RuntimeConfig` reaches the server.

Shutdown is best-effort across the complete pool set. A failure in one pool is
reported with that pool's identity but does not prevent later pools from being
drained, shut down and reaped. When startup rollback also fails, the original
startup error remains the primary error/source and cleanup failures are
reported as additional evidence.

Configuration failures use `ConfigError` categories for arguments, I/O, parse,
validation, and compile errors. User input never causes a panic; the executable
prints the error and exits unsuccessfully before binding the listener.

## Future Reload Considerations

The immutable object boundary permits a later generation to build a complete
replacement and swap it atomically. This milestone deliberately adds no
`ArcSwap`, watcher, reload endpoint, environment interpolation, includes,
wildcard hosts, or regex locations.
