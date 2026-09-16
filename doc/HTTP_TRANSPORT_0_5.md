# HTTP and Transport Foundation 0.5

Status: immediate transport identity is present in source. The second 0.5.x
slice, bounded HTTP/1 admission, is implemented in the working tree and awaits
owner review. It has not been committed, tagged or released. The package
version remains unchanged until that review.

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

## Deliberate non-scope

These slices do not change protocol v1, `REMOTE_ADDR`, request-body timeout or
buffering, static-file implementation, proxy trust, TLS, HTTP/2, observability
export or framework lifecycle. Those are not implied by transport identity or
connection admission.

FACT: the transport value itself represents IPv4 and IPv6, but the existing
Host router does not accept bracketed IPv6 literal authorities. That is a
separate HTTP authority/configuration correctness gap; this slice does not
claim IPv6 virtual-host support.

FACT: request-body reads remain outside the header deadline. A slow PHP body can
hold one of the bounded connection slots and retain up to the existing 1 MiB
body limit. Connection memory/task growth is bounded, but admission starvation
and a total buffered-byte budget are not solved by this slice.

## Next dependency

INFERENCE: a later owner-selected slice must decide body-read time semantics and
a total buffered-byte budget before a streaming backend is added. A blanket
timeout around the service future would violate the established distinction
between HTTP cancellation and application transaction cancellation, so this
cannot safely be treated as another Hyper connection timer.

HYPOTHESIS: after connection/request bounds exist, a concrete large static-file
body is the right consumer from which to derive a streaming response type.
That remains unproved until the static containment and Windows/Linux I/O
designs are tested together.
