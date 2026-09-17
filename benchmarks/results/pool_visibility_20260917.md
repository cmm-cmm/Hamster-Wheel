# Pool identity and degradation visibility — 2026-09-17

Measurements taken while investigating what an operator can actually conclude
about a persistent pool from the runtime state that already exists.

Machine: Windows 11 Pro build 26200, Core i5-12400, 32 GiB,
`x86_64-pc-windows-msvc`, PHP 8.5.10 NTS. Probes are in
`tests/pool_visibility_probe.rs` (`#[ignore]`d) plus direct runs of the release
binary.

## V1 — `ready_workers` on a completely healthy pool

Procedure: four workers, sixteen concurrent ordinary `/json` requests for three
seconds, with a sampler reading `PoolSnapshot` every 2 ms throughout.

| | |
|---|---:|
| requests served | 23,072, all `200` |
| respawns | 0 |
| execution timeouts | 0 |
| lowest `ready_workers` observed | **0** of target 4 |
| samples reading zero | **192 of 958 (20.0%)** |
| live workers, from per-slot pids | 4 of 4 the whole time |

FACT: the only aggregate `PoolSnapshot` exposed reads zero on a pool that did
not fail a single one of 23,072 requests, for a fifth of the samples taken
under load.

FACT (source): `mark_busy` decrements `ready_workers`. The counter measures
idle capacity, not health.

FACT (source): `live_workers` is the counter `execute` gates its 503 on, and it
was **not exposed in `PoolSnapshot` at all**.

INFERENCE: any degradation check built on the exposed counter would fire
continuously under ordinary traffic, and the counter that answers the question
was unreachable from outside the pool. This is why the slice exposes
`live_workers` and documents `ready_workers` as spare capacity.

## Recovery was invisible

Procedure: release binary, two workers, `request_timeout = 1`, four
`/delay?ms=3000` requests to force execution timeouts, then ordinary `/json`
traffic. Captured stdout and stderr.

Recovery traffic: `200 200 200 200 200`.

FACT: the run produced twelve stderr lines, every one of them a failure or a
scheduled replacement, and **not one line indicating the pool came back**. The
log ends on four consecutive failure lines while the server is serving
normally.

FACT (source): `replacement_generation_task` discards the `WorkerStart` report
on success and emits nothing.

INFERENCE: an operator tailing this log, or a rule matching on it, sees a pool
that broke and never recovered. Degradation was not under-reported — it was
over-reported at three lines per event — while recovery was reported not at
all.

## Choosing the outage threshold

A first implementation reported every drop to zero live workers immediately.
Driving a single-worker pool through seven crashes produced:

```text
7 x "has no live worker"
2 x "recovered"
```

FACT: five alarms had no matching all-clear, because five of the outages were
ordinary replacements lasting well under a second while only two crossed the
reporting threshold on the recovery side.

INFERENCE: that is the same asymmetry the slice set out to remove, reintroduced
in a new form, and it reads as a pile of unresolved outages. Both lines must
share one threshold.

Threshold inputs, all already measured:

| Event | Time the pool has no live worker |
|---|---:|
| fixture replacement bootstrap | 35-38 ms |
| real Laravel bootstrap, median | 58-71 ms |
| real Laravel bootstrap, worst observed | 403 ms |
| infrastructure backoff ladder, upper steps | up to 5 s |
| cooldown band | 30-300 s |

Selected: **1 second**, about 2.5 times the worst measured bootstrap, below
every step that a client could notice.

## After the fix

Procedure: release binary, one worker. Phase A drives four execution timeouts,
each of which replaces the worker in tens of milliseconds. Phase B drives seven
crashes to walk the infrastructure backoff ladder.

```text
phase A (4 ordinary replacements):   0 outage lines
phase B (ladder driven past 1 s):
  PHP worker pool localhost/ has no live worker after 1000 ms; requests are refused until one is READY
  PHP worker pool localhost/ recovered: 1 of 1 workers live after 1636 ms with none
  PHP worker pool localhost/ has no live worker after 1000 ms; requests are refused until one is READY
  PHP worker pool localhost/ recovered: 1 of 1 workers live after 3244 ms with none
```

FACT: routine replacement churn produces no outage lines at all, and every
alarm has exactly one matching all-clear.

## Hot path

The per-request path is unchanged: `mark_busy` and `mark_ready_after_job` were
not touched, and the new work happens only when a generation starts or ends.
Two fields were added to `PoolShared`, which sits beside the atomics `execute`
reads per request, so the gate was run anyway.

Release builds of the pre-change and post-change trees, interleaved, 20,000
requests, one worker, concurrency 1, medians of three runs.

| Scenario | Pass | Before rps | After rps | Before p50 | After p50 |
|---|---:|---:|---:|---:|---:|
| `php_json` | 1 | 5986 | 6248 | 0.158 ms | 0.151 ms |
| `php_json` | 2 | 6301 | 6464 | 0.150 ms | 0.145 ms |
| `php_hello` | 1 | 6273 | 6275 | 0.149 ms | 0.149 ms |
| `php_hello` | 2 | 5887 | 6006 | 0.161 ms | 0.159 ms |

FACT: all four cells favour the post-change arm, by 0.0% to 4.4%.

INFERENCE: this is not an improvement and should not be read as one. There is
no per-request change that could produce it, the spread between runs of a
single median has reached 11% in this harness, and the arms were run in a fixed
order within each pass. It is host drift. What the data supports is only that
no regression was detected, and it would not detect one below roughly 5%.

## Tooling regression found and fixed

FACT: changing the startup line from `PHP worker <pid> ready after <ms> ms` to
`PHP worker pool <label> worker <pid> ready after <ms> ms` broke
`benchmarks/php_worker.ps1`, which parses it for bootstrap timing and aborted
with `Worker READY/bootstrap timing was not found in server output`.

The parser now accepts both spellings, so the script also still works against
older binaries kept for A/B comparison. No other benchmark script parses that
line.
