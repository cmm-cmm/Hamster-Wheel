# Concrete streaming response — Windows evidence — 2026-09-16

Status: working-tree evidence for owner review. No release claim.

Machine: Windows 11 Pro build 26200, Core i5-12400, 32 GiB, MSVC release build.
The baseline binary was copied immediately before the 0.6.1 changes from the
0.5 working tree later committed as `5e09042`. Candidate and baseline used the
same configuration and machine session.

## Slow-reader memory

`benchmarks/static_response_memory.ps1` creates a 256 MiB file, gives every TCP
client a 512-byte receive buffer, sends GET and reads nothing. Values are peak
process-private deltas from a ready idle server.

| connections | fully buffered baseline | streaming, 128 KiB chunks | after candidate disconnect |
|---:|---:|---:|---:|
| 1 | 256.57 MiB | 0.34 MiB | 0.36 MiB |
| 4 | 1026.36 MiB | 1.37 MiB | 0.47 MiB |
| 16 | not run | 4.96 MiB | 0.87 MiB |
| 32 | not run | 9.39 MiB | 1.20 MiB |

FACT: the probe independently reproduced the 0.5 four-client result within
0.04 MiB. The candidate's 32-client peak is 109 times smaller than the old
four-client peak even though it has eight times as many clients.

INFERENCE: retained application memory is no longer proportional to file size.
The observed candidate slope is approximately 0.29 MiB/connection, covering a
128 KiB emitted frame, Tokio's bounded file buffer, Hyper/socket buffers and
allocator overhead.

## Chunk-size falsification

Same 256 MiB slow-reader probe at 32 connections, plus three 1 MiB file runs at
concurrency 8 and 2,000 requests:

| chunk | slow-reader private delta | median 1 MiB RPS | median server CPU us/request |
|---:|---:|---:|---:|
| 64 KiB | 5.34 MiB | 3,040 | 1,648 |
| 128 KiB | 9.39 MiB | 3,118 | 1,305 |
| 256 KiB | 17.59 MiB | 2,888 | 1,461 |

FACT: 64 KiB was not free merely because it minimized memory: sixteen reads per
1 MiB response made it the highest-CPU candidate. 256 KiB retained 87% more
slow-reader memory than 128 KiB and did not improve throughput. 128 KiB was
retained.

## Final release A/B

Every cell is three interleaved fresh-server runs. Small-file runs use the
21-byte `public/index.html`. The large cell uses a generated, cache-warmed 1 MiB
file; client and server share the host, so it measures the Hamster/loopback path,
not physical-disk throughput.

| workload | metric | buffered baseline | streaming candidate | change |
|---|---|---:|---:|---:|
| 21 B, C1, 30k requests | RPS | 4,167 | 4,553 | +9.3% |
| | p50 | 0.229 ms | 0.210 ms | -8.3% |
| | CPU/request | 243.750 us | 244.271 us | +0.2% |
| 21 B, C32, 50k requests | RPS | 7,358 | 8,669 | +17.8% |
| | p50 | 4.305 ms | 3.630 ms | -15.7% |
| | CPU/request | 426.250 us | 438.750 us | +2.9% |
| 1 MiB, C8, 10k requests | RPS | 2,492 | 3,266 | +31.1% |
| | p50 | 2.833 ms | 2.026 ms | -28.5% |
| | p95 | 4.447 ms | 3.513 ms | -21.0% |
| | p99 | 18.024 ms | 18.264 ms | inconclusive |
| | CPU/request | 1,620.312 us | 1,378.125 us | -14.9% |

Raw accepted run values, in execution order:

| workload / arm | RPS | p50 ms | CPU us/request |
|---|---|---|---|
| 21 B C1 baseline | 3,999 / 4,167 / 4,290 | 0.235 / 0.229 / 0.224 | 246.354 / 243.750 / 225.521 |
| 21 B C1 candidate | 4,473 / 4,582 / 4,553 | 0.199 / 0.210 / 0.211 | 244.271 / 238.021 / 251.562 |
| 21 B C32 baseline | 7,050 / 7,358 / 7,585 | 4.461 / 4.305 / 4.150 | 423.750 / 426.250 / 432.188 |
| 21 B C32 candidate | 8,574 / 8,802 / 8,669 | 3.693 / 3.581 / 3.630 | 438.750 / 435.000 / 441.250 |
| 1 MiB C8 baseline | 2,491 / 2,492 / 2,517 | 2.866 / 2.830 / 2.833 | 1,645.312 / 1,409.375 / 1,620.312 |
| 1 MiB C8 candidate | 3,231 / 3,336 / 3,266 | 2.033 / 2.003 / 2.026 | 1,300.000 / 1,378.125 / 1,378.125 |

FACT: all three RPS samples in every candidate cell exceeded its paired
baseline. Small-file C1 CPU is unchanged within measurement precision. The C32
candidate uses slightly more CPU/request but completes sooner by using more
cores. The longer 1 MiB cell improves both throughput and median CPU/request;
baseline CPU samples still span 1,409-1,645 us, so the exact percentage is less
stable than the consistent throughput result.

INFERENCE: the main gain is execution model, not cached file contents: the old
path awaited a complete filesystem read before returning a response, while the
candidate overlaps bounded file reads with socket delivery.

## Buffered PHP control

Three interleaved W1/C1 `php_hello` runs, 10,000 measured requests after 1,000
warm-ups, kept protocol v1 and response buffering unchanged. Baseline RPS was
5,494/6,797/6,919; candidate was 6,945/6,956/6,926. The first baseline also had
an anomalous 127 ms bootstrap versus 25-27 ms in every other run.

FACT: excluding nothing, medians are 6,797 versus 6,945 RPS and 0.141 versus
0.137 ms p50. The session does not show a buffered-response regression; the
first baseline anomaly and coarse process CPU counters prevent a small positive
claim.

## Reliability evidence

FACT: checked tests cover exact multi-frame delivery, early EOF, body drop,
GET/HEAD, keep-alive, partial-body connection closure, rejection of a pipelined
request after body corruption, server recovery on a fresh connection, and
bounded shutdown with a stalled 32 MiB response.

FACT: targeted real-PHP tests pass for keep-alive, cancellation safety,
possibly-started no-replay, busy-worker shutdown, queued/disconnected shutdown
drain, and Laravel cleanup-before-response commit.

FACT: a normal-operation slow reader remains active beyond
`header_read_timeout`: with a two-second setting, one 256 MiB response still
held its bounded 0.33 MiB private-memory delta after four seconds. This
falsifies using the header timer as a response-write deadline. It is a
connection-lifetime gap, not a memory-bound failure.
