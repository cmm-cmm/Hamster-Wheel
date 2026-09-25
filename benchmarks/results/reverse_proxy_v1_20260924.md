# Reverse Proxy v1 — measurements, 2026-09-24

Machine: Windows 11 Pro build 26200, Core i5-12400 (6C/12T), 32 GiB,
`x86_64-pc-windows-msvc`, release builds only, Balanced power plan. Loopback
only; no real NIC. Native Linux: NOT VERIFIED; nothing here is a Linux claim.

Harness: `benches/proxy.rs` (`cargo bench --target x86_64-pc-windows-msvc
--bench proxy --features profiling -- <suite>`, suites
`all|matrix|buffers|memory|probe|idle`). The upstream (Hyper 1.11.1 HTTP/1
server) and the load generator (Hyper client, one keep-alive connection per
concurrent worker, `TCP_NODELAY`) share one bench process with a 6-thread Tokio
runtime. Hamster runs as the release binary in its own process with its default
runtime, a fresh process per measured cell, so its CPU time (`GetProcessTimes`)
and private commit (`K32GetProcessMemoryInfo`) are its own. Each cell: 750 ms
warm-up, then a 3 s (23 B, 1 MiB) or 4 s (64 MiB) window; in-flight requests at
the end of the window complete and are counted. Three runs per cell; medians.
"bench CPU" is the bench process (client plus upstream), context only.
"upstream conns" counts connections the upstream accepted during the measured
window: for direct runs these are the client's own connections, for proxied
runs they are new pool connections.

Hamster contends for the same 12 hardware threads as the bench process. These
numbers separate Hamster's overhead from the upstream only to that extent; they
are not a capacity claim, and nothing here says the proxy is faster or slower
than any other proxy.

## Matrix: direct upstream vs Hamster proxy

Defaults: `http1_max_buffer_size` 64 KiB, `max_idle` 64.

| Payload | C | Mode | RPS | p50 ms | p95 ms | p99 ms | Hamster CPU us/req | bench CPU us/req | upstream conns | Hamster private MiB (end / peak) | errors |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---|---:|
| GET 23 B | 1 | direct | 33253.7 | 0.025 | 0.045 | 0.104 | - | 29.3 | 1 | - | 0 |
| GET 23 B | 1 | proxy | 11844.2 | 0.069 | 0.144 | 0.198 | 58.6 | 53.2 | 0 | 1.75 / 1.77 | 0 |
| GET 23 B | 32 | direct | 70643.9 | 0.424 | 0.678 | 1.346 | - | 43.5 | 32 | - | 0 |
| GET 23 B | 32 | proxy | 62708.1 | 0.499 | 0.694 | 0.810 | 64.1 | 54.4 | 0 | 6.01 / 6.62 | 0 |
| GET 23 B | 128 | direct | 78428.5 | 1.592 | 2.414 | 2.801 | - | 44.0 | 128 | - | 0 |
| GET 23 B | 128 | proxy | 60372.9 | 2.074 | 3.031 | 3.513 | 64.1 | 55.2 | 224 | 13.06 / 15.59 | 0 |
| GET 1 MiB | 1 | direct | 2136.1 | 0.394 | 0.595 | 0.750 | - | 423.6 | 1 | - | 0 |
| GET 1 MiB | 1 | proxy | 493.6 | 0.705 | 13.218 | 15.709 | 876.2 | 400.9 | 0 | 1.84 / 1.84 | 0 |
| GET 1 MiB | 32 | direct | 5945.9 | 5.208 | 7.827 | 10.908 | - | 900.7 | 32 | - | 0 |
| GET 1 MiB | 32 | proxy | 4323.2 | 6.916 | 12.119 | 14.296 | 1062.9 | 1017.5 | 0 | 7.68 / 10.38 | 0 |
| GET 1 MiB | 128 | direct | 4649.4 | 26.756 | 38.649 | 46.861 | - | 1102.4 | 128 | - | 0 |
| GET 1 MiB | 128 | proxy | 3819.4 | 31.108 | 54.588 | 67.423 | 1121.7 | 1117.8 | 71 | 17.98 / 29.55 | 0 |
| GET 64 MiB | 1 | direct | 29.4 | 32.429 | 66.347 | 87.174 | - | 23948.6 | 1 | - | 0 |
| GET 64 MiB | 1 | proxy | 27.6 | 34.164 | 54.462 | 63.248 | 49675.7 | 20636.8 | 0 | 1.84 / 1.87 | 0 |
| GET 64 MiB | 32 | direct | 101.0 | 314.208 | 343.085 | 350.319 | - | 54950.4 | 32 | - | 0 |
| GET 64 MiB | 32 | proxy | 65.0 | 492.008 | 531.746 | 550.315 | 73914.0 | 64162.2 | 0 | 6.87 / 9.19 | 0 |
| GET 64 MiB | 128 | direct | 82.5 | 1527.206 | 1617.010 | 1642.222 | - | 65836.6 | 128 | - | 0 |
| GET 64 MiB | 128 | proxy | 56.2 | 2236.833 | 2426.879 | 2502.885 | 77880.9 | 68786.6 | 64 | 17.70 / 30.09 | 0 |
| POST 64 MiB | 1 | direct | 35.3 | 26.434 | 60.267 | 78.342 | - | 23009.4 | 1 | - | 0 |
| POST 64 MiB | 1 | proxy | 26.5 | 34.081 | 56.429 | 93.079 | 46445.0 | 29186.3 | 0 | 1.83 / 1.84 | 0 |
| POST 64 MiB | 32 | direct | 68.0 | 388.299 | 679.700 | 701.167 | - | 61417.9 | 32 | - | 0 |
| POST 64 MiB | 32 | proxy | 50.7 | 630.716 | 721.361 | 740.885 | 77287.9 | 69684.7 | 0 | 5.14 / 9.25 | 0 |
| POST 64 MiB | 128 | direct | 75.6 | 1656.988 | 1831.862 | 1939.561 | - | 73893.2 | 128 | - | 0 |
| POST 64 MiB | 128 | proxy | 48.4 | 2515.340 | 3063.785 | 3247.941 | 90210.0 | 82824.7 | 64 | 11.12 / 32.12 | 0 |

FACT: every proxied cell completed with zero errors. Hamster's private commit
at the end of each window stayed between 1.75 and 18 MiB regardless of payload;
the highest peak was 32 MiB at C128 x 64 MiB, a round with 8 GiB in flight.

FACT: at C1 a 23-byte exchange costs Hamster 58.6 us of CPU and adds 44 us to
the median (25 to 69 us). A 64 MiB download costs Hamster about 0.78 ms CPU
per MiB and runs at 94% of direct throughput (27.6 vs 29.4 RPS, about 1.8 GB/s).

FACT: at C128 with `max_idle` 64, 224 new upstream connections were opened in
a 3 s window of roughly 181,000 exchanges; see the `max_idle` section.

FACT: GET 1 MiB at C1 had p95/p99 of 13.2/15.7 ms through the proxy against
0.60/0.75 ms direct in this run. The direct path is not immune: the later
investigation measured the same stall on direct connections, only less often
(1.6-6.3% of requests at or above 5 ms). See *Windows loopback send stall*.

INFERENCE: the C32/C128 large-payload gaps (for example 65 vs 101 RPS at
C32 x 64 MiB) are at least partly host contention: three processes share six
cores and a proxied run moves every byte through two more sockets. Nothing
here isolates Hamster's own ceiling on separate hardware.

## HTTP/1 buffer bound (`server.http1_max_buffer_size`)

The same value bounds Hamster's downstream connections and now its upstream
connections. Proxied only.

| Buffer | Payload | C | RPS | p50 ms | p99 ms | Hamster CPU us/req | Hamster private MiB (end / peak) | errors |
|---:|---|---:|---:|---:|---:|---:|---|---:|
| 16384 | GET 23 B | 32 | 56876.2 | 0.530 | 1.280 | 64.3 | 5.71 / 6.72 | 0 |
| 16384 | GET 64 MiB | 1 | 14.2 | 66.519 | 82.699 | 76844.3 | 1.95 / 1.97 | 0 |
| 16384 | GET 64 MiB | 32 | 53.9 | 587.064 | 653.262 | 103724.9 | 8.47 / 8.68 | 0 |
| 16384 | POST 64 MiB | 1 | 15.4 | 65.096 | 69.797 | 73840.7 | 1.94 / 1.94 | 0 |
| 65536 | GET 23 B | 32 | 61556.2 | 0.507 | 0.867 | 63.7 | 6.42 / 8.08 | 0 |
| 65536 | GET 64 MiB | 1 | 28.2 | 33.093 | 67.430 | 46441.0 | 1.84 / 1.89 | 0 |
| 65536 | GET 64 MiB | 32 | 53.9 | 567.575 | 803.534 | 73372.1 | 7.16 / 10.09 | 0 |
| 65536 | POST 64 MiB | 1 | 27.8 | 33.105 | 71.598 | 46875.0 | 1.84 / 1.88 | 0 |
| 417792 | GET 23 B | 32 | 60768.7 | 0.508 | 0.948 | 65.2 | 6.34 / 6.78 | 0 |
| 417792 | GET 64 MiB | 1 | 2.2 | 437.055 | 626.998 | 26562.5 | 2.80 / 3.63 | 0 |
| 417792 | GET 64 MiB | 32 | 54.5 | 577.029 | 677.051 | 77581.5 | 19.94 / 37.96 | 0 |
| 417792 | POST 64 MiB | 1 | 2.6 | 392.200 | 575.973 | 34375.0 | 3.50 / 3.91 | 0 |

Probe re-runs (`-- probe`, C1, three runs, medians), including intermediate
sizes:

| Buffer | GET 1 MiB RPS / p50 / p95 ms | GET 64 MiB RPS / p50 ms | POST 64 MiB RPS / p50 ms |
|---:|---|---|---|
| 32768 | 850.0 / 0.752 / 1.839 | 24.2 / 40.9 | 23.4 / 41.5 |
| 65536 | 494.0 / 0.682 / 13.036 | 27.6 / 33.6 | 26.9 / 33.6 |
| 98304 | 771.1 / 0.538 / 8.655 | 30.5 / 29.5 | 29.6 / 30.0 |
| 131072 | 78.1 / 15.271 / 16.631 | 19.6 / 45.7 | 19.6 / 45.7 |
| 262144 | 89.9 / 15.088 / 16.738 | 12.4 / 67.4 | 10.6 / 76.2 |
| 417792 | 130.6 / 2.652 / 16.111 | 2.2 / 457.3 | 2.6 / 374.4 |

FACT: at 417792 bytes (Hyper's own default bound) a proxied 64 MiB transfer
collapses from about 28 to 2.2-2.6 RPS with a median near 400 ms, while
Hamster's CPU per request falls.

FACT: isolation. With the upstream connection bound fixed at 64 KiB and the
downstream bound at 417792, GET 64 MiB recovered to 29.0 RPS while POST 64 MiB
stayed collapsed at 1.7 RPS. The collapse follows the reading side that has a
large buffer and is backpressured by the other connection: the upstream client
for downloads, the downstream server for uploads.

FACT: `TCP_NODELAY` on the downstream socket (Hamster's accepted sockets do not
set it; every other socket on this path does) changed nothing: 2.2 RPS and
424 ms at 417792, p95 14.5 ms for GET 1 MiB at 64 KiB. Reverted.

FACT: `timeBeginPeriod(1)` inside the Hamster process changed nothing: GET
1 MiB at 131072 still had a 15.095 ms median. Reverted.

The first-pass hypothesis recorded here ("the receive window closes and
reopening it waits on a TCP-stack timer") was tested and is superseded: the
receiver advertises an open window at once, and the wait is on the sending
side. See *Windows loopback send stall* below for the evidence and the
current wording.

Default: 64 KiB, the existing server default, is kept and also applied to
upstream connections. It is the current conservative Windows default,
supported by the loopback measurements in this file; it is not established as
a cross-platform optimum or an invariant. 98304 measured marginally better in
the probe above but within the spread of the 64 KiB runs; 32 KiB gives a much
tighter 1 MiB tail and less memory per exchange for about 20% less
large-transfer throughput at C1.

## Windows loopback send stall (investigation)

An investigation-only pass on the same machine asked why a large buffer bound
collapses proxied throughput and where the 1 MiB tail comes from. No
production behaviour was changed by it. Numeric data is in
`reverse_proxy_v1_windows_tcp_20260924/`; the pcap, ETW traces and per-request
samples (about 600 MB) are not kept, and the setup below is enough to
reproduce them.

### Setup

The buffer matrix and the controls used a scratch harness shaped like
`benches/proxy.rs`: the upstream is a Hyper 1.11.1 server in the bench process
that answers `GET` with a known `Content-Length` and a body of 64 KiB frames
(the shape `benches/proxy.rs` uses) and drains `POST`; the client is one Hyper
keep-alive connection per worker with `TCP_NODELAY`; Hamster is the release
binary, one fresh process per cell; 500 ms warm-up, 2.5 s window, three runs
interleaved across all cells. In-repo reproducers:

- `cargo bench --target x86_64-pc-windows-msvc --bench proxy --features profiling -- probe`
  with `PROBE_BUFFERS=32768,65536,131072,262144,417792` and `PROBE_CONCURRENCY`
  (it prints a direct baseline and the share of requests at or above 5 ms);
- `cargo bench --target x86_64-pc-windows-msvc --bench windows_tcp_stall --features profiling`,
  the pure-socket control below.

Tooling for the kernel-side evidence, all built into Windows or Npcap, run as
administrator:

- per-socket TCP state: `WSAIoctl(SIO_TCP_INFO)`, `TCP_INFO_v1`
  (`SndLimTimeRwin`, `SndWnd`, `RcvWnd`, `TimeoutEpisodes`, retransmits);
- packets: the Npcap loopback adapter `\Device\NPF_Loopback`, headers only;
- TCP stack events: `logman start <name> -p Microsoft-Windows-TCPIP
  0xffffffffffffffff 0xff -o <file>.etl -ets`, the workload, `logman stop <name>
  -ets`, `tracerpt <file>.etl -of XML`. The events used are 1159 (send posted),
  1160 (send transmitted), 1161 (send advance), 1587 (window-update ACK with
  `SndWnd`), 1319/1320 (TCP timer rescheduled/fired) and 1426 (send complete).
  The loopback connections ran the `Internet` TCP template (event 1223:
  `MinRto` 300 ms; `Get-NetTCPSetting` lists its delayed-ACK timeout as 40 ms).

### Reproduction matrix

Proxied through Hamster at each `http1_max_buffer_size`; medians of three
interleaved runs; zero errors in all 216 cell-runs. C1, RPS and p95 ms
(64 MiB rows: RPS and p50 ms):

| Cell | direct | 32768 | 65536 | 131072 | 262144 | 417792 |
|---|---|---|---|---|---|---|
| GET 1 MiB | 1943 / 0.48 | 778 / 1.31 | 391 / 14.6 | 92 / 16.3 | 113 / 16.2 | 229 / 15.6 |
| POST 1 MiB | 2055 / 0.41 | 891 / 1.26 | 951 / 1.21 | 89 / 16.3 | 86 / 16.7 | 147 / 16.1 |
| GET 8 MiB | 209 / 18.9 | 155 / 7.8 | 178 / 15.2 | 84 / 34.6 | 54 / 48.1 | 27 / 76.6 |
| POST 8 MiB | 262 / 17.6 | 154 / 7.3 | 192 / 6.4 | 80 / 32.4 | 49 / 49.0 | 22 / 79.0 |
| GET 64 MiB | 33.0 / 26 | 21.4 / 45 | 27.5 / 35 | 23.6 / 35 | 12.2 / 67 | 3.0 / 323 |
| POST 64 MiB | 33.4 / 28 | 21.1 / 45 | 28.1 / 35 | 22.6 / 33 | 9.9 / 95 | 2.7 / 360 |

C32, RPS (Hamster peak private MiB):

| Cell | direct | 32768 | 65536 | 131072 | 262144 | 417792 |
|---|---|---|---|---|---|---|
| GET 1 MiB | 6637 | 3678 (8.0) | 4050 (9.9) | 3923 (14.1) | 3480 (26.2) | 3319 (25.6) |
| GET 8 MiB | 756 | 396 (7.6) | 478 (9.9) | 441 (14.6) | 421 (25.3) | 383 (35.9) |
| GET 64 MiB | 91.6 | 52.7 (7.7) | 59.3 (10.0) | 53.9 (15.0) | 50.0 (25.3) | 44.3 (37.6) |
| POST 64 MiB | 78.8 | 48.0 (7.1) | 51.4 (8.8) | 46.1 (14.0) | 46.2 (25.4) | 43.4 (36.6) |

FACT: at C1 both directions collapse at 131072 and above, and at 417792 a
64 MiB transfer takes 320-360 ms instead of 35 ms. At C32 the collapse is
largely absent: every buffer is within 25% of the others and 65536 is the
fastest in every C32 cell, while peak private memory grows with the bound.

FACT: the collapse needs the upstream to write its body in pieces. The same
Hamster, same buffers, with an upstream that returns one 64 MiB `Full` body
(Hyper writes it as one large vectored write) did not collapse: 24.1 RPS at
417792 against 20.1 at 65536 in a one-run check.

Hamster CPU per request is in `buffer_matrix.csv` for completeness; at a few
requests per window it is too coarse to compare cells.

### Where the time goes

Layer controls, same workload (GET 64 MiB, C1, 417792 unless stated):

| Path | RPS |
|---|---|
| Hamster | 1.9-3.0 |
| Hyper-only mini-proxy (`server::conn::http1` to `client::conn::http1`, `Incoming` passed through, own process) | 1.3-2.4 |
| same mini-proxy inside the bench process | 4.4-5.0 |
| raw Tokio relay (`copy_bidirectional_with_sizes`, no HTTP) | 1.5-2.3 |
| upstream replaced by a blocking std-thread sender, Hamster | 2.4 |

Pure sockets, no HTTP, no Tokio (`benches/windows_tcp_stall.rs`): one thread
sends 458752-byte pieces nonblocking and spins on `WSAEWOULDBLOCK`; the other
reads into a buffer of the given size and busy-waits a fixed delay per read.
64 MiB per run, three runs:

| Read bytes | Delay | SO_RCVBUF | ms per 64 MiB | sender waits >= 5 ms |
|---:|---:|---|---|---|
| 65536 | 0 | default | 43-66 | 1-4 |
| 417792 | 0 | default | 16-80 | 0-5 |
| 65536 | 200 us | default | 219-235 | 0-1 |
| 417792 | 200 us | default | 496-611 | 31-39 |
| 417792 | 200 us | 1 MiB | 45-47 | 0 |

Explicit `SO_SNDBUF` on the sender (64 KiB to 4 MiB) did not change the
417792/200 us cell (451-827 ms).

FACT, from the I/O probe timeline (QPC timestamps on every socket poll in the
Hyper-only proxy and the bench): in each long gap the proxy's upstream read
had just returned `Pending` (its socket drained), the upstream application's
write was `Pending`, and no socket on the path saw any event for about 14 ms.

FACT, from the packet capture of the raw relay at 417792: before each gap the
sender's last segment is a short one with PSH (the tail of what it had
queued); the receiver ACKs everything within about 0.1 ms advertising a fully
open window (261888); then no packet flows for 10-17 ms. Of 102 such gaps
only 11 were preceded by a zero-window advertisement.

FACT, from `SIO_TCP_INFO` on the upstream socket: slow requests had 11-13.5 ms
of `SndLimTimeRwin` (sender limited by the receive window) against 0.2-0.4 ms
for fast ones, with no retransmissions and no timeout episodes.

FACT, from the spinning nonblocking sender: the kernel itself refused data
(`WSAEWOULDBLOCK`) for about 15 ms at a time (p90 15 ms, 171-175 episodes of at
least 5 ms per cell at 417792), so the wait is not a readiness notification
arriving late in mio or Tokio.

FACT, from ETW: in every stall examined -- 13 of 13, 2 of 2 and 3 of 3 in three
pure-socket configurations, and 33 of 33 on Hamster's upstream leg during a
GET 1 MiB C1 run -- the sending connection had data queued (`SendAvailable`
> 0), had received a window-update ACK with an open window (event 1587,
`SndWnd` 261888), and transmitted nothing until a TCP timer fired on the
sender's processor (event 1320). That timer had been scheduled 10 ms ahead
(`DueTime` -100000) and fired 4-15 ticks late. Hamster's downstream leg showed
the same 33 gaps as a consequence, 0.1-0.2 ms behind. In the pure-socket runs
14% of window-update ACKs with data pending were deferred this way with a
417792-byte reader, against 1.6% with a 65536-byte reader.

FACT: stall end times fall in a narrow band of the 15.625 ms clock period;
with `timeBeginPeriod(1)` in the sending process most stalls shortened to
9-12 ms, some still near 15 ms. (The same call inside Hamster, the receiver, changed nothing, as
recorded above.)

INFERENCE, high confidence: large or bursty reads under backpressure drain
the whole socket receive buffer, so the window-limited sender learns that the
window reopened only from a pure window-update ACK; on Windows loopback some of
those do not restart transmission at once, and the sender resumes at a later
TCP timer event, 10-17 ms later on this machine. Each such stall costs a whole
timer period; 20-50 of them per 64 MiB is the collapse, one of them in a
1 MiB request is the tail. A reader whose chunk is smaller than the socket's
autotuned receive buffer (131070 then 262140 here) rarely drains it and rarely
triggers the deferral, which is why 64 KiB behaves and 128 KiB and up do not.

FACT, therefore falsified for this phenomenon: a Hamster-specific cause, a
Hyper-specific cause, a Tokio-specific cause (all three controls reproduce it
without the respective layer), delayed ACK on the receiver (ACKs leave within
about 0.1 ms, and the template's delayed-ACK timeout is 40 ms), Tokio timer
resolution, Tokio cooperative yielding, and Nagle.

HYPOTHESIS, not verified: the exact rule by which Windows defers some window
updates to the timer (neither the sender's SWS-timer state nor cross-processor
delivery separated deferred from immediate ones); behaviour on a real NIC;
behaviour on Linux or any other TCP stack; the effect of CPU power
management (a global power-plan change was not made).

### Tail at 1 MiB

GET 1 MiB, C1, 64 KiB bound, all runs pooled (`tail_get_1mib_c1.csv`):

| Path | Upstream body | Requests | >= 5 ms | >= 25 ms |
|---|---|---:|---:|---:|
| direct | 64 KiB frames | 16578 | 1.57% | 0.01% |
| direct | one `Full` body | 7664 | 6.28% | 0.07% |
| Hamster | 64 KiB frames | 3901 | 12.18% | 0.23% |
| Hamster | one `Full` body | 7502 | 3.87% | 0.01% |
| Hyper-only mini-proxy | 64 KiB frames | 5291 | 16.27% | 0.25% |
| raw Tokio relay | 64 KiB frames | 3738 | 14.34% | 0.24% |
| mini-proxy in the bench process | 64 KiB frames | 3549 | 8.11% | 0.03% |

The slow requests take 5-17 ms, most of them 12-16 ms. The stall is not
Hamster's: direct connections show it, and every proxy shape shows it at a
similar rate. A proxy adds one more TCP leg that can stall, and its read
pattern changes how often the sender's window reopens by a pure window update.
It is a latency effect only: no error, no truncation, no invariant is involved.

### Upstream `SO_RCVBUF` (temporary experiment, not in production)

Hamster was rebuilt with an environment-controlled `SO_RCVBUF` on its upstream
sockets, measured, and the patch was reverted (source hashes verified). GET,
C1, medians of three interleaved rounds (`upstream_so_rcvbuf_experiment.csv`):

| Buffer | Payload | default | 1 MiB | 4 MiB |
|---:|---|---|---|---|
| 417792 | 64 MiB | 2.3 RPS | 25.9 RPS | 12.0 RPS |
| 417792 | 8 MiB | 21.3 RPS | 154.5 RPS | 114.4 RPS |
| 417792 | 1 MiB | 234 RPS, p99 19.9 ms | 1661 RPS, p99 1.08 ms | 1788 RPS, p99 1.00 ms |
| 65536 | 64 MiB | 26.2 RPS | 24.7 RPS | 16.0 RPS |
| 65536 | 1 MiB | 424 RPS, p90 10.8, p99 16.0 ms | 1487 RPS, p90 0.70, p99 1.07 ms | 1467 RPS, p99 1.08 ms |

FACT: a receive buffer larger than the read size removes most of the stall on
this path, confirming the mechanism on Hamster itself. FACT: larger is not
always better -- 4 MiB halved 64 MiB throughput at both bounds, and 1 MiB was
slightly slower than the default for 64 MiB at 65536.

Not measured, and the reasons this is not a production change: an explicit
`SO_RCVBUF` disables or changes Windows receive-window autotuning for that
socket, and it can raise kernel (non-paged) memory per connection, which the
process's private commit does not show. Only the GET direction (upstream
sockets) was tried; the downstream accept side, C32 and other networks were
not.

## Memory slope

Stalled download: each client sends `GET /64m` with a 64 KiB receive buffer
and never reads. Stalled upload: the upstream never reads `POST /hold`
(declared 1 GiB); each client writes until a write makes no progress for
300 ms. Hamster private commit is sampled 1.5 s after the last client is in
place, inside the 30 s progress timeout. Three runs, medians.

| Buffer | Stall | N | Hamster private MiB | delta vs N=0 MiB | per exchange KiB | client bytes accepted MiB |
|---:|---|---:|---:|---:|---:|---:|
| 65536 | download | 0 | 1.71 | 0.00 | 0.0 | 0.0 |
| 65536 | download | 1 | 1.96 | 0.25 | 252.0 | 0.0 |
| 65536 | download | 8 | 3.54 | 1.83 | 234.0 | 0.0 |
| 65536 | download | 32 | 7.88 | 6.16 | 197.1 | 0.0 |
| 65536 | download | 64 | 13.45 | 11.73 | 187.8 | 0.0 |
| 65536 | upload | 0 | 1.73 | 0.00 | 0.0 | 0.0 |
| 65536 | upload | 1 | 1.97 | 0.23 | 240.0 | 0.8 |
| 65536 | upload | 8 | 3.29 | 1.55 | 198.5 | 0.9 |
| 65536 | upload | 32 | 7.84 | 6.11 | 195.4 | 0.9 |
| 65536 | upload | 64 | 13.31 | 11.58 | 185.2 | 0.9 |
| 417792 | download | 0 | 1.73 | 0.00 | 0.0 | 0.0 |
| 417792 | download | 1 | 3.22 | 1.49 | 1528.0 | 0.0 |
| 417792 | download | 8 | 10.92 | 9.19 | 1176.5 | 0.0 |
| 417792 | download | 32 | 36.88 | 35.15 | 1124.8 | 0.0 |
| 417792 | download | 64 | 72.79 | 71.06 | 1137.0 | 0.0 |
| 417792 | upload | 0 | 1.71 | 0.00 | 0.0 | 0.0 |
| 417792 | upload | 1 | 3.24 | 1.53 | 1568.0 | 1.7 |
| 417792 | upload | 8 | 10.54 | 8.82 | 1129.5 | 1.9 |
| 417792 | upload | 32 | 35.83 | 34.12 | 1091.8 | 2.0 |
| 417792 | upload | 64 | 72.74 | 71.03 | 1136.5 | 1.9 |

FACT: memory grows linearly with the number of stalled exchanges and not with
payload size: about 185-252 KiB per exchange at 64 KiB and 1.1-1.6 MiB at
417792, for a 64 MiB download and a 1 GiB upload alike. Each stalled upload
client had only 0.8-2 MiB accepted end to end before the chain stopped reading.

INFERENCE: the per-exchange cost is dominated by Hyper's two connection
buffers (downstream and upstream) at the configured bound plus fixed task and
connection state. That is the structural bound the design claims:
`max_connections` times the per-exchange cost at the configured buffer.

The investigation repeated this with every buffer in the matrix (same method,
the download stall fetching a 64 MiB body of 64 KiB frames;
`reverse_proxy_v1_windows_tcp_20260924/stalled_memory.csv`). Private commit
per stalled exchange, download and upload:

| Buffer | KiB per exchange, N = 8, 32, 64 | N = 1 |
|---:|---|---|
| 32768 | 120-140 | 96-128 |
| 65536 | 185-224 | 196-228 |
| 131072 | 328-407 | 284-332 |
| 262144 | 619-698 | 656-664 |
| 417792 | 1076-1156 | 1500-1560 |

FACT: the per-exchange cost scales with the buffer bound and stays linear in
N; N = 1 is the noisiest (one exchange over a 1.6 MiB baseline). At
N = 64 the totals are 9.1-9.4, 13.2-13.7, 22.1-23.6, 40.3-43.0 and 68.9-70.0
MiB. This is process private commit only; kernel socket buffers are not
included.

## `max_idle` versus concurrency (GET 23 B, C128)

| max_idle | RPS | p50 ms | p99 ms | Hamster CPU us/req | new upstream conns in window |
|---:|---:|---:|---:|---:|---:|
| 64 | 59022.8 | 2.123 | 3.518 | 61.7 | 171 |
| 128 | 57980.3 | 2.159 | 3.648 | 65.1 | 2 |

FACT: concurrency above `max_idle` causes connection churn, and on loopback the
churn has no measurable throughput, latency or CPU cost. The default stays 64.

## Existing static path: no regression detected

`WriteProgressTimeout`, which every response uses, was refactored onto
`ProgressDeadline`, and `ResponseBody` gained a boxed variant.
`static_http.ps1` (200,000 requests, C32, three runs per arm): HEAD `a7a31b4`
built in a separate worktree against the working tree, four interleaved passes,
order swapped for the last two.

| Pass | HEAD RPS | working tree RPS | delta |
|---:|---:|---:|---:|
| 1 | 10835 | 9989 | -7.8% |
| 2 | 9383 | 9425 | +0.4% |
| 3 (swapped) | 9001 | 9058 | +0.6% |
| 4 (swapped) | 7902 | 7805 | -1.2% |

INFERENCE: the host drifted from 10.8k to 7.9k RPS during the session, far more
than any pairwise difference. No regression is detected; one below roughly 8%
would not be.

## Measurements taken inside the permanent tests

Debug build, `cargo test --lib proxy::tests:: -- --nocapture`:

| Test | Measurement |
|---|---|
| early response | 413 reached the client 280 us after the upstream sent it, with 256 KiB of 64 MiB uploaded |
| B' reclamation | upload stalled after 851,968 bytes; exchange reclaimed 814 ms after the client disconnected (1 s progress timeout) |
| slow upstream | client wrote 1.64 MB while the upstream consumed 0.81 MB |
| slow client | upstream wrote 1.90 MB while the client consumed 0.80 MB |
| graceful shutdown | returned 503 ms after the signal with a 500 ms drain bound and an endless upstream stream |
| refused connect | Windows reported the refusal after 2.04 s |

## Falsification

29 mutations of `src/proxy.rs`, each applied alone against the full proxy
suite, file restored afterwards (sha256 verified). All 29 were detected. M29
was added during the final review: the response-side "both lengths, never
reused" rule had no test of its own, so one was written and the mutation run
against it (it fails at the guard: the connection is parked, not closed).

| Mutation | Detected by |
|---|---|
| M01 collect the whole request body before sending | slow-upstream backpressure, early response, B', stalled upload, aborted upload, trailers |
| M02 return the lease at the response head | aborted download, stalled body, crash mid-response, early response |
| M03 reuse the connection of a failed body | stalled upstream body |
| M04 reuse the connection of a dropped body | aborted download |
| M05 retry a possibly-started bodiless request | never-sent-again, oversized head |
| M06 keep Connection-nominated fields | both hop-by-hop tests |
| M07 append to a spoofed X-Forwarded-For | forwarding metadata |
| M08 no upstream write-progress timeout | B' reclamation |
| M09 total deadline on the response body | slow-but-progressing response |
| M10 total deadline on the request body | progressing upload vs head timeout |
| M11 wait for the upload before the response | early response and six others |
| M12 head timer from send time | progressing upload vs head timeout |
| M13 no explicit chunked for unknown length | GET with a chunked body |
| M14 strip Trailer | trailers, hop-by-hop unit test |
| M15 downstream body error becomes a clean end | aborted chunked upload |
| M16 upstream body error becomes a clean end | crash mid-response (chunked) |
| M17 no response-body progress deadline | stalled upstream body |
| M18 no request-body progress deadline | stalled client upload |
| M19 no response-head timeout | missing response head |
| M20 no idle expiry | idle timeout |
| M21 no max_idle bound | max_idle |
| M22 forward Expect | expect-continue |
| M23 no response hop-by-hop normalization | hop-by-hop, never-sent-again, isolation |
| M24 pass an unrequested 101 | switching protocols |
| M25 trust HEAD Content-Length | conflicting HEAD lengths |
| M26 unbounded safe retries | second unsent failure |
| M27 forward absolute-form unchanged | absolute-form |
| M28 ignore preserve_host | absolute-form |
| M29 reuse a connection whose response carried both Transfer-Encoding and Content-Length | response framed by both lengths |
