# Windows Operations (Current Scope)

Current evidence: [Windows Service lifecycle qualification](WINDOWS_SERVICE_LIFECYCLE_GAPS.md).
The pinned WinSW 2.11 NET461 companion extension passed two real LocalService
runs, including independent main exit 0 and wrapper-only idle/busy/stop crashes.
The sample XML now requires that extension. Older evidence below without it
remains SCM-stop scoped; do not infer safety from the unmodified wrapper.

Hamster Wheel is qualified here as a Windows console-hosted/detached server
and, in the specific rehearsal below, through WinSW 2.11.0 under LocalService
with the pinned lifecycle integration. This is not release qualification, boot verification
or qualification of every Service wrapper/account. Build with
`cargo build --release --target x86_64-pc-windows-msvc` and start the binary
with an explicit deployment config:

```powershell
.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe --config D:\site\hamster.toml
```

Windows builds embed `icon/hamster-wheel.ico` into the executable through
`build.rs` and `embed-resource` (build-time only, scoped to `hamster-wheel`).
Install the Windows SDK resource compiler alongside the MSVC build tools;
missing/invalid resources fail the Windows build rather than silently omit
the icon. Other target platforms skip resource compilation.

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

### Health, readiness and metrics

With `[server.admin] listen = "127.0.0.1:9090"` a separate socket serves
`/health`, `/ready` and `/metrics`. It is off by default, never shares the
application listener, and has no TLS or authentication, so it belongs on
loopback or on a management interface only; a non-loopback bind logs a warning
at startup naming what became reachable.

Give a load balancer `/ready`, not `/health`. `/health` answers 200 for as long
as the process can answer, including the whole of a graceful stop, which is
what a supervisor needs so it does not restart a server that is deliberately
stopping. `/ready` answers 503 `draining` from the moment a stop is triggered,
and 503 `php_pool <label> has no live worker` when a pool cannot serve at all.
A pool below its target but still serving stays ready on purpose: pulling a
partly healthy instance during a worker replacement storm makes an incident
worse. Read `hamster_php_pool_workers{pool,state=...}` for that.

FastCGI and proxy backends are **not** in the readiness decision, for the same
reason the startup line does not certify them: Hamster neither owns nor probes
them. Their counters are exposed (`hamster_fastcgi_*`, `hamster_proxy_*`) so an
operator can build that decision outside Hamster.

Scrape `/metrics` for the Prometheus text exposition. Label cardinality is
bounded by the configuration — pool, upstream, endpoint and certificate names,
a fixed status class, fixed duration buckets — so traffic cannot grow the
metric set. Scrapes and health checks are not counted as application traffic.
`--control status` reports the same readiness judgement as `ready=true` or
`ready=false reason=...`, plus `admin=http://...`, for a server reached only
through its pipe. `doc/OBSERVABILITY_0_8.md` has the full metric list.

When a per-request log is configured, `status` also reports it:

```text
access_log=D:\site\logs\access.jsonl queued=0 written=5312 dropped=0 failed=0
```

`dropped` is the only honest answer to "is this log complete?": the queue is
bounded and is never waited on, so a log target slower than the traffic costs
records rather than request latency. A non-zero `dropped` means the log has
gaps, which are also written into the log as `log_dropped` records; a non-zero
`failed` means the target refused writes. Hamster appends and never rotates
this file: give it to the wrapper's rotation or to an out-of-process tool, and
remember that renaming or deleting it under a running server sends records to
the old handle until restart. `doc/ACCESS_LOG_0_8.md` has the record format and
what is deliberately never logged.

Read health from `php_pool=<label> target= live= ready= pids=` by comparing
`live` against `target`. `live` counts workers that exist and reached READY,
while `ready` counts those that are READY **and idle**, so a fully healthy pool
serving at full concurrency reports `ready=0`; do not build a degradation check
on `ready`. `pids=` lists the workers observed while constructing that
snapshot. Counters and individual slots are sampled separately, not atomically;
brief differences during replacement are possible. These PIDs are diagnostics,
**not termination authority**. Before terminating anything, retain its process
handle and verify executable, creation time and deployment/parent ownership.

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
One deadline covers reading, command execution, writing and peer-close, rather
than granting five seconds to each phase. The listener owns, cancels and joins
all exchange/refusal tasks on shutdown; they cannot outlive it. Invalid status
lines and oversized/truncated answers are client errors, never success.
Once a complete `stop` is accepted, losing its client does not undo the stop.

Access is controlled by the pipe's DACL, set when the pipe is created and
protected from inheritance: `D:P(A;;GA;;;SY)(A;;GA;;;OW)` grants LocalSystem
and the pipe's object-owner SID, and grants nobody else anything. On the tested
unelevated account the token user and default owner match. Do not assume this
for an elevated or Service token: verify its actual owner and access in the
deployment. Remote clients are rejected, and the first instance
claims the name so a second process cannot serve the same endpoint beside it.
Treat membership of that account as equal to the ability to stop the server.

Only the control client reads the configuration file to learn the endpoint
name; it never compiles the configuration, so a certificate that no longer
loads from disk still leaves `reload-certificates` usable, which is exactly
when it is needed.

### WinSW deployment (pinned lifecycle integration)

[`examples/winsw/HamsterWheelService.xml`](../examples/winsw/HamsterWheelService.xml)
is a reviewable WinSW configuration, **not** an installed Service. Place a
WinSW v2.11.0 executable named `HamsterWheelService.exe` beside a copy
of that XML with the same base name, `Stop-Hamster.ps1`, the release
`hamster-wheel.exe`, and a deployment `hamster.toml`. Build the required
`HamsterLifecycleExtension.dll` and unchanged `WinSW.NET461.exe` reference copy
into the same deployment directory:

```powershell
.\examples\winsw\Build-LifecycleExtension.ps1 -WinSW D:\tools\WinSW.NET461.exe -OutputDirectory D:\HamsterDeployment
```

The builder and runtime both pin vendor SHA256
`31DCA0D9BD3C5AB7E119172BDB927B90C9DA935D0F4963335EAF60E7D8F7AABF`.
Do not replace or modify the upstream executable. Missing/incompatible assets
are a startup failure, not an uncontained fallback. The extension self-assigns
the wrapper to a kill-on-close job before Hamster is spawned, and uses the
Service's existing status handle for the specific vendor unsolicited-exit-0
access-denied branch. It needs no broader SCM rights. Its private WinSW/CLR
coupling is qualified only within the exact environment documented above.

Keep the main command
exactly `hamster-wheel.exe --config <absolute-config-path>`: the stop helper
deliberately rejects other argv shapes. The example selects the low-privilege built-in
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

**Correction to the earlier runbook:** WinSW v2.11.0's
[implementation](https://github.com/winsw/winsw/blob/v2.11.0/src/WinSW/WrapperService.cs)
waits for both main and custom stop processes **without applying
`<stoptimeout>` in the custom-stop branch**. A bare `--control stop` can
therefore leave Service stop waiting indefinitely. The supplied helper owns
the deadline instead; the XML no longer advertises an ineffective timeout.

[`Stop-Hamster.ps1`](../examples/winsw/Stop-Hamster.ps1) checks its live wrapper
parent, then verifies a direct child's executable, config argv and creation
time, rechecks its deployment record after opening a retained process handle,
and rejects ambiguous matches. It invokes `--control stop`, waits on that
**same handle** for actual exit, and only after `-GraceSeconds` terminates that
verified process using the retained handle. `OK stopping=true` is receipt,
not completion. Failed/busy control does not trigger an immediate kill.
Identity discovery failure exits 1 **without killing**: WinSW may still wait
for the main process; diagnose ownership/permissions, never guess a PID.
Exit 0 means actual exit before the deadline (inspect Hamster's logged exit
code); exit 2 records forced fallback. `logs/last-stop.log` records the latest
stop's identity, elapsed time and graceful/forced result.

`-GraceSeconds 120` is an example, not a universal budget. Include helper
startup/discovery, the five-second control budget, connection drain and all
accepted PHP jobs, not just one request. Pools shut down sequentially; a
conservative worker budget is the sum over pools of
`ceil((queue_capacity + workers) / workers) * request_timeout`, plus worker
SHUTDOWN/ACK/kill/reap and scheduling margin. Also allow the HTTP/TLS/proxy/
FastCGI drain deadlines configured for this deployment. Rehearse the actual
bounded workload and adjust the sample. Forced fallback waits up to another
five seconds for the retained process to signal; native CIM/OS calls are not
claimed hard real-time. Immutable scripts/config/executable are required:
application users must not be able to replace stop policy.

The sample's restart policy applies to crashes, not operator-managed
`php-cgi` endpoints, which need separate supervision. Hamster's Job Object
reclaims its inherited PHP worker tree after forced fallback; it does not own
an externally launched endpoint. Its stdout/stderr files use WinSW's
[size-based rolling mode](https://github.com/winsw/winsw/blob/v2.11.0/doc/loggingAndErrorReporting.md)
at 10 MiB with eight retained files; still monitor wrapper/Event Log errors.

### Automated dedicated Service qualification (Administrator required)

Owner authorized temporary Services on this host for this rehearsal. The
first preparation token was not elevated. After Owner restarted with
Administrator privileges, the full Service procedure completed successfully
on 2026-10-11; see evidence below. A VM is not a prerequisite for this
specifically authorized test. No reboot, firewall change or modification of
existing Services is authorized by the script.

From **PowerShell, Run as administrator**, execute:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "D:\Project\Hamster-Wheel\examples\winsw\Invoke-LifecycleQualification.ps1"
```

[`Invoke-ServiceQualification.ps1`](../examples/winsw/Invoke-ServiceQualification.ps1)
uses the already-built MSVC release binary, local `php/8.5`, Git's OpenSSL
(override with `-OpenSsl <absolute-path>`) and pinned official WinSW 2.11.0
.NET461. The script records its source/hash; the checksum is not a separate
publisher signature verification. It never prompts for elevation. Missing
Administrator returns exit 3 before creating fixtures or touching SCM.

The matrix runs an unmodified baseline then a fixed deployment sequentially.
For only the fixed full rehearsal, use `Invoke-ServiceQualification.ps1 -Mode
Service -LifecycleExtension`. Plain `-Mode Service` retains the older SCM-stop
scope; `-LifecycleBaseline` deliberately reproduces vendor failures, not a
safe deployment. `-RunnerSelfTest` tests error-exit/finally handling without SCM.

Each run creates `target/windows-service-qualification/<GUID>`, unused
loopback ports and two unique `HamsterQual_<id>` Services. The main runs as
LocalService with Automatic startup and three-second SCM crash recovery. The
Manual `_Control` Service runs a finite ping/status/reload exchange under the
same actual account, recording user/owner SIDs; the harness SCM-stops it after
the exchange completes. It is not a new production management service. A
different runner account must get access denied (a
LocalSystem runner cannot qualify denial). Scratch deployment ACL inheritance
is removed: deployer/Administrators/System retain control, LocalService gets
read/execute and modify only on state/logs. No original PHP/config/key ACL is
modified. Private fixture CA/leaf keys never enter the Windows certificate
store; HTTPS probes pin the generated leaf and check hostname, after offline
CA verification. All scripts, keys and PHP sources are outside static roots.

Gates cover application readiness (not SCM RUNNING alone), HTTP/HTTPS/native
PHP and an independently owned php-cgi endpoint, control ACL, valid TLS reload
and invalid-key rollback, disconnected-client transaction drain/ACK, retained
main/child handles, STOPPED-after-exit, replacement-generation/descendant
containment, Hamster crash/SCM recovery, stderr rotation (scratch 64 KiB/eight
retained), and a deliberately failed control client. The latter must wait the
12-second test grace before terminating the verified main handle; another
Hamster and the externally owned endpoint must remain alive. The short grace
is deliberate fault-test policy, not the production sample's 120 seconds.

Actual SCM Auto is reported **CONFIGURED, NOT VERIFIED** until a separately
authorized reboot. Restarting the Service does not verify boot startup. Main
process crash is distinct from abrupt wrapper crash; both were subsequently
qualified with the pinned extension. A wrapper crash during STOP_PENDING also
triggered automatic recovery: verify STOPPED and reissue stop if interrupted.
WinSW's stdout/stderr roll policy does
not rotate its separate `.wrapper.log`; arrange independent retention for
that file/Event Log in deployment policy. Endpoint lifecycle remains external.

**Unmodified vendor limitation:** WinSW 2.11's unsolicited child exit-0
callback invokes `SignalStopped`, which opens SCM with access LocalService
lacks. The first finite-probe run logged Access denied and left the wrapper
RUNNING even though its child had exited. With the pinned extension, actual
independent Hamster exit 0 now reports STOPPED and exits the wrapper 0 without
crash recovery. The normal deployment stop remains **SCM stop / WinSW
`stopwait` → Stop-Hamster → control stop → wait actual main exit**.
`OK stopping=true` alone still proves nothing about completion. Without the
extension, independently stopping main remains unsafe. Do not grant LocalService broad SCM rights or
silently substitute LocalSystem to hide this. The test probe is now held until
SCM stop after its control exchange, with bounded expiry; its completed control
transaction is not cancelled. Vendor code evidence:
[WrapperService.cs, SignalStopped/OnProcessCompleted](https://github.com/winsw/winsw/blob/v2.11.0/src/WinSW/WrapperService.cs).
The issue is in the wrapper, not Hamster's worker or pipe ACL. The exact
before/after reports and wrapper-only crash matrix are in the current lifecycle
qualification report. Other wrapper versions remain separate compatibility work.

Exit 0 means all executed gates passed; exit 1 means failure, with evidence.
`finally` disables recovery only for verified owned Services, waits owned
process handles, uninstalls those Services and removes their Event Log
sources. Successful runs delete scratch deployment/keys; failed runs retain
diagnostic fixtures but still attempt process/SCM cleanup. Inspect the report's
Cleanup result before assuming removal succeeded; never compensate with
process-name kills or snapshot PIDs. The ignored JSON result is retained next
to, not inside, the deleted fixture. `-KeepFixture` retains scratch files,
**not** Services. The cached WinSW tool is retained for reproducibility.

Unprivileged `-Mode Prepare` checks staging/PKI/XML/PHP lint;
`-Mode Process` rehearses application/lifecycle/stop behavior without WinSW or
SCM. These are **not** Service qualification. Process mode passed on
2026-10-11, including real PHP/HTTPS, reload/rollback, graceful ACK, replacement
and ordinary descendants reclaimed in 23 ms, 12-second forced fallback and
instance isolation. Evidence:
`target/windows-service-qualification/98443e3b6c2243238fddd333f6a8f585-results.json`
(local ignored artifact; historical process-only evidence).

### Actual LocalService/SCM evidence (2026-10-11)

Full Service mode returned exit 0. Ignored evidence JSON:
`target/windows-service-qualification/58e4cf9b7f324b79b6d0b35558ff7c7a-results.json`.
Services `HamsterQual_58e4cf9b7f32` and `_Control` used LocalService; recorded
user/owner SID was `S-1-5-19`. Main had SCM Auto and three-second restart
policy; real PHP served HTTP/HTTPS. Both same-account control authorization
and different elevated operator denial passed without changing pipe policy.
Valid TLS adoption and mismatched-key rollback passed pinned handshakes.

SCM stop while a disconnected client's transaction was busy kept main PID
6928 alive after receipt, then completed the transaction, SHUTDOWN_ACK,
worker/descendant reap, main exit 0 and SCM STOPPED. Replacing a killed PHP
worker, then terminating retained main PID 2400, reclaimed recorded initial/
replacement/descendant handles in 29 ms. SCM automatically created wrapper
PID 8548 / main PID 19872 and restored PHP service. These PIDs are historical
diagnostics only, not termination authority. Exact images/creation times and
retained handles were used by the test.

Real WinSW stderr rotation retained eight ~64-KiB archives plus active file.
Deliberately failing control caused forced stop only after the 12-second grace;
actual stop/STOPPED convergence took 14,255 ms, main exit code 1. Separate
Hamster and externally owned php-cgi remained live and responsive. Both test
Services and Event Log sources were removed; successful fixture/keys were
deleted and final process/SCM inventory was empty. No reboot or firewall change
occurred. A second PHP request through the independent instance explicitly
confirmed endpoint responsiveness after forced stop. First corrected pass
(`9f83d684e6b44626a49598d9a22244cf-results.json`) also passed, with 22-ms reclaim
and 14,238-ms forced convergence. Auto was **CONFIGURED, NOT VERIFIED for boot** during the test;
the temporary installed SCM entry no longer exists after cleanup.

Two initial Service attempts failed before the full corrected pass: missing
probe XML description, then WinSW's unsolicited exit-0 behavior described
above. Their owned Services/processes were cleaned up. Four failed diagnostic
directories remain because tool policy rejected explicit recursive deletion:
`f281d460959642b693d96a5cd1067008`, `3babe74087f843ee90803e26c4520043`,
`086855c6ff724553b08a9c1c63ad5f09`, `31df729cbf24402b8763205d42dcba8b`
under `target/windows-service-qualification/`. They contain only scratch PHP,
certificates and diagnostics, not original application data. Owner can remove
these exact directories after reviewing failure logs. JSON evidence and tool
cache are intentionally retained. Linux, SYN-blackhole, abrupt wrapper death,
other service accounts and boot/reboot remain NOT VERIFIED.

### Manual staging procedure (historical VM alternative, NOT VERIFIED here)

Stage a dedicated `C:\HamsterRehearsal` deployment; never use production data.
Pin/check the WinSW 2.11.0 binary checksum independently. Stage the main/XML/
helper/config assets and the built lifecycle DLL plus unchanged vendor reference
copy described above, trusted PHP/fixtures or a scratch application, and scratch TLS keys.
Create `logs`; restrict deployment/script/config modification to the deployer
and System. Give LocalService read/execute on binaries/application, read on
config/keys, and modify only on logs and explicitly writable scratch app data.
Keep keys and PHP sources outside static roots. Do not grant Everyone access
to the pipe or TLS keys. Set a unique control name and unused loopback ports.

Only in an **Owner-authorized dedicated elevated test environment**, from that
staging directory:

```powershell
# Do not install the generic name beside a real deployment; choose a unique ID.
.\HamsterWheelService.exe install
.\HamsterWheelService.exe start
.\HamsterWheelService.exe status
# A safe GET and a verified TLS handshake establish application readiness;
# SCM RUNNING alone does not. Capture wrapper/stdout/stderr/Event Log.
Invoke-WebRequest http://127.0.0.1:18080/ -UseBasicParsing
# Start a bounded scratch application request before requesting this stop.
.\HamsterWheelService.exe stopwait
.\HamsterWheelService.exe status
Get-Content .\logs\last-stop.log
# After all rehearsals, remove only this explicitly named test Service.
.\HamsterWheelService.exe uninstall
```

Require STOPPED only after actual process exit, normal worker ACK/reap on the
graceful case and `forced=false`. Deliberately unavailable control must reach
the configured deadline before `forced=true`; it must not kill another
deployment. For crash recovery, acquire/verify the main's retained handle and
deployment/parent/creation identity first (same rules as the helper), terminate
through that handle, and require a new main instance, initial READY and safe
GET after the restart delay. Record old/new creation times, not PID alone.
Test a workload exceeding 10 MiB of startup/diagnostic logs and check the eight
retained rotations, disk-full/log-reader loss, and wrapper/Event Log errors.
Do not add request logging solely to fill files.

Under the actual Service token, verify `ping`, `status`, `reload-certificates`
and `stop`; replacement leaf adoption and invalid-key rollback must be proven
by verified TLS handshakes. Test a different ordinary operator account gets
access denied. Operators may use SCM stop if granted that specific right; live
reload currently requires an authorized owner/System control procedure. A
broader operator pipe ACL is an Owner policy decision, not silently enabled.
Test compatible/restrictive outer jobs and independently supervised
`php-cgi` crash/restart separately. Record exact handles/identities and clean
only owned scratch processes, Service and files. **Real LocalService account,
cross-account control, SCM recovery and WinSW log rotation have now passed in
the automated scoped rehearsal above.** This manual alternative was not run;
do not infer different wrapper/account compatibility. The automated procedure above is the
preferred repeatable path; the VM-only restriction in older dated entries below
predates Owner's explicit temporary-Service authorization.

## Qualification Boundary

### Final Windows operations pass (2026-10-11)

Scope: local console/detached operation and the supplied supervisor stop
helper, **not SCM/Service qualification**. The starting checkout was clean
at `18a2f13` on `dev`; no existing deployment or Service was modified.

Reproduced before correction: aborting a control listener left a detached
exchange holding its pipe; an invalid framed status was reported as client
success; cancelling TLS reload released serialization while certificate work
was still queued; the old completion log appeared while a disconnected
client's PHP transaction was still running. New regressions fail with the old
behavior and pass after correction. No PHP protocol, dispatcher, FastCGI
connect policy, at-most-once or application lifecycle changes were required.

Verified groups:

| Group | Result |
|---|---|
| Full MSVC default suite | 244 passed; prerequisite/manual tests remain ignored |
| Real-PHP worker regressions | 59 passed, including cancellation, timeout classification, replacement startup budget, recycle/backoff/cooldown, Ctrl+C/Ctrl+Break and broken stdout/stderr |
| Multi-pool / FastCGI / live-readiness | 9 / 8 / 1 passed; real `php-cgi`, rollback, all-pool reap and external endpoint ownership |
| Release process rehearsals | Control 15, containment 4, TLS console reload 2, detached headless 2: all passed |
| Build/tool gates | rustfmt check, strict all-target/all-feature MSVC Clippy, release build, profiling bench linking, diff check; 45 PHP fixtures linted; XML/PowerShell parsed |

Release control overload held 112 baseline handles, peaked at 127 and returned
to 112 after deadlines; status and stop recovered. Release containment
reclaimed busy/bootstrap/replacement/multi-pool/ordinary descendants within
0–5 ms in this run (the assertions allow five seconds, not a latency SLA).
The fallback helper waited 5.009 s before terminating its verified main
handle, confirmed exit at 5.025 s and left another deployment usable. The
graceful barrier test confirmed stop receipt precedes transaction completion,
worker SHUTDOWN_ACK and real process exit. An incorrect wrapper identity was
refused without stopping the server. A duplicate control name now also proves
rollback of an already-READY PHP worker and removal of the failed listener.

Reproduce release process coverage (helpers are not standalone tests):

```powershell
cargo test --release --target x86_64-pc-windows-msvc --test windows_control --test windows_containment --test windows_headless --test tls_reload_process -- --include-ignored --skip console_probe_entry --skip ctrl_break_helper_entry --nocapture --test-threads=1
```

External Laravel/PMS/application workspaces were not reused or modified.
Unchanged historical measurement probes, 10k forced-failure soak and 60-second
random-kill stress were not repeated; existing evidence remains applicable,
not a new result. TCP SYN-blackhole, native Linux, real Service/cross-account
access, SCM recovery and WinSW rotation are **NOT VERIFIED**. Those are
environment qualifications, not reasons to repeat completed local tests.

`HTTP listeners drained` means only connection drain. `Hamster Wheel stopped`
is emitted only after successful worker shutdown/reap; neither log line nor
`status` replaces a retained process handle when proving exit. Logging tolerates
a missing/broken sink; this does not claim asynchronous nonblocking writes to
a sink that remains open but never reads. Cancelled native certificate I/O is
bounded in outstanding work, not forcibly interruptible; use reliable local
certificate storage and the supervisor's last-resort timeout.

### Control endpoint rehearsal (2026-10-11)

`tests/windows_control.rs` starts real `DETACHED_PROCESS` servers and drives
the shipped client against them. Verified: `status` reports the live PID,
`state=serving`, the listener, worker readiness and the exact worker PID;
`stop` ends the server with exit code 0 and its
PHP worker exits through the normal lifecycle rather than forced containment; a reload adopts
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

The WinSW stop helper has a process-level rehearsal with the real binary and
PHP; WinSW/SCM integration itself is not a Service rehearsal. Actual Service
console behavior, recovery policy and log rotation are **NOT VERIFIED**.
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
