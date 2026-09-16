# HTTP admission 0.5 release A/B raw results

Date: 2026-09-16

## Frozen inputs

- Baseline source: `11fd550`.
- Baseline binary SHA-256:
  `9D9BEC8C4CCA7C0AF92E4A9D1BD152FFE84337CB904BFF80DA68A944EDE14D57`.
- Working-tree candidate binary SHA-256:
  `DB0B583CE918BCBADD2A70900C58731B3C5C00CB557528EB0D8DD1AE82FD0119`.
- Rust/Cargo 1.98.1, `x86_64-pc-windows-msvc` release profile.
- Windows 11 Pro build 26200, Core i5-12400 (6C/12T), 32 GiB,
  Balanced power plan.
- Client and server shared the workstation. Every sample launched a fresh
  server. The same checked-in .NET driver and concurrency 32 were used for
  both binaries.

## Static `/`, 500,000 requests, 1,000 warm-up

| Tree/run | wall s | RPS | p50 ms | p95 ms | p99 ms | CPU s | CPU us/request | working/peak MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| baseline 1 | 72.040 | 6,941 | 4.575 | 5.187 | 5.688 | 215.375 | 430.750 | 9.7 / 9.9 |
| baseline 2 | 73.414 | 6,811 | 4.629 | 5.530 | 5.955 | 221.094 | 442.188 | 9.8 / 9.8 |
| baseline 3 | 69.739 | 7,170 | 4.401 | 5.095 | 5.593 | 218.156 | 436.312 | 9.6 / 10.0 |
| candidate 1 | 67.836 | 7,371 | 4.274 | 4.893 | 5.352 | 210.047 | 420.094 | 9.1 / 9.3 |
| candidate 2 | 68.793 | 7,268 | 4.336 | 4.992 | 5.459 | 211.188 | 422.375 | 9.5 / 9.7 |
| candidate 3 | 69.272 | 7,218 | 4.374 | 5.021 | 5.437 | 214.125 | 428.250 | 9.2 / 9.4 |

## In-memory `/api` 404, 500,000 requests, 20,000 warm-up

| Tree/run | wall s | RPS | p50 ms | p95 ms | p99 ms | CPU s | CPU us/request | working/peak MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| baseline 1 | 3.588 | 139,361 | .212 | .356 | .679 | 14.859 | 29.719 | 7.7 / 7.7 |
| baseline 2 | 3.551 | 140,788 | .209 | .344 | .671 | 13.703 | 27.406 | 7.8 / 7.9 |
| baseline 3 | 3.590 | 139,287 | .211 | .360 | .708 | 15.406 | 30.812 | 7.4 / 7.5 |
| baseline 4 | 3.548 | 140,920 | .211 | .352 | .682 | 15.125 | 30.250 | 7.4 / 7.5 |
| baseline 5 | 3.532 | 141,544 | .209 | .347 | .664 | 13.859 | 27.719 | 7.7 / 7.9 |
| baseline 6 | 3.597 | 139,022 | .212 | .356 | .682 | 13.625 | 27.250 | 8.3 / 8.3 |
| baseline 7 | 3.560 | 140,432 | .210 | .352 | .680 | 14.219 | 28.438 | 7.6 / 7.7 |
| candidate 1 | 3.720 | 134,399 | .218 | .370 | .718 | 15.812 | 31.625 | 7.4 / 7.6 |
| candidate 2 | 3.673 | 136,111 | .214 | .363 | .694 | 14.156 | 28.312 | 7.9 / 8.2 |
| candidate 3 | 3.654 | 136,842 | .216 | .368 | .685 | 14.312 | 28.625 | 7.4 / 7.6 |
| candidate 4 | 3.581 | 139,626 | .213 | .353 | .672 | 15.094 | 30.188 | 7.7 / 8.0 |
| candidate 5 | 3.590 | 139,260 | .213 | .353 | .684 | 15.156 | 30.312 | 7.9 / 8.0 |
| candidate 6 | 3.629 | 137,773 | .213 | .356 | .681 | 14.703 | 29.406 | 7.3 / 7.5 |
| candidate 7 | 3.617 | 138,239 | .215 | .358 | .685 | 14.828 | 29.656 | 7.7 / 7.8 |

Medians and interpretation are recorded append-only in `doc/BENCHMARKS.md`.
The static candidate improvement is not claimed. The in-memory control detects
an absolute +1.218 CPU microseconds/request and -1.9% median RPS cost.
