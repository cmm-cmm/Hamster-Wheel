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
| PHP CLI | **8.5, non-thread-safe, 64-bit** | the `php_worker` handler and its integration tests |
| Docker | 24+ | the container image and `docker compose` workflow |

The worker runs one PHP process per location and drives it over a pipe, so the
thread-safe build buys nothing and costs throughput — the non-thread-safe
64-bit build is what this project ships and tests against. CI and the container
images assert it; the local scripts only warn, so an older PHP still works for
day-to-day development.

PHP itself is optional: without it the server still serves static files, and
the PHP worker tests report themselves skipped on stderr.

**Windows**: download the *VS17 x64 Non Thread Safe* ZIP for PHP 8.5 from
<https://windows.php.net/download/> and extract it to `php/8.5/` so that
`php/8.5/php.exe` exists. That directory is deliberately ignored by Git — never
vendor binaries. `hamster.toml` and every script look there first.

**Linux/macOS**: install `php8.5-cli` (Debian/Ubuntu via the ondrej PPA,
Homebrew via `php@8.5`), or point `HAMSTER_PHP` at any PHP binary you already
have.

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
HAMSTER_LISTEN=0.0.0.0:9000 HAMSTER_PHP=/opt/php/8.5/bin/php ./scripts/dev.sh
```

Without the scripts:

```bash
cargo run                              # built-in config, static files on 127.0.0.1:8080
cargo run -- --config hamster.toml     # Windows development config
cargo build --release                  # the only build valid for benchmarks
cargo test
```


## Containers

```bash
docker compose up server                     # release image on http://127.0.0.1:8080
docker compose run --rm dev ./scripts/check.sh
docker compose run --rm --service-ports dev ./scripts/dev.sh
```

The `server` image is a multi-stage build: a Rust builder produces the release
binary, and the runtime layer is `php:8.5-cli` with the binary, `public/`,
`fixtures/`, and [`deploy/hamster.container.toml`](deploy/hamster.container.toml),
running as the unprivileged `hamster` user with an HTTP health check. Both the
`dev` and `runtime` stages assert PHP 8.5 NTS 64-bit at build time, so a wrong
base image fails the build instead of shipping.

Copy [`.env.example`](.env.example) to `.env` to change published ports.

## Deployment

[`doc/DEPLOYMENT.md`](doc/DEPLOYMENT.md) covers the container image, a systemd
unit for bare-metal installs, configuration per environment, health checking,
upgrades, and rollback.

## Continuous integration

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push and
pull request:

- **Linux** — PHP 8.5 NTS 64-bit verified, then `cargo fmt --check`,
  `cargo clippy -D warnings`, `cargo test` (PHP worker tests included), then the
  release smoke test.
- **Windows** — the same PHP verification, then build and test, matching the
  project's primary target.
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
