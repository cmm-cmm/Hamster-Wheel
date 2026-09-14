# Laravel Persistent Worker 0.4

Status: research complete, implementation landed, not released. This records
what was measured, what was rejected, and what is still unknown.

Measured on Windows `x86_64-pc-windows-msvc`, PHP 8.5.10 NTS with OPcache, release
build, against two applications:

- **app12** — a Laravel 12.69.2 skeleton with `artisan optimize` applied
  (config, route, event and view caches). Minimal: almost nothing to bootstrap.
- **PMS** — a real Laravel 12.66.0 application: 305 routes, custom middleware,
  JWT auth, Redis sessions and cache, remote MySQL, dompdf, PhpSpreadsheet. No
  config cache. Supplied locally under the git-ignored `www/`.

## The question

Does keeping the Laravel application alive across requests pay for the
complexity it introduces? The answer is yes, but the size of the win depends
entirely on how much real work a request does, and the complexity is not
optional: a persistent worker without a reset leaks the authenticated user.

## Architecture

All Laravel knowledge lives in PHP. The server learned exactly one generic
thing: how to pass environment variables to a worker process.

```
Hamster dispatch -> worker reads a REQUEST frame
  -> rebuild Illuminate\Http\Request from the frame
  -> $kernel->handle()
  -> write the RESPONSE frame          <- the client is answered here
  -> $kernel->terminate()
  -> reset                             <- ~0.018 ms
  -> publish idle
```

The reset runs after the response is written, so no client waits for cleanup.
A generation owns its worker exclusively, so the reset always completes before
the next request can be assigned and can never overlap one.

Protocol v1 is unchanged: no new frame kinds, no new limits. The 0.3 lifecycle
(generations, backoff and cooldown, at-most-once, shutdown ordering) is reused
as-is; `max_requests` and `max_worker_age` remain the safety net for leaks that
live in application code.

Files:

- `fixtures/php-worker/laravel_worker.php` — the worker loop.
- `fixtures/php-worker/laravel_reset.php` — the request-boundary reset.
- `fixtures/php-worker/env_worker.php` — reports its environment, for tests.

Configuration, entirely through `php.env`:

```toml
[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/laravel_worker.php"
workers = 4

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = "www/pms"
HAMSTER_LARAVEL_RESET = "targeted"
```

`HAMSTER_LARAVEL_RESET` accepts `targeted` (default) or `none`. `none` exists
only so the cost of the reset can be measured against its value; it leaks
authenticated identity and must never serve real traffic. See the measurement
below.

## Why a reset is mandatory

Thirteen adversarial probes over the request lifecycle, each run in its own
process so a leak found by one cannot poison another's baseline. Eight more
probes covering Eloquent statics, managers, macros and URL state are in *The
second probe set* below. Counts are probes that leaked:

| Strategy | Leaks | Notes |
|---|---|---|
| no reset | 11 / 13 | auth, session, DB transaction, locale, container bindings, listeners, `View::share`, config, `Carbon::setTestNow`, `request->user()`, userland statics |
| restore the container's arrays wholesale | 10 / 13 | fixes runtime container bindings and nothing else |
| `laravel_reset.php` | 1 / 13 | only userland statics, which no server can fix |

Both figures were re-measured after the 0.4 reset additions, so they describe
the module as it ships.


The decisive demonstration is on PMS, with `workers = 1` so that consecutive
requests provably reach the same process:

1. log in as a real user; `GET /` returns the 2,223,437-byte dashboard;
2. a second client sends `GET /` **with no cookie at all**.

With `HAMSTER_LARAVEL_RESET=none` that anonymous client receives the same
2,223,437-byte authenticated dashboard. With `targeted` it receives `302` to
`/login`. This is a security failure, not a performance one, and it is why the
reset is not configurable away in production.

A four-worker pool hides this: an anonymous request lands on the leaking worker
only about one time in four. Any future isolation test must pin `workers = 1`.

## What the reset does, and what it deliberately does not

Restoring the container's `instances` table wholesale was evaluated and
rejected. It is neither sufficient nor safe:

- objects shared through it by reference (the config repository, the event
  dispatcher, database connections, the auth manager) keep their own state, so
  ten of thirteen probes still leaked;
- restoring `instances` without also restoring `reboundCallbacks` leaves a
  stale callback pointing at a binding that was just removed, which produced an
  unbounded `url -> router -> routes -> url` resolution loop that exhausted a
  512 MB limit within two requests.

Laravel already re-points the url generator at the current request through
`rebinding('request')`. An early probe reported a leak there; the probe was
wrong, because `url()->current()` drops the query string by design. Nothing in
the reset duplicates what the framework already handles.

The reset has eleven steps: roll back transactions and restore per-connection
callbacks; clear auth guards, session drivers and queued cookies; forget the
request-scoped bindings; prune container instances the request bound, along
with their resolved flags; restore configuration and locale; restore shared
view data; restore event listeners; restore rebound and terminating callbacks;
reset `Carbon::setTestNow` and `Str`'s cache; restore Eloquent's guard and
strict-mode switches; and restore the url generator's forced origin, forced
scheme and route defaults. Each of the last two groups compares before it
writes, so the common case is a handful of boolean and identity reads.

The snapshot the reset restores to must be taken from a **warm** worker.
Deferred providers, the view engine and the url generator's request rebinding
are all resolved lazily by the first request, so a snapshot taken before that
records a state the worker never returns to. `HAMSTER_LARAVEL_WARMUP` is
clamped to at least one for this reason.

Bootstrapping is left to the first warm-up request rather than called eagerly.
`public/index.php` binds the request and bootstraps inside `handle()`; running
every service provider with no request bound is a different order, and PMS
fails outright under it with `Target class [request] does not exist`.

Per-step cost, 400 iterations, measured on both applications:

| Step | app12 | PMS |
|---|---|---|
| roll back transactions, flush query log | 0.0009 ms | 0.0009 ms |
| clear auth guards, session drivers, queued cookies | 0.0010 ms | 0.0006 ms |
| prune request-bound container instances | 0.0016 ms | 0.0011 ms |
| restore configuration | 0.0055 ms | 0.0042 ms |
| restore locale | 0.0111 ms | 0.0102 ms |
| restore shared view data, listeners, bookkeeping | 0.0003 ms | 0.0003 ms |
| framework statics | 0.0006 ms | 0.0007 ms |
| **sum of the steps** | **0.021 ms** | **0.018 ms** |

**Those are per-step timings summed, not the cost of one call.** Timing the
whole closure after a real request, which is the number that belongs in a
performance claim, gives a larger figure because it includes the reflection
reads and the loop overhead the per-step table omits:

| Reset, measured end to end after a real request | p50 | p95 |
|---|---|---|
| app12 `/c/json`, n=800 | 0.0524 ms | 0.0848 ms |
| PMS `/login`, n=400 | 0.0482 ms | 0.0865 ms |

Both figures are for the full reset including steps 10 and 11. Against the same
build with those steps removed the reset measures 0.0492 ms and 0.0442 ms at
p50, so the Eloquent and url-generator additions cost about **0.003–0.004 ms**
per request, roughly 0.3 percent of a 1.1 ms request. An end-to-end throughput
A/B could not resolve that difference at all: run-to-run variation within a
single server instance reached 23 percent, which is far larger than the effect.

The reset is still cheap. The measurable cost of correctness is elsewhere:
rebuilding what the reset discarded costs roughly 0.25–0.46 ms on a session
-using route, and that is the true price of per-request session isolation. A
per-request model pays it too.

## Performance

Four workers, concurrency 16, requests per second.

| Application | Route | cold | persistent + reset | gain |
|---|---|---|---|---|
| PMS | `/up` | 347 | 6,721 | 19.4x |
| PMS | `/login` (19 KB Blade) | 155 | 1,678 | 10.8x |
| PMS | authenticated `/` (2.2 MB, remote MySQL, sequential) | 3.2 | 4.2 | 1.31x |
| app12 | `/c/json` | 1,628 | 3,781 | 2.32x |
| app12 | `/c/db` | 1,179 | 2,957 | 2.51x |
| app12 | `/c/view` | 1,500 | 3,192 | 2.13x |

`cold` is not `artisan serve`. It rebuilds the application per request **inside
the same worker, through the same IPC path**, with the process alive and the
OPcache warm. That is the best case a well-configured PHP-FPM deployment can
reach, so these are conservative numbers.

The pattern is the point: **the gain is inversely proportional to how much real
work a request does.** Bootstrap is a fixed cost of roughly 10 ms on PMS. On a
trivial route that is the entire request; on a 2.2 MB dashboard issuing many
remote database queries it is 3 percent of it. Both numbers are real and a
deployment will see the whole range.

The `none` column is faster than `targeted` but is not a legitimate baseline:
it is faster because it is wrong.

### A production-tuned application: `artisan optimize` measured

The table above was measured against a PMS with no configuration or route cache,
which flattered the persistent column. That gap has now been measured directly.

The supplied application was **not modified**. It was copied to a scratch
directory and `artisan optimize` was run against the copy, so nothing was
written into `www/pms`; its `bootstrap/cache` still holds only the two files it
held before this work.

Medians of three runs, four workers, concurrency 16:

| Route | tuned | cold | persistent | gain |
|---|---|---|---|---|
| `/up` | no | 337 | 6,804 | 20.2x |
| `/up` | **yes** | **1,618** | **5,359** | **3.3x** |
| `/login` | no | 158 | 1,703 | 10.8x |
| `/login` | **yes** | **204** | **1,552** | **7.6x** |

**Most of the twenty-fold figure was PMS being untuned.** On a trivial route a
production-tuned application keeps a 3.3x advantage from persistence, not 20x.
On the Blade login page it keeps 7.6x, because config caching removes far less
of that request's work. This is the honest headline: persistence is worth
somewhere between 1.3x and 8x on a tuned application depending on the route,
and the 19x number should not be quoted.

### `artisan optimize` can make a persistent worker slower

The tuned persistent column is *lower* than the untuned one, 5,359 against
6,804 on `/up`. That is not noise, and the cause is the cached-closure-route
leak documented below meeting Laravel's own code.

`withRouting(health: '/up')` registers the health endpoint as a **closure**
route. With the route cache on, every `/up` dispatch unserializes that closure
again. Measured on one worker, 30,003 requests to `/up`:

| PMS build | rps | worker RSS before | after | growth |
|---|---|---|---|---|
| no route cache | 2,209 | 61.92 MB | 61.92 MB | **0 B/request** |
| `artisan optimize` | 1,773 | 64.16 MB | 74.97 MB | **+377.9 B/request** |

So on a persistent worker `artisan optimize` cost 20 percent of the throughput
of that route and introduced a 378 byte per request leak, on a route the
application author never wrote. This is worth knowing before treating
`artisan optimize` as unconditionally correct for a persistent deployment: it is
right for everything except closure routes, and Laravel ships one.

For scale: the trivial 0.3 worker reaches 5,482 rps at concurrency 1 and 35,169
at 16 on the same machine. Laravel, not Hamster's transport, is the limit;
optimizing the Rust side for this milestone would be wasted effort.

## Memory

`memory_get_usage` slopes are least-squares fits over the second half of a soak.
Two-point slopes are unusable here because PHP's cycle collector makes the
series saw-tooth; an early two-point estimate reported a 130 B/request leak that
regression over 100,000 requests showed to be zero.

| Soak | Slope |
|---|---|
| app12, controller routes, no reset, 100,000 requests | +0.0 B/request |
| app12, controller routes, `targeted`, 100,000 requests | -18.0 B/request |
| app12 over HTTP, 237,477 requests, four routes | worker RSS pinned at 55.1 MB |

Laravel itself does not leak on a persistent worker, with one exception.

### Cached closure routes leak, and this is not Hamster's bug

| Route shape | route cache on | route cache off |
|---|---|---|
| closure `/ping` | +786 B/request | 0 |
| closure `/json` | +1,688 B/request | 0 |
| controller `/c/ping` | 0 | 0 |
| controller `/c/json` | 0 | 0 |

Mechanism, confirmed in the framework source rather than inferred:
`Route::runCallable()` calls `unserialize()` on the action **on every dispatch**
when the route is a cached closure, and
`Laravel\SerializableClosure\Serializers\Native::__unserialize()` reconstructs
it with `include 'laravel-serializable-closure://<the closure source>'`. Every
such `include` compiles a new op_array that PHP does not free until the process
exits, which is why the leak is proportional to the closure's source length and
why `gc_collect_cycles()` makes no difference.

This affects every persistent PHP runtime, not only Hamster. The rule for an
application served this way is: **do not combine closure routes with route
caching.** PMS has one closure route out of 305.

## Stability and failure behaviour

- 237,477 requests over HTTP across four route shapes: worker RSS pinned at
  55.1 MB, server RSS 9.1–9.6 MB, zero timeouts, zero worker failures, zero
  respawns.
- Killing two of four workers mid-load produced exactly two failed requests,
  two respawns, and full throughput restored (3,586 -> 3,865 rps).
- At-most-once holds with real Laravel: six requests, four of them killed while
  PHP was provably executing, produced four 502s and two 200s, and the execution
  marker file contained four unique lines with zero duplicates.
- Bootstrap to READY: 58 ms median for app12, 71 ms for PMS, worst observed 403
  ms. `STARTUP_TIMEOUT` is 5,000 ms. The margin is comfortable but it scales
  with application size and is the constraint to watch.

## Standard output is part of the protocol

Anything a worker prints to stdout is read as a frame. Two real failures were
caused by this before the worker was hardened:

- PHP 8.5 emits `Deprecated:` notices for real application code. PMS triggers
  three during configuration load. Fixed with `display_errors = stderr` at the
  top of the worker, before anything is required.
- A bootstrap exception renders to stdout by default. Fixed by running warm-up
  inside an output buffer and failing with the captured text on stderr.

The request loop also buffers output and reports any stray bytes to stderr
rather than letting them corrupt the stream.

## Request reconstruction

Two defects found by testing against a real application, both the same class of
mistake:

- `Request::create()` builds its cookie bag from its own argument and never
  parses `HTTP_COOKIE`. Passing the header alone gives Laravel a request with no
  cookies: no session, no authenticated user, every POST rejected as a CSRF
  failure.
- `Request::create()` is a test helper that takes form parameters as an
  argument and ignores the body. A POST arrived with an empty request bag, so
  the PMS login returned `419` even with cookies working.

The worker now constructs `Illuminate\Http\Request` directly, parses the
`Cookie` header itself, and parses the body itself, which is what PHP would have
done under a real SAPI. That work lives in
`fixtures/php-worker/laravel_request.php` and is covered by
`fixtures/php-worker/laravel_request_test.php`, a self-checking script the
ignored Rust test `laravel_request_reconstruction_passes_its_own_checks` runs.
The repository deliberately does not vendor a Laravel application, so the test
declares a stub `UploadedFile` and exercises the framework-free half: cookie
parsing, multipart splitting, PHP-style field names and the
`Content-Disposition` grammar. Twenty-six checks.

A body the worker cannot parse is answered with `400` and the worker keeps
serving. Losing a booted application to a malformed client request would be a
denial of service against every other client on that worker.

## Uploads and multipart bodies

PHP parses `multipart/form-data` in the SAPI layer and hands the result to the
application in `$_FILES`. A persistent worker never goes through that layer, so
the worker does it: it splits the body on the boundary, puts field parts into
the request bag under PHP's own `a[b][]` naming rules, writes each file part to
a temporary file, and builds `Illuminate\Http\UploadedFile` objects for the
file bag.

Every upload is constructed in **test mode**. That is not a shortcut. A normal
`UploadedFile` answers `isValid()` with `is_uploaded_file()`, which is true only
for temporaries the SAPI itself created; without test mode Laravel's `file`,
`image` and `mimes` validation rules reject every upload a persistent worker can
ever produce.

The worker also deletes its own temporaries after each request. Laravel's
`store()` copies the upload rather than moving it and relies on the SAPI to
clean up at request shutdown, and a persistent worker has no request shutdown.
Measured: 600 uploads through `/u/store` and `/u/inspect` left **zero** files
behind in the temporary directory.

Verified end to end against a Laravel 12 application over HTTP, with a client
that writes the multipart bytes itself rather than through a client library:

| Case | Result |
|---|---|
| binary file, 4,096 random bytes | SHA-1 matches the client's exactly |
| two files under `docs[]` | both present, both byte-exact, order preserved |
| CRLF and NUL bytes inside file content | preserved |
| nested fields `meta[a]`, `meta[b][c]` | arrive as nested arrays |
| `required|file|max:512` validation | passes |
| `store('uploads')` | file written, SHA-1 preserved |
| empty file input (`filename=""`) | `UPLOAD_ERR_NO_FILE`, not an empty upload |
| `application/x-www-form-urlencoded` | unchanged |
| `multipart/form-data` with no boundary | `400`, worker still serving afterwards |

**The 1 MiB protocol body limit applies.** Protocol v1 caps a request body at
1 MiB and this milestone does not own the protocol, so a larger upload is
rejected by the server with `413` before the worker is involved. Raising it is a
protocol change and belongs to a milestone that owns the protocol.

Not covered: `multipart/mixed`, `Content-Transfer-Encoding` other than binary,
and RFC 5987 `filename*`. A `filename*` parameter is deliberately not read as
`filename`, so such a part is treated as a field rather than silently given the
wrong name.

## Response shapes

`Response::getContent()` returns nothing useful for a streamed or file-backed
response; those bodies exist only while `sendContent()` runs. The worker
captures `sendContent()` output for `StreamedResponse` and `BinaryFileResponse`
rather than reading the file itself, because `Router::prepareResponse()` has
already applied any `Range` header and `sendContent()` sends exactly the range
it computed.

Verified: `response()->download()` returns the file byte-exact with its
`Content-Disposition`, and `response()->stream()` returns the callback's output.
`Content-Length` and `Transfer-Encoding` are stripped as always; the server
writes its own.

## The second probe set

The first thirteen probes covered the request lifecycle. Eight more cover what
they did not: Eloquent's class-level statics, the queue and broadcast managers,
forced URL origins, runtime macros, and connection-level callbacks. All were run
against a **one worker** pool, because a four worker pool hides a leak behind
the dispatcher.

| Probe | no reset | `targeted` |
|---|---|---|
| `Model::unguard()` | leaks | isolated |
| `Model::preventLazyLoading()` | leaks | isolated |
| `URL::forceRootUrl()` | leaks | isolated |
| queue manager connections | leaks | isolated |
| `Model::addGlobalScope()` | leaks | **leaks, by design** |
| `Request::macro()` | leaks | **leaks, by design** |
| `Builder::macro()` | leaks | **leaks, by design** |
| `DB::beforeExecuting()` | leaks | **one request, then stable** |

`Model::unguard()` matters for the same reason the identity leak does: an
application that unguards mass assignment and never restores it leaves every
later request on that worker able to fill any column, including requests from
other users.

### Why global scopes are not restored

The obvious fix is to snapshot `Model::$globalScopes` and put it back. That is
wrong, and the failure is silent and severe.

`SoftDeletes::bootSoftDeletes()` installs the soft-delete scope, and
`Model::bootIfNotBooted()` runs a class's boot methods exactly once per process,
gated by `Model::$booted`. A model first used during request N therefore
installs its scopes during request N, after the warm snapshot was taken.
Restoring that snapshot deletes them, and `$booted` still says the class is
booted, so **they are never reinstalled**.

Measured, not inferred. A probe that boots a `SoftDeletes` model, restores an
empty `$globalScopes`, and re-instantiates the model reports:

```json
{"scopes_before":1,"booted_before":true,"scopes_after_restore":0,
 "booted_after_restore":true,"soft_delete_filter_lost":true}
```

Soft-deleted rows would become visible to every later request on that worker.
Restoring global scopes is only safe if every model class is re-booted each
request, which is precisely the cost persistence exists to avoid. So the reset
leaves them alone, and the rule for an application is: **`addGlobalScope`
belongs in a model's `booted()`, never in a controller or middleware.**

### Why macros are not restored

A macro registered during a request leaks. Restoring them would mean
snapshotting `$macros` on an open-ended set of `Macroable` classes, and the set
cannot be enumerated, so the coverage would be arbitrary. Macros are a
boot-time API; registering one from a request is the same category of mistake
as keeping request state in a userland static, and is documented as such rather
than half-fixed.

### The snapshot must be the last thing that touches the container

Adding the url-generator snapshot introduced a bug worth recording, because it
would have been almost impossible to find from a bug report.

`$app['url']` had not been resolved yet, so reading it resolved it, and
resolving it registered the url generator's `rebinding('request')` callback.
The `reboundCallbacks` snapshot had already been taken one line earlier, so the
reset restored a table without that callback and deleted it permanently. From
then on every request generated links for the *previous* request's URL.

The fix is ordering: every binding the snapshot needs is resolved first, and
`reboundCallbacks` is captured last. The probe that caught it is the `url`
probe, which asserts that two different requests produce two different URLs --
an inverted assertion, where sameness is the failure.

## Known limitations

- Request bodies, uploads included, are capped at the protocol's 1 MiB. A
  larger upload is `413` before the worker sees it. Raising the cap is a
  protocol change and this milestone does not own the protocol.
- Userland static properties leak and no server can prevent it. Application code
  must not hold request state in statics.
- `Model::addGlobalScope()` called during a request leaks, deliberately; see
  the soft-delete demonstration above.
- Macros registered during a request leak, deliberately. Macros are a boot-time
  API.
- `DB::beforeExecuting()` on a connection first opened during a request leaks
  for exactly that one request and is stable afterwards. The reset baselines a
  connection the first time it sees one, because whatever opened the connection
  had a legitimate reason to register callbacks on it, and wiping them would be
  a worse failure than one leaked callback.
- The flush list was derived from twenty-one probes plus one real application.
  It is not proven exhaustive; a larger dependency set may hold state nobody
  probed.
- Not exercised: queue workers and broadcasting beyond manager-level state,
  session drivers other than Redis and array, `multipart/mixed`, non-binary
  `Content-Transfer-Encoding`, RFC 5987 `filename*`, third-party middleware
  beyond what PMS uses, and PMS's own import endpoints, which were left alone
  because exercising them writes to the owner's database.
- Each worker holds its own OPcache copy of the application: 55–68 MB resident
  per worker. Eight workers is roughly half a gigabyte of PHP.
- Linux remains compile-checked only.

## Rejected

- Restoring the container wholesale, for the reasons above.
- Restoring `Model::$globalScopes`, because `Model::$booted` makes the deletion
  permanent and silently unfilters soft deletes.
- Restoring macros, because the set of `Macroable` classes cannot be
  enumerated, so the coverage would be arbitrary and the gap invisible.
- Resetting an unknown database connection's `beforeExecuting` callbacks to
  empty. Safe-looking, but it would silently remove instrumentation a package
  registered when it opened the connection.
- Cloning the application per request. Not measured; the in-place reset already
  costs 0.018 ms, so there is nothing for a clone to win, and changing the
  application's object identity would strand references held by objects built
  inside the sandbox.
- Any Laravel-specific knowledge in Rust. The server passes an environment map
  and knows nothing else, which is what kept the Rust change to one field.
