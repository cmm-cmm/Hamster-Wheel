# Windows: Laravel, CPU-heavy PHP, large bodies and overload

Supplement to the [2026-11-01 baseline](frankenphp_windows_20261101.md).
The same release EXE, host, isolated PHP 8.5.12-1 NTS and FrankenPHP 1.13.4
TS packages are used; see that report for hashes. No production runtime code
was changed for these measurements. No real application database is used.

## Method and workload definitions

Three fresh processes per arm, H F F H H F, 1,000 warm-up requests at
min(concurrency, 8), then ten seconds generating requests with oha `-w`.
Every supplemental process gets a body/status preflight and a normal
response/digest check after the load, outside the timed resource window.
That is not a per-request body assertion or an at-most-once fault test.

Heavy PHP uses the same pure function in
`benchmarks/fixtures/heavy_workload.php` through native protocol and FrankenPHP
adapters. SHA-256 of that function file:
`96CDDC68A387E689BA1057E356F6E383EA3496858C727BCD4D36CD68F441113F`.

- CPU: 20,000 chained SHA-256 hex hashes starting at 64 ASCII `a` bytes;
  64-byte response, W4/C16. Python independently computes the reference digest.
- Large POST: exactly 1 MiB of ASCII `x`, SHA-256 digest response, W4/C16.
  `oha -D` supplies a file; a megabyte is never put on the Windows command line.
  This measures upload/body reconstruction/IPC/PHP hashing, not a 1-MiB response.
- High concurrency: 14-byte Hello response, W4/C64 and W4/C128. Hamster keeps
  queue_capacity=64 and the default aggregate body budget; no queue is enlarged
  to avoid overload. FrankenPHP keeps four workers and five spare classic
  threads as in the earlier worker comparisons.

Heavy cells use minimal INI in both arms. Laravel has a separate INI with
required extensions and OPcache enabled in both CLI and embed SAPIs. The driver
now keeps minimal and Laravel INI paths separate even in a combined invocation.

**Goodput** = completed HTTP 200 / measured elapsed. HTTP attempts/sec includes
503 and is not PHP throughput. CPU/200 below charges all server CPU, including
rejections, to completed successes; it is not pure PHP execution cost.
Latencies at C128 include both 200 and 503; successful-response P99 is NOT
available from these rows. No production SLO is inferred from mixed percentiles.

## Heavy PHP results

Raw: [24 runs](frankenphp_windows_20261101_heavy.jsonl).
**1,162,908 measured attempts: 573,648 HTTP 200 and 589,260 HTTP 503;
zero transport errors and no other statuses.** All 24 post-load response checks
passed. All 503 responses came from Hamster's C128 overload cell.

### Successful RPS, min / median / max

| Workload | Hamster goodput | FrankenPHP goodput |
| --- | ---: | ---: |
| CPU SHA256×20000 W4/C16 | 232 / 235 / 236 | 219 / 223 / 225 |
| POST 1 MiB digest W4/C16 | 508 / 514 / 517 | 145 / 146 / 165 |
| Hello W4/C64 | 6487 / 6916 / 6926 | 3726 / 3885 / 3933 |
| Hello W4/C128 | 3487 / 3810 / 3836 | 3488 / 3580 / 3749 |

### Independent median latency/resources (H / F)

CPU is whole-server-tree µs per successful 200, memory MiB at end of run.

| Cell | P50 ms | P95 ms | P99 ms | CPU/200 µs | Working set | Private bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| CPU W4/C16 | 67.90 / 71.50 | 75.06 / 79.36 | 78.11 / 87.26 | 17386 / 19015 | 109.95 / 68.13 | 39.75 / 98.20 |
| POST 1 MiB W4/C16 | 30.80 / 102.11 | 35.90 / 138.90 | 40.47 / 230.98 | 13730 / 23784 | 136.37 / 73.72 | 62.36 / 99.89 |
| Hello W4/C64 | 9.20 / 15.82 | 11.14 / 28.48 | 12.29 / 40.39 | 984 / 1725 | 111.19 / 77.52 | 41.61 / 107.46 |
| Hello W4/C128 (mixed statuses) | 2.61 / 35.69 | 21.70 / 53.38 | 24.94 / 77.54 | 1642 / 1803 | 111.44 / 86.23 | 42.00 / 117.37 |

### Interpretation

- CPU-heavy PHP: the advantage shrinks to about 1.05× median, not the Hello
  ratio. Hamster-process CPU is about 897 µs/request against 17,386 for the
  whole tree (~5%). Removing dispatch/HTTP work cannot remove the hashing work;
  this does not justify a dispatcher redesign.
- Large POST: Hamster-process CPU is about 9,496 µs/request against 13,730
  whole-tree (~69%). This identifies a useful **profiling candidate**, not
  proof that a particular allocation or pipe syscall is the cause.
- C128: Hamster completed 111,616 requests and refused 589,260 across three
  runs (~84% refusal); FrankenPHP completed 108,648 with no 503. Hamster's
  headline total HTTP rate of ~23k/s is **not** 23k PHP executions/sec.
- Hamster median goodput falls from 6916 at C64 to 3810 at C128. Fast 503s
  make a closed-loop client send more attempts; offered request rates therefore
  differ substantially. A fixed-arrival-rate follow-up with latency split by
  status is needed before attributing the change to admission CPU or changing
  queue policy. The goodput ranges at C128 overlap: no robust winner claim.
- End-of-run memory is not peak memory or a soak/leak result. A normal response
  after load is recovery evidence, not proof of every lifecycle invariant.

## Laravel application and initial diagnostic attempts

The trusted local skeleton is Laravel **12.69.2** with Octane **2.20.0**.
Original composer.lock SHA-256:
`6832E8B07471AE7B045AC9CEEB29C9033F00AD2B567C5FD3E410D8F2D45BFA03`.
The driver copies app/config/resources/public/vendor into its own temporary
directory, not the source .env/storage/database/bootstrap caches. It registers
only AppServiceProvider and two scratch routes: a 17-byte `/login` placeholder
and the default Blade welcome page `/`. Config and views are cached with the
isolated NTS PHP before both arms. Session/cache are arrays, queue sync, DB
SQLite `:memory:`; no MySQL/Redis/PMS resources are used.

Hamster uses its Octane adapter with four workers, 30-second startup budget
and unchanged default request execution timeout. FrankenPHP uses Octane's
FrankenPHP worker stub, four workers and `num_threads 5` spare classic threads.
This is a framework kernel/middleware/view benchmark, not a login/auth/database
or real PMS workload. No number here qualifies a real application's capacity.

The first combined attempt produced a Hamster **504 on the first preflight**,
before any timed row. It was not retried transparently. Its original stderr
was discarded by the old driver, so its cause is **unresolved**; later success
does not erase it. A separate orchestration-only diagnostic served the correct
Hamster response but FrankenPHP never reached readiness.

A defect in this pass's new empty-environment handling was then isolated with
the actual Caddy parser: generated
`env DB_URL` (empty value omitted) fails adaptation; `env DB_URL ""` succeeds.
Values are now quoted, covered by a regression. Readiness also checks a retained
child's exit and reports its code/log instead of waiting for a dead process.
Per-process stdout/stderr are retained under ignored OUTPUT.logs directories.
This invalid Caddyfile is not evidence of a FrankenPHP native crash. An earlier
malformed scratch parser probe failed both arms and is not causal evidence;
the valid before/after probe is `target/tools/benchmark-20261101/caddy-env-valid-probe.json`.

## Laravel measured results

Raw: [24 corrected runs](frankenphp_windows_20261101_laravel.jsonl).
**110,490 measured requests, all HTTP 200, zero transport errors**, all 24
post-load response checks passed. Initial failed attempts and the quarter-
second diagnostic are not performance data. The body is 17 bytes for `/login`
and 81,277 bytes for welcome in both arms; the client does not load page assets.

### RPS min / median / max

| Route / concurrency (W4) | Hamster | FrankenPHP |
| --- | ---: | ---: |
| `/login` C1 | 119 / 119 / 166 | 111 / 156 / 358 |
| `/login` C16 | 537 / 617 / 1065 | 448 / 451 / 1033 |
| Welcome C1 | 190 / 229 / 233 | 251 / 255 / 260 |
| Welcome C16 | 748 / 753 / 761 | 709 / 722 / 732 |

### Independent median latency/resources (H / F)

| Cell | P50 ms | P95 ms | P99 ms | CPU µs/req | Working set MiB | Private MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `/login` C1 | 8.08 / 6.06 | 10.51 / 9.85 | 13.33 / 13.38 | 8148 / 7080 | 244.93 / 116.87 | 87.39 / 120.72 |
| `/login` C16 | 26.02 / 31.59 | 36.45 / 38.12 | 48.85 / 77.36 | 6527 / 9120 | 249.65 / 112.86 | 95.27 / 117.58 |
| Welcome C1 | 4.17 / 3.66 | 5.86 / 5.37 | 7.33 / 7.56 | 4355 / 4384 | 266.37 / 125.86 | 110.34 / 129.55 |
| Welcome C16 | 20.99 / 21.52 | 24.90 / 26.88 | 27.00 / 39.21 | 5927 / 6539 | 270.00 / 133.70 | 112.10 / 140.48 |

FrankenPHP is faster in all three measured Welcome C1 runs: Hamster's median
is about 10% lower, despite the favorable Hello results. Welcome C16 instead
slightly favors Hamster (~4% median); `/login` ranges overlap heavily and do
not support precise comparative gains. This is a useful framework-specific
profiling direction, not proof that reset/reconstruction is the cause. The
isolated first-preflight 504 remains unresolved cold-start evidence even though
the corrected warm-load matrix passed; do not claim it has been fixed.

## Reproduce and verification boundary

```powershell
python -B benchmarks/frankenphp_windows.py `
  --frankenphp target/tools/benchmark-20261101/frankenphp/frankenphp.exe `
  --oha target/tools/benchmark-20261101/oha.exe `
  --php-dir target/tools/benchmark-20261101/php-nts `
  --duration 10 --output target/tools/benchmark-20261101/heavy-new.jsonl heavy

python -B benchmarks/frankenphp_windows.py `
  --frankenphp target/tools/benchmark-20261101/frankenphp/frankenphp.exe `
  --oha target/tools/benchmark-20261101/oha.exe `
  --php-dir target/tools/benchmark-20261101/php-nts --laravel-source www/laravel `
  --duration 10 --output target/tools/benchmark-20261101/laravel-new.jsonl laravel
```

Output must be a new file. stdout/stderr logs remain in ignored OUTPUT.logs;
the [valid parser before/after](frankenphp_windows_20261101_caddy_env_probe.json)
is persisted separately. Diagnostic files are not throughput samples.
PHP fixture lint and seven driver regressions passed. The preceding Windows
default 314 tests/fmt/strict Clippy/release identity evidence remains applicable:
Rust production code and the release binary did not change in this supplement.
No full ignored framework/lifecycle suite, fault injection, leak/peak-memory
qualification or Linux run is claimed. Cleanup audit found no matching live
process among all 604 recorded PID/creation identities including the preceding
baseline and diagnostic, zero fixture-path processes, benchmark ports or owned
temporary roots. Logs/tools remain in ignored cache. Source `www/laravel` and
its caches/data were not altered. Release EXE hash is unchanged.
