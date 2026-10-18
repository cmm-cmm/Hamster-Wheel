# Linux Verification (2026-10-18)

What was verified on native Linux, on the `dev-linux` branch at `ece1391`, and
what was not. The Windows record (`doc/AI_HANDOFF.md`, *Verified Platform*)
is unchanged; nothing here is evidence about Windows.

## Environment

| | |
|---|---|
| Host | Cloud container, 4 vCPU Intel Xeon @ 2.10 GHz, 16 GiB, no swap |
| OS | Ubuntu 24.04.5 LTS, Linux 6.18 |
| Toolchain | rustc / cargo 1.97.0, target `x86_64-unknown-linux-gnu` |
| PHP | 8.5.11 NTS 64-bit, CLI and CGI, built from the `php-8.5.11` tag of `php/php-src` into `/opt/php-8.5` (`doc/LOCAL_TEST_APPLICATIONS.md`); tag signature not verified |
| PHP modules | the extensions the tests use are compiled in: curl, fileinfo, intl, mbstring, openssl, pdo_sqlite, sodium, sqlite3, zip, plus OPcache |
| Runtime layout | `php/8.5/php` and `php/8.5/php-cgi` are symlinks to `/opt/php-8.5/bin` (git-ignored) |
| Applications | `www/pms-scratch` from `prepare_scratch_app.sh` (Laravel 12, Octane 2, built on 8.5); `www/php-apps` from `prepare_php_apps.sh --prefer-source --no-dev` with `COMPOSER_ALLOW_SUPERUSER=1` |
| Load client | `oha` 1.16.0 on the same host |

The host's network policy blocks `cdn.jsdelivr.net`, `api.iconify.design`,
`download.dokuwiki.org`, GitHub archive downloads and the PHP and Ubuntu PPA
package mirrors. That decides which application tests can run (below) and is
why PHP was built from source.

## Functional result

```
cargo fmt --all -- --check                                  clean
cargo clippy --all-targets --all-features -- -D warnings    clean
cargo test                                                  262 passed, 0 failed
cargo bench --no-run --features profiling                   all 11 benches link
cargo test --no-fail-fast -- --ignored                      108 passed, 5 failed
```

| Ignored suite | Result | Note |
|---|---|---|
| `php_worker` | 63 / 63 | includes the eleven Laravel/Octane tests against `www/pms-scratch` |
| `timeout_probe` | 10 / 10 | CPU and memory from `/proc` |
| `php_cgi` | 8 / 10 | all fixture tests pass; DokuWiki and FlatPress need blocked downloads |
| `php_applications` | 7 / 10 | Slim, native, PSR-7, HttpFoundation pass; the three Symfony Demo page tests need importmap vendor assets from `cdn.jsdelivr.net` |

The five failures are workspace failures, not server failures: each needs an
application the network policy keeps from being built. They are not claimed
as passing.

A release binary was also driven by hand, outside the test harness:

- **Running and recovering, on PHP 8.5:**
  - six persistent workers; static 200;
  - a 2 s `request_timeout` answered 504 at 2.005 s;
  - a worker crash answered 502, then the slot respawned;
  - 3,000 POSTs at 64-way concurrency all answered 201.
- **Rejecting bad requests, on the earlier PHP 8.3 host:**
  - three path traversal forms got 403;
  - an unknown host got 404;
  - a missing Host, `Content-Length` with `Transfer-Encoding`, and a garbage request line each got 400;
  - an oversized body got 413.
- **Worker kill and observability, on PHP 8.3:**
  - a worker killed with `kill -9` had its request requeued, and the slot respawned after the 100 ms backoff;
  - `/health`, `/ready` and `/metrics` and the JSON Lines access log matched the traffic.
- **Shutdown:** SIGINT drained and exited. After both SIGINT and SIGTERM, no PHP process remained.

## What `dev-linux` changed to get there

Each item is a separate commit with its reason; see the PR for `dev-linux`.

- Windows-only code gated with `#[cfg(windows)]` so clippy and the bench link
  gate pass off Windows.
- Tests and benches find `php/8.5/php` and `php/8.5/php-cgi` off Windows
  (`tests/support`).
- The crash and kill helpers use `kill` and `/proc`. The Linux kill waits
  until the process has stopped, as `taskkill /F` does.
- The proxy backpressure test allows for Linux loopback receive-buffer
  autotuning, which goes up to 32 MiB.
- php-cgi tests pass `output_buffering=0`, because Ubuntu's `php.ini` sets it
  to 4096.
- Bash workspace builders for both application sets:
  `prepare_scratch_app.sh` and `prepare_php_apps.sh`.
- **`TCP_NODELAY` on accepted connections, a product fix.**
  - The bug: static file responses on keep-alive connections took 44 ms each,
    a delayed-ACK period, because Nagle held the body behind the head.
  - Before: 23 RPS at C1 and 831 at C32. After: 3,261 and 11,562.
  - The regression test is
    `answers_sequential_keep_alive_requests_without_delayed_ack_stalls`.
  - The A/B is in `doc/BENCHMARKS.md`, *Downstream TCP_NODELAY*.

## Performance

`doc/BENCHMARKS.md` holds two Linux entries:

- *Downstream TCP_NODELAY*: the before/after A/B for the fix above.
- *Linux release baseline, `ece1391`*: every cell, three runs each.

Medians from the baseline:

| Path | C1 | Higher concurrency |
|---|---|---|
| `not_found` (no I/O) | 18,774 RPS, p50 0.048 ms | 110,920 RPS at C32 |
| Static 22 B file | 3,521 RPS, p50 0.258 ms | 12,181 RPS at C32 |
| Persistent PHP worker, hello/JSON | 6,408-7,847 RPS, p50 0.12-0.13 ms | 19,083 at C4 and 31,082 at C32, both with 4 workers |
| FastCGI, 4 php-cgi children | 2,439 RPS, p50 0.365 ms | 11,985 RPS at C16 |

Memory:

- Hamster held 5.6-9.6 MiB RSS across all cells.
- Each persistent PHP worker held about 27 MiB RSS.
- In the soak runs (300,000 static requests and 600,000 PHP requests, both at
  C32), every response was 200. Hamster grew by 2.0 MiB and 0.5 MiB
  respectively, and PHP RSS did not change.

These numbers come from a 4-vCPU cloud host that also runs the load client.
They describe this host; they are not a capacity claim, and they are not
comparable with the Windows rows in `doc/BENCHMARKS.md`.

## Known gaps on Linux

- **SIGTERM is not handled.**
  - Only SIGINT starts the graceful drain, and SIGHUP reloads certificates.
    SIGTERM, which `systemctl stop` and `docker stop` send, ends the process
    through the default action, without draining.
  - PHP workers then see EOF on stdin and exit. No orphan was observed, but
    requests in flight are cut.
  - This is the roadmap's "Linux systemd signals, process groups and
    shutdown" item (`doc/ROADMAP_0_5_TO_1_0.md`).
- **No equivalent of the Windows Job Object.** If Hamster is killed, its
  workers do not die with it; they exit when they next read EOF on stdin.
  By inference, not tested: a worker in the middle of a long request outlives
  the server until that request ends.
- **Static files are CPU-heavy.**
  - They cost 268 µs of Hamster CPU per request at C1, against 26 µs for
    `not_found`.
  - The non-Windows `resolve_file` makes four `tokio::fs` calls (metadata,
    canonicalize, open, metadata), each a blocking-pool round trip, before the
    body is read. The Windows path resolves in one blocking task.
  - This is an observation for a later change; it was not profiled.
- **Static containment uses the pathname model.** Off Windows, the file is
  opened by its canonical pathname after the containment check. This is not
  qualified against concurrent namespace changes, as the code itself states
  (`src/static_files.rs`).
- **The control endpoint is Windows-only.** `server.control` is a named pipe
  and is refused off Windows by design.
- **The checked-in `hamster.toml` names `php.exe`.** On Linux, run a local
  copy with `executable = "php/8.5/php"`.

## Not verified

- The effect of `TCP_NODELAY` on Windows.
- systemd units, process groups, and running as a non-root service user.
- Other distributions, distribution PHP packages (all PHP here was built from
  source), aarch64, and musl.
- TLS and reverse-proxy performance on Linux. Their functional tests pass;
  neither path was benchmarked.
- Symfony Demo pages, DokuWiki and FlatPress, which need blocked downloads.

## Reproducing

```sh
# PHP 8.5 and symlinks: doc/LOCAL_TEST_APPLICATIONS.md
fixtures/php-worker/pms/prepare_scratch_app.sh
COMPOSER_ALLOW_SUPERUSER=1 fixtures/php-apps/prepare_php_apps.sh --prefer-source --no-dev
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo bench --no-run --features profiling
HAMSTER_LARAVEL_TEST_PATH="$PWD/www/pms-scratch" HAMSTER_PHP_APPS="$PWD/www/php-apps" \
    cargo test --no-fail-fast -- --ignored --test-threads=1
cargo build --release && cargo install oha
benchmarks/linux_http.py                     # all cells, three runs each
benchmarks/linux_http.py --bin a --bin b static   # interleaved A/B of two builds
```
