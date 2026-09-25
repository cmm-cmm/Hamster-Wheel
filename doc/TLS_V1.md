# TLS v1

Status: implemented in the working tree on 2026-09-25, for Owner review. Not
committed, tagged, released or accepted. The investigation it follows is
`doc/research/TLS_ARCHITECTURE_INVESTIGATION.md`; where this document and the
investigation differ, this document and the source are current. Measurements
are in `benchmarks/results/tls_v1_20260925.md`.

## Scope

An optional HTTPS listener terminates TLS in front of the same virtual hosts,
routers and handlers as the cleartext listener.

```toml
[server]
listen = "0.0.0.0:80"         # optional once [server.tls] exists

[server.tls]
listen = "0.0.0.0:443"
handshake_timeout = 10        # seconds, 1-60, total from accept (default 10)

[[server.tls.certificates]]
cert = "certs/site-chain.pem" # PEM: leaf first, then intermediates
key = "certs/site-key.pem"    # unencrypted PKCS#8, PKCS#1 or SEC1
names = ["example.com", "*.example.net"]
default = true                # at most one: used for no SNI or unknown SNI
```

In v1: HTTP/1.1 over TLS 1.2 and 1.3; rustls 0.23 with the aws-lc-rs provider
and its default cipher suites and key exchange groups; ALPN advertising only
`http/1.1`; PEM certificate chains and keys loaded once at startup; SNI
selection over exact names and one-label wildcards with an optional default;
a total handshake deadline; graceful shutdown and cancellation; a trusted
connection scheme for the reverse proxy and the PHP worker protocol.

Not in v1, deliberately: ACME, certificate reload, OCSP stapling, client
certificates (mTLS), upstream TLS, HTTP/2, HTTP/3, a Windows certificate store,
automatic HTTP to HTTPS redirect, HSTS, per-host listener binding and an
SNI/Host authorization policy (see Owner review points).

## Architecture

TLS is a capability of the listener. The cleartext and TLS paths converge on
one HTTP serving function; below it nothing knows which listener accepted the
connection except through the trusted scheme.

```text
cleartext: TcpStream -> WriteProgressTimeout -------------------------> serve_http
TLS:       TcpStream -> WriteProgressTimeout -> rustls handshake -> TlsStream -> serve_http
serve_http<I: AsyncRead + AsyncWrite>: TokioIo -> Hyper http1 -> HostRouter(ConnectionInfo)
```

- `src/tls.rs` owns everything TLS: validated names (`TlsName`), certificate
  loading and checks, the SNI resolver, the `TlsListener` (address, one
  immutable `TlsAcceptor`, handshake deadline) and handshake failure counters.
  No other module imports rustls.
- `src/server.rs` binds every configured listener before serving any, then runs
  one accept loop over them (`serve_listeners`). A listener is a bound socket
  plus `Option<Arc<TlsListener>>`. Accept rotates which listener is polled
  first so a flood on one cannot starve the other. `serve_with_router` remains
  the cleartext entry point every existing test and probe uses.
- `src/transport.rs` keeps `TransportInfo` as socket identity only and adds
  `Scheme::{Http, Https}` and `ConnectionInfo { transport, scheme }`, a `Copy`
  value passed once from the listener to the router and every handler. It holds
  no rustls type and nothing read from a request.

There is no transport trait or enum dispatch: the only per-connection branch
is `Option<Arc<TlsListener>>` at accept, and `serve_http` is generic. The TLS
branch is one boxed future (`serve_tls`): inline, it sized every connection
task for the rustls handshake and stream state, which measured about 4 KiB
more per idle cleartext connection; boxed, cleartext is back to the pre-TLS
cost and a TLS connection pays one allocation.

### Where the write deadline sits

`WriteProgressTimeout` wraps the TCP socket beneath TLS. It therefore observes
ciphertext the kernel accepted: handshake flights, application records and
`close_notify`. Above TLS it would see `TlsStream::poll_write` accept plaintext
into rustls's buffer as progress, and its delegated `poll_flush` and
`poll_shutdown` would wait on a peer that stopped reading with no deadline at
all. Two tests pin this, and a mutation that moves the wrapper above TLS is
caught by the server-level test (see Verification).

### Connection lifecycle

| State | Owner, bound and cancellation |
|---|---|
| accepted | accept loop admits it under `max_connections` (shared by both listeners), records the accept instant, spawns one task |
| handshaking | the task owns the socket, rustls state and a deadline at `accept + handshake_timeout`; shutdown cancels at once; any ending drops everything, no HTTP response exists |
| serving | Hyper owns the `TlsStream` and the service; handlers see `ConnectionInfo` with `Scheme::Https`; all existing HTTP limits and handler contracts apply unchanged |
| draining | the existing graceful path: `graceful_shutdown`, bounded by `header_read_timeout`, then drop |
| closed | Hyper's shutdown sends `close_notify` through the write deadline; a peer that stopped reading cannot hold it |

The handshake deadline is total, not an inactivity timer: a test trickles one
byte every 50 ms into a 400 ms deadline and the connection closes at about
400 ms. It starts at accept, so a connection waiting in the kernel backlog
while `max_connections` is saturated has not started its deadline. Because a
handshaking connection holds an admission slot, the deadline is what bounds how
long silent peers can occupy the server; the default is 10 s and the range 1-60.

`serve_http` now waits for shutdown with `watch::Receiver::wait_for`, which also
sees a stop signalled while the handshake was completing; `changed()` could
have missed it.

## Certificates and SNI

`names` are the SNI names that select a certificate: exact DNS names, or `*.`
plus a parent domain of at least two labels, matching exactly one extra label
(`*.example.net` covers `a.example.net`, not `example.net` or
`a.b.example.net`). Exact names win over wildcards. Names are compared
ASCII-case-insensitively and a trailing dot in the client's SNI is ignored. A
name may appear once across all certificates. IP addresses are rejected as
names because clients never send them as SNI; serve an IP-addressed site with
the default certificate.

A handshake with no SNI, or with an SNI no entry matches, uses the certificate
marked `default = true`; without one it fails. Resolution is a static map built
at startup: at most two hash probes, no I/O, as required by rustls calling the
resolver synchronously inside the handshake.

SNI selects the certificate only. The effective HTTP authority (Host, or an
absolute-form target) still selects the virtual host, exactly as on the
cleartext listener, and every host is reachable on both listeners. A test
completes a handshake as `a.test` and sends `Host: x.b.test`; it is served by
`x.b.test`. This is the current behavior, not an accepted policy: see Owner
review points.

At startup, before any worker starts or any socket binds, each certificate is
loaded and checked. Failures are `Compile` errors naming the field and path:

- a file that cannot be read; certificate PEM that is malformed or holds no
  `CERTIFICATE` block;
- a key file with no readable PKCS#8, PKCS#1 or SEC1 key, or an encrypted key
  (reported as such); the key's PEM error is never displayed, because it can
  quote a line of the file;
- a key that does not match the leaf, or a key type the provider cannot load;
- a configured name the leaf's subject alternative names do not cover. A
  wildcard name is checked with a probe label, so a certificate listing a few
  explicit subdomains does not pass as a wildcard.

Not checked: validity dates (an expired or not-yet-valid certificate starts),
chain completeness and trust by real clients. rustls would need trust anchors
to verify a chain, and a server does not have the client's. These are
documented limitations and an Owner point, not silent guarantees.

## Protocol and ALPN

`ServerConfig` is built with `builder_with_provider(aws_lc_rs)` and
`with_safe_default_protocol_versions`: TLS 1.3 and 1.2, rustls's default
suites and groups, no client authentication. rustls's defaults are kept: a
256-entry in-memory session cache, TLS 1.3 resumption through it, no stateless
ticket key, 0-RTT disabled, no key log.

ALPN advertises only `http/1.1`. A client offering ALPN without it (h2 only)
fails the handshake with rustls's `no_application_protocol`. A client offering
no ALPN gets HTTP/1.1. No HTTP/2 engine exists, so `h2` is never advertised.

## Trusted scheme

`ConnectionInfo::scheme` is `Https` only when Hamster completed a TLS handshake
on that connection. Nothing a client sends can produce it: not
`X-Forwarded-Proto`, not `Forwarded`, not an absolute-form `https://` target.

- Reverse proxy: `X-Forwarded-Proto` is the connection's scheme. Client-supplied
  forwarding headers are still removed first; the upstream hop stays cleartext.
- PHP worker: protocol v3 adds `scheme u8` (`1` http, `2` https) after the HTTP
  version. `READY` must advertise 3; a v1 or v2 worker fails startup before the
  listener binds, and the PHP decoder rejects any other scheme value. The
  Laravel adapter sets `REQUEST_SCHEME` from it and `HTTPS=on` for https, so
  Symfony's `isSecure()`, `getScheme()` and default port follow the
  connection. Forwarding headers remain ordinary headers for the application's
  trusted-proxy policy. See `doc/APPLICATION_REQUEST_METADATA.md`.
- Static files need no scheme.

## Failure semantics

| Event | Result | Counted as |
|---|---|---|
| no complete handshake by the deadline, including a trickling peer | socket dropped, no response | `timed_out` |
| handshake flight not read by the peer for `write_progress_timeout` | socket dropped | `timed_out` |
| cleartext HTTP, malformed record, no common version/suite, ALPN mismatch, SNI with no certificate | rustls alert, close | `rejected` |
| peer closes or resets during the handshake | dropped | `abandoned` |
| shutdown during a handshake | cancelled at once | not counted |
| TLS read error or close without `close_notify` while serving | connection ends | not logged (peer-controlled) |
| client stops reading a TLS response | socket write deadline ends it and frees the slot | existing write-progress path |
| shutdown with a TLS response stalled on the client | existing drain bound (`header_read_timeout`) | existing path |

Handshake failures are aggregated per listener. The first rejected or timed-out
handshake is reported immediately and then at most one line per 60 s, giving
all three counts and the most recent rustls error; raw SNI and ClientHello
bytes are never logged. Abandoned handshakes (browsers routinely open
speculative connections) are counted but never trigger a report.

## Verification

FACT, Windows native (`x86_64-pc-windows-msvc`), real loopback sockets and a
throwaway CA from `rcgen` (dev-dependency only):

| Boundary | Permanent test |
|---|---|
| both listeners, one router, each its own scheme; keep-alive over TLS; forged `X-Forwarded-Proto` ignored; `close_notify` on `Connection: close` (rustls reports a close without it as an error) | `server::tls_tests::both_listeners_share_one_router_and_each_carries_its_own_trusted_scheme` |
| SNI: exact, wildcard, one label only, unknown and absent SNI use the default; a verifying client accepts the wildcard chain; Host, not SNI, routes | `sni_selects_the_certificate_while_the_host_header_selects_the_virtual_host` |
| without a default, unknown and absent SNI fail the handshake and are counted as rejected | `without_a_default_certificate_an_unknown_or_absent_sni_fails_the_handshake` |
| ALPN: h2-only refused, h2+http/1.1 selects http/1.1, no ALPN serves HTTP/1.1 | `the_listener_negotiates_http_1_1_or_nothing` |
| total handshake deadline against a peer trickling a byte every 50 ms; the held admission slot (`max_connections = 1`) frees at the deadline | `a_trickled_handshake_is_dropped_at_its_total_deadline_and_frees_its_slot` |
| shutdown cancels a handshake with a 30 s deadline at once | `shutdown_cancels_a_pending_handshake_without_waiting_for_its_deadline` |
| cleartext HTTP on the TLS port never gets an HTTP response; the listener keeps serving | `cleartext_http_sent_to_the_tls_listener_receives_no_http_response` |
| a TLS client that stops reading a 32 MiB response is dropped by the socket write deadline, its slot and file handle released | `a_tls_client_that_stops_reading_is_released_by_the_socket_write_deadline` |
| graceful shutdown with a TLS download stalled on the client is bounded by the drain deadline and releases the file | `graceful_shutdown_bounds_a_tls_download_stalled_on_the_client` |
| a peer that vanishes mid-handshake is `abandoned`, not `rejected` | `a_tls_peer_that_vanishes_mid_handshake_is_counted_as_abandoned` |
| a flush stalled beneath TLS times out (40 KiB fits rustls's buffer, not the 16 KiB pipe) | `a_flush_stalled_beneath_tls_times_out_instead_of_waiting_forever` |
| an HTTPS bind failure releases the bound cleartext socket; `run` fails instead of serving on one listener | `server::tests::a_tls_bind_failure_leaves_no_listener_serving` |
| proxy forwards `X-Forwarded-Proto: https` for a TLS client claiming http | `proxy::tests::an_https_client_is_forwarded_with_the_scheme_its_listener_established` |
| name parsing, one-label wildcard lookup, rate-limited reporting | `tls::tests::*` (3) |
| config: TLS with and without cleartext; 21 rejected configurations (listeners, timeouts, names, duplicates, defaults, unknown fields, unreadable/malformed/empty PEM, missing/encrypted/mismatched key, uncovered exact and wildcard names), none leaking key material | `tests/configuration.rs` `compiles_a_tls_listener_with_or_without_a_cleartext_listener`, `rejects_tls_configuration_that_cannot_be_served` |
| protocol v3 byte layout; READY v0/1/2/4/max rejected | `php_protocol::tests` |
| PHP worker receives `https` from the TLS listener and `http` from the cleartext one in the same W1 process; forged headers ignored; local port is the TLS port | `tests/php_worker.rs::protocol_v3_carries_the_listener_scheme_not_a_forwarding_header` (ignored: needs `php/8.5/php.exe`) |
| real Laravel/Symfony over HTTPS only (no cleartext listener): `REQUEST_SCHEME=https`, `HTTPS=on`, `getScheme()=https`, default port 443, a forged `X-Forwarded-Proto: http` from an untrusted peer ignored | `laravel_sees_https_from_the_tls_listener_and_not_from_a_forged_header` (ignored: needs a prepared scratch PMS) |
| PHP decoder: http/https values, 0 and 3 rejected; Laravel server-bag mapping | `fixtures/php-worker/protocol_test.php`, `laravel_request_test.php` |

Gates, Windows native, final tree: `cargo fmt --check`, strict `clippy
--all-targets --all-features -D warnings`, `cargo test` (135 lib, 26
configuration, 7 routing, 5 static), MSVC release build and `cargo bench
--no-run --features profiling` pass. `cargo test -- --ignored` with
`HAMSTER_LARAVEL_TEST_PATH` set to a fresh scratch copy of the prepared PMS:
lib 9, multipool 9, visibility 1, timeout 10, containment 4 and 60 of 64
`php_worker` tests pass, including both new TLS PHP tests and the existing
Laravel metadata probe. The 4 failures: the ConPTY `ctrl_c` harness gap
already documented, two Laravel tests whose scratch vendor tree lacks
`predis/predis/src/Client.php` (also missing in the copy it came from), and
the decoder check-count assertion, which this change had made stale (14 to 18
checks) and which now passes. The TLS, proxy and server unit tests passed 10
of 10 and then 50 of 50 sequential runs, and 40 of 40 under two parallel test
processes; one earlier sequential run of that set failed once without
reproducing, and under parallel load the only failure seen was the existing
cleartext `midstream_response_error_closes_connection_and_server_recovers`,
whose client leaves a pipelined request unread so the close can be an RST.
`cargo check --target x86_64-unknown-linux-gnu` cannot run: the target is not
installed. Native Linux: NOT VERIFIED.

Mutation check (each applied alone, the named tests run, the file restored
and its hash compared): 10 of 10 caught.

| Mutation | Caught by |
|---|---|
| T1 handshake deadline one hour | trickled-handshake test |
| T2 shutdown does not cancel a handshake | shutdown-during-handshake test |
| T3 `WriteProgressTimeout` above TLS instead of beneath | stops-reading test (the stall is not released) |
| T4 `X-Forwarded-Proto` hard-coded `http` | proxy HTTPS test |
| T5 PHP scheme hard-coded `Http` | PHP v3 scheme test |
| T6 resolver ignores the default | SNI test |
| T7 no ALPN advertised | ALPN test |
| T8 wildcard matches any depth | SNI test |
| T9 names not checked against the certificate | config rejection test |
| T10 socket write deadline one hour | stops-reading test |

T3 falsified an assumption made while writing the tests: that above TLS the
deadline would still see `poll_write` return `Pending` once rustls's buffer
filled, so only the flush path would differ. With the wrapper above TLS the
stalled 32 MiB download was never released; beneath TLS it is. The placement is
therefore pinned by both the unit-level flush test and the server-level test.

Found and fixed during measurement: over TLS, Hyper's shutdown writes
`close_notify`, so a client that resets an idle keep-alive connection produced
one `error shutting down connection` stderr line per connection (984 in one
benchmark run); cleartext stays silent in the same case. Shutdown errors whose
cause is a reset, aborted, broken or unconnected peer are no longer reported,
like the other peer-controlled connection errors. Verified by a temporary
probe (removed) and by the benchmark's stderr; there is no permanent test
because the report goes to stderr.

## Performance and memory

FACT (release, Windows loopback, static files, three runs per cell; full
tables in `benchmarks/results/tls_v1_20260925.md`). The bench client does its
own TLS work on the same machine, and session-to-session noise reached 20%,
so only same-table ratios are stated:

| Measure | Cleartext | TLS | TLS / cleartext |
|---|---:|---:|---:|
| keep-alive 23 B, C1, RPS (two sessions) | 5,705; 7,350 | 5,816; 6,996 | 0.95-1.02 |
| keep-alive 23 B, C32, RPS | 10,645; 11,972 | 10,045; 12,379 | 0.94-1.03 |
| keep-alive 1 MiB, C1, MiB/s | 1,298; 1,426 | 658; 1,001 | 0.51-0.70 |
| keep-alive 1 MiB, C32, MiB/s | 3,333; 4,348 | 2,926; 3,335 | 0.77-0.88 |
| keep-alive 64 MiB, C1, MiB/s | 1,580; 1,637 | 1,038; 1,114 | 0.66-0.68 |
| keep-alive 64 MiB, C32, MiB/s | 3,874; 4,652 | 3,302; 4,209 | 0.85-0.90 |
| Hamster CPU per 64 MiB, C1 | 43.2 ms | 64.7 ms | 1.50 |
| new connection per request, C1, conn/s | 1,668 | 651 full, 679 resumed | 0.39 |
| new connection per request, C8, conn/s | 6,603 | 3,965 full, 4,289 resumed | 0.60-0.65 |
| Hamster CPU per new connection, C1 | 344 us | 786 us full | 2.3 |
| idle keep-alive connection | 25.5 KiB | 38.5 KiB | +13 KiB |
| stalled 1 GiB download, per connection (N=64) | 290 KiB | 243 KiB | not larger |
| 1 GiB to a 7 MiB/s reader, private max during | 1.94 MiB | 2.23 MiB | flat |
| 1 GiB at full speed, peak commit | 1.97 MiB | 2.21 MiB | 1.09 s vs 0.79 s |

INFERENCE: small keep-alive requests cost about the same over TLS; large
bodies pay for encryption (about 50% more Hamster CPU per byte, and less
throughput where one connection is CPU bound); a new connection costs the
handshake, about 0.45 ms of Hamster CPU. TLS adds no buffering that grows with
the response: a stalled or slow TLS reader holds no more than a cleartext one,
because backpressure reaches rustls's bounded buffer and the socket write
deadline beneath it. Resumption barely helps here because both sides still do
the ECDHE exchange; its benefit on a real network (round trips) is not
measured.

FACT, cleartext regression check: the pre-TLS release binary against the TLS
build without a TLS listener, four alternating rounds: no throughput, latency
or CPU regression detected at about 10% resolution; new connections at C8 were
0-5% lower in all four rounds, inside the noise but on the same side each
time, so a small accept-path cost is not excluded. Idle cleartext memory per
connection: 25.3 KiB before, 25.5 KiB after (29.4 KiB before the TLS branch
was boxed).

## Known limitations

- **Deliberately not in v1:** ACME, certificate reload, OCSP stapling, mTLS,
  upstream TLS, HTTP/2 and HTTP/3, Windows certificate store, HTTP to HTTPS
  redirect, HSTS.
- **Every host is on both listeners.** There is no per-host listener binding,
  so a host meant only for HTTPS is also served on the cleartext listener when
  one is configured. Run HTTPS-only (omit `server.listen`) if that matters.
- **No SNI/Host authorization relation.** A client can complete the handshake
  for one configured name and request another host; it is served. For
  HTTP/1.1 a well-behaved client never does this, and a hostile client can
  simply send the other SNI, so the risk is confusion rather than bypass; but
  it is a policy decision, not a proof. No `421` is produced.
- **Certificate time validity and chain completeness are not checked** at
  startup. An expired certificate starts and every client rejects it.
- **One private key per certificate entry, first key in the file.** Additional
  keys in the same PEM file are ignored.
- **Handshake counters are not exported**; they are visible only through the
  rate-limited stderr line and the API used by tests.
- **Graceful drain is still bounded by `header_read_timeout`**, the existing
  debt shared with cleartext. `close_notify` is sent when Hyper ends a
  connection normally (tested); when Hyper ends it with an error, such as the
  header timeout on an idle keep-alive connection, its 1.11 dispatcher returns
  before calling shutdown, so the socket closes without `close_notify` (read
  from source, not tested).
- **Resumption state** is rustls's default 256-entry in-memory cache per
  process, lost on restart; no stateless ticket key exists.
- **Dependencies:** rustls, tokio-rustls and the aws-lc-rs provider
  (`aws-lc-sys` builds native code with `cmake`/`cc`). The release binary grew
  from 2.95 MB to 5.32 MB. A `ring` provider was not evaluated.
- **Native Linux: NOT VERIFIED.** Only `x86_64-pc-windows-msvc` is installed;
  the Linux cross-target check could not run and no target was installed.
- Performance was measured on Windows loopback only; the Reverse Proxy v1
  loopback send-stall findings apply to the transport underneath and were not
  re-investigated for TLS.

## Owner review points

1. SNI/Host relation: keep independent routing (current), require the
   effective authority to be covered by the handshake's certificate, or
   require it to equal the SNI; and whether a mismatch is `421` or `400`.
2. Per-host listener binding (a host served only on HTTPS, or only on HTTP).
3. Certificate validity dates at startup: ignore (current), warn, or refuse.
4. Provider: aws-lc-rs (current, native build step, +2.37 MB) or ring.
5. Whether the handshake counters should become part of a metrics surface.
