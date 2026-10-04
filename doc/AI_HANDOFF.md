# AI Handoff

## Windows headless process rehearsal (2026-10-04)

Current `dev` checkout was clean at `0b7fbbe` before this pass. Added the
ignored, self-cleaning `tests/windows_headless.rs` process rehearsal and
updated `doc/WINDOWS_OPERATIONS.md`; no production code was changed. The test
starts Hamster with `DETACHED_PROCESS` and redirected logs, one PHP worker,
and a separately started `php-cgi`. Both routes served successfully. A
separate process could not attach to Hamster's console. On exact Hamster
`TerminateProcess` (PID 18936), the already-open handle for worker PID 17268
signaled exit in 2 ms; operator-owned `php-cgi` PID 6388 stayed alive. A new
Hamster PID 17612 and worker PID 20012 served both routes through the same
endpoint. These PIDs are evidence from one run, not stable IDs. The test
cleaned up all processes and temporary files it created.

An initial `CREATE_NO_WINDOW` attempt inherited the test harness console and
was rejected as a headless test. The passing `DETACHED_PROCESS` probe observed
`AttachConsole` fail with `ERROR_INVALID_HANDLE`.
The unignored TLS headless test replaced the configured certificate/key and
confirmed a new TLS handshake still presented the old loaded leaf; there is
no implicit file-watch reload.

WinSW is a **candidate** Service wrapper because its official configuration
supports restart on failure, logs, service accounts and stop timeouts. No
Windows Service was installed or run: this checkout lacks an isolated Service
test environment and the current user is not elevated. WinSW
`hidewindow=true` gives `CreateNoWindow`, not the tested `DETACHED_PROCESS`;
its documented stop fallback is forced termination if Ctrl+C cannot be
delivered. Hamster currently has only console Ctrl+C/Ctrl+Break for graceful
stop/certificate reload. Console-free graceful stop and live cert reload are
**NOT IMPLEMENTED**. Actual WinSW restart/log rotation and external
`php-cgi` containment/restart are **NOT VERIFIED**. An isolated VM Service
rehearsal and a deliberate
console-free control-path design are the next operational decisions; do not
integrate a Service wrapper merely on the process-level rehearsal.

TCP SYN-blackhole remains **NOT VERIFIED**; no host firewall or unrelated
service was changed. Native Linux remains **NOT VERIFIED**. No commit, tag,
push, merge or release was made in this pass.

## Windows handoff audit (2026-10-04)

This pass began from a clean `dev` checkout at `6e8fd34` (`origin/dev`); the
older sections below describe their own historical uncommitted state, not the
current Git baseline. No commit, tag, push, merge or release was made here.
TCP SYN-blackhole behavior is **NOT VERIFIED** because no isolated Windows VM
was available. The host firewall and unrelated services were not changed.

The highest-value confirmed handoff gap was operational ambiguity, not a new
FastCGI throughput issue. `server::run` binds and prints `listening` after
Hamster-owned PHP workers reach READY, but external FastCGI and proxy
endpoints are not startup-probed. A new FastCGI regression confirms an absent
operator-managed endpoint answers 502 and a later-started endpoint answers
200 without restarting Hamster. `doc/WINDOWS_OPERATIONS.md` now states startup,
backend ownership, readiness checks, private-file policy, Windows signals,
shutdown and qualification boundaries. `doc/CONFIGURATION.md` no longer says
TLS certificates cannot reload: Ctrl+Break reloads only the configured
certificate/key files in an attached Windows console. General config reload
and headless certificate reload remain unavailable.

Windows verification at this checkout: fmt, strict MSVC Clippy, default tests
(180 library tests passed), release build, profiling bench link, eight real
`php-cgi` integration tests, four exact-process abnormal-parent containment
tests, Ctrl+C worker reap and in-flight drain, and Ctrl+Break TLS reload with
rollback all passed. No throughput benchmark was run because production
hot-path code was unchanged. Native Linux is still NOT VERIFIED. Windows
Service operation and packaging are not implemented; an operator must own
external FastCGI processes and their containment/restart policy.

## Windows FastCGI blocking-pool saturation correction (2026-10-04)

Still uncommitted on `dev`; preserve all earlier working-tree changes. A
deterministic one-thread Tokio blocking-pool test falsified the assumption
that dropping/aborting a queued `spawn_blocking` connect promptly releases
its captured exchange claim. Before correction, a 100 ms connect waiter
returned in ~114 ms but its slot remained held after ~368 ms while an
unrelated blocking closure occupied the thread.

`src/fastcgi.rs` now uses a per-connect claim handoff: cancellation or timeout
reclaims the exact server claim and exchange permit if the blocking closure
has not taken them; after handoff, the closure owns them until connect ends.
The closure never writes a FastCGI frame. A second per-location semaphore
caps queued/running blocking connect closures to the endpoint-slot count;
when full, new attempts use cancellable async connect. This avoids replacing
the old delayed-slot problem with an unbounded Tokio blocking queue. No
connection reuse, endpoint lifecycle or protocol change was made.

After correction, the queued timeout returned and reclaimed its slot at
~109 ms before the blocker was released. Queued client cancellation
reclaimed its slot in 23–102 µs across two controlled runs; async fallback
connected locally in ~1.5 ms without waiting for the blocker. A separate
handoff test proves a closure that already took the claim retains its slot
until it exits despite waiter cancellation. A
silent backend that received the complete request produced 504 and a
graceful shutdown in ~412 ms after a 400 ms response deadline, without
replay. A TCP SYN-blackhole was not reproduced; no system firewall was
altered. A running blocking OS connect remains non-cancelable until return,
and unrelated stalled blocking work can still delay Tokio runtime teardown.

Default-feature release Hello C32, same .NET driver (3 fresh runs, 8 s,
100 warm-up), produced W1 min/median/max 587/643/880 RPS and W2
240/910/1,210, zero errors. Host variance is high; these non-interleaved
groups do not prove a regression against the earlier 1,405/1,678 medians.
The interleaved profiling A/B favored current blocking over async in 11/12
W1/W2 C16/C32 pairs, with zero errors. Exact latency, CPU and RAM and
methodology are appended to `doc/BENCHMARKS.md`.

Windows fmt, clippy `-D warnings`, default tests (179 library tests passed),
eight real-`php-cgi` integration tests, release build, profiling bench link,
PHP fixture lint and `git diff --check` passed. A final exact-path process
and temp-directory check found no Hamster/PHP process or new benchmark/session
scratch directory from this pass. Earlier ignored application workspaces were
left untouched. Do not commit or push.

## Windows FastCGI connect-path corrective pass (2026-10-04)

Uncommitted on `dev` at `6fd830d`; preserves all earlier working-tree changes.
Profiling-only stage samples with real operator-started `php-cgi.exe` showed
W1/C32 async connect mean 5–11 ms, backend-head mean <1 ms and median slot
wait 213–371 ms. Direct FastCGI controls and END_REQUEST-vs-EOF controls
falsified the idea that TCP close timing itself caused this. An interleaved
async-vs-blocking connect A/B won in every one of 12 W1/W2 C16/C32 paired
rounds; the precise Windows kernel/IOCP mechanism remains unknown.

Windows now uses `std::net::TcpStream::connect_timeout` on Tokio's blocking
pool only for FastCGI `concurrency = 1` and `connect_timeout <= 5 s`.
`ConnectAttempt` and `ExchangePermit` carry both the server claim and bounded
exchange permit into the blocking task. If the HTTP waiter disappears, the
task cannot send a FastCGI frame and releases claims when it finishes. On
connect failure before any frame, another endpoint may be tried; once a
request may have started, no replay. Multiplexing endpoints and longer
timeouts retain async connect. Protocol, endpoint ownership and static-file
policy are unchanged.

Independent default-feature MSVC release runs (3 fresh processes/cell, 8 s,
100 warm-up, Hello/JSON, HTTP C16/C32, W1/W2) had zero errors. Hello W1/C32
median 1,405 RPS versus historical 143 with the same driver but
non-interleaved runs; W2/C32 median 1,678. JSON W1/C32 median 837 and W2/C32
1,216. Exact min/median/max, latency, CPU and RAM are in `doc/BENCHMARKS.md`.
The improvement direction is supported by paired A/B; do not quote an exact
production percentage across host-drifted groups. No connection reuse was
added. The previous timeout/intermittency and file-boundary notes remain
below. Native Linux remains unverified.

Verification for this pass: Windows fmt and clippy `-D warnings`, default
`cargo test`, the eight real-`php-cgi` fixture tests, MSVC release build,
profiling bench link, PHP lint and `git diff --check` passed. The manual
release profiling controls and load A/B passed with validated responses and
zero errors. No repository `php-cgi.exe` or Hamster process and no new
benchmark/session temp directory remained after measurement. The prior
ignored `target/` application workspaces remain untouched.


## Windows FastCGI follow-up — load, failure and file-boundary review (2026-10-04)

Still uncommitted on `dev` at `6fd830d`; preserve the earlier corrective
working tree and the pre-existing research backlog. A valid UTF-8 path segment
such as `%C3%A9abc` previously panicked in FastCGI's `.php` suffix detection
because it sliced a Rust string at a byte offset; a failing regression proved
it, and the suffix check now uses bytes. The full FastCGI unit group has 21/21
passing, including deterministic slot saturation (503), backend timeout (504),
backend disconnect (502), and no replay of a potentially executed exchange.

Five parallel runs of the eight real-`php-cgi` fixture tests each passed 8/8;
the earlier two 60 s test timeouts have not reproduced. The old host
sleep/resume explanation remains **unproven**: Windows System event 1 reports
a low-power interval from 2026-10-01 19:54:38 UTC to 2026-10-04 08:20:50
UTC, but the failed test log lacks timestamps that tie it to that interval.
No timeout was raised merely to hide it. A fresh release load probe used the
existing driver with 3 fresh process runs/cell, 8 s measured, 100 warm-up,
Hello/JSON, 1 or 2 operator-started endpoints and HTTP C16/C32. All validated
responses succeeded. One endpoint at C32 fell to ~142–143 median RPS with
~227–233 ms P50; C16 was
~304–395 RPS with ~31–49 ms P50. Two-endpoint results had large spread;
`doc/BENCHMARKS.md` has exact cells. This does not isolate TCP connection
cost, so no connection reuse or endpoint-management expansion was made.

File-boundary audit: host `deny_paths`/`deny_names` run before location routing
and against final opened paths for filesystem handlers; dotfiles and PHP
source extensions remain closed by default. Application-specific private
trees still require explicit rules. Static bytes use the validated opened
handle on Windows, but PHP's pathname handoff is not object-identity-atomic
under concurrent namespace changes. No new file-boundary regression was
found beyond the Unicode panic. Linux has not been natively verified.

Windows gates after this follow-up: fmt, clippy `-D warnings`, default
`cargo test`, MSVC release build, profiling bench link, relevant PHP lint and
`git diff --check` all passed. No new persistent benchmark scratch artifacts
were left; the driver removes each temporary run directory. The earlier
ignored `target/` application workspaces listed below predate this follow-up
and were not touched.

## Current Windows corrective pass — FastCGI private files and gateway status (2026-10-04)

Checkout `dev` was fast-forwarded to `origin/dev` at `6fd830d` before this
pass. The earlier FastCGI v1 implementation is committed there; the changes
in this section are uncommitted. Do not commit/tag/push/merge/release without
an Owner request. The pre-existing untracked
`doc/HAMSTER_RESEARCH_BACKLOG_2026-09-27.md` was not touched.

- Host-wide `deny_paths`/`deny_names` now cover static and FastCGI routing,
  including overlapping locations, case/percent variants and final opened
  paths behind in-root junctions. Filesystem handlers deny dotfiles by
  default except root `/.well-known`, and never return PHP source bytes.
  Configure application-specific private trees; `.htaccess` is still ignored.
- PHP worker execution deadline now answers 504 instead of 502. Timeout still
  kills/reaps and replaces its worker, never replays the possibly-started
  request, and remains outside infrastructure failure backoff. Initial and
  replacement generations both use configured `php.startup_timeout` (default
  5 s; demonstrated by the 6.5 s delayed-READY fixture with 12 s setting).
- A new FlatPress 1.5.1 application is rebuilt from a pinned, hash-checked
  release ZIP and installed over two operator-managed `php-cgi.exe` endpoints;
  public home and private-tree 404 checks passed. Fresh DokuWiki integration
  passed. The original `prepare_php_apps.ps1` reached DokuWiki and Slim but
  Symfony's final asset-map compile timed out contacting external Iconify;
  that incomplete workspace does not prove Symfony qualification.
- Release FastCGI micro-workload results: 3 fresh-process runs/cell, 6 s,
  200 warm-up, two endpoints, HTTP C1/C4/C16/C32, Hello/JSON, zero errors.
  Median RPS ranges 186-708 Hello and 189-997 JSON, with substantial
  C16/C32 variance; see `doc/BENCHMARKS.md`. A one-endpoint C16 comparison
  was slower. No evidence isolates fresh TCP connection cost, so no reuse
  optimization was made.
- Windows fmt, clippy `-D warnings`, default cargo test, release build,
  `cargo bench --no-run --features profiling`, relevant PHP lint and
  `git diff --check` passed. Seven targeted PHP worker ignored regressions
  and the cold-READY startup/replacement test passed. The full ignored
  FastCGI suite passed 10/10 when serialized. An earlier parallel run was
  interrupted across an apparent host sleep/resume and had two 60-second
  client timeouts; that cause is an inference, not proven. Do not record that
  run as passing. No `php-cgi.exe` or `hamster-wheel.exe` from this pass remains.
  The tool policy rejected `Remove-Item` for this pass's ignored `target/`
  scratch workspaces and benchmark JSON, so those artifacts still need exact
  cleanup: `target/flatpress-app-20261001`,
  `target/flatpress-rebuild-20261001`,
  `target/flatpress-rebuild-20261001-b`,
  `target/flatpress-suite-20261001`,
  `target/flatpress-serial-20261004`, `target/flatpress-final-20261004`,
  `target/php-apps-rebuild-20261001`, and
  `target/fastcgi-bench{,-one-endpoint}-20261001.json` (use the exact
  directory names shown by `Get-ChildItem target`). `www/`, `ssl/` and older
  investigation directories were untouched.

Next action: review this corrective diff, rerun the interrupted parallel
FastCGI suite only if concurrency qualification is required, and remove the
listed scratch artifacts when filesystem deletion is available. The next Windows product
decision should be based on the operator-managed FastCGI boundary, not an
automatic move to managed endpoints; remaining limitations include explicit
private-tree policy, endpoint ownership by the operator, chunked upload 411,
and the script pathname handoff under concurrent filesystem changes. Native
Linux remains unverified.

## Current Working Scope — third pass: FastCGI v1 slice + open issues (2026-09-28)

Uncommitted, on top of the second pass below. Not committed, tagged or
released; Owner review pending. Windows only; Linux NOT VERIFIED (the Linux
target's standard library is not installed, so even `cargo check` does not
run on this machine).

- FastCGI v1 (`src/fastcgi.rs`, `doc/FASTCGI_V1.md`): `handler = "fastcgi"`
  runs scripts under `root` through operator-started `php-cgi -b` endpoints
  (Hamster does not start them). Script/static/front-controller resolution
  with the static handler's opened-handle containment, CGI/1.1 variables with
  httpoxy removed, streamed request body written concurrently with the
  response, streamed response, connect-only failover, per-location slots,
  503 busy / 502 unavailable or malformed / 504 response timeout / 411
  chunked. Evidence: 18 unit tests (fake FastCGI server), 8/8 mutations
  caught, 9 ignored `tests/php_cgi.rs` tests against real `php-cgi.exe`
  (fixture `fixtures/php-cgi` + DokuWiki 2026-07-14c unmodified: pages, edit,
  media upload, session cookie), stable 3 of 3 parallel runs.
  `validates_and_compiles_fastcgi_locations` in `tests/configuration.rs`.
  DokuWiki's `data/` is served unless closed with `not_found` locations
  (measured); `.htaccess` is not read.
- Test isolation: `src/test_ports.rs` gives "dead"/free ports from outside the
  Windows dynamic range; three tests used bind-then-drop ports that parallel
  tests could be handed (3 failures in 40 concurrent suite runs, 0 in 48
  after). Likely cause of the `upstreams_of_different_hosts_are_independent`
  flake (inference: its own failure was never captured).
- Lingering close: 16 MiB discard bound removed (.NET 413 0/10 -> 10/10 up to
  100 MiB); total bound is `body_read_timeout`; 1 s idle bound kept.
- Cold READY: per-pool `php.startup_timeout` (1-300 s, default 5). Amends the
  accepted 5 s budget (`doc/WORKER_LIFECYCLE_0_3.md`): Owner review.
- Adapter defect fixed: repeated request headers are joined as CGI does.
- `prepare_php_apps.ps1` now downloads DokuWiki. Its URL and `tar.exe`
  extraction were checked by hand: the tarball's tree is identical to the test
  workspace except runtime files DokuWiki wrote itself (`data/`,
  `conf/plugins.local.php`). The whole script was not re-run from an empty
  directory after this change.

Gates, 2026-09-28 afternoon, Windows: fmt ok; clippy failed on two lints in
`src/fastcgi/tests.rs` (fixed; clippy then clean); `cargo test` 171 lib + 31
configuration + integration all pass; ignored: lib 9, multipool 9, php_worker
69 (with the Laravel stand-in), visibility 1, timeout 10, tls_reload_process
3, windows_containment 4, php_applications 11, php_cgi 9; bench link ok;
release ok; Linux `cargo check` cannot run here (target std not installed).
No `hamster-wheel`/`php-cgi`/repository `php.exe` left afterwards; 450 stale
test/probe temp entries from earlier killed runs removed from `%TEMP%`.
Then, misreading a request to stop test processes as a request to delete
files, the session also deleted the nine `%TEMP%\hamster*` investigation/PMS
copies and every scratch workspace, including the `php-apps` (Symfony Demo,
Slim, DokuWiki) and Laravel stand-in used by these gates. `www/` was not
touched (`www/pms` 14,903 files, `git status` clean). Rerunning the ignored
application suites needs `prepare_php_apps.ps1` and `prepare_scratch_app.ps1`
first; the `%TEMP%` investigation notes are not recoverable.

Owner decisions open: managed php-cgi lifecycle (a/b/c in
`doc/FASTCGI_V1.md`); one timeout status policy for gateways (FastCGI 504,
proxy 504, PHP worker 502); deny-list convenience for FastCGI locations;
`startup_timeout` amendment.

Next Windows step: decide managed vs operator-managed php-cgi; if managed,
build it as its own slice under the lifecycle matrix. Then a FastCGI load
measurement (one connection per request is unmeasured) and one more
SAPI application (for example WordPress on SQLite) before calling FastCGI
general.

## Current Working Scope — PHP application compatibility on Windows (2026-09-28, second pass)

Working tree on top of `7ca716f`, together with the uncommitted console and
output work recorded in the next section; nothing committed, tagged, pushed or
merged. `ssl/` and `www/` untouched: every application in this pass was built
in a scratch workspace. The prior section's fixes were re-verified first
(console tests 6/6, TLS reload process 3/3, default suite).

**Scope.** Hamster was exercised as a general PHP server with independent
applications of different structure, each in its own pool, alone and side by
side: Symfony Demo 3.1.0 (Symfony 8.1, external, unmodified), Slim skeleton
4.5.0 (external, application code unmodified), a framework-free test
application in `fixtures/php-apps/native` (simulated), and the Laravel stand-in
(simulated). The authoritative record, including what persistent workers cannot
run, is `doc/PHP_APPLICATION_COMPATIBILITY.md`.

**Product defect found and fixed: early refusals were lost as resets.** A 413
or 503 sent before reading a body reached curl, but .NET's HttpClient, which
sends the whole body first, saw a connection reset in 20 of 20 two-megabyte
uploads against the `7ca716f` release build. The Owner had deferred lingering
close until a real client showed the need; this is that evidence.
`src/lingering_close.rs` wraps every socket: when a connection closes while
its last request declared a body, or with bytes waiting, it sends FIN and
discards input until the client closes, 1 s idle, 5 s or 16 MiB; never after
shutdown has begun. .NET now gets 413 in 20 of 20. The first version lacked the
idle and shutdown bounds and the existing
`shutdown_waits_for_body_deadline_but_not_forever` caught it. Four
regressions, five mutations each caught 3 of 3.

**Adapters.** `symfony_worker.php` (any HttpKernel application) and
`psr15_worker.php` (any PSR-15 handler, reached through an entry file such as
`entries/slim_skeleton.php`) join the Laravel adapter. The framework-neutral
SAPI work moved out of `laravel_request.php` into `adapter_support.php` and
`httpfoundation.php`; all 11 Laravel tests still pass. Adapter-level fixes: a
response over the 8 MiB frame is now `500 Response Too Large` with the worker
kept (it used to kill the worker and answer 502), including in the Laravel
adapter; the PSR-15 adapter closes a native PHP session after every request,
because under the CLI SAPI it would otherwise leak into the next client's
request.

**Compatibility boundary, measured.** Under the CLI SAPI `header()`,
`setcookie()`, `http_response_code()` and native session cookies do nothing,
superglobals are not per request, and the `STDOUT` constant is closed.
Applications built on those need FastCGI (roadmap 0.6, not implemented). The
Slim skeleton's own `SessionMiddleware` reads `$_SERVER` and so never activates
under Hamster, and its `ResponseEmitter` headers are reproduced by the entry
file; both are stated in the entry file, not hidden.

**Environments are rebuildable.** `fixtures/php-apps/prepare_php_apps.ps1`
built the workspace from nothing in 3 min 14 s; `tests/php_applications.rs`
(9 ignored tests) runs against it with `HAMSTER_PHP_APPS`, adding the Laravel
stand-in when `HAMSTER_LARAVEL_TEST_PATH` is set.

Verified on Windows after the final change: format, strict Clippy, default
suite (147 library, 30 configuration, 7 routing, 5 static), every ignored suite
(9 library, 9 multipool, 68 `php_worker`, 1 visibility, 10 timeout, 3 TLS
reload process, 4 containment, 9 `php_applications`), MSVC release build,
profiling bench link. `upstreams_of_different_hosts_are_independent` failed
once in 48 full library runs of the final tree and never in 30 runs alone; its
assertion was not captured, it concerns upstream connection counters on a
"dead" port that a parallel test can reuse, and it was not attributed.

**Test harness leak, found after the report and fixed.** The console tests
start the real server in its own console and owned it only through
`kill_on_drop`. When the test process itself was ended (timeout, terminal
stop, killed `cargo test`) the server survived with its PHP workers: killing
the test binary mid-run left `hamster-wheel.exe` plus two `php.exe` for
`ctrl_break_reload_leaves_php_workers_serving` and `hamster-wheel.exe` for
`ctrl_break_reloads_after_stdout_is_closed`; Ctrl+C in the test's terminal
cannot reach a server in another console. The same applied to the older
`ctrl_c_stops_server_and_reaps_pool` and the TLS reload process test.
`tests/common/mod.rs` now joins the test process to a kill-on-close Job
Object before any server is spawned. Re-run of the same abrupt kill, three
times each with the server confirmed alive at the kill: nothing survived.
In-process tests were never affected, since their PHP workers are in the Job
Object Hamster creates. Product code did not change for this.

**Not verified:** the real PMS (the only Octane-enabled copy uses the live PMS
MySQL and Redis, which this pass must not touch; no isolated copy exists);
native Linux (`x86_64-unknown-linux-gnu` is not installed here, `can't find
crate for core`); frameworks other than the four above; request bodies over
1 MiB (protocol limit) and streamed responses (buffered by design).

Open for the Owner:

- READY budget. Fresh copies booted cold in 3.4-3.8 s (Laravel stand-in) and
  1.7 s (Symfony Demo) against 0.1-0.3 s warm; the pool's READY limit is a
  fixed 5 s in the accepted lifecycle contract. One side-by-side run right
  after building the workspace failed startup this way. Keep it fixed, or make
  it configurable per pool like `request_timeout`.
- Whether the Symfony and PSR-15 adapters become a supported product surface
  (the roadmap's 0.6 persistent-adapter contract) or stay test fixtures.
- Lingering close holds a connection slot for up to 1 s idle / 5 s total after
  an early refusal; these are constants, not configuration.

## Recorded Checkpoint — Windows console and output robustness (2026-09-28)

Working tree on top of `7ca716f`; nothing committed, tagged, pushed or merged.
`ssl/` and `www/` were not touched.

**Closed-pipe output is fixed.** Every operational line in `src/` now goes
through `outln!`/`errln!` (`src/output.rs`), which drop a line that cannot be
written instead of panicking. Before, with the pipe's reader gone (Windows
error 232), three process regressions failed on the unfixed tree: Ctrl+C with
stdout closed exited 101 before worker shutdown; with stderr closed, one PHP
worker crash killed the whole server (exit 101, the crashing request got an
empty response); with stdout closed, the first Ctrl+Break reload killed the
reload task and the second ended the process with `STATUS_CONTROL_C_EXIT`.
Mutations show which paths matter: a connection task's line (the accept loop
then panicked on reporting it), the pool supervisor's line (the pool then
answered 503 for good), the shutdown line and the reload line. A generation
task was already contained. Unit tests keep the standard macros so libtest
still captures them; integration tests that run the library in-process now
show its diagnostics in their output.

**A larger Windows defect was found and fixed: PHP workers shared Hamster's
console.** A console delivers Ctrl+C and Ctrl+Break to every attached process
and `php.exe` ends on either. So Ctrl+C, the documented stop, killed workers
mid-request (in-flight request `502 PHP Worker Unavailable`, worker exit
0xC000013A) instead of draining them, and Ctrl+Break, the documented TLS
reload, killed and replaced every worker on each reload. The 0.2 docs had
recorded the Ctrl+C half as an observation; the reload half was new. Workers
are now spawned with `CREATE_NO_WINDOW`. Cost, measured in release with eight
workers over five alternating runs: one `conhost.exe` per worker (about
1.2 MiB private each) and a READY time of 104.2 ms instead of 86.6 ms, paid at
spawn only (`benchmarks/results/worker_console_isolation_20260928.md`).
Rejected: `DETACHED_PROCESS` (no conhost, but console programs started by the
application would get visible windows; reasoning, not measured).

New permanent Windows process regressions (ignored, need PHP and a console):
`a_closed_stdout_does_not_turn_ctrl_c_into_a_panic`,
`a_closed_stderr_does_not_stop_a_crashed_worker_from_being_replaced`,
`ctrl_c_drains_an_in_flight_php_request`,
`ctrl_break_reload_leaves_php_workers_serving` (`tests/php_worker.rs`) and
`ctrl_break_reloads_after_stdout_is_closed` (`tests/tls_reload_process.rs`).
They hold worker process handles, not PIDs, and require exit code 0 (SHUTDOWN)
or 42 (the deliberate crash); a first PID-based version failed once in a
parallel suite through PID reuse. All five failed on the unfixed tree.
`execution_timeouts_never_enter_the_infrastructure_backoff_ladder` failed once
in six parallel runs: it read the counter the moment the 502 arrived, before
the supervisor records the timeout. It now waits for the record.

**Test environment.** The previous section described `www/laravel`,
`www/plain-php` and 64/64 on the machine that wrote it. This machine's `www/`
holds only `pms`, a plain clone of the real PMS (no Octane, no probe), and the
`%TEMP%` scratch PMS copy has an incomplete vendor tree. The Windows document
could not rebuild the stand-in: its models and migration were described, not
given; `& $php artisan` with arguments inside `$php` does not run in
PowerShell; and `composer.bat` strips `^`, so `laravel/octane:^2` reached
Composer as exactly 2.0.0 and failed to resolve. The new
`fixtures/php-worker/pms/prepare_scratch_app.ps1` writes the same application
as the `dev-linux` shell script and built it from nothing in 2 min 27 s.
`doc/LOCAL_TEST_APPLICATIONS.md` now separates the stand-in (rebuildable,
synthetic) from the real PMS (owner's source, database and Redis).

Verified on Windows in this pass, with the rebuilt stand-in: format, strict
Clippy, default suite (144 library, 30 configuration, 7 routing, 5 static),
every ignored suite (9 library, 9 multipool, 68 `php_worker` including all
eleven Laravel tests and the new console tests, 1 visibility, 10 timeout,
3 TLS reload process, 4 containment), MSVC release build and profiling bench
link. The release binary with `hamster.toml` served `/`, `/php` and `/api`
(200/200/404) with one `conhost.exe` per worker and left no `php.exe` after
being killed. Before the timeout-test fix the complete `php_worker` suite ran
six times in parallel: five clean, one with the two races described above.

**Not verified:** anything against the real PMS in this pass; native Linux.
On this machine `rustup target list --installed` shows only
`x86_64-pc-windows-msvc`, so `cargo check --target x86_64-unknown-linux-gnu`
stops at `can't find crate for core`. The 2026-09-27 statement that the Linux
target is installed and `aws-lc-sys` needs `x86_64-linux-gnu-gcc` describes the
machine that wrote it. The target was not installed here.

Open, for the Owner: whether one `conhost.exe` per worker (and +18 ms per
spawn) is acceptable, or whether the Windows Service work in 0.8 should revisit
the flag; and how a real-PMS environment should be provided for the next pass
(an Octane-enabled copy of `www/pms` needs the platform ignore recorded in
`doc/LARAVEL_WORKER_0_4.md` and touches the PMS MySQL and Redis).

## Recorded Checkpoint — Windows readiness pass (2026-09-27)

A verification pass, not a feature. One test changed; no production code did.

Two recorded diagnoses below this section are wrong and are corrected here.

**`ctrl_c_stops_server_and_reaps_pool` was never a ConPTY problem.** Windows
lets a process disable Ctrl+C for itself with `SetConsoleCtrlHandler(NULL,
TRUE)`, and that attribute is inherited by every child it creates. Shells
disagree about setting it, so when one that sets it launches Cargo, the
attribute reaches the server under test and the console event is swallowed;
Ctrl+Break is unaffected because the attribute covers Ctrl+C only. A variant
matrix (piped and unpiped stdio, null and real helper handler, Ctrl+C and
Ctrl+Break) reproduced it in all Ctrl+C cases and in none of the others, and
clearing the inherited attribute in the parent before spawning fixed it. The
test now clears it and passes; it passed three consecutive runs and inside the
full suite. Hamster itself was never at fault: a server launched from a shell
without the attribute shuts down on Ctrl+C in about 30 ms with every worker
reaped, measured directly.

**The Linux target is installed.** `rustup target list --installed` reports
`x86_64-unknown-linux-gnu` beside the MSVC target. The cross-target check is
blocked by something else: `aws-lc-sys` (through `tokio-rustls`, a mandatory
dependency since TLS v1) compiles C and needs `x86_64-linux-gnu-gcc`, which
this machine does not have. Linux is still NOT VERIFIED, and now not even
compile-checked; the reason in the older sections is inaccurate.

Real applications now exist for the ignored tests. `www/` holds a Laravel
12.69.2 application with Octane 2.20 and the checked-in probe provider, plus a
framework-free persistent PHP application and static assets;
`doc/LOCAL_TEST_APPLICATIONS.md` records how they were built, what they cover
and where they are only stand-ins. With `HAMSTER_LARAVEL_TEST_PATH` set, all 64
`php_worker` ignored tests pass, including both PMS isolation tests, which had
never run on this machine.

Verified on Windows in this pass: format, strict Clippy, the default suite
(144 library, 30 configuration, 7 routing, 5 static), every ignored suite
(9 library, 9 multipool, 64 `php_worker`, 1 visibility, 10 timeout, 2 TLS
reload process, 4 containment), the MSVC release build and the profiling bench
link. Ctrl+C shutdown, Ctrl+Break certificate reload and abnormal-parent
containment were each also exercised outside the test suite against a running
server. No benchmark was run; nothing was committed, tagged, pushed or merged;
`ssl/` and the untracked research backlog were left alone.

Open, unchanged: native Linux, upstream TLS, and the items already listed as
out of scope for Reverse Proxy v1 and TLS v1. One new observation needs an
owner decision: Hamster prints operational lines with `println!`, which panics
if stdout is closed while the process runs. A scratch harness that dropped its
read end saw `failed printing to stdout: The pipe is being closed` and exit
code 101 instead of a clean shutdown. A plain `| head -1` pipe did not
reproduce it, so the exposure is narrow, but a supervisor that closes the pipe
early would turn a graceful stop into a panic.

## Recorded Checkpoint — Transactional certificate reload

After the TLS v1 Windows review, the next capability is implemented in the
working tree. `TlsListener` loads and verifies an entire replacement set before
one atomic `Arc<CertificateSet>` swap. Handshakes hold one set from before the
ClientHello through Host scope selection; established connections retain their
old stream and scope. Invalid key or SAN in any SNI entry leaves the entire
prior set serving. Windows Ctrl+Break and Linux SIGHUP trigger certificate-only
reload from the configured paths; Ctrl+C/SIGINT remain shutdown. No full
configuration reload or TOML change was added. See
`doc/TLS_CERTIFICATE_RELOAD.md` for the operator contract and evidence.

Windows socket regressions for success, rollback, in-progress handshakes,
keep-alive, cleartext, SNI/default selection and successive generations pass.
The Windows process-level trigger is now verified: a `CREATE_NEW_CONSOLE`
server child received `CTRL_BREAK_EVENT` from an attached helper; stdout
reported success, a new handshake showed the replacement leaf, and a prior
TLS keep-alive connection continued serving. Subsequent events with a bad key
and malformed certificate logged field/file-specific failures, left the
process running, and preserved the last good leaf for new handshakes. The
first attempt exposed a test-helper mistake (`SetConsoleCtrlHandler(None, 1)`
does not ignore Ctrl+Break), fixed with an explicit handler. No production
reload code changed in this operational pass. Details and command are in
`doc/TLS_CERTIFICATE_RELOAD.md`. The process test is ignored by default because
it creates a child console; its explicit run passed. Linux SIGHUP remains
unverified.
Native Linux remains NOT VERIFIED: only the MSVC Rust target is installed;
`docker-desktop` WSL has no Cargo and its Docker daemon is unavailable. Windows
format, default tests (144 library passed, 9 ignored; 30 configuration passed),
strict Clippy, MSVC release build, profiling bench link and the ignored PHP
listener-scheme regression passed. No benchmark was run. No
commit, tag, push, merge or release was performed. `ssl/` remains untouched.

## Recorded Checkpoint — TLS v1 (Windows technical review)

The Owner recorded the Reverse Proxy v1 checkpoint and, after the TLS
architecture investigation (`doc/research/TLS_ARCHITECTURE_INVESTIGATION.md`),
moved TLS v1 to implementation on 2026-09-25. The current committed baseline
is `ed5845b`; the final EKU fix and this handoff update are uncommitted. No
tag, push, merge or release was performed in the final review. Authoritative
record: `doc/TLS_V1.md`; measurements:
`benchmarks/results/tls_v1_20260925.md`.

FACT: `[server.tls]` adds an HTTPS listener beside or instead of
`server.listen` (rustls 0.23.45 / tokio-rustls 0.26.5, aws-lc-rs provider,
TLS 1.2/1.3 defaults, ALPN `http/1.1` only). PEM chains and keys are loaded and
checked while the config compiles, before workers start or sockets bind;
configured SNI names must be covered by the leaf. SNI selects the certificate
(exact, one-label wildcard, optional default); the effective HTTP authority
still selects the virtual host. One accept loop serves both listeners under one
`max_connections`. A TLS connection wraps the socket in `WriteProgressTimeout`
beneath TLS, completes its handshake under a total deadline from accept
(default 10 s, 1-60) that shutdown cancels, then enters the same generic
`serve_http` as cleartext. Handlers receive `ConnectionInfo` =
`TransportInfo` + trusted `Scheme`; proxy `X-Forwarded-Proto` and PHP protocol
v3's new scheme byte come only from it. PHP protocol is now v3 (v1/v2 workers
fail startup). The Laravel adapter sets `REQUEST_SCHEME`/`HTTPS` from it.

Follow-up on top of `868eaff` (working tree, 2026-09-25): a TLS connection is
now served only for authorities its presented certificate's configured `names`
cover; anything else is `421 Misdirected Request` (RFC 9110 4.3.3/15.5.20),
checked in the router via `CertificateScope`, never visible to handlers.
`names` may include IP addresses (authorization only, never SNI). Startup now
refuses a leaf or intermediate outside its validity dates and a chain that
does not link (webpki with the file's last certificate as anchor); a bare leaf
with no issuer in the file starts with a note; each certificate's expiry is
printed. The intermittent test failures were identified as pre-existing
Windows reset/flush races (reproduced on `a7a31b4`), not TLS regressions; the
two affected tests now assert only guaranteed behavior. Details and evidence
in `doc/TLS_V1.md`.

Final technical review: the Owner keeps cleartext routing for every host and
the existing early-refusal close behavior. There is no per-host HTTPS-only
policy or redirect in TLS v1. A client that keeps sending after an early
refusal may see a reset; lingering close waits for real-client evidence.
Source review corrected an EKU mistake: a bare leaf's EKU is readable even
without its issuer. Startup now rejects a leaf with client-auth-only EKU for
both bare and bundled certificates; absent EKU and serverAuth remain valid.
A bare leaf still cannot have its signature or full chain verified without
the issuer. Review also found that webpki exempts the last bundled certificate
from intermediate CA rules when it is used as the verification anchor. Startup
now checks that this final issuer is a CA. Regressions cover both defects.

Final readiness on Windows: format check, default tests (141 library passed,
9 ignored; 30 configuration passed), strict Clippy, MSVC release build and profiling bench
link passed. The ignored PHP listener-scheme regression passed with local PHP.
The scratch PMS variable was unset, so the ignored real Laravel TLS test was
not rerun; its earlier Windows pass remains recorded in `doc/TLS_V1.md`.
Native Linux is still unverified because only the MSVC target is installed.
No new benchmark was run: the EKU check is startup-only. No TLS v1 blocker was
found after the fix. The next capability should be transactional certificate
reload, with native Linux bring-up as an explicit qualification gate; do not
infer that reload or Linux support is part of TLS v1.

Known limits kept out of v1: no ACME, reload, OCSP, mTLS, upstream TLS,
HTTP/2/3, redirect/HSTS, HTTPS-only hosts on a server that also listens in
cleartext, trust/revocation checks (the client's). Early refusals to clients
that keep sending can arrive as resets (existing, not TLS). Native Linux: NOT
VERIFIED.
Older sections titled `Current Status` and `Next Action` below are historical
records for their named slices; this top section is the active scope.

## Recorded Checkpoint — TLS architecture investigation (2026-09-24)

Investigation only, superseded by the implementation above; its candidate
comparison, library evidence and Owner review points remain in
`doc/research/TLS_ARCHITECTURE_INVESTIGATION.md`.

## Recorded Checkpoint — Reverse Proxy v1 (0.7 candidate)

Implemented in the uncommitted working tree for owner review on top of
`a7a31b4`. Not committed, tagged, released, or accepted; the owner decides
architecture approval and closure. The current Reverse Proxy diff uses
`a7a31b4` as its baseline.

FACT: a `proxy` location compiles to its own `Arc<Upstream>` (`src/proxy.rs`)
holding the startup-resolved address, timeouts, an idle `VecDeque` behind a
std mutex with single push/pop critical sections, and counters. Connections are
Hyper `client::conn::http1` (the `client` feature was enabled; it adds the
transitive crates `want` and `try-lock`). Request and response bodies stream as
Hyper `Incoming` with no collection and no channel of Hamster's. A `Lease` owns
each exchange and moves into `ResponseBody::Proxy`; a connection is pooled only
after a clean response end and a fully sent request, and every other ending
aborts its driver. The only retry is `TrySendError::take_message() == Some`,
once, on a fresh connection. Forwarding metadata is replaced from
`TransportInfo`. `ProgressDeadline` was extracted from `WriteProgressTimeout`
so both proxy body directions use the exact existing semantics.

FACT: 41 permanent proxy unit tests over raw sockets (`src/proxy/tests.rs`)
plus 7 configuration tests pass. The earlier report says the 37 that existed
before its final review were stable over 10 consecutive runs (an earlier count
of 38 was corrected to 37), and the next 38 over 5 consecutive runs. Three more focused
regressions were added in this readiness review. They include the
B' regression (upstream stops reading + client disconnects: reclaimed 814 ms
after disconnect at a 1 s progress timeout), early 413 during upload (280 us),
deterministic stale keep-alive retry through a `#[cfg(test)]` read-EOF fault,
and no-duplicate execution checks. The earlier investigation reports 29
deliberate mutations of the invariants, each detected by at least one test
(M29 and its test, response-side
"both lengths, never reused", were added in the final review).

FACT: Windows reports a refused loopback connect only after ~2.04 s of SYN
retries; with `connect_timeout` below that, a refused loopback upstream is 504.

FACT (release, loopback, `benches/proxy.rs`): C1 23 B costs Hamster 58.6 us CPU
and +44 us median; 64 MiB C1 runs at 94% of direct with 1.84 MiB private
commit; a stalled exchange costs 185-252 KiB at the 64 KiB buffer bound,
linear in count, independent of payload, and scaling with the bound (about
120-140 KiB at 32 KiB, 1.08-1.56 MiB at 417,792). `http1_max_buffer_size` of
128 KiB or more collapses proxied large transfers at C1 on Windows loopback
(largely absent at C32), so 64 KiB is kept for upstreams too, as a measured
conservative Windows default, not an invariant. Static A/B against `a7a31b4`:
no regression detected at ~8% resolution.

FACT (investigation, no production change): the collapse and the ~15 ms tail
on 1 MiB C1 (about 12% of proxied requests at or above 5 ms, 1.6-6.3% direct)
are one Windows loopback TCP send stall. It reproduces with a Hyper-only
proxy, a raw Tokio relay and two plain std threads
(`benches/windows_tcp_stall.rs`); during a stall the receiver has advertised
an open window, the sender has data and gets `WSAEWOULDBLOCK`, and ETW shows
sending resume when a TCP timer fires 10-17 ms later. Not a Hamster, Hyper or
Tokio defect. An upstream `SO_RCVBUF` of 1 MiB removed most of it in a
reverted experiment, 4 MiB halved 64 MiB throughput; not productionized.
Exact kernel rule, real NIC and Linux: not verified.

Readiness review found and fixed two production defects: transfer codings
before `chunked` were stripped without decoding their payload (400 for such
requests and 502 for such upstream responses now), and explicit invalid
upstream ports were silently treated as port 80 (now rejected at validation).
The raw-socket target probe confirms `CONNECT` authority-form and `OPTIONS *`
return 400 before an upstream connection; documentation was corrected.

Readiness review gates: fmt, strict Windows clippy, default suite (128 lib,
24 configuration, and remaining integration tests), MSVC release build and
bench link passed. The ignored suite had 9 lib and 9 multipool passes;
`php_worker` had 56 passes and 6 environmental failures that require
`HAMSTER_LARAVEL_TEST_PATH`. Visibility (1), Windows containment (4), and
timeout probes (10) passed. The Linux cross-target
check cannot run: only `x86_64-pc-windows-msvc` is installed.

Known limitations, deliberately out of v1: TLS upstreams, multiple endpoints,
load balancing, health checks, WebSocket/Upgrade/CONNECT, HTTP/2, caching,
trusted-proxy chains, DNS re-resolution, path rewriting, 1xx relay, reload.
Close-delimited upstream responses cannot be told apart from truncation. Native
Linux: NOT VERIFIED (and the Linux cross-target check could not run). Authoritative record: `doc/REVERSE_PROXY_V1.md`;
measurements: `benchmarks/results/reverse_proxy_v1_20260924.md`.

## Previous Scope — Windows Abnormal-Parent Child Containment (committed at `4b9c564`)

Candidate A from the completed investigation is implemented in the uncommitted
working tree. On Windows, `server::run` establishes an unnamed Job Object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before `RuntimeConfig::start_workers`; the
latter repeats the idempotent guard before its first pool so its direct public
path also fails closed. The final `PhpWorker` spawn primitive repeats the same
guard so lower-level direct callers cannot bypass containment; replacement
lifecycle policy itself remains unchanged. The job is fully configured before
Hamster assigns itself. A static `OnceLock` owns the sole Hamster-created handle
for process lifetime, so ordinary PHP children, replacement generations, and
direct descendants inherit containment without per-generation assignment.

The production path has no post-spawn assignment, suspended-assignment, or
uncontained fallback. Normal shutdown still uses transaction drain,
`SHUTDOWN`/`SHUTDOWN_ACK`, kill fallback, and reap; the Job Object is only the
abnormal-parent safety net. Permanent Windows tests retain exact process
handles and creation timestamps for busy PHP plus a PHP-created descendant,
bootstrap-before-READY, replacement plus multi-pool, and a compatible outer
Job Object. Setup-failure injection exists only under `cfg(test)` and proves
failure precedes worker spawn and listener bind.

The boundary covers ordinary inherited process trees, not WMI/brokered process
creation. Breakaway is not enabled. Incompatible outer-job policy fails startup.
No Linux containment exists or has been natively verified. Continue with the
quality-gate and independent diff-review evidence from the implementing turn;
do not infer commit, push, tag, merge, or release from this section.

FACT: the four-test abnormal-parent suite passed with exact process handles and
creation timestamps. It reclaimed bootstrap PHP, busy PHP, a PHP-created direct
descendant, a replacement generation, both independent pools, and a worker
under a compatible outer job; observed waits were 0-5 ms against a 5-second
bound. Default tests, the nine multi-pool tests, targeted cancellation,
at-most-once, execution-timeout and graceful-shutdown regressions, strict
clippy, release build, bench linking, PHP fixture lint, and Linux cross-target
compile-check passed. One unrelated body-budget socket test first hit Windows
error 10053, then passed three isolated reruns and the repeated full suite.

## Closed Scope — Multiple Persistent Application Pools

The owner closed the implementation scope on 2026-09-18. Multiple
`php_worker` locations are implemented in the working tree; they are not yet
committed, tagged or released. The former validation guard was removed only
after orchestration gained transactional ownership semantics.

FACT: every location compiles to a distinct `Arc<PhpWorkerPool>` and keeps its
own processes, queue, supervisor, spawn gate, slot/generation state, health and
recovery counters. Request dispatch, recovery and worker lifecycle paths are
unchanged. The one intentional cross-pool resource is the process-wide
`BodyBudget`, because it bounds Rust process memory rather than an application.

FACT: startup policy is all-or-nothing. `RuntimeConfig::start_workers` starts pools
in configuration order and, on failure, shuts already-started pools down in
reverse order before returning. `server::run` also converges every pool on its
startup-error path and never binds the listener first. There is deliberately no
degraded startup or startup retry.

FACT: `shutdown_workers` attempts every pool before returning an aggregate error.
`PhpPoolsError` keeps the initiating pool failure as its error source and lists
cleanup failures as additional, pool-labelled evidence. The same causal rule
now covers partial startup inside a pool when one worker reaches READY and
another fails.

FACT: nine permanent ignored regressions in `tests/multipool_probe.rs` cover healthy
multi-pool routing, cross-pool crash isolation, partial-startup rollback,
startup failure before bind, best-effort shutdown after an earlier pool fails,
multiple simultaneous shutdown failures, bind failure plus cleanup failure,
cross-pool body-budget ownership, PID reaping, and primary-versus-cleanup
diagnostics. The test-only
`record_pid_worker.php` records exact child PIDs, avoiding repository-wide
process counts that can be contaminated by parallel tests.

FACT: the final falsification pass found no new production correctness defect.
The nine-test multi-pool suite, 92-test default suite, formatting, strict
clippy and Windows MSVC release build passed; no repository PHP process
remained. Linux was not natively verified. PMS multi-pool verification was not
run because the prepared external environment was unavailable.

Known limitations, not closure blockers without new evidence:

- FACT: pool startup and shutdown are sequential; shutdown worst-case can
  scale with pool count.
- FACT: 64 workers is a per-pool limit; there is no process-wide worker or
  pool ceiling.
- FACT: direct cancellation/abort of the startup future and concurrent
  lifecycle calls are not a qualified public contract.
- FACT: PMS multi-pool was not verified because the prepared external
  environment was unavailable.
- FACT: Linux was not natively verified; Windows evidence cannot be promoted
  to a Linux support claim.
- HYPOTHESIS: concurrent shutdown or a global ceiling may become useful at a
  deployment scale not represented by current evidence. Do not implement them
  pre-emptively.

INFERENCE: the verified ownership and failure paths meet this slice's contract;
the limitations above do not invalidate it. See the authoritative record in
`doc/MULTIPLE_PERSISTENT_APPLICATION_POOLS.md`.

## Previous Milestone — Pool Identity + Degradation Visibility

Pool Identity + Degradation Visibility is implemented in the working tree for
owner review, on top of the closed Execution Timeout Policy slice. It is not
committed, tagged or released.

The investigation challenged its own premise and the premise was half wrong.
At that milestone only one `php_worker` location was permitted, so pool identity
had no immediate disambiguation value and the label was added in preparation
for a later multi-pool capability. That statement is historical: the current
contract permits multiple independently owned pools. The real defect
was the opposite of the stated one. Degradation was already *over*-reported at
three stderr lines per event, while recovery was reported not at all: a
measured run ended on four consecutive failure lines while the pool was serving
`200 200 200 200 200`. Worse, the one aggregate `PoolSnapshot` exposed,
`ready_workers`, is not a health signal -- `mark_busy` decrements it, and it
read zero in 20% of samples on a four-worker pool that served 23,072 requests
without a single failure. The counter that answers the question, `live_workers`,
was not exposed at all despite being what `execute` gates its 503 on.

The slice therefore exposes `live_workers`, documents `ready_workers` as spare
capacity, names every pool-scoped diagnostic (including drained application
stderr) after its pool, and reports an outage and its recovery as a matched
pair once the pool has had no live worker for longer than
`POOL_OUTAGE_NOTICE_AFTER` (1 s, about 2.5x the worst measured bootstrap of
403 ms). Ordinary replacement churn produces no outage lines. A first
implementation that reported every drop to zero produced seven alarms against
two all-clears, which was the same asymmetry in a new form and was fixed.
Evidence: `benchmarks/results/pool_visibility_20260917.md`.

Known limitations carried out of this slice:

- FACT: `live_workers` is only as fresh as the last dispatch. An idle worker's
  death is detected passively on its next assignment, so an idle pool reports
  workers that are already gone and the outage notice cannot fire for one.
- FACT: partial degradation is not reported. A pool serving on two of four
  workers still answers every request; the measured user-visible failure was
  total refusal. A partial-capacity threshold would need its own evidence.
- FACT: a per-request failure still emits its own line, so a failing generation
  produces one line per queued request on top of the two slot lines. Reducing
  that duplication was left alone as outside the scope.
- HYPOTHESIS: the 1 s threshold is derived from bootstrap times measured on
  this host with this fixture and the recorded Laravel figures. A materially
  slower application bootstrap would make ordinary replacements visible as
  outages.

Execution Timeout Policy is **closed by owner decision**. It is implemented in
the working tree on top of the uncommitted 0.6.x response work and is not
committed, tagged or released. Do not extend or reopen this slice unless new
evidence requires it. An application execution
timeout is now its own failure class: it still kills and reaps the worker and
its request is still never retried, but it no longer increments the slot's
infrastructure failure history, so repeated slow requests cannot walk a pool
into the replacement backoff ladder or its 30-to-300 second cooldown band.
`request_timeout` is configurable per `php_worker` location, 1 to 3600 seconds,
defaulting to the historical 5. Fault injection measured the failure this
fixes: eleven paced slow requests put a one-worker pool into the cooldown band
and refused ordinary traffic for 30 151 ms, while 52 of 52 timeout-driven
replacements reached READY, so the ladder never prevented anything. Mixed
traffic with headroom never degraded (629 of 629 ordinary requests served). The
HTTP status is `502` with the body `PHP Worker Execution Timeout`, by owner
decision: the request is possibly-started, so Hamster must not emit a status
that encourages an external retry it cannot make safe. Evidence:
`benchmarks/results/worker_request_timeout_investigation_20260917.md` and
`benchmarks/results/worker_request_timeout_floor_20260917.md`. The policy, what
deliberately did not change, and the rejected alternatives are in
`doc/EXECUTION_TIMEOUT_POLICY.md`.

Known limitations carried out of the slice, and the hypotheses it did not
test:

- HYPOTHESIS: the same escalation still applies to every failure that happens
  after the REQUEST frame is written -- I/O and protocol faults -- because they
  share the `GenerationOutcome::Failed` path. The slice deliberately left their
  recovery policy unchanged, and no probe drove them at rate.
- HYPOTHESIS: a real Laravel bootstrap is 58-403 ms against the fixture's
  ~60 ms, so the sustained-churn duty cycle in production is higher than the
  measured G1 worst case. The arithmetic is in the floor derivation; it was
  never measured against a real application because that needs
  `HAMSTER_LARAVEL_TEST_PATH`.
- FACT: five seconds is a compatibility default and nothing more. Nothing in
  this repository establishes it as correct for any application.
- FACT: `tests/timeout_probe.rs` is historical instrumentation, not a
  regression suite. All of it is `#[ignore]`d and several probes take minutes.
  The four permanent regressions live in `tests/php_worker.rs`.

0.6.2 — Response Liveness is implemented in the working tree for owner review,
on top of the uncommitted 0.6.1 streaming response. A lazy per-connection timer
now covers only a backpressured socket write with zero byte progress; it does
not time request reads, keep-alive idle, application execution or total response
duration. The two-second release probe returned the sole admission slot in
2008.3 ms, while a progressing 36 KiB/s client survived three timeout
intervals. See `doc/RESPONSE_LIVENESS_0_6_2.md` and
`benchmarks/results/response_liveness_0_6_2_20260916.md`.

0.4.0 — Laravel Persistent Worker. The investigation and Octane adapter are
committed on `dev` at `6e961d5`. They are not tagged or released, and milestone
closure remains an owner decision. Findings, measurements and rejected
alternatives are in `doc/LARAVEL_WORKER_0_4.md`.

0.3.1 is closed: `v0.3.0` and `v0.3.1` are tagged on `dev` and the retirement
hardening is committed as `616e4ee`. The accepted lifecycle contract remains
`doc/WORKER_LIFECYCLE_0_3.md` and 0.4 reuses it unchanged. Milestone 0.2.0 is
closed; its transaction-safety invariants remain mandatory.

## Current Status

Identity, lifecycle supervisor, automatic replacement, per-slot backoff,
optional recycling, shutdown coordination, telemetry, config validation, and
the accepted 18-test lifecycle matrix are implemented. The three old tests
that encoded permanent no-respawn behavior were explicitly reclassified as
0.3-superseded lifecycle tests and updated. Independent review closed with
ACCEPT and no release blocker; its non-blocking follow-ups are in the backlog
below.

## Verified Platform

Windows `x86_64-pc-windows-msvc` with local PHP 8.5 NTS at
`php/8.5/php.exe`. Do not claim native Linux verification. A Windows-host
cross-target check is COMPILE-CHECKED ONLY.

## Architecture Summary

The fixed-size pool has stable slots. Each replacement increments that slot's
generation; PID is process metadata only. A generation task exclusively owns
one `PhpWorker`, process, pipes, protocol state, and capacity-1 mailbox.
`IdleWorker { slot, generation, mailbox }` is the dispatcher capability, so a
stale token naturally returns `Closed` and is the sole safe redispatch case.

The lifecycle supervisor owns the generation `JoinSet`, replacement/backoff
work, dispatcher coordination, and shutdown. `idle_tx` belongs only to a live
generation or pending replacement; the supervisor has no permanent keeper.
Unexpected failure schedules an immediate first replacement, then 100, 200,
400, 800, 1600, 3200, and capped 5000 ms delays. More than 10 consecutive
failures moves the slot into a bounded cooldown band (30 s doubling to a 300 s
ceiling); no slot is ever permanently retired. Stability reset requires 15
seconds READY, measured from READY rather than from spawn, plus a successful
request. Stability is deliberately time based: request-count stability would
make the respawn rate proportional to throughput.

## Critical Invariants

- Never retry once PHP may have started REQUEST.
- Client cancellation never cancels a worker transaction.
- Consume RESPONSE or handle worker failure deterministically.
- One task owns one generation; no worker/global execution mutex.
- Protocol v1 and request/frame/header limits remain unchanged.
- Admission is bounded and non-blocking.
- Zero READY is transient: a slot always has a live or pending generation, so
  `idle_rx` no longer closes through exhausted retries. Admission still refuses
  immediately on `live_workers == 0` rather than blocking on distant capacity.
- Once shutdown enters ShuttingDown, the spawn gate permanently forbids new
  PHP processes.
- Dispatcher converges before workers receive shutdown; every child is reaped.

## Known Limitations

Idle worker death is detected passively on its next assignment, but a request
that finds a dead worker is now queued again rather than failed, because a
process reaped before the frame write proves PHP never received it. There is no
proactive stdout probe, adaptive sizing or memory-threshold recycle.
Byte-based admission now exists: the
`server.max_body_memory` budget bounds request-body bytes buffered process-wide
and refuses over-budget requests with 503 before pool admission, with no
waiting. It defaults to 64 MiB, reports its configured value at startup and its
refusals on standard error at most once per minute, and `0` is an explicit
unlimited opt-out. Static response bodies now stream in bounded 128 KiB frames;
32 slow clients serving 256 MiB files used 9.39 MiB additional private memory.
PHP responses remain buffered protocol-v2 frames; their response-frame layout
is unchanged. `header_read_timeout` still
does not cover response writes; `write_progress_timeout` now releases an active
response after its socket write makes zero progress for a configured interval.
This is deliberately not a total-duration or minimum-rate policy, so a peer
that makes deliberate trickle progress can still hold a slot indefinitely.
Graceful shutdown retains its separate finite drain deadline. Charging a
declared `Content-Length` before reading also lets a slow
client hold budget it never fills; four such clients made every other POST 503
for `body_read_timeout` while GET kept serving. Both are documented in
`doc/CONFIGURATION.md` and `doc/HTTP_TRANSPORT_0_5.md` and measured in
`benchmarks/results/review_0_5_20260916.md`. Permanent slot
retirement no longer exists: prolonged
failure settles into a 300-second cooldown ceiling instead. HTTP bodies
still buffer before pool admission. `max_requests` and `max_worker_age` default
OFF; recycling is stop-then-spawn and permits one voluntary recycle at a time.
Both recycle policies are thresholds rather than caps; see the recycling
limitation in `doc/PHP_WORKER.md`. Laravel request ownership is implemented in
0.4.0 with Octane's sandbox/lifecycle plus a narrow Hamster resource boundary.
`multipart/form-data` with file uploads remains bounded by the protocol's 1 MiB
body limit.

## Current Findings

Windows quality gates: `fmt --check`, clippy with `-D warnings`, the default
suite (34 unit, 16 configuration, 4 routing, 5 static), a release build and
`cargo bench --no-run --features profiling` all pass. The final Octane
regression pass ran the complete ignored suite in its default parallel mode:
eight PHP pool unit tests and 47 process/HTTP integration tests passed. It
includes the accepted worker lifecycle and 0.2 cancellation/shutdown
regressions, the 100,000-request pool-level soak, a 10,000-request
forced-failure HTTP soak, the 60-second W4 continuous-load random-kill stress,
and all Laravel/PMS paths. A new real-PMS W4 regression forces all workers to
serve fresh controllers, injects one cleanup failure, and proves only that slot
advances generation before W4 recovers. No project PHP process remains after
the suite.

The first complete run exposed a deterministic harness regression: two
external Laravel tests omitted the scratch application's `PHPRC`, so repository
PHP lacked `mbstring`. The tests now derive the ini path themselves; both pass
with the parent shell's `PHPRC` removed and the complete matrix passes. This
was configuration dependence in the new tests, not a product race.

Protocol v1 is unchanged on the wire. `src/php_protocol.rs` differs from
`v0.2.0` only inside the old `write_frame` body, which now delegates to
`write_frame_tracked`; the constants, encoders and decoders are untouched and
`fixtures/php-worker/protocol.php` is byte-identical.

The Linux cross-target check could not be run on this machine: the
`x86_64-unknown-linux-gnu` target is not installed in this rustup toolchain.
Linux remains unverified, as before.

`ctrl_c_stops_server_and_reaps_pool` needs a real conhost console. Its
`GetConsoleCP` guard does not detect a ConPTY or pseudo-console session, so
under those terminals the test hard-fails at the five-second wait instead of
skipping. The same failure reproduces on the `v0.2.0` tree, so it is a test
harness gap, not a lifecycle regression. Ctrl+C was verified on a real Windows
console only.

The limited steady-state benchmark detected no greater-than-10% regression in
W1/C1, W4/C16, or W4/C32 for Hello or JSON. Several cells moved positively by
more than 30%, so host variance dominates and the data can only fail to detect
a regression, not prove there is none. Exact data and high-variance notes are
appended to `doc/BENCHMARKS.md`. Shutdown during replacement bootstrap
interrupts startup and kill/reaps the child rather than waiting for the
five-second READY timeout.

## Backlog

Non-blocking follow-ups from the independent 0.3.0 review, highest value first.
The first item is done on `dev` after the `v0.3.0` tag.

- DONE: the dispatcher drains idle capabilities continuously and discards
  closed mailboxes, so `max_worker_age` advances on a pool with no traffic.
  Regression tests are `max_worker_age_recycles_a_pool_with_no_traffic` and
  `dispatcher_drains_idle_capabilities_without_a_pending_job`; both fail
  against the `v0.3.0` tree.
- DONE: end-to-end coverage that the supervisor actually applies the delay
  ladder. `shutdown_during_prolonged_failure_cooldown`,
  `prolonged_failure_never_retires_a_slot_permanently` and
  `one_bad_slot_does_not_disturb_healthy_slots` drive a real slot past
  `MAX_CONSECUTIVE_FAILURES` and assert the resulting state.
- DONE, and the diagnosis was wrong: `ctrl_c_stops_server_and_reaps_pool` did
  not need a ConPTY guard. The server inherited an "ignore Ctrl+C" attribute
  from the launching shell through Cargo. The test now clears that attribute
  before spawning; see the current scope section.
- DONE: "the process exited before REQUEST was written" is distinguished from a
  possibly-started failure. `PhpWorkerError::NotStarted` is returned when
  `try_wait` reaps the process first or when the frame write transfers zero
  bytes, and such a job is queued again once. `write_frame_tracked` reports the
  transferred count, which `write_all` cannot.
  `respawn_after_idle_crash_restores_target` now asserts a 200 and exactly one
  execution.
- DONE: `all_workers_crash_then_pool_recovers` is deterministic. It no longer
  asserts a particular status, which was never knowable, and instead asserts
  that every request is answered within ten seconds with 200, 502 or 503 and
  that the number of executions equals the number of requests served, with no
  duplicates. Five consecutive runs pass.
- Smaller debt: `WorkerState` and `WorkerLifecycleState` duplicate each other;
  the pre-request exit path drops the worker without awaiting its stderr
  drain; `total_respawns` also counts planned recycles; the dispatcher's
  full-mailbox branch lost the 0.2 `debug_assert`; `SpawnGate` holds a std
  mutex across `Command::spawn`.
- DONE: `IdleWorker` no longer derives `Clone`. One capability per generation
  is what bounds the capacity-1 mailbox to a single buffered job, and that
  bound was held only by the shape of `run_worker_generation`. Cloning is now
  a type error rather than a silent way to break it.

## 0.3.1 Retirement Hardening (committed at `616e4ee`)

`STABILITY_RESET_PERIOD` is 15 s (was 60 s), measured from READY.
`WorkerLifecycleState::Retired` and `retire_slot` are gone; `replacement_delay`
returns a `Duration` rather than `Option`, moving into `COOLDOWN_BASE` 30 s ..
`COOLDOWN_MAX` 300 s past `MAX_CONSECUTIVE_FAILURES`.

Verified on Windows with real php.exe: permanent bootstrap failure settles at
the 300 s ceiling using 0.08 s CPU over 423 s with stable RSS; a W4 pool driven
into the cooldown band on all four slots returned to W4 by itself 46 s after
the fault stopped; a generation living 20.1 s resets failure history while
generations killed faster still accumulate; one slot driven into cooldown left
the other three at zero failures and serving; shutdown during a cooldown
returns in under two seconds with no child left behind.

At-most-once semantics are untouched: no transaction-ownership code changed and
`possibly_started_request_is_never_retried` still passes. Protocol v1 is
byte-identical to `v0.2.0`.

Known test flakiness, not a regression: `ctrl_c_stops_server_and_reaps_pool`
fails under ConPTY and also fails that way on the unmodified tree.
`no_orphan_php_processes` failed once under a ConPTY run and passed four out of
four under a real console; it compares raw PIDs, and the new tests recycle PIDs
much faster. Repository php.exe count was zero after every run, so no orphan
was ever present.

## 0.4.0 Laravel Persistent Worker (implemented at `6e961d5`)

The independent review rejected the in-place targeted reset. Its
`warmInstanceKeys` snapshot made binding lifetime depend on warm-up resolution,
retained a bootstrap-resolved scoped binding, and could not see a controller
cached on a shared `Route`. More reflection/list entries would not establish a
real ownership boundary.

The PHP worker now requires Laravel Octane 2.x and uses its application factory,
per-request sandbox, manager rewiring and lifecycle listeners. Hamster supplies
only the protocol-v1 client and a narrow resource boundary: abandoned DB
transactions, Eloquent guard/strict statics, Carbon test time, cwd/default
timezone and worker-created upload temporaries. The old `none` and `targeted`
modes fail before READY.

Responses are staged in PHP until kernel termination, Octane cleanup and the
Hamster resource boundary all succeed. A terminating-listener fault injection
initially exposed that sending a synthetic 500 made Rust briefly mark an exited
generation READY. The final worker emits no frame on lifecycle failure: Rust
receives EOF for a possibly-started request, returns 502 without replay,
immediately marks the generation unavailable, boots generation 2 and returns
200 there. The handler marker remains exactly one.

Checked-in probes now cover bootstrap-resolved scoped state, scoped separation,
lazy versus warm singleton ownership, cloned config, cached controllers,
deferred providers, listener ordering, framework/runtime baselines, transaction
and upload cleanup failure, response commit ordering, and stdout descriptor
isolation. The real lifecycle test deliberately needs an Octane-enabled Laravel
app and W1. A PMS-specific checked-in test drives 50 authenticated/anonymous
pairs through the Rust server on one PID, checks cached-controller ownership,
then fault-injects termination and requires exact-once recovery.

The actual Octane 2.19.1 package was installed in a no-hardlink scratch PMS,
with package discovery and `artisan optimize`; no source injection was used.
The ownership suite passed 25 checks. A release same-HTTP/IPC fresh-bootstrap
comparison measured 1.51x on controller JSON, 1.49x on Blade login and 1.08x
on the remote-MySQL dashboard. A W1 soak passed 20,000 alternating auth/guest
requests on one PID and measured 0 B/request RSS slope over the second half.
Raw results and reproduction scripts are under `benchmarks/`.

The original PMS remains untouched and does not declare Octane. Its pinned
PhpSpreadsheet 1.30.6 declares PHP `<8.5`, so the scratch dependency install
needed an explicit platform-ignore. At 0.4 closure, protocol v1 still lacked
typed peer metadata and left `REMOTE_ADDR=127.0.0.1`; that historical decision
was subsequently resolved by strict typed protocol v2. See the current handoff
section below and
`doc/research/LARAVEL_LIFECYCLE_0_4_REVIEW.md`.

## Accepted 0.5-1.0 Production Spine direction

The old post-0.4 order is rejected: adaptive worker management and Morph no
longer precede basic compatibility and production operation. Hamster Wheel is a
general-purpose Web/Application Server; Laravel Octane remains confined to the
Laravel persistent-worker adapter.

The dependency order is:

1. 0.5 HTTP/transport identity, bounded request/response policy, streaming
   static consumer, minimum instrumentation and native Linux bring-up;
2. 0.6 FastCGI on Windows/Linux, multiple PHP backends and the explicit generic
   persistent-adapter contract;
3. 0.7 minimal reliable reverse proxy with streaming, backpressure, forwarding
   trust and an at-most-once retry boundary;
4. 0.8 transactional reload/drain, logs/metrics/health, Windows Service, systemd
   and operational CLI;
5. 0.9 TLS/SNI/certificate reload, optional HTTP/2 product gate and full release
   qualification;
6. 1.0 stable Web/Application Server contract.

No generic backend trait/plugin system is authorized in 0.5. FastCGI and proxy
must first provide real additional backend shapes. Adaptive sizing and Morph
are post-1.0 research unless production evidence changes their dependency.
`doc/ROADMAP_0_5_TO_1_0.md` contains milestone exits, alternatives, hypotheses
and owner decisions.

The first 0.5.x working-tree slice now preserves the immediate peer returned by
`accept()`, queries the accepted stream's concrete local endpoint, and passes
immutable `TransportInfo` through routing to every handler. It deliberately
does not copy request-owned HTTP version/authority into a second context, trust
forwarding headers, or alter protocol v1. A real-socket unit test covers a
wildcard listener, keep-alive, forged proxy headers and 32 concurrent clients.
See `doc/HTTP_TRANSPORT_0_5.md`. The change awaits owner review and has not been
committed or released.

## Next Action

Owner review of the Execution Timeout Policy slice, then of 0.6.1 Concrete
Streaming Response. The measured response-memory
failure is closed and no static or PHP performance regression was detected.
The pass also falsified the 0.5 inference that `header_read_timeout` reclaims an
active response write: a streaming slow reader remains active past it. Graceful
shutdown has its own bounded drain using that configured interval, but the
normal-operation response-write timeout policy remains a documented gap and is
not silently folded into 0.6.1.

Nothing in 0.6.1 is committed, tagged, pushed, merged or released. Do not start
the next 0.6.x slice until the owner decides closure.
Remaining 0.4 decisions and out-of-scope qualifications:

- Decide how the external PMS adopts Octane and resolves its PhpSpreadsheet
  PHP 8.5 constraint. The disposable scratch proves lifecycle compatibility,
  not that ignoring Composer platform requirements is a production policy.
- Decide whether correct client IP reconstruction is a 0.4 blocker, and if a
  reserved v1 metadata header or typed protocol-v2 field is the accepted
  contract. Do not infer it from an untrusted forwarded header.

- Raise the 1 MiB request body limit, or stream bodies, so uploads larger than
  that work. This remains a protocol change and needs a milestone that owns the
  current protocol v2 contract.
- Exercise the supplied application's own import endpoints. Not done here
  because they write to the owner's database.
- Native Linux verification, still deferred.
- Strengthen the `ctrl_c_stops_server_and_reaps_pool` console guard so a ConPTY
  session skips instead of failing. It is now the only test that fails under
  ConPTY.

Roadmap decisions, due at their relevant design gates:

- DONE: strict typed protocol v2 owns transport metadata; protocol v1 workers
  fail startup before listener bind and there is no fallback;
- choose managed versus externally managed Windows FastCGI before 0.6 is
  finalized;
- choose edge-capable versus origin-only and the HTTP/2 gate before 0.9;
- identify 1.0-stable config/protocol surfaces before the release candidate.

## Current working-tree handoff: Application Request Metadata (2026-09-18)

This section supersedes the older `Next Action` and protocol-v1 decision notes
above. Those passages are retained as historical context, not current work.

FACT: the owner selected typed protocol v2. The PHP handler now passes the
accepted connection's immutable `TransportInfo` plus the actual HTTP version
into each independently owned `WorkerRequest`. V2 encodes HTTP/1.0 versus 1.1,
peer IP/port and local IP/port before the existing request fields. IPv4 and
IPv6 use family-tagged fixed-width bytes. READY compatibility is strict v2;
there is no v1 fallback or optional tail.

FACT: Laravel's Symfony request server bag now receives `REMOTE_ADDR/PORT`,
`SERVER_ADDR/PORT`, actual `SERVER_PROTOCOL`, cleartext
`REQUEST_SCHEME=http`/`HTTPS=off`, effective `HTTP_HOST`, and `SERVER_NAME`
derived from that same effective authority hostname. No PHP `$_SERVER` globals
are hydrated. Forwarded headers remain ordinary untrusted client input.

FACT: checked-in regressions cover PHP decoder corruption, old-worker startup
mismatch before listener bind, `.2`/`.3` real loopback peers and ports,
HTTP/1.0, absolute-form authority conflict, keep-alive request separation,
W1 reuse and independent multi-pool delivery. The checked-in scratch PMS probe
configures Symfony to trust only `127.0.0.1`; an actual `.2` peer with forged
forwarded client/host/scheme/port values remains `.2`/`localhost`/`http`, and a
following `.3` request replaces all request-owned metadata on the same W1 PID.

FACT: Windows verification passed the 100,000-request protocol soak, the full
nine-test multi-pool fault matrix, PHP decoder/request reconstruction checks,
PMS W1/W4 lifecycle and cleanup recovery, cancellation stress, crash recovery,
at-most-once execution timeout and shutdown drain. The temporary prepared PMS
copy was updated for the probe; the original PMS was not modified.

INFERENCE: a four-pair alternating release comparison demonstrates no material
trivial-worker regression, but its spread cannot exclude a low-single-digit
cost. Paired RPS and CPU signs split two/two. Do not claim v2 is zero-cost.
Raw samples and exact contract are in
`doc/APPLICATION_REQUEST_METADATA.md`.

Known limits are not blockers without new evidence: PHP globals and duplicate
header semantics remain separate adapter compatibility work; TLS/trusted proxy
implementation was not opened; IPv6 was binary-decoder tested but not exercised
end to end; IPv6 scope IDs are not represented; Linux remains not natively
verified. The working tree is not committed, tagged, pushed, merged or released.
Owner decides closure and repository actions.
