# Persistent Worker Cost — Gate 2 Results

Date: 2026-09-16

Status: Gate 2 completed for owner review. Gate 3 was not opened. No execution
architecture, protocol or roadmap change was made.

Raw records, rejected observer designs, hashes and reproduction commands are
in [`benchmarks/results/persistent_cost_gate_2_20260916/`](../../benchmarks/results/persistent_cost_gate_2_20260916/).

## Technical result

FACT: on the PMS light route at W1/C1, sampled PHP P50 is 1.869 ms. The
per-record buckets have P50 1.224 ms for Laravel kernel/middleware/controller,
0.536 ms for Octane request lifecycle, and 0.085 ms for Hamster's direct PHP
adapter work. At W4/C16 they are 1.515 ms, 0.670 ms and 0.099 ms respectively.
Independent bucket medians do not add exactly to the total median; each raw
record does, with a 0.4 us P50 rounding residual.

FACT: the direct Hamster adapter is about 4-5% of measured PHP time on this
route. The material costs are Laravel application handling (about 66%) and
Octane lifecycle (about 29%). This falsifies any explanation that assigns the
roughly 2 ms Gate 1 residual primarily to Hamster framing, request
reconstruction, response encoding or final cleanup.

FACT: W1-to-W4 light-route service interval increased from approximately
2.51 ms to 2.95 ms/worker. Median PHP CPU increased from 2.209 ms to 2.690 ms
per request; Rust CPU increased only about 0.018 ms. The sampled P50 increase
was 0.291 ms in Laravel handling, 0.135 ms in Octane lifecycle and 0.013 ms in
the direct adapter. The efficiency change is therefore inside the PHP
framework/workload, not pool queueing or the Rust/PHP boundary.

FACT: the bare-controller control changes the result materially. Removing the
`web` middleware group reduced PHP P50 to 0.779 ms at W1 and 0.866 ms at W4.
W4 delivered approximately 3.98 times W1 throughput and its per-worker service
interval remained approximately 1.00 ms. The light route delivered only about
3.40 times W1 throughput. PHP CPU increased about 0.115 ms W1-to-W4 on bare,
versus 0.480 ms on light.

INFERENCE: roughly three quarters of the light route's W1-to-W4 PHP CPU
increase is associated with the `web` middleware workload, not worker count by
itself. This experiment removes the entire group, so it does not identify one
middleware as causal.

HYPOTHESIS: file-backed sessions and cookie/session middleware account for a
large part of that delta when sixteen independent client sessions are active.
The evidence does not isolate file session I/O, allocator/cache locality or an
individual middleware, so this is not recorded as a Hamster optimization.

## Accepted stage budget

Pooled raw samples from the final CPU-recording batch:

| PHP stage | Light W1 P50 | Light W4 P50 | W4-W1 |
|---|---:|---:|---:|
| frame decode | 11.2 us | 13.2 us | +2.0 us |
| request reconstruction | 21.2 us | 23.8 us | +2.6 us |
| output-guard setup | 1.2 us | 1.5 us | +0.3 us |
| Octane prepare / `RequestReceived` | 318.0 us | 407.6 us | +89.6 us |
| Laravel kernel/middleware/controller | 1,223.6 us | 1,514.8 us | +291.2 us |
| response staging and protocol encode | 22.3 us | 28.0 us | +5.7 us |
| Octane terminate / `RequestTerminated` | 173.5 us | 219.1 us | +45.6 us |
| Octane tail / sandbox flush | 29.5 us | 38.4 us | +8.9 us |
| Hamster external cleanup | 14.2 us | 17.0 us | +2.8 us |
| response commit / pipe write | 11.8 us | 14.3 us | +2.5 us |
| PHP total | 1,868.8 us | 2,304.7 us | +435.9 us |

Grouped per-record distributions:

| Workload | W/C | direct Hamster adapter P50/P95 | Octane lifecycle P50/P95 | Laravel handler P50/P95 | PHP total P50/P95 |
|---|---:|---:|---:|---:|---:|
| light | 1/1 | 85.4 / 119.7 us | 535.5 / 730.1 us | 1,223.6 / 1,676.3 us | 1,868.8 / 2,493.2 us |
| light | 4/16 | 98.5 / 129.8 us | 670.3 / 872.6 us | 1,514.8 / 2,147.1 us | 2,304.7 / 3,057.2 us |
| bare | 1/1 | 56.8 / 76.4 us | 384.5 / 537.5 us | 336.4 / 464.8 us | 779.2 / 1,080.5 us |
| bare | 4/16 | 62.3 / 81.6 us | 435.9 / 662.1 us | 362.0 / 524.8 us | 865.5 / 1,248.1 us |

`direct Hamster adapter` comprises frame decode, request reconstruction,
output guard, response staging/encoding, Hamster cleanup and response commit.
`Octane lifecycle` comprises prepare, terminate and sandbox tail. `Laravel
handler` still includes the framework kernel and middleware; it is not a
measurement of PMS controller user code alone.

## End-to-end and CPU evidence

Final light batch, three fresh servers per cell:

| W/C | RPS median (range) | service interval/worker | Rust CPU/request | PHP CPU/request | client CPU/request |
|---|---:|---:|---:|---:|---:|
| 1/1 | 399.1 (381.9-405.1) | 2.506 ms | 0.253 ms | 2.209 ms | 0.194 ms |
| 4/16 | 1,357.1 (1,302.2-1,390.1) | 2.948 ms | 0.271 ms | 2.690 ms | 0.171 ms |

Bare control:

| W/C | RPS median (range) | service interval/worker | Rust CPU/request | PHP CPU/request | client CPU/request |
|---|---:|---:|---:|---:|---:|
| 1/1 | 998.1 (985.1-1,024.3) | 1.002 ms | 0.244 ms | 0.810 ms | 0.191 ms |
| 4/16 | 3,976.7 (3,720.1-4,070.3) | 1.006 ms | 0.228 ms | 0.925 ms | 0.111 ms |

FACT: known server/PHP CPU accounts for the W4 light service interval; there
is no large wait residual requiring a queue or pipe explanation. Client CPU per
request did not increase at W4. The shared workstation still prevents a claim
about small effects or isolated-host saturation.

## Observer qualification and falsified measurement design

FACT: the normal release and the `profiling` release plus generated worker with
collection disabled differed by at most 1.1% in median throughput. With the
accepted in-memory sampler enabled, median throughput changed by -1.5% at W1
and -2.4% at W4 versus normal release, below the 3% observer band and below
normal full run spread. All responses completed and all four W4 profiler PIDs
dumped records after counter capture.

FACT: the first profiler wrote batches during the timed window. It reduced W4
median throughput by 6.7% at sample rate 1/8. Reducing the sampling rate to
1/64 while retaining timed filesystem writes still produced a 7.8% median
difference. Those results were rejected before interpretation.

INFERENCE: filesystem/antivirus work in the observer, not timestamp calls
alone, materially perturbed the concurrent PHP workload. Moving output after
the timed counters removed the repeatable large bias. This is also a warning
against synchronous per-request diagnostics in future performance work.

FACT: absolute RPS drifted substantially during the long session. Topologies
and candidates were alternated, medians and full ranges are reported, and no
small optimization conclusion is drawn from cross-batch RPS.

FACT: the accepted artifact could sample a same-route post-window dump trigger
when its per-worker ID matched the 1/64 selector, although its file write was
outside `php_total_us`. A conservative sensitivity check removed the final
record from every PID: grouped P50 moved by at most 10.1 us at W1 and 3.8 us
at W4, without changing any attribution. The reproducibility generator now
excludes dump-trigger requests entirely.

## What is actionable for Hamster

FACT: direct adapter work is only about 85-99 us on the light route. Individual
Hamster-owned pieces are each approximately 12-28 us at P50. There is no
dominant framing, reconstruction, cleanup, staging or commit bucket.

INFERENCE: optimizing those direct pieces cannot recover the observed
0.44-0.48 ms W1-to-W4 efficiency loss. A rewrite of protocol or process
placement is not supported by Gate 2.

FACT: Octane lifecycle is material at 0.54-0.67 ms on light and 0.38-0.44 ms
on bare. It is part of the Laravel adapter's correctness model, not Hamster
core. Gate 2 does not show that any specific listener is redundant or safe to
remove.

HYPOTHESIS: listener-level profiling could find application-specific Octane
work that can be disabled by deployment configuration. That would be a
Laravel-adapter compatibility/tuning study, not a generic worker-architecture
optimization, and current evidence does not justify weakening lifecycle
defaults.

INFERENCE: the largest near-term opportunity belongs to application/framework
configuration: keep routes that do not need sessions out of `web`, audit PMS
middleware, and compare an appropriate production session backend. Those are
not automatic Hamster changes and must preserve application semantics.

## Technical judgment

Gate 2 answered the approved question. Most of the roughly 2 ms Gate 1 gap is
normal Laravel kernel/middleware work plus the Octane isolation lifecycle.
Hamster's direct PHP adapter is small and its W1-to-W4 inflation is negligible
in absolute terms. The multi-worker efficiency loss is reproducible in the
session-bearing light workload but nearly disappears in the bare control; it
does not indict the fixed process pool or subprocess boundary.

No Gate 3 trigger was found. There is no evidence here for embedded PHP,
sharded hosting, Morph or another execution architecture. The current
actionable lead is PMS/Laravel middleware and session behavior; the only
Hamster-specific research lead is finer Octane-listener attribution if the
owner considers roughly 0.4-0.7 ms worth the compatibility and correctness
risk. No optimization was implemented.

## Qualification and limits

FACT: formatting, all-target/all-feature clippy, profiling bench linkage and
the non-ignored Rust suite passed. The Octane ownership contract and PMS W1
authenticated-to-anonymous plus cleanup-failure retirement tests also passed
against the scratch. Response validation returned complete 200 responses for
bare, light and login. No Hamster or fixture PHP process remained afterwards.

FACT: measurement and lifecycle qualification ran on Windows x86_64 MSVC.
Linux was not run. Gate 2 did not add soak/failure experiments because it did
not change production execution or lifecycle code; it reused the Gate 0-1
reliability qualification and reran the two Laravel invariants most adjacent
to the profiling hook.
