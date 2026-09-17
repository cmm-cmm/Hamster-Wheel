# Multiple persistent application pools — investigation, 2026-09-17

Evidence for what it would actually take to let more than one `php_worker`
location coexist. No production code was changed. The one-pool guard in
`config::validate_raw` was lifted temporarily to take these measurements and
then restored, so the tree still permits exactly one pool.

Machine: Windows 11 Pro build 26200, Core i5-12400, 32 GiB,
`x86_64-pc-windows-msvc`, PHP 8.5.10 NTS. Probes are in
`tests/multipool_probe.rs`; they need the guard lifted to run. A new fixture,
`fixtures/php-worker/ignores_shutdown.php`, exists to force a pool shutdown
error, which is otherwise unreachable from outside.

## Ownership: already per-pool

FACT (source): `php_worker.rs` contains no `static`, no `lazy_static` and no
`thread_local`. Every piece of pool state lives in one `PoolShared` per pool:
its own `SpawnGate`, `lifecycle` mutex, `jobs_tx`, slot table, `live_workers`,
`ready_workers`, `recycle_owner`, respawn and timeout counters, and outage
timestamp.

FACT (source): the only deliberately shared resource is
`body_budget: Arc<BodyBudget>`, which is process-wide by design because the
resource it bounds is process memory.

INFERENCE: the pool architecture needs no redesign for N pools. Nothing in it
assumes it is the only instance.

## M2 — failure isolation between pools

Procedure: two virtual hosts, `a.test` and `b.test`, each with its own two-worker
pool at `/`. Six `/crash?ms=0` requests against `a.test`, interleaved with six
ordinary `/json` requests against `b.test`.

| | |
|---|---|
| `a.test` | `502` x6 |
| `b.test` | **`200` x6** |
| after settling | both `200` |

FACT: crashing every worker of one pool repeatedly did not cost the other pool
a single request.

A first version of this probe mounted both pools under one host at `/a` and
`/b` and reported six `200`s from the crash route. That was a probe bug, not a
result: a pool at `/a` receives `/a/crash` as its path, which the fixture does
not recognise, so nothing crashed. One application per virtual host is both the
realistic shape and the one where the shared fixture works.

## M1 — partial startup failure

Procedure: pool one healthy (`worker.php`, two workers), pool two fails
(`exit_before_ready.php`). Call `start_workers()`.

| | |
|---|---|
| `start_workers()` | `Err(Startup("worker 0: ... early eof"))` |
| repository `php.exe` still alive afterwards | **2** |
| `shutdown_workers()` called after the failure | `Ok(())` |
| `php.exe` after that cleanup | **0** |

FACT: `start_workers` uses `?` inside its loop, so it returns on the first
failing pool and the already-started pool is left running.

FACT: the cleanup machinery nonetheless works. Calling `shutdown_workers()`
after the failure reaped everything.

FACT (source): `server::run` does *not* call it. Its first line is
`config.start_workers().await?`, which propagates without cleanup, while the
listener-bind and shutdown-signal paths below it both do call
`shutdown_workers()` on error.

### The shipped binary does not leak

Procedure: the same two-pool configuration given to the release binary.

```text
server exit code: 1
php.exe still alive after the server process exited: 0
```

FACT: no orphan survives in production. `kill_on_drop(true)` reaps the children
when the process exits.

INFERENCE: the concern as originally stated — partial startup leaves orphans —
is **not** true of the shipped binary. What is true is narrower: the healthy
pool's workers are *killed* rather than sent SHUTDOWN, so their application
cleanup never runs. At startup that is nearly harmless. It would stop being
harmless if `run` were ever called without the process then exiting.

FACT: the startup error names `worker 0` and does not name the pool that
failed. The only reason `broken.test/` is identifiable in the run above is that
the child happened to write to its own standard error, which the drain labels.
A pool that failed silently could not be identified.

## M3 and M4 — shutdown-all

M3, three healthy pools, happy path:

| | |
|---|---|
| children started | 6 |
| `shutdown_workers()` | `Ok(())` |
| children left | **0 of 6** |

M4, the stubborn pool placed **first**, then two healthy pools. The stubborn
worker reaches READY, serves, and then ignores SHUTDOWN so the pool's two-second
budget expires.

```text
shutdown_workers returned: Err(Shutdown("slot 0 generation 1: PHP worker
  shutdown failed: timeout after 2000 ms; worker was killed"))
php.exe still alive after shutdown_workers: 4 of 5
```

FACT: the failing pool killed its own worker, but the two healthy pools behind
it in the loop were **never shut down at all**. `shutdown_workers` uses `?`, so
one pool's error ends the loop.

INFERENCE: in the shipped binary those four would still be reaped by process
exit, so again this is not a durable orphan. The real consequence is that
graceful shutdown is silently skipped for every pool after the first failure:
no SHUTDOWN frame, no drain of in-flight jobs, no application cleanup. For a
persistent application server that means abandoned transactions and skipped
framework teardown, which is precisely what the pool shutdown path exists to
avoid.

## The startup policy question

FACT (source): `PhpWorkerPool::start` on failure shuts down whatever reached
READY, sets `PoolLifecycle::Stopped`, and returns. **No supervisor task is
spawned.** There is no retry, no backoff and no recovery path.

FACT (source): `accepting` stays false, so `execute` returns `Unavailable` and
the location answers `503`.

INFERENCE: this is the asymmetry that decides the question. A pool that starts
and *later* loses every worker self-heals — slots are never permanently
retired, and the cooldown band keeps retrying at up to 300 s forever. A pool
that fails to *start* is permanently dead until the process is restarted.

So the two options are not symmetric in cost:

| | fail the whole server | serve the healthy pools |
|---|---|---|
| One broken application | nothing serves, including unrelated static hosts and healthy applications | the broken host answers `503`; everything else serves |
| Operator signal | immediate and impossible to miss: the server does not come up | a `503` host and one startup line, on a server that looks healthy |
| Recovery | operator fixes and restarts | **none** — that pool stays dead until restart regardless |
| Risk | a typo in one application's config takes down every site on the host | a deployment silently runs without one of its applications |

HYPOTHESIS: "serve the healthy pools" would be more defensible if a failed pool
entered the existing backoff/cooldown ladder instead of dying permanently,
because then it would behave like every other failure in the system and would
recover by itself once the fault was fixed. That is a change to
`PhpWorkerPool::start`, not just a policy switch, and it was not prototyped
here.

This is recorded for the owner to decide. No policy was chosen.

## Smallest scope this evidence supports

1. `start_workers`: on failure, shut down the pools that already started before
   returning. Measured need: M1.
2. `shutdown_workers`: attempt every pool and report the first error afterwards,
   rather than returning on the first failure. Measured need: M4.
3. `server::run`: call `shutdown_workers()` on the startup-error path, as the
   two paths below it already do. Measured need: M1 plus source.
4. Name the pool in the startup failure. Measured need: M1.
5. Only then lift the config guard.

Not required by any evidence here: any change to `PhpWorkerPool` itself, a
shared supervisor, a backend trait, or pool identity work. Identity and
live-worker health semantics already landed in the previous slice and were used
unchanged by every probe above.
