# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Read `AGENTS.md` too; it holds the repository conventions. `doc/HAMSTER_WHEEL_SERVER_PLAN.md` is
the long-form roadmap and is written in Vietnamese.

## What this is

Hamster Wheel is a Rust HTTP server (hyper 1 + Tokio, edition 2024) built milestone by milestone.
Current version is 0.3.0 (PHP Worker Lifecycle), implemented on the Windows target. It serves
static files, routes by host/location, loads configuration from TOML, and runs a fixed pool of
persistent PHP CLI workers with automatic replacement over a custom binary stdio protocol. Windows
(`x86_64-pc-windows-msvc`) is the primary development and measurement platform.

## Commands

```powershell
cargo run                                      # built-in dev config on 127.0.0.1:8080
cargo run -- --config hamster.toml             # explicit config file
cargo build --release --target x86_64-pc-windows-msvc   # the only build valid for perf data
cargo test                                     # unit + integration tests (PHP tests are #[ignore]d)
cargo test --test static_files                 # one integration test file
cargo test rejects_path_traversal              # one test by name
cargo test -- --ignored                        # PHP worker tests; requires php/8.5/php.exe
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Benchmarks are feature-gated behind `profiling` and use `harness = false`; each takes a suite name
after `--`:

```powershell
cargo bench --target x86_64-pc-windows-msvc --bench php_ipc --features profiling -- raw
cargo bench --target x86_64-pc-windows-msvc --bench windows_filesystem --features profiling -- handoff
cargo bench --target x86_64-pc-windows-msvc --bench configuration --features profiling
```

Suites: `php_ipc` = all|raw|variants|blocking|sync|stderr|timeline|timeline_blocking;
`windows_filesystem` = all|handoff|backend|files|operations. The other benches
(`static_pipeline`, `blocking_pool`, `routing`) take no argument. Omitting
`-- <suite>` runs `all`. Use `cargo bench --no-run --features profiling` as the
link gate — `cargo clippy --all-targets` checks benches without linking them, so
it cannot catch a missing platform library.

End-to-end load drivers are PowerShell + .NET `HttpClient` scripts that start the release binary
themselves, so build release first:

```powershell
./benchmarks/static_http.ps1 -Runs 3 -Requests 500000 -Concurrency 32
./benchmarks/php_worker.ps1 -Runs 3 -Requests 20000 -WarmupRequests 100
./benchmarks/php_worker_concurrency.ps1
./benchmarks/php_worker_soak.ps1 -Requests 100000
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 -WarmupRequests 1000 -MinimumSeconds 8
```

`php/` is git-ignored. Supply a PHP 8.5 NTS distribution at `php/8.5/php.exe` before using
`hamster.toml`, the PHP benchmarks, or the ignored PHP tests.

## Architecture

Request path, all of it built once at startup and shared by `Arc`:

```
TcpListener -> hyper http1 conn -> HostRouter -> LocationRouter -> Handler -> Response
```

- `main.rs` parses argv (`config::parse_arguments`), loads config, calls `server::run`.
- `config.rs` is a three-stage pipeline: `RawConfig` (serde/TOML, `deny_unknown_fields`) ->
  `ValidatedConfig` (all validation, error messages carry host/location indices) -> `RuntimeConfig`
  (canonicalized paths, compiled routers, started PHP worker pools). Nothing downstream re-reads
  TOML or resolves paths. Relative paths resolve against the config file's directory, not the CWD;
  the built-in config (no CLI args) resolves against the startup CWD.
- `router.rs` holds `HostRouter` (small `Vec<VirtualHost>`, exact ASCII-case-insensitive Host match,
  port ignored) and `LocationRouter` (prefixes normalized and sorted by descending length once,
  linear longest-prefix scan on segment boundaries, `/` is the catch-all). Missing/invalid/duplicate
  Host -> 400, unknown host -> 404. Wildcards and regex are intentionally absent.
- `handler.rs` is a plain enum (`Static` / `Simple` / `PhpWorker`), not an async trait.
- `static_files.rs` resolves per request: percent-decode + normalize -> join root -> metadata ->
  (append `index.html` for a directory) -> canonicalize -> `starts_with(root)` containment ->
  metadata -> read. HEAD deliberately skips the body read. Per-request canonicalization is what
  rejects symlink/junction escapes; do not remove it for speed.
- `php_worker.rs` + `php_protocol.rs`: a fixed target of `php.exe` children spawned directly (no
  shell, no FastCGI), stdio carries only frames, and stderr is drained per child. Stable slots own
  monotonically increasing generations; PID is not identity. The topology is a bounded FIFO job
  queue (`queue_capacity`, 1-128, default `min(workers * 8, 128)`) -> dispatcher -> generation-specific
  capacity-1 mailboxes -> exclusive worker tasks. `IdleWorker` carries slot, generation, and that
  generation's mailbox; the dispatcher has no lookup table. There is no `Arc<Mutex<PhpWorker>>` or
  global PHP execution lock. HTTP admission uses `try_send`; full or unavailable is controlled 503.
- Transaction ownership is cancellation-safe and this is a correctness requirement, not a
  refinement. Once a REQUEST frame starts, the owning worker task always consumes the RESPONSE (or
  fails the worker), so a disconnected HTTP client can never desynchronize the protocol stream. A job
  whose receiver is already gone is skipped before execution begins; a job is re-dispatched only when
  the mailbox send proves PHP never started, so a possibly-started request is never retried. Any I/O,
  protocol, request-ID mismatch, or timeout marks that worker `Unavailable`, kills it, and ends its
  task. The lifecycle supervisor replaces failed generations without retrying their request;
  surviving workers serve degraded until replacement READY. Only live generations or pending
  replacement/backoff tasks hold `idle_tx`; the supervisor never keeps a clone. Thus zero READY can
  be transient, while `idle_rx` closure means terminal unavailability. Startup remains
  all-or-nothing. Backoff is per slot (immediate, then 100 ms doubling to 5 s), with retirement after
  more than 10 consecutive failures and stability reset after 60 s plus one success. Optional
  `max_requests` and `max_worker_age` recycle policies default OFF and use serialized
  stop-then-spawn. No adaptive sizing, proactive idle probe, or application Reset exists in 0.3.0.
- `queue_capacity` bounds admitted job count, not bytes. Request bodies are buffered before pool
  admission, so a full queue can retain roughly 128 MiB at the 1 MiB body limit. A total-byte
  admission budget and body streaming are deferred, not implemented.
- Wire format is version 1: `[u32 BE length][payload]`, first payload byte is the kind
  (READY/REQUEST/RESPONSE/SHUTDOWN/SHUTDOWN_ACK). Limits: 8 MiB frame, 1 MiB request body,
  128 headers. Body over the limit -> 413 without touching the worker. Worker-supplied
  hop-by-hop headers, `Content-Length`, or `Server` are rejected -> 502. Changing framing means
  changing `php_protocol.rs` and `fixtures/php-worker/protocol.php` together.
- `server.rs` owns the accept loop and graceful shutdown: worker pools start before the listener
  binds, Ctrl+C stops accepts, a `watch` channel tells live connections to `graceful_shutdown`, then
  the pools are shut down (stop admission, drain already-accepted jobs, SHUTDOWN / SHUTDOWN_ACK,
  then kill on timeout, reap, join; `kill_on_drop` is the orphan fallback).
- `*_profile.rs` modules and `profiling.rs` are `#[cfg(feature = "profiling")]` only and exist to
  feed `benches/`. They must never affect the default build's hot path.

## Working rules specific to this repo

- Platform status for 0.3.0: Windows `x86_64-pc-windows-msvc` is the verified target; Linux is compile-checked
  only (cross-target `cargo check`/`clippy` from Windows) with native verification deferred. Do not
  describe Linux as supported, and do not report a cross-target check as a native run.
- Every milestone's behavior, measurements, and explicitly rejected alternatives are written down in
  `doc/`. When you change behavior in an area, update its doc (`ROUTING.md`, `CONFIGURATION.md`,
  `PHP_WORKER.md`, `BENCHMARKS.md`, `WINDOWS_STATIC_PROFILING.md`).
- `doc/BENCHMARKS.md` results are historical and immutable — append new sections, never rewrite old
  numbers. Report medians of at least three runs, release builds only, with the conditions recorded.
- The PHP worker's Windows latency fix is `['pipe' => ['blocking' => true]]` on `php://stdin` in
  `fixtures/php-worker/worker.php`. Samples clustering near the 15.625 ms Windows timer quantum mean
  that path regressed; the benchmark script flags those multiples explicitly.
- Keep the hot path free of per-request config parsing, path canonicalization of the root, global
  locks, and synchronous logging. Add dependencies only for an active milestone.
- Tests are named for behavior (`rejects_path_traversal`). Unit tests live beside their module,
  cross-module tests in `tests/`. Security coverage (traversal, encoded separators, NUL, Windows
  drive/ADS syntax, reparse points) is a correctness requirement, not optional.
- Commit subjects are short, imperative, and usually scoped: `php: investigate Windows IPC latency`.
