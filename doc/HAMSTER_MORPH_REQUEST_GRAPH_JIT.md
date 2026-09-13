# Hamster Morph — Request-Graph JIT

> **The server JIT-compiles itself around your application.**

## 1. Overview

**Hamster Morph** is a proposed adaptive execution technology for **Hamster Wheel**.

Instead of compiling PHP source code into machine code, Hamster Morph specializes the **execution path of HTTP requests** based on observed production behavior.

The core idea is:

```text
Observe
  ↓
Learn
  ↓
Build specialized request execution path
  ↓
Canary
  ↓
Promote
  ↓
Continuously verify
  ↓
Deoptimize when assumptions fail
```

Hamster Morph therefore acts like a **JIT compiler for request graphs**.

Traditional JIT compilers optimize instructions and functions. Hamster Morph optimizes:

- route dispatch
- worker selection
- worker topology
- middleware execution graph
- framework bootstrap reuse
- dependency resolution
- queue path
- IPC transport
- buffer allocation
- state-reset policy
- resource affinity
- execution mode

The goal is not simply to make the web server faster.

The goal is to let the runtime **learn how an application behaves and automatically create the most efficient safe execution architecture for that application**.

---

## 2. Why Morph Exists

A conventional PHP request typically follows a generic path:

```text
HTTP Request
    ↓
Web Server
    ↓
FastCGI / PHP-FPM
    ↓
PHP Request Lifecycle
    ↓
Framework Bootstrap
    ↓
Router
    ↓
Middleware
    ↓
Container Resolution
    ↓
Controller
    ↓
Response
```

A persistent PHP worker eliminates repeated framework bootstrap, but normally keeps a generic worker-pool architecture:

```text
HTTP Request
    ↓
Router
    ↓
Generic Queue
    ↓
Dispatcher
    ↓
Generic Worker Pool
    ↓
Persistent Application
```

Hamster Morph asks a different question:

> If the same request pattern executes millions of times, why should every request continue to pass through the same generic execution machinery?

Morph observes the actual request graph and creates specialized execution paths for hot and stable workloads.

---

## 3. Request-Graph JIT

Consider a Laravel endpoint:

```text
GET /api/projects
```

The generic execution graph may be:

```text
HTTP Parser
    ↓
Host Router
    ↓
Laravel Front Controller
    ↓
Laravel Router
    ↓
Auth Middleware
    ↓
Tenant Middleware
    ↓
Service Container
    ↓
ProjectController@index
    ↓
ProjectService
    ↓
ProjectRepository
    ↓
JSON Response
```

After enough observations, Hamster Morph may discover that this graph is highly stable.

It can then generate a specialized execution definition called a **Request Execution Capsule**.

Example:

```text
Capsule #27

Route
GET /api/projects

Workload class
FAST_READ

Middleware graph
Auth → Tenant

Controller
ProjectController@index

Stable services
ProjectService
ProjectRepository

Worker class
FAST_READ_POOL

Transport
HamsterLink

Queue
Direct

State policy
StateFence SF-194

Version guards
application_hash = 8fd41a
config_hash      = 913aac
routes_hash      = 44fb02
```

The request path can then become:

```text
HTTP Request
    ↓
Capsule Lookup
    ↓
Guard Validation
    ↓
Specialized Queue / Worker
    ↓
Persistent PHP Application
    ↓
Response
```

Morph does **not** bypass application correctness rules.

It specializes only where runtime evidence proves that doing so is safe and beneficial.

---

## 4. Core Architecture

Hamster Morph combines several Hamster technologies into one closed-loop runtime.

```text
                    HAMSTER MORPH
               Request-Graph JIT Engine
                         │
        ┌────────────────┼────────────────┐
        │                │                │
        ▼                ▼                ▼
 Hamster Cortex      StateFence       Hamster Turbo
 profiling/AI        correctness      specialization
        │                │                │
        └────────────────┼────────────────┘
                         ▼
                    HamsterLink
                  optimized transport
                         │
                         ▼
                  Persistent PHP
```

### Hamster Cortex

Cortex is the control-plane intelligence layer.

It observes runtime telemetry and determines:

- which routes are hot
- which routes are stable
- which routes are CPU-bound
- which routes are I/O-bound
- which routes are memory-heavy
- optimal worker count
- whether routes should share workers
- whether a route is eligible for Turbo
- whether a specialized Capsule should be created
- whether an existing Capsule should be promoted, changed, or destroyed

Cortex should remain outside the hot path.

```text
Telemetry
    ↓
Cortex
    ↓
New immutable runtime policy
    ↓
Atomic policy swap
```

If Cortex fails, Hamster continues using the last known safe policy.

### StateFence

StateFence is the runtime correctness layer.

It verifies that persistent execution does not contaminate subsequent requests.

Possible observations include:

- superglobal state
- session identity
- request-scoped objects
- framework container scope
- open transactions
- open resources
- memory growth
- worker RSS
- worker age
- abnormal persistent state
- request identity correlation

StateFence does not exist to increase RPS directly.

Its purpose is to make aggressive specialization safe.

### Hamster Turbo

Turbo is the specialized execution backend.

A Turbo path may use:

- dedicated worker class
- dedicated queue
- direct dispatcher bypass
- pre-warmed application state
- route-specific buffer reuse
- persistent DB connections
- compiled route decisions
- CPU affinity when evidence supports it
- specialized worker topology
- HamsterLink transport

### HamsterLink

HamsterLink is a future high-performance transport layer between Hamster and PHP workers.

Possible design:

```text
             Shared Memory
Rust ───────────────────────── PHP
 │                              │
 ├─ request descriptor ───────► │
 │                              │
 │ ◄──── response descriptor ───┤
 │                              │
 └──── notification channel ────┘
```

Potential implementation concepts:

- shared-memory ring buffers
- SPSC queues
- reusable buffers
- small control channel
- event notification
- minimal payload copying

HamsterLink is an optimization layer, not a prerequisite for Morph.

---

## 5. Progressive Execution Levels

Morph should support multiple execution levels rather than a single permanent mode.

```text
LEVEL 0
Compatibility Mode
FastCGI

      ↓ qualification

LEVEL 1
Persistent Worker

      ↓ profiling + safety + benefit

LEVEL 2
Turbo Capsule
```

A single application can simultaneously use different levels.

Example:

```text
WordPress legacy plugin
→ FastCGI

Laravel /admin/*
→ Persistent Worker

Laravel /api/products
→ Turbo Capsule
```

This allows Hamster to optimize aggressively without forcing the whole application into one execution model.

---

## 6. Progressive Deoptimization

A core requirement of Morph is **deoptimization**.

Specialization must always be reversible.

```text
TURBO
  ↓ assumption fails
WORKER
  ↓ persistent safety fails
FASTCGI
```

Possible causes:

- route table changed
- application deployment changed
- config changed
- middleware graph changed
- StateFence anomaly
- worker crash rate increased
- P99 latency regressed
- memory slope increased
- error rate increased
- performance benefit disappeared

Example:

```text
Capsule #27
routes_hash = 44fb02

Deploy occurs
routes_hash = 14cc91

→ Capsule invalidated
→ requests immediately return to Normal Worker
→ Cortex profiles the new execution graph
→ new Capsule may be generated later
```

Correctness always has priority over performance.

---

## 7. Canary Promotion

Morph should never immediately route all traffic into a newly generated optimization.

Example promotion flow:

```text
Candidate Capsule
       ↓
      1%
       ↓
      5%
       ↓
     20%
       ↓
     50%
       ↓
    100%
```

At each stage Hamster compares:

- P50
- P95
- P99
- throughput
- CPU/request
- queue wait
- RSS
- memory slope
- error rate
- StateFence score

Example:

```text
Generic Worker
P50          2.40 ms
P99          6.80 ms
CPU/request  820 µs

Candidate Capsule
P50          1.60 ms
P99          4.10 ms
CPU/request  510 µs
StateFence   PASS
```

Only then should Morph promote the Capsule.

If results degrade, Morph rolls back automatically.

---

## 8. Workload Fingerprints

Each route or request class can develop a lightweight runtime fingerprint.

Possible vector:

```text
route_id
method
request_rate
queue_wait
execution_time
cpu_time
rss_before
rss_after
response_size
status
error_rate
worker_age
worker_generation
statefence_score
```

Framework adapters may contribute more information.

Laravel example:

```text
route_name
middleware_chain
db_time
query_count
cache_hits
external_io_time
container_resolution_cost
```

Example fingerprint:

```text
/api/projects

CPU                0.8 ms
DB                 1.4 ms
Queue              0.1 ms
RSS delta          12 KB
Total              2.6 ms
P99 variance       LOW
StateFence score   0.99999
```

Morph uses the fingerprint to choose execution strategy.

---

## 9. Adaptive Worker Fabric

Morph can dynamically create workload-specific worker groups.

Traditional pool:

```text
W1 W2 W3 W4

/api
/report
/import
```

Morph may learn:

```text
/api/*
short + predictable

/report/*
I/O heavy

/import/*
CPU + memory heavy
```

and reorganize execution:

```text
                    Cortex
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
     FAST POOL       I/O POOL      HEAVY POOL
      W1 W2           W3 W4            W5
        │              │               │
     /api/*         /report/*       /import/*
```

Unlike static route-to-pool configuration, this topology is learned and continuously evaluated.

---

## 10. Turbo Cells

A **Turbo Cell** is a highly specialized execution unit created for a stable hot workload.

Example:

```text
Turbo Cell: product-api

HTTP route
    ↓
Compiled route slot
    ↓
Dedicated SPSC queue
    ↓
Dedicated PHP worker
    ↓
Pre-warmed Laravel application
    ↓
Persistent services
    ↓
HamsterLink
```

Multiple cells may coexist:

```text
                      Hamster Morph
                           │
        ┌──────────────────┼──────────────────┐
        │                  │                  │
        ▼                  ▼                  ▼
 Turbo Cell: Products  Turbo Cell: Auth   Normal Pool
        W1                  W2             W3 W4
```

Cells should be created only when measurable benefit exists.

If workload changes, Morph may resize, merge, destroy, or demote a cell.

---

## 11. Mini-AI / Online Learning

Morph does not require an LLM.

The first implementation should begin with deterministic heuristics.

Later Cortex may use:

- contextual multi-armed bandits
- Bayesian optimization
- lightweight online regression
- anomaly detection

Possible actions:

```text
NORMAL
TURBO
SPLIT_POOL
MERGE_POOL
ADD_WORKER
REMOVE_WORKER
MOVE_ROUTE
HAMSTERLINK
DEMOTE
QUARANTINE
```

The optimization objective should not be RPS alone.

Example:

```text
maximize:
    throughput / CPU

minimize:
    P99
    memory
    queue_wait

subject to:
    error_rate <= baseline
    StateFence == safe
    correctness_regression == false
```

The AI/control system must never trade correctness for benchmark gains.

---

## 12. Execution Genome

Each application can build a persistent learned profile called an **Execution Genome**.

Example:

```text
PMS.execution-profile

/api/projects
  class        fast-read
  optimal      turbo-2
  cpu          410us
  p99          3.2ms
  safe_score   0.99998

/report/*
  class        io-heavy
  optimal      pool-io-3

/import
  class        memory-heavy
  optimal      isolated-worker
```

On restart:

```text
Hamster starts
    ↓
Load previous genome
    ↓
Warm candidate topology
    ↓
Revalidate assumptions
    ↓
Reuse safe learned optimizations
```

The system must never trust an old genome blindly after application/config/runtime changes.

Every reused specialization needs guards and revalidation.

---

## 13. Request Execution Capsule

A Capsule is the compiled output of Morph.

Conceptual structure:

```text
RequestExecutionCapsule
{
    route_match
    route_version
    application_version
    config_version

    workload_class
    worker_class
    queue_strategy
    transport_strategy

    middleware_fingerprint
    framework_adapter

    state_policy
    safety_score

    expected_cpu_range
    expected_latency_range
    expected_memory_range

    promotion_state
    generation
}
```

A Capsule must contain enough guard information to determine whether its assumptions remain valid.

---

## 14. Guard System

Before a Capsule can execute, lightweight guards validate assumptions.

Possible guards:

```text
application hash
route hash
config hash
PHP version
extension set
framework version
worker generation
StateFence status
runtime policy generation
```

Fast guard path:

```text
Request
   ↓
Capsule found
   ↓
Guard pass?
   ├─ YES → specialized path
   └─ NO  → generic worker path
```

Guard failure is normal behavior, not an exception.

---

## 15. Safety Model

Morph must follow these principles:

### Correctness before performance

No optimization may weaken request correctness or isolation.

### No unsafe retry

If a PHP transaction may already have started, Hamster must not transparently retry the same request on another backend.

Example:

```text
POST /payment
    ↓
worker may have written DB
    ↓
worker crashes

→ request fails
→ future requests may use another execution mode
→ original request is NOT automatically replayed
```

### Specialized execution must be disposable

Any Turbo path or Capsule must be removable without server restart.

### Generic path is always available

Morph optimization is optional.

Failure of Cortex, Capsule generation, or Turbo must never make the basic server unavailable.

---

## 16. Relationship with StateFence

Morph and StateFence form a feedback loop.

```text
Morph specializes
       ↓
StateFence monitors
       ↓
Safe?
 ┌─────┴─────┐
YES          NO
 │            │
keep       deoptimize
```

Example anomaly:

```text
Request A
user_id = 123

Request B
user_id = 456

Persistent object still references 123
```

StateFence response:

```text
worker → SUSPECT
worker → DRAINING
replacement → READY
Capsule safety score ↓
route → NORMAL WORKER
```

Repeated violations may cause:

```text
TURBO
  ↓
WORKER
  ↓
FASTCGI
```

---

## 17. Relationship with HamsterLink

HamsterLink should be activated only when IPC becomes a meaningful part of request cost.

For example:

```text
Total request = 50 ms
IPC = 0.05 ms
```

Optimizing IPC provides almost no benefit.

But for:

```text
Total request = 0.40 ms
IPC = 0.08 ms
```

transport optimization may matter significantly.

Therefore Morph should decide whether HamsterLink is justified based on measurements.

---

## 18. Expected Performance Behavior

Morph should not promise a fixed speedup.

Expected benefit depends on workload.

Illustrative engineering targets only:

| Workload | Traditional baseline | Potential Morph benefit |
|---|---:|---:|
| Plain PHP | 1.0x | 1.3–1.7x |
| Laravel basic | 1.0x | 2.0–3.0x |
| Laravel realistic | 1.0x | 1.7–2.5x |
| Bootstrap-heavy Laravel | 1.0x | 2.5–4.0x |
| DB-heavy | 1.0x | 1.1–1.5x |
| External-API-heavy | 1.0x | small |

These are targets/hypotheses, not performance claims.

Every optimization requires benchmark evidence.

---

## 19. Benchmark Requirements

Morph must be evaluated against modern PHP runtimes, not only Apache.

Recommended comparison set:

```text
Apache + PHP-FPM
Nginx + PHP-FPM
OpenLiteSpeed / LSAPI
RoadRunner + Laravel Octane
FrankenPHP Worker + Laravel Octane
OpenSwoole + Laravel Octane
Hamster Worker
Hamster Morph
```

Metrics:

```text
RPS
P50
P95
P99
CPU/request
CPU utilization
RSS/worker
total memory
queue wait
context switches
worker churn
error rate
StateFence violations
```

Same hardware, PHP version, database, dataset, routes, concurrency, and business logic must be used.

---

## 20. Morph Success Criteria

Morph is successful only if it provides measurable benefit beyond the generic Hamster worker architecture.

Example gate:

```text
Morph candidate must produce one or more of:

>= 15% throughput improvement
>= 15% CPU/request reduction
>= 15% P99 reduction
>= 15% memory-efficiency improvement

while:

error rate does not increase
StateFence remains safe
correctness remains identical
```

Thresholds should eventually be configurable and based on statistically meaningful measurements.

---

## 21. Morph Development Strategy

Morph should be built incrementally.

### Phase M0 — Telemetry Foundation

Collect only the metrics necessary to understand workload behavior.

No AI.
No automatic topology changes.

### Phase M1 — Offline Analyzer

Export runtime traces and analyze them offline.

Prove that workload classification can identify useful route classes.

### Phase M2 — Runtime Heuristics

Cortex recommends:

```text
optimal worker count
route classes
split/merge candidates
Turbo candidates
```

Human approval may still be required.

### Phase M3 — Automatic Canary

Morph creates candidate Capsules and sends a small traffic percentage through them.

Automatic rollback is mandatory.

### Phase M4 — Adaptive Execution

Morph can automatically promote/demote Capsules and reshape worker topology.

### Phase M5 — Online Learning

Introduce lightweight learning algorithms only after deterministic heuristics are proven.

### Phase M6 — HamsterLink Integration

Use specialized IPC where measurement proves it matters.

---

## 22. Interaction with Hamster Wheel Roadmap

Recommended relationship with the core roadmap:

```text
0.2.0
Fixed Worker Pool + Cancellation Safety
✓ foundation

0.3.0
Worker Lifecycle + Recovery
↓

0.4.0
Laravel Persistent Feasibility
+ telemetry foundation
↓

0.5.x
Morph M0/M1
Workload profiling
↓

0.6.x
Morph M2/M3
Capsules + canary specialization
↓

0.7.x
StateFence integration
Adaptive promotion/deoptimization
↓

0.8.x
HamsterLink experiments
↓

1.0+
Production Morph runtime
```

Morph should not block basic PHP server functionality.

---

## 23. Non-Goals

Hamster Morph is **not**:

- a PHP bytecode JIT
- a replacement for Zend JIT
- an LLM inside the HTTP hot path
- generic machine learning for marketing
- automatic unsafe caching
- automatic replay of failed requests
- a reason to bypass framework correctness
- a guarantee that every route will become Turbo

Some routes may permanently remain in Normal Worker or FastCGI mode.

That is acceptable.

---

## 24. Future Research — Hamster Rewind

A separate experimental technology may later complement Morph.

**Hamster Rewind** investigates whether persistent PHP can preserve bootstrap state while rolling back request-specific mutations.

Concept:

```text
Laravel Bootstrap
      ↓
Clean State S0
      │
      ├──────────────┐
      ↓              │
Request              │
      ↓              │
Mutations            │
      ↓              │
Response             │
      ↓              │
REWIND ──────────────┘
```

Goal:

> Persistent PHP performance with stronger request-state isolation.

Possible research areas:

- process snapshots
- copy-on-write
- request overlays
- Zend extension hooks
- dirty-state tracking
- framework-aware reset contracts

Rewind is a research project and must not become a dependency of Morph or block the core roadmap.

---

## 25. Signature Concept

Hamster Morph should be described as:

> **Adaptive Request-Graph JIT for PHP applications.**

Long form:

> Hamster Morph continuously profiles real application traffic, learns stable request execution graphs, generates specialized execution capsules, validates them through canary traffic and StateFence, promotes optimizations that measurably improve efficiency, and automatically deoptimizes when runtime assumptions stop being valid.

Short form:

> **The server JIT-compiles itself around your application.**

---

## 26. Design Principles

Hamster Morph inherits the Hamster Wheel philosophy:

> **Fast. Simple. Adaptive. Observable. Measured.**

Additional Morph principles:

1. **Measure before specializing.**
2. **Correctness before performance.**
3. **Every optimization is reversible.**
4. **Never put AI inference in the HTTP hot path.**
5. **Generic execution must always remain available.**
6. **No optimization claim without benchmark evidence.**
7. **Application changes invalidate assumptions.**
8. **Specialization must outperform its own complexity.**
9. **State safety is a hard constraint, not a score to trade away.**
10. **The runtime adapts to the application, not the application to the runtime.**

---

## 27. Final Architecture Vision

```text
                         HAMSTER WHEEL
                              │
                              ▼
                        HTTP / TLS
                              │
                              ▼
                      Runtime Config
                              │
                              ▼
                       HAMSTER MORPH
                    Request-Graph JIT
                              │
           ┌──────────────────┼──────────────────┐
           │                  │                  │
           ▼                  ▼                  ▼
      Compatibility       Normal Worker       Turbo Capsule
        FastCGI              Pool                 Path
           │                  │                  │
           │                  ├──── Cortex ──────┤
           │                  │                  │
           │                  └── StateFence ────┘
           │                                     │
           │                                HamsterLink
           │                                     │
           └──────────────────┬──────────────────┘
                              ▼
                         PHP Runtime
                              │
                              ▼
                        Application
```

The long-term objective is not to build another static PHP worker pool.

The objective is to build a runtime that can:

```text
observe
→ understand
→ specialize
→ measure
→ promote
→ verify
→ deoptimize
→ learn
```

and continuously reshape itself around the real application workload.

---

## Status

**Concept / R&D proposal**

No performance claims in this document should be treated as benchmark results.

Before implementation, Morph should undergo:

- architecture review
- prior-art research
- threat/correctness analysis
- telemetry design review
- offline feasibility experiments
- benchmark plan review

