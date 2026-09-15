# HTTP and Transport Foundation 0.5

Status: the first 0.5.x slice is implemented in the working tree and awaits
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

## Deliberate non-scope

This slice does not change protocol v1, `REMOTE_ADDR`, request limits, timeout
policy, response buffering, static-file behavior, proxy trust, TLS, HTTP/2,
FastCGI, observability export or framework lifecycle. Those are not implied by
the presence of two socket addresses.

FACT: the transport value itself represents IPv4 and IPv6, but the existing
Host router does not accept bracketed IPv6 literal authorities. That is a
separate HTTP authority/configuration correctness gap; this slice does not
claim IPv6 virtual-host support.

FACT: the accept loop still has no concurrent-connection admission bound and
Hyper connections have no configured header/read/idle time policy. A malformed
connection is isolated, but arbitrarily many slow connections are not yet
bounded. The existing synchronous per-connection error diagnostic is also not
a production logging/backpressure design.

## Next dependency

INFERENCE: the next 0.5.x slice should establish bounded HTTP connection and
request admission before response/static work: concurrent-connection limits,
header/read/idle time bounds and explicit Hyper parser limits. It should be
fault-injected with slow and partial clients and measured independently. This
is a proposal for owner review, not work started by this slice.

HYPOTHESIS: after connection/request bounds exist, a concrete large static-file
body is the right consumer from which to derive a streaming response type.
That remains unproved until the static containment and Windows/Linux I/O
designs are tested together.
