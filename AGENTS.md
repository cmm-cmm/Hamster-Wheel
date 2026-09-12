# Repository Guidelines

## Project Structure & Module Organization

Architecture and milestones live in `doc/HAMSTER_WHEEL_SERVER_PLAN.md`; `README.md` covers the day-to-day workflow. The repository is one Rust package: `src/` holds the server, router, configuration, static files, and PHP worker; `tests/` holds integration tests; unit tests sit beside their modules; `benches/` holds benchmarks. Supporting directories are `fixtures/` (PHP worker fixtures), `public/` (default document root), `deploy/` (configuration shipped with the container image), and `scripts/` (development entry points). Keep the single package until a subsystem has a concrete reason to become a crate under `crates/` (for example, `hamster-http` or `hamster-php`).

## Build, Test, and Development Commands

`./scripts/check.sh` (PowerShell: `./scripts/check.ps1`) runs format, lint, and test in one pass, which is exactly what CI enforces. `./scripts/dev.sh` starts the server against a generated development configuration, and `./scripts/smoke.sh` verifies that a release build serves traffic. The individual commands:

- `cargo run` starts the development server (the baseline target is `127.0.0.1:8080`).
- `cargo build --release` creates the only build suitable for performance comparisons.
- `cargo test` runs unit and integration tests.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` rejects lint warnings.

`docker compose run --rm dev ./scripts/check.sh` runs the same checks in a container that already has the toolchain and a PHP CLI, and `docker compose up server` runs the release image. See `doc/DEPLOYMENT.md`.

The PHP worker integration tests need a PHP CLI. They use `HAMSTER_PHP`, then the repository's `php/8.5/php.exe`, then the first `php` on `PATH`, and report themselves skipped on stderr when none exists. Never let a skipped worker test pass for a verified one.

Never report debug-build results as performance data. Record RPS, latency percentiles, CPU, RAM, and test conditions.

## Coding Style & Naming Conventions

Use stable Rust and default `rustfmt` formatting. Name modules, functions, and files in `snake_case`; types and traits in `UpperCamelCase`; constants in `SCREAMING_SNAKE_CASE`. Keep the request hot path measurable: avoid synchronous logging, unnecessary allocation, global locks, and runtime configuration parsing. Add dependencies and abstractions only for an active milestone. Document non-obvious choices with an ADR or research note.

## Testing Guidelines

Use Rust's built-in test harness unless another framework is formally adopted. Name tests by behavior, such as `rejects_path_traversal`. Cover malformed HTTP, path normalization, configuration validation, graceful shutdown, worker state isolation, timeouts, and crash recovery. Bug fixes need a regression test; performance changes need comparable release benchmarks.

## Commit & Pull Request Guidelines

Use short, imperative subjects, optionally scoped, such as `http: reject invalid host headers`. Keep commits focused. Pull requests should explain the problem and approach, list verification commands, link relevant issues or ADRs, and include benchmark evidence for hot-path changes. Add screenshots only for manager/UI changes, and call out platform-specific behavior on Windows or Linux.

## Security & Configuration

Never commit certificates, private keys, `.env` files, or local `hamster.toml` secrets. Treat path traversal prevention, request limits, invalid hosts, safe TLS defaults, and worker isolation as correctness requirements rather than optional hardening.
