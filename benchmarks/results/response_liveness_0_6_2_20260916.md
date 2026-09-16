# Response liveness 0.6.2 raw evidence — 2026-09-16

Machine: Windows 11 build 26200, Intel Core i5-12400, 32 GiB RAM. Both binaries
were MSVC release builds. The baseline executable was copied from the complete
0.6.1 working tree before the 0.6.2 source change. The candidate added the
write-progress wrapper and configuration; application/static behavior was
otherwise identical.

## TCP liveness fault probe

Command:

```powershell
.\benchmarks\response_write_liveness.ps1 `
  -WriteProgressTimeoutSeconds 2 `
  -ProgressDurationSeconds 6 `
  -ProgressReadBytes 4096 `
  -ProgressIntervalMilliseconds 100
```

The driver creates a sparse 256 MiB file and uses `max_connections = 1`.
The zero-progress client requests the file with a 512-byte receive buffer and
stops reading immediately after the response head. A healthy connection waits
in the listen backlog, so its response time measures release of the first
connection's admission slot. The progress case reads a small amount every
100 ms for three timeout intervals.

```text
CASE=zero_progress TIMEOUT_S=2 WAITING_RESPONSE_MS=2008.3 PEER_CONTROLLED_STDERR_BYTES=0
CASE=slow_progress TIMEOUT_S=2 OBSERVED_S=6 BYTES_READ=221184 READ_BYTES_PER_S=36864.0
```

The checked TCP test uses a 200 ms interval and also verifies that the timed-out
body is incomplete, the connection closes, the file handle is released, and a
healthy response is served after the slot returns.

## Static hot-path A/B

Driver: `benchmarks/static_http.ps1`, 2,000 warm-up requests, fresh server per
sample, loopback 22-byte static response. The first three C1 pairs ran
baseline then candidate. Three more C1 pairs reversed the order. C32 used three
baseline-then-candidate pairs. CPU is server process CPU only.

### C1, 50,000 requests per sample

| variant | RPS samples | CPU us/request samples |
|---|---|---|
| baseline | 4546, 4585, 4655, 4448, 4359, 4172 | 244.375, 240.000, 228.125, 249.688, 245.938, 262.500 |
| candidate | 4417, 4583, 4284, 4135, 4444, 4490 | 250.312, 241.250, 249.062, 265.625, 240.938, 235.312 |

Using the existing driver's upper-middle convention, medians were 4546 versus
4444 RPS (-2.2%) and 245.938 versus 249.062 CPU us/request (+1.3%). Median p50
was 0.214 ms for both. Within-arm RPS spread was about 10% for both binaries;
paired signs changed after reversing order.

### C32, 100,000 requests per sample

| variant | RPS samples | CPU us/request samples |
|---|---|---|
| baseline | 8776, 9030, 9080 | 408.125, 412.656, 387.188 |
| candidate | 9085, 9442, 9089 | 400.625, 395.469, 402.188 |

Medians were 9030 versus 9089 RPS (+0.7%), 408.125 versus 400.625 CPU
us/request (-1.8%), and p50 3.482 versus 3.467 ms.

FACT: no material C32 regression is visible. FACT: the C1 point estimates move
against the candidate, but the difference is smaller than run-to-run spread and
the paired effect is not stable. The defensible conclusion is a possible low
single-digit C1 cost, not zero cost and not a demonstrated material regression.

## Scope

The fault probe and A/B test are Windows-native. They do not qualify Linux,
minimum-rate policy, physical-disk behavior, or response producers that stop
yielding frames before a socket write is pending.

