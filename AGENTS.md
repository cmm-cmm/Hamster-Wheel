# Repository Guidelines

## Project Structure & Module Organization

Hamster Wheel is one Rust package (`hamster-wheel`, edition 2024, currently 0.3.1). Architecture and
milestones live in `doc/HAMSTER_WHEEL_SERVER_PLAN.md`; each shipped milestone also has a behavior
document in `doc/` (`ROUTING.md`, `CONFIGURATION.md`, `PHP_WORKER.md`, `BENCHMARKS.md`,
`WINDOWS_STATIC_PROFILING.md`, `ACCESS_LOG_0_8.md`, `OBSERVABILITY_0_8.md`). The accepted 0.3 lifecycle contract is
`doc/WORKER_LIFECYCLE_0_3.md`.

`src/` holds `main.rs`, `lib.rs`, and the runtime modules `server`, `config`, `router`, `handler`,
`static_files`, `php_worker`, `php_protocol`, `proxy`, `tls`, `access_log`, `metrics`, `admin`, `validate`, `fastcgi` (FastCGI to operator-started
`php-cgi`, see `doc/FASTCGI_V1.md`) and their helpers. The `*_profile.rs` modules plus `profiling.rs` are
`#[cfg(feature = "profiling")]` measurement code and must not affect the default build. Integration
tests live in `tests/`, unit tests beside their modules, Cargo benches in `benches/`, PowerShell load
drivers and their configs in `benchmarks/`, PHP worker fixtures in `fixtures/php-worker/`, FastCGI fixtures in `fixtures/php-cgi/`, and the
static document root in `public/`. Keep it a single package until a subsystem has a concrete reason
to become a crate under `crates/` (for example, `hamster-http` or `hamster-php`).

`php/` is deliberately git-ignored. A locally supplied PHP 8.5 NTS distribution must exist at
`php/8.5/php.exe` before the checked-in `hamster.toml`, the PHP benchmarks, or the ignored PHP
integration tests can run. On Linux and other non-Windows hosts the tests and benches look for
`php/8.5/php` instead, so a symlink to the system binary is enough:
`mkdir -p php/8.5 && ln -s "$(command -v php8.5)" php/8.5/php`. `hamster.toml` still names
`php.exe`; point its `executable` at `php/8.5/php` in a local copy to run it there. The crash and
kill tests use `taskkill`/`tasklist` on Windows and `kill`/`/proc` elsewhere. The Laravel tests
additionally need `HAMSTER_LARAVEL_TEST_PATH`; `fixtures/php-worker/pms/prepare_scratch_app.sh`
builds a PMS-shaped Laravel 12 + Octane application for it under the git-ignored `www/`, and
`fixtures/php-apps/prepare_php_apps.sh` builds the `HAMSTER_PHP_APPS` workspace (Symfony Demo needs
PHP 8.4 or newer). The FastCGI tests look for `php/8.5/php-cgi`, a symlink to the system `php-cgi`.

## Build, Test, and Development Commands

- `cargo run` starts the development server with the built-in config (`127.0.0.1:8080`).
- `cargo run -- --config hamster.toml` starts it from an explicit configuration file.
- `cargo run -- --config hamster.toml --validate` compiles that file, prints the
  resources it would serve, and starts nothing; exit 1 on any error.
- `cargo build --release --target x86_64-pc-windows-msvc` creates the only build suitable for
  performance comparisons.
- `cargo test` runs unit and integration tests; `cargo test -- --ignored` adds the PHP worker tests.
  The Laravel tests need `HAMSTER_LARAVEL_TEST_PATH` and the application tests `HAMSTER_PHP_APPS`;
  both workspaces are built by scripts described in `doc/LOCAL_TEST_APPLICATIONS.md`.
- `cargo test --test static_files` or `cargo test rejects_path_traversal` runs one file or one test.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` rejects lint warnings.
- `cargo bench --target x86_64-pc-windows-msvc --bench <name> --features profiling -- <suite>` runs a
  profiling bench. Benches use `harness = false` and require the `profiling` feature; `php_ipc`
  accepts `all|raw|variants|blocking|sync|timeline|timeline_blocking` and `windows_filesystem`
  accepts `all|handoff|backend|files|operations`. Omitting `-- <suite>` selects
  `all`.
- `cargo bench --no-run --features profiling` is the gate that links every bench;
  `cargo clippy --all-targets` only checks them and misses linker errors.
- `./benchmarks/static_http.ps1` and `./benchmarks/php_worker*.ps1` are end-to-end .NET load drivers
  that launch the release binary themselves, so build release first.
- `benchmarks/linux_http.py` is the Linux counterpart (static, `not_found`, PHP worker and FastCGI
  cells, client `oha`); `doc/LINUX_VERIFICATION.md` records how Linux was verified and what is not.
- `benchmarks/frankenphp_linux.py --frankenphp PATH [--laravel APP]` runs the same cells against
  FrankenPHP on the same host, interleaved; results are in `doc/FRANKENPHP_LINUX_COMPARISON.md`.

Never report debug-build results as performance data. Record RPS, latency percentiles, CPU, RAM, and
test conditions.

## Coding Style & Naming Conventions

Use stable Rust and default `rustfmt` formatting. Name modules, functions, and files in
`snake_case`; types and traits in `UpperCamelCase`; constants in `SCREAMING_SNAKE_CASE`. Keep the
request hot path measurable: avoid synchronous logging, unnecessary allocation, global locks, and
runtime configuration parsing. Configuration is parsed, validated, and compiled once into an
immutable `RuntimeConfig`; request handling uses only the already-compiled router and handler graph.
Add dependencies and abstractions only for an active milestone. Document non-obvious choices with an
ADR or research note, and update the relevant `doc/` file in the same change.

Operational output in `src/` goes through `outln!` and `errln!` (`src/output.rs`), never `println!`
or `eprintln!`: the standard macros panic once a supervisor or log shipper closes the pipe, and a
panic on the main task, in a pool supervisor or in the reload task ends shutdown, replacement or
signal handling. Tests and benches keep the standard macros.

## Testing Guidelines

Use Rust's built-in test harness unless another framework is formally adopted. Name tests by
behavior, such as `rejects_path_traversal`. Cover malformed HTTP, path normalization, configuration
validation, graceful shutdown, worker state isolation, timeouts, and crash recovery. Tests that need
the local PHP distribution are marked `#[ignore]` with a reason. Bug fixes need a regression test;
performance changes need comparable release benchmarks.

## Benchmarking & Measurement

`doc/BENCHMARKS.md` is an append-only record: earlier milestone results are immutable and must not be
rewritten or reinterpreted to flatter a later comparison. Report medians of at least three runs from
a release build, and state the machine, concurrency, request count, and warm-up used. PHP worker
latency samples clustered near the Windows 15.625 ms timer quantum indicate the blocking-pipe path in
`fixtures/php-worker/worker.php` regressed; treat that as a release blocker rather than noise.

## Commit & Pull Request Guidelines

Use short, imperative subjects, optionally scoped, such as `http: reject invalid host headers`. Keep
commits focused. Pull requests should explain the problem and approach, list verification commands,
link relevant issues or ADRs, and include benchmark evidence for hot-path changes. Add screenshots
only for manager/UI changes, and call out platform-specific behavior on Windows or Linux.

## Security & Configuration

Never commit certificates, private keys, `.env` files, local `hamster.toml` secrets, PHP binaries, or
ETW traces. Treat path traversal prevention, per-request target canonicalization and root
containment, request limits, invalid hosts, safe TLS defaults, and worker isolation as correctness
requirements rather than optional hardening. The PHP worker is spawned directly with a trusted
executable and script path, never through a shell and never resolved from `PATH`.
