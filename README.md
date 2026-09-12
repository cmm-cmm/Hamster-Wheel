# Hamster Wheel

An HTTP server written in Rust with static file serving, host and location
routing, compiled-once configuration, and a persistent PHP worker prototype.
One PHP process bootstraps once and then serves requests in a loop — the
"hamster wheel" the project is named after.

Kế hoạch phát triển chi tiết: [`doc/HAMSTER_WHEEL_SERVER_PLAN.md`](doc/HAMSTER_WHEEL_SERVER_PLAN.md).

## Requirements

| Component | Version | Needed for |
| --- | --- | --- |
| Rust (stable) | 1.85+ (edition 2024) | building and testing |
| PHP CLI | 8.2+ | the `php_worker` handler and its integration tests |
| Docker | 24+ | the container image and `docker compose` workflow |

PHP is optional: without it the server still serves static files and the PHP
worker tests report themselves skipped.

## Quick start

```bash
git clone <repository> && cd hamster-wheel

./scripts/dev.sh          # build and serve on http://127.0.0.1:8080
./scripts/check.sh        # fmt + clippy + tests, exactly what CI runs
./scripts/smoke.sh        # release build, start, probe /, /api, /php, stop
```

`scripts/dev.sh` generates `target/hamster.dev.toml` from the repository paths
and whichever PHP CLI it finds, so the same command works on Linux, macOS,
WSL, and inside the development container. Override the pieces it guesses:

```bash
HAMSTER_LISTEN=0.0.0.0:9000 HAMSTER_PHP=/opt/php/8.4/bin/php ./scripts/dev.sh
```

Without the scripts:

```bash
cargo run                              # built-in config, static files on 127.0.0.1:8080
cargo run -- --config hamster.toml     # Windows development config
cargo build --release                  # the only build valid for benchmarks
cargo test
```

On Windows the checked-in `hamster.toml` expects a PHP runtime at
`php/8.5/php.exe`. That directory is deliberately ignored by Git — supply the
runtime locally, never vendor binaries.

## Containers

```bash
docker compose up server                     # release image on http://127.0.0.1:8080
docker compose run --rm dev ./scripts/check.sh
docker compose run --rm --service-ports dev ./scripts/dev.sh
```

The `server` image is a multi-stage build: a Rust builder produces the release
binary, and the runtime layer is `php:8.4-cli` with the binary, `public/`,
`fixtures/`, and [`deploy/hamster.container.toml`](deploy/hamster.container.toml),
running as the unprivileged `hamster` user with an HTTP health check.

Copy [`.env.example`](.env.example) to `.env` to change published ports.

## Deployment

[`doc/DEPLOYMENT.md`](doc/DEPLOYMENT.md) covers the container image, a systemd
unit for bare-metal installs, configuration per environment, health checking,
upgrades, and rollback.

## Continuous integration

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push and
pull request:

- **Linux** — `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
  (PHP worker tests included), then the release smoke test.
- **Windows** — build and test, matching the project's primary target.
- **Container** — build the image and probe a running container.

## Layout

```text
src/            server, router, configuration, static files, PHP worker
tests/          integration tests (routing, static files, configuration, PHP worker)
benches/        release-only benchmarks behind the `profiling` feature
fixtures/       PHP worker fixtures used by tests and local runs
public/         default document root
deploy/         configuration shipped with the container image
scripts/        dev.sh, check.sh, smoke.sh, check.ps1
doc/            plan, configuration, routing, PHP worker, benchmarks, deployment
```

Contributor conventions live in [`AGENTS.md`](AGENTS.md).
