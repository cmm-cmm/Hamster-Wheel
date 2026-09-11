# Repository Guidelines

## Project Structure & Module Organization

This repository is in the planning phase. Architecture and milestones live in `Doc/HAMSTER_WHEEL_SERVER_PLAN.md`. The planned Phase 0 layout is one Rust package with `Cargo.toml` and `src/{main,server,config}.rs`. Keep it small until a subsystem has a concrete reason to become a crate under `crates/` (for example, `hamster-http` or `hamster-php`). Put integration tests in `tests/`, unit tests beside their modules, and benchmarks in `benches/`.

## Build, Test, and Development Commands

There is no Cargo project yet, so these commands become applicable after Phase 0 is scaffolded:

- `cargo run` starts the development server (the baseline target is `127.0.0.1:8080`).
- `cargo build --release` creates the only build suitable for performance comparisons.
- `cargo test` runs unit and integration tests.
- `cargo fmt --all -- --check` verifies standard Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` rejects lint warnings.

Never report debug-build results as performance data. Record RPS, latency percentiles, CPU, RAM, and test conditions.

## Coding Style & Naming Conventions

Use stable Rust and default `rustfmt` formatting. Name modules, functions, and files in `snake_case`; types and traits in `UpperCamelCase`; constants in `SCREAMING_SNAKE_CASE`. Keep the request hot path measurable: avoid synchronous logging, unnecessary allocation, global locks, and runtime configuration parsing. Add dependencies and abstractions only for an active milestone. Document non-obvious choices with an ADR or research note.

## Testing Guidelines

Use Rust's built-in test harness unless another framework is formally adopted. Name tests by behavior, such as `rejects_path_traversal`. Cover malformed HTTP, path normalization, configuration validation, graceful shutdown, worker state isolation, timeouts, and crash recovery. Bug fixes need a regression test; performance changes need comparable release benchmarks.

## Commit & Pull Request Guidelines

No Git history is present, so no existing commit convention can be inferred. Use short, imperative subjects, optionally scoped, such as `http: reject invalid host headers`. Keep commits focused. Pull requests should explain the problem and approach, list verification commands, link relevant issues or ADRs, and include benchmark evidence for hot-path changes. Add screenshots only for manager/UI changes, and call out platform-specific behavior on Windows or Linux.

## Security & Configuration

Never commit certificates, private keys, `.env` files, or local `hamster.toml` secrets. Treat path traversal prevention, request limits, invalid hosts, safe TLS defaults, and worker isolation as correctness requirements rather than optional hardening.
