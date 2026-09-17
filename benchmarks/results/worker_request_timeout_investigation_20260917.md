# Persistent-worker request timeout — fault injection, 2026-09-17

Investigation into whether the fixed five-second `REQUEST_TIMEOUT` lets ordinary
slow application requests degrade a PHP worker pool. No production code was
changed to take these measurements.

Machine: Windows 11 Pro build 26200, Core i5-12400, 32 GiB,
`x86_64-pc-windows-msvc`, PHP 8.5.10 NTS. Debug test build driving real
`php.exe` children through the real pool and real TCP; the numbers of interest
are wall-clock lifecycle intervals, which the profile does not move.

Instrument: `tests/timeout_probe.rs`, `#[ignore]`d, one experiment per test.
Workload is the existing `fixtures/php-worker/worker.php` route
`/delay?ms=6000`, which sleeps past the timeout, against `/json` as ordinary
traffic. No new fixture was needed.

```powershell
cargo test --test timeout_probe e1_ -- --ignored --nocapture --test-threads=1
```

## E1 — one slow request, one worker

| | |
|---|---:|
| client result | `502 Bad Gateway` after 5008 ms |
| generation | 1 -> 2 (pid 12336 -> 3436) |
| `consecutive_failures` | 0 -> 1 |
| pool unable to serve for | 61 ms |

FACT: a single request that exceeds the deadline does not merely fail. It kills
the PHP child, ends its generation and charges the slot one failure.

FACT: the replacement reached READY in 61 ms, so the outage from one timeout is
negligible. `doc/BENCHMARKS.md` records 58 ms median bootstrap for app12 and
71 ms for PMS, so a real Laravel worker is in the same range; the fixture is
not flattering this result.

## E2 — repeated slow requests, one worker

A first attempt fired the requests back to back and measured nothing: rounds 2
to 13 were refused in 0 ms with `503` because `live_workers == 0` while the
replacement was bootstrapping, so they never reached PHP and never cost a
failure. Requests must be paced to READY to reach the timeout path at all.

Paced to READY, one slow request per round:

| Round | Waited for READY | Failures after | Next replacement delay |
|---:|---:|---:|---:|
| 1 | 0 ms | 1 | 0 ms |
| 2 | 26 ms | 2 | 100 ms |
| 3 | 155 ms | 3 | 200 ms |
| 4 | 265 ms | 4 | 400 ms |
| 5 | 468 ms | 5 | 800 ms |
| 6 | 865 ms | 6 | 1600 ms |
| 7 | 1677 ms | 7 | 3200 ms |
| 8 | 3273 ms | 8 | 5000 ms |
| 9 | 5039 ms | 9 | 5000 ms |
| 10 | 5051 ms | 10 | 5000 ms |
| 11 | 5050 ms | 11 | **30000 ms (cooldown)** |
| 12 | 30058 ms | 12 | 60000 ms |
| 13 | 60078 ms | 13 | 120000 ms |

FACT: eleven slow requests move the slot from healthy into the cooldown band.
Every one of them was answered `502`, and every replacement reached READY.

FACT: after round 13 an ordinary `/json` request received `503` in 0 ms with the
slot in `Backoff` and its next generation 120 seconds away.

INFERENCE: the escalation is self-sustaining while slow traffic continues. The
slot's recovery interval grows, but one slow request per interval is enough to
kill the new generation, so the interval keeps doubling to the 300-second
ceiling.

## E3 — four concurrent slow requests on a four-worker pool

| | |
|---|---:|
| slow requests | 4 x `502` after 5027-5028 ms |
| workers killed and replaced | 4 of 4 |
| `consecutive_failures` after | 1 on every slot |
| ordinary request issued at 513 ms | `200 OK` after **4557 ms** |
| ordinary requests from 5579 ms on | `200 OK` in 1-7 ms |
| pool back to 4/4 | 5583 ms |

FACT: one burst of concurrent slow requests replaces the entire pool at once.

FACT: an ordinary request that arrives while every worker is occupied is not
refused; it waits in the queue and is served 4557 ms later, when a replacement
arrives. Its latency is bounded by the timeout, not by the slow route.

FACT: a single burst does not escalate. All four slots stopped at one failure.

## E4 — mixed traffic: continuous ordinary requests plus one slow request every 7 s, two workers

| | |
|---|---:|
| slow requests | 10, all `502` |
| respawns | 10 in 70 s |
| ordinary requests | **629 served `200`, 0 refused, 0 other** |
| `consecutive_failures` range observed | 1 to 4 |
| stability reset observed | slot 1 went 3 -> 1 at 54 s |

FACT: with healthy traffic interleaved, the pool never degraded. Ordinary
traffic saw no `503` at all, and the 15-second stability reset visibly pulled a
slot's failure history back down mid-run.

FACT: the pool respawned a PHP child every 7 seconds for the whole run without
ever approaching the cooldown band.

## E5 — saturation and recovery, two workers

Two concurrent slow requests per burst, paced to a fully READY pool.

| Burst | Failures after |
|---:|---:|
| 1 | 1, 1 |
| 2 | 2, 2 |
| 3 | 3, 3 |
| 4 | 4, 4 |
| 5 | 5, 5 |
| 6 | 6, 6 |
| 7 | 7, 7 |

| | |
|---|---:|
| respawns | 14 |
| back to 2/2 after slow traffic stopped | 3371 ms |
| first ordinary request after recovery | `200 OK` in 2 ms |
| `consecutive_failures` after recovery | **7 on both slots** |
| leaked PHP processes | 0 |

FACT: concurrent slow requests walk every slot up the ladder in lockstep.

FACT: recovery is bounded and complete, and no child is orphaned.

FACT: the failure history survives recovery. A pool that is serving normally
again is still primed, so the next wave starts at failure 8 rather than 1.

## E6 — does ordinary traffic clear the history?

Five paced slow requests to failure 5, then ordinary `/json` only.

| | |
|---|---:|
| failure history cleared after | **15 036 ms** of ordinary traffic |
| ordinary requests during that window | all `200` |

FACT: the reset fires at exactly `STABILITY_RESET_PERIOD`, and it needs a
generation that both reaches 15 seconds of age and serves a request.

## E7 — what one timeout wave costs ordinary traffic

Eleven paced slow requests to enter the cooldown band, then slow traffic stops
completely and only ordinary requests continue.

| | |
|---|---:|
| slow requests to reach cooldown | 11, over 77 s |
| **ordinary traffic refused for** | **30 151 ms (59 consecutive `503`)** |
| ordinary traffic served again at | `200 OK` |
| failure history cleared | 14 985 ms later |
| total time from wave end to healthy | ~45 s |
| leaked PHP processes | 0 |

FACT: eleven slow requests bought thirty seconds of complete application
outage, including for clients that never touched the slow route.

FACT: the state is not a permanent trap. Once slow traffic stops the pool
recovers by itself in about 45 seconds with no operator action.

## Cross-cutting result

FACT: across E2 through E7, 52 timeout-driven replacements were observed and
**all 52 reached READY**. Not one bootstrap failed.

FACT: every probe ended with zero leaked PHP processes.

INFERENCE: the backoff ladder bought nothing in any observed case. It exists to
protect a slot that cannot start; a slot killed by an application timeout
started successfully every time. The delay purchased only the outage in E7.

INFERENCE: the degradation boundary is a rate. A generation resets its history
only if it survives 15 seconds and serves a request, and it dies about
`request_timeout` after a slow request reaches it. With `W` workers and slow
requests arriving every `T` seconds, each slot meets one roughly every `W * T`
seconds, so escalation needs `W * T < 15 s`. E4 measured `W * T = 14 s` and saw
oscillation without escalation; E2, E5 and E7 drove `W * T` far below that and
escalated every time. Predicted thresholds are more than 4 slow requests per
minute at one worker, 16 at four, 32 at eight.

HYPOTHESIS (untested): the same escalation applies to any failure that occurs
after the request frame is written, not only to timeouts, because they share the
`GenerationOutcome::Failed` path. Protocol and I/O faults were not driven at
rate here.
