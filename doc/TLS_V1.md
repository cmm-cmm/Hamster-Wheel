# TLS v1

Transactional certificate reload was added after the TLS v1 review. Its
current behavior and verification are in `doc/TLS_CERTIFICATE_RELOAD.md`;
statements below about reload being outside v1 describe the historical v1
boundary.

Status: implemented on 2026-09-25; the current baseline, including the
authority policy and certificate checks, is `ed5845b`. The final EKU fix and
readiness review below are uncommitted. Not tagged or released. The investigation it follows is
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
names = ["example.com", "*.example.net"]  # also the hosts this certificate serves
default = true                # at most one: used for no SNI or unknown SNI
```

In v1: HTTP/1.1 over TLS 1.2 and 1.3; rustls 0.23 with the aws-lc-rs provider
and its default cipher suites and key exchange groups; ALPN advertising only
`http/1.1`; PEM certificate chains and keys loaded once at startup; SNI
selection over exact names and one-label wildcards with an optional default;
a connection serves only the hosts its certificate's names cover (`421`
otherwise); startup refusal of certificates outside their validity period and
of chains that do not link; a total handshake deadline; graceful shutdown and cancellation; a trusted
connection scheme for the reverse proxy and the PHP worker protocol.

Not in v1, deliberately: ACME, certificate reload, OCSP stapling, client
certificates (mTLS), upstream TLS, HTTP/2, HTTP/3, a Windows certificate store,
automatic HTTP to HTTPS redirect, HSTS, and binding a host to the cleartext
listener only.

## Architecture

TLS is a capability of the listener. The cleartext and TLS paths converge on
one HTTP serving function; below it nothing knows which listener accepted the
connection except through the trusted scheme.

```text
cleartext: TcpStream -> WriteProgressTimeout -------------------------> serve_http
TLS:       TcpStream -> WriteProgressTimeout -> rustls handshake -> TlsStream -> serve_http
serve_http<I: AsyncRead + AsyncWrite>: TokioIo -> Hyper http1
    -> HostRouter(ConnectionInfo, Option<&CertificateScope>) -> Handler(ConnectionInfo)
```

- `src/tls.rs` owns everything TLS: validated names (`TlsName`), certificate
  loading and checks (`src/tls/x509.rs` reads validity dates), the SNI
  resolver, each certificate's `CertificateScope`, the `TlsListener` (address,
  one immutable `TlsAcceptor`, handshake deadline) and handshake failure
  counters. No other module imports rustls.
- `src/server.rs` binds every configured listener before serving any, then runs
  one accept loop over them (`serve_listeners`). A listener is a bound socket
  plus `Option<Arc<TlsListener>>`. Accept rotates which listener is polled
  first so a flood on one cannot starve the other. `serve_with_router` remains
  the cleartext entry point every existing test and probe uses.
- `src/transport.rs` keeps `TransportInfo` as socket identity only and adds
  `Scheme::{Http, Https}` and `ConnectionInfo { transport, scheme }`, a `Copy`
  value passed once from the listener to the router and every handler. It holds
  no rustls type and nothing read from a request.
- The router alone also receives the presented certificate's
  `CertificateScope` (`None` on cleartext) and answers an authority it does
  not cover with `421`. Handlers never see it.

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

`names` are exact DNS names, `*.` plus a parent domain of at least two labels
matching exactly one extra label (`*.example.net` covers `a.example.net`, not
`example.net` or `a.b.example.net`), or IP addresses. Names are compared
ASCII-case-insensitively and a trailing dot in the client's SNI is ignored. A
name may appear once across all certificates. DNS names do two things: they
select the certificate by SNI, and they are the hosts a connection that
presented that certificate may be served for. An IP address only does the
second: clients never send one as SNI, so an IP name belongs on the
certificate that serves no-SNI clients, normally the default.

A handshake with no SNI, or with an SNI no entry matches, uses the certificate
marked `default = true`; without one it fails. Resolution is a static map built
at startup: at most two hash probes, no I/O, as required by rustls calling the
resolver synchronously inside the handshake.

### Which hosts a TLS connection may be served for

A TLS connection is authoritative only for the hosts its certificate's
configured `names` cover. The router checks the effective authority (Host, or
the absolute-form target) against the scope of the certificate the handshake
presented, after validating it (a malformed authority is still `400`) and
before virtual-host lookup (so the answer reveals nothing about which hosts
exist). A request outside the scope is answered `421 Misdirected Request` with
body `Misdirected Request`; the connection stays open for requests it does
cover. Inside the scope, routing is unchanged.

Why this rule: RFC 9110 section 4.3.3 makes an `https` origin authoritative
for a host only when the certificate presented on the connection is valid for
that host, and section 15.5.20 defines `421` for a request "directed at a
server that is unable or unwilling to produce an authoritative response" on
that connection, which a client may retry on a new connection. Hamster is the
origin (or the gateway acting as one) for every configured host, never a
forward proxy, so `421` is the status RFC 9110 prescribes. Before this change
a handshake completed for `a.test` served `Host: x.b.test`, and a client with
no SNI got the default certificate and any configured host, including hosts
no certificate names at all. A browser never sends such a request because it
checks the certificate against the host first; the rule closes the gap for
every other client and makes the configured `names` the explicit list of hosts
the TLS listener serves.

Consequences, each covered by a test:

| Connection presented | Request authority | Result |
|---|---|---|
| certificate naming `a.test` and `b.test` | `b.test` | served (one certificate for several hosts) |
| certificate for `a.test` | `x.b.test`, which another certificate names | `421` |
| any certificate | a configured host that no certificate names | `421` over TLS; still served over cleartext |
| default certificate (no or unknown SNI) | a host the default names | served |
| default certificate | any other host | `421` |
| default certificate naming `127.0.0.1` | `Host: 127.0.0.1` | served |
| any | absolute-form `https://other/` with a covered Host | `421`: the target authority decides |

The check compares against the configured `names`, not the certificate's full
SAN list: `names` are verified to be a subset of the SANs at startup, and a SAN
the operator did not list is not a host this listener was asked to serve. The
scope is found with the same selection function rustls used to choose the
certificate, so the two cannot disagree.

Alternatives considered: requiring the authority to equal the SNI is stricter
than RFC 9110 and refuses legitimate reuse of one multi-name certificate;
keeping independent routing leaves the gap above. A cleartext listener has no
certificate and is unchanged.

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
  explicit subdomains does not pass as a wildcard; an IP name is checked
  against the IP SANs;
- a leaf whose extended key usage (EKU), when present, does not explicitly
  include `serverAuth`, including when the file contains only that leaf;
- the leaf, or any intermediate in the file, outside its validity period at
  startup (`... expired on <date>` or `... is not valid until <date>`). A
  trailing self-signed root is exempt: clients use their own copy of a root,
  so a bundled one decides nothing;
- a chain that does not link: with two or more certificates in the file, the
  last one is used as the anchor and the path from the leaf to it is verified
  with webpki at the current time (signatures, CA flags and path length for
  intermediates below the anchor, and server-auth usage when an EKU is
  present). The last certificate's CA bit is checked separately. A wrong
  issuer, a missing intermediate between the leaf and a bundled root, or an
  issuer that is not a CA fails startup.

Leaf EKU is readable without an issuer certificate: it is an extension of the
leaf itself. The startup check reads it for every leaf. This does not verify
the leaf's signature or its path to an issuer. Those path checks need at least
two certificates in the configured file; with only a leaf, the server cannot
tell whether its issuer is trusted or whether an omitted intermediate is needed.

What the server can decide and what it cannot: it knows the current time and
the certificates it will send, so it can refuse to serve with a certificate
every client would reject for its dates, and it can prove the certificates it
sends form a path. It cannot know which roots a client trusts. So a file that
holds only a leaf whose issuer is not in the file is accepted, because that
issuer may be a root every intended client holds (a private CA) or an
intermediate the operator forgot; startup prints a note saying the issuer is
not included. Revocation, certificate transparency and trust by real clients
remain the client's decisions. The x509 reader walks the fixed prefix of
`TBSCertificate` for dates and names and reads the leaf's EKU extension.

Startup prints each certificate's field, names and expiry, for example
`TLS certificate server.tls.certificates[0] for localhost: valid until
2027-01-01 00:00:00 UTC (463 days)`, because Hamster does not reload
certificates: a certificate that expires while the server runs keeps being
served, and clients reject it, until a restart with a renewed file.

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
| request for an authority the presented certificate does not cover | `421 Misdirected Request`, connection kept | not counted |
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
| SNI: exact, wildcard, one label only, unknown and absent SNI use the default; a verifying client accepts the wildcard chain; a host another certificate names is `421` and the connection then serves its own host | `sni_selects_the_certificate_and_a_host_it_does_not_cover_is_misdirected` |
| authority scope: multi-name certificate, default certificate, IP name, configured host no certificate names (421 over TLS, served over cleartext), unknown host, absolute-form target | `a_tls_connection_serves_only_the_hosts_its_certificate_names` |
| router: 421 after authority validation and before lookup; absolute-form judged by target; cleartext unaffected | `router::tests::a_tls_connection_serves_only_hosts_its_certificate_covers` |
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
| name parsing, one-label wildcard lookup, shared selection rule, scope coverage, rate-limited reporting | `tls::tests::*` (4) |
| validity dates from UTCTime and GeneralizedTime, self-issued detection, malformed DER | `tls::x509::tests::*` (3) |
| config: TLS with and without cleartext; 22 rejected configurations (listeners, timeouts, names, duplicates, defaults, unknown fields, unreadable/malformed/empty PEM, missing/encrypted/mismatched key, uncovered exact, wildcard and IP names, IP wildcard), none leaking key material | `tests/configuration.rs` `compiles_a_tls_listener_with_or_without_a_cleartext_listener`, `rejects_tls_configuration_that_cannot_be_served` |
| expired and not-yet-valid leaf (with CA, and self-signed alone), expired intermediate; an expired trailing root is accepted | `a_certificate_outside_its_validity_period_fails_startup` |
| linked chains (with and without root) compile and report expiry; wrong issuer and missing intermediate fail; a bare leaf compiles with a note; a self-signed leaf has none | `a_chain_that_does_not_link_fails_startup_and_a_bare_leaf_is_reported` |
| bare leaf with client-auth-only EKU fails startup; server-auth EKU and absent EKU compile with the missing-issuer note | `a_bare_leaf_with_client_auth_only_eku_fails_startup` |
| a non-CA certificate at the end of a two-certificate file cannot serve as an issuer anchor | `a_non_ca_issuer_at_the_end_of_a_chain_fails_startup` |
| protocol v3 byte layout; READY v0/1/2/4/max rejected | `php_protocol::tests` |
| PHP worker receives `https` from the TLS listener and `http` from the cleartext one in the same W1 process; forged headers ignored; local port is the TLS port | `tests/php_worker.rs::protocol_v3_carries_the_listener_scheme_not_a_forwarding_header` (ignored: needs `php/8.5/php.exe`) |
| real Laravel/Symfony over HTTPS only (no cleartext listener): `REQUEST_SCHEME=https`, `HTTPS=on`, `getScheme()=https`, default port 443, a forged `X-Forwarded-Proto: http` from an untrusted peer ignored | `laravel_sees_https_from_the_tls_listener_and_not_from_a_forged_header` (ignored: needs a prepared scratch PMS) |
| PHP decoder: http/https values, 0 and 3 rejected; Laravel server-bag mapping | `fixtures/php-worker/protocol_test.php`, `laravel_request_test.php` |

Earlier gates, Windows native, after the authority and certificate changes:
`cargo fmt --check`, strict `clippy --all-targets --all-features -D warnings`,
`cargo test` (141 lib, 28 configuration, 7 routing, 5 static), MSVC release
build and `cargo bench --no-run --features profiling` passed. `cargo test
-- --ignored` with `HAMSTER_LARAVEL_TEST_PATH` set to a scratch copy of the
prepared PMS: lib 9, multipool 9, visibility 1, timeout 10, containment 4 and
61 of 64 `php_worker` tests pass, including both TLS PHP tests and the
Laravel metadata probe. The 3 failures are environmental and unchanged: the
ConPTY `ctrl_c` harness gap already documented, and two Laravel tests whose
scratch vendor tree lacks `predis/predis/src/Client.php` (also missing in the
copy it came from). `cargo check --target x86_64-unknown-linux-gnu` cannot run:
the target is not installed. Native Linux: NOT VERIFIED.

Corrected on 2026-09-27. Two environment claims above were wrong. The `ctrl_c`
failure is not ConPTY: the server inherited an "ignore Ctrl+C" attribute from
the launching shell, the test now clears it, and it passes. The Linux target is
installed; the cross-target check is blocked by `aws-lc-sys` needing
`x86_64-linux-gnu-gcc`. Native Linux is still NOT VERIFIED. See
`doc/AI_HANDOFF.md` and `doc/LOCAL_TEST_APPLICATIONS.md`.

End to end with the release binary, a TLS-only config and `curl`: `localhost`
200, `https://127.0.0.1` with no SNI 200 through an IP name on the default
certificate, `Host: other.test` (a configured alias the certificate does not
name) 421 `Misdirected Request`; startup printed the certificate's expiry; an
expired certificate made the binary exit with `the leaf certificate expired on
2021-01-01 00:00:00 UTC` before binding.

### Intermittent test failures

The earlier report recorded one unidentified failure in ten runs of the
TLS/proxy/server tests. It was reproduced and identified; neither cause is a
TLS regression.

1. `server::tests::the_body_budget_caps_concurrent_bodies_and_releases_every_one`
   (and once `an_undeclared_body_is_charged_as_it_grows_and_refused_when_it_cannot_grow`):
   10 of 100 full `--lib` runs on `868eaff` while the machine was also
   building; the client's read of the 503 failed with `WSAECONNABORTED`
   (10053). Cause: the test pushed a 64 KiB body that the budget refuses
   before reading. Hyper 1.11 then reads at most one buffer of the unread
   body (`poll_drain_or_close_read`) and closes; closing a socket with unread
   bytes makes Windows send a reset, which can discard the 503 before the
   client reads it. The same test binary built from `a7a31b4` (before Reverse
   Proxy and TLS) failed the same way: over two alternating A/B series, 1 of
   160 runs before, 3 of 160 after, inside the noise. Fix, in the tests: the
   refused client sends `Expect: 100-continue` and waits, as curl does for large
   bodies, and the chunked client stops one byte past the budget, so nothing is
   left unread when Hamster closes. Both still assert the refusal, its body
   text, the budget accounting and recovery. After the fix: 0 failures in 100
   full runs.
2. `server::tests::midstream_response_error_closes_connection_and_server_recovers`:
   1 in 100 full runs, and 2 in 600 runs of the test with four copies in
   parallel; the head arrived without the five body bytes that precede the
   injected body error (and, under the same stress, sometimes nothing arrived). The `a7a31b4` build failed the same assertion 1 in 600
   under the same stress. Cause: Hyper 1.11 buffers body chunks in a loop and,
   when the body fails inside it, returns the error and drops the connection
   without flushing what it buffered (`proto/h1/dispatch.rs`, `poll_write`); a
   pipelined request left unread can also turn the close into a reset. The
   delivered prefix is therefore not guaranteed. The test now asserts what is:
   if anything arrives it is the 200 head with `content-length: 10` and at most
   a prefix of the five bytes, so the response never looks complete; the
   pipelined request never runs; the server recovers.

Confirmation on the final tree: 100 full `--lib` runs with no failure, and
600 runs of the three affected tests with four copies in parallel with no
failure. A first version of the relaxed midstream assertion still required
exactly one response head and failed 5 of 600 under that stress with an empty
response: under load Hyper may flush nothing at all before the body error,
which is the same cause.

Mutation check (each applied alone, the named tests run, the file restored
and its hash compared): 10 of 10 caught for the first slice, and 9 of 9 for the
authority and certificate changes (below the first table).

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

| Mutation | Caught by |
|---|---|
| S1 router never refuses an uncovered authority | router scope test |
| S2 scope taken from the default certificate, not the presented one | nine TLS server tests |
| S3 scope wildcard matches any depth | scope unit test |
| S4 absolute-form judged by Host instead of its target | router scope test |
| D1 expiry not checked | validity test |
| D2 not-yet-valid not checked | validity test |
| D3 chain linkage not checked | chain test |
| D4 trailing self-signed root date-checked | validity test (expired bundled root) |
| D5 bare leaf not reported | chain test |

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

Not re-measured after the authority and certificate changes: they add one
resolver lookup per TLS handshake, one `Arc` clone and a few string
comparisons per TLS request, nothing on cleartext requests, and a startup-only
certificate walk, below the resolution of the harness below.

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
- **The cleartext listener still serves every host.** Over TLS a host is
  served only if a certificate names it, but when `server.listen` is configured
  a host meant only for HTTPS is also reachable over cleartext, and there is no
  redirect or HSTS. Run HTTPS-only (omit `server.listen`) if that matters.
- **Certificates are checked once, at startup.** A certificate that expires
  while the server runs keeps being served until a restart with a renewed
  file; startup prints each expiry date. Trust by real clients, revocation and
  certificate transparency are not checked and are the client's decisions. A
  bare leaf whose issuer is not in the file starts with a note, because only
  clients know whether they hold that issuer. Its EKU is checked at startup;
  the missing issuer still prevents signature and path verification.
- **The final bundled issuer is a verification anchor.** Its CA bit is checked,
  but webpki does not apply that anchor's own path-length or key-usage
  restrictions at startup. A client building a path through a higher root may
  reject a chain Hamster accepts; actual client trust remains authoritative.
- **Hosts added by `names` beyond the configured virtual hosts** are harmless
  (lookup returns `404` as before), and a configured SAN that `names` omits is
  not served over TLS: `names` is the authority list.
- **Early refusals and mid-body failures are not guaranteed to reach a client
  that keeps sending (existing, not TLS-specific).** When Hamster answers before
  reading a body (body budget `503`, `413`, `408`), Hyper drains at most one
  buffer and closes; on Windows the remaining unread bytes turn the close into
  a reset that can destroy the response. A client that uses
  `Expect: 100-continue` receives it reliably. Bytes buffered before a
  response body fails mid-stream may not be flushed. Reproduced with the
  pre-TLS build; see Verification.
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

## Owner decisions and final readiness

The Owner keeps the HTTP listener serving every configured host at this
milestone. No per-host HTTPS-only policy or redirect is added. Early refusals
continue to close the connection after the response; a client that keeps
sending can observe a reset instead of the status. Lingering close is deferred
until evidence from a real client warrants it.

The final source review found two TLS startup defects: a bare leaf with
client-auth-only EKU previously passed because EKU was checked only during
chain verification, and a non-CA certificate used as the final bundled issuer
passed because webpki treats that final certificate as an anchor. The leaf EKU
check and final-issuer CA check, with regressions above, close both gaps.
These checks do not establish client trust. Windows final gates passed:
`cargo fmt --all -- --check`, `cargo test`
(141 library passed, 9 ignored; 30 configuration passed, including both new
regressions),
strict Clippy, MSVC release build, and bench link with profiling. The ignored
PHP regression `protocol_v3_carries_the_listener_scheme_not_a_forwarding_header`
passed with the local PHP distribution. The scratch PMS environment variable
was unset, so the ignored real Laravel TLS test was not rerun; its earlier
Windows result is recorded in Verification. No benchmark was rerun because
the fix runs only at startup. Native Linux remains unverified: only the MSVC
target is installed here. No TLS v1 blocker remains from this review.
