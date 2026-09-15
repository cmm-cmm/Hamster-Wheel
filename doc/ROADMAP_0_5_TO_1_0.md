# Hamster Wheel 0.5 to 1.0 Roadmap

Status: the Production Spine direction and a 0.5.x delivery sequence are owner
approved. Milestone details remain evidence-driven rather than an implementation
specification. The first 0.5.x transport-identity slice is described in
`HTTP_TRANSPORT_0_5.md` and awaits owner review.

## Product boundary

Hamster Wheel is a general-purpose Web/Application Server. Its core owns HTTP,
routing, transport metadata, resource admission, backend dispatch, process and
connection lifecycle, configuration, observability, and platform operation.

PHP is a first-class workload with two distinct execution models:

- FastCGI is the compatibility path for ordinary PHP applications and runtimes
  that require an SAPI request boundary.
- Persistent workers are an explicit application-server contract. The core
  owns process/protocol/pool lifecycle and at-most-once dispatch; each adapter
  owns application bootstrap, request reconstruction, framework lifecycle and
  request isolation.

Laravel Octane belongs only to the Laravel persistent-worker adapter. It is the
first real-world validation of the adapter boundary, not the architecture of
Hamster Wheel and not a lifecycle implementation for other frameworks.

Static serving and reverse proxying are peer handler capabilities. Non-PHP
applications normally integrate through the reverse proxy rather than by
acquiring a Hamster-specific worker protocol before evidence justifies one.

## Evidence from 0.0 through 0.4

FACT: the server currently accepts HTTP/1.1 only and discards the socket peer
address before routing. Handler responses use one fully buffered body type.
Static GET canonicalizes a path and reads the complete file for every request.
The compiled configuration allows one PHP-worker location and has no runtime
reload. Protocol v1 has a 1 MiB request-body limit and no typed transport
metadata.

FACT: the Windows static investigations attributed the main cost to filesystem
operations and contention, not URI parsing or the Tokio blocking-thread limit.
They also retained a canonicalize-to-open TOCTOU window. No secure,
handle-relative/open-once design has yet been proven on both target platforms.

FACT: the Windows PHP investigation found a PHP CLI pipe mode responsible for
the 15.625 ms latency plateau. The correct fix was platform-specific stream
configuration, not a new protocol, more workers, or an IPC rewrite.

FACT: 0.2 established that HTTP cancellation does not cancel a worker
transaction. 0.3 established stable slot/generation ownership, bounded
recovery and coordinated shutdown. Permanent retirement was removed after it
turned a transient failure into a permanent outage.

FACT: 0.4 falsified heuristic Laravel resets. Its replacement uses Laravel's
own Octane lifecycle inside the adapter and commits no response until cleanup
succeeds. The Rust worker lifecycle and protocol remain framework-agnostic.
Real-application measurements showed that persistent bootstrap reuse ranges
from material on light routes to modest on database-heavy work.

INFERENCE: the recurring project risk is an undefined ownership or failure
boundary, not lack of an optimization mechanism. The shortest path to 1.0 is a
production spine that makes those boundaries explicit across every handler.

## Changes from the old roadmap

| Old assumption | Revised decision | Reason |
|---|---|---|
| Adaptive worker manager in 0.5 | Move adaptive sizing and Morph after 1.0 evidence | Fixed pools are qualified; safe adaptation still lacks workload telemetry, resource policy and deoptimization foundations. |
| FastCGI after adaptive work | Put FastCGI in 0.6, immediately after the shared request contract | Compatibility is useful by itself and supplies the second real execution model. |
| TLS, proxy, HTTP/2, service operation and static optimization grouped into `0.7+` | Give proxy, operation/reload and secure-edge work separate milestones | They have different ownership, failure models and dependency order. |
| Linux service support near 1.0 | Begin native Linux qualification in 0.5 | Platform behavior already changed the correct Windows PHP design; late Linux discovery would invalidate FastCGI/static/service work. |
| Reload implied by 0.0.4 | Implement reload after the heterogeneous backend graph exists | The current immutable config is a startup boundary, not a proven resource-generation swap. |
| 1.0 as a production PHP application server | Define 1.0 as a Web/Application Server | Static, proxy and application backends are peer product capabilities. |
| Laravel lifecycle as application reset | Keep Laravel Octane wholly inside one adapter | 0.4 proved framework lifecycle cannot be inferred safely by the core. |

FACT: this revision preserves the accepted fixed pool, lifecycle supervisor,
protocol-v1 compatibility and Laravel adapter. It does not use roadmap cleanup
as permission to redesign working 0.2-0.4 foundations.

## Abstraction decisions

There is enough evidence for:

- one transport/request context, because peer identity is already lost and the
  same data is required by PHP mapping, proxy forwarding, logging and TLS;
- explicit identity for long-lived resource generations, because worker
  slot/generation ownership demonstrated the value of that model. This is
  design evidence for config generations, not proof that worker supervisor code
  can be reused unchanged;
- a documented persistent-worker adapter boundary, because a generic fixture
  and the Laravel implementation now provide two concrete applications of the
  same core process/protocol contract;
- a streaming-capable response representation when static serving becomes its
  first production consumer.

There is not yet enough evidence for:

- a generic async backend trait, dynamic plugin ABI or multi-crate backend SDK;
- one process-supervisor abstraction shared by persistent workers, Windows
  `php-cgi`, external PHP-FPM and proxy connections;
- a framework-neutral state reset or StateFence implementation;
- route capsules, specialized queues, shared-memory IPC or adaptive pools;
- automatic proxy retry policy beyond a proved zero-delivery boundary.

INFERENCE: concrete enum variants and explicit platform implementations remain
the safer default. Extract common machinery only after FastCGI and proxy code
show repeated semantics rather than merely similar names.

## Dependency graph

```text
transport identity + HTTP policy + bounded body model
                         |
             +-----------+-----------+
             |                       |
       static data path       backend request mapping
                                     |
                         +-----------+-----------+
                         |                       |
                      FastCGI              reverse proxy
                         +-----------+-----------+
                                     |
                     stable multi-resource config graph
                                     |
                     transactional reload and drain
                                     |
                 service operation + observable control plane
                                     |
                      TLS / HTTP2 / release qualification
                                     |
                                    1.0
                                     |
                    adaptive management / Morph research
```

Cross-cutting rules apply at every milestone:

- a possibly-started operation is never automatically replayed;
- cancellation never silently abandons owned backend work;
- queues, reads, writes, retries, shutdown and failure churn are bounded;
- new metrics are added with the capability they measure;
- Windows and native Linux results are reported separately;
- performance changes require comparable release measurements;
- no framework-specific state policy enters the core.

## 0.5 — HTTP and Transport Foundation

Goal: define a correct, bounded request/response contract that every later
handler can consume.

The milestone is delivered as separately reviewable 0.5.x slices. The first
slice captures immediate TCP peer/local identity and carries it to handlers
without changing protocol v1. Source review falsified the need to copy HTTP
version and authority into a parallel context: Hyper's request already owns
them. Scheme and trusted-proxy-derived client identity wait for authoritative
listener/policy inputs. Later 0.5.x boundaries remain owner decisions.

Scope:

- Use one internal dispatch contract consisting of Hyper's request plus
  server-established transport facts. Keep HTTP version and authority in their
  existing request representation; add scheme only when a listener can
  establish it. Keep immediate peer identity distinct from a client address
  derived through an explicit trusted-proxy policy.
- Decide the persistent protocol compatibility mechanism for typed metadata.
  Protocol v1 must remain deterministic; a versioned v2 or a rigorously
  reserved server-owned extension requires its own compatibility tests.
- Define per-listener and per-handler limits for header/read/body/idle time,
  concurrent connections and buffered bytes. Count-bounded queues alone are
  not a total memory bound.
- Replace the assumption that every response is `Full<Bytes>` using a concrete
  streaming static-file use case. Do not create a generic plugin system.
- Establish correct static validators, conditional/HEAD behavior and a
  bounded-memory large-file path. Range support enters 0.5 only if a concrete
  pre-1.0 workload requires it. Investigate an opened-handle containment model
  without weakening Windows symlink/junction protection.
- Add request correlation and the minimum stage counters/timers needed to
  qualify later backends. This is production instrumentation, not Morph.
- Bring up native Linux CI/test execution for the existing core and worker
  fixture. Cross-compilation from Windows is not native verification.

Exit evidence:

- adversarial HTTP/1.1 tests cover timeout, limit, malformed framing,
  disconnect and graceful-drain behavior;
- peer metadata reaches a test handler without trusting client-supplied proxy
  headers;
- large static responses have bounded application memory and preserve path
  containment on Windows and Linux;
- the 0.2/0.3/0.4 at-most-once and lifecycle matrices remain unchanged;
- native Windows and Linux correctness gates pass, with separate release
  static measurements.

Non-goals: FastCGI, proxying, TLS, HTTP/2, automatic certificate management,
autoscaling, Morph, or framework adapters.

## 0.6 — PHP Compatibility and Persistent Adapter Contract

Goal: support general PHP safely and turn the existing persistent worker into
an explicit adapter product surface rather than a Laravel-shaped feature.

Scope:

- Implement FastCGI framing and request mapping as a separate backend.
- Qualify managed `php-cgi` over loopback TCP on Windows and PHP-FPM over Unix
  socket/TCP on Linux. Process ownership may differ by deployment and must be
  explicit.
- Carry the 0.5 request context into correct CGI/FastCGI parameters.
- Define cancellation, timeout, abort and retry semantics from bytes actually
  delivered to the FastCGI peer. A missing response is not proof that an
  application did not execute.
- Permit multiple independently configured PHP backends/locations. Preserve
  per-backend resource bounds and shutdown ownership.
- Publish the generic persistent-worker adapter contract: READY/REQUEST/
  RESPONSE/SHUTDOWN behavior, response commit point, application-owned cleanup,
  compatibility declaration and failure semantics.
- Keep the Laravel Octane adapter as one implementation of that contract. Do
  not provide a fictional generic reset for arbitrary persistent PHP.

Exit evidence:

- the same ordinary PHP fixture passes through Windows `php-cgi` and Linux
  PHP-FPM with equivalent method, URI, metadata, headers, body, upload and
  response semantics;
- crash, cancellation, saturation, shutdown and possibly-started fault
  injection preserve at-most-once behavior;
- multiple FastCGI and persistent pools coexist without shared lifecycle state;
- release comparisons use a production-configured compatibility baseline, not
  a deliberately cold or misconfigured competitor.

Abstraction gate: add a concrete FastCGI handler variant. Do not extract a
shared backend trait or crate merely because persistent worker and FastCGI are
both PHP executors. Re-evaluate during 0.7 only when the proxy supplies a third,
structurally different backend and repeated semantics are visible.

## 0.7 — Reverse Proxy and General-Purpose Data Plane

Goal: make Hamster useful for non-PHP applications without turning it into a
full load-balancing platform.

Scope:

- Add an HTTP upstream backend with pooled keep-alive connections, streaming
  request/response bodies, backpressure and bounded timeouts.
- Define Host, hop-by-hop header, `Forwarded`/`X-Forwarded-*` and trusted-proxy
  behavior from the 0.5 transport context.
- Treat retry as a correctness decision. The default may retry only when the
  upstream provably received no request bytes; broader idempotent retry policy
  requires explicit configuration and evidence.
- Support one upstream and a small statically configured upstream set. A basic
  round-robin policy is sufficient if multiple endpoints are included.
- Validate with at least two non-PHP upstreams and mixed static/PHP/proxy virtual
  hosts.

Exit evidence:

- cancellation, partial writes, upstream crash, malformed upstream response,
  slow clients, slow upstreams and shutdown are fault-injected;
- no hop-by-hop or untrusted forwarding metadata crosses the boundary
  incorrectly;
- buffering remains inside declared byte budgets;
- mixed-handler soak leaves no connection, process or task orphan;
- release measurements separate Hamster overhead from upstream latency.

Non-goals: service discovery, dynamic orchestration, caching proxy, automatic
canary, least-connections optimization, circuit breaker, WebSocket, HTTP/3, or
transparent replay after ambiguous delivery.

## 0.8 — Transactional Reload, Observability and Production Operation

Goal: make the now-stable resource graph operable without restarting or
guessing its state.

Scope:

- Implement transactional reload: parse and compile, validate, stage new
  listeners/backends, atomically publish the new route graph, then drain old
  resources. Failure before publication leaves the old generation serving.
- Give configuration and runtime generations explicit identity. Define which
  listener changes require a handoff rather than an in-process swap.
- Provide non-blocking structured access, error and lifecycle logs with secret
  redaction and request/generation correlation.
- Expose bounded metrics and health/readiness information for HTTP,
  static/FastCGI/persistent/proxy backends, queues, failures and reload state.
  The administrative surface must not silently occupy an application route or
  be publicly exposed by default.
- Qualify Windows Service start/stop/recovery and child containment. Qualify
  Linux systemd signals, process groups and shutdown. Exact OS mechanisms need
  platform-specific design and fault injection.
- Define CLI operations for validate, serve, status, reload and stop. A GUI is
  not required.

Exit evidence:

- invalid or partially failed reloads never disturb the serving generation;
- accepted requests finish under the old generation and new requests use the
  published generation without duplicate execution;
- repeated reload, service stop, parent failure and backend crash tests leave
  no orphan resources on both platforms;
- logging/metrics backpressure cannot block the request hot path and secrets do
  not appear in diagnostics;
- operator documentation covers recovery from every degraded state.

Ordering rationale: reload follows FastCGI and proxy so it is designed around
the actual heterogeneous resource graph, not a hypothetical abstraction.

FastCGI and reverse proxy are sibling branches after 0.5, not a hard technical
dependency on each other. The proposal puts FastCGI first because it completes
the existing PHP compatibility story and supplies a second execution model
before the broader proxy surface. The owner may reverse 0.6 and 0.7 for an
edge-first product without changing later dependencies; reload should still
follow both.

## 0.9 — Secure Edge and Release Qualification

Goal: choose and qualify the public-edge profile for 1.0, then stop expanding
features.

Required for an edge-capable 1.0:

- TLS 1.2/1.3 with secure defaults, certificate/key loading, SNI and
  transactional certificate reload;
- HTTP/2 over negotiated TLS, using the same request context, limits,
  cancellation and backend contracts;
- cross-platform packaging, upgrade/rollback instructions and compatibility
  matrices;
- long mixed-workload, connection, memory, worker, FastCGI, proxy, reload and
  TLS failure soaks;
- security review of HTTP parsing policy, forwarding trust, filesystem
  containment, configuration secrets, admin exposure and backend isolation;
- reproducible Windows and Linux release benchmarks with declared saturation
  points rather than best-run headlines.

An origin-only 1.0 may defer TLS and HTTP/2 only if the owner explicitly makes
an external TLS terminator/load balancer part of the supported deployment
contract. It must not then be presented as a standalone public-edge server.

Non-goals: automatic HTTPS/ACME, HTTP/3, GUI, service discovery, advanced load
balancing, adaptive topology or new framework adapters.

## 1.0 — Stable Web/Application Server

The recommended edge-capable 1.0 contract contains:

- bounded HTTP/1.1 and HTTP/2 over TLS;
- virtual hosts and deterministic routing;
- correct transport/client metadata policy;
- secure bounded-memory static serving;
- FastCGI PHP compatibility;
- persistent-worker adapter contract plus the qualified Laravel adapter;
- minimal reliable reverse proxy;
- transactional configuration reload and graceful resource drain;
- structured logs, metrics, health/readiness and CLI operation;
- Windows Service and Linux systemd production support;
- documented configuration/protocol compatibility and upgrade policy.

1.0 does not require Hamster Manager, automatic HTTPS, WebSocket, HTTP/3,
application/proxy caching, advanced load balancing, additional framework
adapters, adaptive worker sizing, HamsterLink or Morph.

## Deliberately deferred research

Morph remains valuable, but its previous 0.5 placement is rejected.

- Metrics and traces before 1.0 are operational facts; they must not be branded
  as Morph or force a future learning architecture.
- An offline worker-sizing analysis may begin after real 1.0 workload traces
  exist.
- Opt-in min/max autoscaling needs evidence that memory savings or burst
  handling outweigh cold-start latency, churn and failure complexity.
- Request capsules, StateFence inference, automatic canary/deoptimization,
  route-specific pools and HamsterLink require stable execution modes, config
  generations and observable baselines. They are post-1.0 R&D with no promised
  version.

Other post-1.0 candidates are automatic HTTPS, WebSocket proxying, static range
support if not pulled by a real workload, HTTP/3,
TransmitFile/sendfile/mmap/cache experiments, advanced proxy policies, service
discovery, container orchestration, GUI integration and additional framework
adapters selected by real demand.

## Self-review and alternatives

### Challenge: 0.5 may still be too broad

INFERENCE: metadata, HTTP bounds and a streaming static consumer form one data
contract and should be reviewed together. If implementation evidence shows the
opened-file security work dominates the milestone, split a patch release or
0.5.x sequence; do not move FastCGI ahead of an undefined request context.

### Challenge: observability appears late

The production interface appears in 0.8, but instrumentation does not. Every
milestone must add the measurements necessary to validate its own queues,
latency and failures. 0.8 stabilizes export, access control and operator UX only.

### Challenge: reload could precede proxy

INFERENCE: implementing reload against only static plus one PHP pool would
encode the wrong resource model. Startup-only configuration is acceptable
during 0.5–0.7 development; transactional reload follows once all three backend
shapes exist.

### Alternative A: origin-first 1.0

Ship 0.5–0.8, use 0.9 only for cross-platform release qualification, and
defer TLS/HTTP2. This is the shortest responsible route if Hamster is explicitly
deployed behind IIS, Caddy, Nginx or a cloud load balancer. It sacrifices the
standalone-edge claim.

### Alternative B: PHP-migration-first

Narrow 0.5 to transport metadata, HTTP bounds and native Linux, then prioritize
FastCGI in 0.6 before completing static streaming. This yields a useful PHP
migration path sooner but carries the current static memory/TOCTOU limitations
longer. Reverse proxy and reload ordering should remain unchanged.

### Alternative C: edge-first

Move reverse proxy and TLS before FastCGI. This demonstrates general-purpose
identity sooner but delays the only compatibility fallback for persistent PHP
and weakens Hamster's existing adoption path. It is not recommended unless PHP
ceases to be a first-class workload.

## Unproven assumptions and owner decisions

HYPOTHESIS: a secure opened-handle static path can improve containment and
Windows performance simultaneously. This needs a prototype and cross-platform
evidence; the roadmap must accept a correctness-first fallback.

HYPOTHESIS: a minimal in-house reverse proxy can meet the required correctness
and maintenance cost without adopting a larger proxy framework. Research must
precede implementation.

HYPOTHESIS: HTTP/2 provides enough 1.0 edge value to justify its qualification
cost. It is not required for the origin-first alternative.

HYPOTHESIS: managed Windows `php-cgi` and external Linux PHP-FPM can share a
request contract without sharing a process-supervision abstraction.

## Largest risks to 1.0

1. **Ambiguous delivery across new backends.** FastCGI and proxy errors can
   occur after side effects but before a response. Reusing generic HTTP retry
   folklore would violate the strongest existing invariant.
2. **Unbounded resource retention.** The current count-bounded worker queue is
   not a byte bound, static files are fully buffered, and slow connections add
   another ownership lifetime. A streaming API alone does not prove bounded
   memory.
3. **Reload split-brain.** Publishing routes before their resources are ready,
   or retiring old resources before accepted work drains, can create outage,
   duplicate execution or orphans.
4. **Platform divergence.** Windows pipe/filesystem behavior has already
   falsified portable assumptions. FastCGI process ownership, file containment,
   signals and service shutdown can differ materially on Linux.
5. **Proxy security semantics.** Hop-by-hop headers, authority rewriting,
   request smuggling boundaries and trusted forwarded metadata can turn a
   syntactically working proxy into an unsafe one.
6. **Operational failure outside graceful shutdown.** Existing tests strongly
   cover coordinated shutdown, but service termination, parent crash, machine
   restart and log/metrics sink failure need OS-level containment evidence.
7. **Premature compatibility freeze.** Config and worker protocol stability are
   valuable at 1.0, but freezing them before FastCGI, proxy and reload reveal
   the required model would preserve accidental 0.x constraints.
8. **Milestone breadth.** Each proposed milestone is a capability boundary,
   not a mandate for one giant change. If its fault model cannot be reviewed as
   a unit, split it into 0.x patch steps without reordering dependencies.

## Owner decision gates

Required to adopt the roadmap direction:

1. select the recommended production spine, origin-first alternative or
   edge-first alternative;
2. accept or reject keeping adaptive sizing and Morph off the pre-1.0 critical
   path;
3. decide whether 0.5 is one reviewable milestone or a 0.5.x sequence with the
   opened-handle static investigation isolated.

Required during 0.5 design, before metadata ships:

4. select the protocol-v1 compatibility policy and typed metadata mechanism.

Required before 0.6 implementation is finalized:

5. choose whether Windows FastCGI process management is built in or integrates
   an external manager.

Required before 0.9 scope is frozen:

6. decide whether 1.0 is edge-capable or explicitly origin-only and therefore
   whether HTTP/2 is a 1.0 gate;
7. pull WebSocket support before 1.0 only if a concrete required workload
   justifies it.

Required before the 1.0 release candidate:

8. identify which configuration and protocol surfaces receive a 1.0 stability
   guarantee.
