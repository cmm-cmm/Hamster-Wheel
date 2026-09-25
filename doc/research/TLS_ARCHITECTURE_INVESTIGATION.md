# TLS transport architecture investigation — 2026-09-24

Status: investigation only. The Owner chooses architecture and implementation
scope. Follow-up (2026-09-25): Candidate A was implemented as TLS v1; the
current behavior, the decisions taken and the evidence are in
`doc/TLS_V1.md`, which supersedes this record where they differ. No production TLS code, production dependency, commit, or release is
part of this record. Source and library evidence is separated from proposals.

## FACT — current Hamster boundary

`main.rs` loads and compiles config before `server::run`. Config has one numeric
`server.listen` and an immutable `Arc<RuntimeConfig>`. `server::run` starts
workers, binds one `TcpListener`, and calls `serve_with_router` (`src/server.rs`).
The accept loop admits at most `max_connections`, obtains `peer_addr` and
`local_addr`, constructs a `Copy` `TransportInfo`, and spawns one connection task
in a `JoinSet`. The downstream accept path currently makes no explicit
per-socket `TCP_NODELAY`/receive-buffer change; proxy upstream sockets have
their own configuration. That task builds `service_fn` → `HostRouter::serve` →
`LocationRouter` → `Handler`, then creates a Hyper HTTP/1.1 server connection:

```text
TcpListener::accept
  → TcpStream, peer/local address
  → WriteProgressTimeout<TcpStream>
  → TokioIo
  → hyper::server::conn::http1::Builder::serve_connection
  → service_fn → HostRouter → Handler::{Static, PhpWorker, Proxy}
```

Hyper owns HTTP header parsing and response framing. `header_read_timeout`
begins inside Hyper. `WriteProgressTimeout` measures a raw socket `poll_write`
that returned `Pending`; a positive write resets its deadline. Its
`poll_flush`/`poll_shutdown` merely delegate. On shutdown the listener is
dropped, the watch signal asks active HTTP connections to drain, and each is
bounded by `header_read_timeout`; after all connection tasks leave, proxy idle
connections close and PHP workers shut down. This same header timeout is a
known drain-bound debt, retained from the Reverse Proxy checkpoint.

`TransportInfo` (`src/transport.rs`) means **immediate TCP peer and local socket
identity**, independent of untrusted HTTP headers. `HostRouter` resolves the
effective HTTP authority from the request and normalizes absolute-form Host
before routing. SNI selection occurs earlier and is a different decision.
No handler currently receives TLS metadata.

Integration seams that TLS must change in a later implementation:

* Proxy forwarding removes client-supplied forwarding metadata but then writes
  `X-Forwarded-Proto: http` in `src/proxy.rs`. For an HTTPS client terminating
  at Hamster, the value must come from the accepted downstream transport; an
  HTTP upstream remains a separate matter.
* `WorkerRequest` in `src/php_protocol.rs` protocol version 2 carries peer and
  local addresses but no scheme. `fixtures/php-worker/laravel_request.php`
  hardcodes `REQUEST_SCHEME=http` and `HTTPS=off`. A trusted scheme must cross
  the worker protocol boundary before HTTPS requests can be described
  correctly to Laravel. Its version/compatibility change needs explicit tests;
  forwarding headers supplied by clients cannot substitute for it.
* Static serving needs no encryption branch. PHP and proxy need only the
  trusted connection scheme, not `rustls` types. The current PHP worker,
  reverse-proxy pool, body budgets and authority rules need no redesign.

## FACT — current library behaviour verified

This investigation read official source/API documentation for
[`rustls` 0.23.45](https://docs.rs/rustls/0.23.45/rustls/),
[`tokio-rustls` 0.26.5](https://docs.rs/tokio-rustls/0.26.5/tokio_rustls/),
and [`rustls-pemfile` 2.2.0](https://docs.rs/rustls-pemfile/2.2.0/rustls_pemfile/).
These versions were resolved and `cargo check` succeeded in a scratch crate
under the Windows temp directory, outside this repository. That is a dependency
build check, not a Hamster integration or TLS runtime probe.

* [`TlsAcceptor`](https://docs.rs/tokio-rustls/0.26.5/tokio_rustls/server/struct.TlsAcceptor.html)
  holds `Arc<ServerConfig>` and accepts a Tokio `AsyncRead + AsyncWrite +
  Unpin` stream. Its accept future can be wrapped by `tokio::time::timeout_at`;
  `accept_with` can configure the `ServerConnection` before handshake. On
  cancellation, dropping that future drops its stream/connection state if no
  other owner retains it. `TlsStream` is Tokio `AsyncRead/AsyncWrite` and
  exposes its underlying IO and rustls session through `get_ref`.
* [`TlsStream::poll_write`](https://docs.rs/tokio-rustls/0.26.5/tokio_rustls/)
  can report plaintext accepted while encrypted bytes remain in a rustls
  buffer. A later `poll_flush` is needed to ensure those bytes reach the
  underlying IO. Its `poll_shutdown` sends `close_notify` and then shuts down
  the underlying IO ([source](https://docs.rs/crate/tokio-rustls/0.26.5/source/src/server.rs)).
  Therefore an outer `WriteProgressTimeout<TlsStream<TcpStream>>` would count
  plaintext buffering as network progress; its delegated `poll_flush` can
  stall without the present timer. This **falsifies** that placement for the
  current network-progress meaning.
* A `TlsStream<WriteProgressTimeout<TcpStream>>` lets the existing wrapper
  observe ciphertext writes to the socket. rustls reports a default 64 KiB
  limit on internally buffered unsent plaintext/TLS records; `None` disables
  that limit. This is the relevant library bound, not a measured Hamster TLS
  memory result ([rustls API](https://docs.rs/rustls/0.23.45/rustls/server/struct.ServerConnection.html#method.set_buffer_limit)).
  TLS handshake needs its own total deadline: repeated small progress must
  not keep a handshake alive without limit.
* `ClientHello::server_name()` supplies optional SNI;
  [`ResolvesServerCert`](https://docs.rs/rustls/0.23.45/rustls/server/trait.ResolvesServerCert.html)
  is synchronous, `Debug + Send + Sync`, returns an `Arc<CertifiedKey>` or
  `None` to abort handshake. The built-in
  [`ResolvesServerCertUsingSni`](https://docs.rs/rustls/0.23.45/src/rustls/server/handy.rs.html)
  uses an exact-name map. It has no wildcard lookup, no no-SNI/default
  fallback, and insertion replaces a duplicate unless Hamster rejects one
  while compiling configuration. A small static resolver is needed if those
  policies are required. Certificate selection must not do file or network IO
  inside this synchronous method; `LazyConfigAcceptor` is available for an
  asynchronous selection model, but not needed for preloaded certificates.
* `ServerConfig::alpn_protocols` defaults empty. Rustls selects the first
  configured protocol the client offers; the selected protocol can be read
  after handshake. TLS v1 could advertise only `http/1.1`, and a later `h2`
  branch can use ALPN after handshake. No-ALPN HTTP/1.1 behaviour must be an
  explicit Hamster policy. Do not advertise `h2` until an HTTP/2 engine exists
  ([ServerConfig](https://docs.rs/rustls/0.23.45/rustls/server/struct.ServerConfig.html),
  [ClientHello](https://docs.rs/rustls/0.23.45/rustls/server/struct.ClientHello.html)).
* The default rustls provider path in this candidate is `aws-lc-rs`;
  `tokio-rustls` defaults include `tls12`, `aws_lc_rs`, and `logging`. Current
  rustls supports TLS 1.2 and 1.3, not older protocol versions. Keep the
  provider's cipher/key-exchange defaults rather than making a Hamster list
  without evidence. The default server config has a 256-entry in-memory
  session store, no stateless ticket producer, 0-RTT disabled, and no key log.
  TLS 1.2 can use the bounded session store; TLS 1.3 issues stateful tickets
  backed by that store (two by default) when storage admits them, while
  stateless ticket encryption remains disabled by default
  ([TLS 1.3 server source](https://docs.rs/rustls/0.23.45/src/rustls/server/tls13.rs.html)).
  Do not share resumption state between configs
  with different identities/client-auth policies
  ([features](https://docs.rs/crate/tokio-rustls/0.26.5/features),
  [builder source](https://docs.rs/rustls/0.23.45/src/rustls/server/builder.rs.html),
  [config warning](https://docs.rs/rustls/0.23.45/rustls/server/struct.ServerConfig.html)).
* Rustls's handshake deframer caps a handshake payload at `0xffff` bytes and
  rejects a larger one, limiting one malformed ClientHello's parser memory;
  this does not replace Hamster's total handshake deadline or connection
  admission bound ([deframer source](https://docs.rs/rustls/0.23.45/src/rustls/msgs/deframer/handshake.rs.html)).
* `with_single_cert` validates DER/key compatibility and mismatch, while a
  PEM parser only decodes PEM blocks. A complete chain, SAN coverage, expiry,
  not-yet-valid dates and trust by real clients are distinct checks; merely
  constructing `ServerConfig` does not prove them all
  ([builder API](https://docs.rs/rustls/0.23.45/rustls/struct.ConfigBuilder.html)).

## FACT — dependency and platform cost

The scratch `Cargo.toml` pinned only `rustls=0.23.45`,
`tokio-rustls=0.26.5`, and `rustls-pemfile=2.2.0`. `cargo tree --target
x86_64-pc-windows-msvc --edges normal,build` includes `aws-lc-rs`,
`aws-lc-sys`, `rustls-webpki`, `rustls-pki-types`, `zeroize`, `subtle`,
`untrusted`, and build crates including `cmake` and `cc`. Tokio is already a
Hamster dependency. This is a **build-time native toolchain cost** for the
default provider. The Windows MSVC scratch `cargo check` passed in 20 s on a
warm/partly cached machine; that is not a cold-build timing claim. No
production `Cargo.toml`/`Cargo.lock` change was made. Link-time binary delta,
runtime memory delta, and whether an actual deployed binary needs anything
besides ordinary OS facilities are **not measured**. A `ring` provider is a
candidate only if the Windows build/deployment trade-off warrants it; it was
not compiled here. Native Linux remains **NOT VERIFIED**.

Certificate and key paths should be resolved relative to the configuration
file and loaded before binding. A Windows service may run with a different
working directory; relying on process cwd would be fragile. PEM key access
and file permissions are operator responsibilities; private key contents must
never enter diagnostics. A filesystem PEM model does not require Windows
certificate-store integration. No Windows-specific API is needed in the core
TLS path.

## INFERENCE — boundary and lifecycle

Recommended investigation candidate: the listener owns a TLS mode and an
immutable, preloaded `Arc<ServerConfig>`. On accept, capture socket identity,
preserve the selected socket policy, wrap the **socket** with `WriteProgressTimeout`, then
perform TLS handshake with a separate **total** timeout. Pass either the raw
wrapped stream or the completed TLS stream into one small generic HTTP serving
function. The cleartext branch directly enters that function, so it has no
per-request TLS dispatch. This is a proposal, not an implementation decision.

```text
cleartext: TcpStream → WriteProgressTimeout → TokioIo → HTTP engine
TLS:       TcpStream → WriteProgressTimeout → rustls handshake/TlsStream
                    → TokioIo → HTTP engine
```

The lower wrapper measures ciphertext accepted by the OS socket, which is the
same progress surface as the current cleartext path. It does **not** prove the
peer received/decrypted it. The TLS handshake total timeout covers a client
that sends one byte at a time forever. Ordinary Hyper header timeout should
start only after TLS succeeds. During HTTP serving, the connection task owns
the TLS stream and Hyper future; dropping it drops TLS/socket state. The
existing bounded HTTP drain remains the controlling shutdown deadline. On
normal completion, TLS `close_notify` can be attempted, but a stalled peer
must not keep shutdown beyond that bound. Whether Hyper explicitly polls TLS
shutdown on every end path needs an integration test; do not assume it.

State/ownership model:

| State | Owner and cancellation result |
|---|---|
| ACCEPTED | accept loop hands socket and immutable listener config to one admitted task; drop closes socket |
| TLS_HANDSHAKING | task owns accept future, socket, rustls state and total timer; timeout/shutdown drops all; no HTTP response exists |
| TLS_READY | task owns `TlsStream` and trusted connection scheme; failed handoff drops stream |
| HTTP_SERVING | Hyper future owns IO and service; handler gets trusted context; TLS read/write error ends connection, with normal handler cancellation contracts |
| DRAINING | stop new accepts/requests, allow current exchange until existing bounded drain expires, then drop IO/handler future |
| CLOSED/TLS_ERROR | task exits `JoinSet`; no detached TLS driver exists in `tokio-rustls` |

The `JoinSet` admission limit bounds live handshake and HTTP tasks together.
It does not stop an attacker from occupying all slots with slow handshakes
until timeout, so the timeout and operational counters are still necessary.
Rely on rustls for record parsing, protocol/cipher policy and malformed
handshake rejection. Hamster owns accept admission, total handshake deadline,
SNI lookup cost, bounded diagnostics, startup certificate checks and shutdown.
Do not label counters by raw SNI, path, or arbitrary client input; aggregate by
configured listener and failure category.

## INFERENCE — identity, routing, and configuration

TLS certificate selection should use SNI only as a **certificate identity
hint**. The HTTP request's effective authority still chooses `HostRouter`.
Literal equality of SNI and Host is too restrictive for certificates that
cover multiple origins and future HTTP/2 connection coalescing; unconditional
cross-host routing can expose a virtual host across tenant boundaries. RFC
9110 requires an HTTPS origin response to be authoritative for the request
target and describes misdirected requests
([RFC 9110 §7.4](https://www.rfc-editor.org/rfc/rfc9110.html#section-7.4)).
Policy therefore needs an explicit authorization relation between the
selected certificate/listener and effective authority, not an assumption that
SNI equals Host. A request can be rejected **after** TLS/HTTP parsing when
that relation fails. `421` is an origin-server signal, but RFC 9110 says a
proxy must not generate `421`; the routing boundary and status need role-aware
design. The Owner must choose the concrete policy.

For `SNI=a.example.com`, `Host=b.example.com`: independent routing works if
the listener is configured as authoritative for `b` and the selected
certificate covers it; otherwise reject before `Handler`. For no SNI, an
explicit default certificate can be used and then the same authority policy
applied. For unknown SNI, failing handshake is the safer default unless a
default certificate is deliberately configured. A wildcard entry should match
one label only (`*.example.net` → `x.example.net`, not `example.net` or
`x.y.example.net`); exact names take precedence. A resolver can store owned
`Arc<CertifiedKey>` chains/keys in a static map and return cloneable `Arc`s.
Its lookup must be bounded and deterministic. This remains a design proposal;
the built-in rustls resolver does not implement wildcard/default selection.

Minimal **candidate config shape**, not final syntax:

```toml
[[listeners]]
address = "0.0.0.0:443"

[listeners.tls]
default_certificate = "site"
handshake_timeout = 10 # seconds; example only, not a selected default

[[listeners.tls.certificates]]
id = "site"
names = ["example.com", "api.example.com", "*.example.net"]
cert = "certs/site-chain.pem"
key = "certs/site-key.pem"
```

The TLS mode, ALPN and handshake timer belong to the listener. Certificates
are identities referenced by its resolver. Virtual hosts are HTTP routing
config, though config validation should verify which listeners are allowed to
serve each host. The current single `server.listen` could evolve to one
optional TLS mode without multiple listeners in initial TLS v1; multi-listener
schema is a separate product choice. All configured TLS material should be
loaded and validated **before any listener bind**, avoiding partially started
multi-listener state. Duplicate exact/wildcard mapping and missing default
must be explicit validation failures, with listener/cert/path identity in
errors. Never print key contents.

Loading policy proposal: fail startup on unreadable/missing files, malformed
PEM/DER, missing/unsupported/encrypted private key, key mismatch, empty leaf,
invalid configured SNI name, duplicate mapping, or chain material that cannot
be parsed. Check configured names against leaf SAN where the API supports it.
Expiry, not-yet-valid dates, incomplete chain and real-client trust require a
clear operator policy and independent validation; they cannot be inferred
from `with_single_cert` success. A later implementation can choose fail or
warning for time validity, with tests and clear startup diagnostics. Certificate
renewal/ACME and dynamic reload are outside this investigation.

`TransportInfo` should keep socket identity semantics. A small adjacent
connection context can carry a trusted `Scheme::{Http,Https}` and, if an
actual consumer needs them, SNI/ALPN metadata. Do not put a `rustls` type or
untrusted request header in it. Pass scheme once from listener/handshake to
router/handler, then derive proxy `X-Forwarded-Proto` and PHP worker scheme
from that value. Avoid separate guesses by each handler. A future resolver
could hold an atomically swapped immutable certificate map behind a stable
`Arc<ServerConfig>`; this only preserves a reload seam. It does not implement
reload or assume old session resumption remains safe after identities change.

## Candidate comparison

| | A: listener branch + common HTTP function | B: generalized transport enum/trait |
|---|---|---|
| Flow | accept → clear/TLS setup → generic HTTP function | accept → transport abstraction → HTTP function |
| Ownership | listener owns `Arc<ServerConfig>`; task owns one stream; no TLS driver | enum/trait owns stream and TLS state; potentially more boxed/generic plumbing |
| Cleartext cost | one connection-setup branch; no per-request TLS branch | may add enum dispatch or trait boxing in hot path |
| Failure containment | handshake failure ends one admitted task | same if abstraction is correct; more state transitions to audit |
| SNI/config | listener certificate resolver; HTTP host stays separate | same resolver still required; abstraction does not solve identity policy |
| Reload/h2 | resolver can evolve; ALPN selects a later HTTP engine after handshake | transport abstraction alone does not select HTTP engine or reload certs |
| Windows/Linux | Tokio/rustls common path | same dependencies, more code to maintain/test |

Candidate A is the current source-shaped recommendation for a **first** TLS
implementation. Candidate B is meaningful if several transport types later
need distinct runtime behaviour, but today it adds machinery without solving
SNI, scheme or ALPN. A small generic `serve_http<T: AsyncRead + AsyncWrite +
Unpin + Send>` is a shared function, not a general transport framework.
Neither candidate changes Reverse Proxy Candidate A (`location → Arc<Upstream>
→ Hamster HTTP/1.1 pool → hyper::client::conn::http1`). Upstream TLS is a
separate future client-side capability.

## Backpressure and memory model

For a slow TLS client, Hyper's response producer awaits IO, `tokio-rustls`
buffers at most its configured record/plaintext allowance on the write side,
and socket backpressure eventually reaches `TlsStream::poll_write`/flush.
Putting `WriteProgressTimeout` under TLS bounds stalled **socket writes**.
This supports a bounded-by-connection-and-buffer **model**, but no Hamster TLS
streaming memory measurement or 1 GiB slow-client probe was run. The default
rustls buffer limit applies to application writes, not a universal bound on
every allocation in the TLS handshake/parser. A production implementation
must test slow upload, slow download, stalled handshake, response drain and
large payloads before carrying over Reverse Proxy's memory claim to HTTPS.

Rough resident model:

```text
per accepted connection = kernel TCP buffers + rustls connection/record state
                        + Hyper HTTP buffers + active handler/body state
per listener            = Arc<ServerConfig> + certificate resolver + session cache
per certificate         = parsed chain + private key + lookup names
per payload             = streamed frames, not whole payload, if all stages await backpressure
```

The 256-session default cache is bounded in entries. Certificate count is
configuration bounded, not request bounded. `max_connections` bounds accepted
connection tasks. Exact heap bytes, kernel buffers, CPU, handshake latency,
keep-alive savings and binary size have not been measured. A future scratch
probe should compare cleartext/new-connection, TLS/new-connection, and TLS
keep-alive; then a 1 GiB response to a slow/stalled TLS reader with private
memory, socket and task counts. Do not turn a loopback result into a network
claim. No prototype was left in production source.

## Failure semantics proposal

| Event | Proposed action | HTTP response? | Diagnostics |
|---|---|---|---|
| TCP accepted, no completed handshake by total deadline | drop stream/task | no | aggregate timeout counter; rate-limited listener log |
| malformed/oversized ClientHello or bad TLS record during handshake | rustls alert/error, close | no | bounded category counter; no raw ClientHello/SNI log |
| unsupported version/cipher/ALPN | rustls negotiation failure, close | no | bounded category counter |
| unknown SNI/no SNI without configured default/cert resolver miss | fail handshake | no | configuration category counter; avoid raw SNI label |
| client disconnect in handshake | drop state | no | low-noise category counter |
| TLS record error during HTTP request | end HTTP connection and handler future | usually no reliable response | bounded connection error record |
| TLS write failure during response | drop connection/body; preserve handler cancellation contract | response may already be committed | bounded connection error record |
| shutdown during handshake | cancel handshake task and close socket | no | normal shutdown accounting |
| shutdown during active HTTPS request/backpressured response | Hyper graceful drain, then current finite deadline drops TLS/socket/body | response may be partial | normal drain/timeout accounting |

Before handshake completion, ordinary HTTP 4xx/5xx is impossible. Rustls owns
TLS protocol rejection; Hamster owns HTTP commit semantics after handshake.
Do not synchronously log each hostile ClientHello. No detached handshake or
TLS driver tasks are proposed. A valid ClientHello still consumes CPU and
memory while admitted; deadline and `max_connections` are the primary
existing-style bounds. A separate rate/admission policy would need evidence.

## Proposed TLS v1 scope for Owner consideration

Smallest production-quality candidate: one HTTPS listener mode; preloaded PEM
leaf/chain + private key; rustls TLS 1.2/1.3 defaults; HTTP/1.1 ALPN and an
explicit no-ALPN policy; total handshake deadline; trusted connection scheme;
proxy/PHP scheme integration; startup validation before bind; SNI selection
with exact names and a documented no-SNI/unknown policy; bounded error
logging; shutdown/drop and backpressure tests. Multiple SNI certificates are
worth including **if** one listener must serve multiple named hosts at first
launch; otherwise a single certificate with an explicit resolver seam may be
the smaller first increment. The Owner decides this product requirement.

Deliberately outside this candidate: ACME/Let's Encrypt automation, hot
certificate/config reload, OCSP stapling, mTLS, upstream TLS, HTTP/2, HTTP/3,
QUIC, Windows certificate store, automatic HTTP→HTTPS redirect, HSTS policy,
and certificate management UI. These are future features, not silently
implemented dependencies.

## HYPOTHESIS / NOT VERIFIED

* TLS application backpressure appears structurally bounded by rustls's
  default buffer plus Hyper/handler limits, but the combined Hamster path has
  no large-response/slow-client runtime probe. Every shutdown and EOF race has
  not been exercised.
* New-connection TLS latency, CPU, resident memory, binary-size impact and
  session-resumption benefit are unmeasured. Windows loopback performance
  research from Reverse Proxy v1 says nothing conclusive about TLS.
* Native Linux build/test/runtime and real-NIC behaviour remain NOT VERIFIED.
  The Linux cross-target is not installed; none was installed for this task.
* Future `h2` ALPN dispatch and atomic certificate reload are compatible in
  concept with Candidate A, but require implementation and tests. They are
  not proven by this investigation.

## Owner review points

1. Should TLS be listener-owned with a certificate resolver mapping to hosts,
   as proposed, or should a different product/config ownership rule apply?
2. Is the common generic HTTP serving function sufficient for TLS v1, leaving
   a generalized transport enum/trait until a second concrete need exists?
3. Should `WriteProgressTimeout` wrap the TCP socket **under** TLS to continue
   observing ciphertext writes, with separate flush/shutdown tests?
4. Should the handshake have a total deadline from accept, separate from HTTP
   header and socket write-progress timeouts? What configured/default value?
5. Must TLS v1 support multiple exact/wildcard SNI certificates and a default
   certificate immediately, or is one cert enough for the first product slice?
6. Should a trusted `Scheme` in a small connection context be the single
   source for proxy and PHP worker metadata while `TransportInfo` stays socket
   identity only?
7. Can handlers remain TLS-independent except for trusted scheme metadata?
   The source shows no other required handler-level TLS detail today.
8. Does Candidate A's cleartext branch and ALPN-after-handshake seam meet the
   desired simplicity/HTTP2/reload balance?
9. What slow-client/large-stream TLS probe is required before claiming HTTPS
   preserves bounded memory and backpressure? Current evidence is structural
   only.
10. Is the proposed TLS v1 scope the smallest production-quality slice for
    Hamster's expected multi-host deployment and certificate policy?

Separate policy choice: when SNI and effective HTTP authority differ, which
certificate/host authorization relation should Hamster enforce, and which HTTP
status is correct for origin versus gateway roles? Do not infer acceptance of
cross-host routing from the current HostRouter alone.
