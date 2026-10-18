# Health, Readiness and Metrics (0.8 slice)

Status: implemented in the working tree, default off, gated on Windows
`x86_64-pc-windows-msvc`. The release A/B benchmark for the per-request
counters was taken once and is **inconclusive**; it rejects a large regression
and claims no cost figure (see *Measurement*). This is the second slice of
the 0.8 scope in `doc/ROADMAP_0_5_TO_1_0.md`, after
`doc/ACCESS_LOG_0_8.md`. Transactional configuration reload with generation
identity, lifecycle records and a `validate` CLI operation are still missing.

## The constraint that shaped it

The 0.8 scope requires that this surface "must not silently occupy an
application route or be publicly exposed by default". Both halves are
structural here rather than advisory:

- It is a **separate listener** with its own address, its own connection bound
  and its own three fixed routes. It never receives the application's
  `HostRouter`, so no configuration can make `/metrics` shadow an application
  path, and no application host can be asked for it. A test asks the
  application listener for `/metrics`, `/health` and `/ready` and requires the
  application's own answer.
- It **does not exist** unless `[server.admin]` names an address, and that
  address is the operator's choice. Binding it to something other than
  loopback logs a warning on startup naming what is now reachable.

## Configuration

```toml
[server.admin]
listen = "127.0.0.1:9090"
```

`listen` is the only key. The address is bound before pools start and before
the application listeners are reported, so an address already in use is a
startup failure rather than a surface that silently went missing. Naming an
address an application listener already uses is a configuration error; two
port-`0` addresses are not a clash, because port 0 asks the operating system
for a free port.

There is no TLS and no authentication on this listener. That is a boundary,
not an omission to work around: anyone who can reach the socket can read the
server's operational state, so reaching it must be a privilege the network
already grants. Keep it on loopback and let the supervisor, a tunnel or a
firewall decide who gets there.

## The three routes

| Route | Meaning |
|---|---|
| `GET /health` | Liveness. 200 `ok` whenever the process can answer at all, including while draining |
| `GET /ready` | Readiness. 200 `ready`, or 503 with the reason on one line |
| `GET /metrics` | Prometheus text exposition, `text/plain; version=0.0.4` |

`HEAD` is accepted, every other method is 405, and every other path is 404.
Answers carry `Cache-Control: no-store`, because they describe this instant.

### Health is not readiness

`/health` answers while the server drains. `/ready` does not: it returns 503
`draining` from the moment a stop is triggered until the process exits. That
distinction is the whole point of having both. A supervisor that restarts on a
failed health check must not restart a server that is deliberately stopping,
and a load balancer that keeps sending traffic to a draining server is filling
a queue that is closing.

Readiness is 503 in exactly two cases:

- `draining` — a graceful stop has begun (control `stop`, Ctrl+C, or a
  Service stop that runs `--control stop`);
- `php_pool <label> has no live worker` — that pool cannot serve its
  locations at all.

A pool below its target but still serving is **degraded, not unready**:
removing a partly healthy instance from a load balancer during a worker
replacement storm would make an incident worse. Read `hamster_php_pool_workers`
for that distinction.

FastCGI endpoints and proxy upstreams are deliberately **not** part of
readiness. Hamster does not own, start or probe them, and
`doc/WINDOWS_OPERATIONS.md` says the same about startup, so reporting
readiness for a process it does not supervise would be a claim it cannot
support. Their own counters are exposed
instead, and an operator who wants them in a readiness decision must make that
decision outside Hamster.

## The exposition

Cardinality is bounded by the configuration, never by traffic. Labels come
from pool, upstream, FastCGI endpoint and certificate names an operator wrote
down, plus a fixed status class and a fixed set of duration buckets. Nothing
is labelled by request path, peer or exact status code, so no sequence of
requests can grow the metric set. Configured names are escaped before they
reach a label.

HTTP, from counters this slice adds:

```text
hamster_up 1
hamster_ready 1
hamster_draining 0
hamster_uptime_seconds 412.880
hamster_connections_total 5
hamster_connections_active 0
hamster_connections_limit 1024
hamster_requests_total{status="2xx"} 3
hamster_requests_total{status="4xx"} 2
hamster_requests_incomplete_total 0
hamster_response_bytes_total 63
hamster_request_duration_seconds_bucket{le="0.001"} 2
...
hamster_request_duration_seconds_bucket{le="+Inf"} 5
hamster_request_duration_seconds_sum 0.004
hamster_request_duration_seconds_count 5
```

`hamster_requests_incomplete_total` counts responses the client did not
receive in full, which is the same judgement the access log records as
`complete:false`. Both come from the one place a finished request is observed,
so they cannot disagree. Scrapes and health checks are **not** counted as
application traffic: the administrative listener does not touch these
counters.

Buckets are fixed at 1 ms, 5, 10, 25, 50, 100, 250, 500 ms, 1 s, 2.5, 5, 10
and `+Inf`. They are deliberately not configurable: a configurable histogram
is a configurable cardinality.

Everything else is read from snapshots the subsystems already keep for
`--control status`, so nothing is counted twice:

- `hamster_php_pool_workers{pool,state="target"|"live"|"idle_ready"}`,
  `hamster_php_pool_respawns_total{pool}`,
  `hamster_php_pool_execution_timeouts_total{pool}`;
- `hamster_proxy_*{upstream}` — active exchanges, idle connections, opened and
  reused connections, safe retries, connect failures, upstream timeouts and
  failures;
- `hamster_fastcgi_*{endpoint}` — active exchanges, scripts, static files,
  connect failures, busy refusals, timeouts, backend failures;
- `hamster_tls_certificate_expiry_days{field}` and the handshake outcome
  counters;
- `hamster_access_log_records_total{result="written"|"dropped"|"failed"}` and
  `hamster_access_log_queued`, when a log is configured.

There is deliberately **no** `hamster_tls_handshakes_started_total`. That
instrument exists only in test builds, and adding it to the shipped build
would put one more atomic on every handshake; the three outcome counters are
the ones an operator acts on. This is a known gap, not an oversight: the TLS
outcome counters have no denominator.

## The control endpoint also answers readiness

A server reached only through its named pipe has no HTTP client, so
`--control status` reports the same judgement:

```text
ready=true
admin=http://127.0.0.1:9090
```

or `ready=false reason=draining`. Both come from one function, so the pipe and
`/ready` cannot disagree.

## Cost when it is off

No `[server.admin]` section means no listener, no counters allocated, and no
per-request or per-connection atomic. The response-body wrapper that observes
a finished request is created only when the access log or the counters exist;
otherwise the body is the one it always was, behind a single enum branch.

## Evidence

Unit tests in `src/metrics.rs`: a status outside the five classes is counted
once but not classified; bucket boundaries are inclusive and anything above
the last bound lands in the overflow counter rather than being lost; an
incomplete response is one request and one incompletion; the active-connection
gauge returns to zero; a configured name cannot end a label or a line.

Unit tests in `src/admin.rs`: only the three documented paths answer; a write
method is refused before a path is considered; a draining server is alive but
not ready; the exposition reports readiness, the fixed histogram, cumulative
buckets, a `# TYPE` line for every sample, and nothing labelled by anything a
client chooses; a loopback address is not reported as public; readiness
ignores backends Hamster does not own.

Integration tests in `tests/admin_endpoint.rs`, against real sockets: the
application listener answers `/metrics`, `/health` and `/ready` as the
application and never leaks the exposition; the administrative listener serves
the three routes with the scraper's content type; a draining server answers
`/health` 200 and `/ready` 503; application traffic moves the counters a
scrape then reports, while the scrape itself is not counted; writes are 405
and unknown paths 404; a configuration without the section has no surface.

Process-level, in `tests/windows_control.rs`: a detached server with a PHP
pool reports `/ready` 200 and `ready=true` before a stop; with a three-second
request in flight, a control `stop` makes `/ready` answer 503 `draining` and
`/health` still answer 200, `hamster_draining 1`, `hamster_ready 0`, the pool
still showing one live worker, while `status` reports `ready=false` — then the
in-flight request completes and the process exits 0.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite, 278 tests;
ignored Windows suites `windows_control` 14/14, `windows_headless` 2/2,
`windows_containment` 4/4, `tls_reload_process` 3/3; the non-Windows cfg-flip
compile check; MSVC release build; `cargo bench --no-run --features
profiling` linking all 11 benches; and a manual probe of the release binary
(startup lines, three app requests and one 404 counted as `2xx`/`4xx`, the
application listener answering `/metrics` with 404, a scrape showing the
counters, `--control status` reporting `ready=true` and `admin=`, and the
process exiting 0 on `--control stop`).

## Measurement

Taken once and **inconclusive**, recorded in `doc/BENCHMARKS.md` under
*Observability counters A/B — inconclusive, with the reason (2026-10-12)*: five
order-balanced pairs of 100,000-request runs with `[server.admin]` present and
absent, on a quiet machine (4% background load), nothing scraping the
administrative listener.

Order-balanced the RPS difference was -3.5% with a +2.2% ordering bias, but the
same comparison produced -27.6% in one pair and +18.4% in another, so the run
cannot resolve a small cost. The reason is in the driver's own numbers rather
than dismissed as variance: across ten runs the server process used 3.44-3.77
effective cores, a 10% spread, while throughput ranged 3,768-6,062 RPS, a 61%
spread. The server was CPU-saturated every time and got nearly the same CPU;
what differed was how much work that CPU delivered, which on this
performance/efficient-core processor is the signature of per-process thread
placement. `CPU_US_PER_REQUEST` therefore tracks placement rather than
per-request work here, and the +48.6 µs balanced CPU delta cannot be the
counters: five relaxed atomics are tens of nanoseconds, four orders of
magnitude below it.

What the run does support: no large regression, and no measurable memory
difference (10.3-11.0 MiB without the counters, 10.4-10.7 MiB with them). No
cost figure is claimed. A valid measurement needs the server pinned to the
performance cores or a machine with homogeneous cores, the in-memory `/api`
route, and the historical 500,000-request parameters; the driver does not set
affinity today.

## Known limits

- No TLS, no authentication, no IP allow-list on the administrative listener.
- No OpenMetrics, no protobuf, no `/metrics` filtering or sampling.
- No per-location or per-host request metrics, by design: that is traffic-shaped
  cardinality.
- No TLS handshake denominator, as above.
- Readiness does not consider FastCGI or proxy backends, by design.
- No generation identity in the exposition, because there is no configuration
  reload yet to have generations.
- Linux is unverified, as everywhere else in this project.
