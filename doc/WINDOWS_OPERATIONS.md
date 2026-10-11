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
transactionally. There is still no general configuration reload: a changed
`hamster.toml` needs a controlled restart. The current project does not install
or manage a Windows Service.

### The control endpoint

A server with no console has no Ctrl+C and no Ctrl+Break, which is the state a
Service, a supervisor or `DETACHED_PROCESS` leaves it in. Name a control
endpoint and the same graceful stop and the same transactional certificate
reload become available without one:

```toml
[server.control]
name = "edge"
```

The endpoint is a Windows named pipe at
`\\.\pipe\hamster-wheel-control-<name>`. It exists only when configured, and
the startup line reports its full path. Operators reach it through the same
binary, which needs the same configuration file because that file names the
endpoint:

```powershell
hamster-wheel.exe --config C:\hamster\hamster.toml --control status
hamster-wheel.exe --config C:\hamster\hamster.toml --control reload-certificates
hamster-wheel.exe --config C:\hamster\hamster.toml --control stop
```

| Command | Effect |
|---|---|
| `ping` | Liveness and the server's PID, with no other work |
| `status` | PID, uptime, lifecycle state, listeners, certificate expiry, pool readiness with exact worker PIDs, upstreams, FastCGI endpoints and the connection ceiling |
| `stop` | Starts the same graceful drain as Ctrl+C; `OK stopping=true` acknowledges the command, **not** process exit. Wait for Hamster's process handle to signal before reporting shutdown complete. |
| `reload-certificates` | The same transactional reload as Ctrl+Break |

The exit code is 0 for `OK` and 1 for `ERR`, and the server's own answer is
printed verbatim. A refused reload answers `ERR reload_failed` with
`active_set=previous`: the previously loaded set is still serving, so a renewal
that produced a mismatched pair does not interrupt TLS.

An answer from the endpoint is already evidence about startup. Hamster starts
its pools and waits for every initial worker to reach READY, then binds every
listener, and only then creates the control pipe (`src/server.rs`, in that
order), so a `status` that answers at all means the initial workers reached
READY and the sockets are bound. It does not mean the pools are healthy *now*;
that is what the live fields are for.

Read health from `php_pool=<label> target= live= ready= pids=` by comparing
`live` against `target`. `live` counts workers that exist and reached READY,
while `ready` counts those that are READY **and idle**, so a fully healthy pool
serving at full concurrency reports `ready=0`; do not build a degradation check
on `ready`. `pids=` lists the exact worker PIDs taken from the same snapshot as
the counts, so a supervisor tracking or reaping the tree Hamster owns never has
to match process names.

`state=` is `serving` or `draining`. The endpoint keeps answering while
connections drain, so without it a health poll during a deliberate stop would
be told the server is still serving, and a supervisor that restarts on an
unhealthy answer could not tell "stopping on purpose" from "broken". It turns
`draining` the moment a stop is triggered, before the drain begins.

Every answer ends with a blank line, which is its frame: a client reads up to
that line and closes first. This is not cosmetic. If an answer were not framed,
a client could only know it was complete when the server closed, while the
server waits for the client to close, so both ends would be released only by
their deadlines; and bytes a client has not read when the server closes first
can be discarded by Windows. The frame is what makes an answer prompt and
reliable rather than timing-dependent.

The endpoint handles four exchanges at once and at most eight simultaneous
`ERR busy` refusals. Refusals run off the accept loop, but each can wait up to
five seconds for its peer to close. Once those eight slots fill, further
connections are closed immediately and may see a pipe error or incomplete
answer; they must never be reported as successful commands. The shipped client
retries `ERROR_PIPE_BUSY` and framed `ERR busy` within one five-second deadline,
but sustained overload can still make an operator command fail. Retry it after
the overload subsides; do not interpret `ERR busy` as a stop acknowledgement.
The high pipe-instance ceiling is not a task/handle budget. Silent or partial
requests are released at the five-second exchange deadline without execution.

Access is controlled by the pipe's DACL, set when the pipe is created and
protected from inheritance: `D:P(A;;GA;;;SY)(A;;GA;;;OW)` grants LocalSystem
and the pipe's owner, which is the account the server runs as, and grants
nobody else anything. Remote clients are rejected, and the first instance
claims the name so a second process cannot serve the same endpoint beside it.
Treat membership of that account as equal to the ability to stop the server.

Only the control client reads the configuration file to learn the endpoint
name; it never compiles the configuration, so a certificate that no longer
loads from disk still leaves `reload-certificates` usable, which is exactly
when it is needed.

### WinSW candidate (not Service-qualified)

[`examples/winsw/HamsterWheelService.xml`](../examples/winsw/HamsterWheelService.xml)
is a reviewable WinSW configuration, **not** an installed Service. Place a
WinSW v2.11.0 executable named `HamsterWheelService.exe` beside a copy
of that XML with the same base name, the release `hamster-wheel.exe`, and a
deployment `hamster.toml`. The example selects the low-privilege built-in
`LocalService` account; create a writable `logs` directory and grant that
account only the access it needs to the executable, configuration, application
files, TLS keys and logs. Do not put an account password in the XML. Set a
unique control endpoint name in `hamster.toml`; restrict config access because
its reader can discover the endpoint and the pipe owner can issue `stop`.
The current pipe DACL allows only its owner account and LocalSystem: an
ordinary administrator/operator in a different account cannot directly run
`--control reload-certificates`. A Service deployment needs a reviewed
same-account control procedure or a separately approved ACL policy; do not
silently broaden pipe access.

WinSW v2.11.0's [XML contract](https://github.com/winsw/winsw/blob/v2.11.0/doc/xmlConfigFile.md)
says `<stoparguments>` launches the stop command and waits for **both** it
and the main process to exit before reporting the Service stopped. The sample
uses `--control stop`, not a console event. `OK stopping=true` means Hamster
accepted the command; it does not mean its connections drained or its process
exited. The 120-second `<stoptimeout>` is a sample upper bound, not a verified
universal value: choose it above the deployment's longest legitimate request
and worker drain, **plus** the control client's own five-second retry budget,
which it may legitimately spend waiting for a pipe instance or retrying an
`ERR busy` answer; then verify with an in-flight request. Only after that
deadline may WinSW's forced termination act as a last resort. A failed stop
client or an overloaded pipe is **not** proof that Hamster stopped; inspect
the wrapper log and the exact Hamster PID. The sample's `onfailure` restart
policy applies to crashes, not operator-managed `php-cgi` endpoints, which
need separate supervision. Its stdout/stderr files use WinSW's
[size-based rolling mode](https://github.com/winsw/winsw/blob/v2.11.0/doc/loggingAndErrorReporting.md)
at 10 MiB with eight retained files; still monitor wrapper/Event Log errors.

For a manual VM rehearsal, capture a process handle for the PID reported by
`--control status` *before* issuing stop, then wait on that same process
object. Do not infer exit from the control client's exit code alone:

```powershell
$hamsterPid = 12345 # replace with pid= from --control status
$main = Get-Process -Id $hamsterPid
& .\hamster-wheel.exe --config .\hamster.toml --control stop
if (-not $main.WaitForExit(120000)) { throw 'Hamster did not exit gracefully' }
```

Before use, rehearse in an isolated Windows VM with a pinned WinSW version:
start and confirm HTTP readiness and log capture; stop during a bounded
in-flight request and require normal Hamster exit, worker ACK/reap and Service
STOPPED only afterwards; terminate the exact Hamster PID and check restart;
verify ordinary PHP descendants are reclaimed while independently managed
`php-cgi` follows its own policy; rotate logs and reload a replacement TLS
certificate through `--control reload-certificates`; verify Service-account
and operator access separately. No Service was installed on this host.

## Qualification Boundary

### Control endpoint rehearsal (2026-10-11)

`tests/windows_control.rs` starts real `DETACHED_PROCESS` servers and drives
the shipped client against them. Verified: `status` reports the live PID,
`state=serving`, the listener, worker readiness and the exact worker PID;
`stop` ends the server with exit code 0 and its
PHP worker exits with it, which `TerminateProcess` does not do; a reload adopts
a replacement leaf that a verified TLS handshake then presents, and a
mismatched key is refused with `active_set=previous` while the adopted leaf
keeps serving; a thread impersonating a token restricted to `Everyone` is
refused with `ERROR_ACCESS_DENIED` while the owner still has access; a client
that connects and sends nothing is released by the exchange deadline, is never
answered, and does not delay a later `stop`; a request line that never ends is
released the same way, is not executed even when its text is `stop`, and the
endpoint still serves afterwards; four held connections make a fifth read
`ERR busy`, and once they are released the shipped client recovers and the
endpoint answers `ping` and `stop` normally; every answer arrives framed with
its blank line in well under a second, while the connection is still open; an
unknown request is answered `ERR unknown_command`; and a second server
configured with an endpoint name already in use fails at startup with exit
code 1 instead of silently sharing it. One unignored test checks the opposite
case: a server with no `[server.control]` section keeps serving.

Two defects were found by exactly these rehearsals and fixed here. The
endpoint's instance ceiling had been set to the concurrency ceiling plus one,
so at saturation the next instance could not be created and the accept loop
returned, which killed the control endpoint for the rest of the process life;
instance creation is now retried and reported once instead of being fatal.
And `OK` answers were not framed with a blank line, which only worked because
the client read to end of stream: every command therefore cost a full
five-second deadline once the server started waiting for the client to close,
and a refusal could be discarded unread. Both have regression tests.

Two further rehearsals cover what a supervisor actually polls. With a
three-second PHP request in flight, a `stop` is acknowledged, `status` then
reports `state=draining`, the in-flight request still completes with HTTP 200
and the server exits 0: a health poll cannot mistake a deliberate stop for a
healthy server, nor for a crash. And with the log reader gone — a detached
server whose piped stdout is closed under it, which is what a wrapper rotating
logs does — `status`, two consecutive `reload-certificates` and a graceful
`stop` all still succeed, the server still exits 0, and nothing reaches
stderr. That test bites: replacing the control stop's `outln!` with the
standard `println!` makes the server stop exiting 0, which is the reason the
`outln!`/`errln!` rule exists.

A later continuous-overload probe found another bound missing: 96 peers that
kept refusal pipes open raised the exact server PID's handle count from 112
to 211. Limiting concurrent refusal waits to eight reduced the same probe to
112 baseline / 126 peak, with 112 after the five-second deadline; a second
96-client burst remained bounded and `status`/`stop` then succeeded. The
client now rejects an unframed/empty answer instead of treating a closed pipe
as a successful command. This is a process-level resource and recovery test,
not a Service-account or throughput qualification.

```powershell
cargo test --target x86_64-pc-windows-msvc --test windows_control
cargo test --target x86_64-pc-windows-msvc --test windows_control -- --ignored --test-threads=1
```

Not verified: behavior under a Windows Service account, cross-account denial
with a second real user, and anything about Linux. No isolated Windows VM and
no second account were available here, and no service or firewall on this
machine was changed, so a supervisor rehearsal with `--control stop` under a
real service account remains **NOT VERIFIED**. The non-Windows build of the
control module is covered only by a substitute: a scratch copy of `src/` with
the platform `cfg` attributes flipped to the non-Windows configuration passes
`cargo clippy --lib --bins --all-features -- -D warnings`, which is what
caught ten dead-code errors a Linux gate would have failed on. That is a
compile-level check, not Linux operation.

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

The WinSW sample and stop contract above are documentation-backed, not a
Service rehearsal. Actual Service console behavior is **NOT VERIFIED**.
Operator-managed `php-cgi` requires separate supervision; Hamster's Job Object
does not protect that endpoint.

Before approving a headless Service deployment, verify on an isolated Windows
VM: Service start/readiness; stop with an in-flight request and normal worker
ACK/reap; crash recovery with exact PID tracking; external endpoint
containment/restart; log rotation; and certificate replacement/reload. The
console-free control path is implemented; its integration with WinSW under a
real Service account remains **NOT VERIFIED**. Do not work around a failed
routine renewal by force-killing Hamster.

The Windows tests cover startup, graceful/abnormal PHP-worker shutdown,
compatible nested jobs, and operator-managed FastCGI recovery. An actual TCP
SYN-blackhole with a running blocking connect is **NOT VERIFIED**: no isolated
Windows VM was available, and the host firewall was not changed. Native Linux
operation is also **NOT VERIFIED**. These limits must remain visible in any
deployment or release decision.
