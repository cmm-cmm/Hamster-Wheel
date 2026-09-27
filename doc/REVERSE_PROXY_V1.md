# Reverse Proxy v1

Status: implemented in the working tree. On 2026-09-24 the Owner decided to
record the current Reverse Proxy v1 checkpoint and continue development. This
records technical evidence, not slice acceptance or milestone closure. Nothing
here is committed, tagged, released or accepted. This document records the behavior,
the reasons for it, the evidence gathered, and what v1 deliberately does not
do. Measurements are in
`benchmarks/results/reverse_proxy_v1_20260924.md`.

## Scope

A `proxy` location forwards every request it receives to one HTTP/1.1
upstream endpoint and streams the response back.

```toml
[[hosts.locations]]
path = "/api"
handler = "proxy"

[hosts.locations.proxy]
upstream = "http://127.0.0.1:3000"
connect_timeout = 5           # seconds, 1-3600
response_header_timeout = 30  # seconds, 1-3600
progress_timeout = 30         # seconds, 1-3600
idle_timeout = 4              # seconds, 1-3600
max_idle = 64                 # 0-4096; 0 disables connection reuse
preserve_host = true
```

Only `upstream` is required. It must be `http://host[:port]` with no path
other than a bare `/`, no query, no user information and a non-zero port; port
80 is implied. An explicit port must parse as a `u16`; an empty, nonnumeric or
out-of-range port is rejected rather than silently treated as port 80.
`https` is rejected: v1 has no upstream TLS. A hostname is resolved once while
the configuration compiles and the first address is used; an unresolvable
name fails startup before the listener binds, and the resolved address is
printed at startup as `Proxy upstream <host><path> -> http://<address>`. There
is no re-resolution.

The request path is forwarded unchanged: a location at `/api` sends
`/api/users?page=2` upstream as `/api/users?page=2`.

## Architecture

The handler is a concrete enum variant, `Handler::Proxy { upstream:
Arc<Upstream> }`. Each location compiles to its own `Upstream`, which owns the
resolved address, the timeouts, an idle-connection stack and its counters. There
is no backend trait, registry, load balancer or generic client abstraction, and
no global lock: the idle stack is a `std::sync::Mutex<VecDeque>` whose critical
sections are a single push or pop and never contain network I/O.

Connections are Hyper's own `client::conn::http1` connections over a Tokio
`TcpStream` with `TCP_NODELAY`, wrapped in the same `WriteProgressTimeout` that
bounds downstream response writes. Each connection has a spawned driver task;
the exchange holds its `SendRequest` and the task's `AbortHandle`.

```text
downstream Incoming --> RequestBody (progress deadline) --> SendRequest
                                               upstream driver task (own socket)
upstream Incoming  --> ProxyBody (progress deadline, owns Lease) --> downstream
```

Neither body is collected, copied into a `Vec`, or passed through a channel of
Hamster's. The `Bytes` frames Hyper produces on one side are the frames Hyper
consumes on the other, so Hamster adds no explicit payload copy. Hyper's own
read/write buffers, and the kernel's socket buffers, are the only buffering.

## Ownership and connection reuse

A `Lease` exists from admission to the end of the response body and is what
`UpstreamSnapshot::active_exchanges` counts. Once the response head arrives the
lease moves into the response body (`ResponseBody::Proxy`), so it lives inside
the downstream connection task and ends with it.

A connection returns to the idle stack only through `Lease::release`, which is
called only when **both** hold:

- the upstream response reached a clean end (`Incoming` returned end-of-stream,
  or a bodiless HEAD/204/304 response was complete), and
- the whole request body was handed to Hyper.

Every other way an exchange ends -- a body error, any timeout, the handler
future being dropped, the response body being dropped by a disconnected or
shut-down client, a malformed or unrequested-101 response, a response that
carried both `Transfer-Encoding` and `Content-Length` -- drops the lease, and
dropping a connection aborts its driver task, which closes the socket. That is
also how an early response ends: when an upstream answers before the upload
finished, the connection is closed rather than reused.

Checkout takes the newest idle connection. If it has been idle for
`idle_timeout`, every older one has too, and the whole stack is discarded;
there is no eviction timer. A connection Hyper reports closed is skipped, and
the rest are awaited with `SendRequest::ready`, which resolves at once for a
parked connection. Check-in beyond `max_idle` closes the oldest connection.
Idle connections are closed when the server finishes shutting down.

Hyper's client detects an upstream close of a parked idle connection promptly,
because its driver keeps a read outstanding while idle, so a stale connection is
usually discarded before it is used. The remaining window is covered by the
retry rule below.

## Retry boundary

A request is sent again only when Hyper hands it back unsent:
`TrySendError::take_message()` returns `Some` only if the dispatcher never took
the request from its queue, so no byte of it reached the socket. That is the one
proof of non-delivery. Such a request is retried at most once, always on a fresh
connection, and counted in `safe_retries`.

Every other failure is final, whatever the method: once the dispatcher took the
request, a later error cannot say how much the upstream received or did. There
is no method-based or idempotency-based retry.

The deterministic stale-keep-alive test injects a read EOF into a parked idle
connection. Its driver does not run until the queued request wakes it, finds
EOF, and returns the request unsent. The upstream sees two connections and the
retried request exactly once.

## HTTP rewriting

Request, before sending upstream:

- The target becomes origin-form (`/path?query`). The router has already
  replaced Host with an absolute-form authority, so nothing is lost.
- Hop-by-hop fields are removed: every field named in any `Connection` value,
  then `Connection`, `Keep-Alive`, `Proxy-Connection`, `Proxy-Authenticate`,
  `Proxy-Authorization`, `TE`, `Transfer-Encoding` and `Upgrade`. `Trailer` is
  end-to-end and is kept; Hyper needs it to encode trailer fields.
- `TE: trailers` is re-sent only if the downstream client offered it, since
  Hamster relays trailers and Hyper only emits them to a client that asked.
- `Content-Length` and `Transfer-Encoding` are not forwarded as received. Hyper
  decoded the downstream framing and re-encodes from the body: a known length
  becomes `Content-Length`, an unknown one is sent chunked. Chunked is set
  explicitly, because Hyper's client otherwise assumes a GET, HEAD or CONNECT of
  unknown length has no body and would send none. An explicit
  `Content-Length: 0` on an empty body is kept.
- Only a single `Transfer-Encoding: chunked` is accepted. Hyper removes the
  final chunk framing but does not decode earlier transfer codings such as
  `gzip`; stripping their header would change the meaning of the payload.
  Requests with additional codings receive 400 before upstream checkout.
- A request with both `Content-Length` and `Transfer-Encoding` is normalized by
  Hyper's server to chunked-only before Hamster sees it; conflicting
  `Content-Length` values are rejected with 400 and never reach the upstream.
- `Expect` is not forwarded. Hyper's server sends `100 Continue` downstream when
  the body is first read, which is when Hamster starts streaming it.
- `CONNECT` authority-form and `OPTIONS *` are rejected with 400 by routing
  before proxy checkout. Upgrades are not supported: `Upgrade` is stripped,
  so a WebSocket handshake reaches the upstream as an ordinary request.
- The request is always sent as HTTP/1.1.

Response, before sending downstream (the upstream is untrusted input):

- The same hop-by-hop removal, including fields the upstream's `Connection`
  nominates. `Trailer` is kept.
- Responses with transfer codings beyond a single `chunked` are rejected with
  502 before downstream commit, because Hyper does not decode those codings.
- `Content-Length` is removed and re-derived by Hyper from the body it actually
  streams, except on a HEAD response, where it carries the length a GET would
  have. Hyper does not validate that value because there is no body to frame,
  so Hamster does: all values must parse and agree, otherwise 502.
- On 204 and 304 any `Content-Length` is dropped.
- A 101 is a 502: `Upgrade` was stripped, so it was never requested, and the
  connection is no longer HTTP. Other 1xx responses are consumed by Hyper and
  not relayed.
- Status, reason-independent headers and the upstream `Server`/`Date` are kept.

## Forwarding metadata

v1 treats Hamster as the edge. There is no trusted-proxy configuration, so
nothing a client says about earlier hops is believed. `Forwarded`,
`X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Forwarded-Port`
and `X-Real-IP` are removed, and three fields are set from local facts:

| Field | Value |
|---|---|
| `X-Forwarded-For` | the immediate peer IP from `TransportInfo` |
| `X-Forwarded-Proto` | the trusted scheme of the client's connection: `https` when Hamster's TLS listener accepted it, otherwise `http` (TLS v1) |
| `X-Forwarded-Host` | the effective authority the router selected |

They replace, never append. `Host` is the effective authority when
`preserve_host = true` (the default) and the configured upstream authority when
it is `false`. `Forwarded` and `Via` are not generated.

## Timeouts

Every deadline is an inactivity deadline except connect and response-head, and
none is a total deadline on a streaming body. The progress deadlines are one
implementation, `write_progress::ProgressDeadline`, shared with the existing
downstream `WriteProgressTimeout`: the first `Pending` arms it, repeated
`Pending` keeps the original deadline, progress clears it, and the inner I/O is
polled first so progress wins a tie. A direction that never stalls allocates no
timer.

| Deadline | Arms on | Bounds | Result |
|---|---|---|---|
| `connect_timeout` | TCP connect | unreachable/black-holed upstream | 504 |
| upstream write progress (`progress_timeout`) | upstream socket write `Pending` | upstream stops reading the request | 504 before commit |
| `response_header_timeout` | request body fully handed to Hyper (at once for bodiless requests) | upstream that never answers | 504 |
| response-body progress (`progress_timeout`) | upstream `Incoming` returns `Pending` | upstream stalls mid-response | downstream aborted |
| request-body progress (`progress_timeout`) | downstream `Incoming` returns `Pending` | client stops sending mid-upload | 408 before commit |
| `server.write_progress_timeout` | downstream socket write `Pending` | client stops reading | downstream closed (existing) |
| `idle_timeout` | checkout | stale idle connections | discarded, no request cost |

Two properties make these correct rather than merely present.

**The head timer starts after the upload.** An upstream may answer at any time,
including while the client is still uploading (an early `413`), and that
response is returned at once. A long upload is not cut by a head deadline: while
it progresses, the progress deadlines bound it. Measured: an early `413` reached
the client 280 us after the upstream sent it, with 256 KiB of a declared
64 MiB uploaded.

**The request side and the response side cannot both look idle.** Hyper polls
the request body only while it can write upstream, so a `Pending` there means
the client is silent, never that the upstream is slow. The response body is
polled only when the downstream connection can take another frame, so a slow
client never arms the upstream deadline.

### The B' liveness case

The investigation found an exchange that could live forever: the upstream stops
reading the upload, Hyper stops polling the downstream body, the downstream
socket is no longer read, and a client disconnect is therefore never observed.
The upstream socket's write-progress deadline is what ends it. Measured with a
1 s progress timeout: the client filled every buffer, disconnected, and the
exchange was reclaimed 814 ms later, the upstream connection closed, and
`active_exchanges` returned to zero. Removing that deadline makes the permanent
regression test fail (mutation M08 below).

## Failure mapping

Before commit (no response head sent downstream yet):

| Condition | Status | Body |
|---|---|---|
| connect refused/reset, handshake failure | 502 | `Bad Gateway` |
| upstream closed or reset before a valid head | 502 | `Bad Gateway` |
| malformed head, head over the buffer bound, unsupported transfer coding, conflicting HEAD length, unrequested 101 | 502 | `Invalid Upstream Response` |
| connect timeout | 504 | `Upstream Connect Timeout` |
| no head within `response_header_timeout` | 504 | `Upstream Response Timeout` |
| upstream stopped accepting the request body | 504 | `Upstream Progress Timeout` |
| client stopped sending the body | 408 | `Request Body Timeout` |
| client body failed or aborted | 400 | `Invalid Request Body` |
| unsupported request transfer coding | 400 | `Unsupported Transfer-Encoding` |
| `CONNECT` authority-form or `OPTIONS *` | 400 | `Invalid Path` |

After commit, a failure never becomes a synthetic status or a shorter valid
response: the body errors, Hyper aborts the downstream connection, a sized
response ends short of its `Content-Length`, and a chunked response never
receives its terminating chunk.

A close-delimited upstream response (neither `Content-Length` nor chunked) is
the one exception HTTP/1.1 itself imposes: its end *is* the upstream closing, so
a crash there is indistinguishable from completion. It is relayed chunked to an
HTTP/1.1 client and cannot be detected as truncated.

On Windows a connect to a closed loopback port is reported refused only after
the SYN is retried, about 2.04 s later (measured). With a `connect_timeout`
below that, a refused loopback upstream is reported as 504.

## Memory model

The proxy does not use the PHP `BodyBudget`; that budget accounts for request
bodies Hamster buffers for PHP, and the proxy buffers none. Its memory bound is
structural: at most `max_connections` exchanges exist, and each owns Hyper's
downstream and upstream connection buffers (bounded by
`server.http1_max_buffer_size`, which is also applied to upstream connections),
at most one in-flight body frame per direction, and one boxed `ProxyBody`. Idle
connections add at most `max_idle` per upstream. Kernel socket buffers are
outside the process.

Measured on Windows: a stalled exchange costs 185-252 KiB of private commit at
the default 64 KiB buffer bound, whether it is a stalled 64 MiB download or a
stalled 1 GiB upload, and the cost grows linearly with the number of stalled
exchanges (64 of them: 11.7 MiB). A 64 MiB transfer at C1 ran with Hamster at
1.84 MiB private commit throughout. The cost scales with the bound: about
120-140 KiB per stalled exchange at 32 KiB, 330-400 KiB at 128 KiB,
620-700 KiB at 256 KiB and 1.08-1.56 MiB at 417,792 bytes. These are process
figures; kernel socket buffers are not in them.

The buffer bound is a performance setting for the proxy as well as a memory
one; see *Windows loopback performance*. 64 KiB, the existing server default,
is kept and also applied to upstream connections. It is the current
conservative Windows default, supported by the measured memory and loopback
performance behaviour; it is not established as a cross-platform optimum or
an invariant.

## Windows loopback performance

Two effects were measured and then investigated on Windows 11 loopback: a
throughput collapse of proxied large transfers at buffer bounds of 128 KiB and
more, and a ~15 ms tail on proxied 1 MiB responses at the 64 KiB default.
Controls falsify a Hamster-, Hyper- or Tokio-specific cause; the evidence
points to one mechanism in the Windows TCP stack (inference, high confidence).
Evidence, setup and data:
`benchmarks/results/reverse_proxy_v1_20260924.md`, section *Windows loopback
send stall*.

Buffer bound, C1, RPS (medians of three interleaved runs):

| Cell | 32 KiB | 64 KiB | 128 KiB | 256 KiB | 417,792 |
|---|---:|---:|---:|---:|---:|
| GET 1 MiB | 778 | 391 | 92 | 113 | 229 |
| GET 8 MiB | 155 | 178 | 84 | 54 | 27 |
| POST 64 MiB | 21.1 | 28.1 | 22.6 | 9.9 | 2.7 |

At C32 the collapse is mostly absent (GET 64 MiB: 52.7, 59.3, 53.9, 50.0 and
44.3 RPS), 64 KiB was fastest in every C32 cell, and Hamster's peak private
commit grew with the bound (about 7.7, 10, 15, 25 and 37.6 MiB).

FACT:

- The collapse reproduces without Hamster: in a Hyper-only proxy, in the same
  proxy inside the load generator's process, in a raw Tokio relay with no
  HTTP, and between two plain std threads on a socket pair
  (`benches/windows_tcp_stall.rs`).
- It appears when the reader pauses (backpressure) and reads in chunks as large
  as the socket's autotuned receive buffer (131,070 then 262,140 bytes here):
  with a 200 us pause per read, 408 KiB reads stalled 31-39 times per 64 MiB and
  64 KiB reads 0-1 times; a 1 MiB `SO_RCVBUF` on the reader removed the stalls.
  It follows whichever side reads with the large buffer: the upstream
  connection for downloads, the downstream connection for uploads.
- During a stall the receiver has already ACKed everything and advertises an
  open window, the sender has data queued and has received that window
  update, and a nonblocking send still returns `WSAEWOULDBLOCK`: the kernel
  is not taking data. ETW shows transmission resuming when a TCP timer
  scheduled 10 ms ahead fires on the sender's processor, 10-17 ms after the
  stall began.
- A receive buffer larger than the read size (`SO_RCVBUF`) changes the picture
  substantially; see below.
- `TCP_NODELAY` on the downstream socket and a 1 ms timer resolution inside
  Hamster changed nothing.

INFERENCE, high confidence: large or bursty reads under backpressure interact
with Windows loopback TCP so that a sender whose window reopened can wait for
a later TCP timer event before it sends again. Each such wait costs a timer
period: tens of them make the 64 MiB collapse, one of them makes a slow
1 MiB request.

HYPOTHESIS, not verified: the exact internal Windows rule for which window
updates are deferred; real-NIC behaviour; Linux and other TCP stacks; the
effect of an explicit `SO_RCVBUF` on kernel memory at scale.

**Tail.** GET 1 MiB at C1, share of requests at or above 5 ms: about 12%
through Hamster with an upstream that writes 64 KiB frames, 3.9% with an
upstream that writes one large body, 1.6-6.3% direct with no proxy, and
8-16% through the Hyper-only proxy and the raw relay. The slow requests take
5-17 ms, mostly 12-16 ms. The direct path has the same tail at a lower rate:
Hamster does not create it, it adds one more TCP leg that can stall. It is a
latency effect, not a correctness defect: nothing is lost, truncated or
misreported.

**`SO_RCVBUF` is documented, not adopted.** A temporary, reverted experiment
set `SO_RCVBUF` on Hamster's upstream sockets. At 1 MiB, GET 64 MiB at the
417,792 bound went from about 1.9-2.7 to 24-27 RPS, and GET 1 MiB at 64 KiB
from about 430 RPS with a 16 ms p99 to about 1,500 RPS with a 1 ms p99. At
4 MiB, 64 MiB transfers ran at about half the default's throughput, and 1 MiB
was slightly slower than the default for 64 MiB at 64 KiB: larger is not
always better. An explicit `SO_RCVBUF` also disables or changes Windows
receive-window autotuning for the socket and can raise kernel memory per
connection, which was not measured. Only upstream sockets and C1 were tried.
It is recorded as the most promising lever, with too little evidence to ship.

## Observability

`Upstream::snapshot()` returns `active_exchanges`, `idle_connections`,
`connections_opened`, `connections_reused`, `safe_retries`, `connect_failures`,
`upstream_timeouts` and `upstream_failures`.
`connections_reused` counts successful idle checkouts, including one that
subsequently fails before the request is sent. Failures are reported on
standard error with the upstream's label and address, at most once per 60 s per
upstream with a running count, so a dead upstream cannot turn request volume
into log volume. Nothing is logged per request or per frame.

## Falsification

The earlier investigation reports that each invariant was attacked by
temporarily breaking the implementation and running the permanent suite. No
mutation remains in the current source. Its record says all 29 mutations were
detected by at least one test; the table is in the results file. The 29th was
added in the final review, when the rule "a response carrying both
`Transfer-Encoding` and `Content-Length` never returns its connection to the
pool" turned out to have no test of its own; forcing that connection reusable
now fails
`a_response_framed_by_both_lengths_is_relayed_but_its_connection_is_closed`.
Among them:
collecting the request body, returning the lease before EOF, reusing an
aborted or failed connection, retrying a possibly-started request, keeping
Connection-nominated fields, appending to a spoofed `X-Forwarded-For`,
removing the upstream write-progress deadline, replacing any progress deadline
with a total one, waiting for the upload before reading the response, and
starting the head timer at send time.

## Known limitations

The checkpoint retains these categories so that moving to TLS investigation
does not erase them:

- **Known design limitations:** startup-time DNS resolution, first resolved
  address only, close-delimited response EOF/truncation ambiguity, progress
  timeouts rather than a total exchange lifetime (a progressing peer can keep
  an exchange alive indefinitely), and `header_read_timeout` also serving as
  the shutdown drain bound.
- **Performance research, not v1 correctness blockers:** 32 versus 64 KiB
  buffer, `SO_RCVBUF`, Windows TCP loopback timer behaviour and the roughly
  15 ms tail, and C128 connection churn / `max_idle` behaviour. No experimental
  socket or timer change was retained in production.
- **Not verified:** native Linux build/test/runtime, Linux cross-target check
  without an installed target, real-NIC behaviour, a separate IPv6 network
  probe, and every scheduler interleaving close to response EOF, driver failure,
  and shutdown.
- **Future features:** upstream TLS, HTTP/2, HTTP/3, WebSocket/Upgrade,
  CONNECT, load balancing, multiple upstream endpoints, health checks, and
  DNS re-resolution.

Checkpoint hygiene: HEAD `a7a31b45448d2d4aa324980965c834c7166aa5bb`
(`dev`), with the Reverse Proxy worktree uncommitted. Before this documentation
checkpoint, `git diff --stat` reported 12 tracked files, 712 insertions and 46
deletions; untracked production source, tests, benchmark probes, and result
files were also present and therefore are not included in that stat. The
tracked diff, untracked production files, and probes were inspected. No pcap
or ETW capture was in the repository. `SO_RCVBUF` appears in benchmark/test
probes only; no production `timeBeginPeriod` or temporary TCP instrumentation
was present. `git diff --check` passed. The performance probes are
`benches/proxy.rs` and `benches/windows_tcp_stall.rs`; production paths are in
`src/proxy.rs`, `src/server.rs`, and `src/write_progress.rs`. Detailed v1
evidence and measurements remain below and in the linked results file.
The final readiness review reported no unresolved CRITICAL or IMPORTANT
finding in v1 scope, and no evidence requiring a change from the current
`location → Arc<Upstream> → Hamster HTTP/1.1 pool → hyper::client::conn::http1`
architecture. For this documentation-only checkpoint, `cargo fmt --check`,
`git diff --check`, and the default `cargo test --quiet` suite passed on native
Windows (119 library tests, 24 configuration tests, and the remaining default
integration tests; environment-dependent ignored tests were not run). Native
Linux remains NOT VERIFIED.

Deliberately not in v1: upstream TLS, more than one endpoint per location, load
balancing, health checks, circuit breaking, retry of possibly-started requests,
WebSocket and any `Upgrade`/`CONNECT` tunnelling, HTTP/2 and HTTP/3 on either
side, caching, compression, trusted-proxy chains, `Forwarded`/`Via`
generation, DNS re-resolution, Unix sockets and named pipes, path-prefix
rewriting, 1xx relaying (including 103), and configuration reload.

Also known:

- A close-delimited upstream response cannot be distinguished from a truncated
  one (HTTP/1.1 framing, not a Hamster choice).
- A request that reaches a connection the upstream is closing at the same
  instant, after the dispatcher took it, fails with 502 rather than being
  retried. This is the cost of the at-most-once rule; `idle_timeout` defaults
  below common upstream keep-alive timeouts to keep the window rare.
- Hostname resolution uses the first address only. `localhost` may resolve to
  `::1` first; use an IP literal when the upstream listens on one family.
- A peer that keeps making some progress forever can hold an exchange forever;
  the deadlines bound *no* progress, not total duration, as everywhere else in
  Hamster.
- Graceful shutdown drains an active proxied stream for at most
  `header_read_timeout` (measured: 503 ms with a 500 ms bound), the same
  existing debt as for static streams.
- Raising `server.http1_max_buffer_size` to 128 KiB or more severely degrades
  proxied large transfers at low concurrency on Windows loopback (see
  *Windows loopback performance*). 64 KiB is a measured conservative default,
  not an invariant.
- About 12% of proxied 1 MiB responses at C1 take 5-17 ms (median 0.7 ms)
  when the upstream writes its body in pieces. The cause is a Windows loopback
  TCP send stall that direct connections show too; Hamster does not mitigate
  it. See *Windows loopback performance*.
- Concurrency above `max_idle` churns upstream connections (224 new
  connections in 3 s at C128 with the default 64). On loopback this had no
  measurable cost; over a real network each new connection costs a handshake.
- Only Windows `x86_64-pc-windows-msvc` was measured, on loopback; no real NIC.
  Native Linux: NOT VERIFIED. Linux was not even cross-checked for this
  change: the Linux target is not installed in the current toolchain.
  Corrected on 2026-09-27: the target *is* installed. The cross-target check
  is blocked instead by `aws-lc-sys`, which compiles C and needs
  `x86_64-linux-gnu-gcc`. Linux remains NOT VERIFIED either way.
