# Runtime Configuration

For startup, backend ownership, readiness and signal procedures on Windows,
see `doc/WINDOWS_OPERATIONS.md`.

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
# Optional host-wide, component-aware denials applied before routing.
deny_paths = ["/data", "/conf", "/inc", "/bin", "/vendor"]
deny_names = ["composer.json", "composer.lock"]

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
# Budget for each generation to reach READY, in seconds (1-300). Defaults to 5.
startup_timeout = 30
# Optional lifecycle policies; omitted means OFF.
max_requests = 10000
max_worker_age = 3600

# Optional extra environment for the worker process.
[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = "www/pms"
```

A reverse-proxy location forwards to one HTTP/1.1 upstream. Only `upstream`
is required; the other fields show their defaults:

```toml
[[hosts.locations]]
path = "/api"
handler = "proxy"

[hosts.locations.proxy]
upstream = "http://127.0.0.1:3000"
connect_timeout = 5           # seconds, 1-3600
response_header_timeout = 30  # seconds, 1-3600, counted from the end of the upload
progress_timeout = 30         # seconds, 1-3600, inactivity on any body direction
idle_timeout = 4              # seconds, 1-3600
max_idle = 64                 # 0-4096; 0 disables connection reuse
preserve_host = true
```

`upstream` must be `http://host[:port]` (a bare trailing `/` is accepted):
`https`, any other path, a query, user information, port 0, and an explicit
empty, nonnumeric or out-of-range port are rejected,
because v1 has no upstream TLS and forwards the request path unchanged. A hostname is resolved once during compilation, before
the listener binds; an unresolvable name is a `Compile` error, the first
resolved address is used, and it is printed at startup. `progress_timeout` is an
inactivity interval, never a total deadline, so a long upload or download that
keeps moving is not cut. `server.http1_max_buffer_size` also bounds each upstream
connection's HTTP/1 buffers, and with them the largest upstream response head
Hamster accepts (a larger head is a 502). On Windows loopback, values of
128 KiB and above collapsed proxied large transfers at low concurrency, and
memory per stalled exchange grows with the value; 64 KiB is the measured
conservative default for proxy deployments, not a proven optimum. The proxy does not use `server.max_body_memory`,
because it buffers no request body. `idle_timeout` defaults below the 5-second
keep-alive timeout common to several upstream servers so Hamster normally
retires an idle connection first; it is a policy default, not a measured
optimum. See `doc/REVERSE_PROXY_V1.md` for the full semantics.

A FastCGI location runs the PHP scripts under `root` through FastCGI endpoints
the operator starts, for example `php-cgi.exe -b 127.0.0.1:9001` with
`PHP_FCGI_MAX_REQUESTS=0`. Hamster never starts or restarts them. `root` and
`servers` are required; the other fields show their defaults:

```toml
[[hosts.locations]]
path = "/"
handler = "fastcgi"
root = "sites/wiki"

[hosts.locations.fastcgi]
servers = ["127.0.0.1:9001", "127.0.0.1:9002"]  # 1-64, numeric IP:port, no duplicates
concurrency = 1               # 1-256 exchanges per server at once
index = "index.php"           # relative .php path: front controller and directory index
connect_timeout = 5           # seconds, 1-3600: waiting for a slot, then the connect
response_timeout = 60         # seconds, 1-3600: backend inactivity
keep_connections = true       # keep connections for later exchanges (FCGI_KEEP_CONN)
resolve_cache = 1             # seconds, 0-60: reuse a path's script resolution; 0 = off
```

`php-cgi` on Windows serves one connection at a time, so each process is one
`servers` entry with `concurrency = 1`. Hostnames are refused rather than
resolved. On Windows, that single-exchange configuration uses a bounded
blocking-pool TCP connect when `connect_timeout <= 5`; longer timeouts and
multiplexing endpoints use async connect, and only for a server that has no
kept connection: with `keep_connections` (the default) a connection whose
exchange ended cleanly serves the next exchange on that server, and the
connect cost is paid once per connection (`doc/FASTCGI_V1.md`). A canceled HTTP waiter cannot send a FastCGI request from a
detached connect task. If that task has not started, its exchange slot is
released immediately; if it has started, the slot is held until connect
returns. Queued blocking connect work is capped per location, with async
connect fallback when the cap is occupied. The whole
root is otherwise served, and `.htaccess` is not read.
Declare private directories with host-wide `deny_paths`, and sensitive file
basenames with `deny_names`; both return 404 before any location handler sees
the request. Paths match whole components, ignoring ASCII case, so `/data`
blocks `/DATA/file` and `/%64ata/file` but not `/database`. Names match at any
depth. Rules must use plain ASCII path components, without percent escapes,
`..`, trailing dots or spaces. File handlers also deny dotfile components
except root `/.well-known`, and never serve PHP source extensions as bytes.
The final opened file location is checked again, so a junction alias inside
the same root does not bypass a rule. These defaults do not identify every
application-specific private directory: configure those explicitly. See
`doc/FASTCGI_V1.md` for examples and the remaining script-path handoff limit.

An optional HTTPS listener terminates TLS in front of the same hosts. Only
`listen` and at least one certificate are required; the example shows the
default handshake timeout:

```toml
[server]
listen = "0.0.0.0:80"         # optional once [server.tls] exists

[server.tls]
listen = "0.0.0.0:443"
handshake_timeout = 10        # seconds, 1-60, total from accept

[[server.tls.certificates]]
cert = "certs/site-chain.pem" # leaf first, then intermediates
key = "certs/site-key.pem"    # unencrypted PKCS#8, PKCS#1 or SEC1
names = ["example.com", "*.example.net"]  # SNI names and the hosts it serves
default = true                # at most one; used for no or unknown SNI
```

`server.listen` becomes optional when `[server.tls]` is present, so a server
can be HTTPS-only; with neither, validation fails. Both listeners serve every
host and share `max_connections`, the HTTP limits and the router. A TLS
connection that has not finished its handshake occupies a connection slot, and
`handshake_timeout` is a total deadline from accept, not an inactivity timer.
The two listen addresses must not overlap.

`names` are exact DNS names, `*.` plus a parent domain matching exactly one
extra label, or IP addresses. DNS names select the certificate by SNI (exact
names win over wildcards); every name is also a host that a connection which
presented this certificate may be served for. An IP name cannot be selected by
SNI, which never carries an address, and only authorizes a `Host` that is that
address, so it belongs on the default certificate. A name may appear only once
across all certificates. Every configured name is checked against the leaf's
subject alternative names while the configuration compiles, so a certificate
that does not cover a name it claims fails startup instead of every handshake.
A handshake with no SNI, or an SNI nothing matches, uses the `default = true`
certificate, and fails when there is none.

Over TLS, the request's effective HTTP authority still selects the virtual
host, but only among the hosts the presented certificate's `names` cover; any
other authority, including a configured host that no certificate names, is
answered `421 Misdirected Request` (RFC 9110). The cleartext listener is
unaffected.

Certificate and key paths resolve against the configuration file's directory.
Files are read and checked once, before any worker starts or socket binds:
unreadable files, PEM without a certificate, an unreadable, unsupported or
encrypted key, a key that does not match the leaf, a leaf or intermediate that
is expired or not yet valid, and a chain whose certificates do not link from
the leaf to the last one in the file are `Compile` errors naming the field and
path. Key contents never appear in a diagnostic. A file holding only a leaf
whose issuer is not included starts with a printed note, because only clients
know which issuers they trust; startup also prints each certificate's expiry
date. Configured certificate/key files can later be reloaded transactionally
with Ctrl+Break in an attached Windows console, or with the control endpoint
below on a server that has none; listeners, names and other configuration do
not reload. TLS 1.2 and 1.3 use rustls's defaults with the aws-lc-rs provider,
and ALPN advertises only `http/1.1`. See `doc/TLS_V1.md` and
`doc/TLS_CERTIFICATE_RELOAD.md` for the full semantics.

An optional local control endpoint gives a server with no console the graceful
stop and the certificate reload that Ctrl+C and Ctrl+Break provide in one:

```toml
[server.control]
name = "edge"                 # ASCII letters, digits, '-', '_', '.'; 1-64 long
```

`name` is the only key, and the endpoint exists only when it is present. It is
a Windows named pipe at `\\.\pipe\hamster-wheel-control-<name>`, so the key is
rejected while compiling a configuration on any other platform. A name already
in use is a startup failure rather than a shared endpoint. The pipe's DACL is
set at creation and protected: LocalSystem and the pipe's object-owner SID
have full access and nobody else has any; verify the actual Service token's
default owner rather than assuming it equals its user SID. Remote
clients are rejected. Anyone who can open it can stop the server.

There is no concurrency key: the endpoint handles four exchanges at once, and a
connection beyond that is answered `ERR busy` within eight bounded refusal
slots, which the client retries inside its own five-second budget. Beyond
those slots it closes without a guaranteed frame; that is a client error,
not success. One five-second exchange deadline covers all phases. The
listener owns and joins its exchanges on shutdown. Sustained overload can
still prevent an operator command: this is bounded admission, not a fairness
or reserved-operator-slot guarantee.

The same binary is the client, and needs the same configuration file because
that file names the endpoint:
`hamster-wheel --config hamster.toml --control status|ping|stop|reload-certificates`.
The client reads only `[server.control]` and compiles nothing else, so a
certificate that no longer loads does not stop `reload-certificates` from
reaching the running server. The server's answer is printed verbatim and the
exit code is 0 for `OK`, 1 for `ERR`. See `doc/WINDOWS_OPERATIONS.md` for the
operator runbook.

An optional per-request log records what the server served. It is off unless
the section is present, and its absence means no queue, no writer task and no
per-request work:

```toml
[server.access_log]
path = "logs/access.jsonl"   # omit for stdout; relative resolves beside this file
include_query = false        # default; credential-shaped values are replaced
queue_capacity = 1024        # 16-65536
rotate_bytes = 10485760      # 0 or absent: never rotate; at least 4096; needs path
keep = 4                     # archives kept, 0-64; needs rotate_bytes
```

Records are JSON Lines: one object per request, plus `lifecycle` records for
what the server itself did (startup, listeners, workers, reloads, drain, stop).
With `rotate_bytes` the log's own writer rotates the file at that size and
keeps `keep` archives beside it, splitting only at a record boundary. The file is opened at startup,
before pools start and before listeners bind, so a path that cannot be written
is a startup failure; a path naming an existing directory, an empty path and a
queue outside the range are configuration errors. Hamster appends and never
rotates: stream rotation belongs to the Service wrapper or to an out-of-process
tool.

No headers other than the validated authority are logged, and no bodies. A
query string is omitted unless `include_query = true`, in which case the values
of credential-shaped keys are replaced with `REDACTED`. The queue is bounded
and never waited on: when it is full the record is dropped, counted, reported
in the log as a `log_dropped` record and shown by the control endpoint's
`status` as `access_log=<target> queued= written= dropped= failed=`. See
`doc/ACCESS_LOG_0_8.md` for the record fields and the complete behaviour.

A configuration file can be checked without running it:

```powershell
hamster-wheel.exe --config D:\site\hamster.toml --validate
```

This compiles the file exactly as a server start would — resolving roots,
loading certificates, validating pool, proxy and FastCGI settings — then prints
the resources it would serve as `key=value` lines and exits 0. Run it before
`--control reload`, which compiles the same file inside the running server. Any error is
reported on stderr with exit 1. It starts nothing: no listener is bound, no PHP
worker is spawned, no log file is created, so it is safe to run on the machine
that is currently serving. The last line names what it did **not** check:
`not_checked=listener_bind,log_write,worker_startup,backend_reachability`.

A running server can re-read this file with `--control reload`. It publishes
the next **generation** when the change is one a reload may make — the route
graph, static roots, `deny_paths`, `deny_names`, host aliases, and the
per-connection timeouts — and refuses it by name otherwise. Everything that
owns operating-system state is carried unchanged into the published
generation, so `server.listen`, `server.tls`, `server.control`,
`server.admin`, `server.access_log`, `server.max_body_memory`,
`server.http1_max_buffer_size`, and any change to a PHP pool, proxy upstream
or FastCGI endpoint (including adding or removing one) need a restart instead.
`max_connections` is read once by the accept loop. See
`doc/CONFIG_RELOAD_0_8.md`.

An optional administrative listener serves health, readiness and metrics. It
is off unless the section is present, and it is a separate socket that never
receives the application router, so it cannot occupy an application route:

```toml
[server.admin]
listen = "127.0.0.1:9090"
```

`listen` is the only key. It is bound before pools start, so an address already
in use is a startup failure; naming an address an application listener already
uses is a configuration error. The listener serves exactly `GET`/`HEAD` on
`/health`, `/ready` and `/metrics`, answers 405 for other methods and 404 for
other paths, and has no TLS and no authentication: keep it on loopback and let
the network decide who reaches it. Binding it to a non-loopback address logs a
warning naming what became reachable. `/ready` answers 503 while the server
drains and while a PHP pool has no live worker; `/health` answers 200 whenever
the process can answer at all. See `doc/OBSERVABILITY_0_8.md` for the metric
list and the readiness rules.

On Windows, the presence of any `php_worker` location also requires process
containment to be established before worker startup. Hamster creates and fully
configures a private kill-on-close Job Object, assigns itself, and only then
starts the configured pools. Failure to create, configure, or join that job is
a startup error: no PHP worker is spawned and the listener is not bound. There
is deliberately no uncontained fallback. This does not add a configuration
switch; it is a correctness property of Windows PHP execution.

Ordinary worker descendants inherit containment, including replacement
generations and direct children created by the application. Breakaway is not
enabled. Brokered process creation such as WMI `Win32_Process.Create` is outside
the inherited process-tree contract. On non-Windows platforms these settings do
not imply an equivalent OS containment mechanism.

`server.listen` and `server.tls.listen` must be numeric IP socket addresses.
`max_connections` bounds accepted connections owned by the process, across both
listeners and including TLS handshakes in progress, and defaults to 1024 (valid
range 1-65536). At the bound, Hamster stops accepting until a connection task exits;
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

`body_read_timeout` also bounds lingering close. When Hamster answers a request
before reading its body (a declared body over the PHP limit is `413`, over the
body budget `503`), it sends the response and FIN and then keeps reading and
discarding what the client still sends, so the client, which may send its whole
body before reading, receives the status instead of a TCP reset. That discard
stops when the client closes, when nothing arrives for one second, when
`body_read_timeout` has passed, or immediately once server shutdown has begun;
a refused body is never read longer than an accepted one would be, and there is
no byte bound. Measured with .NET `HttpClient` against a 1 MiB PHP limit:
refused uploads of 2-100 MiB and a 4 MiB upload sent at 640 KiB/s all received
`413`; a client pausing longer than one second between writes is still reset.
The connection keeps its `max_connections` slot while lingering. See
`src/lingering_close.rs`.

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
`504`s.

Exceeding it is not an ordinary request failure. Protocol v1 has no cancel
frame, so the only way to reclaim a worker that is still mid-transaction is to
kill it; the request is answered `504` with the body
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

`startup_timeout` is the budget, in seconds, for one worker generation to
reach READY: process start, the application's bootstrap and the READY frame.
It applies to the initial workers, whose failure fails server startup, and to
every replacement and recycle, whose failure counts against the slot's
backoff. It is configured per `php_worker` location, defaults to 5 (the value
that was fixed before it became configurable) and must be between 1 and 300.
Raise it for an application whose first start is slow. Measured on the Windows
development machine: reading 10,072 PHP files took 100.7 s the first time and
0.47 s the second (on-access scanning was enabled), so a cold first start is
dominated by opening files the machine has not opened recently; a Laravel
stand-in went from 0.25 s warm to 3.4-3.8 s cold, and Symfony Demo in its
debug environment needed 9.9-11.7 s. A larger value also lets a replacement
that hangs in bootstrap keep its slot out of service for that long before the
failure is counted.

An execution timeout is deliberately *not* charged to the slot's
infrastructure failure history, so repeated slow requests cannot walk a slot
into the replacement backoff ladder or its cooldown band. Crashes, I/O faults
and protocol faults still are. Both counts are visible in the pool snapshot,
and each timeout writes one line to standard error naming the condition. The
status is `504` because the upstream application did not respond before the
deadline; `502` remains for a broken/invalid worker response. This status
does not authorize a retry: the request may already have had side effects,
and Hamster never replays it. See
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

PHP responses remain buffered protocol frames; the response frame shape is
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
base-directory rule, as do TLS certificate and key paths. The static root must
be a directory; PHP paths must be files. All are canonicalized once while compiling the runtime configuration.

The explicit built-in development configuration is used only when no CLI
arguments are supplied. It listens on `127.0.0.1:8080`, maps `localhost` and
`127.0.0.1`, and resolves `public` against the startup working directory once.
`hamster-wheel --config PATH` always uses that file; missing, malformed, or
invalid explicit files never fall back to the built-in configuration.

## Validation Rules

Configuration fails fast for an invalid or overlapping listen address, no
listener at all, or an HTTP bound out of range; invalid TLS names, duplicate
names, more than one default certificate, TLS files that cannot be read or used
together, names the certificate does not cover, certificates outside their
validity dates and chains that do not link; empty or invalid host
names; case-insensitive primary/alias collisions; absent hosts or locations;
invalid or duplicate normalized location paths; unknown handlers; missing or
unexpected handler fields; invalid pool bounds; static roots that cannot be
canonicalized or are not directories; PHP executable/script paths that are
missing or not files; and proxy upstreams that are not `http://host[:port]`,
cannot be resolved, or carry out-of-range timeouts or `max_idle`; and FastCGI
locations without a root or `servers`, with nonnumeric, duplicate or port-0
servers, out-of-range `concurrency` or timeouts, or an `index` that is not a
relative `.php` path inside the root. Errors identify the field plus the host/location index or
name where applicable. No dangerous input is silently repaired, and PHP is not
searched from `PATH`.

## Startup Lifecycle

`RawConfig` mirrors TOML using strings and paths. Validation parses the listen
address, checks global host uniqueness, normalizes location definitions, and
resolves roots against a stable base directory. Compilation canonicalizes static
roots and executable/script paths, loads and checks TLS certificates and keys,
and constructs the existing vector-based routers and handler enum. Every compiled `PhpWorkerPool` starts and completes
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
