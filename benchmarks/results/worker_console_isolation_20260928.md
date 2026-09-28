# PHP worker console isolation — 2026-09-28

Windows 11 Pro 10.0.26200, 12th Gen Intel Core i5-12400, 31.8 GiB RAM,
loopback only. Release `x86_64-pc-windows-msvc` builds of `7ca716f` (base,
workers share Hamster's console) and of the working tree (current, workers
spawned with `CREATE_NO_WINDOW`). Pool: eight workers of
`fixtures/php-worker/worker.php` under the repository PHP 8.5.10 NTS, started
in parallel at server startup. Hamster was started hidden with stdout and
stderr redirected to files; each run read the eight `ready after` lines, then
counted `conhost.exe` processes whose parent is one of those workers and
summed private bytes, then killed Hamster (the Job Object reaps the workers).
Five runs, base and current alternating. Script: kept in the session
scratchpad, not checked in; the procedure above is complete.

`ready after` is measured by Hamster from spawn to a validated READY frame, so
it includes process creation.

| Binary | Run | Median READY ms | Max READY ms | conhost.exe | conhost private MiB | php.exe private MiB |
|---|---:|---:|---:|---:|---:|---:|
| base | 1 | 94.09 | 96.11 | 0 | 0.00 | 61.16 |
| current | 1 | 101.59 | 108.61 | 8 | 9.40 | 61.19 |
| base | 2 | 88.33 | 90.81 | 0 | 0.00 | 61.19 |
| current | 2 | 104.18 | 112.76 | 8 | 9.40 | 61.23 |
| base | 3 | 86.38 | 90.91 | 0 | 0.00 | 61.23 |
| current | 3 | 104.20 | 108.43 | 8 | 9.41 | 61.29 |
| base | 4 | 86.64 | 91.72 | 0 | 0.00 | 61.23 |
| current | 4 | 110.52 | 119.37 | 8 | 9.39 | 61.17 |
| base | 5 | 80.38 | 86.34 | 0 | 0.00 | 61.25 |
| current | 5 | 106.41 | 109.42 | 8 | 9.37 | 61.09 |

Median of the run medians: base 86.6 ms, current 104.2 ms (+17.6 ms, about
+20%). Every current run is slower than every base run, so the direction is
not noise; the size is one machine and one fixture. Memory: one `conhost.exe`
per worker, 9.4 MiB for eight (about 1.2 MiB each); PHP's own private memory is
unchanged. After each kill no `conhost.exe` parented by a worker remained.

Scope: this is a spawn cost, paid at startup and at each replacement or
recycle. Request handling does not touch the console and was not re-measured.
A 58-403 ms Laravel bootstrap (recorded in `doc/LARAVEL_WORKER_0_4.md`) makes
the added time proportionally smaller there; that was not measured. Native
Linux: not applicable (the flag is Windows-only) and NOT VERIFIED.
