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
```

`server.listen` must be a numeric IP socket address. Host names and aliases use
the router's restricted DNS-name grammar, do not include a port, and match
requests exactly and ASCII case-insensitively. Handler names are case-sensitive;
only `static`, `not_found`, and `php_worker` are supported. A static location
requires `root`; a PHP worker location requires `php.executable` and
`php.script`. `workers` defaults to 1 and must be 1-64. `queue_capacity`
defaults to `min(workers * 8, 128)` and must be 1-128. Handler-specific fields
on another handler are errors. Milestone 0.2.0 permits one PHP worker pool
location. Unknown fields are errors.

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

Configuration fails fast for an invalid listen address; empty or invalid host
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
