# FastCGI v1 (Windows, operator-managed endpoints)

Status: v1 and its Windows corrective pass are in `dev` at `6e8fd34`.
Operator-managed endpoints only; see `doc/WINDOWS_OPERATIONS.md` for the
deployment boundary. Windows `x86_64-pc-windows-msvc` only. Native Linux:
NOT VERIFIED.

## Why

Persistent workers run PHP under the CLI SAPI, so `header()`, `setcookie()`,
`http_response_code()`, native session cookies, per-request superglobals and
`move_uploaded_file()` do not work there (`doc/PHP_APPLICATION_COMPATIBILITY.md`).
A traditional PHP application is written against a real SAPI. FastCGI gives it
one: `php-cgi.exe` parses the request itself, exactly as under IIS, Apache or
nginx. This is roadmap 0.6's compatibility path.

## Scope of this slice

- A `fastcgi` location runs scripts under a document root through FastCGI
  endpoints that the operator starts and supervises, for example
  `php-cgi.exe -b 127.0.0.1:9000` with `PHP_FCGI_MAX_REQUESTS=0`.
- Hamster does **not** start, restart or stop `php-cgi` in this slice. Whether
  it should (managed endpoints) is an Owner decision; see the end.
- Nothing about persistent workers changes.

```toml
[[hosts.locations]]
path = "/"
handler = "fastcgi"
root = "sites/wiki"                 # document root, required

[hosts.locations.fastcgi]
servers = ["127.0.0.1:9001", "127.0.0.1:9002"]  # 1-64 numeric addresses, required
concurrency = 1                     # exchanges per server at once, 1-256, default 1
index = "doku.php"                  # front controller and directory index, default "index.php"
connect_timeout = 5                 # seconds, 1-3600: waiting for a slot, then the TCP connect
response_timeout = 60               # seconds, 1-3600: backend inactivity, head and body
keep_connections = true             # reuse a connection across exchanges (FCGI_KEEP_CONN)
resolve_cache = 1                   # seconds, 0-60: reuse a path's script resolution; 0 = every request
```

`php-cgi` on Windows serves one connection at a time per process, so each
process is one `servers` entry with `concurrency = 1`; parallelism is the
number of processes. `concurrency` above 1 is for endpoints that multiplex
(a PHP-FPM pool on Linux, not verified).

## Request mapping

What a request path runs is decided on the object actually opened, with the
same traversal rules and the same Windows opened-handle containment as the
`static` handler (`src/static_files.rs`):

1. The first path segment ending in `.php` (any letter case) names the script,
   if it is a regular file inside the root; the rest of the path is
   `PATH_INFO` (`/index.php/extra/path`). A `.php` path that names no file is
   `404`; it is never handed to the front controller.
2. Otherwise an existing file is served statically by Hamster (`GET`/`HEAD`,
   `405` otherwise), unless its final location ends in `.php`, in which case
   it runs. A script's source is never sent.
3. A directory runs its `index` script if present, else serves its
   `index.html`, else falls through to 4. There are no directory listings.
4. Anything else runs the front controller, `index` at the root, or is `404`.

Traversal, encoded separators, NUL, drive and stream syntax are refused
(`403`/`400`) and anything whose final location is outside the root, through a
junction for example, is `403`. Because the canonical root is a verbatim
`\\?\` path, `app.php.` and `app.php%20` name no file (Windows does not strip
the dot or space beneath such a path) and go to the front controller.

**Resolution cache (`resolve_cache`, default 1 s, owner decision 2026-10-19).**
When a request path resolves to a script -- a named `.php` file or the front
controller -- that result is reused for the same raw request path for
`resolve_cache` seconds without touching the filesystem. Static files,
directories, refusals and `404`s are resolved on every request, and the host's
`deny_paths`/`deny_names` policy is applied to every request, cached or not.
At most 4,096 paths are remembered per location; when full, expired entries go
first and then all of them, so client-chosen paths cannot grow it. What this
changes, stated plainly: a script deleted, a new file that would now be served
statically, or a junction created or retargeted under the root is seen up to
`resolve_cache` seconds late for a path already cached. A junction that exists
when a path is first resolved is still refused. The containment check was
never a guarantee against a concurrent namespace change for scripts, because
`php-cgi` opens `SCRIPT_FILENAME` by its path after Hamster checked it; the
cache widens that existing window from microseconds to at most the configured
period. `resolve_cache = 0` restores a resolution on every request.

Why: on this Windows host each filesystem open through the blocking pool
costs tens of microseconds of CPU, same-file opens stop scaling at about 40,000
per second, and a script resolution is several of them. A build that skipped
resolution for repeated paths ran the classic cells 1.75x (C1) and 1.85x (C16)
faster, with Hamster's own CPU per request falling from 206-289 us to 75-81 us.

**The root is otherwise served.** Hamster has no `.htaccess`. An application
that relies on Apache deny rules needs an explicit host-wide file access
policy. For DokuWiki:

```toml
[[hosts]]
name = "wiki.example"
deny_paths = ["/data", "/conf", "/inc", "/bin", "/vendor"]
deny_names = ["composer.json", "composer.lock"]
```

The policy runs before location selection, including overlapping FastCGI and
static locations. Whole path components are ASCII case-insensitive and URI
percent-decoded before comparison. Thus `/data`, `/DATA`, and `/%64ata` are
denied, but `/database` is not. File handlers also deny dotfiles at any depth
(except public root `/.well-known`) and never serve `.php`, `.phtml`, `.phar`,
`.phps` or the other blocked PHP-source extensions as static bytes. They
recheck the final location of the opened file, preventing an in-root junction
alias to a private subtree from evading the rule. A denied path receives 404,
without revealing its filesystem location. Configure each application's own
private paths; the defaults cannot infer them. Pointing `root` at a public
directory, as Laravel and Symfony lay out, remains preferable when possible.
For scripts, Hamster checks an opened handle but passes a pathname to the
operator-managed FastCGI endpoint; the endpoint later opens that pathname.
Thus the static-byte path has opened-handle consistency, while script-path
handoff cannot make an object-identity guarantee under concurrent namespace
replacement. Do not grant an untrusted actor filesystem write access to the
application's PHP tree.
For FlatPress, deny `/fp-defaults`, `/fp-includes`, `/setup` and the private
`/fp-content/{config,users,content,cache,compile,index}` trees. Leave
`/setup.php` available only during installation; remove it or add it to
`deny_paths` after setup. The public `fp-content/images` tree remains usable.

CGI/1.1 variables (RFC 3875) sent: `GATEWAY_INTERFACE`, `SERVER_SOFTWARE`,
`SERVER_PROTOCOL`, `REQUEST_METHOD`, `REQUEST_URI` (the target as received),
`QUERY_STRING`, `DOCUMENT_ROOT` and `SCRIPT_FILENAME` (plain Windows paths,
without `\\?\`), `SCRIPT_NAME`, `PATH_INFO`/`PATH_TRANSLATED` when present,
`REMOTE_ADDR`/`REMOTE_PORT` (the socket peer), `SERVER_ADDR`/`SERVER_PORT`,
`SERVER_NAME` (the effective Host without its port), `REQUEST_SCHEME`, `HTTPS=on`
over TLS (from the listener, never a header), `REDIRECT_STATUS=200` (php-cgi
refuses to run without it by default), `CONTENT_TYPE`, `CONTENT_LENGTH`, and
one `HTTP_*` per request header field, repeated fields joined with `", "`
(Cookie with `"; "`). A `Proxy` request header is never passed:
`HTTP_PROXY` is read as the outgoing proxy by many HTTP clients (httpoxy,
CVE-2016-5385).

## Exchange

With `keep_connections = true` (the default, 2026-10-19) every request sets
`FCGI_KEEP_CONN`, and a connection is kept for the next exchange on the same
server when its exchange ended cleanly: END_REQUEST was read, nothing followed
it in the read buffer, and the whole request body including its end record
was sent. Every other ending -- a timeout, a malformed or truncated response,
a client that disconnects, a script that answered before its body was sent,
`HEAD` -- closes the connection. A kept connection is checked with a
non-blocking peek before reuse and discarded if the backend closed it or
wrote to it while idle; a fresh connection is then opened to the same server.
The connection is the slot: at most `concurrency` connections exist per
server, so a single-connection `php-cgi` is never left waiting on a second
one. A backend that closes a kept connection between the check and the
request makes that request a `502`, never a retry (at-most-once is unchanged).
`keep_connections = false` restores one connection per request.

Measured on Windows against `php-cgi` with a minimal script, from a direct
FastCGI client: 320-330 us of wall time and 120-135 us of `php-cgi` CPU per
request with a new connection each time, against 66-72 us and 37-63 us on a
kept one. Through Hamster, the classic hello C1 cell went from 1,516 to 3,293
requests/s and the json C16 cell from 6,844 to 10,726 (`doc/BENCHMARKS.md`).

The request body is
streamed as `FCGI_STDIN` by its own task while the response is read, so a
script that answers, or writes more than a socket buffer, before reading its
input cannot deadlock against Hamster. A request body needs `Content-Length`
(`411` for chunked), because PHP needs `CONTENT_LENGTH` first; it is not
buffered and has no Hamster size limit (PHP's `post_max_size` applies). The
client body gets `body_read_timeout` in total.

On Windows, a single-exchange endpoint (`concurrency = 1`) with
`connect_timeout <= 5` uses a bounded blocking-pool TCP connect. Controlled
release measurements found a long tail in Tokio async connect on this
two-hop path; this change does **not** reuse TCP connections or alter FastCGI
framing. A per-connect ownership handoff lets cancellation reclaim the
exchange permit and exact server claim immediately if the blocking closure
has not started. If it has started, the closure retains both until connect
returns; it never sends a FastCGI frame itself. A separate, per-location
semaphore bounds queued/running blocking connect closures to the configured
endpoint-slot count. When that cap is occupied, new connects use cancellable
async I/O rather than enqueue more blocking work.
Multiplexing endpoints and longer connect timeouts retain cancellable async
connect; their Windows throughput was not qualified by the new A/B. A running
blocking connect remains non-cancelable until the OS call returns. Tokio can
also delay when a queued closure runs, so the 5 s configuration cap is not an
absolute process-shutdown deadline; unrelated stalled blocking work can delay
runtime teardown. A connect-only task cannot initiate a new FastCGI request
after its HTTP waiter disappears; this does not roll back work already sent.

The response head is the CGI header block (at most 64 KiB, else `502`):
`Status:` sets the status, `Location:` without `Status:` is `302`, headers
repeat as PHP sent them (both `Set-Cookie` lines survive), and hop-by-hop
fields, `Content-Length` and `Server` from the backend are dropped. Records
that arrived together with the head are read without waiting: when they
already contain END_REQUEST and at most 64 KiB of output -- the usual shape
of a small page -- the exchange ends there, its connection goes back to the
idle list and its slot is released before the response is handed to Hyper,
and the client gets the exact `Content-Length` Hamster counted. Otherwise the
body is streamed as PHP produces it: output PHP flushes reaches the client
before the script ends, and there is no response size limit. `FCGI_STDERR` lines are
written to Hamster's stderr prefixed `FastCGI <host><path> stderr:`.

Failures:

| Condition | Answer |
|---|---|
| Every slot busy for `connect_timeout` | `503 FastCGI Busy`, the backend never contacted |
| A server refuses or times out the connection | the next free server is tried; nothing was sent, so this is safe |
| A Windows blocking connect's HTTP waiter expires before the connect task returns | `502 FastCGI Unavailable`; a queued attempt releases its exchange slot immediately, a running attempt after connect returns; no FastCGI request was sent and no unsafe replay occurs |
| Every server tried and refused | `502 FastCGI Unavailable` |
| No response bytes within `response_timeout` before the head | `504 FastCGI Response Timeout` |
| Empty, malformed or oversized head | `502 FastCGI Error` |
| Backend gone with the head, before anything reached the client | `502 FastCGI Error` |
| Backend silent or gone later in the body | the response ends without its final chunk: never mistaken for complete |
| Client body stalls past `body_read_timeout` before the head | `408` |

A request is never sent twice: once `FCGI_BEGIN_REQUEST` is written, the
exchange is final. A timeout cannot stop PHP: `php-cgi` finishes the abandoned
script, and that endpoint is busy until it does (measured: the next request to
it waited behind a 3 s script). Failure notices are rate limited to one per
60 s per location. Server shutdown drains an exchange in progress like any
other connection. Each location has its own slots and servers; nothing is
shared between locations or hosts.

On Windows, a single `php-cgi -b` process handles only one exchange at a time.
When HTTP concurrency exceeds the available endpoint slots, requests wait up
to `connect_timeout`, then receive 503 without contacting PHP. An exchange
that timed out after being sent can still finish inside PHP or remain queued
in that endpoint's TCP backlog; 504 is **not** proof that an application-side
effect was rolled back. Do not retry such requests automatically. In the
follow-up release probe (`doc/BENCHMARKS.md`), one endpoint at HTTP C32 had
~143 median RPS and ~230 ms P50 despite zero HTTP errors: queue latency can
be material well before a 503 appears.

The distinction is cause-based: an application response timeout is 504;
invalid or failed backend protocol is 502; no exchange slot is 503; a client
body deadline is 408. A status code does not mean replay is safe. If the
response head has already gone to the client, a later timeout or backend
failure only ends the body/connection; Hamster cannot replace that response
with a new 502 or 504.

The path resolver also accepts valid UTF-8 names without panicking when
checking for a `.php` suffix; a regression covers a multibyte character at
the former byte-slice boundary. This changes no PHP file access policy.

## Evidence

Unit tests, `src/fastcgi/tests.rs` (a scripted FastCGI server behind a real
Hamster listener, and real directories): path resolution, front controller,
`PATH_INFO`, missing scripts, static fallback, letter case, trailing dot and
space, stream syntax, traversal, junction containment; CGI variables, body,
repeated headers, httpoxy; status, redirect, LF-only heads, malformed and
oversized heads; response and post-head timeouts; truncated bodies; connect
failover; slot exhaustion; `411`; a backend answering before reading its input;
`HEAD`; static files served without contacting the backend. Eight mutations
(httpoxy, PHP index served statically, repeated headers collapsed, body sent
before reading, slot never released, containment ignored, `Location` not a
redirect, backend `Content-Length` trusted) were each caught.

The original v1 integration set had 9 ignored tests; the corrective pass adds
an independent FlatPress test. `php-cgi.exe` is started by the test as an
operator would:

| Application | Origin | Verified |
|---|---|---|
| `fixtures/php-cgi` | fixture, this repository | superglobals (`$_GET` arrays, `$_POST`, `$_COOKIE`), `$_FILES` with `is_uploaded_file()`/`move_uploaded_file()` of a 300 KB binary, `PATH_INFO` and `PHP_SELF`, `HTTP_PROXY` absent, `http_response_code(201)`, two `setcookie()`, repeated `header()`, `Location` redirect, native sessions shared by two php-cgi processes and never crossing clients, `flush()` streaming (first chunk well before the end), a 20 MiB response byte-exact, a fatal error as PHP's `500`, `exit`, source never served under `.`/space/case spellings, `404` for a missing script, static CSS, two endpoints in parallel and a third request waiting, an endpoint killed (skipped) and all killed (`502`) and restarted (used again), `504` at `response_timeout`, two hosts with separate roots and endpoints, shutdown draining a running script |
| DokuWiki 2026-07-14c "Mort" | external, the release tarball unmodified | pages through the `doku.php` front controller, template assets served statically, its `DokuWiki` session cookie, a page edited through its form and read by another client, a PNG uploaded through its media manager and served back byte-exact by `lib/exe/fetch.php`; private directories closed by host-wide rules, including case/percent-encoded variants |
| FlatPress 1.5.1 | external, pinned source tag unmodified | fresh installer over FastCGI, resulting public home page, private config/user/content paths denied; tested with two operator-managed endpoints |

No real PMS, MySQL or Redis was used.

## Limits

- Endpoints are operator-managed: a dead `php-cgi` is skipped but never
  restarted by Hamster; `PHP_FCGI_MAX_REQUESTS` must be `0` or the process exits
  after that many requests (500 by default).
- Kept connections are verified against `php-cgi` 8.5.10 on Windows. A
  backend that mishandles `FCGI_KEEP_CONN` can be served with
  `keep_connections = false`. PHP-FPM and Linux are not verified.
- Request bodies need `Content-Length`; chunked uploads are `411`.
- A timed-out or disconnected request's script keeps running in PHP.
- The backend's `Content-Length` is dropped. A response that arrived whole
  with its head carries the length Hamster counted; a streamed one is chunked
  (HTTP/1.1) or close-delimited (HTTP/1.0).
- No `.htaccess`, rewrite rules or per-directory PHP settings; use host-wide
  `deny_paths`/`deny_names` for private content.
- Only the applications above were run in this FastCGI integration suite. This is not a claim that any other
  application or framework works.

## Owner decisions

1. **Managed endpoints.** Options: (a) keep operator-managed endpoints; (b)
   Hamster starts N `php-cgi -b` processes per location on loopback ports,
   restarts them with the PHP worker pool's backoff, contains them in its Job
   Object and stops them at shutdown; (c) both, by configuration. (b) makes a
   Windows deployment one process to supervise and reuses tested lifecycle
   code, but adds process ownership to a second backend and a port-allocation
   policy; it should be its own slice with the lifecycle matrix applied.
