# PHP Application Compatibility (Windows, persistent workers)

Status: working tree on top of `7ca716f`, 2026-09-28, Windows
`x86_64-pc-windows-msvc`, repository PHP 8.5.10 NTS. Nothing here is a claim
about Linux (NOT VERIFIED) or about PHP applications that were not run.

Hamster runs PHP two ways. Persistent workers are a pool of `php.exe` CLI
processes speaking Hamster's protocol (`doc/PHP_WORKER.md`). FastCGI
(`doc/FASTCGI_V1.md`, 2026-09-28) hands each request to `php-cgi` endpoints
the operator starts, so the script runs under a real SAPI. This document is
about the first mode. What an application needs in order to run on persistent
workers follows from two facts: it runs under the CLI SAPI, and it is booted
once and then serves many requests. This document records what that means,
which applications were run, how, and what was verified.

## What the CLI SAPI does not do

Probed directly with the repository PHP (`php -c php.ini probe.php`):

| SAPI feature | Under a Hamster worker |
|---|---|
| `header()`, `setcookie()`, `http_response_code()` | No effect. `headers_list()` is always empty and `http_response_code()` returns `false`. |
| `session_start()` (native sessions) | Starts, but its cookie is never sent. An open session outlives the request unless the adapter closes it. |
| `$_SERVER`, `$_GET`, `$_POST`, `$_COOKIE`, `$_FILES` | Not populated per request. `$_SERVER` holds the worker process environment. |
| `echo` / `print` / `php://output` | Captured by the adapter, never sent to the client unless the adapter chooses to. |
| `STDOUT` constant | Closed, because fd 1 is the protocol pipe: `fwrite(STDOUT, ...)` throws `TypeError`. `php://stdout` opened later writes to a null sink. |
| `exit()`, fatal errors, memory exhaustion | End the worker process. Hamster answers 502, never retries the request, and replaces the worker. |
| `register_shutdown_function` error pages | Cannot answer: the process that would send them has ended. |

An application whose HTTP layer uses those features directly (a classic
`index.php` that calls `header()` and echoes its page, most of WordPress,
applications reading `$_SERVER` for request data) cannot run on persistent
workers. It needs the `fastcgi` handler. An application whose HTTP layer is a request and
response object (Symfony HttpFoundation, PSR-7) can, through an adapter that
builds the request from the protocol frame and turns the response object into
the RESPONSE frame. That is the boundary this document tests.

## Adapters

All adapters live in `fixtures/php-worker/` and share `adapter_support.php`,
which does the work a SAPI does before application code runs: server
variables from the frame (method, URI, protocol, peer and local endpoints,
scheme, `HTTP_*` headers, `SERVER_NAME` from the effective authority), the
cookie header, urlencoded and `multipart/form-data` bodies with PHP-style
nested field names, upload temporaries created per request and removed after
it, the response headers Hamster owns (`Content-Length`, `Connection`,
`Transfer-Encoding`, `Server` and the other hop-by-hop fields are dropped
rather than sent, since the server rejects them), and the 8 MiB frame limit.

| Adapter | For | Configuration | Lifecycle between requests |
|---|---|---|---|
| `laravel_worker.php` | Laravel with `laravel/octane` 2.x | `HAMSTER_LARAVEL_PATH` | Octane's sandbox and listeners plus Hamster's resource boundary (`doc/LARAVEL_WORKER_0_4.md`) |
| `symfony_worker.php` | any Symfony `HttpKernel` application | `HAMSTER_SYMFONY_PATH`, optional `HAMSTER_SYMFONY_KERNEL`, `APP_ENV`, `APP_DEBUG` | Symfony's own: `Kernel::handle()` resets `kernel.reset` services before the next request; sessions use the cookie bag |
| `psr15_worker.php` | any PSR-15 handler with PSR-17 factories | `HAMSTER_PSR15_ENTRY` names an entry file | none beyond the entry's optional `reset`; any native session is closed after every request |
| protocol directly | an application written for Hamster (`fixtures/php-apps/native`) | the application's own worker script | the application's |

Integration conventions an application must meet, stated rather than hidden:

- **Symfony**: the container cache must be warm for `APP_ENV` before the pool
  starts (`bin/console cache:warmup`, part of any Symfony deployment), because
  the kernel boots before READY and the pool allows five seconds. Assets are
  compiled (`asset-map:compile`) and served by a Hamster `static` location.
  `kernel.terminate` work runs before the response is committed, so it delays
  the response; see the commit point below.
- **PSR-15**: a front controller that builds its request from superglobals and
  emits with `header()` cannot be used as is. The application is reached
  through an entry file that builds it the same way and returns it.
  `fixtures/php-worker/entries/slim_skeleton.php` does this for an unmodified
  Slim skeleton and reproduces the headers the skeleton's `ResponseEmitter`
  adds, because the worker never calls an emitter.
- **Everyone**: keep `display_errors` on stderr (the adapters set it); write
  logs to files or stderr, not the `STDOUT` constant; keep per-request state
  out of statics or reset it.

Commit point: the Laravel and Symfony adapters finish the framework's
termination work and remove upload temporaries before writing the RESPONSE
frame. If that work throws, they write nothing and exit; the server answers
502, never retries, and starts a clean worker. The server therefore never
hands the next request to a worker whose previous request did not finish
cleanly.

A response larger than the frame allows is answered `500 Response Too Large`
by every adapter, and the worker keeps serving. Before this change, writing
it failed inside the worker, the server killed the worker and answered 502.

## Applications run

| Application | Origin | Version | Mode | Adapter |
|---|---|---|---|---|
| Symfony Demo | external, unmodified | `symfony/symfony-demo` 3.1.0 (Symfony 8.1.0, Doctrine ORM 3.6.7, Twig 3.27.1), SQLite shipped with the package | `APP_ENV=prod`, W1 and W2, compiled assets served by Hamster | `symfony_worker.php` |
| Slim skeleton | external, application code unmodified | `slim/slim-skeleton` 4.5.0 (Slim 4.15.3, slim/psr7 1.8.0, PHP-DI 6.4.0) | W1 and W2 | `psr15_worker.php` + entry file |
| Native test application | simulated, this repository | `fixtures/php-apps/native` | W2 | protocol directly |
| Laravel stand-in | simulated, `prepare_scratch_app.ps1` | Laravel 12.69.2, Octane 2.20.0 | W2 in the side-by-side test | `laravel_worker.php` |
| DokuWiki | external, unmodified | 2026-07-14c "Mort" release tarball | **FastCGI**, two `php-cgi` endpoints, not a persistent worker | none: a SAPI application |
| FastCGI fixture | simulated, `fixtures/php-cgi` | this repository | **FastCGI** | none |

"Unmodified" means no file of the application was edited. The Slim skeleton
is started through an entry file outside it; Symfony Demo was prepared with
its documented production steps. The simulated applications are written for
testing and prove server behavior, not framework compatibility. The real PMS
was not run: the only Octane-enabled copy connects to the production PMS MySQL
and Redis, and no isolated copy of its data was available.

Rebuild everything with `fixtures/php-apps/prepare_php_apps.ps1` (Symfony
Demo, Slim, native; 3 min 14 s from an empty directory on this machine) and
`fixtures/php-worker/pms/prepare_scratch_app.ps1` (Laravel). See
`doc/LOCAL_TEST_APPLICATIONS.md`.

## Verified behavior

`tests/php_applications.rs`, eleven ignored tests, run with
`HAMSTER_PHP_APPS` and `HAMSTER_LARAVEL_TEST_PATH` set:

| Behavior | Evidence |
|---|---|
| Front-controller routing, application 404s | Symfony Demo pages and unknown post (404 from the application); Slim routes, 404 as the application's JSON error |
| Static assets beside an application | Symfony's compiled stylesheet served by the `/assets` static location, `text/css` |
| Form login, session cookie, same-origin CSRF | Symfony Demo login as `john_user` (302 away from the form, `PHPSESSID` set), stateless CSRF accepted from the `Origin` header, which proves the scheme and authority Hamster gives the application |
| Authorization and redirects | Anonymous admin access redirects to `/en/login` on the request's own authority; `john_user` gets 403 |
| Form POST with a database write | A comment posted through the form (303) is visible on the next GET |
| Client isolation on one persistent worker | W1 Symfony, administrator and anonymous client alternating ten times: the administrator stays logged in, the anonymous client never inherits the session, one PID and one generation served all 21 requests |
| Native session leak prevention | A PSR-15 handler using `session_start()` served three cookieless clients; each saw `visits=1` |
| Request reconstruction | multipart with nested names, arrays, a 300 KB binary upload (FNV-1a digest compared), an empty file input (`UPLOAD_ERR_NO_FILE`), urlencoded nested arrays and UTF-8, cookies with percent-encoding, repeated request headers in order |
| Adapter mapping against the real libraries | `httpfoundation_test.php` (28 checks, Symfony Demo's symfony/http-foundation) and `psr7_test.php` (30 checks, slim/psr7): URI, scheme, port, client address, query, form and multipart bodies, uploads that are valid and movable (`move()` / `moveTo()` under the CLI SAPI), upload cleanup, repeated headers, response cookies, streamed and file bodies, the native session guard |
| Response headers | two `Set-Cookie`, repeated custom headers, repeated `Cache-Control`; an application's own `Content-Length: 999` and `Connection` dropped, Hamster's length sent |
| Body limits | 7 MiB response delivered byte-exact; 9 MiB answered `500 Response Too Large` without replacing the worker; a request declaring 1 MiB + 1 byte answered 413 before the worker; HEAD answered with the GET length and no body |
| Application output | `echo`, `print`, `php://output`, reopened `php://stdout` never corrupt a frame; the `STDOUT` constant throws `TypeError` |
| Sessions across workers | an application's file-backed session follows its cookie to both workers of a W2 pool under concurrent requests |
| Failure containment | a handled exception is the application's 500 and replaces nothing; `exit` and memory exhaustion answer 502 and each replaces exactly one worker, after which the pool serves again |
| Pools side by side | Symfony, Slim, native and Laravel pools in one server, 8 clients x 24 mixed requests while the native pool's workers were killed 8 times: every Symfony, Slim and Laravel request answered 200 with its own content, none of those pools replaced a worker or changed PID, the native pool answered only 200 or 503 and returned to two live workers |
| Shutdown | every worker of every adapter exits with code 0 through SHUTDOWN/SHUTDOWN_ACK, never the kill fallback |

Real clients against an oversized upload (`native` pool, 2 MiB multipart, 20
attempts each, MSVC release builds of this tree and of `7ca716f`):

| Client | This tree | `7ca716f` |
|---|---|---|
| curl, `Expect: 100-continue` (its default for large bodies) | 413 in 20 | 413 in 20 |
| curl, `Expect` disabled | 413 in 20 | 413 in 20 |
| .NET `HttpClient` | 413 in 20 | connection reset or abort in 20 |

curl reads while it sends and stops; .NET sends the whole body first.

## Early refusals and lingering close

Several refusals are sent before the request body is read: a declared body
over the PHP limit (413), over the body memory budget (503), and any handler
that answers first. Hyper drained at most one buffer and closed; Windows then
answered the remaining body with a reset, and a client that sends its whole
body before reading, as .NET's HttpClient does and browsers commonly do, lost
the response. `src/lingering_close.rs` now sits on every accepted socket. When
a connection closes while its last request declared a body, or with bytes
waiting, it sends FIN and reads and discards until the client closes, stops
sending for 1 s, or `server.body_read_timeout` passes. Otherwise the close is
immediate. Once server shutdown has begun, a closing connection does not
linger. A lingering connection keeps its `max_connections` slot for at most
those bounds.

The first version also stopped after 5 s or 16 MiB. Both bounds were measured
wrong with .NET `HttpClient` against the 1 MiB PHP limit (MSVC release, 10
attempts each): refused uploads of 15 MiB received 413 in 10 of 10, of 17, 32
and 100 MiB a reset in 10 of 10, and a 4 MiB upload sent at 640 KiB/s was reset
at 5.1 s. Discarding 15 MiB took about 7 ms on loopback, so the byte bound
protected nothing time does not. With the current bounds all of those receive
413 (100 MiB in a median 50 ms); a client pausing 1.5 s between writes is
still reset, by design. 32 concurrent refused 4 MiB uploads with
`max_connections = 8` all received 413 and the server served normally
afterwards.

A first version lingered on any declared body without the idle and shutdown
bounds; the existing `shutdown_waits_for_body_deadline_but_not_forever`
regression caught it (shutdown took the graceful drain deadline instead of
the body deadline). Regressions in `src/server.rs`:
`an_early_refusal_reaches_a_client_that_sends_its_whole_body_first` (whole
body at once, and in bursts with a pause during which the server closes),
`a_closing_connection_without_a_body_does_not_linger`,
`a_stalled_upload_frees_its_slot_after_the_linger_idle_bound`, the shutdown
test above, `an_early_refusal_over_tls_reaches_a_client_that_sends_its_whole_body_first`
in `src/server/tls_tests.rs` (20 MiB through rustls), and five unit tests in
`src/lingering_close/tests.rs`, one per rule, on a real socket pair. Five
mutations -- never linger, ignore the declared body, always linger, linger
during shutdown, no idle bound -- are each caught in 3 of 3 runs.

## Limits that remain

- Request bodies are limited to 1 MiB by the protocol, so uploads larger than
  that are refused with 413. Protocol work, not an adapter setting.
- Responses are assembled whole: nothing reaches the client before the
  application finishes, a streamed response is buffered, and the frame caps a
  response at a little under 8 MiB.
- Applications that depend on the SAPI features above need the `fastcgi`
  handler, whose endpoints the operator must start and supervise in this slice
  (`doc/FASTCGI_V1.md`). None of Symfony Demo, Slim or Laravel was run through
  FastCGI, and DokuWiki was not run on persistent workers.
- Cold start: on a freshly copied application tree the first boot measured
  3.4-3.8 s for the Laravel stand-in and 1.7 s for Symfony Demo, against
  0.1-0.3 s warm, and Symfony Demo in its debug environment 9.9-11.7 s. The
  cost is the first open of each file (10,072 files: 100.7 s first read, 0.47 s
  second, on-access scanning enabled; attributing it to the scanner is an
  inference). The READY budget is now `php.startup_timeout` (default 5 s,
  1-300); a pool whose application boots cold must raise it.
- Repeated request headers reach HttpFoundation and PSR-7 server parameters as
  one value joined with ", " (Cookie: "; "), as under a CGI server. PSR-7
  requests also carry each value separately through `getHeader()`. Before
  2026-09-28 only the last value survived.
- PHP-DI 6.4 (Slim skeleton 4.5.0) emits deprecation notices on PHP 8.5; they
  reach stderr, prefixed with the pool, and do not affect responses.
- Only the applications above were run. Symfony and PSR-15 adapters are
  generic by construction, but other frameworks on them were not tested, and
  nothing here says any other framework works.
