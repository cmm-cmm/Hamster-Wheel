# PMS Octane lifecycle measurements — 2026-09-15

These are additive 0.4 investigation results, not a release baseline and not a
reinterpretation of earlier entries in `doc/BENCHMARKS.md`.

Machine: Windows 11 build 26200, Intel Core i5-12400 (6 cores / 12 logical),
32 GiB RAM. Runtime: release `x86_64-pc-windows-msvc` Hamster, PHP 8.5.10 NTS
with CLI OPcache, Laravel 12.66.0 PMS commit
`552e27525b1161ed10b36bdd726d203ecfa3ccff`, Laravel Octane 2.19.1. The PMS
was an independent scratch clone with `artisan optimize`; session/cache/queue
were file/array/sync so the tests did not use production Redis. Its real MySQL
configuration was used read-only for auth selection and dashboard requests.

The `fresh` candidate rebuilds the Laravel application for every request inside
the same persistent PHP process. PHP, OPcache, Rust HTTP and protocol-v1 IPC
therefore stay warm. This is an optimistic bootstrap-cost baseline, not a full
PHP-FPM emulation, because arbitrary PHP extension/static state does not receive
an SAPI request shutdown. Both candidates used the same worker count,
concurrency, routes, optimization cache and response consumption.

Light and login cells: three fresh server instances per candidate, four PHP
workers, concurrency 16, 500 sequential warm-up requests, at least 5,000
measured requests and at least four measured seconds. Dashboard: three fresh
server instances, W1/C1, 10 warm-up and 100 measured authenticated requests per
run because the route performs many queries against remote MySQL.

| Workload | Fresh median RPS | Octane median RPS | Gain | Fresh p50 | Octane p50 |
|---|---:|---:|---:|---:|---:|
| controller JSON | 751.1 | 1137.0 | 1.51x | 20.609 ms | 13.209 ms |
| Blade login | 623.1 | 931.3 | 1.49x | 24.637 ms | 15.832 ms |
| authenticated dashboard | 3.28 | 3.54 | 1.08x | 301.828 ms | 273.423 ms |

The exact multiplier is lower-confidence than the direction: persistent light
RPS ranged 1044.2–1150.4 and login 865.2–976.5. The corresponding fresh ranges
did not overlap, so this batch supports a persistent advantage, but host and
remote-database variance are material. The dashboard p50 ranges did not overlap
either, while its second fresh run had a 631 ms p99; it supports only a modest
benefit claim.

The W1 soak used 500 warm-up auth/anonymous pairs followed by 10,000 measured
pairs (20,000 requests). Every authenticated response returned the seeded PMS
user, every immediately following anonymous response redirected to login, and
one PID served the entire measured period. RSS was 70,213,632 bytes at the
first sample, settled at 70,459,392, and stayed exactly there from request
8,000 through 20,000. Least-squares RSS slope over the second half was
0 B/request. A final termination-listener fault returned 502, appended its
execution marker once, retired PID 21268, booted PID 2912, and the replacement
served 200. No measured worker remained after shutdown.

Reproduction:

```powershell
./benchmarks/prepare_laravel_pms_0_4.ps1 -SourcePmsPath ./www/pms `
  -ScratchPath $env:TEMP/hamster-pms-octane -EnvironmentFile ./www/pms/.env `
  -IgnorePhpPlatformRequirement
cargo build --release --target x86_64-pc-windows-msvc
./benchmarks/laravel_pms_0_4.ps1 -PmsPath $env:TEMP/hamster-pms-octane `
  -Candidate all -Workload all -Runs 3 -Requests 5000 `
  -WarmupRequests 500 -MinimumSeconds 4 -Workers 4 -Concurrency 16
./benchmarks/laravel_pms_0_4.ps1 -PmsPath $env:TEMP/hamster-pms-octane `
  -Candidate all -Workload dashboard -Runs 3 -Requests 100 `
  -WarmupRequests 10 -Workers 1 -Concurrency 1
./benchmarks/laravel_pms_soak_0_4.ps1 -PmsPath $env:TEMP/hamster-pms-octane `
  -Pairs 10000 -WarmupPairs 500 -SampleEveryPairs 250
```

The platform-requirement switch is recorded, not endorsed: PMS pins
PhpSpreadsheet 1.30.6, whose Composer requirement is PHP `>=7.4.0 <8.5.0`.
Octane 2.19.1 itself supports PHP 8.5. The owner must update or otherwise
resolve that PMS dependency before treating PHP 8.5 deployment as supported.
The preparation script was rerun from an empty directory and produced the
expected 133-package Laravel 12.66.0 graph; that scratch then passed both the
25-check ownership test and the Rust W1 auth/failure test.

Raw per-run and per-sample values are in
`laravel_pms_octane_0_4_20260915.csv` and
`laravel_pms_octane_soak_0_4_20260915.csv`.
