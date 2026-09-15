# Laravel Persistent Worker 0.4

Status: investigation and implementation committed at `6e961d5`; milestone
closure, tag and release remain owner decisions.

The current worker uses Laravel Octane for application/container lifecycle and
keeps Hamster's Rust pool and protocol framework-agnostic. The previous
reflection-based targeted reset was removed after direct probes showed that its
warm snapshot did not represent Laravel ownership: a bootstrap-resolved scoped
binding survived while a lazy singleton was discarded, and cached controller
state lived outside the container instance table.

The independent investigation, rejected prototypes, measurements and verdict
are in [research/LARAVEL_LIFECYCLE_0_4_REVIEW.md](research/LARAVEL_LIFECYCLE_0_4_REVIEW.md).

## Application requirement

The served application must install a compatible Laravel Octane 2.x package:

```text
composer require laravel/octane:^2
```

Hamster uses Octane's `ApplicationFactory`, `Worker`, events and default
listeners. It does not start RoadRunner, Swoole or FrankenPHP and does not use
their transport. An application without Octane, or one that removes a required
default isolation listener, fails before READY.

Example:

```toml
[[hosts.locations]]
path = "/"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/laravel_worker.php"
workers = 4
queue_capacity = 32

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = "www/pms"
```

`HAMSTER_LARAVEL_RESET=none` and `targeted` are rejected. The optional legacy
variable is accepted only when its value is `octane`; omitting it is preferred.
`HAMSTER_LARAVEL_WARMUP` no longer exists. Configure deterministic worker-owned
services with Octane's `octane.warm` list and request-rebuilt legacy singletons
with `octane.flush`.

## Request lifecycle

```text
boot Laravel + load deferred providers + warm configured services
    -> clear bootstrap-resolved scopes
    -> redirect application stdout away from the protocol pipe
    -> READY

decode protocol request + reconstruct Illuminate Request
    -> clone Octane application sandbox
    -> reset/rebind framework request state
    -> kernel handle
    -> stage response (no protocol bytes yet)
    -> kernel terminate + Octane operation cleanup
    -> flush cached route controller + sandbox
    -> rollback abandoned DB transactions
    -> restore security-relevant framework statics
    -> delete worker upload temporaries
    -> write the one protocol-v1 RESPONSE frame
```

The response is deliberately committed after cleanup. Any uncaught handling,
termination, sandbox or Hamster cleanup error makes Octane retire the client;
the staged response is left invisible and the PHP process exits. Rust observes
EOF, returns 502, classifies the already-written request as possibly started
and never replays it. It marks the generation unavailable immediately instead
of briefly advertising a dead process as READY after accepting a synthetic 500.

The worker duplicates stdout for its private frame writer, then redirects fd 1
to the null device after Laravel boot and before READY. This is stronger than
output buffering: request and cleanup code using either `STDOUT` or a newly
opened `php://stdout` cannot contaminate protocol frames. Bootstrap still needs
stdout for Symfony Console; ordinary bootstrap output is buffered and any
direct write is rejected by Rust's READY decoder.

After READY, direct writes to the closed `STDOUT` constant raise `TypeError`;
application code must use Laravel logging or stderr. Opening `php://stdout`
again yields the null sink, not the protocol descriptor.

## Ownership contract

- Providers, the route graph and services listed in `octane.warm` are worker
  state.
- A singleton resolved in the base application is worker state. It must not
  capture a request, user or tenant.
- A singleton first resolved in a request sandbox is discarded with that
  sandbox. Add it to `octane.warm` only if it is genuinely worker-safe.
- A scoped binding is request state and is forgotten at every boundary,
  including the boundary before request one.
- Config and URL state are cloned. Shared framework managers are rewired by
  Octane's lifecycle listeners.
- Cached route controllers are flushed after termination.
- Userland globals, statics, runtime event listeners and macros remain process
  state. Request-sensitive data in them is incompatible with persistent mode.

This is Octane-compatible lifecycle semantics, not universal emulation of a
fresh PHP process. Arbitrary process-global package state cannot be isolated
while the same PHP process and bootstrap remain alive.

## Hamster-owned cleanup

`laravel_lifecycle.php` intentionally contains no reflection and no container
instance snapshot. It owns only boundaries a cloned application cannot own:

- roll back every open database transaction;
- forget any late scoped instances again;
- restore Eloquent unguard and strictness switches to their boot baseline;
- restore Carbon's test clock to its boot baseline;
- restore the worker's default timezone and working directory;
- delete upload temporaries created by request reconstruction.

The protocol adapter has a commit gate: it refuses to flush a staged response
until cleanup is explicitly marked complete. Upload deletion failures retain
the path and retire the generation instead of being suppressed after success.

Adding another entry requires a reproduction showing that the state is both
request-sensitive and outside Octane's lifecycle. It must not become another
general reset list.

## Request and response mapping

`laravel_request.php` builds the request directly because CLI SAPI does not
populate cookies, form fields or uploads. It supports query strings, cookies,
`application/x-www-form-urlencoded`, multipart fields, nested files and
worker-owned upload temporaries. Multipart delimiters must be complete boundary
lines; boundary-like file bytes are preserved. Field-name normalization follows
PHP `parse_str` behavior for dots, spaces and malformed brackets.

Streamed and file-backed Symfony responses are semantically supported but are
buffered before the single response frame. This is not transport streaming.
Protocol v1 still limits HTTP request bodies to 1 MiB and complete frames to
8 MiB.

## Tests

- `laravel_request_test.php`: 41 framework-independent reconstruction checks.
- `laravel_octane_client_test.php`: 10 response staging/commit checks.
- `laravel_stdout_test.php`: 4 descriptor-isolation checks.
- `laravel_lifecycle_test.php`: 19 generic adversarial checks on one real
  Octane worker, plus two JWT parser and four real session/auth checks when PMS
  dependencies are present; set `HAMSTER_LARAVEL_TEST_PATH` to an
  Octane-enabled application.
- `pms_w1_isolates_auth_and_retires_after_cleanup_failure` runs 50
  authenticated/anonymous pairs through the real Rust server on one PID, checks
  route-controller freshness, injects a post-response termination failure,
  proves one execution and requires recovery on generation +1.
- `pms_w4_isolates_controllers_and_recovers_only_failed_slot` forces all four
  Octane workers to handle the same controller route, requires request-local
  controller state on every PID, then injects a cleanup failure and proves that
  only the affected stable slot advances generation. The other three slots
  retain their PID/generation and the recovered W4 pool remains isolated.
- Rust ignored wrappers run all scripts and real-app probes. The real lifecycle
  wrapper is deliberately not a stub and therefore requires the external
  application. The scratch-only provider used for PMS testing is checked in
  under `fixtures/php-worker/pms` and is never registered in production PMS;
  it refuses to boot unless the test config sets `HAMSTER_PMS_PROBE=1`.
- Rust's existing partial-write, crash and NotStarted tests continue to protect
  at-most-once semantics; protocol v1 is byte-identical.

## PMS evidence and performance

An independent scratch clone of PMS installed the real `laravel/octane` 2.19.1
package and ran with `artisan optimize`; no Octane source was injected. The
in-process ownership suite passed 25 checks. The final Rust/protocol W1 path
then completed 20,000 alternating authenticated/anonymous requests on one PID
with no isolation error and zero second-half RSS slope. Cleanup fault injection
returned 502, executed once and recovered on a new PID with no orphan.

Release medians against a same-IPC fresh-application candidate were 1.51x for a
controller JSON route, 1.49x for the Blade login page, and 1.08x on the remote-
database dashboard. These replace, and are materially smaller than, the stale
3.3x/7.6x/1.31x figures. See `doc/BENCHMARKS.md` and the checked-in raw data and
scripts under `benchmarks/`.

## Known blockers and compatibility limits

- The original PMS checkout in `www/` remains untouched and does not declare
  Octane. Its pinned PhpSpreadsheet 1.30.6 declares PHP `<8.5`, so Composer can
  install Octane under repository PHP 8.5 only with an explicit platform-ignore
  in a disposable scratch clone. This is a PMS deployment dependency decision,
  not an Octane lifecycle failure.
- `REMOTE_ADDR` remains `127.0.0.1`. Rust already receives the accepted peer
  address but discards it before routing. A server-owned reserved pseudo-header
  could carry it inside protocol v1 if all client values under that name are
  stripped, while a typed metadata field requires protocol v2. Trusting an
  inbound `X-Forwarded-For` is not equivalent: Laravel/Symfony must first know
  the immediate peer and apply an explicit trusted-proxy policy. Owner must
  choose between the v1 semantic extension and typed protocol metadata.
- Warm mutable singletons, request-sensitive userland statics, runtime macros
  and runtime listeners remain incompatible unless the application places the
  binding in `octane.flush` or otherwise provides an Octane-safe boundary.
- Cached closure routes retain the measured Laravel/PHP memory-growth
  limitation; controller routes did not grow in the new soak.
- Stream/file responses remain buffered and request bodies remain capped at
  1 MiB. Neither is required to establish request lifecycle ownership in 0.4.
- Native Linux verification remains outstanding.
