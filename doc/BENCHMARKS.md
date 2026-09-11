# Performance Baselines

These results are local development baselines, not cross-server performance claims. Milestone 0.0.1 is immutable and must not be retroactively changed or reinterpreted to improve later comparisons.

## Methodology

- Windows `x86_64-pc-windows-msvc` release build
- Loopback HTTP/1.1 request to `GET /`
- .NET `HttpClient`, response body fully consumed
- 1,000 warm-up requests
- 500,000 measured requests at concurrency 32
- 12 logical processors
- Server CPU is process CPU normalized against elapsed time and logical processor count

## Results

| Metric | 0.0.1 HTTP | 0.0.2 Static | Delta |
|---|---:|---:|---:|
| RPS | 119,284 | 5,962 | -95.0% |
| P50 | 0.249 ms | 5.326 ms | +2,039.0% |
| P95 | 0.413 ms | 5.904 ms | +1,329.5% |
| P99 | 0.670 ms | 6.700 ms | +900.0% |
| Normalized CPU | 29.8% | 21.6% | -8.2 pp |
| CPU time | 15.000 s | 217.156 s | +1,347.7% |
| CPU/request | 30.000 us | 434.312 us | +1,347.7% |
| Working set | 6.8 MiB | 8.9 MiB | +30.9% |
| Peak working set | 7.5 MiB | 9.8 MiB | +30.7% |

The 0.0.2 path intentionally performs filesystem metadata lookup, canonicalization, and an asynchronous file read for every request. It has no cache or OS-specific fast path. Lower normalized CPU does not mean less total work: the static run took 83.862 seconds versus 4.192 seconds and consumed substantially more CPU time per request.

## 0.0.2 Static Performance Investigation

This investigation does not replace either historical baseline above. It used the same release target, loopback HTTP/1.1 endpoint, static file, .NET client, 1,000 warm-up requests, 500,000 measured GET requests, concurrency 32, keep-alive, and machine. Important results use the median of three runs rather than the best run.

### Static Pipeline Breakdown

The profiling-only Cargo bench runs in an optimized bench profile and is excluded from the default production build. Each value is the median of three trials against a warm OS file cache.

| Stage | Median wall time/op | Increment from previous stage |
|---|---:|---:|
| A. Constant response construction | 0.127 us | reference only |
| B. Percent-decode + normalize | 0.127 us | pipeline start |
| C. B + join/precomputed-root validation | 0.840 us | 0.713 us |
| D. C + candidate metadata | 24.559 us | 23.719 us |
| E. D + target canonicalization/root containment | 64.071 us | 39.512 us |
| F. E + final metadata + HEAD response, no body read | 89.921 us | 25.850 us |
| G. Full static GET | 151.770 us | 61.849 us |

Decode/normalize contributes less than 0.1% of the sequential full-GET wall time. Filesystem work dominates: first metadata about 15.6%, target canonicalization about 26.0%, final metadata/HEAD response about 17.0%, and read/allocation/GET response about 40.8%. These are cumulative-stage differences, not syscall-level CPU measurements.

The canonical document root was already computed once in `server::serve` before the accept loop and stored in `Arc<PathBuf>`. It was never a per-request operation, so no “Precomputed Canonical Root” result was invented. The target remains canonicalized per request because that check rejects symlink and junction escapes.

### Observed Bottleneck

The main cost is filesystem work executed through Tokio's blocking pool, not URI parsing. On Windows, Tokio 1.53.1 sends `metadata`, `canonicalize`, and `read` through `spawn_blocking`. A regular GET performs candidate metadata, target canonicalization, final metadata, then `std::fs::read`; the latter opens the file, queries handle metadata to preallocate, allocates one `Vec<u8>`, and reads the body. `Bytes::from(Vec<u8>)` transfers ownership without copying the payload.

The micro-profile full GET is about 0.152 ms sequentially, while network P50 is 4.633 ms at concurrency 32. The gap suggests blocking-pool/filesystem scheduling and concurrent queueing are material, but this investigation did not directly instrument queue time.

### Changes Made

- Added a feature-gated, dependency-free A-G micro-profiler and a reproducible .NET load driver. Neither affects the default hot path.
- Changed HEAD to use the length from the final metadata result and construct an empty body. HEAD no longer calls `tokio::fs::read` or allocates the file-sized body buffer.
- Expanded path and reparse-point security tests. GET behavior, target canonicalization, root containment, and response payload were not weakened.

### Benchmark Before/After

| Metric | Historical 0.0.2 (one run) | Investigation runs | Investigation median | Observed delta |
|---|---:|---:|---:|---:|
| RPS | 5,962 | 6,743 / 6,742 / 6,898 | 6,743 | +13.1% |
| P50 | 5.326 ms | 4.633 / 4.678 / 4.564 ms | 4.633 ms | -13.0% |
| P95 | 5.904 ms | 5.606 / 5.473 / 5.343 ms | 5.473 ms | -7.3% |
| P99 | 6.700 ms | 6.319 / 5.856 / 5.925 ms | 5.925 ms | -11.6% |
| Working set | 8.9 MiB | 9.1 / 9.7 / 9.2 MiB | 9.2 MiB | +3.4% |
| Peak working set | 9.8 MiB | 9.3 / 10.0 / 9.5 MiB | 9.5 MiB | -3.1% |

Throughput ranged from 6,742 to 6,898 RPS (2.3% max-min range relative to the median). P50, P95, and P99 ranges were 2.5%, 4.8%, and 7.8%. The GET code path was not optimized by the HEAD change, so the difference from the single historical run must not be attributed to that change.

### CPU Metric Validation

The investigation takes the process CPU snapshot after warm-up and reports independently:

- Median process CPU time: 219.203 seconds
- Median wall time: 74.153 seconds
- Effective cores: `219.203 / 74.153 = 2.956`
- Normalized utilization: `2.956 / 12 = 24.6%`
- CPU time/request: `219.203 s / 500,000 = 438.406 us`

Wall-clock latency/request and process CPU time/request are different quantities. Historical CPU deltas took their initial CPU snapshot before warm-up, so they include the 1,000 warm-up requests. Those historical numbers remain unchanged, but they are not used as exact CPU before/after evidence.

### Security and Memory Impact

Tests cover `..`, `%2e%2e`, encoded slash, encoded and raw backslash, raw and encoded NUL, Windows colon/drive syntax, ADS, valid nested files, index files, GET, HEAD, unsupported methods, missing files, and keep-alive. Windows symlink and junction escape tests both created real reparse points on the investigation machine and confirmed `403 Forbidden`.

No security check was removed. The TOCTOU window between target canonicalization and the later open/read remains. Median working memory was 9.2 MiB and peak was 9.5 MiB; no cache or persistent file buffer was introduced.

### Open Questions

- How much of the network/micro-profile gap is Tokio blocking-pool queue time at concurrency 32?
- Can one opened file handle provide final-path containment, metadata, and body read while preserving Windows symlink/junction semantics and reducing TOCTOU exposure?
- How do cold OS cache, larger files, and concurrency levels change the breakdown?

### Rejected Optimizations

This investigation did not add RAM or metadata caches, mmap, TransmitFile, sendfile, a custom buffer pool/allocator, or remove target canonicalization. It also did not begin router or runtime-config work.

### Recommended Next Step

Before changing production code, measure blocking-pool queue time across a concurrency sweep (1, 2, 4, 8, 16, 32) while keeping the same security pipeline. If queueing remains dominant, evaluate a separate open-once, handle-based proof of concept that derives containment and metadata from the opened handle; require symlink/junction and TOCTOU-focused tests before considering it for production.

## Static Concurrency Investigation

This section is additive. It does not replace or modify the historical 0.0.1, 0.0.2, or prior investigation results.

### Methodology

- Windows `x86_64-pc-windows-msvc` release build, same machine and `public/index.html`
- Loopback HTTP/1.1 with keep-alive and the same .NET `HttpClient`
- Concurrency 1, 2, 4, 8, 16, 32, and 64
- 1,000 warm-up requests, then 100,000 measured requests per run
- Three fresh server processes per concurrency; tables report per-metric medians, not the best run
- CPU snapshot taken after warm-up; process CPU and wall time measured over the same 100,000 requests

The request count is lower than the 500,000-request historical baseline to keep the full 21-run matrix practical. Comparisons inside this matrix use the same count and policy. They do not rewrite the historical baseline.

### Concurrency Matrix

| Concurrency | RPS | P50 | P95 | P99 | Wall | Process CPU | Effective cores | CPU/request | Working set | Peak |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 3,716 | 0.250 ms | 0.399 ms | 0.554 ms | 26.908 s | 25.281 s | 0.940 | 252.812 us | 5.8 MiB | 5.8 MiB |
| 2 | 6,099 | 0.312 ms | 0.458 ms | 0.593 ms | 16.397 s | 28.938 s | 1.765 | 289.375 us | 6.3 MiB | 6.3 MiB |
| 4 | 7,464 | 0.523 ms | 0.705 ms | 0.845 ms | 13.397 s | 35.734 s | 2.667 | 357.344 us | 7.0 MiB | 7.0 MiB |
| 8 | 6,661 | 1.175 ms | 1.482 ms | 1.690 ms | 15.012 s | 43.266 s | 2.882 | 432.656 us | 7.4 MiB | 7.4 MiB |
| 16 | 6,361 | 2.485 ms | 2.963 ms | 3.397 ms | 15.720 s | 45.781 s | 2.912 | 457.812 us | 7.9 MiB | 8.0 MiB |
| 32 | 6,077 | 5.203 ms | 6.083 ms | 6.949 ms | 16.455 s | 48.031 s | 2.919 | 480.312 us | 9.3 MiB | 9.6 MiB |
| 64 | 6,507 | 9.782 ms | 10.756 ms | 11.506 ms | 15.369 s | 48.734 s | 3.171 | 487.344 us | 11.7 MiB | 12.0 MiB |

The complete recorded C32 batch is shown above. An earlier independent C32 repeat had a 6,541 RPS median, demonstrating material between-batch noise; it was not substituted into the matrix. The peak observed matrix throughput remains C4.

### Variance

| Concurrency | RPS max-min / median | P99 max-min / median |
|---:|---:|---:|
| 1 | 1.3% | 1.1% |
| 2 | 0.4% | 2.2% |
| 4 | 1.5% | 2.1% |
| 8 | 8.3% | 22.1% |
| 16 | 9.0% | 14.6% |
| 32 | 5.1% | 1.9% |
| 64 | 6.8% | 15.1% |

Variance rises after C4, consistent with a contended workload. This limits the precision of small differences inside the C8-C64 throughput plateau.

### Blocking Queue Observations

Profiling-only wrappers timestamp submission immediately before `spawn_blocking`, closure start/end on the blocking thread, and observation immediately after `await`. Thus:

- `queue_wait = closure_start - submitted`
- `filesystem_execution = closure_end - closure_start`
- `completion = await_observed - closure_end`
- `total_fs_stage_time = await_observed - submitted`

A no-op `spawn_blocking` control isolates queue/wake and completion/rescheduling overhead:

| Concurrency | Queue P50 | Completion P50 | Total P50 | Total P99 |
|---:|---:|---:|---:|---:|
| 1 | 5.6 us | 8.0 us | 13.7 us | 26.3 us |
| 2 | 3.3 us | 2.0 us | 6.1 us | 30.2 us |
| 4 | 1.7 us | 2.3 us | 5.0 us | 77.6 us |
| 8 | 1.9 us | 4.6 us | 7.6 us | 165.7 us |
| 16 | 2.3 us | 8.1 us | 11.8 us | 419.5 us |
| 32 | 2.4 us | 22.8 us | 26.4 us | 653.6 us |
| 64 | 2.3 us | 57.7 us | 62.1 us | 964.4 us |

Median queue wait does not grow with concurrency. Tail scheduling and async completion overhead do grow from C8 onward, but their medians remain far below the filesystem execution times.

Tokio 1.53.1 defaults to 512 maximum blocking threads and a 10-second idle-thread keep-alive. It queues a blocking task, wakes an idle thread or creates one below the cap, and applies no queue backpressure. With at most 64 request pipelines each awaiting one blocking operation at a time, this experiment stays well below the 512-thread cap. There is no evidence of hard `max_blocking_threads` saturation.

### Filesystem Stage Timings

Median execution time inside the blocking closure:

| Concurrency | Metadata candidate | Canonicalize target | Metadata canonical | Read file |
|---:|---:|---:|---:|---:|
| 1 | 14.9 us | 33.1 us | 13.5 us | 27.4 us |
| 2 | 24.1 us | 51.7 us | 23.5 us | 44.5 us |
| 4 | 70.8 us | 129.2 us | 69.3 us | 125.8 us |
| 8 | 187.0 us | 293.3 us | 189.1 us | 322.6 us |
| 16 | 450.6 us | 558.7 us | 448.6 us | 591.9 us |
| 32 | 983.7 us | 1,109.1 us | 978.9 us | 1,140.3 us |
| 64 | 2,085.4 us | 2,190.0 us | 2,074.6 us | 2,229.5 us |

All four filesystem operations slow sharply under concurrency. Canonicalization and read are most expensive at low concurrency, but by C32-C64 every filesystem operation is around 1-2.2 ms. That convergence points to shared Windows filesystem/kernel contention rather than one isolated parsing or allocation hotspot.

The complete four-operation pipeline shows the same scaling shape as HTTP latency:

| Concurrency | Pipeline P50 | Pipeline P95 | Pipeline P99 |
|---:|---:|---:|---:|
| 1 | 0.161 ms | 0.253 ms | 0.435 ms |
| 2 | 0.207 ms | 0.296 ms | 0.562 ms |
| 4 | 0.460 ms | 0.628 ms | 0.893 ms |
| 8 | 1.079 ms | 1.425 ms | 1.625 ms |
| 16 | 2.171 ms | 2.627 ms | 2.911 ms |
| 32 | 4.400 ms | 5.260 ms | 5.719 ms |
| 64 | 8.772 ms | 9.640 ms | 10.046 ms |

### Saturation Point, CPU, and Latency Scaling

Throughput scales from C1 to C4, reaching 7,464 RPS. From C4 to C8, throughput falls 10.8% while HTTP P50 rises 124.7%, CPU/request rises 21.1%, and effective CPU use rises only from 2.667 to 2.882 cores. Beyond C8, throughput remains roughly 6,000-6,500 RPS while latency approximately doubles whenever concurrency doubles.

Normalized process CPU is only 22.2% of 12 logical processors at C4 and remains around 24-26% through C64. Latency therefore increases before total CPU saturation. The practical saturation point is C4-C8. Evidence attributes it primarily to increased duration/contention inside Windows filesystem operations, with growing blocking-task completion/tail scheduling overhead as a secondary factor—not to URI normalization, a full CPU, or the blocking pool's thread cap.

### Measurement Limitations

- Tokio does not expose per-operation internal queue timestamps through the stable filesystem API. The wrapper uses the same `spawn_blocking + std::fs` shape but is not an internal Tokio queue probe. `queue_wait` includes enqueue, thread wake/create, and OS scheduling until closure entry.
- `completion` includes blocking-result handoff and async task rescheduling. `Instant` calls add small profiling overhead.
- The profiler uses one long-lived runtime and ascending concurrency, so blocking threads remain warm and may carry order effects.
- The tiny file is hot in the OS cache. Results do not represent cold cache, large files, or physical disk throughput.
- Client and server share one machine. Client work and Windows background activity contribute to run variance.
- Client-side HTTP latency contains networking and .NET scheduling in addition to server time.
- The matrix uses 100,000 requests per run, while historical baselines used 500,000; only within-matrix comparisons are exact.

### Security and Production Impact

Instrumentation and the no-op control compile only with the `profiling` feature. The default production static handler, traversal checks, encoded separator/backslash/NUL/ADS rejection, target canonicalization, root containment, and symlink/junction behavior were not changed. No runtime blocking-pool configuration was changed.

### Recommended Next Experiment

Do not tune `max_blocking_threads`: the measured concurrency is far below its cap and median queue wait is not the bottleneck. Next, run an investigation-only comparison of the same filesystem sequence on a fixed set of dedicated threads versus `spawn_blocking`, ideally with Windows ETW/WPA filesystem and context-switch traces at C4 and C8. This would separate Windows filesystem/API contention from Tokio handoff costs before considering any architectural change. No router, cache, or production open-once path should be implemented from the current evidence.

## 0.0.2-P3 Windows Filesystem Investigation

This section is additive. It does not replace the 0.0.1 or 0.0.2 baselines, and
none of these profiling-only results is a production benchmark.

### Methodology

- Windows 11 Pro build 26200, Intel Core i5-12400 (6 cores/12 logical
  processors), 32 GiB RAM, and a SATA SSD document root; Balanced power plan
- Rust 1.98.1, release `x86_64-pc-windows-msvc`, Tokio 1.53.1
- One 22-byte warm-cache file, or 4/16 equal-size copies selected round-robin
- 1,000 warm-up and 10,000 measured pipelines per trial, three trials; each
  reported value is the per-field median, never the best run
- The no-op handoff control was repeated with 500,000 operations so
  `GetProcessTimes` exceeded its observed 15.625 ms quantization
- CPU seconds are kernel plus user process time over the same measured window;
  effective cores are CPU/wall, and CPU/request is CPU seconds/request

The profiler timestamps submit, blocking-thread entry/exit, and observation
after `await`. Full current sequence uses four separate handoffs for both
executors, preserving `metadata(candidate) -> canonicalize ->
metadata(canonical) -> read`. HTTP parsing and network transport are excluded.

### Dedicated Threads vs `spawn_blocking`

Full-sequence pipeline throughput (pipelines/s):

| Request concurrency | Tokio | Dedicated 1 | Dedicated 2 | Dedicated 4 | Dedicated 8 |
|---:|---:|---:|---:|---:|---:|
| 1 | 5,155 | 5,169 | 4,793 | 4,831 | 4,672 |
| 4 | 7,966 | 9,751 | 10,990 | 8,140 | 8,658 |
| 8 | 7,203 | 9,918 | 11,176 | 7,964 | 7,382 |
| 16 | 7,222 | 9,904 | 10,955 | 8,136 | 7,530 |
| 32 | 7,013 | 9,864 | 11,711 | 8,156 | 7,561 |

The two-worker result is not accepted production tuning. It is evidence that
limiting simultaneous filesystem work helps. At C4, the focused repeat's
median summed queue/execution/total times were 23/420/483 us for Tokio versus
137/159/334 us with two
workers. At C32 they were 26/4,443/4,535 us versus 2,502/154/2,711 us. The fixed
pool deliberately converts kernel contention into visible queueing.

RPS max-min ranges for the focused paired batches were:

| Concurrency | Tokio range | Dedicated-2 range |
|---:|---:|---:|
| 1 | 1.1% | 4.9% |
| 4 | 10.8% | 1.0% |
| 8 | 1.6% | 3.0% |
| 16 | 2.6% | 4.6% |
| 32 | 4.5% | 1.2% |

The configurations were rerun rather than selecting a favorable trial.

The 500,000-operation no-op control isolates handoff and wake/resume cost:

| Concurrency | Tokio P50/P99 | Dedicated-2 P50/P99 | Tokio CPU/op |
|---:|---:|---:|---:|
| 1 | 14.5 / 31.4 us | 1.4 / 19.1 us | 19.688 us |
| 4 | 5.1 / 101.4 us | 2.0 / 9.5 us | 19.094 us |
| 8 | 7.0 / 183.6 us | 3.2 / 11.7 us | 18.281 us |
| 16 | 9.1 / 327.1 us | 6.4 / 16.1 us | 18.250 us |
| 32 | 27.5 / 677.2 us | 12.9 / 38.6 us | 20.812 us |

Tokio handoff is measurable and its tail grows, but its median is much smaller
than the full filesystem pipeline. The control observed 3, 50, 76, 228, and
325 distinct Tokio blocking thread IDs over the C1-C32 trials. These are
distinct threads used over time, not simultaneous active-thread counts; the
long-lived runtime and Tokio's ten-second idle retention amplify this number.

### Same File vs Multiple Files

Tokio full-sequence throughput, changing only candidate selection:

| Concurrency | Same file | 4 files, one dir | 16 files, one dir | 16 split dirs |
|---:|---:|---:|---:|---:|
| 1 | 5,108 | 4,729 | 4,539 | 4,241 |
| 4 | 9,234 | 14,882 | 12,639 | 10,180 |
| 8 | 7,554 | 20,128 | 17,445 | 13,539 |
| 16 repeat | 6,917 | 23,841 | 34,008 | 36,095 |
| 32 repeat | 6,724 | 19,844 | 20,864 | 21,816 |

Spreading requests across files produced a large, repeatable advantage over
one hot path: at C16 the repeat was 3.45x for four files and 4.92x for sixteen.
Directory placement is inconclusive: split directories were slower at C4/C8
but slightly faster in the C16 repeat. At C16 the within-batch RPS ranges were
9.7%, 1.3%, 13.4%, and 6.1%; an earlier C16 batch also differed materially.
This order/batch sensitivity prevents a stronger cache-manager or directory
lookup claim, while the same-file contention conclusion remains robust. The
C32 16-file same-directory RPS range was 28.0%, so its exact advantage should
not be over-interpreted.

### Filesystem Operation Scaling

Median execution inside the Tokio blocking closure, in microseconds:

| Operation | C1 | C4 | C8 | C16 | C32 | C32/C1 |
|---|---:|---:|---:|---:|---:|---:|
| Metadata only | 15.0 | 69.6 | 183.8 | 389.8 | 802.9 | 53.5x |
| Canonicalize only | 38.1 | 125.4 | 331.7 | 676.0 | 1,508.6 | 39.6x |
| Metadata + canonicalize | 58.2 | 193.4 | 505.3 | 1,035.6 | 2,249.4 | 38.6x |
| Read only | 35.8 | 131.4 | 337.5 | 759.4 | 1,380.8 | 38.6x |

Corresponding queue P50 stayed near 5-8 us for every single-operation case
(13 us for the two-operation case). Thus the large execution increase happens
after entry into the blocking closure. Metadata has the largest relative
slowdown; canonicalize and read have the highest absolute C32 cost. C32 RPS
ranges in the repeat were 1.8-14.1%; canonicalize P99 was especially noisy, so
P50 and the common cross-operation trend carry more weight than one tail run.

### CPU and Scheduling

| Concurrency | Tokio effective cores | Tokio CPU/request | Dedicated-2 cores | Dedicated-2 CPU/request |
|---:|---:|---:|---:|---:|
| 1 | 1.095 | 210.938 us | 1.131 | 235.938 us |
| 4 | 2.763 | 346.875 us | 2.131 | 192.188 us |
| 8 | 2.718 | 379.688 us | 2.253 | 200.000 us |
| 16 | 2.573 | 356.250 us | 2.191 | 200.000 us |
| 32 | 2.871 | 409.375 us | 2.104 | 179.688 us |

Neither path saturates the 12 logical processors. Tokio's no-op control proves
handoff/completion overhead and thread churn exist, especially in the tail.
However, the full-path gap is dominated by reduced time inside filesystem
calls, not the roughly 3-15 us P50 no-op handoff difference. Application-level
timestamps cannot supply context-switch counts, ready time, wait reasons, or
core migration; those require ETW.

### Windows ETW/WPA Status

WPR 10.0.26100 and WPA are installed on the investigation machine, and WPR
exposes GeneralProfile, CPU, DiskIO, and FileIO profiles. No ETL stack analysis
was treated as evidence in this run. `Doc/WINDOWS_STATIC_PROFILING.md` gives
exact C4/C8 capture commands and WPA views for CPU sampling, precise scheduling,
File I/O, Disk I/O, ready/wait time, context switches, and thread migration.
ETW benchmark numbers must remain separate because tracing adds overhead.

### Open-Once Feasibility and Security

Rust `std::fs::File` can read and query metadata from one handle, but the Rust
standard library does not expose the final path resolved by an opened Windows
handle. A credible design therefore needs Windows handle APIs such as
`GetFinalPathNameByHandleW`, explicit reparse-point semantics, and tests against
symlink/junction swaps. It could reduce operations and narrow the current
canonicalize-to-open TOCTOU window, but it is not yet proven security-equivalent.
No workaround or open-once PoC was implemented.

All new code is behind the `profiling` feature. Production path normalization,
encoded separator/backslash/NUL/colon/ADS rejection, target canonicalization,
root containment, symlink/junction protection, and Tokio runtime configuration
were not changed.

### Root Cause Update and Recommendation

**Conclusion B: Windows filesystem contention is the primary bottleneck.**
Tokio handoff is a measurable secondary cost, not the dominant one. The best
next experiment is an ETW/WPA C4-versus-C8 capture followed by a design review
of handle-based final-path validation. Do not implement production open-once,
cache, runtime thread tuning, or router work until that evidence exists.

## 0.0.3 Routing Baseline

This section is additive. Historical 0.0.1 and 0.0.2 values and all static
performance-investigation conclusions above remain unchanged. In particular,
Windows filesystem contention remains the primary static bottleneck,
`spawn_blocking` overhead remains secondary, and static throughput still peaks
at low concurrency. Milestone 0.0.3 did not optimize the static implementation.

### Architecture and Methodology

The immutable startup-built path is `HostRouter -> LocationRouter -> Handler`.
Two exact host aliases (`localhost` and `127.0.0.1`) share one location table.
`/api` selects a simple 404 placeholder and `/` selects the unchanged static
handler. No dependency was added.

A-D used a profiling-only MSVC release microbenchmark: 2,000,000 operations per
trial and three trials, reporting per-field medians and max-min wall-time range.
Headers and routing tables were created outside the timed loop. CPU time is
kernel plus user process time from `GetProcessTimes`; its 15.625 ms observed
resolution corresponds to 7.8125 ns/operation at this sample size.

E used the normal production binary, loopback HTTP/1.1, keep-alive, the same
22-byte `public/index.html`, and the .NET client. C4 used 1,000 warm-up plus
100,000 measured requests to compare with the concurrency investigation. C32
used 1,000 warm-up plus 500,000 measured requests to match the historical
static methodology. Both used three fresh server runs and per-field medians.

### Routing Layer Cost

| Case | Measured work | Wall median | Process CPU median | Wall range |
|---|---|---:|---:|---:|
| A | Constant response construction | 105.099 ns | 109.375 ns | 5.6% |
| B | HostRouter only | 38.485 ns | 39.062 ns | 18.0% |
| C | HostRouter + LocationRouter | 42.579 ns | 39.062 ns | 8.7% |
| D | Router + simple handler response | 152.149 ns | 148.438 ns | 1.4% |

B and C are standalone lookup totals, not values added to A. Location lookup
adds about 4.094 ns over host lookup. Comparing D with A, routing adds about
47.050 ns wall and 39.063 ns process CPU per request. The CPU clock cannot
resolve the smaller B-to-C delta, so equal CPU medians do not mean zero cost.

### Router + Static Handler

| C | Requests | RPS | P50 | P95 | P99 | CPU time | Effective cores | CPU/request | WS/Peak | RPS/P99 range |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 4 | 100,000 | 7,337 | 0.536 ms | 0.689 ms | 0.791 ms | 36.812 s | 2.712 | 368.125 us | 6.8/6.8 MiB | 0.6%/1.4% |
| 32 | 500,000 | 6,291 | 4.895 ms | 5.918 ms | 7.199 ms | 230.672 s | 2.902 | 461.344 us | 9.2/9.5 MiB | 10.2%/18.2% |

At C4, versus the 0.0.2 concurrency matrix, RPS changed -1.7%, P50 +2.5%,
P99 -6.4%, and CPU/request +3.0%. At C32, versus the 500,000-request static
performance-investigation median, RPS changed -6.7%, P50 +5.7%, P99 +21.5%,
and CPU/request +5.2%. The C32 batch is noisy and prior investigations already
showed material between-batch filesystem variance. Relative to the immutable
historical 0.0.2 single run, current C32 RPS is +5.5%, P50 -8.1%, P95 +0.2%,
and P99 +7.4%.

The micro-profiled ~47 ns routing cost is far too small to explain multi-ms
static latency or the C32 variance. There is no evidence that routing displaced
the established filesystem bottleneck, but the observed C32 tail regression is
recorded rather than hidden.

### Routing Semantics and Production Impact

- Exactly one Host header is required. DNS hostname matching is exact and
  ASCII case-insensitive; an optional valid `u16` port is ignored for lookup.
- Missing, duplicate, malformed, or out-of-range-port Host values return 400.
  Unknown configured hosts return 404. Wildcards are not supported.
- Locations use longest path-segment prefix: `/api` matches `/api/users` but
  not `/apix`. Prefixes are normalized, deduplicated, and sorted once.
- Runtime data is immutable behind `Arc`; lookup performs no config parsing,
  table rebuilding, hostname allocation, or global mutex acquisition.
- Static path normalization, target canonicalization, root containment, and
  traversal/symlink/junction protections were not changed.

The design is documented in `Doc/ROUTING.md`. Router micro-instrumentation is
compiled only with the `profiling` feature and is absent from normal builds.

### Recommendation

Keep the vector-based router and current handler enum for this milestone. Its
measured overhead is not significant enough to justify `matchit`, hashing, or a
custom tree. Do not begin 0.0.4 automatically; the next milestone should first
define its configuration semantics and an independent benchmark plan.

## 0.0.4 Configuration Baseline

This section is additive. It does not alter the immutable 0.0.1 HTTP baseline,
the 0.0.2 correctness baseline, any static investigation, or the 0.0.3 routing
results. Windows filesystem contention remains the main static bottleneck,
Tokio handoff remains secondary, and no production static code was optimized.

### Methodology

The profiling-only MSVC release benchmark separates in-memory TOML parsing,
semantic validation/normalization, router compilation, and total file loading.
The total uses the real `load_file` path, including config-path canonicalization
and file read. Each field is the median of three trials; total range is max-min
divided by the median. Synthetic configurations have one alias per host, four
locations per host, and exactly one static root. The static root is canonicalized
during every compile trial so startup filesystem validation remains represented.
The reproducible command is
`cargo bench --target x86_64-pc-windows-msvc --bench configuration --features profiling`.

The hot-path comparison performs 2,000,000 router-plus-simple-handler operations
per trial over three trials. Case A constructs the 0.0.3 router directly. Case B
loads the same host/location/handler graph through `RawConfig -> RuntimeConfig`
before timing. Headers, TOML, validation, path resolution, and router construction
are all outside the request loop.

### Startup Compile Timings

| Size | Hosts / locations | Parse median | Validation median | Router compile median | Total median | Total range |
|---|---:|---:|---:|---:|---:|---:|
| Small | 2 / 8 | 17.0 us | 6.0 us | 48.1 us | 144.3 us | 3,581.6% |
| Medium | 50 / 200 | 345.9 us | 70.7 us | 114.7 us | 615.1 us | 799.3% |
| Large synthetic | 500 / 2,000 | 3,640.2 us | 756.8 us | 1,039.4 us | 5,958.3 us | 11.6% |

Total samples were 121.8/144.3/5,290.1 us, 587.8/615.1/5,504.3 us, and
5,576.2/5,958.3/6,265.4 us respectively. Stages are timed with independently
prepared inputs, so their medians are diagnostic components and are not expected
to sum exactly to the independently measured total. Small and medium each had a
single roughly 5 ms file-load outlier; three samples are too few to attribute it
to cache, scanning, or background activity. The median is reported rather than
discarding that run. Even the 500-host synthetic median remains about 6 ms and
needs no optimization at this milestone.

### Routing Hot-Path Comparison

| Case | Wall samples | Wall median | CPU samples | CPU median | Wall range |
|---|---:|---:|---:|---:|---:|
| A. 0.0.3 Router + Simple Handler | 154.798 / 155.534 / 157.912 ns | 155.534 ns | 132.812 / 148.438 / 156.250 ns | 148.438 ns | 2.0% |
| B. RuntimeConfig compiled router + Simple Handler | 148.222 / 154.240 / 156.478 ns | 154.240 ns | 140.625 / 148.438 / 156.250 ns | 148.438 ns | 5.4% |

The -1.294 ns wall difference is inside the observed variance and CPU medians
are equal. Windows process CPU time advances in approximately 15.625 ms
steps here, equal to 7.8125 ns per operation at this iteration count. The result
therefore shows no measurable configuration-induced request-path regression and
does not claim that the compiled path is faster. Production extracts the same
immutable `Arc<HostRouter>` once before entering the accept loop.

### Memory and Scope

No reliable isolated working-set delta was recorded: allocator and process
startup noise would dominate these short in-process trials. Runtime storage is
linear in configured host names, aliases, and locations. TOML input and raw and
validated representations are dropped after compilation; only the immutable
socket address and compiled router graph remain reachable from
`Arc<RuntimeConfig>`.

No `ArcSwap`, reload/watch path, wildcard/regex routing, config framework, or
static optimization was added. Re-run the startup benchmark only if real-world
configuration sizes or a future reload requirement make compile latency
material; retain the current vector routers and handler enum meanwhile.

## 0.1.0 PHP Worker Prototype

This is an architectural baseline for one serialized persistent PHP process,
not a production-throughput claim. Historical HTTP, static, investigation,
routing, and configuration results above are unchanged. The run used the
locally supplied PHP 8.5.10 CLI NTS (`php/8.5/php.exe`, VS 17/VC++ 2022 x64;
excluded from Git), the
MSVC release server, loopback HTTP/1.1 keep-alive, one client connection, 100
warm-up requests, and 1,000 measured requests. Each result is the median of
three runs; ranges include every run. Reproduce with:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/php_worker.ps1 -Runs 3 -Requests 1000 -WarmupRequests 100
```

### Cold and Warm Behavior

| Scenario | READY/bootstrap median (range) | First request median (range) | Warm P50 median (range) |
|---|---:|---:|---:|
| PHP Hello World | 33.758 ms (32.777-39.740) | 11.646 ms (6.235-15.693) | 15.191 ms (15.153-15.194) |
| PHP small JSON | 31.119 ms (31.004-33.602) | 9.931 ms (8.229-12.137) | 15.171 ms (15.159-15.198) |

READY/bootstrap is measured inside Hamster Wheel from process spawn through the
validated READY frame. The first-request measurement also includes creation of
the benchmark client's first HTTP connection and is therefore not pure PHP
execution time. The separate listener probe was approximately 506-518 ms but is
dominated by the PowerShell synchronous connection probe and is not used as the
worker bootstrap metric.

### Warm Request Baseline

| Scenario | RPS | P50 | P95 | P99 | Wall/run | RPS range | P99 range |
|---|---:|---:|---:|---:|---:|---:|---:|
| Simple not-found handler | 9,371 | 0.102 ms | 0.169 ms | 0.231 ms | 0.107 s | 8,721-10,042 | 0.201-0.239 ms |
| PHP Hello World | 66 | 15.191 ms | 15.756 ms | 16.076 ms | 15.227 s | 65.65-65.77 | 16.036-16.163 ms |
| PHP small JSON | 66 | 15.171 ms | 15.765 ms | 16.077 ms | 15.215 s | 65.69-65.74 | 16.069-16.087 ms |

The simple result is a control for this sequential .NET client, not a
replacement for the immutable 0.0.1 baseline. Hello and JSON are effectively
identical. With one mutex-serialized worker, the approximately 15 ms warm
latency bounds throughput near 66 RPS. Process CPU is much smaller than wall
time, so the prototype is predominantly waiting on the synchronous request/
response IPC path rather than saturating CPU. This observation does not isolate
which Windows pipe wakeup or scheduling component owns that wait.

### CPU and Memory

| Scenario | Hamster CPU/request | PHP CPU/request | Combined CPU/request | Hamster WS/peak | PHP WS/peak |
|---|---:|---:|---:|---:|---:|
| Simple not-found handler | 46.875 us | 0 | 46.875 us | 6.4/6.4 MiB | n/a |
| PHP Hello World | 203.125 us | 78.125 us | 375.000 us | 6.9/6.9 MiB | 16.6/16.6 MiB |
| PHP small JSON | 406.250 us | 187.500 us | 593.750 us | 6.9/6.9 MiB | 16.7/16.7 MiB |

CPU/request uses each process's `TotalProcessorTime` delta divided by 1,000
completed requests. Windows CPU accounting advances in coarse increments at
this short sample size, so per-process and combined medians are computed
independently and may not add exactly. The three-run combined ranges were
250.000-500.000 us for Hello and 484.375-703.125 us for JSON. Working set and
peak are process-specific snapshots after the measured run.

### Acceptance and Scope

The PHP 8.5 NTS fixture passed 100 sequential HTTP requests with one PID,
`bootstrap = 1`, and request counters 1 through 100. GET/POST, URI/query,
repeated headers, binary and 256 KiB bodies, custom status/header, keep-alive,
serialized concurrent requests, startup failures, malformed/truncated frames,
timeouts, worker exit, and ready/used shutdown paths were verified. The fixture
linted with `php -l`.

The protocol buffers whole messages, limits request bodies to 1 MiB, and has no
streaming, state reset, health probe, respawn, or pool. Persistent state can leak
between requests. No FastCGI, PHP-FPM, Laravel, shell command, automatic PATH
lookup, static optimization, router replacement, or production pooling was
introduced. A future milestone should first profile the Windows pipe round-trip
and define state-reset semantics before choosing a worker-pool design.

## 0.1.1 Windows PHP Worker IPC Investigation

This additive investigation does not replace the immutable 0.1.0 baseline and
does not modify production architecture or protocol. It used the same local PHP
8.5.10 CLI NTS, an optimized MSVC profiling bench, concurrency one, and three
runs per reported field. The direct child-process suites exclude HTTP, Hyper,
router, application bootstrap, and the production worker mutex. Parent and
child CPU values use separate `GetProcessTimes` snapshots over the same window.

### End-to-End Timeline

The default timeline uses a real production-format request/response. Rust and
PHP clocks provide durations only within their own process; absolute timestamps
are never subtracted across clock domains.

| Stage | Default pipe P50 | Profiling `pipe.blocking` P50 |
|---|---:|---:|
| Rust request encode (T0-T1) | 2.5 us | 0.8 us |
| Rust write + flush completion (T1-T2) | 57.0 us | 40.6 us |
| Combined transfer/wakeup residual | 14,976.0 us | 16.6 us |
| PHP decode (T3-T4) | 23.9 us | 5.4 us |
| PHP fixture handler (T4-T5) | 1.3 us | 0.2 us |
| PHP response encode (T5-T6) | 10.4 us | 2.1 us |
| PHP write + flush (T6-T7) | 17.9 us | 1.7 us |
| Rust decode + validation (T8-T9) | 2.8 us | 0.4 us |
| Total transaction P50 | 15,136.8 us | 67.9 us |

The residual is `Rust T2-T8 - PHP T3-T7`. It combines both transfer directions,
pipe polling/wakeup, and task resumption; the protocol has no synchronized
one-way marker with which to split Rust-to-PHP from PHP-to-Rust safely. PHP
diagnostics travel on stderr after stdout completion and never enter protocol
stdout.

### Pipe Round-Trip Payload Matrix

Minimal child loops read one length-prefixed frame and echo it without HTTP
metadata or application logic. The table reports three-run medians using the
current Rust split-prefix/payload write shape.

| Payload | Rust child P50 | Rust RPS | PHP child P50 | PHP RPS |
|---:|---:|---:|---:|---:|
| 0 B | 39.4 us | 24,969 | 15,128.3 us | 69.6 |
| 16 B | 58.8 us | 16,524 | 15,159.1 us | 66.1 |
| 128 B | 72.6 us | 13,433 | 15,147.2 us | 67.3 |
| 1 KiB | 49.2 us | 18,306 | 15,123.5 us | 65.7 |
| 16 KiB | 71.2 us | 13,806 | 15,147.8 us | 65.9 |
| 64 KiB | 113.7 us | 7,729 | 15,166.0 us | 65.5 |
| 1 MiB | 1,160.3 us | 764 | 15,446.6 us | 64.0 |

PHP latency is almost payload-independent through 64 KiB, then rises only
about 0.3 ms at 1 MiB. That shape is a fixed wait, not serialization or memory
bandwidth cost. PHP `fread` counters were two reads/request at 16-1,024 B,
three at 16 KiB, nine at 64 KiB, and 129 at 1 MiB, consistent with an
approximately 8 KiB read chunk after the four-byte prefix.

### Rust-Rust, Synchronous, and Tokio Controls

At 128 B, a completely synchronous parent using ordinary process pipe handles
recorded Rust-child P50 18.9 us, P99 51.7 us, and 46,709 RPS. The same parent
and framing recorded PHP-child P50 15,090.5 us, P99 16,029.8 us, and 75.3 RPS.
Thus PHP retains the delay without Tokio, Hyper, or async scheduling.

The Tokio parent/Rust child control recorded 72.6 us P50. Tokio child stdio is
therefore a measurable tens-of-microseconds cost on this host, but it is more
than two orders of magnitude below the PHP default and is not the primary
bottleneck.

### Frame, Flush, and Buffering Experiments

All values below use a 128-byte PHP echo at concurrency one:

| Experiment | P50 |
|---|---:|
| Current: Rust split, PHP contiguous + flush | 15,142.4 us |
| Rust single contiguous request | 15,204.2 us |
| Rust `BufWriter` around split request | 15,314.0 us |
| PHP split response (two `fwrite`) | 15,198.9 us |
| PHP read buffer set to zero | 15,175.3 us |
| PHP response without `fflush` | 15,187.1 us |
| PHP requested 8 KiB write buffer + one flush | 15,179.7 us |
| `stream_set_blocking(stdin, true)` | 15,179.2 us |
| `pipe.blocking` stream context (profiling only) | 68.7 us |

For 920 small requests, the default PHP loop performed 1,841 `fread`, 920
`fwrite`, and 920 `fflush` calls. Splitting the response doubled `fwrite` to
1,840 without a meaningful latency change. `ob_get_level()` was zero;
`stream_set_write_buffer` returned `-1` (unsupported), read-buffer zero returned
`0`, and `stream_set_blocking` returned `false`. Omitting flush still delivered
responses and did not remove the delay.

### First, Warm, and Timer Distribution

| Window | Default P50/P95/P99 | Default min/max | Near 15.625 ms quantum | Blocking P50/P95/P99 |
|---|---:|---:|---:|---:|
| First request | 14,240/14,240/14,240 us | 14,240/14,240 us | 0% | 86.5/86.5/86.5 us |
| Requests 2-10 | 15,418/15,519/15,519 us | 14,755/15,539 us | 100% | 76.8/109.9/109.9 us |
| Requests 100-999 | 15,112/15,715/16,076 us | 119.6/16,442 us | 94.8% | 67.7/95.2/133.6 us |
| Requests 1000+ | 15,283/15,770/16,051 us | 156.3/16,119 us | 95.5% | 68.9/102.8/147.2 us |

The first value is the median of three separately identified first requests.
Default steady-state samples overwhelmingly cluster within 1 ms of a 15.625 ms
multiple, with occasional sub-millisecond samples when PHP reaches its read at
the favorable time. The blocking-context distribution has no quantum cluster.

### CPU versus Wall Time

For the production-format timeline, default median CPU/request was 234.375 us
in Rust and 117.188 us in PHP (351.562 us combined) versus 15,136.8 us wall
P50. The profiling blocking context used 91.146 us Rust and 26.042 us PHP
(117.188 us combined) versus 67.9 us wall P50; CPU can exceed wall because the
two processes execute on different cores and per-trial CPU accounting is
coarsely quantized. The default wall/combined-CPU gap confirms waiting rather
than CPU saturation.

### Root Cause and Scope

**Conclusion A: PHP buffering/Windows pipe polling is the primary cause.** PHP
8.5's Windows stdio stream path treats `php://stdin` as a non-blocking anonymous
pipe, polls availability with `PeekNamedPipe`, and calls `usleep(10)` when empty.
On this host that wait aligns with the approximately 15.625 ms timer quantum.
Opening the profiling stream with context option
`['pipe' => ['blocking' => true]]` bypassed the poll and reduced the same
production-format P50 by roughly 223x. A generic Windows pipe, protocol/frame
handoff, and Tokio child stdio are secondary or rejected as main causes by the
Rust and synchronous controls.

No timer resolution API, production `pipe.blocking`, socket/named-pipe
transport, worker pool, protocol change, state reset, respawn, FastCGI, or
Laravel work was added. The production worker and all 0.1.0 security/validation
semantics remain unchanged.

### Measurement Limits and Recommendation

Timeline stderr diagnostics add work after each response. Tiny-trial process
CPU has 15.625 ms accounting granularity, and medians of independent fields do
not necessarily add exactly. The PHP internal timer and Rust `Instant` are used
only for within-process durations. `read_exact`/`write_all` are API-call counts,
not guaranteed one-to-one kernel syscall counts.

The next step should be a single-variable production candidate review of
opening PHP stdin with the supported blocking-pipe stream context, including
request timeout, Ctrl+C, malformed/truncated frame, dead-worker, and shutdown
tests. Re-run both 0.1.0 HTTP and IPC baselines before accepting it. Do not add a
pool to mask the default polling delay.

## 0.1.2 Production Blocking Pipe

This section is additive. It preserves the 0.1.0 approximately 66 RPS
prototype baseline and every 0.1.1 investigation result above. The only
production behavior change is opening Windows `php://stdin` with
`['pipe' => ['blocking' => true]]` before the worker sends `READY`; the protocol,
Rust child stdio, router, configuration, and single-worker mutex are unchanged.

### Methodology

- PHP 8.5.10 CLI NTS VS17 x64 and MSVC release server on the same Windows host
- Loopback HTTP/1.1 keep-alive, concurrency one, 100 warm-up requests
- 20,000 measured requests per run, three fresh processes, per-field medians
- Process CPU deltas cover the measured requests only; memory is sampled after
  each run
- A latency sample is quantum-adjacent when it is within 1 ms of a positive
  15.625 ms multiple

Reproduce with:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/php_worker.ps1 -Runs 3 -Requests 20000 -WarmupRequests 100
```

### Production Before and After

| Scenario | RPS before / after | P50 before / after | P95 before / after | P99 before / after |
|---|---:|---:|---:|---:|
| Simple control | 9,371 / 19,774 | 0.102 / 0.046 ms | 0.169 / 0.076 ms | 0.231 / 0.128 ms |
| PHP Hello | 66 / 7,004 | 15.191 / 0.138 ms | 15.756 / 0.184 ms | 16.076 / 0.252 ms |
| PHP JSON | 66 / 7,204 | 15.171 / 0.132 ms | 15.765 / 0.188 ms | 16.077 / 0.260 ms |

Hello throughput improved about 106x and P50 fell about 110x; JSON improved
about 109x and 115x respectively. The simple control varies strongly at this
short sequential workload and is not a replacement for 0.0.1. The production
HTTP medians are above the 0.1.1 direct-pipe 67.9 us because they include the
client, TCP, Hyper, router, mutex, request mapping, and response construction.

### Cold, Warm, Distribution, and Variance

| Scenario | READY median | First request median | Min | P50 | P95 | P99 | Max | Quantum-adjacent |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| PHP Hello | 27.811 ms | 1.167 ms | 0.095 ms | 0.138 ms | 0.184 ms | 0.252 ms | 2.323 ms | 0.0% |
| PHP JSON | 26.312 ms | 5.637 ms | 0.085 ms | 0.132 ms | 0.188 ms | 0.260 ms | 2.658 ms | 0.0% |

Hello RPS ranged 6,788-7,241, P50 0.134-0.140 ms, and P99
0.243-0.270 ms. JSON RPS ranged 6,374-7,425, P50 0.129-0.144 ms, and P99
0.258-0.308 ms. Reported max ranges were 0.782-2.566 ms and 1.425-4.260 ms.
No measured request in any production run was quantum-adjacent, so the default
15.625/31.25 ms modes have disappeared. First request includes client
connection setup and is intentionally reported separately from READY and warm
latency. READY remains near the 0.1.0 31-34 ms range rather than being folded
into request throughput.

### CPU and Memory

| Scenario | Hamster CPU/request | PHP CPU/request | Combined | Hamster WS/peak | PHP WS/peak |
|---|---:|---:|---:|---:|---:|
| Simple | 25.781 us | 0 | 25.781 us | 6.5/6.5 MiB | n/a |
| PHP Hello | 146.094 us | 26.562 us | 170.312 us | 7.0/7.0 MiB | 16.6/16.6 MiB |
| PHP JSON | 144.531 us | 29.688 us | 178.125 us | 7.0/7.0 MiB | 16.6/16.6 MiB |

Hello wall time averages about 143 us/request and JSON about 139 us/request.
Combined process CPU can exceed wall because Rust and PHP may execute on
different cores and Windows process accounting remains quantized. The result
primarily proves removal of waiting latency; it is not presented as a CPU
optimization. Memory stays near the 0.1.0 6.9 MiB Hamster and 16.6-16.7 MiB PHP
measurements.

### Soak and Concurrency Smoke

The production soak command
`./benchmarks/php_worker_soak.ps1 -Requests 100000` completed 100,000/100,000
requests in 14.713 s (6,797 RPS). PID stayed constant, bootstrap remained one,
and the request counter advanced exactly 1 to 100,000. Hamster working set was
6.7 MiB before and 7.2 MiB after; PHP was 16.5 and 16.7 MiB. There was no
timeout, crash, deadlock, or protocol corruption.

The correctness-only concurrency smoke used 100 warm-up plus 2,000 measured
Hello requests at each level against the same one-worker process:

| HTTP concurrency | Successes | RPS | P50 | P95 | P99 | Max |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2,000 | 4,808 | 0.194 ms | 0.284 ms | 0.380 ms | 2.747 ms |
| 2 | 2,000 | 9,241 | 0.203 ms | 0.300 ms | 0.393 ms | 2.847 ms |
| 4 | 2,000 | 8,047 | 0.474 ms | 0.643 ms | 0.758 ms | 5.775 ms |
| 8 | 2,000 | 8,233 | 0.955 ms | 1.209 ms | 1.332 ms | 1.839 ms |
| 16 | 2,000 | 8,957 | 1.721 ms | 2.267 ms | 2.511 ms | 2.731 ms |

All 10,000 measured requests succeeded; the same PID retained `bootstrap=1`
and the final counter matched 10,502 including warm-up and state probes.
Latency grows as requests queue before one serialized worker. These figures are
not a scalability benchmark and do not hide the single-worker constraint.

### Lifecycle, Correctness, and Decision

Tests cover READY-blocked shutdown, shutdown after use, BUSY drain, request
timeout, EOF, malformed and truncated response, child exit after reading only
the request prefix, unavailable-state behavior, binary payloads, repeated
headers, GET/POST/query, custom response status/headers, keep-alive, and
concurrent HTTP connections. Manual Ctrl+C checks in READY and BUSY states
left no orphan PHP process; the BUSY console case returned a controlled 502
after PHP received the console interrupt.

PHP cannot query the internal `pipe.blocking` option after `fopen`: context
options are not reflected by `stream_context_get_options`, while generic stream
metadata reports `blocked=true` for both paths. Production therefore fails
before `READY` if context creation, stream open, or STDIO validation fails, and
the zero-quantum production distribution supplies behavioral verification.
There is no silent polling fallback.

The measured bottleneck is fixed without a protocol or architecture redesign.
The evidence supports proceeding to a separate 0.2.0 worker-pool design; this
milestone does not implement it.
