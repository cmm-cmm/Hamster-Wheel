# Application Request Metadata

Status: implementation working tree, 2026-09-18. Owner decides closure and
repository actions.

## Contract

FACT: the accepted TCP connection owns the immediate peer and local endpoints.
`server::serve_with_router` captures both in `TransportInfo`; neither value is
read from HTTP headers. The request itself owns its HTTP version. The HTTP
boundary owns effective authority: origin-form uses the validated Host field,
absolute-form uses the validated request-target authority, and Host is rewritten
before routing and application dispatch.

The persistent PHP path now preserves those values in a typed protocol-v2
request. Laravel/Symfony receives:

| application field | server-owned source |
|---|---|
| `REMOTE_ADDR`, `REMOTE_PORT` | accepted socket peer endpoint |
| `SERVER_ADDR`, `SERVER_PORT` | accepted socket local endpoint |
| `SERVER_PROTOCOL` | actual request version (`HTTP/1.0` or `HTTP/1.1`) |
| `HTTP_HOST` | effective authority already rewritten at the HTTP boundary |
| `SERVER_NAME` | hostname component of the same effective authority |
| `REQUEST_SCHEME` | `http` for the current cleartext transport |
| `HTTPS` | `off` for the current cleartext transport |

`SERVER_NAME` deliberately does not use the configured virtual-host spelling:
that would create a second application-visible host identity. An explicit port
remains in `HTTP_HOST` but is removed from `SERVER_NAME`; IPv6 brackets are also
removed there.

Forwarded, `X-Forwarded-*`, `X-Real-IP` and similar fields remain ordinary
client headers. Hamster does not interpret or promote them. Application trusted
proxy policy can now evaluate them against the real immediate peer.

The adapter populates the Symfony `Request` server bag. It does not hydrate the
process-global PHP `$_SERVER`; SAPI emulation remains a separate compatibility
concern.

## Protocol v2

Framing and response messages are unchanged. A v2 `REQUEST` payload is:

```text
kind u8 = 2
request_id u64
http_version u8                 1 = HTTP/1.0, 2 = HTTP/1.1
peer endpoint
local endpoint
method u16 + bytes
uri u32 + bytes
path u32 + bytes
query u32 + bytes
header_count u16
  repeated: name u16 + bytes, value u32 + bytes
body u32 + bytes
```

An endpoint is `family u8` (`4` or `6`), exactly 4 or 16 network-order address
bytes, then port `u16`. The format adds 15 bytes to an IPv4/IPv4 request and 39
bytes to an IPv6/IPv6 request compared with v1.

Startup compatibility is strict. `READY` must advertise version 2; version 1,
unknown versions, malformed frames and PID mismatch fail startup. Since all
persistent pools reach READY before `server::run` binds the listener, a stale
worker cannot defer incompatibility until the first request and cannot accept
traffic. There is no v1 fallback or optional tail.

The PHP decoder rejects unknown HTTP versions, unknown address families,
truncation and trailing bytes. Such corruption throws out of the worker loop;
Rust treats the possibly-started request as failed, does not replay it, reaps
the generation and follows the existing recovery policy.

## Data Flow and Ownership

```text
TcpListener::accept / TcpStream::local_addr
  -> immutable per-connection TransportInfo
  -> HostRouter establishes and rewrites effective authority per request
  -> Handler::PhpWorker passes Request + TransportInfo
  -> body admission reconstructs one owned WorkerRequest
  -> protocol-v2 frame
  -> PHP typed decoder
  -> Laravel/Symfony Request server bag
```

No metadata lives in pool-global or worker-global mutable state. Keep-alive
requests copy the connection endpoints into each independent request frame;
different connections, workers and application pools cannot reuse a prior
request's values through this path.

Transaction ownership did not move. Metadata is constructed before queueing,
then travels inside the same `PhpJob` whose tracked REQUEST write distinguishes
NotStarted from possibly-started failure. Protocol v2 does not add a retry path,
cancellation frame or new execution timeout.

## Verification

FACT, Windows native:

- Rust protocol tests decode the exact emitted IPv4/IPv6 layout and reject all
  READY versions except 2.
- The standalone PHP decoder passes 14 binary/adversarial checks, including
  both IP families, both HTTP versions, binary fields, bad enums, truncation and
  trailing data.
- A protocol-v1 fixture is rejected during `server::run` startup; the listener
  remains unbound and the child is reaped.
- Real sockets bound to `127.0.0.2` and `127.0.0.3` reach the same W1 worker with
  their actual source addresses and ephemeral ports. The accepted local address
  and port match the listener. HTTP/1.0 remains HTTP/1.0.
- An absolute-form request whose target authority conflicts with Host is routed
  by the target authority and the rewritten Host reaches PHP. Forged forwarding
  fields remain visible only as ordinary headers.
- Two requests on one keep-alive connection retain the connection endpoints but
  do not retain the previous request's forwarding-header value.
- Two independent application pools concurrently receive their own request
  metadata and have distinct PHP PIDs.
- A prepared scratch PMS received all server-bag fields through the real
  Laravel/Octane adapter. With Symfony configured to trust only `127.0.0.1`, a
  request from `127.0.0.2` carrying forged client/host/scheme/port forwarding
  headers still reported client IP `127.0.0.2`, host `localhost`, scheme `http`
  and the effective authority port. A following `.3` request on the same W1
  generation replaced the metadata.
- The existing 100,000-request protocol soak, all nine multi-pool fault tests,
  W1/W4 PMS lifecycle tests, cancellation stress, crash recovery, execution
  timeout exact-once test and shutdown drain tests pass with v2.

The PMS proof used only the disposable scratch copy under the user's temporary
directory. The original PMS checkout was not modified.

## Performance Evidence

FACT: a Windows MSVC release C1 trivial-worker comparison used the same
`php_worker.ps1` driver, 1,000 warm-ups and 30,000 measured requests per sample.
Four pairs alternated run order between a protocol-v1 snapshot (current
effective-authority router overlaid) and v2.

| arm | RPS samples | combined CPU us/request samples | p50 ms samples |
|---|---|---|---|
| v1 | 5609, 6288, 6332, 6369 | 216.146, 195.833, 198.438, 188.542 | 0.168, 0.149, 0.146, 0.145 |
| v2 | 5715, 5751, 6408, 6285 | 209.375, 209.375, 184.896, 195.312 | 0.167, 0.165, 0.144, 0.145 |

Paired RPS and CPU signs split two/two. The middle observed RPS values are about
6,310 for v1 and 6,018 for v2 if averaged, while the repository script's
upper-middle convention reports 6,332 and 6,285. The difference is below the
within-arm spread and p50 overlaps.

INFERENCE: no material hot-path regression is demonstrated. The test is not
sensitive enough to exclude a low-single-digit cost, and therefore does not
support a zero-cost claim. The added work is two endpoint encodes/decodes and
15 bytes on the currently tested IPv4 path.

## Remaining Limits

FACT:

- Only cleartext HTTP exists, so scheme mapping is intentionally `http`/`off`.
- PHP globals are not populated.
- The Laravel adapter's duplicate-header collapse remains adapter-local and was
  not changed by the typed metadata format.
- IPv6 binary decoder behavior is checked, but no native IPv6 end-to-end socket
  run was performed. IPv6 flow information and interface scope IDs are not
  carried; application fields receive the IP address and port only.
- Native Linux compilation/execution was not verified in this pass.

HYPOTHESIS: link-local IPv6 deployments may eventually need an explicit scope
contract. No current configuration or workload demonstrates that requirement,
so v2 does not add it speculatively.
