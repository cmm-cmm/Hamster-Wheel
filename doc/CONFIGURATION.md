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

These three limits bound connection tasks, slow/idle request heads and HTTP/1
parser buffering. They do not constitute a total process-memory budget. PHP
request bodies are still buffered separately up to the protocol's 1 MiB body
limit, and a client may hold a body read after its head is complete. The header
deadline deliberately stops before handler execution, so it cannot cancel a
possibly-started application transaction.

Host names and aliases use
the router's restricted DNS-name grammar, do not include a port, and match
requests exactly and ASCII case-insensitively. Handler names are case-sensitive;
only `static`, `not_found`, and `php_worker` are supported. A static location
requires `root`; a PHP worker location requires `php.executable` and
`php.script`. `workers` defaults to 1 and must be 1-64. `queue_capacity`
defaults to `min(workers * 8, 128)` and must be 1-128. Handler-specific fields
on another handler are errors. `max_requests` is an optional positive request
count. `max_worker_age` is an optional integer number of seconds and must be at
least 1. Both default OFF. Milestone 0.3.0 permits one PHP worker pool location.
Unknown fields are errors.

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
HTTP request memory. PHP request bodies are currently buffered before pool
admission. At the 1 MiB body limit, a full 128-job queue can therefore retain
about 128 MiB of payload, before metadata, allocator overhead, in-flight jobs,
and bodies still being read by HTTP tasks. A total-byte admission budget and
body streaming are deferred; configure a smaller queue when the deployment's
memory envelope requires it.

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
unexpected handler fields; multiple PHP worker locations; invalid pool bounds;
static roots that cannot be
canonicalized or are not directories; and PHP executable/script paths that are
missing or not files. Errors identify the field plus the host/location index or
name where applicable. No dangerous input is silently repaired, and PHP is not
searched from `PATH`.

## Startup Lifecycle

`RawConfig` mirrors TOML using strings and paths. Validation parses the listen
address, checks global host uniqueness, normalizes location definitions, and
resolves roots against a stable base directory. Compilation canonicalizes static
roots and executable/script paths and constructs the existing vector-based
routers and handler enum. A compiled `PhpWorker` starts and completes its READY
handshake before the listener binds. Only the finished `RuntimeConfig` reaches
the server.

Configuration failures use `ConfigError` categories for arguments, I/O, parse,
validation, and compile errors. User input never causes a panic; the executable
prints the error and exits unsuccessfully before binding the listener.

## Future Reload Considerations

The immutable object boundary permits a later generation to build a complete
replacement and swap it atomically. This milestone deliberately adds no
`ArcSwap`, watcher, reload endpoint, environment interpolation, includes,
wildcard hosts, or regex locations.
