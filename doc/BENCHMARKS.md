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

## 0.1.2 Review/Maintenance Correction

This section records a corrective-maintenance pass over the 0.1.2 test and
benchmark tooling. It is **not** a new benchmark baseline: no measurement in any
earlier section was rerun, reinterpreted, or replaced, and no production code
path, protocol, timeout, or methodology changed. Benchmark numbers published
above remain the record for their milestones.

### Corrections

- `SERVER_NAME` now derives from `CARGO_PKG_VERSION` instead of repeating the
  version as a literal, and the static-file test asserts against that constant.
  The 0.1.2 version bump had left `tests/static_files.rs` asserting
  `HamsterWheel/0.1.0`, so `cargo test` failed on the branch.
- `benches/php_ipc.rs` drains child stderr concurrently with the child's
  lifetime in the async peers, the synchronous suite, and the timeline peer.
  Reading stderr only after `wait()` deadlocks once a child fills the pipe
  buffer. A child that does not exit within the pipe timeout is now killed and
  reaped, and its diagnostics are still reported.
- The Windows `GetProcessTimes` CPU accounting moved behind `cfg(windows)`. Other
  platforms report `cpu_metric=unavailable` and print `unavailable` for each CPU
  field rather than a zero that would read as measured idle CPU. No dependency
  was added to obtain a portable metric.
- The suite argument is read from the first positional argument, because Cargo
  appends its own `--bench` flag; running the bench with no suite now selects
  `all` as documented.
- Timeline sample windows derive from named constants with an explicit minimum
  sample check, replacing hard-coded `99`/`999` slices that depended silently on
  `iterations = 1200`. The window boundaries themselves are unchanged.

### Verification

`cargo fmt --all -- --check`, `cargo clippy --target x86_64-pc-windows-msvc
--all-targets --all-features -- -D warnings`, `cargo test --target
x86_64-pc-windows-msvc` (42 passed), `cargo test -- --ignored` (11 PHP worker
tests passed), `cargo build --release --target x86_64-pc-windows-msvc`, and
`cargo bench --no-run --features profiling` (links all six benches) all pass.

The new `stderr` suite is a deterministic regression rather than a timing
measurement: the child writes 512 KiB to stderr — far beyond the 64 KiB Windows
pipe buffer — before echoing, then completes ten round trips.

```text
IPC_STDERR,peer=rust,mode=stderr_flood,flood_bytes=524288,received_bytes=524288,round_trips=10,status=drained
```

A production spot check on the release binary returned `200` for static and PHP
routes, `404` for `/api` and unknown hosts, `403` for traversal, and `413` above
the 1 MiB body limit. Across 60 sequential `/php` requests no sample fell in the
14.6-16.6 ms band, so the blocking-pipe path is intact; these latencies include
per-request client process startup and are not comparable to the load-driver
results above. Shutdown left no orphan `php.exe`.

### Platform Status and Acceptance

Windows `x86_64-pc-windows-msvc` is VERIFIED for 0.1.2. Cross-target
`cargo check` and `cargo clippy` for `x86_64-unknown-linux-gnu` pass from the
Windows host, so the non-Windows `cfg` paths type-check, but no native Linux
link, test, or benchmark run exists. Linux is therefore COMPILE-CHECKED ONLY
with native verification deferred to a future CI job or Linux host.

0.1.2 is accepted for the verified Windows target. Linux native verification
remains a deferred platform qualification task and is not treated as an open
defect in this milestone.

A one-run production spot check after the corrections reported
`quantum_pct=0.0` for the simple, PHP Hello, and PHP JSON scenarios at 3,000
requests each. It confirms the blocking-pipe path is intact and is explicitly
not a replacement for the 0.1.2 baselines recorded above.

## 0.2.0 Fixed PHP Worker Pool

### NON-COMPARABLE / INVESTIGATION DATA (superseded methodology)

This section is additive and does not replace the immutable 0.1.2 results.
Measurements used PHP 8.5.10 NTS, the MSVC release build, loopback HTTP/1.1,
1,000 warm-up requests, 10,000 measured requests, and three fresh server/PHP
process sets per cell. The pool config used `queue_capacity = 256`. Values are
field medians. Individual measured runs were often only about one second, which
is too short for stable Windows scheduling and process-CPU observations. These
tables are retained as investigation data, not an accepted 0.2.0 baseline.
The command below records the historical invocation; the driver has since been
corrected to duration-controlled runs and `queue_capacity = 128`, so it no
longer reproduces these exact cells.

```powershell
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 10000 -WarmupRequests 1000
```

`SCPU`, `PCPU`, and `CPU` are Hamster, total PHP, and combined microseconds per
request. `SRAM` and `PRAM` are Hamster and total PHP working set in MiB.
Queue wait was not instrumented because the current driver cannot isolate it
without adding hot-path timing solely for the benchmark.

### PHP Hello

| W | C | RPS | P50 | P95 | P99 | SCPU | PCPU | CPU | SRAM | PRAM |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,755 | .308 | .633 | .762 | 301.6 | 57.8 | 359.4 | 6.6 | 16.0 |
| 1 | 2 | 3,750 | .518 | .885 | 1.083 | 404.7 | 67.2 | 473.4 | 6.7 | 16.0 |
| 1 | 4 | 8,304 | .448 | .608 | 1.364 | 228.1 | 42.2 | 265.6 | 6.8 | 16.0 |
| 1 | 8 | 8,541 | .901 | 1.076 | 2.259 | 215.6 | 29.7 | 242.2 | 6.9 | 16.0 |
| 1 | 16 | 8,733 | 1.794 | 2.030 | 2.953 | 209.4 | 37.5 | 246.9 | 7.0 | 16.0 |
| 1 | 32 | 8,603 | 3.616 | 4.045 | 5.223 | 204.7 | 26.6 | 223.4 | 7.3 | 16.0 |
| 2 | 1 | 4,427 | .202 | .384 | .587 | 217.2 | 45.3 | 253.1 | 6.7 | 32.0 |
| 2 | 2 | 8,797 | .211 | .367 | .531 | 182.8 | 35.9 | 218.8 | 6.9 | 32.0 |
| 2 | 4 | 14,534 | .251 | .427 | .814 | 192.2 | 40.6 | 232.8 | 7.2 | 32.0 |
| 2 | 8 | 15,598 | .487 | .664 | 1.253 | 200.0 | 42.2 | 243.8 | 7.4 | 32.0 |
| 2 | 16 | 15,876 | .942 | 1.399 | 2.687 | 195.3 | 43.8 | 245.3 | 7.7 | 32.0 |
| 2 | 32 | 15,521 | 1.893 | 2.731 | 6.407 | 214.1 | 37.5 | 253.1 | 8.3 | 32.0 |
| 4 | 1 | 2,133 | .461 | .694 | .822 | 429.7 | 73.4 | 506.3 | 6.9 | 63.9 |
| 4 | 2 | 10,763 | .181 | .234 | .276 | 170.3 | 37.5 | 207.8 | 7.1 | 63.9 |
| 4 | 4 | 6,598 | .578 | .858 | 1.061 | 523.4 | 100.0 | 623.4 | 7.4 | 63.9 |
| 4 | 8 | 8,961 | .845 | 1.391 | 1.767 | 604.7 | 90.6 | 681.3 | 7.8 | 63.9 |
| 4 | 16 | 9,652 | 1.623 | 2.299 | 2.785 | 557.8 | 71.9 | 642.2 | 8.1 | 64.0 |
| 4 | 32 | 9,079 | 3.443 | 4.580 | 5.627 | 595.3 | 68.8 | 657.8 | 9.0 | 63.9 |
| 8 | 1 | 3,889 | .195 | .558 | .822 | 251.6 | 48.4 | 285.9 | 7.3 | 127.8 |
| 8 | 2 | 2,496 | .822 | 1.278 | 1.651 | 584.4 | 112.5 | 681.3 | 7.5 | 127.8 |
| 8 | 4 | 6,015 | .603 | .989 | 2.075 | 525.0 | 92.2 | 617.2 | 7.7 | 127.9 |
| 8 | 8 | 4,127 | 1.632 | 3.947 | 6.295 | 960.9 | 125.0 | 1,085.9 | 8.3 | 127.8 |
| 8 | 16 | 20,924 | .684 | 1.346 | 2.447 | 282.8 | 31.3 | 314.1 | 8.9 | 127.9 |
| 8 | 32 | 9,505 | 2.643 | 8.013 | 13.266 | 670.3 | 67.2 | 740.6 | 9.5 | 127.9 |

### PHP JSON

| W | C | RPS | P50 | P95 | P99 | SCPU | PCPU | CPU | SRAM | PRAM |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,232 | .419 | .761 | .941 | 406.3 | 65.6 | 459.4 | 6.6 | 16.0 |
| 1 | 2 | 3,885 | .504 | .893 | 1.159 | 409.4 | 56.3 | 465.6 | 6.7 | 16.0 |
| 1 | 4 | 8,352 | .453 | .589 | 1.002 | 204.7 | 29.7 | 235.9 | 6.8 | 16.0 |
| 1 | 8 | 8,717 | .897 | 1.031 | 1.542 | 212.5 | 31.3 | 243.8 | 6.9 | 16.0 |
| 1 | 16 | 8,607 | 1.797 | 2.059 | 3.206 | 204.7 | 28.1 | 235.9 | 7.3 | 16.0 |
| 1 | 32 | 8,573 | 3.624 | 4.090 | 6.514 | 221.9 | 32.8 | 251.6 | 7.5 | 16.0 |
| 2 | 1 | 2,109 | .459 | .710 | .907 | 414.1 | 75.0 | 489.1 | 6.7 | 32.0 |
| 2 | 2 | 8,856 | .210 | .342 | .529 | 184.4 | 39.1 | 212.5 | 6.9 | 32.0 |
| 2 | 4 | 14,749 | .250 | .386 | .707 | 203.1 | 43.8 | 250.0 | 7.1 | 32.0 |
| 2 | 8 | 15,389 | .481 | .676 | 1.556 | 204.7 | 51.6 | 254.7 | 7.3 | 32.0 |
| 2 | 16 | 15,146 | .983 | 1.483 | 2.994 | 207.8 | 46.9 | 254.7 | 7.6 | 32.0 |
| 2 | 32 | 15,501 | 1.919 | 2.681 | 5.786 | 190.6 | 40.6 | 231.3 | 8.0 | 32.0 |
| 4 | 1 | 3,004 | .264 | .688 | .890 | 310.9 | 50.0 | 371.9 | 6.9 | 64.0 |
| 4 | 2 | 4,435 | .435 | .610 | .782 | 396.9 | 84.4 | 471.9 | 7.1 | 64.0 |
| 4 | 4 | 6,326 | .613 | .876 | 1.069 | 543.8 | 98.4 | 646.9 | 7.4 | 64.1 |
| 4 | 8 | 9,378 | .808 | 1.322 | 1.663 | 545.3 | 84.4 | 653.1 | 7.8 | 64.0 |
| 4 | 16 | 9,262 | 1.661 | 2.377 | 2.987 | 595.3 | 82.8 | 678.1 | 8.3 | 64.0 |
| 4 | 32 | 8,696 | 3.621 | 4.759 | 5.536 | 620.3 | 67.2 | 692.2 | 8.7 | 64.1 |
| 8 | 1 | 3,085 | .323 | .514 | .825 | 317.2 | 43.8 | 360.9 | 7.3 | 128.0 |
| 8 | 2 | 4,302 | .312 | 1.017 | 1.248 | 317.2 | 79.7 | 396.9 | 7.5 | 128.0 |
| 8 | 4 | 6,091 | .636 | .891 | 1.280 | 546.9 | 114.1 | 660.9 | 7.7 | 128.0 |
| 8 | 8 | 9,379 | .775 | 1.457 | 1.960 | 550.0 | 51.6 | 601.6 | 8.3 | 128.0 |
| 8 | 16 | 10,143 | 1.394 | 2.808 | 4.657 | 601.6 | 53.1 | 671.9 | 9.1 | 128.0 |
| 8 | 32 | 19,934 | 1.379 | 3.158 | 6.150 | 312.5 | 34.4 | 334.4 | 9.3 | 128.0 |

Latency columns are milliseconds. The best observed medians were 8,733/15,876/
10,763/20,924 RPS for Hello and 8,717/15,501/9,378/19,934 for JSON at W1/W2/
W4/W8. Relative to W1, peak throughput was 1.82x/1.23x/2.40x for Hello and
1.78x/1.08x/2.29x for JSON, or about 91%/31%/30% and 89%/27%/29% per-worker
efficiency. W2 scales well; W4 is anomalously slower than W2; W8 needs C16-C32
to win. Several W8 cells varied by more than 4x between runs, so these are honest
host results, not evidence of linear scaling. Saturation begins near C8-C16 for
W1/W2, while larger pools are noisy enough that a stable saturation point cannot
be claimed.

### W1 comparison and soak

The immutable 0.1.2 concurrency smoke used 2,000 requests and one long-lived
process across its concurrency levels, while the investigation tables above
used a different parallel driver, 10,000 requests, and fresh processes per
cell. The earlier statement that W1 was materially slower at C1-C2 is withdrawn:
those figures cannot establish a regression percentage. The 3,000-request C1
spot check was also too short to replace the published 20,000-request 0.1.2
methodology. The accepted apples-to-apples W1 comparison is recorded below
after rerunning the 0.1.2 driver unchanged.

A separate release PHP worker pool-level correctness soak called
`pool.execute()` directly for 100,000 requests at W4/C32. It completed in
6.91 s while preserving all four PIDs, bootstrap count one per PID, the exact
aggregate request count, protocol synchronization, and clean reap. It is not an
end-to-end HTTP throughput benchmark; its elapsed time and derived ~14.5k RPS
must not be compared with HTTP benchmark cells. Working set was not sampled at
the start and end, so this soak makes no memory-stability claim. A distinct
external HTTP W4/C16 100,000-request run reported 11,917 RPS, P50/P95/P99
1.124/2.648/3.518 ms, Hamster/PHP/combined CPU 443.6/45.5/489.1 us per request,
and 8.6/64.0 MiB Hamster/total-PHP working set. Cancellation stress is a
correctness test, not a throughput workload; 32 marker-synchronized disconnects
left all four original workers healthy and serving.

### Accepted corrective benchmark methodology

The corrective run used the same PHP 8.5.10 NTS and Windows MSVC release target,
loopback HTTP/1.1, 1,000 sequential warm-up requests, `queue_capacity = 128`,
and three fresh Hamster/PHP process sets per cell. Each measured run continued
for at least eight seconds and at least 20,000 requests. The matrix covered
W1/W2/W4/W8, C1/C2/C4/C8/C16/C32, and Hello/JSON. Reproduce with:

```powershell
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 `
  -WarmupRequests 1000 -MinimumSeconds 8
```

`RPS min/med/max` is the spread across three runs. Latencies are milliseconds;
`SCPU`, `PCPU`, and `CPU` are median Hamster, total-PHP, and combined process
microseconds per request. `SRAM` and `PRAM` are median working set MiB. Fields
are medians independently and therefore need not sum exactly. The host still
showed distinct slow/fast modes in some cells; the spread is part of the result.

#### PHP Hello — accepted duration-controlled data

| W | C | RPS min/med/max | P50 | P95 | P99 | SCPU | PCPU | CPU | SRAM | PRAM |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,060/2,162/2,280 | .452 | .665 | .848 | 403.1 | 62.5 | 468.8 | 6.7 | 16.0 |
| 1 | 2 | 3,081/3,386/3,784 | .619 | .888 | 1.094 | 466.7 | 84.2 | 550.9 | 6.8 | 16.0 |
| 1 | 4 | 4,040/4,817/4,850 | .598 | 1.482 | 1.873 | 341.1 | 62.0 | 403.1 | 6.9 | 16.0 |
| 1 | 8 | 3,087/3,128/4,081 | 2.504 | 3.220 | 3.731 | 531.4 | 91.7 | 623.1 | 6.9 | 16.0 |
| 1 | 16 | 2,510/3,484/4,532 | 4.914 | 6.727 | 7.484 | 457.1 | 85.1 | 542.3 | 7.3 | 16.0 |
| 1 | 32 | 3,427/4,297/5,281 | 8.512 | 11.068 | 12.238 | 392.6 | 62.2 | 454.8 | 7.6 | 16.0 |
| 2 | 1 | 2,342/2,445/3,430 | .386 | .613 | .823 | 352.3 | 49.2 | 401.6 | 6.7 | 31.9 |
| 2 | 2 | 3,643/4,048/5,459 | .502 | .744 | .922 | 440.0 | 80.1 | 520.1 | 7.0 | 31.9 |
| 2 | 4 | 8,759/8,849/9,144 | .340 | .805 | .980 | 324.9 | 73.5 | 398.3 | 7.3 | 32.0 |
| 2 | 8 | 6,886/7,684/8,087 | 1.112 | 1.413 | 1.640 | 408.7 | 83.3 | 491.0 | 7.5 | 31.9 |
| 2 | 16 | 9,889/10,866/11,097 | 1.435 | 1.691 | 1.820 | 285.4 | 57.9 | 339.5 | 7.8 | 31.9 |
| 2 | 32 | 10,268/10,390/10,787 | 3.081 | 3.442 | 3.668 | 291.4 | 56.8 | 352.5 | 8.1 | 32.0 |
| 4 | 1 | 3,418/3,463/3,483 | .289 | .364 | .410 | 243.1 | 40.6 | 280.9 | 6.9 | 63.9 |
| 4 | 2 | 5,155/5,385/5,690 | .369 | .473 | .540 | 280.0 | 57.3 | 337.3 | 7.1 | 63.9 |
| 4 | 4 | 7,661/9,617/10,199 | .392 | .583 | .778 | 341.4 | 64.6 | 406.0 | 7.5 | 63.9 |
| 4 | 8 | 8,441/10,006/10,692 | .759 | 1.377 | 1.716 | 507.5 | 67.7 | 575.2 | 7.9 | 63.9 |
| 4 | 16 | 9,975/10,228/10,522 | 1.500 | 2.615 | 3.175 | 527.0 | 51.6 | 579.2 | 8.4 | 64.0 |
| 4 | 32 | 8,696/9,010/9,748 | 3.654 | 5.089 | 6.144 | 598.0 | 62.4 | 660.4 | 9.1 | 63.9 |
| 8 | 1 | 1,806/2,216/3,016 | .417 | .902 | 1.157 | 421.1 | 84.4 | 505.5 | 7.4 | 127.9 |
| 8 | 2 | 3,550/3,795/5,669 | .529 | .784 | .949 | 432.4 | 87.0 | 519.3 | 7.5 | 127.8 |
| 8 | 4 | 6,301/6,398/6,407 | .606 | .868 | 1.086 | 529.2 | 98.2 | 627.4 | 7.7 | 127.9 |
| 8 | 8 | 8,494/8,762/8,842 | .839 | 1.557 | 2.038 | 651.9 | 63.7 | 715.6 | 8.4 | 127.9 |
| 8 | 16 | 8,946/10,100/11,285 | 1.449 | 2.935 | 3.982 | 638.9 | 50.2 | 690.5 | 9.3 | 127.8 |
| 8 | 32 | 9,138/9,911/10,697 | 2.921 | 5.936 | 7.665 | 704.7 | 49.8 | 754.7 | 9.9 | 127.8 |

#### PHP JSON — accepted duration-controlled data

| W | C | RPS min/med/max | P50 | P95 | P99 | SCPU | PCPU | CPU | SRAM | PRAM |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,296/2,477/2,988 | .392 | .700 | .935 | 385.2 | 45.3 | 436.7 | 6.7 | 16.0 |
| 1 | 2 | 4,888/4,936/4,993 | .299 | .758 | 1.002 | 328.6 | 60.5 | 386.5 | 6.8 | 16.0 |
| 1 | 4 | 3,396/4,129/4,535 | 1.081 | 1.593 | 1.969 | 391.2 | 78.5 | 469.7 | 6.8 | 16.0 |
| 1 | 8 | 3,896/4,751/4,863 | 1.216 | 3.079 | 3.638 | 343.7 | 60.0 | 403.7 | 7.0 | 16.0 |
| 1 | 16 | 4,077/4,280/4,896 | 4.248 | 5.595 | 6.615 | 378.0 | 69.8 | 447.8 | 7.2 | 16.0 |
| 1 | 32 | 3,392/4,109/4,867 | 8.800 | 11.005 | 12.191 | 410.7 | 62.2 | 472.9 | 7.6 | 16.0 |
| 2 | 1 | 2,358/2,492/3,230 | .405 | .605 | .817 | 353.9 | 48.4 | 407.0 | 6.8 | 32.0 |
| 2 | 2 | 5,969/5,971/6,178 | .250 | .653 | .822 | 291.1 | 60.4 | 348.1 | 7.1 | 32.0 |
| 2 | 4 | 8,636/8,877/9,016 | .345 | .801 | .975 | 333.3 | 71.5 | 401.5 | 7.3 | 32.0 |
| 2 | 8 | 7,915/8,526/9,049 | 1.048 | 1.435 | 1.659 | 368.8 | 76.7 | 445.5 | 7.5 | 32.0 |
| 2 | 16 | 10,001/10,454/10,681 | 1.519 | 1.816 | 1.971 | 287.1 | 61.6 | 348.8 | 7.8 | 32.0 |
| 2 | 32 | 10,263/10,553/11,147 | 3.015 | 3.482 | 3.748 | 297.9 | 58.7 | 357.1 | 8.3 | 32.0 |
| 4 | 1 | 3,208/3,286/3,320 | .301 | .420 | .516 | 271.8 | 53.5 | 324.5 | 6.9 | 64.0 |
| 4 | 2 | 4,973/5,030/6,163 | .388 | .508 | .616 | 328.8 | 69.9 | 398.7 | 7.2 | 64.0 |
| 4 | 4 | 8,224/8,446/8,636 | .429 | .799 | 1.023 | 387.6 | 72.8 | 459.5 | 7.5 | 64.1 |
| 4 | 8 | 8,682/10,620/10,951 | .682 | 1.365 | 1.760 | 485.9 | 59.8 | 545.7 | 7.8 | 64.0 |
| 4 | 16 | 8,871/10,466/10,848 | 1.460 | 2.557 | 3.097 | 527.4 | 62.3 | 584.7 | 8.4 | 64.0 |
| 4 | 32 | 6,221/7,825/11,502 | 4.042 | 5.338 | 6.171 | 724.4 | 65.1 | 789.5 | 8.9 | 64.0 |
| 8 | 1 | 1,861/1,995/2,975 | .449 | .868 | 1.160 | 473.4 | 88.3 | 561.7 | 7.3 | 128.0 |
| 8 | 2 | 4,257/5,149/5,892 | .289 | .699 | .876 | 339.1 | 73.2 | 412.3 | 7.5 | 128.1 |
| 8 | 4 | 7,186/7,332/8,065 | .556 | .837 | 1.056 | 460.8 | 82.6 | 541.3 | 7.8 | 128.1 |
| 8 | 8 | 8,871/10,539/10,772 | .673 | 1.476 | 1.997 | 501.4 | 51.1 | 551.6 | 8.4 | 128.0 |
| 8 | 16 | 8,882/9,748/11,362 | 1.499 | 3.086 | 4.056 | 698.7 | 51.9 | 750.6 | 9.3 | 128.0 |
| 8 | 32 | 9,420/10,011/10,187 | 2.823 | 6.419 | 8.553 | 672.7 | 47.7 | 720.4 | 9.9 | 128.0 |

#### W1 apples-to-apples and scaling interpretation

The unchanged 0.1.2 driver was run against 0.2.0 W1 with its published
20,000-request, 100-warm-up, concurrency-one methodology. The first three-run
batch produced Hello 3,406/5,129/5,300 RPS and JSON 2,086/3,002/5,293 RPS
(min/median/max). A workload-isolated repeat produced Hello 1,984/2,009/3,408
and JSON 2,171/2,402/2,410 RPS. Historical 0.1.2 medians were 7,004 and 7,204
RPS respectively. Thus the same-methodology measurements do show lower W1
throughput, but the two contemporary batches disagree substantially; they do
not isolate a trustworthy pool-overhead percentage on this host. No 0.2.0
performance improvement is claimed.

Same-concurrency median RPS illustrates the measured scaling without mixing
different load levels:

| Workload | C | W1 | W2 | W4 | W8 |
|---|---:|---:|---:|---:|---:|
| Hello | 4 | 4,817 | 8,849 | 9,617 | 6,398 |
| Hello | 16 | 3,484 | 10,866 | 10,228 | 10,100 |
| Hello | 32 | 4,297 | 10,390 | 9,010 | 9,911 |
| JSON | 4 | 4,129 | 8,877 | 8,446 | 7,332 |
| JSON | 16 | 4,280 | 10,454 | 10,466 | 9,748 |
| JSON | 32 | 4,109 | 10,553 | 7,825 | 10,011 |

The best observed median configurations were W2/C16 at 10,866 RPS for Hello
and W4/C8 at 10,620 RPS for JSON. Those are best configurations, not scaling
efficiency claims. W2 supplied the major concurrency gain; W4 and W8 did not
improve consistently at the same concurrency. Latency increased once
concurrency exceeded useful parallelism, and PHP working set scaled at roughly
16 MiB per worker.

The earlier W4 Hamster CPU signal reproduced at higher concurrency: W4 C16 was
about 527 us/request for both workloads versus about 285-287 us for W2 C16,
with similar throughput. W4 C32 rose to 598 us (Hello) and 724 us (JSON), and
W8 C16-C32 was about 639-705 us (Hello) and 673-699 us (JSON). It did not appear
uniformly at W4 C1-C4, and host variance remains material. This is an observation
requiring later investigation, not evidence for a speculative dispatcher or
channel rewrite in 0.2.0.

## 0.3.0 Worker Lifecycle Regression Matrix

The limited 0.3 regression run used the existing duration-controlled pool
driver and the same Windows MSVC release/PHP 8.5.10 NTS environment as the
accepted 0.2 corrective data. Each cell used three fresh Hamster/PHP process
sets, 1,000 warm-up requests, at least 20,000 measured requests, at least eight
measured seconds, and `queue_capacity = 128`. Both `max_requests` and
`max_worker_age` were OFF. This is a steady-state lifecycle regression check,
not a new scaling study.

```powershell
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 `
  -WarmupRequests 1000 -MinimumSeconds 8 `
  -WorkerCounts 1 -Concurrencies 1 -Workload all
./benchmarks/php_worker_pool.ps1 -Runs 3 -Requests 20000 `
  -WarmupRequests 1000 -MinimumSeconds 8 `
  -WorkerCounts 4 -Concurrencies 16,32 -Workload all
```

| Workload | W/C | RPS min/med/max | P50 | P95 | P99 | SCPU | PCPU | CPU | SRAM | PRAM |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Hello | 1/1 | 2,572/2,904/4,588 | .376 | .485 | .642 | 336.3 | 45.7 | 382.0 | 6.7 | 16.0 |
| Hello | 4/16 | 9,945/10,314/15,815 | 1.515 | 2.267 | 2.951 | 579.5 | 67.4 | 646.9 | 8.5 | 63.9 |
| Hello | 4/32 | 8,990/9,601/11,815 | 3.293 | 4.361 | 5.184 | 645.5 | 69.3 | 716.9 | 8.9 | 63.9 |
| JSON | 1/1 | 3,434/3,526/3,950 | .251 | .452 | .605 | 299.1 | 45.4 | 344.5 | 6.7 | 16.0 |
| JSON | 4/16 | 11,743/12,634/13,595 | 1.205 | 2.178 | 2.728 | 464.9 | 56.0 | 520.9 | 8.4 | 64.0 |
| JSON | 4/32 | 11,508/11,911/12,675 | 2.828 | 3.996 | 4.756 | 510.9 | 61.4 | 575.5 | 8.7 | 64.0 |

Latency columns are milliseconds; CPU columns are microseconds per request;
RAM columns are MiB. Relative to the matching 0.2 cells, median RPS changed by
+34.3%, +0.8%, and +6.6% for Hello W1/C1, W4/C16, and W4/C32, and +42.3%,
+20.7%, and +52.2% for JSON. No cell crossed the release flag of more than 10%
steady-state regression. The Hello W1/C1 and W4/C16 spreads remain wide, so
the positive deltas are host observations, not claimed lifecycle performance
improvements.

Server CPU/request was 336 us (Hello) and 299 us (JSON) at W1/C1; 580/465 us
at W4/C16; and 646/511 us at W4/C32. This limited matrix shows no new CPU
regression attributable to dormant lifecycle management. It does not reopen
the existing W4/W8 CPU investigation.

## 0.3 Retirement Hardening — Steady-State Regression Check

Release `x86_64-pc-windows-msvc`, PHP 8.5.10 NTS, `benchmarks/php_worker_pool.ps1`,
recycling OFF, no injected failures, queue_capacity 128, three runs per cell with an
eight-second floor. Baseline and candidate were built and measured in the same session
on the same host, back to back, because this host's run-to-run spread has repeatedly
exceeded the effect being looked for.

Baseline is the pre-change working tree (60-second stability period, permanent
retirement). Candidate is the same tree with `STABILITY_RESET_PERIOD` at 15 s and
permanent retirement replaced by the bounded cooldown band.

| Workload | W/C | Baseline RPS (min/med/max) | Candidate RPS (min/med/max) | Median delta |
|---|---|---|---|---|
| Hello | 1/1 | 6080 / 6455 / 6574 | 5734 / 6235 / 6425 | -3.4% |
| JSON | 1/1 | 6354 / 6455 / 6463 | 6432 / 6489 / 6564 | +0.5% |
| Hello | 4/16 | 29000 / 30285 / 31378 | 30332 / 31606 / 31617 | +4.4% |
| JSON | 4/16 | 30648 / 31231 / 31621 | 30909 / 31574 / 31953 | +1.1% |
| Hello | 4/32 | 27691 / 31127 / 31440 | 30701 / 31206 / 32319 | +0.3% |
| JSON | 4/32 | 31122 / 31141 / 31811 | 30702 / 31904 / 32426 | +2.4% |

Latency medians, milliseconds:

| Workload | W/C | Baseline p50/p95/p99 | Candidate p50/p95/p99 |
|---|---|---|---|
| Hello | 1/1 | 0.145 / 0.211 / 0.347 | 0.147 / 0.234 / 0.329 |
| JSON | 1/1 | 0.143 / 0.221 / 0.327 | 0.141 / 0.211 / 0.337 |
| Hello | 4/16 | 0.485 / 0.859 / 1.155 | 0.463 / 0.822 / 1.129 |
| JSON | 4/16 | 0.470 / 0.834 / 1.140 | 0.466 / 0.808 / 1.115 |
| Hello | 4/32 | 0.972 / 1.541 / 2.308 | 0.977 / 1.460 / 1.926 |
| JSON | 4/32 | 0.982 / 1.479 / 1.993 | 0.962 / 1.484 / 2.009 |

Server RAM was 6.8-9.1 MiB on both trees; PHP RAM was 16.0 MiB at W1 and 64.0-64.1 MiB
at W4 on both. Combined CPU per request was 184-223 us on both trees with no ordering
by tree.

Hello W1/C1 was the only negative cell, so it was re-measured with five runs per tree:
baseline 6025/6467/6717, candidate 6221/6599/6681, a median delta of **+2.0%**. The sign
of that cell reversed between repetitions, which shows the first -3.4% was host noise
rather than a measured cost.

No cell crossed the release flag of more than 10% steady-state regression. Because the
run-to-run spread on this host is larger than every delta observed, these numbers can
only fail to detect a regression; they do not establish an improvement, and none is
claimed. The structural argument is the stronger evidence: the diff adds no operation to
a successful request. `record_success` is unchanged except for the value of a constant
it already compared against, the new `cooldown_delay` runs only from
`handle_serving_exit` on the failure path, and the `stash_idle` bound assertion is a
`debug_assert!` that compiles out of the release profile, which sets no
`[profile.release]` override and therefore leaves `debug-assertions` off.

## 0.4 Laravel Persistent Worker — Bootstrap Cost

Windows `x86_64-pc-windows-msvc`, PHP 8.5.10 NTS with OPcache, release build, four
workers, concurrency 16, requests per second. Two applications:

- **app12** — Laravel 12.69.2 skeleton with `artisan optimize` applied.
- **PMS** — a real Laravel 12.66.0 application (305 routes, JWT, Redis sessions and
  cache, remote MySQL, dompdf, PhpSpreadsheet), no config cache, supplied under the
  git-ignored `www/`.

The `cold` column rebuilds the Laravel application for every request **inside the same
worker, through the same IPC path**, with the process alive and the OPcache warm. It is
the best case a well-configured PHP-FPM deployment can reach, not `artisan serve`.
The `no reset` column is faster than `reset` but is not a legitimate baseline: it is
faster because it leaks authenticated identity between requests.

| Application | Route | cold | no reset | reset | reset vs cold |
| --- | --- | --- | --- | --- | --- |
| PMS | /up | 347 | 7429 | 6721 | 19.4x |
| PMS | /login (19 KB Blade) | 155 | 2090 | 1678 | 10.8x |
| app12 | /c/json | 1628 | 4284 | 3781 | 2.32x |
| app12 | /c/db | 1179 | 3291 | 2957 | 2.51x |
| app12 | /c/view | 1500 | 3496 | 3192 | 2.13x |

Authenticated PMS dashboard, 2,223,437 bytes, many queries against a remote MySQL,
measured sequentially rather than at pool saturation: cold p50 312.90 ms / 3.2 rps,
reset p50 238.27 ms / 4.2 rps, a gain of **1.31x**.

The gain is inversely proportional to how much real work a request does. Bootstrap is a
fixed cost of roughly 10 ms on PMS: the whole request on a trivial route, three percent
of it on the dashboard. Both figures are real and a deployment will see the range.

app12 has `artisan optimize` applied and PMS does not, so PMS's cold column re-reads
every configuration file per request; a production-tuned PMS would land between the two
applications, and that was not measured.

For scale, the trivial 0.3 worker reached 5482 rps at concurrency 1 and 35,169 at 16 on
the same host and build. Laravel, not Hamster's transport, is the limit.

Reset cost, 400 iterations per step: 0.021 ms total on app12, 0.018 ms on PMS, dominated
by `setLocale` at 0.010 ms. The reset itself is free; the measurable cost of correctness
is rebuilding what it discarded, roughly 0.25-0.46 ms on a session-using route, which a
per-request model pays as well.

Memory, least-squares slope over the second half of each soak. Two-point slopes are
unusable because PHP's cycle collector makes the series saw-tooth; an early two-point
estimate reported 130 B/request that regression over 100,000 requests showed to be zero.

| Soak | Slope |
| --- | --- |
| app12, controller routes, no reset, 100,000 requests | +0.0 B/request |
| app12, controller routes, reset, 100,000 requests | -18.0 B/request |
| app12 over HTTP, 237,477 requests, four routes | worker RSS pinned at 55.1 MB |

One genuine leak was found and it is Laravel's, not Hamster's: a cached closure route
leaks 786-1688 B per dispatch, scaling with the closure's source length, because
`Route::runCallable()` unserializes the action on every dispatch and the serializer
reconstructs it with an `include` of a stream whose op_array PHP never frees. Controller
routes leak nothing. See `doc/LARAVEL_WORKER_0_4.md`.

Bootstrap to READY: 58 ms median for app12, 71 ms for PMS, 403 ms worst observed, against
a 5000 ms `STARTUP_TIMEOUT`.

## 0.4 Laravel Persistent Worker — Production-Tuned Application

Windows `x86_64-pc-windows-msvc`, PHP 8.5.10 NTS with OPcache, release build. Medians of
three runs per cell, four workers, concurrency 16, minimum four seconds per run. The
earlier 0.4 section measured PMS with no configuration or route cache, which flattered
the persistent column; this measures both. The supplied application was not modified: it
was copied to a scratch directory and `artisan optimize` was run against the copy.

| Route | `artisan optimize` | cold rps | persistent rps | gain |
| --- | --- | --- | --- | --- |
| `/up` | no | 337 | 6,804 | 20.2x |
| `/up` | yes | 1,618 | 5,359 | 3.3x |
| `/login` | no | 158 | 1,703 | 10.8x |
| `/login` | yes | 204 | 1,552 | 7.6x |

Most of the twenty-fold figure was PMS being untuned. A production-tuned application
keeps 3.3x on a trivial route and 7.6x on a Blade page.

`artisan optimize` made the persistent worker slower on `/up`, and the cause is the
cached-closure-route leak already documented meeting Laravel's own code:
`withRouting(health: '/up')` registers the health endpoint as a closure, so the route
cache makes every dispatch unserialize it. One worker, 30,003 requests to `/up`:

| PMS build | rps | worker RSS before | worker RSS after | growth |
| --- | --- | --- | --- | --- |
| no route cache | 2,209 | 61.92 MB | 61.92 MB | 0 B/request |
| `artisan optimize` | 1,773 | 64.16 MB | 74.97 MB | +377.9 B/request |

Request-boundary reset, measured end to end after a real request rather than as a sum of
per-step timings. The earlier 0.021 ms / 0.018 ms figures in the previous section are
per-step sums and are not the cost of one call.

| Reset | p50 | p95 |
| --- | --- | --- |
| app12 `/c/json`, n=800, full reset | 0.0524 ms | 0.0848 ms |
| app12 `/c/json`, n=800, without the 0.4 Eloquent and url steps | 0.0492 ms | 0.0728 ms |
| PMS `/login`, n=400, full reset | 0.0482 ms | 0.0865 ms |
| PMS `/login`, n=400, without the 0.4 Eloquent and url steps | 0.0442 ms | 0.0694 ms |

The added steps cost 0.003-0.004 ms per request, about 0.3 percent of a 1.1 ms request.
An end-to-end throughput A/B could not resolve the difference: run-to-run variation
within one server instance reached 23 percent, which is larger than the effect. As in
0.3, this data can fail to detect a regression; it cannot prove there is none.

## 0.4 Laravel Persistent Worker — Octane Lifecycle

This additive result supersedes the old reset implementation for current 0.4
decisions; it does not rewrite either historical table. Windows release build,
PHP 8.5.10 NTS with CLI OPcache, Laravel 12.66.0 PMS scratch clone at commit
`552e27525b1161ed10b36bdd726d203ecfa3ccff`, Laravel Octane 2.19.1 and
`artisan optimize`. Both candidates used the same Rust HTTP server, protocol-v1
IPC, pool size, routes and fully consumed responses. `fresh` rebuilt Laravel on
every request while keeping PHP and OPcache alive; it is an optimistic
bootstrap baseline rather than a complete PHP-FPM emulation.

Light and login cells are medians of three fresh server runs, W4/C16, 500
warm-up and at least 5,000 measured requests. Dashboard cells are three W1/C1
runs of 10 warm-up and 100 measured authenticated requests against the real
remote MySQL dependency.

| Route | fresh RPS | Octane RPS | gain | fresh p50 | Octane p50 |
|---|---:|---:|---:|---:|---:|
| controller JSON | 751.1 | 1137.0 | 1.51x | 20.609 ms | 13.209 ms |
| Blade `/login` | 623.1 | 931.3 | 1.49x | 24.637 ms | 15.832 ms |
| authenticated dashboard | 3.28 | 3.54 | 1.08x | 301.828 ms | 273.423 ms |

The W4 persistent ranges were 1044.2–1150.4 RPS for the controller and
865.2–976.5 for login, so exact multipliers have material machine variance;
the fresh ranges did not overlap them. Remote-DB variance was larger on the
dashboard and supports only a modest-benefit conclusion. The old 3.3x, 7.6x
and 1.31x figures must not be attributed to this lifecycle.

A W1 20,000-request authenticated/anonymous alternating soak held one PID,
produced no wrong response, and had 0 B/request least-squares RSS slope over
its second half. A final cleanup-failure injection returned 502, executed its
marker once, replaced the process and served 200 on the new generation. Raw
measurements, exact conditions and reproduction commands are in
`benchmarks/results/laravel_pms_octane_0_4_20260915.md`.

Uploads, Laravel 12 over HTTP, four workers, 600 multipart requests across `/u/store`
and `/u/inspect`: every response 200, and zero temporary files left in the system
temporary directory. The request rate is not reported because the client is a serial
`fsockopen` loop written to control the exact bytes on the wire, not a load driver.

## 0.5.x Immediate Transport Identity

Windows 11 Pro build 26200, Intel Core i5-12400 (6 cores / 12 logical), 32 GiB,
`x86_64-pc-windows-msvc` release build based on `9a0bb0c`. The checked-in static
HTTP driver ran three fresh-server samples per tree, concurrency 4, 1,000 warm-up
and 50,000 measured requests per sample. The baseline was built immediately
before the source edit; the candidate was rebuilt after it.

```powershell
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/static_http.ps1 -Runs 3 -Requests 50000 -WarmupRequests 1000 -Concurrency 4
```

| Tree | median RPS | p50 | p95 | p99 | CPU us/request | working set |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 8,035 | 0.487 ms | 0.633 ms | 0.743 ms | 343.125 | 7.1 MiB |
| transport identity | 8,272 | 0.479 ms | 0.597 ms | 0.678 ms | 330.312 | 6.9 MiB |

Baseline RPS ranged 7,946–8,126 (2.2 percent); candidate RPS ranged
8,151–8,384 (2.8 percent). Candidate median RPS was 3.0 percent higher, but this
is not claimed as an improvement: it is close to host variation and the
baseline p99 range was 13.3 percent. The evidence rules out a material
steady-state regression under this workload, not a small cost.

The source-level result is narrower and stronger. The change queries
`TcpStream::local_addr()` once per accepted connection, then copies two
`SocketAddr` values through dispatch. It creates no per-request allocation,
lock, syscall or proxy-header parsing. A design using request `Extensions` was
not benchmarked because inspecting `http` 1.5.0 showed that the first insertion
would allocate its lazy type map on every request; it was rejected before
implementation.

## External Research Evidence — FrankenPHP PMS Worker Mode

This is an additive research record, not a Hamster release benchmark or a
cross-server ranking. The canonical consolidated report is
[`doc/FRANKENPHP_BENCHMARK.md`](FRANKENPHP_BENCHMARK.md); independent analysis
is in
[`doc/research/FRANKENPHP_PMS_EVIDENCE.md`](research/FRANKENPHP_PMS_EVIDENCE.md).

On the same Windows 11 / 12-logical-CPU machine, FrankenPHP 1.12.7 with PHP
8.5.10 ZTS served PMS with 12 Laravel Octane workers. The consolidated evidence
covers `/login` plus four authenticated database-backed routes at C1/C5/C20,
and retains the first-round `/login` C1/C10/C50 ceiling probe. No replicated
raw sample file was supplied.

At C1, worker mode reduced observed latency by an absolute 23.1-58.8 ms/request
across the five routes. The resulting worker/classic ratio ranged from 11.0x on
`/login` to 1.3x on the expense report. This isolates persistent versus repeated
Laravel bootstrap inside the same FrankenPHP process; it does not isolate
in-process versus subprocess placement. Laravel caches raised the observed
classic C50 result from 366 to 577 RPS while the worker result remained 2,663
versus 2,643 RPS. Tuning therefore changes the fresh-bootstrap baseline but
does not turn its speedup ratio into native-runtime evidence.

Coarse `/login` normalization gives FrankenPHP about 223.5 RPS/worker at C20
and 243.8 at C50, versus Hamster's 232.8 at W4/C16. The available FrankenPHP
cells therefore straddle Hamster rather than show a stable placement advantage.
This remains an invalid product comparison because PMS commits, PHP ZTS/NTS
builds, application cache/environment settings, compression, concurrency and
session/cache configuration differ. It does show that total RPS, which mostly
scales with worker count, cannot justify a native embedding rewrite.

The heavy expense-report worker cell plateaus around 15.4-15.7 RPS from C5 to
C20 while latency grows sharply. That is evidence of application/backend
saturation, not server-runtime cost. First-request versus warm observations
also differ by as much as 65x, but the largest gaps could not be isolated from
shared Redis/application cache state. Cold behavior, warm steady state and
database saturation must therefore remain separate measurements.

No CPU breakdown, comparable private-memory accounting, replicated runs,
multi-user lifecycle qualification, recycle, crash/recovery, cleanup-failure
or long memory soak was recorded. The reported 317 MB covers one process
containing Caddy, Go, classic PHP capacity and 12 persistent workers, so it
cannot be compared with summed multi-process working sets.

## Persistent Worker Cost Gate 0–1

Windows 11 build 26200, Core i5-12400, 32 GiB, Balanced power plan. Release
MSVC Hamster at `fc75f705`, PHP 8.5.10 NTS, PMS `552e2752`, Laravel 12.66.0
and Octane 2.19.1. Each accepted cell used three fresh servers and at least ten
measured seconds. Raw evidence and rejected runs are under
[`benchmarks/results/persistent_cost_gate_0_1_20260916/`](../benchmarks/results/persistent_cost_gate_0_1_20260916/);
analysis is in
[`doc/research/PERSISTENT_WORKER_COST_GATE_0_1_RESULTS.md`](research/PERSISTENT_WORKER_COST_GATE_0_1_RESULTS.md).

Production-format blocking IPC measured 86.9 us P50. Its production blocking
payload curve was 68.6 us P50 at 16 KiB, 104.5 us at 64 KiB and 950.9 us at
1 MiB, with no Windows timer-quantum samples. The default PHP negative control
still reproduced the known 15.071 ms timer-quantum failure.

| Workload | W/C | median RPS | P50 | Rust CPU/request | PHP CPU/request | private/working set |
|---|---:|---:|---:|---:|---:|---:|
| trivial JSON | 1/1 | 5,394 | .173 ms | 161.9 us | 28.4 us | 9.6/24.0 MiB |
| trivial JSON | 4/16 | 27,134 | .542 ms | 200.1 us | 29.4 us | 36.4/76.0 MiB |
| PMS light | 1/1 | 465 | 2.048 ms | 275.0 us | 1,815.6 us | 26.3/70.1 MiB |
| PMS login | 1/1 | 365 | 2.618 ms | 321.9 us | 2,343.8 us | 28.5/73.2 MiB |
| PMS light, isolated sessions | 4/16 | 1,361 | 11.560 ms | 318.1 us | 2,519.7 us | 102.0/235.6 MiB |
| PMS login, isolated sessions | 4/16 | 1,095 | 14.016 ms | 332.1 us | 3,266.4 us | 110.7/245.3 MiB |

The comparable W1 trivial JSON interval is .185 ms, versus 2.151 ms for the
minimal Laravel controller. The 1.965 ms difference is a combined framework,
middleware and Octane-lifecycle budget; Gate 1 cannot attribute it further.
Process-private memory scaled approximately linearly from W1 to W4. System
commit was rejected as a metric because unrelated workstation activity moved
it from -576 MiB to +1,037 MiB while process-tree private bytes stayed stable.

An initial W4 harness shared one CookieContainer across all lanes and therefore
measured one concurrent Laravel session. It is retained only as diagnostic raw
evidence. Accepted W4 cells use an independent client and cookie jar per lane.
One externally contaminated shared-session run and the full repeat are also
preserved rather than silently discarded.

## 2026-09-16 — Persistent worker cost Gate 2

Gate 2 used a generated, measurement-only Laravel worker behind the existing
`profiling` feature build. It sampled PHP lifecycle boundaries at 1/64 and
dumped buffered records only after throughput and CPU counters were captured.
The accepted observer changed median throughput by -1.5% at W1 and -2.4% at
W4, both below the 3% band and normal run spread. Earlier designs that wrote
profile batches during the timed window slowed W4 by 6.7-7.8% and are retained
as rejected evidence.

On the PMS light route, PHP P50 decomposed to 1.224 ms Laravel
kernel/middleware/controller, 0.536 ms Octane lifecycle and 0.085 ms direct
Hamster PHP adapter at W1/C1. At W4/C16 these became 1.515 ms, 0.670 ms and
0.099 ms. PHP CPU/request rose from a 2.209 ms median to 2.690 ms; Rust rose
only from 0.253 ms to 0.271 ms.

A scratch-only controller without the `web` middleware group measured
0.779 ms PHP P50 at W1 and 0.866 ms at W4, while scaling from 998 to 3,977 RPS
(approximately 3.98x). The session-bearing light route scaled about 3.40x.
This locates most multi-worker efficiency loss in the PHP `web` middleware
workload, not Hamster pool/IPC, but does not isolate file sessions from the
rest of the middleware group.

Raw records, hashes, run ranges, observer failures and commands are in
`benchmarks/results/persistent_cost_gate_2_20260916/`. Detailed interpretation
is in `doc/research/PERSISTENT_WORKER_COST_GATE_2_RESULTS.md`. Gate 3 was not
opened and no production architecture or protocol was changed.

## 0.5.x Bounded HTTP/1 Admission

Windows 11 Pro build 26200, Core i5-12400 (6 cores / 12 logical), 32 GiB,
MSVC release builds. Baseline `11fd550` and the working-tree candidate used the
same .NET client, Balanced power plan, concurrency 32 and 1,000 warm-up
requests. Three fresh servers each handled 500,000 static `/` requests. Exact
per-run values and binary hashes are in
[`benchmarks/results/http_admission_0_5_20260916.md`](../benchmarks/results/http_admission_0_5_20260916.md).

```powershell
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/static_http.ps1 -Runs 3 -Requests 500000 -Concurrency 32 -WarmupRequests 1000
```

| Tree | median RPS | p50 | p95 | p99 | CPU us/request | working set |
|---|---:|---:|---:|---:|---:|---:|
| `11fd550` | 6,941 | 4.575 ms | 5.187 ms | 5.688 ms | 436.312 | 9.7 MiB |
| bounded admission | 7,268 | 4.336 ms | 4.992 ms | 5.437 ms | 422.375 | 9.2 MiB |

Baseline RPS ranged 6,811-7,170 (5.2%); candidate ranged 7,218-7,371
(2.1%). The candidate's 4.7% higher median is not attributed to the change:
this fully buffered static path is filesystem-dominated and the A/B runs were
sequential. This evidence rejects a material static-path regression but cannot
resolve a small HTTP-core cost.

The same binaries therefore ran the built-in `/api` not-found route, whose
small in-memory 404 response removes filesystem work. The driver now accepts a
request path, expected status and executable so this control is reproducible.
Seven fresh servers per tree each handled 500,000 measured requests after
20,000 warm-up requests:

```powershell
./benchmarks/static_http.ps1 -Runs 7 -Requests 500000 -Concurrency 32 `
  -WarmupRequests 20000 -RequestPath /api -ExpectedStatus 404 `
  -Executable <release-binary>
```

| Tree | median RPS | p50 | p95 | p99 | CPU us/request | working set |
|---|---:|---:|---:|---:|---:|---:|
| `11fd550` | 140,432 | 0.211 ms | 0.352 ms | 0.680 ms | 28.438 | 7.7 MiB |
| bounded admission | 137,773 | 0.214 ms | 0.358 ms | 0.685 ms | 29.656 | 7.7 MiB |

Baseline RPS ranged 139,022-141,544 (1.8%); candidate ranged
134,399-139,626 (3.8%). The candidate used 1.218 additional CPU
microseconds/request (+4.3%) and served 1.9% fewer requests/second at the
median. This is a real measurable cost on an intentionally trivial response,
not a general 4.3% application regression. The locked Hyper 1.11.1 path boxes
a Tokio sleep future for each request head and discards it after parsing; that
is the most plausible source, but no allocation profile was taken, so the
attribution remains an inference.

The cost buys a total deadline for every request head and idle keep-alive
period. Replacing it with a service-wide timeout would be incorrect because it
could cancel a possibly-started backend transaction. A custom shared timer or
I/O wrapper was not introduced without evidence that roughly 1.2 us/request on
the smallest response justifies new synchronization and lifecycle risk.

## 0.5.x Bounded PHP body reconstruction — working tree

The body-read deadline A/B is recorded in
[`benchmarks/results/http_body_0_5_20260916.md`](../benchmarks/results/http_body_0_5_20260916.md).
It used seven fresh Windows MSVC release servers per binary, 20,000 measured
requests, 2,000 warm-up requests and one client connection against the local
PHP 8.5 worker. The candidate adds no timer to empty bodies: it checks the
Hyper end-stream fast path before entering `tokio::time::timeout`.

| Scenario | baseline median RPS | candidate median RPS | baseline combined CPU us/request | candidate combined CPU us/request |
|---|---:|---:|---:|---:|
| PHP hello, empty body | 5,942 | 5,867 | 195.312 | 210.938 |
| PHP echo, small body | 5,483 | 5,546 | 228.125 | 224.219 |

FACT: the empty-body result is a 1.3% lower candidate median with substantial
run-to-run CPU variance; it is not a release-grade regression claim. The body
carrying result is 1.1% higher RPS and 1.7% lower combined CPU. The in-memory
`/api` control had 19.8-20.5% RPS spread across three runs and cannot resolve a
small core-path effect. These numbers support retaining the simple fast path and
do not justify an optimization pass before a larger body-size matrix.

## 0.5.x aggregate body-memory investigation — working tree

The bounded-body memory probe is recorded in
[`benchmarks/results/body_memory_0_5_20260916.md`](../benchmarks/results/body_memory_0_5_20260916.md).
It uses a Windows MSVC release binary, the fixture PHP worker, 1 MiB declared
bodies and a delayed endpoint. Hamster private-memory deltas for one worker and
`queue_capacity=128` were 6.52, 10.93, 21.32, 37.52 and 73.71 MiB at 4, 8, 16,
32 and 64 accepted connections respectively. With 32 connections, changing the
queue from 1 to 8 to 128 changed the delta from 6.17 to 14.78 to 37.62 MiB.

An incomplete-body probe sent only 0.5 MiB of a declared 1 MiB body on 32
connections with `queue_capacity=1`; Hamster private memory still rose 37.58 MiB
while PHP memory did not change. FACT: queue capacity cannot bound bodies that
have not reached pool admission. INFERENCE: the current aggregate bound is
primarily `max_connections * body_limit`, plus parser/frame/allocator overhead.
No byte semaphore was added because the measurement establishes the risk but not
yet the best rejection/backpressure semantics or a justified hot-path trade-off.

## 0.5.x aggregate body-memory budget prototype — working tree

Full conditions are recorded in
[`benchmarks/results/body_budget_0_5_20260916.md`](../benchmarks/results/body_budget_0_5_20260916.md).
Both arms are the same Windows MSVC release binary; `server.max_body_memory = 0`
disables the budget and restores the previous code path, so this is an A/B of
one binary rather than of two builds.

Slow-upload flood, 1 MiB declared bodies with half sent, one worker,
`queue_capacity=128`, Hamster private-memory delta:

| accepted connections | budget off | budget 16 MiB |
|---:|---:|---:|
| 32 | 36.52 MiB | 18.23 MiB |
| 64 | 72.41 MiB | 18.93 MiB |
| 128 | 140.59 MiB | 18.87 MiB |
| 256 | 278.50 MiB | 18.79 MiB |

FACT: off, the delta is linear at 1.09 MiB per accepted connection. On, it is
flat across an eightfold change in connection count, a 14.8x reduction at 256
connections.

At a fixed 128 connections the delta tracked the configured budget: 4.48 MiB at
4 MiB, 18.36 MiB at 16 MiB, 70.07 MiB at 64 MiB, against 140.59 MiB disabled —
1.09 to 1.15 times the budget. The excess is Hyper per-connection buffers and
the worker's protocol frame, which the budget deliberately does not cover.

With all 128 bodies sent whole so they sit in the bounded queue, the delta was
20.06 MiB with a 16 MiB budget against 143.93 MiB disabled.

Hot-path A/B, `php_worker.ps1`, one worker, concurrency 1, 20,000 measured
requests after 1,000 warm-up requests:

| Scenario | budget 64 MiB | budget off |
|---|---:|---:|
| PHP hello, no body (5 runs) | 7,086 rps | 7,097 rps |
| PHP echo, 12 B body (5 runs) | 6,728 rps | 6,514 rps |
| PHP echo, 12 B body (interleaved pass 1) | 6,524 rps | 6,803 rps |
| PHP echo, 12 B body (interleaved pass 2) | 6,694 rps | 6,745 rps |
| PHP echo, 64 KiB body (interleaved pass 1) | 3,703 rps | 3,733 rps |
| PHP echo, 64 KiB body (interleaved pass 2) | 3,722 rps | 3,686 rps |

FACT: the sign of the difference flips between passes and within-arm spread
reached 16% on the 12-byte POST. HONEST LIMIT: this harness can fail to detect a
regression below roughly 5%; it shows no visible cost and does not prove there
is none. The same change removed a full `Vec` copy of every reconstructed body,
which plausibly offsets the added atomics on this workload.

## 0.5 milestone review resource probes — working tree

Full procedures are recorded in
[`benchmarks/results/review_0_5_20260916.md`](../benchmarks/results/review_0_5_20260916.md).
These are resource-bound observations taken while reviewing the four 0.5 slices
as a milestone candidate, not throughput benchmarks. The probe scripts were
throwaway and are not checked in; each procedure is written out instead.

FACT: four clients that requested a 256 MiB static file and then stopped reading
held 1026.4 MiB of process-private memory against a 1.47 MiB idle baseline —
256.6 MiB per in-flight request, one complete copy of the file each. Response
bodies are outside `max_body_memory` and outside every other 0.5 limit. This is
sixteen times the entire default request-body budget, reached by four ordinary
requests.

FACT: the hold is bounded in time but not in size. With one connection slot and a
stalled 256 MiB response, a second client was served after 3.1 s at a 5-second
`header_read_timeout` and after 7.0 s at a 10-second one, and process memory
returned to 1.6 MiB. INFERENCE: Hyper's header deadline reclaims a stalled
response write, which is more than the second slice documents for it, is load
bearing, and is pinned by no test.

FACT: with `max_body_memory = 4194304` and `body_read_timeout = 20`, four clients
that declared 1 MiB each and sent one byte held the whole budget; other POSTs
received 503 for the duration while GET requests were answered 200. At the 64 MiB
default the same denial costs 64 connections.

An earlier eight-client run against a 64 MiB file measured only a 0.75 MiB delta
and is recorded as a sampling failure, not a contrary result: loopback drained
those responses faster than the 250 ms sampling interval.

## 0.6.1 concrete streaming response — working tree

Full methodology and the chunk-size comparison are in
[`benchmarks/results/streaming_response_0_6_1_20260916.md`](../benchmarks/results/streaming_response_0_6_1_20260916.md).
Windows 11, Core i5-12400, 32 GiB, MSVC release builds were used throughout.

The slow-reader probe independently reproduced the old result: a 256 MiB file
cost 256.57 MiB private memory with one client and 1026.36 MiB with four. With
128 KiB streaming frames the deltas were 0.34, 1.37, 4.96 and 9.39 MiB at 1, 4,
16 and 32 clients. The 32-client process returned to within 1.20 MiB of idle
after disconnect.

Three interleaved fresh-server release runs per cell:

| workload | baseline RPS / CPU us | streaming RPS / CPU us |
|---|---:|---:|
| 21 B static, C1 | 4,167 / 243.750 | 4,553 / 244.271 |
| 21 B static, C32 | 7,358 / 426.250 | 8,669 / 438.750 |
| 1 MiB static, C8 | 2,492 / 1,620.312 | 3,266 / 1,378.125 |

The 1 MiB p50 improved from 2.833 to 2.026 ms and p95 from 4.447 to
3.513 ms; p99 was 18.024 versus 18.264 ms and is inconclusive. Every candidate
RPS sample exceeded its paired baseline. These loopback, cache-warmed numbers
show the cost of Hamster's data path, not physical-disk throughput.

64, 128 and 256 KiB chunks were measured rather than assumed. At 32 slow
clients their private deltas were 5.34, 9.39 and 17.59 MiB. On the 1 MiB cell,
64 KiB paid the highest CPU from sixteen body reads, while 256 KiB retained 87%
more memory than 128 KiB without higher throughput. The implementation retains
128 KiB.

## 0.6.2 response write liveness — working tree

Full methodology and raw samples are in
[`benchmarks/results/response_liveness_0_6_2_20260916.md`](../benchmarks/results/response_liveness_0_6_2_20260916.md).

With `max_connections = 1`, a 256 MiB response, a non-reading client and a
two-second `write_progress_timeout`, the waiting healthy connection completed
after 2008.3 ms. The expected timeout produced zero stderr bytes. A separate
client reading about 36 KiB/s stayed connected for six seconds, three complete
timeout intervals.

Release A/B on the 22-byte loopback static path found no material C32
regression: median RPS was 9,030 baseline and 9,089 candidate, with 408.125 and
400.625 CPU us/request. Six C1 samples produced 4,546 versus 4,444 median RPS
and 245.938 versus 249.062 CPU us/request. The C1 point estimate is a 2.2% RPS
loss and 1.3% CPU increase, but within-arm RPS spread was about 10% and paired
signs were unstable after reversing run order. This supports “possible
low-single-digit C1 cost,” not a zero-cost claim or a demonstrated material
regression.

## Application request metadata / protocol v2 — working tree

Full raw samples and baseline construction are recorded in
[`APPLICATION_REQUEST_METADATA.md`](APPLICATION_REQUEST_METADATA.md). Windows
11, Core i5-12400, 32 GiB, MSVC release builds; trivial persistent PHP hello,
C1, 1,000 warm-ups and 30,000 measured requests. Four pairs alternated a v1
snapshot with current effective-authority routing against protocol v2.

| arm | RPS samples | combined CPU us/request samples | p50 ms samples |
|---|---|---|---|
| v1 | 5609, 6288, 6332, 6369 | 216.146, 195.833, 198.438, 188.542 | 0.168, 0.149, 0.146, 0.145 |
| v2 | 5715, 5751, 6408, 6285 | 209.375, 209.375, 184.896, 195.312 | 0.167, 0.165, 0.144, 0.145 |

Paired RPS and CPU signs split two/two. The repository script's upper-middle
medians are 6,332 versus 6,285 RPS. Within-arm variance is larger than that
difference. This run shows no material regression but cannot exclude a
low-single-digit cost; it is not evidence for a zero-cost claim.

## Reverse proxy v1 — working tree

Full tables, method and raw probe data:
[`../benchmarks/results/reverse_proxy_v1_20260924.md`](../benchmarks/results/reverse_proxy_v1_20260924.md).
Windows 11, Core i5-12400, 32 GiB, MSVC release, loopback. `benches/proxy.rs`
runs a Hyper upstream and a Hyper load generator in one process and Hamster as
a separate release process; three runs per cell, medians; Hamster shares the
host's 12 hardware threads with the bench process.

| cell | direct RPS / p50 ms | proxy RPS / p50 ms | Hamster CPU us/req | Hamster private MiB (end) |
|---|---|---|---:|---:|
| GET 23 B, C1 | 33,254 / 0.025 | 11,844 / 0.069 | 58.6 | 1.75 |
| GET 23 B, C32 | 70,644 / 0.424 | 62,708 / 0.499 | 64.1 | 6.01 |
| GET 64 MiB, C1 | 29.4 / 32.4 | 27.6 / 34.2 | 49,676 | 1.84 |
| POST 64 MiB, C1 | 35.3 / 26.4 | 26.5 / 34.1 | 46,445 | 1.83 |

Stalled exchanges cost 185-252 KiB each at the default 64 KiB HTTP/1 buffer
bound, independent of payload size. Raising `server.http1_max_buffer_size` to
128 KiB or more collapsed proxied large transfers on this host (417,792 bytes:
2.2 RPS, ~400 ms median, lower CPU); the collapse follows whichever reading
side is backpressured, and neither `TCP_NODELAY` nor a 1 ms timer resolution
changed it. A p95 of ~13 ms remained for proxied 1 MiB C1 at the default. A
follow-up investigation (same file, *Windows loopback send stall*) traced both
effects to one Windows loopback TCP send stall that also appears on direct
connections at a lower rate and reproduces without Hamster, Hyper or Tokio;
64 KiB stays as a measured conservative Windows default, not an optimum. A
four-pass interleaved static A/B against
`a7a31b4` showed pairwise deltas of -7.8, +0.4, +0.6 and -1.2% while the host
drifted 27%; no regression is detected, and one below about 8% would not be.

## TLS v1 — working tree

Full tables and method:
[`../benchmarks/results/tls_v1_20260925.md`](../benchmarks/results/tls_v1_20260925.md).
Windows 11, Core i5-12400, 32 GiB, MSVC release, loopback, Balanced plan.
`benches/tls.rs` runs Hamster as a separate release process with a cleartext
and a TLS listener on one static root, and a Hyper/rustls load generator in the
bench process; three runs per cell, medians. Session-to-session noise reached
20%, so TLS is compared with cleartext inside one table only.

| cell (final binary) | cleartext | TLS |
|---|---:|---:|
| keep-alive GET 23 B, C1, RPS | 5,705 | 5,816 |
| keep-alive GET 1 MiB, C1, MiB/s | 1,298 | 658 |
| keep-alive GET 64 MiB, C1, MiB/s / Hamster CPU per request | 1,580 / 43.2 ms | 1,038 / 64.7 ms |
| new connection per request, C1, conn/s / Hamster CPU per connection | 1,668 / 344 us | 651 / 786 us |
| idle keep-alive connection | 25.5 KiB | 38.5 KiB |
| stalled 1 GiB download, per connection (N=64) | 290 KiB | 243 KiB |
| 1 GiB to a 7 MiB/s reader, Hamster private max | 1.94 MiB | 2.23 MiB |

A four-round alternating A/B of the pre-TLS release binary against the TLS
build without a TLS listener detected no cleartext throughput, latency or CPU
regression at about 10% resolution (new connections at C8 were 0-5% lower in
every round, inside the noise). It did find about 4 KiB more per idle
cleartext connection while the TLS branch was inline in the connection task;
boxing that branch restored 25.3 KiB before versus 25.5 KiB after. The release
binary grew from 2,945,024 to 5,318,656 bytes.
