# Windows Service Lifecycle — WinSW 2.11 / LocalService

## Actual before/after qualification (2026-10-11)

**PASS within the pinned integration scope.** The elevated token was
`OBJECTLAPTOP\cmmph`, Administrator=True. Baseline failures were reproduced
with actual Hamster; the companion extension then passed two full real-Service
runs on Windows 11 Home Single Language 10.0.26300 (x64), LocalService,
unchanged WinSW 2.11.0 NET461. The deployment XML now enables the extension;
compile and stage its required companion assets before installation.
This section supersedes the historical preparation status below.

Evidence under `target/windows-service-qualification/`:

- Complete before/after matrix: `1a4843a8118e44749ff26f5f8de2d55b-matrix.json`.
- Unmodified baseline: `531b5518ba534d1d80f9b5d4105836fb-results.json`.
- Fixed full run: `6e6e8a5984e04317bf5ad6d4c7334066-results.json`.
- Second full fixed run after XML integration:
  `ce6598d8467e481c8481e1ef45cb6c53-results.json`.

### Root causes confirmed with actual Service processes

Baseline main PID 4336 drained its disconnected transaction and exited 0,
but wrapper 22788 remained RUNNING with `SignalStopped` Access denied.
The vendor requests SCM manager All access, unnecessary for reporting its own
Service status. After that callback disposes its main `Process`, a subsequent
SCM custom stop can also throw `No process is associated with this object`.
Do not treat issuing SCM stop afterward as a reliable repair for that state.
The fixed synchronous callback uses the already-owned status handle only
after the vendor observed actual child exit 0. No SCM ACL/account escalation.

Baseline wrapper-only termination (22956) left main 19368 and PHP
10888/descendant 17220 alive and serving. The baseline's owned recovery was
disabled to avoid a deliberately competing instance; these are
**FAIL (REPRODUCED)** outcomes, not safe deployment PASS.

Fixed direct control stop completed the transaction; main 14444 and wrapper
23884 exited 0, SCM reached STOPPED after actual main exit, and no restart
occurred during a six-second observation (configured crash recovery: three
seconds). Normal SCM stop independently retained SHUTDOWN_ACK/main exit 0.

### Wrapper-only crash matrix

Only the retained, identity-verified wrapper handle was terminated. All old
main/PHP/ordinary descendants and the in-progress stop helper signaled exit.

| Case | Wrapper / main PID | PHP / descendant PID | Reclaim ms | Recovered readiness ms |
| --- | --- | --- | ---: | ---: |
| Idle | 6252 / 11832 | 16708 / 24224 | 6 | 4716 |
| Busy transaction | 18484 / 24000 | 12104 / 16272 | 15 | 4655 |
| STOP_PENDING | 5612 / 12948 | 2676 / 13900 | 40 | 4545 |
| Repeat 1 | 944 / 9868 | 9296 / 21128 | 14 | 5648 |
| Repeat 2 | 24356 / 14304 | 19680 / 3664 | 12 | 4354 |

STOP_PENDING also reclaimed stop-helper PID 21852. The second fixed run
repeated all five cases: reclaim 9–30 ms, readiness 4387–4499 ms. Each recovery
served real PHP/HTTP/HTTPS/FastCGI, had one matching main, and left the
independent instance and operator-owned php-cgi responsive. These are lifecycle
observations, not hard real-time guarantees or throughput benchmarks.

**Operational distinction:** a wrapper crash during stop triggered automatic
SCM recovery in both runs. It is not an orderly stop. If stop is interrupted,
verify SCM STOPPED and reissue SCM stop for a recovered instance as necessary.
Abnormal containment kills a busy transaction; only orderly shutdown promises
transaction drain. No request replay was introduced.

### Other qualification and harness corrections

Both fixed runs passed actual LocalService control (user/owner S-1-5-19),
different-account denial, TLS adoption/rollback, initial/replacement tree
containment, main-crash recovery, independent endpoint isolation, real WinSW
stderr rotation (eight archives plus active, scratch 64 KiB), and forced
fallback. Twelve-second test grace converged in 13,494 / 13,308 ms; the
deployment example still uses 120 seconds, subject to workload budgeting.
WinSW's separate wrapper.log is not covered by stderr size rotation.

Early rehearsal failures exposed harness defects, not application regressions:
fixture barrier IDs now obey their existing alphanumeric contract; the matrix
runner captures native stdout/stderr via ProcessStartInfo and waits for child
cleanup even on an error exit. `-RunnerSelfTest` verifies stderr, exit 1 and
completion of finally. The unmodified baseline is staged without the extension
even though the production example now enables it. An exact-handle repair of
the first failed rehearsal is recorded in
`9a846c39af004436b42e39279f58bae2-repair-cleanup.json`; the later failure
`c4b39c487968474491b7130b976e806f-results.json` captured the vendor's disposed
Process/custom-stop issue. These failed attempts are not qualification PASS.

No Rust production code or upstream WinSW binary changed in this follow-up.
Fresh fmt, strict MSVC all-target/all-feature Clippy, default tests (244),
release build and all profiling bench linking passed. Prior unchanged PHP
cancellation/no-retry/timeout/multi-pool regression evidence remains applicable;
the new Service runs independently exercised real drain/ACK and recovery.
Fresh follow-up gates also passed: ignored control 14/14, containment 4/4,
C# compilation/layout/filter/non-Service fail-closed smoke, matrix runner
error-exit/finally self-test, all six PowerShell parsers, XML parser, 42 PHP
fixture lints and `git diff --check`.

All successful runs removed their unique Services/Event Log sources and scratch
deployment/keys, waited owned process handles, and retained ignored JSON/log
evidence. Four previously tool-denied diagnostic directories remain untouched.
The final scoped inventory found zero qualification Services and zero processes
under this turn's fixture identities/paths. A removal command for the two new
failed diagnostic fixtures (`9a846c39...`, `c4b39c48...`) was rejected by the
tool before execution; both directories remain intact, no archive was created,
and no deletion retry/workaround was attempted. They have no live owned
processes or Services. This is a filesystem-cleanup limitation, not orphan PASS
by inference from a process name. The four older denied directories also remain.
The account rights, firewall, unrelated Services/data and cached vendor binary
were not changed. No reboot, commit, tag, push, merge or release.

### Limits of the result

The workaround intentionally depends on private pinned WinSW/CLR layout and a
narrow first-chance exception filter; it is not a generic Service integration.
Hash/layout/setup mismatch fails before application spawn. Other vendor builds,
accounts, OS versions, restrictive outer jobs and broker-created descendants
are not qualified by these runs. Boot startup is CONFIGURED, NOT VERIFIED
(temporary SCM entries were removed); Linux and SYN-blackhole remain NOT VERIFIED.

## Historical preparation before elevation

The following records the previous pass, not current qualification status.

### Previous status (2026-10-11)

**NOT CLOSED.** The current tool token is `OBJECTLAPTOP\cmmph`, Administrator
`False`. No new Service was installed. A RunAs/UAC launch did not complete;
the waiting tool session was cancelled and produced no elevated matrix report.
Do not infer an elevated run from the historical successful Service reports.
The new shim is **experimental, opt-in**, not enabled in the deployment XML.

## Root cause and evidence

WinSW 2.11 NET461, unchanged official SHA256
`31DCA0D9BD3C5AB7E119172BDB927B90C9DA935D0F4963335EAF60E7D8F7AABF`:

- **FACT:** unsolicited child exit 0 invokes `SignalStopped`, which calls
  `ServiceManager.Open()` with its default `All` access. LocalService lacks
  that manager access. Historical real-Service evidence is the `_Control`
  wrapper log under `target/windows-service-qualification/31df729cbf24402b8763205d42dcba8b/logs/`:
  `CommandException` / native Access denied, with `SignalStopped` on the stack.
  The wrapper remained RUNNING after its finite child exited.
- **FACT:** orderly SCM stop sets `orderlyShutdown` before running the stop
  helper, bypassing that unsolicited-exit branch; nonzero child exit instead
  exits the wrapper nonzero for SCM recovery.
- **FACT:** Hamster's process-lifetime Job Object contains Hamster/PHP, not its
  WinSW parent. Its handle remains live while Hamster survives. It cannot by
  itself contain Hamster on wrapper death.
- **INFERENCE:** the same vendor exit-0 branch applies to a independently
  graceful Hamster exit. The new harness reproduces this with actual Hamster;
   that specific Service case remains NOT VERIFIED this turn.

Primary source: [WrapperService.cs](https://github.com/winsw/winsw/blob/v2.11.0/src/WinSW/WrapperService.cs),
[ServiceManager.Open](https://github.com/winsw/winsw/blob/v2.11.0/src/WinSW.Core/Native/Service.cs).
Do not broaden SCM ACLs or substitute LocalSystem. Merely changing stop helper
timeouts or mapping Hamster's successful exit to a crash does not fix this.

## Pinned integration mechanism (original preparation notes)

`examples/winsw/HamsterLifecycleExtension.cs` compiles against the pinned
vendor assembly without changing the vendor executable or Rust server:

1. `OnWrapperStarted` validates the exact vendor SHA256/assembly/service layout and obtains
   the Service's existing status handle. It runs before WinSW spawns Hamster.
2. Create a private, unnamed, non-inheritable Job Object; configure
   KILL_ON_JOB_CLOSE with no breakaway; self-assign WinSW. A static rooted
   SafeHandle stays process-lifetime. Never close it in `BeforeWrapperStopped`.
   Failure propagates before application startup, without an uncontained fallback.
3. A synchronous CLR first-chance hook filters only `WinSW.CommandException`,
   native error 5 and the exact pinned `SignalStopped` stack frame. At this
   point the vendor has already observed child exit 0. Report STOPPED once
   through the existing status handle, then exit 0. No new SCM access is opened.
   Unrelated errors, nonzero exits and orderly SCM stop retain vendor behavior.
   Failed status reporting retains the original failure, not a fake success.

[SetServiceStatus](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-setservicestatus)
uses the registered Service status handle; no process/PID discovery is needed
for that operation. Actual CLR callback/status ordering, Service-stop races,
extension assembly loading and SCM recovery were subsequently exercised above.
Vendor [extension manager](https://github.com/winsw/winsw/blob/v2.11.0/src/WinSW.Core/Extensions/WinSWExtensionManager.cs)
propagates startup/load failures; `WrapperService.DoStart` invokes it before
`StartProcess`. Thus missing/failed containment initialization does not proceed
to application spawn in the pinned implementation. Disabling/removing the XML
entry is an operator policy change, not protected by the extension.
The shim depends on private vendor layout and must remain pinned; it is not a
generic extension or a reason to increase account privileges.

Expected inherited tree: WinSW → Hamster → PHP → ordinary descendants, plus
its stop helper. Existing Hamster containment remains a nested safety boundary.
The operator-started php-cgi and independent instance are outside this tree.
Wrapper crash is **abnormal termination**, not successful transaction drain.

## Complete elevated before/after rehearsal

Run **once from elevated Windows PowerShell**:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "D:\Project\Hamster-Wheel\examples\winsw\Invoke-LifecycleQualification.ps1"
```

The command never elevates itself. Missing Administrator returns 3 before
writes/SCM changes. Each stage uses two unique scratch LocalService Services,
private copies/ACLs/ports/logs and the release Hamster plus real PHP.

- Baseline (`-LifecycleBaseline`): busy direct control stop, release/drain,
  actual main exit 0, retained live wrapper / fake RUNNING and access-denied
  diagnostic. Then wrapper-only termination while idle, retained main still
  serving. Recovery is temporarily disabled on **that owned baseline Service**
  to avoid deliberately creating a competing orphan instance. Both expected
  failures are recorded as `FAIL (REPRODUCED)`, not production PASS.
- Fixed (`-LifecycleExtension`): the direct-stop transaction completes before
  main/wrapper exit 0 and SCM STOPPED. Rerun normal SCM graceful drain/ACK,
  main-crash recovery, replacement containment, reload/rollback, ACL, rotation
  and forced fallback. Kill only the retained wrapper handle during idle,
  busy PHP, STOP_PENDING and two additional recovery cycles. Require every
  retained old main/PHP/descendant/stop-helper handle to signal; recover real
  HTTP/PHP/HTTPS/FastCGI, check one matching main and independent endpoint.
  Automatic recovery after STOP_PENDING crash is a falsifiable assertion,
  not assumed Windows behavior; a different observation must be reported.

JSON evidence includes identity/creation timestamps, mechanism, reclaim and
recovery times. Snapshots discover candidates or diagnose counts; termination
uses retained handles after image/argv/parent/creation checks. Cleanup disables
only owned recovery, waits handles, uninstalls own Services/Event Log sources.
Successful scratch files are removed; failures retain diagnostics. No reboot,
firewall, original WinSW replacement or broad system-permission change.

## Prior non-Service verification (historical)

| Check | Result |
| --- | --- |
| WinSW cause/source and historical native access-denied log | FACT |
| Shim strict compilation, x64 Win32 layouts, non-Service fail-closed, unrelated exception filtering | PASS (non-Service only) |
| Actual unsolicited Hamster exit 0 / shim SCM status | NOT VERIFIED |
| Wrapper-only idle/busy/stopping crash, repeated SCM recovery | NOT VERIFIED |
| Release process rehearsal: PHP/TLS/drain/ACK/replacement/forced fallback/isolation | PASS |

Fresh process-only evidence:
`target/windows-service-qualification/63e55b41ec0942a19b5f6eadb312bb71-results.json`.
Recorded normal main exit 0/ACK; main 12788 crash reclaimed initial/replacement
ordinary tree in 20 ms; 12-second grace fallback converged in 12,352 ms. These
are lifecycle observations, not throughput or SCM wrapper results. All owned
processes were waited, scratch deployment/keys removed, JSON retained.

Windows default suite (244 passed), strict MSVC Clippy, fmt, release build,
all profiling bench linking, containment (4) and ignored control (14) passed.
An existing parallel multi-pool test initially failed (8/9): sending A's
headers did not prove A had reserved memory before B competed. An isolated
rerun passed. The regression now waits for HTTP `100 Continue`, emitted when
the body is polled after declared-length reservation, before sending B. This
strengthens ordering without changing the budget, assertions or production
code; the complete parallel suite then passed three times (9/9 each). Fresh
PHP cancellation regressions/stress (2), possibly-started no-retry (1), timeout
no-duplicate (1) and busy shutdown (1) passed. All six PowerShell scripts parsed,
the original XML parsed, 42 worker/CGI PHP fixtures linted and diff check passed.

Files added this turn: this report, `HamsterLifecycleExtension.cs`,
`Build-LifecycleExtension.ps1`, `Test-LifecycleExtension.ps1` and
`Invoke-LifecycleQualification.ps1`. Existing qualification/probe scripts,
multi-pool test, operations doc and handoff were updated. Prior dirty Rust,
Cargo/icon/XML/control/TLS changes are preserved. Final tracked diff summary
(including prior work): 12 files, 1347 insertions / 132 deletions before this
last documentation update; untracked files are separately visible in
`git status --short`. No commit/tag/push/merge/release. No new Service remains
in the scoped inventory; original cached WinSW SHA256 is unchanged. Compile
cache and ignored JSON evidence are intentionally retained. Do not interpret
old report PIDs as cleanup authority.

## Previous blocking environment (historical, superseded)

The only blocker to actual before/after Service evidence here is an elevated
execution environment. Do not deploy the experimental shim based on build or
process-only PASS. Private CLR/vendor coupling and shutdown/recovery timing
are unresolved until the matrix runs. Existing SCM-stop qualification remains
valid only within its previous scope. Linux, boot/reboot and SYN-blackhole are
outside this pass / NOT VERIFIED. The four previously deletion-denied diagnostic
folders are preserved; never bypass that refusal to claim full cleanup.
