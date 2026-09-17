# Execution timeout policy — floor derivation and hot path, 2026-09-17

Measurements taken while implementing the Execution Timeout Policy slice. They
derive the `request_timeout` validation floor and gate the change's hot-path
cost. The fault-injection evidence that motivated the slice is separate, in
`worker_request_timeout_investigation_20260917.md`.

Machine: Windows 11 Pro build 26200, Core i5-12400, 32 GiB,
`x86_64-pc-windows-msvc`, PHP 8.5.10 NTS.

## Why the floor is not a matter of taste

`request_timeout` does two things at once, and the floor has to satisfy both.

1. It is the deadline for the whole transaction — the REQUEST frame write, PHP
   execution and the RESPONSE frame read. A floor below the cost of moving the
   protocol's own largest legal payload would let the deadline expire on
   Hamster's I/O and be reported as an *application* timeout, which is exactly
   the misclassification this slice exists to remove.
2. Exceeding it kills the worker, so it bounds a slot's worst-case respawn
   rate.

## F2 — cost of the worst legal payload

Procedure: single worker, `/large`, which echoes the request body. A 1 MiB
body is the largest legal request and produces the largest response that route
can return. Twelve samples over real TCP through the real pool.

| | |
|---|---:|
| median round trip | 6.2 ms |
| worst round trip | 18.1 ms |

FACT: the protocol's worst legal round trip is 18.1 ms.

INFERENCE: a one-second floor leaves the application 98.2% of the smallest
allowed deadline even in that worst case, so the deadline cannot plausibly fire
on Hamster's own work.

## F1 — respawn rate and cost at candidate floors

Procedure: single worker driven continuously for a 20-second window at each
candidate `request_timeout`, always waiting for READY so every request reaches
PHP and costs a generation. CPU is the summed CPU of the server process and its
PHP children.

| `request_timeout` | respawns | window | respawns/min/slot | CPU | CPU per respawn |
|---:|---:|---:|---:|---:|---:|
| 1 s | 19 | 20.2 s | 56.5 | 0.06 s | 3.3 ms |
| 2 s | 10 | 20.6 s | 29.2 | 0.08 s | 7.8 ms |
| 5 s | 4 | 20.2 s | 11.9 | 0.03 s | 7.8 ms |

FACT: the predicted bound `1 / (request_timeout + bootstrap)` holds. At one
second with this fixture's ~60 ms bootstrap it predicts 56.6 respawns per
minute per slot; 56.5 were measured.

A first run of this table measured zero respawns at 1 s and 2 s. That was an
instrumentation fault, not a result: the probe's generated configuration was
not carrying `request_timeout`, so all three rows ran at the five-second
default and the 3 s and 4 s sleeps simply never expired. The table above is the
corrected run.

INFERENCE: because bootstrap sits in the denominator, the fraction of a slot's
time spent bootstrapping is `bootstrap / (request_timeout + bootstrap)`.
Against the worst real-application bootstrap this repository has measured —
403 ms, `doc/BENCHMARKS.md` — a one-second floor bounds that fraction at 28.7%
and a five-second setting at 7.5%. The 28.7% case only arises when every
request is already timing out, which is an application outage by itself.

Floor selected: **1 second**. It is 55 times the worst legal protocol round
trip and it keeps the respawn duty cycle bounded and finite. Ceiling: 3600
seconds.

Operator guidance that follows from the same arithmetic, recorded in
`doc/CONFIGURATION.md`: set `request_timeout` to at least ten times the
application's bootstrap time to keep the worst-case duty cycle under 10%.

## Hot path

The success path is unchanged by construction: the same single `timeout()`
wrapper, one extra `bool` on the already-cold `TransactionFailure`, and
`request_timeout` read once at configuration compile into the existing
`WorkerTimeouts`. No allocation and no branch was added to a successful
request. The measurement below is the gate, not the argument.

Release builds of the pre-change and post-change trees, run interleaved,
20,000 requests, one worker, concurrency 1, medians of three runs.

| Scenario | Pass | Before rps | After rps | Delta |
|---|---:|---:|---:|---:|
| `php_json` | 1 | 5832 | 6276 | +7.6% |
| `php_json` | 2 | 6431 | 6216 | -3.3% |
| `php_hello` | 1 | 6359 | 6419 | +0.9% |
| `php_hello` | 2 | 6353 | 6200 | -2.4% |

p50 latency, before against after: `php_json` 0.160/0.145 ms against
0.145/0.149 ms; `php_hello` 0.147/0.146 ms against 0.144/0.147 ms.

FACT: the sign of the delta flips between passes in both scenarios, and the
within-arm spread across the three runs of a single median reached 11%
(5480-6121 rps in one `php_json` arm).

INFERENCE: no regression is detectable. As with every other measurement in this
repository, host variance dominates and this data can only fail to detect a
regression, not prove there is none. It would not detect a regression smaller
than roughly 5%.
