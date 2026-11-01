# Hamster Wheel / FrankenPHP — native Windows, PHP 8.5.12-1

## Conditions and artifact identity

Measured on 2026-11-01, native Windows 11 Home Single Language 10.0.26300,
Intel Core i5-1235U (10 physical / 12 logical CPUs). Same-host loopback HTTP/1.1
keep-alive, no TLS, no access logging, no host policy/service changes. This is
not a dedicated benchmark host. Results are for these fixtures, not a framework
or production workload qualification; no Linux measurements were made.

Hamster is the MSVC release build of 0.8.0, tree `db5c4726f53c` plus the current
icon/build/test/driver changes, stamp `v0.3.1-56-gdb5c4726f53c-dirty`.
Executable SHA-256:
`AF03B0A8E4FD7F7BDBF2C9D1ECA8C8EBC2E988AA7971C4A730C227307F822877`.
Its new Concept 02 icon contains 16/24/32/48/64/128/256-pixel images, all
compared byte-for-byte with the executable resources by the release identity
test (the Windows SDK normalizes the directory's zero plane count to one).
ICO SHA-256:
`16E2795C20B108147B9A91F8FC3DEF16AD5A93FBCF0DE859193A7EC5AD427AC2`.

Runtime distributions were staged separately under ignored
`target/tools/benchmark-20261101/`; neither `php/8.5` nor system PHP was replaced.
Downloads were checked against the official published checksums:

| Package | SHA-256 |
| --- | --- |
| PHP 8.5.12-1 NTS VS17 x64 ZIP | `2a36a2edcb7a5b9252f638fd5e4cf84d7f6e066092654220dc0b63e5cf4e9723` |
| PHP 8.5.12-1 TS VS17 x64 ZIP (identity reference) | `0dacd6484b092b5e9ae9fa3d7f96a937caf9d874eee2d980b68ed3129741144a` |
| FrankenPHP 1.13.4 Windows x64 ZIP | `6022fcc180e9a3575c637dd0dc0aba8d23f78e14ac2a0e46f1f914535194be14` |
| oha 1.16.0 Windows amd64 executable | `d692d392e958e7e8e1b16810287e74a835a62255b079ba2766a4b3b04499f267` |

Sources: [PHP Windows downloads](https://www.php.net/downloads.php?os=windows),
[FrankenPHP release](https://github.com/php/frankenphp/releases/tag/v1.13.4),
[oha release](https://github.com/hatoo/oha/releases/tag/v1.16.0).
FrankenPHP reports **1.13.4 / PHP 8.5.12 / Caddy 2.11.7**. Its `php8ts.dll`
matches the official PHP **8.5.12-1 TS** archive byte-for-byte:
`2cad79b8aca7ccb8051d24e76ee394a1bf10f6f1fb0a0698ec570f4e6f4cbd5b`.
The Windows package revision `-1` is not included in PHP_VERSION. FrankenPHP
ships its own different Brotli DLLs; the entire distribution is identified by
its own ZIP digest, not claimed identical to the PHP TS ZIP.

## Method

Driver: `benchmarks/frankenphp_windows.py`; 13 existing cells, each with six
fresh processes ordered H F F H H F, three runs per server. Each measurement
lasts 10 seconds; `oha -w` waits for requests in flight at the deadline rather
than reporting deliberate deadline aborts as server errors. Warm-up: 1,000
requests per process (200 for classic), concurrency min(cell concurrency, 8).
The quarter-second smoke run checked orchestration only and is **not** used as
performance data.

PHP workers: W1 and W4. Classic Hamster: four separately started NTS php-cgi
endpoints, one connection slot each, kept connections/default script cache;
FrankenPHP classic: `num_threads 4`. Worker cells preserve the existing driver's
`worker.num = W` and `num_threads = W + 1` configuration. Important version
distinction: in [FrankenPHP 1.13.4](https://github.com/php/frankenphp/blob/v1.13.4/docs/config.md),
`num_threads` denotes non-worker threads and the W workers are additional;
this is not a claim that total PHP threads equal W + 1. The W1/W4 request paths
still use one/four persistent workers, but spare classic threads are included
in measured process memory. These are tested configurations, not a search for
each server's minimum-memory or best-tuned configuration.
Both arms receive scratch php.ini/PHPRC; external INI scanning disabled in child
environments. Minimal fixtures use the same explicit memory/log settings;
Laravel/extensions/OPcache tuning is not added to this matrix. No real
application, database or external service is used.

CPU is server-tree user+kernel time (including Hamster's four external CGI
processes for classic), divided by **actual** measured requests, not the
request-count plan used in older untimed runs. Memory is sampled after each
run: summed working set can count shared pages repeatedly; private bytes do
not. Hidden console-host descendants are included where present, consistently
with the process-tree method. CPU quantization and same-host client scheduling
remain limitations. This closed-loop saturation test is not a rate-controlled
tail-latency/SLO measurement.

Reproduce after building release and ensuring ports 8080 and 9101–9104 are free:

```powershell
python -B benchmarks/frankenphp_windows.py `
  --frankenphp target/tools/benchmark-20261101/frankenphp/frankenphp.exe `
  --oha target/tools/benchmark-20261101/oha.exe `
  --php-dir target/tools/benchmark-20261101/php-nts `
  --duration 10 --output target/tools/benchmark-20261101/measurement-new.jsonl all
```

The output path must be new; existing evidence is never overwritten. Temporary
roots/configs/worker/INI files are created and removed by the driver. Cleanup
terminates only retained subprocess objects this driver started, never a
process-name match or a guessed parent-PID tree. Per-run identity/creation time
is evidence, not authority to kill a process later.

## Results

Raw [78-run matrix](frankenphp_windows_20261101.jsonl) and
[six-run W4/C4 follow-up](frankenphp_windows_20261101_w4c4_repeat.jsonl) are
retained. The matrix completed **3,834,582 measured requests**, zero client
transport errors and zero unexpected statuses. Warm-up requests are additional.
The small static file is **23 bytes** on this checkout (CRLF); the raw driver's
historical `static 22 B` label is nominal, not the actual on-disk length.

### RPS spread (min / median / max)

Ratios divide the two medians within the same cell. They are observations, not
confidence intervals or guarantees. Worker capacity is not compared at matching
concurrency across W1/W4, so this matrix does not establish scaling efficiency.

| Cell | Hamster RPS | FrankenPHP RPS | H/F median |
| --- | ---: | ---: | ---: |
| not_found C1 | 6455 / 6521 / 7460 | 3953 / 4892 / 5281 | 1.33 |
| not_found C32 | 32146 / 34513 / 34647 | 29006 / 30647 / 32845 | 1.13 |
| static 23 B C1 | 1372 / 1390 / 1446 | 1102 / 1142 / 1267 | 1.22 |
| static 23 B C32 | 3074 / 3092 / 3104 | 2772 / 3052 / 3134 | 1.01 |
| static 64 KiB C32 | 2730 / 2882 / 2990 | 2584 / 2602 / 2682 | 1.11 |
| static 1 MiB C8 | 1320 / 1367 / 1367 | 804 / 819 / 822 | 1.67 |
| worker hello W1 C1 | 1370 / 1519 / 1630 | 716 / 867 / 872 | 1.75 |
| worker json W1 C1 | 2020 / 2043 / 2092 | 830 / 1000 / 1049 | 2.04 |
| worker echo 12 B W1 C1 | 1235 / 1400 / 1662 | 887 / 944 / 1017 | 1.48 |
| worker hello W4 C4 | 4608 / 4649 / 4873 | 826 / 2901 / 2945 | 1.60 |
| worker hello W4 C32 | 6041 / 6979 / 9989 | 2628 / 5148 / 6009 | 1.36 |
| classic hello 4 C1 | 741 / 744 / 805 | 371 / 517 / 854 | 1.44 |
| classic json 4 C16 | 3313 / 3602 / 3622 | 1119 / 1716 / 5039 | 2.10 |

### Latency and server-tree resources

Each field below is independently the median of the three runs, **not** the
field from the median-RPS run. Console summaries retain the older median-RPS-
run convention; the raw evidence makes both reproducible. Latencies are ms;
CPU is microseconds/request; memory is MiB. Paired resource columns are H / F.

| Cell | H P50 / P95 / P99 | F P50 / P95 / P99 | CPU µs/req H / F | WS H / F | Private H / F |
| --- | ---: | ---: | ---: | ---: | ---: |
| not_found C1 | 0.13 / 0.26 / 0.36 | 0.18 / 0.37 / 0.50 | 84.15 / 245.23 | 15.23 / 52.70 | 3.24 / 68.06 |
| not_found C32 | 0.88 / 1.54 / 1.92 | 0.59 / 3.32 / 7.57 | 118.62 / 176.21 | 16.55 / 65.35 | 5.08 / 80.29 |
| static 23 B C1 | 0.65 / 1.07 / 1.41 | 0.83 / 1.26 / 1.56 | 660.69 / 919.44 | 15.43 / 52.97 | 3.43 / 67.65 |
| static 23 B C32 | 10.28 / 11.52 / 12.20 | 10.19 / 13.21 / 21.39 | 1216.76 / 1687.09 | 18.22 / 63.70 | 8.15 / 79.46 |
| static 64 KiB C32 | 11.04 / 12.23 / 12.95 | 11.65 / 18.94 / 24.04 | 1425.28 / 1900.05 | 18.51 / 63.97 | 8.21 / 79.24 |
| static 1 MiB C8 | 5.74 / 7.12 / 8.17 | 9.57 / 14.92 / 18.11 | 4941.36 / 6804.99 | 17.25 / 62.07 | 6.13 / 77.12 |
| worker hello W1 C1 | 0.61 / 0.99 / 1.18 | 1.06 / 1.75 / 2.05 | 807.16 / 1241.36 | 39.46 / 63.63 | 12.45 / 80.81 |
| worker json W1 C1 | 0.46 / 0.64 / 0.84 | 0.92 / 1.52 / 1.92 | 651.45 / 1079.85 | 39.39 / 64.46 | 12.35 / 81.65 |
| worker echo 12 B W1 C1 | 0.65 / 1.00 / 1.10 | 1.00 / 1.42 / 1.71 | 935.52 / 1128.84 | 39.44 / 64.45 | 12.43 / 81.03 |
| worker hello W4 C4 | 0.81 / 1.25 / 1.61 | 1.32 / 1.84 / 2.47 | 997.02 / 1329.71 | 110.64 / 69.03 | 40.65 / 98.86 |
| worker hello W4 C32 | 4.41 / 6.58 / 8.74 | 5.07 / 13.13 / 21.66 | 853.04 / 1218.32 | 111.81 / 72.38 | 42.20 / 102.53 |
| classic hello 4 C1 | 1.32 / 1.63 / 2.04 | 1.64 / 2.86 / 6.72 | 1238.71 / 1851.36 | 121.19 / 63.48 | 47.55 / 82.99 |
| classic json 4 C16 | 4.21 / 7.79 / 14.49 | 6.32 / 20.59 / 38.91 | 951.93 / 1595.85 | 124.12 / 68.80 | 50.91 / 88.45 |

### Outlier and controlled repeat

Do not discard matrix W4/C4 FrankenPHP sequence 6: 32,732 completed requests,
39.645 seconds reported elapsed, 825.6 RPS, no reported errors. The duration
option bounds request generation; `-w` may extend completion time. The first
matrix retained percentiles/elapsed but not the client's maximum latency or
full summary, so it cannot locate this delay in server, client or host.

The driver now also retains the complete oha summary and outer monotonic wall
time. A separate, unchanged-workload W4/C4 repeat (three fresh processes/arm)
completed **250,755 requests**, zero errors/unexpected statuses:

| Arm | RPS min / median / max | Client elapsed range | Outer wall range | Largest request latency |
| --- | ---: | ---: | ---: | ---: |
| Hamster | 3792 / 6032 / 7270 | 10.012–10.017 s | 10.099–10.138 s | 31.27 ms |
| FrankenPHP | 1900 / 2056 / 3991 | 10.011–10.015 s | 10.100–10.115 s | 35.95 ms |

The 39.645-second interval was **not reproduced**, but the throughput variation
was. No cause was established and no server/transport optimization was made.
Both the outlier and repeat remain in the record, not substituted or pooled.

### Interpretation and limits

- FACT: every matrix median is higher for Hamster, but the static-small C32
  ranges overlap; call that near parity, not a demonstrated improvement.
- FACT: classic JSON FrankenPHP ranges from 1119 to 5039 RPS, crossing Hamster's
  entire 3313–3622 range. Its 2.10 median ratio is **not a stable comparative
  speedup**. Classic C1 and W4 also have material spread; more controlled host
  conditions are needed for precise claims. The repeat does not resolve this.
- FACT: W1 fixture medians favor Hamster in this matrix. They do not establish
  Laravel/framework performance. The extra spare FrankenPHP threads and NTS
  versus TS runtime boundaries are explicitly part of the tested setups.
- FACT: Hamster W4 has lower private bytes but higher summed working set here.
  Do not turn either metric into an unqualified "uses less RAM" claim. These
  are end-of-run samples, not peak-memory or leak/stability measurements.
- INFERENCE: static/PHP integration costs dominate the minimal HTTP control
  in these fixtures. The run does not isolate a particular syscall, lock or
  backend phase, so it does not justify an implementation optimization.
- HYPOTHESIS: the isolated long interval could be a remaining request, client
  scheduling or host interruption. Its cause is unresolved; it is not evidence
  of a FrankenPHP-specific defect. If investigating further, preserve full
  client timings and correlate host/server traces on a controlled repeat.

This is not an A/B of the icon change or PHP patch level. The 2026-10-19 rows
use other server/runtime/host conditions; they remain historical, not a basis
for direct regression percentages. No fault/retry/lifecycle, remote-network,
TLS, real framework/database or Linux qualification is claimed by this matrix.

## Verification and cleanup

PASS: MSVC release build; release `build_identity` 3/3 (including actual icon
payloads); default Windows suite **314 passed, 0 failed, 145 ignored**; fmt;
strict MSVC Clippy all targets/all features; Python driver mock regressions
3/3; diff check. The ignored PHP/framework/lifecycle suites were not rerun and
are not counted as PASS. The existing release-build `compile_validated`
dead-code warning was not suppressed or changed for this icon task.

Cleanup audit: all 306 distinct recorded PID/creation-time identities no longer
matched live processes; no fixture-path processes, no benchmark temp roots,
and ports 8080/9101–9104 no longer listening. Cleanup itself used the retained
subprocess objects, not this later inventory or PID snapshots. Downloaded
tools/checksummed archives and ignored logs remain as reproducibility caches;
the persisted JSONL copies above contain no binary packages or private data.
No service, firewall, system PHP, real application data or global environment
was changed. No commit, tag, push, merge or release.
