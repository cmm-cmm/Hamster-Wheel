# AI Handoff

## Current: lifecycle records and log rotation (2026-10-18)

The reload slice below was committed and pushed as `60f4822`. This is
uncommitted work on top of it, and it finishes the 0.8 logging scope:
`doc/ACCESS_LOG_0_8.md` now covers all three record kinds and rotation.

FACT: the log holds `lifecycle` records beside `access` ones, in one file, in
the order things happened. Ten events, as a fixed list in `src/access_log.rs`
rather than free strings at the call sites: `started`, `listener_bound`,
`worker_ready`, `reload_published`, `reload_refused`,
`certificates_reloaded`, `certificate_reload_refused`, `draining` (with
`trigger` = console or control), `listeners_drained`, `stopped`. Each is
written beside the human `outln!`/`errln!` line it accompanies, so the console
and the log cannot disagree, and each goes to the same bounded queue as an
access record, so neither blocks and both are dropped and counted the same
way. Values are typed at the call site, so the writer escapes text in the one
place escaping is tested; a reason built from an error string cannot forge a
record.

FACT: the sink is reached from those places through a process-wide **`Weak`**
handle installed once before any pool starts. It is `Weak` because of a
defect this slice produced and the new test caught: with an owning `Arc` in
the `OnceLock`, the queue never closed when the server dropped its own sink at
shutdown, so `writer.finish()` waited for a sender nothing would close and the
process **never exited**. The detached-server test failed on
`wait_for_exit`; the handle is now a view and the sink's lifetime stays with
`server::run`. The request path still keeps its own `Arc`, so nothing global
is read per request.

FACT: the same test exposed an ordering bug. The log was flushed before the
final `stopped` record was produced, so the last thing the server did could
never reach the file. The flush now happens at the very end of `run`, after
the outcome has been reported.

FACT: rotation is in the writer task, so nothing on the request path waits for
it. `[server.access_log]` gained `rotate_bytes` (0 or absent = never; floor
4 KiB; requires `path`, because stdout belongs to whoever owns it) and `keep`
(0-64, default 4, and an error without `rotate_bytes` rather than a silently
ignored key). The bound is **exact at record granularity**: a batch is split
at a record boundary, so a record is never divided across two files and the
active file does not pass `rotate_bytes`; a single record larger than the
whole bound goes through whole into an empty file rather than being dropped.
Appending to an existing file counts what is already there. A rotation that
fails is reported once and the server keeps writing to the open file, because
a log that stops writing is worse than one that grew past its bound.
Rotation is reported at startup and in `--control status` as `rotation=`.

FACT: an integration test drove 120 sequential requests against a 4 KiB bound
with `keep = 2`, then read the retained window oldest archive first. The point
is not how many records survive — three files of 4 KiB cannot hold 120, and
discarding the oldest archive is what was asked for — but that the ids form
**one contiguous run ending at the last request**, which is what proves
nothing was lost or written twice across a rotation. An earlier version of
that test asserted a retention level instead and failed for the wrong reason;
do not reintroduce it.

FACT (`tests/windows_control.rs`, ignored, detached server): one file contains
`started` with the real pid and `generation=1`, `listener_bound` with the
address, an `access` record for a served request, `reload_published`,
`reload_refused`, `draining` with `trigger=control`, `listeners_drained` and
`stopped` with `graceful=true`, in that order. A release probe with a 4 KiB
bound produced `access.jsonl` at 1,959 bytes beside two archives of 3,952,
152 records written and none dropped.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite 301 tests (296
at `60f4822`, plus 4 record and rotation unit tests and 1 integration test);
ignored `windows_control` 16/16, `windows_headless` 2/2,
`windows_containment` 4/4, `tls_reload_process` 3/3, `validate_cli` 1/1; the
non-Windows cfg-flip compile check; MSVC release build; `cargo bench --no-run
--features profiling` linking all 11 benches.

No benchmark: a lifecycle record is produced a handful of times per process
life, and rotation happens in the writer task. The per-request path is
unchanged by this slice, so the measurement owed is still the one recorded in
`doc/BENCHMARKS.md` for the access log itself.

Remaining in 0.8: nothing, on the reading of the scope in
`doc/ROADMAP_0_5_TO_1_0.md` — transactional reload, generation identity,
structured access/error/lifecycle logs, metrics, health and readiness, and the
validate/serve/status/reload/stop CLI all exist. What 0.8 asks for and this
project cannot do here remains Linux systemd operation, and the Windows
Service qualification is scoped as recorded in
`doc/WINDOWS_SERVICE_LIFECYCLE_GAPS.md`. 0.9 is untouched: no HTTP/2, no
packaging or upgrade/rollback documentation, no soak matrix, no security
review, and no reproducible Windows **and** Linux release benchmarks.

## Transactional configuration reload (2026-10-18, in 60f4822)

The slices below were committed and pushed as `e91fd65` (access log, health,
readiness, metrics) and `abeffa5` (`--validate`). This is uncommitted work on
top of them and closes the last large 0.8 item.
`doc/CONFIG_RELOAD_0_8.md` is the behaviour document.

FACT: `--control reload` re-reads the file the server was started from,
compiles it, and either publishes it as the next generation or refuses it and
keeps serving. A refusal costs the serving generation nothing, because the
refusal happens before anything is built. The answers are
`OK generation=N previous_generation=M` and
`ERR reload_failed ... active_generation=N`.

FACT: the boundary is wider than the roadmap's "which listener changes require
a handoff", and simpler to state: **a reload may change the route graph and
the limits read per connection; it may not create, destroy or alter anything
that owns operating-system state.** Pools, proxy upstreams, FastCGI endpoints,
the TLS listener, the control pipe, the administrative listener and the access
log's file and queue are carried into the published generation as the same
objects, each fingerprinted from the settings it was built with, and a
configuration that disagrees is refused naming the resource. So a static root,
a new or removed static location, `deny_paths`, `deny_names`, aliases and the
three per-connection timeouts reload; `server.listen`, `server.tls`,
`server.control`, `server.admin`, `server.access_log`,
`server.max_body_memory`, `server.http1_max_buffer_size` and any pool,
upstream or endpoint that is new, removed or altered are restarts.

The reason for that line is not caution for its own sake: retiring or
re-pointing those resources safely needs drain semantics for in-flight
requests that this slice does not implement, and the at-most-once guarantee for
a PHP transaction is a correctness requirement in this project. The upside is
concrete — a published reload never restarts a worker, so warm workers stay
warm, which is exactly what made a full restart expensive.

FACT: generations are per connection. `RuntimeConfig` carries a generation
number from 1; `Generations` in `src/server.rs` holds the current one behind an
`RwLock` with a reload gate, mirroring the TLS listener's existing pattern. A
connection reads the generation current **when it is accepted** and keeps it
until it ends, so a keep-alive client does not change behaviour
mid-conversation and a request always finishes under the configuration that
admitted it. The generation is reported by `--control status`
(`generation=`), by `/metrics` (`hamster_config_generation`) and in every
access-log record (`"generation"`), which is the request/generation
correlation 0.8 asked for.

FACT (`tests/reload.rs`): a connection kept open across a publish still serves
the old static root, one opened afterwards serves the new one, and the three
access-log records read generation 1, 1, 2. That is the 0.8 exit evidence
stated as a test. An invalid file and a listener change both leave the serving
generation at 1 with the server answering as before.

FACT (`src/config.rs` unit tests): generation 2 then 3 publish while the
generation that asked stays complete; each process-owned field is refused by
name; a proxy upstream is the same object (`Arc::ptr_eq`) after a reload that
changed a static root beside it, while changing, removing or adding one is
refused with the label and whether it was new, changed or retired; the
built-in configuration and any in-memory compilation have no file to re-read.

FACT (`tests/windows_control.rs`, ignored, detached server): `generation=1`,
then a published static-root change answered `generation=2
previous_generation=1` with HTTP serving the new root, then an invalid file
answered `ERR reload_failed active_generation=2`, then a listener change
refused naming `server.listen`, with the server still serving generation 2 and
then exiting 0.

FACT: two findings worth carrying forward.
1. The control client reads the endpoint name **from the configuration file**,
   so a file broken badly enough to lose `[server.control]`, or to stop
   parsing as TOML, cannot be used to reach the server at all — including to
   stop it. Found when the first version of the process-level test wrote a
   minimal invalid file and could no longer address the server. It is a
   property of the client, not of the reload, and it is now the runbook's
   strongest reason to run `--validate` before writing over a serving
   configuration.
2. A lost string continuation left a run of indentation inside the refusal
   message, visible in the release probe as "that a          reload must not
   replace". A refusal is read by a person in a terminal, so a unit test now
   asserts the message is one line with no double space.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite 296 tests (290
at `abeffa5`, plus 4 reload unit tests and 2 integration tests); ignored
`windows_control` 15/15, `windows_headless` 2/2, `windows_containment` 4/4,
`tls_reload_process` 3/3, `validate_cli` 1/1; MSVC release build. A release
probe published generation 2 end to end and showed
`hamster_config_generation` move from 1 to 2 in a scrape.

No benchmark: the per-connection change is reading one `Arc` from an `RwLock`
at accept time instead of cloning a prebuilt value, which is not a per-request
change. If the owner wants it measured, the order-balanced paired method in
`doc/BENCHMARKS.md` applies, and the machine caveats recorded there still
stand.

Remaining in 0.8: lifecycle records of their own (the access log covers
requests, not lifecycle), log rotation or reopen inside Hamster, and resource
lifecycle in a reload if that is ever wanted. Linux remains unverified.

## The validate CLI operation, third 0.8 slice (2026-10-12, in abeffa5)

The two slices below were committed and pushed as `e91fd65`
(`observability: add a structured access log, health, readiness and metrics`)
at the owner's instruction; this slice is uncommitted work on top of it. The
0.8 scope asks for "CLI operations for validate, serve, status, reload and
stop": serve exists, status and stop are the control endpoint, certificate
reload exists, and this adds validate. A general configuration reload is still
missing and is the last large 0.8 item.

FACT: `hamster-wheel --config PATH --validate` compiles the file exactly as a
start would — resolving roots, loading certificates, validating pool, proxy and
FastCGI settings — then prints `key=value` lines describing the resources it
would serve and exits 0; any error goes to stderr with exit 1. It starts
nothing: `config::load` never calls `start_workers`, so no worker is spawned,
no listener is bound, no log file is created and no Job Object is established.
That is the operation's entire value, and it is what makes it safe to run on
the machine that is currently serving.

FACT: the report's last line is
`not_checked=listener_bind,log_write,worker_startup,backend_reachability`. A
report that looked complete would invite the conclusion that a validated
configuration is a working deployment, which it is not: an address can still
be in use, a log path unwritable, a worker unable to reach READY, and an
external FastCGI or proxy endpoint is never probed. A unit test pins that line
in place.

FACT: the report lists resources rather than the route table, deliberately.
`HostRouter` answers routing questions and does not enumerate itself;
describing the routes here would be a second description to keep true. Pool,
upstream and FastCGI labels are host+path, so they already say where each
resource is mounted. Proxy upstreams print the address resolved at compile
time rather than the name in the file, which is the address a running server
would actually use.

FACT (`tests/validate_cli.rs`, through the shipped binary): a valid file
reports its listener and limits with nothing on stderr; **the configured
address is still free afterwards**, which is the operation's claim stated as a
test that binds it; an invalid handler exits 1 with the offending word on
stderr and no `configuration=ok` on stdout; a certificate that does not exist
fails validation, which proves certificates really are loaded; the built-in
configuration validates; `--validate` beside `--control` is a usage error. One
ignored test (needs `php/8.5/php.exe`) validates a four-worker pool, requires
`php_pool=localhost/ workers=4` in the report, requires no "ready after" line,
and checks the port is still free. The checked-in `hamster.toml` validates in
0.04 s through the release binary.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite 290 tests (278
at `e91fd65`, plus 5 report unit tests, 1 CLI parsing test and 6 integration
tests, with 1 more ignored); the ignored Windows suites unchanged
(`windows_control` 14/14, `windows_headless` 2/2, `windows_containment` 4/4,
`tls_reload_process` 3/3); the non-Windows cfg-flip compile check; MSVC release
build; `cargo bench --no-run --features profiling` linking all 11 benches. No
benchmark: this adds no request-path work at all, and the request path is
untouched.

## Health, readiness and metrics, second 0.8 slice (2026-10-12, in e91fd65)

Owner chose to continue 0.8 after the access log. Nothing was committed,
tagged, pushed, merged or released. The previous section's heading was
demoted; `doc/OBSERVABILITY_0_8.md` is the behaviour document.

FACT: `[server.admin] listen = "127.0.0.1:9090"` adds a **separate listener**,
off by default, serving exactly `GET`/`HEAD` on `/health`, `/ready` and
`/metrics`, 405 for other methods and 404 for other paths. It never receives
the application `HostRouter`, which is how the 0.8 requirement that this
surface "must not silently occupy an application route" is met structurally
rather than by convention: a test asks the application listener for all three
paths and requires the application's own 404 with no exposition in the body.
The address is bound before pools start, so a clash fails startup; a
non-loopback bind logs a warning naming what became reachable. There is no TLS
and no authentication, deliberately and documented.

FACT: health and readiness are different answers on purpose. `/health` is 200
for as long as the process can answer, including the whole of a graceful stop.
`/ready` is 503 `draining` from the moment a stop is triggered, and 503
`php_pool <label> has no live worker` when a pool cannot serve. A pool below
its target but still serving stays ready: pulling a partly healthy instance
during a replacement storm makes an incident worse. FastCGI and proxy backends
are not in the decision, because Hamster neither owns nor probes them — the
same reason its startup line does not certify them. `--control status` reports
the same judgement (`ready=true` / `ready=false reason=...`, plus `admin=`)
from the same function, so the pipe and `/ready` cannot disagree.

FACT: the exposition is hand-written Prometheus text with cardinality bounded
by the configuration. New HTTP counters: connections total/active/limit,
requests by status class, incomplete responses, response bytes, and a
fixed-bucket duration histogram (1 ms to 10 s, not configurable, because a
configurable histogram is a configurable cardinality). Everything else —
pools, proxy upstreams, FastCGI endpoints, TLS certificates and handshake
outcomes, access-log records and queue depth — is read from the snapshots
those subsystems already keep for `--control status`, so nothing is counted
twice. Scrapes are not counted as application traffic. Configured names are
escaped before reaching a label.

FACT: the request observation point was generalised rather than duplicated.
The response-body wrapper from the access-log slice is now `MaybeObserved` /
`ObservedBody` and feeds both the log record and the counters from the one
place a request finishes, so `requests_incomplete_total` and the log's
`complete:false` cannot disagree. A server with neither consumer keeps the
body it always had behind one enum branch, and allocates no counters.

FACT: two findings worth keeping.
1. `TlsListener::handshakes_started` is `#[cfg(test)]` — the **field** too, so
   the counter does not exist in a shipped build. A `handshakes_started_total`
   metric was therefore dropped rather than quietly adding an atomic to every
   handshake in another subsystem; the TLS outcome counters have no
   denominator, and `doc/OBSERVABILITY_0_8.md` says so. Reverted cleanly.
2. The new clash validation rejected `listen = "127.0.0.1:0"` for both the
   application and the admin listener, because the two addresses compare
   equal. Port 0 asks the OS for a free port, so they are two sockets; the
   rule now ignores port 0. Found by the integration test, which could not
   start at all, and pinned by a configuration test.

Also unconditional now: the `draining` flag, which was `#[cfg(windows)]` for
the control endpoint. Readiness needs it on every platform.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite 278 tests (260
before this slice: 5 metrics unit, 6 admin unit, 6 admin integration and 1
configuration test, plus the extended drain rehearsal); ignored
`windows_control` 14/14, `windows_headless` 2/2, `windows_containment` 4/4,
`tls_reload_process` 3/3; the non-Windows cfg-flip compile check; MSVC release
build; `cargo bench --no-run --features profiling` linking all 11 benches. A
release-binary probe confirmed the startup lines, that the application
listener answers `/metrics` with 404, that three 200s and two 404s appear as
`2xx`/`4xx` with five connections and five access-log records, that
`--control status` reports `ready=true` and `admin=`, and that `--control
stop` exits 0.

FACT: the release A/B for the counters was taken and is **inconclusive**,
recorded in `doc/BENCHMARKS.md` (*Observability counters A/B — inconclusive,
with the reason, 2026-10-12*). Five order-balanced pairs of 100,000-request
runs, this time on a quiet machine (4% background load, against 81% for the
access-log run earlier the same day). Balanced RPS -3.5% with a +2.2% ordering
bias — but the same comparison gave -27.6% in one pair and +18.4% in another.

The spread was diagnosed rather than called variance, and the diagnosis is
worth keeping for every future benchmark on this machine: across ten runs the
**server process** used 3.44-3.77 effective cores, a 10% spread, while
throughput ranged 3,768-6,062 RPS, a 61% spread. The server was CPU-saturated
in every run and received nearly the same CPU time; what differed is how much
work that CPU delivered, which on this 2-performance/8-efficient-core
processor is the signature of per-process thread placement, chosen afresh for
each server start — and the driver starts a fresh server per run. It follows
that `CPU_US_PER_REQUEST` from this driver tracks placement rather than
per-request work (it moves inversely with throughput at constant core usage),
so the +48.6 µs balanced CPU delta is not the counters and cannot be: five
relaxed atomics are tens of nanoseconds, four orders of magnitude below it.
1% of this baseline is about 7 µs per request, so the change being measured is
roughly three orders of magnitude below the driver's resolution here.

What the run supports: no large regression, and no measurable memory
difference. No cost figure is claimed for either 0.8 slice. A valid
measurement needs the server pinned to the performance cores or a homogeneous
machine, the in-memory `/api` route, and the historical 500,000-request
parameters; `benchmarks/static_http.ps1` does not set affinity today. The
500,000-request run on a clean machine is still owed for both slices.

Still missing from 0.8: transactional configuration reload with generation
identity, lifecycle records of their own, a `validate` CLI operation, and log
rotation inside Hamster. Linux remains unverified.

## Structured access log, first 0.8 slice (2026-10-12, in e91fd65)

Owner asked for a release assessment, accepted that a public release is not
ready, and chose to start the 0.8 scope with structured access/error logging
because it is the heaviest missing piece: before this slice a 500 reaching a
browser left nothing behind at all. Nothing was committed, tagged, pushed,
merged or released.

FACT: `[server.access_log]` is new and off by default. `path` (omit for
stdout, relative resolves beside the configuration file), `include_query`
(default false) and `queue_capacity` (16-65536, default 1024). The file is
opened before pools start and before listeners bind, so an unwritable path is
a startup failure. Records are JSON Lines with `kind`, `ts_ms`, `id`, `method`,
`target`, `host`, `version`, `scheme`, `peer`, `status`, `head_us`,
`total_us`, `bytes`, `complete` and an optional `error`. Full behaviour,
including what is deliberately never logged, is in `doc/ACCESS_LOG_0_8.md`.

FACT: the hot path builds one owned record and offers it to a bounded queue
with a non-waiting send; a single writer task owns all JSON formatting and
every write. Stdout goes through Tokio `io-std` (a new feature flag on an
existing dependency), so a stalled log reader occupies a blocking-pool thread
rather than a runtime worker. A full queue drops and counts, and the count is
reported both as a `log_dropped` record in the log and as
`access_log=<target> queued= written= dropped= failed=` in `--control status`.
Queued records are flushed on a graceful stop: the server drops its sink only
after every connection task ended, then waits for the writer.

FACT: error reasons needed no plumbing. Every error status in the crate is
built by `handler::simple_response` or `static_files::text_response`, whose
`&'static str` body is already the precise reason, so those two constructors
attach it as a response extension that the logger removes. `FastCGI Response
Timeout` and `PHP Worker Queue Full` therefore reach the log without touching
routing, FastCGI, the pool or the proxy.

FACT: two defects were found by the new tests, not by reading the code.
`complete` read from the body's end-of-stream alone marked **every** fully
served static file incomplete, because hyper stops polling a body of declared
length once those bytes are written and drops it without a final
end-of-stream; a declared length is now satisfied by its byte count. And
Clippy's `large_enum_variant` showed the response-body wrapper would have
sized every response — including every response on a server with no log — for
the logged variant (360 vs 144 bytes); the record state is now boxed.

FACT: a 200-request burst against the smallest queue (16) drops roughly 60% of
its records while answering every request 200. That is the designed trade and
it is now a test: written plus dropped equals the traffic, and the gap is
visible in the log itself. The same burst against the default queue loses
nothing. A test that asserted "no drops at queue 16" was wrong and was
rewritten; do not reintroduce it.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite 260 tests (244
before this slice, plus 8 unit, 7 integration and 1 configuration test);
ignored `windows_control` 14/14, `windows_headless` 2/2,
`windows_containment` 4/4, `tls_reload_process` 3/3; the non-Windows cfg-flip
compile check; MSVC release build; `cargo bench --no-run --features profiling`
linking all 11 benches; and a PowerShell parse check of the modified
`benchmarks/static_http.ps1`.

FACT: the release A/B was taken and is recorded in `doc/BENCHMARKS.md`
(*Structured access log A/B — order-balanced paired run, 2026-10-12*). The
owner declined the full 500,000-request run as too heavy at that moment and
approved a lighter one, so `benchmarks/static_http.ps1` gained `-ConfigPath`
(default unchanged, so earlier entries stay comparable) and five pairs of
100,000-request runs were taken with the arms alternating. The first three
pairs, all run baseline-first, made the logging arm look 3-9% faster, which a
change that only adds work cannot be: the ten runs in order show a monotonic
warm-up drift favouring whichever arm ran second, and reversing the order
reversed the sign. Order-balanced: RPS **-0.21%** against an ordering bias of
**±5.6%**, CPU **+19.8 µs/request (+2.0%)**, working set **+0.3-0.4 MiB**. The
machine was an i5-1235U at 81% background load yielding 3.0-4.0k RPS where
this project's 0.5 runs recorded 6.9-7.3k, so **no absolute number from this
run is comparable with any earlier entry**. The honest statement is "no large
regression found, small cost not excluded"; a clean machine at the historical
parameters is still owed before any cost claim. Completeness, separately: the
logging runs wrote 303,000 records for 303,000 requests with zero
`log_dropped`.

Still missing from 0.8, and therefore from any release: metrics, health and
readiness; transactional configuration reload with generation identity (the
`id` field is per process and there are no generations yet); lifecycle records
of their own; log rotation or reopen inside Hamster; a `validate` CLI
operation. Linux remains unverified.

## Two Service lifecycle gaps qualified (2026-10-11)

Owner enabled Administrator; actual elevated `OBJECTLAPTOP\cmmph` token was
verified. Read `doc/WINDOWS_SERVICE_LIFECYCLE_GAPS.md` and the current WinSW
deployment section of `doc/WINDOWS_OPERATIONS.md` first. This entry supersedes
the NOT CLOSED preparation below; no commit/tag/push/merge/release was made.

FACT: unmodified WinSW 2.11 NET461 / LocalService reproduced actual main exit 0
with wrapper RUNNING/access denied, and wrapper-only death leaving main/PHP
serving. Vendor `SignalStopped` unnecessarily requests SCM All access; after
its error disposes the Process, subsequent custom stop can also fail on Id.
Hamster's own job cannot contain its parent wrapper.

Fixed via a small companion C# extension, not a vendor binary/Rust rewrite:
exact vendor hash/layout pin; wrapper self-membership in kill-on-close job
before any child spawn; rooted non-inheritable process-lifetime handle; narrow
synchronous first-chance filter reports STOPPED through existing own-Service
status handle only after actual main exit 0. No LocalSystem/SCM ACL expansion.
Sample XML enables it; build DLL/reference assets using
`examples/winsw/Build-LifecycleExtension.ps1`. Private vendor/CLR coupling is
intentional and scoped; mismatches fail closed before application startup.

Before/after matrix PASS:
`target/windows-service-qualification/1a4843a8118e44749ff26f5f8de2d55b-matrix.json`;
baseline `531b5518ba534d1d80f9b5d4105836fb-results.json`;
fixed `6e6e8a5984e04317bf5ad6d4c7334066-results.json`.
Second full fixed pass after template integration:
`ce6598d8467e481c8481e1ef45cb6c53-results.json` (same evidence directory).
Direct stop drained disconnected transaction, main/wrapper exit 0, SCM STOPPED,
no recovery during six-second observation. Normal SCM stop retained ACK/reap.
Wrapper-only idle/busy/STOP_PENDING/repeat1/repeat2 all reclaimed exact retained
old trees (6–40 ms first, 9–30 ms second) and automatically recovered actual
PHP/TLS/CGI readiness (4.354–5.648 s first, 4.387–4.499 s second), with one main,
no port conflict and independent instance/operator php-cgi still serving.
Crash during stop DID trigger recovery: operator must verify STOPPED/reissue
SCM stop if interrupted. Abnormal kill is not transaction drain/retry.

Both runs also passed real LocalService control SID S-1-5-19, different-account
denial, TLS reload/rollback, worker replacement/descendants, main crash recovery,
64-KiB stderr rolling (eight archives plus active), 12-second forced-budget
convergence (13.494 / 13.308 s). No request-hot-path change or new throughput claim.
Fresh fmt, strict all-target/all-feature MSVC Clippy, default 244 tests, release
build and bench link gates passed. Fresh ignored control 14/14, containment 4/4,
C# smoke and runner error/finally self-test, six PowerShell/XML parsers, 42 PHP
fixture lints and diff check passed. Previous unchanged ignored cancellation,
no-retry, timeout, multi-pool and containment evidence remains applicable.

Successful run Services/Event Log sources/fixture keys removed; owned handles
waited. Four old deletion-denied diagnostic directories remain untouched.
Ignored JSON/log/compiler evidence intentionally retained. Two new failed
diagnostic fixture directories (`9a846c39af004436b42e39279f58bae2`,
`c4b39c487968474491b7130b976e806f`) also remain: their removal command was
tool-denied before execution, no retry/bypass or archive. Final scoped inventory
found zero qualification Services and zero fixture processes. Successful
deployment directories were removed automatically.
Early new harness
failures are explicitly recorded, not PASS: invalid fixture ID corrected;
native child runner now waits finally despite stderr/exit 1 (self-test).
Only exact owned handles/Service identities are cleanup authority, never old
report PIDs or process names. No unrelated Service/firewall/data changes.

Qualification scope: Windows 11 x64 build 26300, pinned WinSW 2.11 NET461,
LocalService. Other vendor/OS/account/outer-job environments remain unqualified.
Boot Auto CONFIGURED, NOT VERIFIED (no reboot, test Services removed); Linux
and SYN-blackhole NOT VERIFIED. Next useful work: Owner review the scoped
integration/workaround before deployment; do not repeat completed stress absent
new changes/evidence. No remaining administrative blocker for these two cases.

## Historical preparation: Service lifecycle gaps (before elevation, NOT CLOSED)

Current token `OBJECTLAPTOP\cmmph` has Administrator=False. A RunAs/UAC attempt
did not complete; its waiting tool session was cancelled, no elevated matrix
report or new Service was created. Historical Service PASS below remains
SCM-stop scoped. Do not claim the two new gaps were closed.

Read `doc/WINDOWS_SERVICE_LIFECYCLE_GAPS.md` next. Root cause is WinSW 2.11's
unsolicited exit-0 `SignalStopped` opening SCM with default All access, not
Hamster drain/control ACL. Wrapper death is outside Hamster's own job boundary.
New opt-in C# integration shim in `examples/winsw/` uses wrapper self-membership
before application spawn and the existing own-Service status handle for the
specific vendor access-denied exit-0 branch. Original WinSW exe, Rust runtime,
LocalService/pipe/SCM privileges and deployment XML were not changed this turn.
Private vendor/CLR coupling is UNQUALIFIED; never deploy merely because it
compiles. Build/fail-closed/filter/native-layout smoke checks passed.

Next action is one Administrator command, no architectural choice needed:
`powershell.exe -NoProfile -ExecutionPolicy Bypass -File "D:\Project\Hamster-Wheel\examples\winsw\Invoke-LifecycleQualification.ps1"`.
It runs original vendor baseline then opt-in shim qualification: actual main
exit 0, wrapper-only idle/busy/stop crashes, two repeated recovery cycles,
real serving, retained identity handles and independent php-cgi isolation.
No automatic elevation, reboot or changes to other Services; own fixtures/
Services are cleaned. STOP_PENDING recovery is an assertion still to verify.

Fresh release Process rehearsal passed (real PHP, HTTPS, reload/rollback,
drain/ACK, replacement/ordinary descendant containment, fallback/isolation),
evidence `target/windows-service-qualification/63e55b41ec0942a19b5f6eadb312bb71-results.json`.
20-ms main-death reclaim, 12,352-ms fallback with 12-second grace; NOT SCM
wrapper results. Its processes/fixture/keys were removed. Four old tool-denied
diagnostic directories are preserved. New ignored compile cache/JSON retained.
Rust default 244, fmt/strict Clippy/release/bench links, containment 4 and
ignored control 14 passed. Existing parallel multi-pool test initially failed
because socket write did not establish reservation ordering; now waits for
100 Continue before competing body, full parallel 9 passed three times after
correction. Fresh PHP cancellation/stress (2), possibly-started no-retry (1),
timeout no-duplicate (1) and busy shutdown (1) passed; 42 PHP fixtures linted.
No production request/worker behavior was changed; Linux/boot remain outside
scope / NOT VERIFIED. No commit/tag/push/merge/release.

## Actual WinSW/SCM qualification completed in scoped rehearsal (2026-10-11)

After Owner enabled Administrator, `OBJECTLAPTOP\cmmph` was elevated and the
full `Invoke-ServiceQualification.ps1 -Mode Service` returned exit 0. Two
unique LocalService Services were installed then uninstalled, without reboot,
firewall/other Service changes or commit/tag/push/merge/release. Latest evidence:
`target/windows-service-qualification/58e4cf9b7f324b79b6d0b35558ff7c7a-results.json`.
This supersedes the no-elevation/Service NOT VERIFIED preparation below.

PASS — real HTTP/HTTPS/PHP worker and independent operator php-cgi; same-account
control with actual LocalService user/owner SID `S-1-5-19`; different elevated
operator denied; TLS reload/last-good rollback; disconnected busy transaction
drain/ACK/main exit 0 before SCM STOPPED; replacement generation/ordinary child
containment (29 ms reclaim); automatic SCM recovery and PHP readiness; actual
WinSW 64-KiB stderr rolling (8 archives + active); 12-second forced budget,
14,255-ms observed stop/STOPPED convergence; other-instance/endpoint isolation
verified by a real PHP response through the independent instance afterward.
The first corrected Service pass also succeeded (9f83d684 report, 22-ms reclaim,
14,238-ms forced convergence). No throughput claim is derived from these times.
SCM Auto was configured during the test, but boot is NOT VERIFIED, no reboot
was authorized. The temporary installed SCM entry was removed afterward.

Two real Service runs reproduced harness/compatibility issues before correction:
probe XML lacked required description; then WinSW 2.11's unsolicited child
exit-0 callback called SignalStopped and failed SCM access under LocalService,
leaving wrapper RUNNING. Keep low privilege. The qualified stop contract is
SCM/WinSW stopwait -> Stop-Hamster -> control stop -> actual exit, NOT an
out-of-band direct control stop under that wrapper. Probe now SCM-stops only
after its finite exchange completes; expiry is bounded. No production Rust,
worker/IPC protocol or ACL broadening was needed. See source-backed limitation
and actual Service results in `doc/WINDOWS_OPERATIONS.md`.

Gates rerun after corrections: full MSVC default suite 244 passed, fmt, strict
all-target/all-feature Clippy, PowerShell/XML parse, fixture PHP lint and diff
check, release build and complete profiling bench linking passed. Prepare
mode also passed with the new required-field checks. Required WinSW XML fields
are now checked before installation;
denied operator SID/error is recorded. Existing ignored lifecycle evidence is
unchanged; two real Service rehearsals verify the new integration, not a new
request hot path.

Cleanup PASS for every attempt's processes/owned Services/Event Log sources;
final SCM/process inventory empty. Successful scratch deployment/keys deleted.
Four earlier failed diagnostic directories remain, recursive delete rejected
by tool policy: `f281d460959642b693d96a5cd1067008`,
`3babe74087f843ee90803e26c4520043`, `086855c6ff724553b08a9c1c63ad5f09`,
`31df729cbf24402b8763205d42dcba8b` under
`target/windows-service-qualification/`. JSON evidence/tool cache retained.
Do not claim complete file cleanup; no original application data was touched.

NOT VERIFIED — abrupt WinSW wrapper death (main-crash recovery is different),
independent main exit-0 compatibility outside SCM-stop contract, other Service
accounts, non-elevated operator SCM permissions, boot/reboot, SYN-blackhole and
native Linux. Wrapper.log needs separate retention; stdout/stderr rolling does
not cover it. This is scoped Windows Service evidence, not release readiness.

## Icon and dedicated WinSW qualification preparation (2026-10-11)

Latest Owner authorization permits two separate temporary test Services on
this host, Auto startup and SCM recovery; no reboot, firewall/other Service
changes, commit, tag, push, merge or release. Current token is
`OBJECTLAPTOP\cmmph`, **Administrator=False**. No Service was installed or UAC
prompted. Earlier VM-only limitations below are historical, not current Owner
policy. Actual Service qualification remains **NOT VERIFIED** for lack of
elevation, not lack of a VM.

Windows release now embeds `icon/hamster-wheel.ico` (not the `-2` variant).
`build.rs`/`icon/hamster-wheel.rc` use build-only embed-resource, restricted to
the main Windows binary; missing resource compilation fails the build.
Read-only PE resource inspection verified all six image payloads byte-for-byte
against the source ICO. Non-Windows compilation skips the resource; Linux is
still NOT VERIFIED. Release build, fmt, strict all-target/all-feature Clippy
and default MSVC suite (244 passed) passed after this build change.

New `examples/winsw/Invoke-ServiceQualification.ps1` modes:
Prepare (staging only), Process (no SCM), Service (fails exit 3 before writes
unless elevated). See `doc/WINDOWS_OPERATIONS.md` for the complete procedure.
Run once in **PowerShell, Run as administrator**:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "D:\Project\Hamster-Wheel\examples\winsw\Invoke-ServiceQualification.ps1" -Mode Service
```

It creates GUID-scoped deployment, copied PHP, private fixture PKI, loopback
ports and two LocalService Services (main Auto/restart; finite same-account
control probe). Only state/logs are writable by LocalService; inherited broad
checkout ACLs are removed on the scratch root. XML sample remains Manual;
the test explicitly verifies SCM Auto without rebooting. Service mode checks
SCM STOPPED only after retained main exit and verifies real recovery/rotation.
Cleanup disables only owned recovery, waits handles, uninstalls owned Services
and removes Event Log sources; JSON stays outside the deleted fixture.
Inspect Cleanup status on failure; diagnostic fixture retention does not mean
a failed Service cleanup is safe to ignore. Do not kill by name/snapshot PID.

FACT — Prepare and Process completed. Release real PHP/independent php-cgi,
HTTP/HTTPS, control, TLS adoption/rollback, disconnected-client drain/ACK,
initial/replacement/ordinary-descendant containment, 12-second forced fallback
and other-instance isolation passed. Latest local ignored evidence:
`target/windows-service-qualification/98443e3b6c2243238fddd333f6a8f585-results.json`.
Two harness errors (URI mount expectation and path-separator identity check)
were corrected and the full process rehearsal rerun successfully; they were
not production failures. Successful rehearsal files/keys and all owned
processes were cleaned up. Two earlier failed-run fixture directories remain:
`target/windows-service-qualification/f281d460959642b693d96a5cd1067008` and
`target/windows-service-qualification/3babe74087f843ee90803e26c4520043`.
Read-only process/Service inventory is empty for these deployments; the tool
policy rejected their explicit recursive deletion. They contain scratch PHP
copies/certificates, not production data. Do not claim complete file cleanup;
the Owner can remove these two exact directories after inspecting diagnostics.
WinSW cache and ignored JSON evidence are intentionally retained.

Additional gates: XML/all three PowerShell scripts parsed, 46 PHP fixtures
linted, non-admin Service guard exit 3 confirmed, fmt/strict Clippy, complete
profiling bench linking and diff check passed. No production hot-path change
was made in the Service-preparation pass; earlier Windows regression evidence
above remains applicable.

NOT VERIFIED — actual LocalService/cross-account ACL, SCM recovery, Service
STOPPED ordering, WinSW log rotation and boot startup (only XML prepared,
not an installed SCM entry). Abrupt wrapper death is distinct from main crash;
WinSW `.wrapper.log` is not covered by stdout/stderr rolling policy. Native
Linux and TCP SYN-blackhole remain NOT VERIFIED. Next action is the single
elevated command above; do not infer Service readiness from Process PASS.

## Windows operations handoff — final local qualification (2026-10-11)

Current authoritative operational entry point: `doc/WINDOWS_OPERATIONS.md`.
Baseline was clean `dev` at `18a2f13` (matching `origin/dev`), not the older
uncommitted states recorded below. This pass is uncommitted. No commit, tag,
push, merge, release, Service installation, firewall change or persistent
system setting change was performed. Existing application data/services and
historical investigation directories were preserved.

FACT — corrections proved before/after:

- Control listener abort left detached exchanges/pipe handles alive. A
  listener-owned JoinSet now bounds ownership, aborts and joins all exchanges
  and refusals; server shutdown explicitly signals and awaits it.
- An invalid framed response status was treated as client success. Validate
  OK/ERR status and the complete response size before accepting it. One
  five-second deadline now covers all exchange phases; partial EOF is not a
  command, peer-close means EOF rather than one extra byte, and accepted stop
  is not lost on a response write failure. Early disconnect cannot permanently
  remove the endpoint; ERROR_NO_DATA renews the waiting pipe instance.
- Cancelled TLS reload released its gate while spawn_blocking work remained.
  Its owned mutex guard now travels with the blocking job and through adoption;
  cancelled work cannot publish or enqueue more compilations behind itself.
- The completion log preceded disconnected-client PHP transaction drain.
  `HTTP listeners drained` is distinct from `Hamster Wheel stopped`, now
  emitted only after successful pool shutdown/reap. A barrier fixture checks
  normal transaction completion and SHUTDOWN_ACK before actual exit.

FACT — important corrections to earlier handoff claims:
`PoolSnapshot` samples counters and slots independently: pids/counts can differ
briefly during lifecycle transitions. PID/status is diagnostics, not identity
or permission to terminate. WinSW 2.11.0 **does not apply stoptimeout to its
custom-stop branch** (confirmed against WrapperService.cs), despite earlier
guidance below. The example now uses `examples/winsw/Stop-Hamster.ps1`: verify
wrapper parent, retained main handle, executable/config argv/creation time;
issue --control stop; wait real exit; terminate only that handle after grace.
Failed identity validation refuses any kill. XML, ownership/timeout budgeting,
LocalService/pipe policy, rotation and VM-only commands are in the runbook.
The helper's process-level rehearsal is not a WinSW/SCM Service rehearsal.

FACT — final Windows gates: full default MSVC suite 244 passed; real-PHP worker
regressions 59 passed; multi-pool 9, real FastCGI 8, readiness 1 passed.
Release process rehearsals: windows_control 15, windows_containment 4,
tls_reload_process 2, windows_headless 2 passed. rustfmt, strict all-target/
all-feature Clippy, release build, all profiling bench linking and diff check
passed; 45 PHP fixtures linted and XML/PowerShell parsed. Control overload:
112 baseline / 127 peak / 112 after deadline handles, followed by recovery.
Containment used retained identities and reclaimed tested descendants in
0–5 ms; graceful and 5-second forced helper paths passed, including isolation
from a different deployment and refusal of an unverified wrapper.

No broad throughput benchmark was warranted: no request hot-path change.
Long historical soak/random-kill and measurement probes were not repeated;
external Laravel/PMS/application environments were deliberately excluded,
not marked passing. Owned processes/temp fixtures were reaped/removed; no
repository PHP/Hamster process or new control scratch directory remained.

INFERENCE — Windows console/detached local operation and its bounded stop
helper are ready for Owner handoff; this is neither a release decision nor
Service qualification. Only deployment/environment qualification remains in
this operations slice. Keep native SCM integration deferred; no evidence
requires a new architecture. Broken log sinks are handled, but indefinitely
blocked native file/log I/O is not claimed interruptible or hard real-time.

NOT VERIFIED — real Service account and cross-account access, SCM restart/log
rotation, unusual outer-job restrictions, TCP SYN-blackhole and native Linux.
No isolated Service VM was available. Next action, when that environment
exists: execute the staged VM procedure, verify actual token owner/ACL and
operator reload policy; do not infer Service readiness from XML/local tests.
No additional local FastCGI optimization or repeated VM request is needed.

## Lifecycle visibility, applied from the supervision study (2026-10-11)

Owner approved applying what
`doc/research/HERDR_HAMSTER_CONTROL_SUPERVISION_STUDY.md` actually supports on
this host. Nothing was committed, tagged, pushed, merged or released; no
Service, firewall or application data was touched.

CORRECTION first, because it changed what was worth building. The study's
lesson "SCM `Running` is not application readiness" was initially read here as
a gap in `status`: the control endpoint logs its line before
"Hamster Wheel listening on", so it looked as though `status` could answer
while workers were still starting. That inference was wrong, and it came from
log order rather than code order. `src/server.rs` runs
`config.start_workers().await` (every initial worker READY), then
`TcpListener::bind` for every listener, and only then creates the control
pipe. An answer from the endpoint therefore already proves initial readiness
and bound sockets. A `serving=true|false` key at startup would have been
noise.

FACT: the real lifecycle gap is at the other end. The control task is aborted
only after `serve_listeners` returns, so the endpoint keeps answering
throughout a graceful drain and reported a draining server exactly as it
reports a serving one. `status` now carries `state=serving|draining`, set from
an `AtomicBool` the shutdown future stores before the drain begins. A
supervisor that restarts on an unhealthy answer can now tell "stopping on
purpose" from "broken". Rehearsed with a three-second PHP request in flight:
`stop` is acknowledged, `status` reports `state=draining`, the in-flight
request still completes 200 and the process exits 0.

FACT: `status` now also carries `pids=` per pool, taken from the same snapshot
as the counts so the two cannot disagree. This serves the study's
"exact PID, never process names" gate for the tree Hamster owns. The existing
`status` rehearsal asserts the reported PID equals the one in the startup log.
Note for whoever reads health from this surface: compare `live` against
`target`. `ready` counts READY *and idle* workers, so a healthy pool at full
concurrency reports `ready=0`; the runbook now says so.

FACT: the study's "measure the log when the pipe closes" gate had a precedent
for Ctrl+Break only (`tests/tls_reload_process.rs`). It now covers the control
path: a detached TLS server whose piped stdout is closed under it still
answers `status`, two consecutive `reload-certificates` and a graceful `stop`,
exits 0, and writes nothing to stderr. The test was verified to bite —
replacing the control stop's `outln!` with `println!` makes the server stop
exiting 0 — and that is the first executable evidence for the `outln!`/`errln!`
rule in `AGENTS.md`. A first negative check aimed at
`report_certificate_reload` did **not** bite, which established something
worth knowing: the control reload path logs nothing of its own, so only the
stop path's own output is exposed to a closed log reader.

FACT: the runbook's WinSW `<stoptimeout>` guidance now includes the control
client's five-second retry budget in the sum, because the client may
legitimately spend it waiting for a pipe instance or retrying `ERR busy`. A
timeout sized only from request-plus-drain can force-kill a server that was
stopping correctly.

Deliberately NOT applied, as the study itself argues: swapping the transport
for `interprocess`; widening the control plane towards Mullvad's gRPC
subscriptions or Distant's remote process/file protocol; Mullvad's broad
local-user pipe ACL; WezTerm's client-auto-spawns-daemon model; and any
Zellij/Distant session machinery, which belongs to Herdr rather than Hamster.
Choosing WinSW versus a native SCM adapter still needs the isolated VM and is
untouched.

This closes the open decision recorded below as "whether `status` should ever
carry more than the operational fields it has": it carries two more fields,
both lifecycle facts a supervisor must act on, and no new subsystem.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; `windows_control` ignored 11/11.

## Control overload bound and WinSW stop contract (2026-10-11, current pass)

This pass preserved the pre-existing uncommitted control-path, configuration,
test and documentation changes. No commit, tag, push, merge, release, Service
installation, firewall change or unrelated process cleanup was performed.

FACT: the four-exchange semaphore did not bound refusal tasks. Each refused
client could hold a spawned task and pipe handle for the five-second peer-close
deadline. An ignored, process-level Windows regression (`tests/windows_control.rs`)
held 96 refusal peers open: exact Hamster PID handle count grew from 112 to
211 and the new resource-bound assertion failed. A separate eight-permit
refusal budget now caps those waits. Under the same two-burst probe the count
was 112 baseline / 127 peak / 112 after the deadline, then `status` and
`stop` succeeded. Three subsequent repetitions passed with peaks 124–127;
Windows may surface either a closed-pipe write error or incomplete-frame read
for an excess client, and neither is command success. When both budgets are
full, the server closes excess pipes
without attempting an unbounded `ERR busy` task. The client now rejects an
empty or incomplete response frame rather than reporting false command
success. Its reply exchange shares the same overall five-second deadline as
pipe-open and `ERR busy` retries.

FACT: the WinSW v2.11.0 candidate XML is
`examples/winsw/HamsterWheelService.xml`. It uses `startarguments` for the
server and `stoparguments` for `--control stop`, with a sample 120-second
graceful deadline, log rolling and restart-on-failure. WinSW's XML contract
says it waits for both the stop helper and main process to exit; Hamster's
`OK stopping=true` only acknowledges receipt. The runbook now requires an
exact process-exit check and treats forced termination as the last resort.
The sample uses `LocalService`; the pipe DACL admits only owner/System, so a
different operator account cannot directly reload TLS without a reviewed
same-account procedure or a deliberate ACL decision.
The XML parsed locally, but WinSW and a real Service were **NOT VERIFIED**:
no isolated VM or VM command was available. Service-account/operator ACLs,
SCM stop timing, crash restart and log rotation remain VM qualification work.
The operator-managed `php-cgi` endpoint still needs separate supervision.

Verification: Windows `cargo fmt --all -- --check`, strict MSVC Clippy,
default suite, ignored control suite (9/9), ignored headless suite (2/2),
ignored containment suite (4/4), MSVC release build, profiling bench link and
`git diff --check` passed. The
100-client-style bound probe was run before (failed, 112→211) and after
(passed, 112→127); its exact process was reaped. Linux remains **NOT
VERIFIED**. TCP SYN-blackhole remains **NOT VERIFIED**.

Next: run the isolated WinSW Service rehearsal from `doc/WINDOWS_OPERATIONS.md`,
then decide whether a service integration is necessary. Do not infer Service
readiness from detached-process tests or the WinSW XML alone.

## Control-path verification gaps closed (2026-10-11, second pass)

This pass continues from the section below, at the committed `e902c78` with the
control endpoint already in place, and closes the verification gaps that
section left open: connection overload and recovery after `ERR busy`,
timeout/cancellation of an incomplete request, and a compile check of the
non-Windows configuration. Nothing was committed, tagged, pushed, merged or
released. No firewall or service on this machine was changed.

FACT: three real defects were found by building those checks, and all three are
fixed with regression tests.

1. The pipe's instance ceiling was `MAX_CONCURRENT_EXCHANGES + 1`. At
   saturation the next instance could not be created, the accept loop treated
   that as fatal and returned, and **the control endpoint was then dead for the
   rest of the process life** — a server an operator could no longer stop
   gracefully. The ceiling is now high (254) because the work bound is the
   semaphore, not the instance count; the waiting instance is ensured at the
   top of the accept loop with a short retry, a failure is reported once
   through `errln!` instead of ending the loop, and the instance is restored
   immediately after an accept so a client arriving during the handoff is not
   met with a bare `ERROR_PIPE_BUSY`.
2. `OK` answers carried no terminating blank line. `Response::ok` appended one
   newline to a detail block that sometimes already ended with one, so `ping`
   answered `OK\npid=N\n`. This was invisible while the client read to
   end of stream, and became a hard fault the moment the server waited for
   the client to close first: every command then cost the full five-second
   exchange deadline, and three rehearsals failed with
   `control request timed out`. The terminator is now normalised in
   `Response::ok`, so the documented frame is real for every command.
3. A refusal was written on the accept loop and the pipe was dropped straight
   after. A client that never read it could delay later accepts, and Windows
   can discard bytes an unread client leaves behind when the server closes
   first. Refusals are now spawned off the accept loop, and both refusals and
   answers wait — bounded by the exchange deadline — for the client to close,
   which is the proof it read the answer.

FACT: the client now treats contention as contention rather than as an error.
`ERROR_PIPE_BUSY` on open is retried every 25 ms, and an `ERR busy` answer is
retried after 100 ms, both inside the same five-second budget.

FACT (new rehearsals in `tests/windows_control.rs`, all `#[ignore]`, real
detached servers): four held connections make a raw fifth read
`ERR busy\nmessage=too many control connections\n\n`, and after the
holders are released the shipped client recovers and the endpoint still
answers `ping` and `stop`; a request line written without its newline is
never executed — even though its text is `stop` — is released well inside
15 s, leaves the server running and the endpoint serving; and every answer
(`ping`, `status`, an unknown command) arrives framed with its blank line in
under a second while the connection is still open. The framing test was
verified to bite: with the terminator removed it fails with
`"ping\n" was answered without its frame`.

FACT: the suite is now stable rather than intermittent. Before the framing fix
`an_overloaded_endpoint_refuses_and_then_recovers` failed in roughly two of
five runs; after it, nine consecutive runs of
`cargo test --test windows_control -- --ignored --test-threads=1` passed (six
at 7 tests, then three at 8 with the framing test added), each in 11.8–13.9 s.
The flake had one cause, not two: an answer that was only complete when the
server closed made every exchange a race with that close.

FACT: the non-Windows configuration is now compile-checked by substitute. A
scratch copy of `src/` with the platform `cfg` attributes flipped to the
non-Windows arrangement (Windows-only items excluded, `not(windows)` items
always compiled, tokio's unix signal syscalls stubbed because they do not exist
on this target) first produced **ten dead-code errors** under
`cargo clippy --lib --bins --all-features -- -D warnings`: the protocol
constants, `PIPE_SDDL`, `Response` and its four associated functions,
`single_line`, `execute`, `status_details` and `reload_certificates`. A Linux
gate would have failed on them. `src/control.rs` was restructured: the shared
`Command` and `validate_name`, which configuration and the CLI need on every
platform, stay unconditional, and everything else moved into
`#[cfg(windows)] mod protocol`. The same flipped check now lints clean. This
corrects the sentence in the section below that the non-Windows path is "not
compile-checked"; it is checked at compile level, which is still not Linux
operation.

Windows gates at this working tree: `cargo fmt --all -- --check`; strict MSVC
Clippy `--all-targets --all-features -D warnings`; the default suite (189
library plus every integration file, 0 failed); ignored rehearsals
`windows_control` 8/8, `windows_headless` 2/2, `windows_containment` 4/4,
`tls_reload_process` 3/3; MSVC release build; `cargo bench --no-run --features
profiling` linking all 11 bench executables; and a manual probe of the release
binary itself — control endpoint line in its log, `ping` and `status` in
114–131 ms, HTTP 200 on its listener, `stop` answered `OK ... stopping=true`
with the process gone. No benchmark was run: nothing on the request path
changed.

Still **NOT VERIFIED**, unchanged: anything under a real Windows Service
account, cross-account denial with a second real user, and native Linux. A
supervisor rehearsal with `--control stop` needs an isolated Windows VM, which
was not available: there is no WSL, no Docker, no C toolchain for a Linux
cross-build (`aws-lc-sys` still needs `x86_64-linux-gnu-gcc`) and this shell is
not elevated, so no service was installed and no firewall rule was touched.
The owner decision named below — whether a Service wrapper's stop command
should be `--control stop` — is still open, and the runbook still only
recommends it.

## Console-free control endpoint (2026-10-11)

This pass began from a clean `dev` checkout at `f5eaa76` (`origin/dev`) and
closes the gap the previous section named: console-free graceful stop and
certificate reload were **NOT IMPLEMENTED**, so a Service or supervisor could
only `TerminateProcess` and could not rotate a certificate at all. Both now
exist. Nothing was committed, tagged, pushed, merged or released; `ssl/`,
`www/` and the research backlog were left alone.

FACT: `[server.control] name = "..."` adds a Windows named pipe at
`\\.\pipe\hamster-wheel-control-<name>`, created before the server serves so a
name already in use is a startup failure. `src/control.rs` answers four
commands — `ping`, `status`, `stop`, `reload-certificates` — one request per
connection, as a status line, `key=value` details and a blank line. `stop`
reaches the same graceful path as Ctrl+C and is answered before shutdown
begins; `reload-certificates` calls the same transactional reload as
Ctrl+Break and reports `active_set=previous` when the replacement set is
refused. The same binary is the client:
`hamster-wheel --config PATH --control COMMAND`, exit 0 for `OK` and 1 for
`ERR`. `[server.control]` is rejected while compiling a configuration on any
non-Windows target.

FACT: the pipe's DACL is set at creation and protected,
`D:P(A;;GA;;;SY)(A;;GA;;;OW)`: LocalSystem and the pipe's owner only. Remote
clients are rejected and the first instance claims the name. The endpoint does
not exist unless configured.

FACT (`tests/windows_control.rs`, real `DETACHED_PROCESS` servers, five
ignored rehearsals plus one unignored regression, all passing): `status`
reported the live PID, listener and `ready=1`; `stop` exited the server with
code 0 and its PHP worker exited with it; a reload adopted a replacement leaf
that a verified handshake then presented, and a mismatched key was refused
with the adopted leaf still serving; a thread impersonating a token restricted
to `Everyone` was refused `ERROR_ACCESS_DENIED` while the owner kept access; a
client that connected and sent nothing was released by the five-second
exchange deadline, was never answered, and did not delay a later `stop`; an
unknown request was answered `ERR unknown_command`; a second server naming an
endpoint already in use exited 1 at startup.

Two defects were found by testing and fixed. Wiring the control stop into the
shutdown future first closed the trigger channel when no endpoint was
configured, so `recv` returned immediately and **a server without
`[server.control]` stopped the instant it began serving**; the unignored
`windows_headless` TLS test caught it, the sender is now held for the lifetime
of the wait, and `a_server_without_a_control_endpoint_keeps_serving` guards it.
The control client also compiled the whole configuration, so a certificate that
no longer loaded made `reload-certificates` unusable exactly when it was
needed; the client now reads only `[server.control]`.

Herdr (`herdrdev/herdr`) was read for its local control surface. Adopted: a
local socket rather than a TCP port, a named pipe on Windows, the DACL set at
pipe creation with that SDDL, and the endpoint name living in configuration.
Deliberately not adopted: newline-delimited JSON with request ids and event
subscriptions, which earn their cost across dozens of methods and long-lived
subscribers but would add a JSON dependency and correlation machinery to four
commands; and a client-started background server, because Hamster's lifetime
belongs to its supervisor and its Windows Job Object containment.

Windows gates at this checkout: format, strict MSVC Clippy, the default suite
(188 library, 32 configuration, 7 routing, 7 static and both unignored process
tests), the control rehearsal (5), headless (2), containment (4), TLS reload
process (3), MSVC release build and the profiling bench link all passed. No
benchmark was run: the endpoint adds no request-path work, and nothing on the
hot path changed.

Still **NOT VERIFIED**: anything under a real Windows Service account,
cross-account denial with a second real user (the restricted-token check
exercises the DACL, not a second login), and native Linux. The Linux
cross-target check still cannot run here, because `aws-lc-sys` needs
`x86_64-linux-gnu-gcc`; the non-Windows control path is therefore written to
compile but is **not compile-checked**. An owner decision remains: whether a
Service wrapper should call `--control stop` as its stop command, which the
runbook now recommends, and whether `status` should ever carry more than the
operational fields it has.

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
`php/8.5/php.exe`. A Windows-host cross-target check is COMPILE-CHECKED ONLY.

Native Linux (`x86_64-unknown-linux-gnu`, Ubuntu 24.04, PHP 8.5.11 NTS built
from source) was verified on the `dev-linux` branch on 2026-10-18: gates,
default suite, ignored PHP suites and release benchmarks. What that covers and
what it does not (SIGTERM, systemd, Windows effect of `TCP_NODELAY`) is in
`doc/LINUX_VERIFICATION.md`; claim nothing on Linux beyond it.

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
