# FastCGI v1 (Windows, operator-managed endpoints)

Status: implemented in the working tree on top of `7ca716f`, 2026-09-28. Not
committed, tagged or released; the Owner decides acceptance. Windows
`x86_64-pc-windows-msvc` only. Native Linux: NOT VERIFIED.

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

**The root is served.** Hamster has no `.htaccess`. An application that relies
on Apache deny rules for directories under its root (DokuWiki's `data/`,
`conf/`, `inc/`, `bin/`, `vendor/`) must have them closed with `not_found`
locations, which win by longest prefix:

```toml
[[hosts.locations]]
path = "/data"
handler = "not_found"
```

Measured with DokuWiki: without such locations `/data/pages/wiki/dokuwiki.txt`
was served as text; with them it is `404`. Pointing `root` at a public
directory, as Laravel and Symfony lay out, avoids the question.

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

One TCP connection per request (`FCGI_KEEP_CONN` off). The request body is
streamed as `FCGI_STDIN` by its own task while the response is read, so a
script that answers, or writes more than a socket buffer, before reading its
input cannot deadlock against Hamster. A request body needs `Content-Length`
(`411` for chunked), because PHP needs `CONTENT_LENGTH` first; it is not
buffered and has no Hamster size limit (PHP's `post_max_size` applies). The
client body gets `body_read_timeout` in total.

The response head is the CGI header block (at most 64 KiB, else `502`):
`Status:` sets the status, `Location:` without `Status:` is `302`, headers
repeat as PHP sent them (both `Set-Cookie` lines survive), and hop-by-hop
fields, `Content-Length` and `Server` from the backend are dropped. The body
is streamed as PHP produces it: output PHP flushes reaches the client before
the script ends, and there is no response size limit. `FCGI_STDERR` lines are
written to Hamster's stderr prefixed `FastCGI <host><path> stderr:`.

Failures:

| Condition | Answer |
|---|---|
| Every slot busy for `connect_timeout` | `503 FastCGI Busy`, the backend never contacted |
| A server refuses or times out the connection | the next free server is tried; nothing was sent, so this is safe |
| Every server tried and refused | `502 FastCGI Unavailable` |
| No response bytes within `response_timeout` before the head | `504 FastCGI Response Timeout` |
| Empty, malformed or oversized head | `502 FastCGI Error` |
| Backend silent or gone after the head | the response ends without its final chunk: never mistaken for complete |
| Client body stalls past `body_read_timeout` before the head | `408` |

A request is never sent twice: once `FCGI_BEGIN_REQUEST` is written, the
exchange is final. A timeout cannot stop PHP: `php-cgi` finishes the abandoned
script, and that endpoint is busy until it does (measured: the next request to
it waited behind a 3 s script). Failure notices are rate limited to one per
60 s per location. Server shutdown drains an exchange in progress like any
other connection. Each location has its own slots and servers; nothing is
shared between locations or hosts.

## Evidence

Unit tests, `src/fastcgi/tests.rs` (18, a scripted FastCGI server behind a real
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

Integration, `tests/php_cgi.rs` (9 ignored; repository PHP 8.5.10
`php-cgi.exe`, started by the test as an operator would), passing 3 of 3 runs
in parallel mode:

| Application | Origin | Verified |
|---|---|---|
| `fixtures/php-cgi` | fixture, this repository | superglobals (`$_GET` arrays, `$_POST`, `$_COOKIE`), `$_FILES` with `is_uploaded_file()`/`move_uploaded_file()` of a 300 KB binary, `PATH_INFO` and `PHP_SELF`, `HTTP_PROXY` absent, `http_response_code(201)`, two `setcookie()`, repeated `header()`, `Location` redirect, native sessions shared by two php-cgi processes and never crossing clients, `flush()` streaming (first chunk well before the end), a 20 MiB response byte-exact, a fatal error as PHP's `500`, `exit`, source never served under `.`/space/case spellings, `404` for a missing script, static CSS, two endpoints in parallel and a third request waiting, an endpoint killed (skipped) and all killed (`502`) and restarted (used again), `504` at `response_timeout`, two hosts with separate roots and endpoints, shutdown draining a running script |
| DokuWiki 2026-07-14c "Mort" | external, the release tarball unmodified | pages through the `doku.php` front controller, template assets served statically, its `DokuWiki` session cookie, a page edited through its form and read by another client, a PNG uploaded through its media manager and served back byte-exact by `lib/exe/fetch.php`, `data/`, `conf/`, `inc/` closed with `not_found` locations |

No real PMS, MySQL or Redis was used.

## Limits

- Endpoints are operator-managed: a dead `php-cgi` is skipped but never
  restarted by Hamster; `PHP_FCGI_MAX_REQUESTS` must be `0` or the process exits
  after that many requests (500 by default).
- One connection per request; no keep-alive to the backend. Not measured.
- Request bodies need `Content-Length`; chunked uploads are `411`.
- A timed-out or disconnected request's script keeps running in PHP.
- The backend's `Content-Length` is dropped, so script responses are chunked
  (HTTP/1.1) or close-delimited (HTTP/1.0).
- No `.htaccess`, rewrite rules or per-directory PHP settings; deny rules are
  `not_found` locations.
- Only the applications above were run. This is not a claim that any other
  application or framework works.

## Owner decisions

1. **Managed endpoints.** Options: (a) keep operator-managed endpoints; (b)
   Hamster starts N `php-cgi -b` processes per location on loopback ports,
   restarts them with the PHP worker pool's backoff, contains them in its Job
   Object and stops them at shutdown; (c) both, by configuration. (b) makes a
   Windows deployment one process to supervise and reuses tested lifecycle
   code, but adds process ownership to a second backend and a port-allocation
   policy; it should be its own slice with the lifecycle matrix applied.
2. **Timeout status.** `504` follows the reverse proxy. The PHP worker answers
   `502` for its execution timeout because the request is possibly started; the
   same reasoning applies here. Choose one policy for all gateways.
3. **Deny rules.** `not_found` locations work but must be written per prefix.
   A small per-location deny list, or shipping documented recipes per
   application, would make traditional deployments safer by default.
