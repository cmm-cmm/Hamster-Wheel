# Independent lifecycle review: Laravel persistent worker 0.4

Date: 2026-09-15  
Platform investigated: Windows 11, PHP 8.5.10 NTS, Laravel 12.66.0 PMS copy  
Status: implementation changed; milestone closure remains an owner decision

## Technical conclusion

The in-place targeted reset was the wrong abstraction. It inferred ownership
from which container instances happened to exist after two `/up` requests,
then used reflection and a growing list of state-specific repairs to reconstruct
that snapshot. Request traffic and warm-up route order could therefore change
the effective lifetime of a binding. It also missed state held by shared
objects outside the container instance table, including a controller cached on
an `Illuminate\Routing\Route`.

The replacement is an Octane application sandbox plus a small Hamster-owned
resource boundary:

1. Octane boots and deliberately warms one base application.
2. Hamster clears scoped instances once before request one.
3. Octane shallow-clones the application for each request, activates that
   sandbox, clones configuration and URL state, and rewires shared framework
   managers to the sandbox.
4. Laravel handles and terminates the request in the sandbox. Octane runs its
   request/operation listeners, forgets scoped instances, flushes the sandbox,
   and drops the cached controller from the matched route.
5. Hamster rolls back abandoned database transactions, restores the small set
   of security-relevant Eloquent static switches, Carbon's boot-time test clock,
   process cwd and default timezone, and removes worker-owned upload temporaries.
6. Only after every boundary step succeeds does Hamster write the staged
   protocol response. A lifecycle failure retires or kills the generation.

Octane's worker really does clone the application per operation and flush the
sandbox in `finally`; this is not just documentation.[^1] Its application
factory bootstraps once, loads all deferred providers before traffic, then warms
an explicit configured service list.[^2] The lifecycle is not a deep clone:
Octane's default listeners clone selected state, rewire managers, flush
authentication/session/cache/log/package state, and provide explicit `warm`
and `flush` lists.[^3]

## Facts established by falsification

### FACT — the old snapshot encoded accidental ownership

A direct PMS probe registered a lazy singleton and a scoped binding, varied
when they were resolved, then ran the old reset. The result was:

```text
lazy_singleton_survived=false
warm_scoped_was_forgotten=false
```

The reset dropped any instance absent from `warmInstanceKeys`, regardless of
Laravel's declared singleton lifetime, and retained a scoped instance present
in the warm snapshot. A warm-up path was therefore part of the application's
ownership semantics even though it was merely a Hamster implementation detail.

### FACT — the container instance table was not the state boundary

A real Laravel route caches its controller object on the `Route` instance.
Request A wrote `alice` to a controller property; after the old targeted reset,
Request B on the same worker read `alice`. No container-table restoration could
see that reference. Octane handles this explicitly after termination, and its
own tests assert both a fresh controller counter and object destruction across
requests.[^4]

### FACT — a shallow clone alone is insufficient

PHP object cloning is shallow. The cloned Laravel `Application` initially
contains the same config repository, router, kernel, event dispatcher and
manager objects as the base application. Mutating configuration through a raw
clone mutates the base. Shared managers may also retain a reference to the base
container. Octane's listener set exists to clone selected repositories and
replace those manager application references; copying only `clone $app` would
reintroduce the same class of leaks.

### FACT — Octane has an explicit but non-traditional singleton contract

An adversarial PMS process probe produced:

```json
request_A: {"scoped":"resolved-before-first-request","lazy":null,"warm":null,"config":null}
request_B: {"scoped":null,"lazy":null,"warm":"alice","config":null}
```

Before the Hamster first-boundary fix, a scoped binding resolved during
bootstrap entered request A. The lifecycle adapter now calls
`forgetScopedInstances()` before READY, so the checked-in test requires request
A to see a new scoped value.

A singleton resolved only in a sandbox is discarded with that sandbox. A
singleton resolved in the base application (during bootstrap or through
`octane.warm`) is shared across requests. Octane's own binding tests explicitly
call the former fresh and the latter stale when either captures an application
or request reference.[^4] This means `singleton` alone does not specify a
uniform lifetime in a cloned-container runtime. Hamster accepts Octane's
contract because warm ownership is explicit before real traffic; it does not
claim traditional per-process singleton semantics.

### FACT — cleanup previously completed after Rust saw success

The old worker wrote `RESPONSE`, then ran kernel termination and reset. Rust
could return the generation capability to the pool while PHP was still cleaning
it. PHP still serialized execution because it had not read the next frame, but
a cleanup crash could occur after the generation was momentarily advertised as
reusable.

The new `HamsterOctaneClient` encodes a response into memory. It does not write
the frame until Octane termination, sandbox cleanup, database rollback, static
restoration and upload cleanup have completed. This keeps protocol v1 unchanged
while moving the visible response to the lifecycle commit point.

The adapter now implements that boundary as an explicit gate: `flushResponse()`
throws until the worker calls `markCleanupCompleted()`. An upload deletion error
retains ownership of the path, throws before that gate opens and poisons the
generation. It can no longer be suppressed and then forgotten after success was
already visible.

An HTTP fault injection made a `RequestTerminated` listener throw after the
handler produced a response. The first version staged a synthetic 500, which
revealed another lifecycle defect: Rust accepted the valid frame and briefly
marked the generation READY even though PHP immediately exited. The next
request was still at-most-once safe through `NotStarted`, but the advertised
state violated the cleanup-failure invariant.

The final worker leaves every staged frame invisible when Octane stops the
client. Observed sequence with one worker:

```text
handler side effect -> terminating listener throws
generation 1 exits without RESPONSE -> Rust returns HTTP 502
Rust marks generation 1 unavailable; no request replay
generation 2 boots
next request -> HTTP 200
```

The marker contained exactly one execution. No protocol change was required:
EOF after a written request already has possibly-started semantics in Rust.

### FACT — output buffering was not a protocol boundary

`ob_start()` captures normal PHP output but `fwrite(STDOUT, ...)` bypasses it.
Closing stdout before Laravel bootstrap was tested and rejected: a real PMS
worker failed before READY because Symfony Console could no longer construct
its `StreamOutput`.

The final adapter duplicates fd 1 into a private protocol stream before loading
Laravel. After bootstrap, but before READY, it closes `STDOUT` and occupies fd 1
with the platform null device. PHP's CLI wrapper duplicates the descriptor for
subsequent `php://stdout` opens, so request, termination and cleanup code can
only reopen the null sink while the private duplicate remains writable.[^6]
A checked-in subprocess probe verifies both direct paths and an end-to-end W1
probe served two `/up` requests on the same generation through protocol v1.

Bootstrap-time direct writes remain visible before stdout is redirected.
Ordinary output is buffered, and Rust rejects a direct write as an invalid
startup frame (or times out); request handling has not begun at that point.
After READY, code that writes the closed `STDOUT` constant receives a PHP
`TypeError`; it must use Laravel logging or stderr. A newly opened
`php://stdout` succeeds but points to the null sink.

## Ownership contract after the change

| State | Owner/lifetime | Boundary behavior |
|---|---|---|
| Bootstrapped providers, route graph, explicitly warm services | worker/base application | retained |
| Singleton resolved in base or listed in `octane.warm` | worker | retained; must not contain request/user state |
| Singleton first resolved in a sandbox | request sandbox | discarded with sandbox |
| Laravel scoped binding | request | forgotten before request one and after every operation |
| Transient binding | resolution/request | rebuilt normally |
| Request, auth guard, session state, queued cookies | request | Octane listener reset/rebind |
| Config and URL generator | request sandbox | cloned per operation |
| Framework managers | worker objects pointing at current sandbox | rewired by Octane listeners |
| Matched route controller | request | `Route::flushController()` after termination |
| Open DB transaction | external connection/request | rolled back before response commit |
| Worker-created upload temporary | request | removed before response commit; deletion failure poisons worker |
| Eloquent guard/strict switches, Carbon clock, cwd and timezone | process-global runtime/framework state | restored to boot baseline by Hamster |
| Userland globals, statics, runtime listeners/macros | process | unsupported for request-sensitive data |

The last row is a compatibility limit, not a solved isolation boundary. No
application clone can deep-copy arbitrary static variables, extension state,
open resources or objects reachable from package globals. Even constructing a
new Laravel `Application` in the same PHP process leaves PHP class statics
alive. Universal isolation for arbitrary Laravel code requires a fresh process,
which contradicts persistent bootstrap.

## Architectures tried and rejected

### Rejected: in-place targeted reset

It was fastest in the synthetic route, but it depended on private reflection,
warm snapshot contents and an ever-growing list of framework/package states.
Two falsifying counterexamples were enough to reject the ownership model. More
patches would increase apparent coverage without making the boundary complete.

### Rejected: raw application clone

It only duplicates the container object's arrays. Objects inside those arrays
remain shared, and many retain a pointer to the base application. It fails
configuration and manager isolation immediately.

### Rejected: fresh Laravel application for every request

It is the cleanest container boundary but reruns nearly all bootstrap work and
still cannot clear PHP statics. On the PMS copy it was about 18.5 times slower
than the full Octane lifecycle on the measured lightweight paths. It gives up
the objective of 0.4 without providing process-level isolation.

### Kept: first-party Octane lifecycle, independent transport adapter

Hamster does not use an Octane server or wire format. It uses Octane's
`ApplicationFactory`, `Worker`, events and listeners, and supplies its own
`Client` for protocol v1. This avoids maintaining a fork of Laravel-specific
manager rewiring. FrankenPHP's official Laravel worker also constructs the
first-party Octane `Worker`, while the maintained RoadRunner Laravel bridge
declares `laravel/octane` as a dependency; the ecosystem alternatives therefore
do not provide a demonstrably simpler independent Laravel lifecycle.[^5]

## Performance evidence

These are diagnostic in-process measurements, not release claims. Each request
constructed an Illuminate request, dispatched a real PMS Laravel kernel route,
read the response and ran the complete candidate lifecycle. PHP was 8.5.10 NTS
with `mbstring`; the machine was otherwise uncontrolled. IPC and Rust HTTP cost
were excluded.

| Candidate | Work/run | mean, three runs | representative p50 | representative p95 | peak allocated |
|---|---:|---:|---:|---:|---:|
| old targeted reset | 5,000 | 438, 446, 441 us | 403 us | 649 us | 26 MiB |
| Octane sandbox | 5,000 | 749, 760, 735 us | 682 us | 1,076 us | 28 MiB |
| fresh application | 100 | 14.57, 13.87, 13.66 ms | 12.39 ms | 16.23 ms | 36 MiB |

FACT: Octane added about 0.31 ms, or roughly 70%, versus the invalid reset on
this deliberately lightweight route. FACT: fresh application bootstrap was
about 13.9 ms/request and was an order of magnitude worse.

INFERENCE: the sandbox cost is material for the lightest endpoints but is the
only measured candidate that preserves most bootstrap benefit and has a
maintained lifecycle model. The previous PMS multipliers are stale and must not
be reused.

HYPOTHESIS: removing individual Octane listeners could recover part of the
0.31 ms, but there is no evidence they are redundant for the supported Laravel
and package matrix. Doing so now would recreate targeted reset under another
name.

The descriptor isolation, commit-gate boolean and cwd/timezone restoration
were added after these measurements. Their expected per-request work is small,
but that is an inference; the table is not a measurement of the exact final
file set.

## Tests that now protect the boundary

- `laravel_lifecycle_test.php` runs all ownership probes through one real
  Octane worker: bootstrap-resolved scoped state, scoped request separation,
  lazy and warm singleton behavior, config cloning, cached controller reuse,
  deferred-provider loading, listener-order rejection, Eloquent/Carbon/cwd/timezone
  restoration, transaction rollback and injected termination failure. On PMS it also
  exercises `tymon.jwt.parser`, a warmed package singleton which stores a
  Request reference: A's bearer token is present for A and absent for B because
  the package's request rebinding callback is preserved by the sandbox.
- `laravel_octane_client_test.php` proves that `respond()` emits zero bytes,
  an early flush is rejected without output, lifecycle success commits exactly
  one frame, and a stopped client leaves its staged response invisible.
- `laravel_request_test.php` reconstructs a real Request stub and protects
  cookie, urlencoded, multipart, PHP field-name, false-boundary and missing
  closing-boundary behavior. It fault-injects upload deletion failure and
  requires ownership to be retained for a later retry.
- `laravel_stdout_test.php` proves the protocol duplicate survives while both
  `STDOUT` and a newly opened `php://stdout` are disconnected from the pipe.
- The real-app Rust protocol test boots one worker and requires two `/up`
  requests to succeed without changing PID or generation. A second W1 PMS test
  runs 50 authenticated/anonymous pairs, verifies a fresh route controller,
  injects cleanup failure and requires exactly-once execution plus generation
  replacement.
- Existing Rust partial-write and NotStarted tests continue to protect
  at-most-once semantics. Protocol v1 did not change.

The real lifecycle test is ignored unless
`HAMSTER_LARAVEL_TEST_PATH` identifies an Octane-enabled Laravel application.
That prerequisite is intentional: a stubbed container cannot establish Laravel
ownership semantics.

## Remaining blockers and compatibility limits

FACT: the original supplied PMS still does not declare `laravel/octane` and was
not modified. A no-hardlink scratch clone installed the actual Octane 2.19.1
package, ran package discovery and `artisan optimize`, and passed the 25-check
in-process lifecycle suite plus the final Rust/protocol W1 test. No Octane
source injection was used.

FACT: Composer required `--ignore-platform-req=php` for that scratch install,
not because of Octane, but because PMS pins PhpSpreadsheet 1.30.6 with PHP
`>=7.4.0 <8.5.0`. This prevents an honest clean PHP 8.5 dependency install
until the PMS dependency is updated or explicitly accepted by its owner.

FACT: a deliberately warmed mutable singleton leaked `alice` to request B under
Octane. That is worker-owned state by contract. Applications must put
request/user data in scoped bindings or list a legacy singleton in
`octane.flush`. Userland statics, runtime event listeners and macros have the
same long-lived compatibility risk.

FACT: the Laravel request reports `REMOTE_ADDR=127.0.0.1`. `TcpListener::accept`
already yields the immediate peer address, but `server.rs` discards it before
routing. PMS contains rate-limit logic that uses `$request->ip()`.

INFERENCE: a framing change is not strictly required. Rust could attach the
peer to the Hyper request and serialize it as a reserved, server-owned
pseudo-header inside the existing header list; it must reject or overwrite the
same client header, and PHP must consume it without exposing it to Laravel.
This preserves protocol-v1 bytes but extends its semantics with an untyped
metadata convention. A protocol-v2 peer metadata field is clearer, typed and
extensible, at the cost of negotiation/deployment work. An inbound
`X-Forwarded-For` alone is not an answer: Symfony/Laravel evaluates forwarded
addresses only relative to a known immediate peer and configured trusted proxy
set.[^7]

HYPOTHESIS: if 0.4's compatibility target includes correct application IP rate
limiting, the reserved v1 metadata convention is the smallest viable change.
The owner explicitly reserved this decision, so neither option was implemented.

FACT: `StreamedResponse` and `BinaryFileResponse` are captured into the single
buffered response frame. This supports response semantics but is not transport
streaming. The 1 MiB HTTP body and 8 MiB frame limits remain.

FACT: no native Linux run was performed. The old headline multipliers and old
reset timing are not evidence for this implementation.

### Real PMS end-to-end follow-up

FACT: a release W4/C16 comparison used the optimized scratch PMS, the same Rust
HTTP path, protocol-v1 IPC, worker count, concurrency, routes and response
consumption for both candidates. The optimistic `fresh` candidate rebuilt a
Laravel application for every request while leaving PHP and OPcache alive.
Medians of three fresh server processes were:

| Workload | fresh RPS | Octane RPS | gain | fresh p50 | Octane p50 |
|---|---:|---:|---:|---:|---:|
| controller JSON | 751.1 | 1137.0 | 1.51x | 20.609 ms | 13.209 ms |
| Blade login | 623.1 | 931.3 | 1.49x | 24.637 ms | 15.832 ms |
| authenticated dashboard, W1/C1 | 3.28 | 3.54 | 1.08x | 301.828 ms | 273.423 ms |

FACT: persistent run ranges were 1044.2–1150.4 RPS for the controller and
865.2–976.5 for login. They did not overlap the fresh ranges, but the spread is
large enough that reporting extra multiplier precision would be false. The
remote-MySQL dashboard had still larger tail variance.

INFERENCE: correct Octane lifecycle preserves a meaningful but much smaller
bootstrap advantage than the rejected reset. On the heavy dashboard, the
database/render workload dominates Hamster and the benefit is modest. The old
3.3x, 7.6x and 1.31x claims are rejected for the final architecture.

FACT: a release W1 soak ran 500 warm-up pairs and 10,000 measured
authenticated/anonymous pairs. All 20,000 responses were correct on one PID.
RSS was constant at 70,459,392 bytes for every sample from request 8,000 through
20,000, producing a 0 B/request least-squares slope over the second half. A
post-soak cleanup fault returned 502, executed its marker once, booted a new
PID, served 200, and left no measured worker orphan.

INFERENCE: this falsifies continuing growth on the tested controller,
authentication, session and Octane lifecycle path. It does not prove arbitrary
PMS routes or packages leak-free and does not overturn the reproduced cached
closure-route limitation.

## Final regression and pool hardening

FACT: the complete Windows ignored matrix was rerun in the test harness's
default parallel mode after the Octane replacement. It passed 55 tests: eight
PHP pool unit tests and 47 process/HTTP integration tests. The matrix includes
the 100,000-request pool soak, 10,000-request forced-failure soak, 60-second W4
random-kill stress, cancellation, queue saturation, shutdown at each lifecycle
boundary, NotStarted/possibly-started behavior, protocol corruption, recovery,
recycling, environment propagation, stdout isolation and the real Laravel/PMS
paths.

FACT: the first full run found two deterministic Laravel test failures. Both
external-app tests invoked the repository PHP binary without the scratch PMS
`php.ini`, so `mbstring` was absent; one failed bootstrap at `mb_split` and the
direct lifecycle test emitted no success report. Both passed when `PHPRC` was
provided. The harness now derives `PHPRC` from
`HAMSTER_LARAVEL_TEST_PHP_INI` or `<app>/php.ini` for every external Laravel
test. The same two tests pass with the parent shell's `PHPRC` explicitly
removed, and the complete matrix then passes.

INFERENCE: those failures were a test-environment regression introduced by the
new external Octane tests, not a timing race or product failure. Depending on a
developer shell's PHP configuration had allowed focused runs to create false
confidence.

FACT: a new real-PMS W4 test forces one delayed controller request onto each
worker and requires `controller_calls=1` on all four PIDs across repeated
waves. A post-response cleanup fault executes once and returns 502. Exactly one
stable slot advances by one generation; the other three retain PID and
generation, and a final four-worker wave remains isolated. Shutdown leaves no
project PHP process.

INFERENCE: the Octane failure boundary composes correctly with the accepted
0.2/0.3 pool supervisor for the tested multi-worker case; cleanup failure is
slot-local rather than pool-wide.

## Verdict

INFERENCE: Octane plus a Hamster resource boundary is a substantially better
0.4 architecture than the reset snapshot. It defines ownership before traffic,
uses maintained first-party lifecycle knowledge, keeps Rust framework-agnostic,
keeps protocol v1, and fails closed on missing, reordered or prematurely
preceded lifecycle listeners and on cleanup failure.

The lifecycle architecture now has credible end-to-end evidence on the actual
optimized PMS dependency graph: W1 security isolation, controller ownership,
failure commit ordering, at-most-once behavior, recovery, stable tested-path
memory and a release performance advantage all stood up to adversarial tests.

INFERENCE: Octane plus the narrow Hamster boundary is technically suitable for
0.4's lifecycle goal. It is not universal PHP-process isolation; the warm
singleton/static contract must be explicit compatibility policy.

This review does **not** close 0.4. Owner decisions remain for the original PMS
dependency change / PhpSpreadsheet PHP 8.5 conflict and whether correct peer IP
metadata is a 0.4 release requirement. Native Linux remains unverified.

## Sources

[^1]: [Laravel Octane 2.19.1 `Worker.php`](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/Worker.php)
[^2]: [Laravel Octane 2.19.1 `ApplicationFactory.php`](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/ApplicationFactory.php)
[^3]: [Laravel Octane 2.19.1 default lifecycle options](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/src/Concerns/ProvidesDefaultConfigurationOptions.php)
[^4]: [Laravel Octane 2.19.1 request/controller state tests](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/tests/RequestStateTest.php) and [binding-state tests](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/tests/BindingStateTest.php)
[^5]: [FrankenPHP's Laravel Octane worker entry point](https://github.com/laravel/octane/blob/68a2516a0318baba0de0e4648f61e335c5e69dca/bin/frankenphp-worker.php) and [RoadRunner PHP Laravel bridge package manifest](https://github.com/roadrunner-php/laravel-bridge/blob/78d0b8a9a50fe7695b8e17a54d865fade6fcd1de/composer.json)
[^6]: [PHP CLI `php://stdout` descriptor duplication](https://github.com/php/php-src/blob/PHP-8.5/ext/standard/php_fopen_wrapper.c)
[^7]: [Laravel request IP and trusted proxy configuration](https://laravel.com/framework/docs/12.x/requests#configuring-trusted-proxies) and [Symfony trusted proxy model](https://symfony.com/doc/current/deployment/proxies.html)
