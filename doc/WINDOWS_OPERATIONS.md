# Windows Operations (Current Scope)

Hamster Wheel is qualified here as a Windows console-hosted Web/Application
Server, not as a Windows Service or release package. Build with
`cargo build --release --target x86_64-pc-windows-msvc` and start the binary
with an explicit deployment config:

```powershell
.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe --config D:\site\hamster.toml
```

The checked-in `hamster.toml` is a development example. Relative roots, PHP
paths, and certificate paths in an explicit config resolve beside that config
file. Protect the config, private keys, application tree and writable data with
normal Windows file permissions; do not place secrets in the repository.

## Backend Ownership and Readiness

Persistent `php_worker` pools are Hamster-owned. All initial workers must reach
READY before listeners bind. On Windows, Hamster establishes its kill-on-close
Job Object before spawning them; startup fails closed if containment cannot be
established. That containment covers ordinary descendants of those workers,
not arbitrary processes started by an external supervisor.

FastCGI `php-cgi.exe` endpoints are **operator-owned**: start, monitor and stop
each process separately. One Windows `php-cgi` process serves one exchange at
a time, so configure one numeric loopback address per process with
`concurrency = 1`. For a local smoke check in a separate terminal:

```powershell
$env:PHP_FCGI_MAX_REQUESTS = '0'
& 'D:\php\php-cgi.exe' -b 127.0.0.1:9001 -d display_errors=0
```

Hamster does not probe FastCGI or reverse-proxy backends at startup. Its
`listening` line means listeners are bound and Hamster-owned workers are ready;
it does **not** certify external application health. Check a safe,
application-specific GET route after startup and during operation. A refused
FastCGI connection returns 502; once that endpoint starts, later requests can
succeed without restarting Hamster. Do not automatically replay a request that
may already have run. FastCGI slot exhaustion returns 503; backend response
timeout returns 504. An external `php-cgi` process is not reaped by Hamster's
Job Object and needs its own operator containment/restart policy.

## Files, Signals, and Shutdown

Hamster does not read `.htaccess`. Set host-wide `deny_paths` and `deny_names`
for every application's private trees and configuration names; test both
static and FastCGI routes. File handlers deny dotfiles (except root
`/.well-known`) and never serve PHP source as static bytes. See
`doc/FASTCGI_V1.md` and `doc/CONFIGURATION.md` for exact rules.

In an attached Windows console, Ctrl+C starts graceful connection and worker
drain; verify process exit before stopping externally managed endpoints.
Ctrl+Break reloads **only** the configured TLS certificate/key files
transactionally. There is no general config reload or console-free reload
trigger; a headless deployment must arrange a controlled restart for config
changes and certificate renewal. The current project does not install or
manage a Windows Service.

## Qualification Boundary

### Headless process rehearsal (2026-10-04)

`tests/windows_headless.rs` launches the actual binary with Windows
`DETACHED_PROCESS`, redirected stdout/stderr, one Hamster-owned PHP worker and
one separately started `php-cgi` endpoint. It checks HTTP readiness and both
handlers, then force-terminates the exact Hamster process. An already-open
process handle confirms the worker exits within five seconds; the external
`php-cgi` remains alive. A fresh Hamster process then serves both handlers
through the same endpoint, and the test reaps every process it created. The
captured readiness and empty stderr files show basic headless logging works.
This is a **process-level rehearsal**, not a Windows Service test or a
supervisor recovery test: the test explicitly performs the restart.

An initial `CREATE_NO_WINDOW` attempt still had an attachable console when
launched from the console-hosted test harness; it was **not** evidence of a
console-free process. The passing rehearsal uses `DETACHED_PROCESS`, and its
helper verifies `AttachConsole` fails with `ERROR_INVALID_HANDLE`.
The same test file also replaces a TLS certificate/key on disk while a
detached Hamster runs: a new verified handshake still presents the previously
loaded certificate, and no reload success is logged. File replacement alone
is not a headless reload mechanism.

Reproduce on a Windows checkout with the local PHP distribution:

```powershell
cargo test --target x86_64-pc-windows-msvc --test windows_headless -- --nocapture
cargo test --target x86_64-pc-windows-msvc --test windows_headless -- --ignored --nocapture
```

WinSW is the candidate wrapper for a later isolated Service rehearsal because
its [configuration reference](https://github.com/winsw/winsw/blob/v3/docs/xml-config-file.md)
documents process restart on non-zero exit, log capture/rotation, service
accounts, and a configurable graceful-stop timeout. Do **not** install it from
this runbook yet. Its `hidewindow=true` uses `CreateNoWindow`, not the tested
`DETACHED_PROCESS`; actual Service console inheritance is **NOT VERIFIED**.
For a truly console-free Hamster, the current stop/reload inputs are still
Ctrl+C/Ctrl+Break only. WinSW's documented
fallback after an undeliverable stop signal is process termination, not
Hamster's connection/worker drain. Certificate rotation in that topology
would require a full process restart, not the transactional reload. Whether
WinSW with its default console behavior can deliver graceful stop under a
real Service account is **NOT VERIFIED**. Operator-managed `php-cgi` requires
its own separate supervisor policy; Hamster's Job Object must not be presented
as protecting that endpoint.

Before approving a headless Service deployment, verify on an isolated Windows
VM: Service start/readiness; stop with an in-flight request and normal worker
ACK/reap; crash recovery with exact PID tracking; external endpoint
containment/restart; log rotation; and certificate replacement/reload. A
console-free control path for graceful stop and TLS reload remains a design
decision, not an implemented feature. Do not work around it by force-killing
Hamster during routine renewal.

The Windows tests cover startup, graceful/abnormal PHP-worker shutdown,
compatible nested jobs, and operator-managed FastCGI recovery. An actual TCP
SYN-blackhole with a running blocking connect is **NOT VERIFIED**: no isolated
Windows VM was available, and the host firewall was not changed. Native Linux
operation is also **NOT VERIFIED**. These limits must remain visible in any
deployment or release decision.
