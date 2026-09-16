# HTTP and Transport Foundation 0.5

Status: four slices are implemented — immediate transport identity, bounded
HTTP/1 admission, bounded PHP request-body reconstruction, and a bounded
aggregate request-body memory budget. The last two are in the working tree and
are not committed, tagged or released; the package version is unchanged.

An independent review of all four as a milestone candidate found no correctness
defect, no resource leak, no path that weakens at-most-once and no measurable
performance regression. It found two claims the documentation had to state
honestly before closure: response-body memory is not bounded by anything this
milestone adds, and the declared-`Content-Length` reservation lets a slow client
hold budget it never uses. Both are recorded below with their measurements.
Streaming responses and an alternative charging policy are documented debt and
are deliberately not implemented here. Milestone closure is an owner decision.

## First slice: immediate transport identity

The first slice captures the two socket addresses established by TCP accept
and carries them through routing to the selected handler:

```text
TcpListener::accept
    -> TransportInfo { peer_addr, local_addr }
    -> one HTTP/1.1 connection
    -> every request on that connection
    -> selected handler
```

`peer_addr` is the immediate TCP peer. `local_addr` comes from the accepted
stream, not from the listener. That distinction is observable when a listener
is bound to `0.0.0.0` or `[::]`: the accepted stream records the concrete local
endpoint that received the connection.

The value is immutable and owned by one connection. It is copied into each
request dispatch; there is no global or mutable current-request slot. Failure
to obtain the accepted stream's local address closes only that connection and
returns the accept loop to service. HTTP parse/connection errors now include
both immediate endpoints in the existing error diagnostic.

## Why this precedes a larger request context

FACT: the old accept loop discarded its returned peer address. Later PHP,
proxy and access-log mapping therefore could not recover an authoritative
immediate peer.

FACT: Hyper's request already owns method, URI, HTTP version and headers.
Duplicating them in a second context would create two representations without
a current consumer.

FACT: the current listener is cleartext HTTP only. There is no TLS termination
or trusted forwarding policy from which to derive a different scheme or client
identity.

INFERENCE: the smallest sound contract today is `Request` plus immutable
`TransportInfo`, not a speculative all-purpose request context. Additional
transport facts should enter only with the listener or policy that can
authoritatively establish them.

The implementation deliberately passes the value as a normal internal
argument. Inserting it into `http::Extensions` would allocate the extensions
type map on requests that otherwise have no extensions. This slice adds no
per-request heap object or synchronization and exposes no public backend trait.

`HostRouter::serve` previously happened to be public even though its only
callers were inside the crate and handlers are not user-extensible. It is now
crate-private because a caller without an accepted socket cannot satisfy the
transport contract. This is an intentional pre-1.0 API tightening, not a
promise of a stable embedding API.

## Security and trust boundary

`TransportInfo::peer_addr` never means an end-user address after a trusted
proxy. Client-supplied `Forwarded` and `X-Forwarded-For` values do not alter it.
A future client-address policy must begin with this immediate peer, explicitly
decide whether that peer is trusted, and produce a separate derived identity.

Protocol v1 is unchanged. The persistent PHP worker still receives no typed
transport metadata and its Laravel adapter still reports the documented
placeholder `REMOTE_ADDR=127.0.0.1`. Capturing the source fact in Rust does not
pretend that the unresolved wire compatibility decision has been made.

## Checked-in evidence

`server::tests::carries_actual_socket_identity_across_keep_alive_requests`
uses real TCP connections and a test-only handler. It verifies:

- a listener bound to `0.0.0.0` yields the concrete loopback local endpoint;
- the accepted peer equals the client's actual local socket address;
- a malformed HTTP connection is contained and a later connection is served;
- two requests on one keep-alive connection retain one connection identity;
- forged forwarding headers cannot replace either address;
- 32 concurrent connections do not exchange connection identity;
- server shutdown still drains the connection tasks.

The test's socket reads have explicit time bounds so a connection regression
fails instead of hanging the suite.

Windows verification on Windows 11 Pro build 26200, MSVC:

- `cargo test --all-features`: 62 executed, zero failed, 55 dependency-gated
  tests ignored;
- ignored PHP/process tests: 55 unique tests passed after supplying the
  existing Octane scratch PMS to the five external-application cases;
- the transport test passed 50 consecutive runs and left no Hamster or PHP
  process;
- `cargo clippy --all-targets --all-features -- -D warnings`, formatting,
  explicit MSVC release build and all-benchmark linking passed.

The release static A/B is recorded append-only in `BENCHMARKS.md`. It detects
no material steady-state regression. Because it reuses HTTP connections, it
does not quantify the one `local_addr()` query added per new connection.

Native Linux remains **not verified**. The only WSL distribution on the review
machine was Docker Desktop's internal Linux environment, with no Rust/PHP
toolchain, and its Docker engine was not running. No toolchain was installed
and no cross-compiled result is presented as native evidence.

## Second slice: bounded HTTP/1 admission

The accept loop now owns at most `server.max_connections` connection tasks.
When that count is reached it stops polling `TcpListener::accept` and waits for
a task to finish or shutdown. Excess connections therefore remain in the
operating system's bounded listen backlog instead of allocating a Tokio task,
Hyper state and application-visible socket inside Hamster. No semaphore,
application queue or per-request atomic operation was added.

Every HTTP/1 connection now uses Hyper's Tokio timer and three compiled startup
limits:

| Setting | Default | Valid range | Boundary |
|---|---:|---:|---|
| `max_connections` | 1024 | 1-65536 | accepted connection tasks owned by Hamster |
| `header_read_timeout` | 30 s | 1-3600 s | total time for each complete request head, including keep-alive idle time |
| `http1_max_buffer_size` | 65536 B | 8192-1048576 B | Hyper's adaptive HTTP/1 read/write buffer |

FACT: in the locked Hyper 1.11.1 source, `Builder::new` specifies a nominal
30-second header timeout but has no timer. Hyper's `Time::check` disables that
default when no timer is present. The old server therefore had no effective
header timeout. A request head held open forever also held graceful shutdown
open forever.

FACT: Hyper starts the header deadline before reading a head, clears it once
the head is parsed, and starts it again for the next keep-alive head. It does
not wrap body reads or the service future. Enabling it cannot time out or replay
a PHP transaction after dispatch. During graceful shutdown Hyper waits for a
partially received head; the same deadline now bounds that wait.

FACT: Hyper's locked default maximum buffer is 417792 bytes and grows
adaptively from 8192 bytes. The explicit 65536-byte default reduces the
per-connection parser/write-buffer ceiling without allocating 64 KiB eagerly.
Responses larger than the cap remain legal; the cap is a buffering limit, not
a response-size limit.

The implementation intentionally leaves Hyper's locked 100-header default
unset. Calling `max_headers`, even with 100, switches its parser scratch space
from stack to heap for every request; Hyper documents an approximate 5%
performance loss. The byte cap and existing count cap are both active without
paying that cost on valid traffic.

Malformed heads, header timeouts and incomplete peer-controlled messages are
closed without synchronous per-connection stderr output. Unexpected server or
service errors keep the existing endpoint diagnostic. This prevents hostile
input from turning the bounded connection path into an unbounded blocking-log
workload; structured counters/logging remain later observability work.

### Adversarial evidence

Checked-in real-TCP tests prove that:

- a proven-accepted keep-alive connection occupies a one-connection test limit
  and a second request is not dispatched until that slot is released;
- partial heads and idle keep-alive connections close at the configured
  deadline, after which new requests still succeed;
- an HTTP head beyond the 8192-byte test cap and a head beyond Hyper's locked
  100-header count cap both receive 431, then a later connection succeeds;
- shutdown while saturated finishes within the configured header deadline
  without aborting an application service future;
- a 256 KiB static response remains complete through the 64 KiB default
  buffer cap.

Configuration tests cover defaults, explicit compilation and every lower and
upper bound. The existing worker cancellation, at-most-once and lifecycle code
is untouched. On the available Windows PHP/PMS environment, all 55 ignored
unit/process/HTTP/Laravel tests passed, including cancellation, possibly-started
no-replay, crash/recovery, random-kill soak, shutdown and W1/W4 isolation; no
Hamster or PHP process remained afterward.

### Hot-path cost

FACT: seven-run release A/B on the small in-memory `/api` 404 control measured
29.656 versus 28.438 CPU microseconds/request (+1.218 us, +4.3%) and 1.9% lower
median RPS. The filesystem-dominated static `/` comparison detected no
regression. Full conditions and run ranges are recorded append-only in
`BENCHMARKS.md`.

INFERENCE: most of the trivial-route cost is Hyper's boxed header-deadline
future, which the locked source constructs and drops for every parsed head.
The absolute cost is small for application workloads but visible at the HTTP
ceiling. It is retained because removing the deadline restores an unbounded
slow-head and shutdown failure. A custom timer wheel or I/O wrapper would add
synchronization and lifecycle code without current evidence that it is the
safer trade.

## Third slice: bounded PHP request-body reconstruction

Hyper owns the `Incoming` body stream for a connection, but only the selected
handler can decide whether the body is needed. Static and simple responses drop
an unread body and let Hyper close the incomplete exchange. The PHP adapter is
different: it must reconstruct the body before writing a worker-protocol frame,
so a client that sends one byte and then pauses can otherwise hold a connection
slot indefinitely.

The body policy is deliberately local to that consumer:

```text
HTTP head accepted
    -> PHP adapter checks header count and declared lower bound
    -> total body-read deadline around Incoming::frame()
    -> <= 1 MiB reconstruction
    -> only then PHP pool admission / application execution
```

`server.body_read_timeout` is compiled once and passed as an ordinary argument
through the server and router to the PHP handler. It is not a request extension,
global timer, or service-wide timeout. The deadline covers the complete body
reconstruction, so receiving a byte does not reset it. A declared body larger
than the existing 1 MiB protocol limit is rejected before reading; chunked and
other unknown-length bodies are bounded while frames arrive. Empty bodies use a
no-read fast path.

FACT: a real Hyper/TCP test with one connection slot sends body bytes every 75 ms
while the configured deadline is 300 ms. The request receives HTTP 408 before the
writer finishes, the queued connection is admitted, and a second request on that
connection succeeds. The test passes 20 consecutive repetitions. An ignored PHP
integration test proves the same boundary against the actual worker: the partial
body receives 408, `/state` reports exactly one backend request, and the worker is
not given the incomplete request.

FACT: the timeout is before `PhpWorkerPool::execute`, so dropping the incomplete
`Incoming` cannot replay or cancel a PHP transaction. Hyper closes the incomplete
HTTP exchange after the 408; this is intentional, since the body is not reusable
for keep-alive parsing. A subsequent connection or an already queued connection
recovers normally.

INFERENCE: this is a consumer deadline, not a universal HTTP body policy. A future
proxy or streaming backend must own a compatible policy because it may consume
frames incrementally rather than reconstructing them. A total process byte budget
is still not present. Bodies still being reconstructed are bounded by
`max_connections * 1 MiB`; once reconstruction completes, queue capacity
controls how many of those same connection-owned bodies can remain admitted,
but it does not add a second independent population. Parser, frame-encoding and
allocator overhead remain on top, and the `BytesMut`/`Vec`/protocol-frame
transition can create a short-lived peak above the retained body bytes.

The timer uses Tokio's cancellation-safe `timeout` around body reads only. It does
not wrap the handler future, worker queue wait, PHP IPC, or response generation;
the established cancellation/at-most-once boundary therefore remains unchanged.

### Adversarial evidence

The checked-in body test proves all of the following on real TCP sockets:

- a one-byte body holds the only connection slot until the configured deadline;
- trickling bytes faster than the deadline does not extend the total budget;
- the timed-out exchange returns 408 and releases the slot;
- a queued complete body is served after release;
- a second request on that recovered keep-alive connection still succeeds;
- the server shuts down cleanly after the timeout.

The PHP regression test is dependency-gated because it needs the local PHP 8.5
distribution, but it exercises the production PHP request path and checks that no
backend request count is incremented by the partial body.

## Aggregate body-memory investigation

The working-tree Windows probe in `benchmarks/body_memory_probe.ps1` measures
process-private memory while delayed 1 MiB requests are held. With one worker and
`queue_capacity=128`, Hamster grew by 6.52, 10.93, 21.32, 37.52 and 73.71 MiB
at 4, 8, 16, 32 and 64 accepted connections. With 32 connections, changing the
queue from 1 to 8 to 128 changed the growth from 6.17 to 14.78 to 37.62 MiB.
An incomplete-body run with 32 connections and `queue_capacity=1` still grew
37.58 MiB after each client sent only half of a declared body; PHP memory did not
grow because no request reached the worker.

FACT: queue capacity cannot bound body bytes still owned by HTTP consumers.
INFERENCE: the aggregate body population is primarily bounded by
`max_connections * MAX_REQUEST_BODY`; queue capacity controls retention after
body reconstruction but does not add an independent population. Allocator,
parser and worker-frame copies remain overhead.

No global byte semaphore was added in that pass. The following slice answers the
question it left open, with measurements rather than a hypothesis.

## Fourth slice: bounded aggregate request-body memory

Status: working tree only, not committed. The owner accepted the mechanism, a
64 MiB default, and `0` as an explicit unlimited opt-out, on the condition that
the mechanism carry its own minimum observability before the default is on.
That observability is implemented and described below.

### The problem the investigation left

The per-request body limit bounds one body. Nothing bounded how many bodies
existed at once, and `queue_capacity` cannot help because it only sees a body
after reconstruction finishes. The only aggregate bound was
`max_connections * MAX_REQUEST_BODY`, roughly 1 GiB at the shipped defaults.

Lowering `max_connections` does bound it, and that option was taken seriously
before anything was written. It was rejected as the whole answer because it
prices every request the same: to hold body memory to 64 MiB an operator would
have to refuse the 65th concurrent static GET, which costs no body memory at
all. Connection admission and body memory are different resources and the
measurement shows they can be bounded separately.

### Designs considered

- **Lower `max_connections` only.** Zero code, zero hot-path cost, and it does
  bound the risk. Rejected as insufficient, not as wrong: it couples an HTTP
  concurrency decision to a memory decision, and the owner can still choose it
  by setting `max_body_memory = 0`.
- **A coarse permit: N concurrent body reconstructions.** A ceiling of
  `N * MAX_REQUEST_BODY` that is easy to reason about, but it charges a 20-byte
  form post the same as a megabyte upload, so it throttles ordinary traffic for
  no memory benefit. Rejected.
- **A byte semaphore with waiting.** More accurate, but a waiting request holds
  a connection slot and a parser buffer while it waits, so it converts a memory
  problem into a queueing and fairness problem and introduces a new way for one
  client to delay another. Rejected against the requirement that no new
  unbounded waiting appear.
- **Watching process RSS and refusing above a threshold.** Platform specific,
  lagging, and untestable, because a test cannot make RSS a known value.
  Rejected.
- **Spilling bodies to disk.** Solves memory by adding I/O latency, a cleanup
  obligation and a new failure mode, for a 1 MiB per-request limit. Grossly
  disproportionate. Rejected.
- **Chosen: a byte-counted reservation, taken before allocation, refused
  immediately when it does not fit, released by ownership.**

### Why the chosen shape follows from the measured lifetime

The investigation established that body bytes have exactly one owner at a time
and that the chain is short:

```text
Incoming frames -> reader's Vec -> WorkerRequest -> PhpJob in queue or worker
```

That is an ownership chain, so the mechanism is an owned value that travels
along it. `BodyReservation` is created before the reader allocates, moves into
`AdmittedRequest`, moves into `PhpJob`, and is dropped when the job is dropped.
There is no explicit release call anywhere, which is what makes "released on
every success, error, timeout, rejection and cancellation path" a property of
the type rather than a claim about the code.

Placing it in the job, rather than holding it in the HTTP handler, is
load-bearing. A client that disconnects while its job is queued drops the
handler future but not the job. A handler-scoped guard would have released the
reservation while the bytes were still held, and the counter would have been
behind reality exactly when the server was under the most pressure.

Reserving before allocating is the other load-bearing choice. The reader asks
the budget for the capacity it is about to take, and only then calls
`reserve_exact`. A declared `Content-Length` is charged in full before the first
body byte is read, so trickling a declared megabyte is not a cheaper way past
the ceiling than sending it.

### Failure behavior

Over budget is `503`, the same class as a full worker queue: a well-formed
request that the server is out of a bounded resource for right now. It is
deliberately not `413`, which would tell the client its body was too large when
the body was within its own limit. Nothing waits, so refusal is immediate and
adds no queue.

The check sits beside the existing header-count, body-size and body-deadline
checks, before `PhpWorkerPool::execute`. It therefore cannot cancel or replay a
possibly-started PHP transaction, and the at-most-once boundary is untouched.

This `503` is an aggregate condition and depends on other traffic, so unlike a
`413` the same request may succeed a moment later and a client should retry it.
What a slow client can do with a reservation it never fills is measured in
*The cost of charging a declared Content-Length* below.

### Evidence

Unit tests cover release on drop, refusal without disturbing the counter, growth
acquiring only the difference, a refused growth leaving the original intact,
overflow refusal, and sixteen threads contending without the counter crossing
the limit.

Real-TCP tests in `server.rs` prove that a saturated budget refuses an
individually legal body with `503` while admitted bodies are still served whole,
that the counter returns to zero after served, timed-out, disconnected, refused
and chunked-overgrown bodies, that a chunked body with no declared length is
charged as it grows, and that the server serves normally after each.

An ignored PHP integration test proves the refusal happens before the worker:
with the budget set to exactly one maximum body, a saturating in-flight 1 MiB
POST causes a concurrent small POST to be refused, and the worker's own executed
request counter never includes it.

### Operator visibility

The budget is on by default, so a refusal has to be explainable without the
operator already suspecting it. Three signals cover that, and nothing else was
added.

**The configured ceiling, once at startup.** `server::run` prints
`Request body memory budget 67108864 bytes`, or
`Request body memory budget disabled (server.max_body_memory = 0)`, beside the
existing worker-ready and listening lines. A running server should not have to
be asked what its own limit is.

**The refusal itself, rate limited, on standard error.**

```text
body memory budget exhausted: 3 request(s) refused with 503 since start
(server.max_body_memory = 67108864 bytes); reported at most once per 60 s
```

The first refusal of a run reports immediately, so the condition is visible when
it starts rather than a window later. After that one line per minute, whatever
the arrival rate. That bound is the point: a refusal is peer-controlled, and
logging each one would turn refused traffic into a synchronous stderr workload —
the same failure `should_report_connection_error` exists to avoid on the
connection path. The clock read and the compare-exchange that enforce the window
are inside the refusal path, which by definition only runs when the server is
already at its ceiling; the served path is untouched.

The count is cumulative and is never rate limited. An operator reading it must
not see a number that was quietly reduced to keep standard error quiet.

**Which 503 it was, in the response body.** A PHP location can answer 503 for a
full worker queue, an unavailable pool, or this budget. Hyper writes the
canonical reason phrase for the status, so the status line cannot carry the
distinction; the body does, and it reads `Request Body Memory Budget`. Both the
checked-in TCP test and the PHP integration test assert that exact string, so it
cannot change silently and leave the operator's other signal broken.

### Why `peak` is not reported

`BodyBudget::peak()` exists and is what makes the ceiling checkable in a test
rather than only arguable, but it is deliberately not surfaced at runtime.

Its only operational use is proactive sizing — "you are running at 60 of 64 MiB,
consider raising it" — and that answer is only useful if it is sampled
periodically. Periodic sampling of a gauge is a metrics subsystem, which is
explicitly out of scope here and is properly the job of the later observability
work. At the moment a refusal happens, `peak` is also nearly uninformative,
because it is pinned at or just under the limit by definition.

The refusal count answers the question this slice owes an operator, which is
"did this mechanism refuse my request, and what do I change". `peak` answers a
different question that no existing surface can carry.

### Cost of the visibility

None on the served path. Nothing was added to `acquire`'s success path, to
`grow_to`, to `release`, or to the no-body fast path. The startup line runs once
per process. The counter increment already existed; the clock read and the
window compare-exchange run only when a request is being refused.

FACT: the hot-path A/B was re-run after this change and still measures no
difference outside noise. Conditions are appended to
`benchmarks/results/body_budget_0_5_20260916.md`.

### Measured ceiling and cost

Full conditions are in
[`benchmarks/results/body_budget_0_5_20260916.md`](../benchmarks/results/body_budget_0_5_20260916.md).

FACT: with the budget off, private memory under a slow-upload flood grew
linearly at 1.09 MiB per accepted connection: 36.52, 72.41, 140.59 and
278.50 MiB at 32, 64, 128 and 256 connections. With a 16 MiB budget the same
sweep measured 18.23, 18.93, 18.87 and 18.79 MiB.

FACT: at a fixed 128 connections the delta tracked the configured budget at
1.09-1.15 times it: 4.48 MiB at 4 MiB, 18.36 MiB at 16 MiB, 70.07 MiB at 64 MiB,
against 140.59 MiB disabled.

FACT: release A/B on the PHP path measured no difference outside noise. GET with
no body: 7086 versus 7097 rps. 12-byte POST across three passes: 6728/6514,
6524/6803, 6694/6745 rps, with the sign of the difference flipping. 64 KiB POST
across two interleaved passes: medians 3712 versus 3710 rps.

HONEST LIMIT: within-arm spread reached 16% on the 12-byte POST, so this harness
can fail to detect a regression below roughly 5%. It shows no visible cost; it
does not prove there is none.

INFERENCE: part of why no cost is visible is that the same change removed a full
`Vec` copy of every reconstructed body. `build_worker_request` used to copy a
`BytesMut` into the `WorkerRequest`; the reader now builds the `Vec` directly.
The budget's atomics and that removed copy are plausibly of similar magnitude on
this workload.

### What the ceiling does not cover

Response bodies, the protocol frame a worker builds for an in-flight job, Hyper
per-connection parser and write buffers, and allocator fragmentation. The
measured 1.09-1.15 factor on the request-body probes is where the last three
appear. The first is not a factor on top of the budget at all; it is an
independent and larger term, and it has its own section below.

An operator should size roughly 1.2 times `max_body_memory` for the request-body
component of RSS, bound the connection component separately, and read
*What 0.5 does not bound* before sizing the machine.

### The cost of charging a declared Content-Length

Charging the declared `Content-Length` before the first body byte is read is what
makes the aggregate bound hold: without it, sending slowly would be the cheap way
past the ceiling. The choice has a price, and the review measured it rather than
leaving it as a footnote.

FACT: with `max_body_memory = 4194304` and `body_read_timeout = 20`, four clients
that declared 1 MiB each and then sent one byte held the entire budget. Every
other POST received `503` for the duration; GET requests were still answered
`200`. At the 64 MiB default the same denial costs an attacker 64 connections.

INFERENCE: the budget therefore traded a memory-exhaustion risk for a
POST-availability risk. That trade is still favourable — the process survives,
static and GET traffic keep serving, the hold is bounded by `body_read_timeout`,
and the condition names itself on standard error — but it is a real property that
0.5 introduced and did not previously exist.

The alternative charging policy is to count only the capacity actually
allocated, which is already what an undeclared chunked body does. It would hold
the same memory bound, because a reservation still cannot be smaller than the
buffer it covers, and it would remove this denial entirely, because a stalled
client would hold almost nothing. Its cost is refusing late: a client could
upload most of a megabyte before being told there is no room, instead of being
turned away at the door. That is a genuine trade rather than an obvious
improvement, so it is recorded as debt and left to the owner rather than changed
here.

Configuration levers, and how this differs from `413`, are in
`doc/CONFIGURATION.md` under *What a declared Content-Length reserves, and what
that costs*.

### What 0.5 does not bound

0.5 adds four limits and one budget. It does not add a process-memory ceiling,
and no combination of its settings produces one. The largest unbounded term is
the response body.

FACT (source): a `static` location reads the entire file into memory before
sending it, and a PHP response arrives as one protocol frame. Every in-flight
request holds a complete copy of what it is about to write. Nothing limits the
size of a static file and nothing limits the total across connections.

FACT (measured, Windows MSVC release): four clients that requested a 256 MiB
static file and then stopped reading held 1026.4 MiB of process-private memory —
256.6 MiB per in-flight request. That is sixteen times the entire default
`max_body_memory`, reached by four ordinary requests rather than by an attack.

FACT (measured): the hold is bounded in time but not in size. A connection whose
response write is stalled is closed at `header_read_timeout` and its memory
released; a second client was served 3.1 s after a 5-second setting and 7.0 s
after a 10-second setting, in both cases while a large response to a
non-reading peer was outstanding.

INFERENCE: Hyper's header deadline covers more of the connection than this
document's second slice describes. That section says the deadline is cleared once
a head is parsed and re-armed for the next one, and says nothing about the write
phase; the observed behavior is that a stalled write is also reclaimed at that
deadline. The favourable behavior is therefore load bearing and comes from a
dependency's internals, and no test pins it. A Hyper upgrade could remove it
without anything in this repository failing.

HYPOTHESIS: the same unbounded shape applies to PHP responses up to the 8 MiB
frame limit, which is smaller by construction and was not separately measured.

Streaming responses are the actual fix and are documented debt, not part of this
milestone. Until then the operational levers are to keep large files out of
`static` roots and to lower `max_connections`, which scales this term
proportionally. `max_body_memory` does not affect it.

### Technical judgment

The protection is real and the ceiling is now a configured number rather than a
product of two unrelated limits. The cost is one small module, one owned value
threaded along an ownership chain that already existed, and no measurable
throughput change.

The default is 64 MiB and that is a behavior change: a configuration that today
survives a burst of more than 64 concurrent megabyte uploads will begin
returning `503`. It is accepted because for ordinary traffic it is unreachable —
64 MiB of 8 KiB form posts is 8192 concurrent bodies against a
`max_connections` default of 1024 — so it binds only when bodies are both large
and concurrent, and because the refusal is recoverable and now explains itself.
An operator whose clients upload near the 1 MiB limit over slow links should
size it with `expected concurrent uploads * typical body size * 1.2`; `0`
restores the previous behavior exactly.

## Deliberate non-scope

These slices do not change protocol v1, `REMOTE_ADDR`, total byte budgeting,
streaming request/response transport, static-file implementation, proxy trust,
TLS, HTTP/2, observability export or framework lifecycle. These are not implied
by transport identity, connection admission or the PHP body deadline.

FACT: the transport value itself represents IPv4 and IPv6, but the existing
Host router does not accept bracketed IPv6 literal authorities. That is a
separate HTTP authority/configuration correctness gap; this slice does not
claim IPv6 virtual-host support.

FACT: request-body reads for the PHP adapter are now outside the header deadline
but inside their own total body deadline. A slow PHP body can retain up to the
existing 1 MiB body limit only until that deadline. Connection memory/task growth
and time-based admission starvation are bounded, and aggregate request-body bytes
are now bounded by `max_body_memory`, but streaming bodies are not solved here.

FACT: response-body memory is not bounded by anything this milestone adds, and
was measured at 256.6 MiB per in-flight request for a 256 MiB static file. See
*What 0.5 does not bound*. Streaming responses and an alternative body-charging
policy are both recorded debt; neither is implemented here.

## Next dependency

The total buffered-byte question this section used to pose is answered: the
fourth slice added `max_body_memory` for request bodies, and a blanket timeout
around the service future was not used, because it would violate the established
distinction between HTTP cancellation and application transaction cancellation.

What the milestone hands forward, in the order the review ranked it:

- INFERENCE: response streaming is now the largest unbounded resource in the
  server, at a measured 256.6 MiB per in-flight request for a 256 MiB static
  file. A concrete large static file is the right consumer from which to derive a
  streaming response type, which is what this section previously guessed at; the
  measurement now supports it. Static containment and Windows/Linux I/O still
  have to be designed together, so the shape remains unproved.
- The alternative body-charging policy, counting allocated capacity instead of
  the declared length, with the late-refusal cost that comes with it.
- A regression test pinning the observed reclaim of a stalled response write at
  `header_read_timeout`, which is load bearing and currently rests on a
  dependency's internals.
