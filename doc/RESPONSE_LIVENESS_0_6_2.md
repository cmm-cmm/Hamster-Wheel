# Response Liveness 0.6.2

Status: implemented in the working tree for owner review. This document records
the concrete HTTP/1/TCP policy and the evidence used to choose it. It is not a
claim of milestone closure.

## Threat model

0.6.1 bounded the memory owned by a streaming response, but a peer that stopped
reading could leave Hyper waiting for socket write readiness forever. Because
each connection owns one admission slot, enough zero-window peers could exhaust
`max_connections` without growing application memory.

The policy must distinguish that condition from a client that is slow but still
causes bytes to reach the socket. It must not apply while a PHP/application
transaction is running, and it must not treat producing another body frame as
network progress.

There is an unavoidable boundary: a peer that deliberately makes some progress
forever can also hold a slot forever. Proving a finite total response lifetime
would require either a maximum duration or a minimum accepted transfer rate.
Both reject some progressing clients and need a separate product policy. 0.6.2
therefore guarantees a bound for **zero socket-write progress**, which is the
measured 0.6.1 failure, rather than claiming an absolute response-duration
bound.

## Source findings

The exact dependency versions reviewed were Hyper 1.11.1, hyper-util 0.1.20 and
Tokio 1.53.1 from `Cargo.lock`.

- Hyper polls a response body only while its HTTP/1 write buffer can accept
  another body item. It can therefore accept a body frame before the underlying
  socket accepts those bytes.
- Hyper drains that buffer through the `hyper::rt::Write` implementation and
  stops when Tokio's socket `poll_write`/`poll_write_vectored` returns
  `Pending`.
- `TokioIo` delegates those calls directly to Tokio `AsyncWrite`.
- Tokio registers the connection waker when a TCP write is not ready. A later
  writable event causes Hyper to retry.

FACT: body-frame production is not a trustworthy write-progress signal. The
observable boundary that matches the failure is a successful underlying socket
write of `n > 0` bytes.

## Concrete policy

`WriteProgressTimeout<T>` wraps the TCP stream before `TokioIo`:

1. Reads pass through unchanged and do not arm this policy.
2. A successful socket write of at least one byte is progress.
3. The first `Pending` socket write arms one inactivity timer.
4. Repeated `Pending` polls retain the original deadline.
5. Any positive write disarms it; a later stall receives a fresh interval.
6. If the timer wins, the wrapper returns `io::ErrorKind::TimedOut`. Hyper fails
   the in-progress response and drops the HTTP/1 connection.

The inner write is polled before the timer on every wake. If write readiness and
the deadline become ready together, actual progress wins. The timer allocation
is lazy and reused: an unstalled connection owns no Tokio timer and a stalled
connection owns at most one. No watchdog task, global timer collection, or
per-frame state exists.

`server.write_progress_timeout` defaults to 30 seconds and accepts 1-3600. It is
an inactivity interval, not a total response deadline and not a minimum-rate
policy.

## Lifecycle and failure semantics

The timer cannot fire while the service future is executing because Hyper has
not attempted a response socket write yet. PHP protocol v1 still reads and
validates a complete staged response before the HTTP handler returns it.
Dropping a client connection after response writing begins does not cancel or
replay a worker transaction; the worker pool remains the authority for that
lifecycle.

A timeout after headers or body bytes have been written closes the connection.
It cannot synthesize another status or a shorter valid `Content-Length`
response. The existing mid-stream failure test continues to require a new
connection before another request is served.

Graceful shutdown retains its independent outer drain deadline. Write
inactivity may end a stalled connection sooner, while a progressing peer still
cannot extend server shutdown beyond the existing shutdown bound.

Timed-out peer writes are expected adversarial outcomes. Hyper wraps the
underlying `io::Error`, so connection error classification walks the source
chain and suppresses only `io::ErrorKind::TimedOut`; otherwise every reclaimed
peer could create synchronous stderr pressure.

## Evidence

The checked in tests cover:

- deterministic in-memory backpressure that produces `TimedOut` after zero
  progress;
- one-byte-at-a-time progress slower than normal throughput but faster than the
  inactivity interval;
- read/keep-alive inactivity remaining outside this policy;
- a real TCP/static response with `max_connections = 1`, where a stalled first
  response must close before a queued healthy connection can be served;
- partial-response close, released file ownership, ordinary keep-alive,
  bounded shutdown and the existing PHP cancellation/at-most-once controls.

The release TCP probe with a two-second interval reclaimed the admission slot
in 2008.3 ms and emitted zero peer-controlled stderr bytes. A second client
reading about 36 KiB/s remained connected for six seconds, three complete
timeout intervals. The reproducible driver is
`benchmarks/response_write_liveness.ps1`; raw performance data and methodology
are in `benchmarks/results/response_liveness_0_6_2_20260916.md`.

## Limits and hypotheses

- FACT: a peer making no accepted socket-write progress is bounded after the
  first backpressured write plus the configured interval.
- FACT: a peer making positive progress resets the inactivity interval.
- INFERENCE: for a finite static file and a completely non-reading TCP peer,
  finite kernel buffers eventually make the socket write pending. The bound is
  therefore not measured from response start; it begins after those buffers
  fill.
- HYPOTHESIS: 30 seconds is a useful general-purpose default. The mechanism is
  verified, but the default needs deployment evidence across high-latency and
  low-bandwidth clients.
- A deliberate trickle receiver can retain admission indefinitely. Closing it
  requires an explicit minimum-rate or total-duration contract, not a hidden
  change to inactivity semantics.
- The policy observes socket writes, not a response producer stuck before it
  yields a frame. A hung filesystem read is a separate producer-liveness
  failure; Tokio's Windows file implementation can also leave an already
  running blocking read alive until that OS read completes.
- Native Linux behavior remains unverified.

