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

The Windows tests cover startup, graceful/abnormal PHP-worker shutdown,
compatible nested jobs, and operator-managed FastCGI recovery. An actual TCP
SYN-blackhole with a running blocking connect is **NOT VERIFIED**: no isolated
Windows VM was available, and the host firewall was not changed. Native Linux
operation is also **NOT VERIFIED**. These limits must remain visible in any
deployment or release decision.
