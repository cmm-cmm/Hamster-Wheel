# Static Opened-Handle Containment A/B (2026-09-21)

Old: `HEAD` (4b9c564), pathname model. New: working tree, Windows opened-handle
model. Both `cargo build --release --target x86_64-pc-windows-msvc`.

Host: Intel Core i5-1235U (10 cores, 12 logical), 15.7 GiB RAM, Windows 11,
laptop, not isolated. Driver: `benchmarks/static_http.ps1 -Runs 1`, default
config serving a temporary `public/` directory, loopback, OS file cache warm.
Five pairs per cell; each pair runs both builds back to back and the order
alternates between pairs.

| Cell | File | Concurrency | Requests | Warm-up |
|---|---|---:|---:|---:|
| small_c1 | `index.html`, 23 B | 1 | 20,000 | 1,000 |
| small_c32 | `index.html`, 23 B | 32 | 200,000 | 1,000 |
| mib_c1 | 1 MiB random | 1 | 2,000 | 200 |
| mib_c32 | 1 MiB random | 32 | 6,000 | 200 |

## Raw samples

RPS / P50 ms / P99 ms / server CPU microseconds per request.

| Cell | Pair | Old | New |
|---|---:|---|---|
| small_c1 | 1 | 1194 / 0.794 / 1.594 / 857.0 | 1915 / 0.494 / 1.200 / 537.5 |
| small_c1 | 2 | 1644 / 0.586 / 1.144 / 607.0 | 2167 / 0.458 / 0.747 / 445.3 |
| small_c1 | 3 | 1805 / 0.505 / 1.049 / 603.9 | 2462 / 0.364 / 0.840 / 416.4 |
| small_c1 | 4 | 1424 / 0.664 / 1.313 / 730.5 | 2182 / 0.446 / 0.851 / 470.3 |
| small_c1 | 5 | 1180 / 0.812 / 1.356 / 873.4 | 1829 / 0.525 / 1.024 / 557.8 |
| small_c32 | 1 | 2282 / 13.793 / 17.968 / 1775.6 | 3195 / 9.840 / 14.102 / 1051.9 |
| small_c32 | 2 | 2002 / 15.742 / 22.704 / 1984.8 | 3058 / 10.324 / 14.733 / 1120.7 |
| small_c32 | 3 | 1957 / 17.199 / 22.612 / 1970.6 | 2515 / 12.536 / 17.675 / 1254.1 |
| small_c32 | 4 | 2550 / 15.934 / 21.173 / 1504.4 | 2495 / 12.619 / 17.951 / 1292.2 |
| small_c32 | 5 | 4283 / 7.138 / 15.989 / 923.9 | 4030 / 5.551 / 16.244 / 821.9 |
| mib_c1 | 1 | 502 / 1.662 / 4.808 / 2007.8 | 541 / 1.507 / 5.043 / 1781.3 |
| mib_c1 | 2 | 659 / 1.394 / 3.304 / 1539.1 | 359 / 2.648 / 5.072 / 2718.8 |
| mib_c1 | 3 | 384 / 2.474 / 6.063 / 2703.1 | 341 / 2.692 / 5.526 / 2765.6 |
| mib_c1 | 4 | 319 / 2.970 / 4.749 / 2945.3 | 370 / 2.565 / 4.449 / 2570.3 |
| mib_c1 | 5 | 389 / 2.675 / 5.290 / 2468.8 | 373 / 2.545 / 4.228 / 2453.1 |
| mib_c32 | 1 | 1271 / 24.246 / 50.367 / 5679.7 | 1035 / 29.289 / 61.536 / 7059.9 |
| mib_c32 | 2 | 767 / 42.512 / 67.341 / 10013.0 | 841 / 37.928 / 64.319 / 8768.2 |
| mib_c32 | 3 | 1064 / 29.475 / 56.310 / 6802.1 | 1187 / 25.304 / 53.685 / 6221.4 |
| mib_c32 | 4 | 1307 / 23.524 / 47.538 / 5619.8 | 1084 / 29.262 / 54.263 / 6697.9 |
| mib_c32 | 5 | 1265 / 24.211 / 53.608 / 5760.4 | 1418 / 21.335 / 49.221 / 4599.0 |

## Summary

| Cell | Old median RPS (range) | New median RPS (range) | Per-pair new/old RPS | Old / new median CPU us/req |
|---|---|---|---|---|
| small_c1 | 1424 (43.9%) | 2167 (29.2%) | +60.4, +31.8, +36.4, +53.2, +55.0 | 730.5 / 470.3 |
| small_c32 | 2282 (101.9%) | 3058 (50.2%) | +40.0, +52.7, +28.5, -2.2, -5.9 | 1775.6 / 1120.7 |
| mib_c1 | 389 (87.4%) | 370 (54.1%) | +7.8, -45.5, -11.2, +16.0, -4.1 | 2468.8 / 2570.3 |
| mib_c32 | 1265 (42.7%) | 1084 (53.2%) | -18.6, +9.6, +11.6, -17.1, +12.1 | 5760.4 / 6697.9 |

## Interpretation

The host was noisy: run-to-run ranges reach 102%, and CPU per request for a
23-byte file is far above what an idle machine should show. Absolute numbers
are not comparable with earlier records in `doc/BENCHMARKS.md`.

- small_c1: every pair favors the new build and the CPU-per-request ranges do
  not overlap (old 604-873 us, new 416-558 us). This is the only cell with a
  consistent direction. A plausible cause is that one blocking-pool task now
  replaces four (`metadata`, `canonicalize`, `open`, handle `metadata`); that
  cause was not measured separately.
- small_c32: three pairs favor the new build and two slightly favor the old.
  Direction is suggestive, not established.
- mib_c1 and mib_c32: signs alternate and the spread exceeds the difference.
  This evidence does not show a material regression or an improvement for
  1 MiB responses.

No zero-cost claim is made. The final-path query is an added system call per
request; on this host its cost was not separable from noise.
