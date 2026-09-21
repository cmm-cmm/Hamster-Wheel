# Concrete Streaming Response 0.6.1

Status: implemented in the working tree for owner review. It is not committed,
tagged or released. Windows measurements are recorded in
`benchmarks/results/streaming_response_0_6_1_20260916.md`. Native Linux remains
unverified.

## Scope and ownership

`ResponseBody` is one internal concrete enum with two shapes:

- `Buffered(Full<Bytes>)` retains the existing simple and PHP response path;
- `File(FileBody)` owns one already-open Tokio file and emits at most 128 KiB
  per frame.

This is not a backend trait, plugin API or public response SDK. Protocol v1 and
the worker transaction boundary are unchanged. A PHP response is still fully
validated and buffered before it is exposed to Hyper.

Hyper 1.11.1 polls another body frame only while its HTTP/1 connection can
buffer/write more data. The file body therefore reads only when the transport
asks for more, which preserves socket backpressure. Its exact remaining length
is reported through `Body::size_hint` and decremented only after a successful
read.

The 128 KiB limit is applied both to the emitted frame and to
`tokio::fs::File::set_max_buf_size`. Tokio's Windows file path otherwise has a
2 MiB default internal buffer, which would make a smaller Hamster frame a
misleading memory bound. Measurements of 64, 128 and 256 KiB selected 128 KiB:
64 KiB paid materially more per-frame CPU, while 256 KiB retained almost twice
the slow-reader memory without improving throughput.

The resolved file is opened before the response is constructed and metadata is
read from that handle. This removes the old second path-based metadata/read
pair, but it does not solve the existing canonicalize-to-open containment race;
opened-handle containment remains a separate static security design.

Later work, recorded in `doc/STATIC_CONTAINMENT.md`, replaces that race on
Windows: containment is now decided from the opened handle's final location,
and the same handle is streamed. Non-Windows targets still use the pathname
model described above.

## Failure semantics

If a read fails after response headers are committed, or the open file reaches
EOF before the declared response length, `FileBody` yields an `io::Error` once
and then ends. Hyper treats the body error as a connection error and closes the
connection. Hamster does not attempt to emit a replacement status on a response
whose 200 and `Content-Length` are already on the wire.

A checked transport regression sends a five-byte file with a declared length
of ten followed by a pipelined request. The client receives only the partial
first response, the connection closes, the pipelined request is not served, and
a new connection succeeds. Protocol corruption therefore cannot become a
valid short response or a reused keep-alive exchange.

Disconnect drops the connection future and its owned `ResponseBody`. A Tokio
file read already running on the blocking pool may retain its OS handle until
that bounded 128 KiB read completes; no subsequent whole-file task or buffer
survives it. The memory probe shows private memory returning to within 1.20 MiB
of idle after as many as 32 slow clients disconnect.

Streaming exposed a graceful-shutdown regression: once Hyper has begun an
active streaming body, `header_read_timeout` no longer expires it. Waiting for
`connection.await` after `graceful_shutdown()` could therefore hang forever on
a non-reading client. Shutdown now gives each connection one
`header_read_timeout` interval to drain and then drops that HTTP connection.
Dropping it cannot replay or cancel an owned worker transaction; the existing
pool cancellation contract continues processing the transaction, and worker
shutdown follows after connection tasks converge. Targeted cancellation,
possibly-started and busy-shutdown PHP regressions pass.

## Resource bound

The old static path retained one complete file per response. The new retained
application memory is proportional to active connections and the fixed chunk,
not file size. With a 256 MiB file and 32 non-reading clients the measured
private delta is 9.39 MiB, versus 1026.36 MiB for four clients on the old path.

This is not a total response-memory budget. Hyper/socket buffers, Tokio's file
buffer and allocator overhead remain outside the 128 KiB frame. The measured
32-client slope is about 0.29 MiB per connection. `max_connections` still bounds
the population.

## Deliberate non-scope and remaining gap

0.6.1 does not add proxying, protocol streaming, sendfile, mmap, ranges,
validators, a backend abstraction or an aggregate response-byte semaphore.

A non-reading client can keep an ordinary streaming response active beyond
`header_read_timeout`; a four-second probe with a two-second header timeout
still retained the stream. Memory and connection count are bounded, but the
connection slot lifetime is not. The shutdown path is explicitly bounded.
Choosing a normal-operation response write/inactivity policy is a later
HTTP-transport decision because it affects all handlers and cannot be inferred
from a header timer.

Follow-on status: 0.6.2 implements that decision as socket-write inactivity,
documented in `doc/RESPONSE_LIVENESS_0_6_2.md`. This section remains the 0.6.1
finding that motivated it.
