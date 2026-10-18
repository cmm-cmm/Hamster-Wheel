# Structured Access and Error Log (0.8 slice)

Status: implemented in the working tree, default off, gated on Windows
`x86_64-pc-windows-msvc`. The release A/B benchmark required by `AGENTS.md`
for hot-path changes has been taken once, on a loaded machine, and rejects a
large regression without resolving a small cost (see *Measurement*). This is
the first
slice of the 0.8 scope in `doc/ROADMAP_0_5_TO_1_0.md`; metrics, health and
readiness, and transactional configuration reload are separate and still
missing.

## Why a per-request log at all

Until this slice, Hamster wrote only lifecycle lines: startup, worker
readiness, reload results, connection failures. A 500 reaching a browser left
nothing behind, and a pool degrading mid-afternoon was visible only if an
operator happened to run `--control status`. The 0.8 exit evidence requires
that "logging backpressure cannot block the request hot path and secrets do not
appear in diagnostics", so the design question was never whether to log, but
how to log without making the request path depend on a reader.

## Configuration

```toml
[server.access_log]
path = "logs/access.jsonl"   # omit for stdout; relative resolves beside this file
include_query = false        # default; see Redaction
queue_capacity = 1024        # 16-65536; owned records, so this is a memory budget
```

Absent section means absent feature: no queue, no writer task, and no
per-request work beyond one enum branch on the response body. The file is
opened once at startup, before pools start and before listeners bind, so a
path that cannot be written is a startup failure rather than a surprise under
load. An unknown key in the section is a configuration error, as everywhere
else.

## Record format

JSON Lines: one object per line, `kind` naming the record type. The format is
written by hand rather than through a serializer, because a log line is the
one place where an injected newline or quote must not be able to forge a
second record.

```json
{"kind":"access","ts_ms":1760000000123,"id":1,"method":"GET","target":"/index.html","host":"localhost","version":"HTTP/1.1","scheme":"http","peer":"127.0.0.1:51000","status":200,"head_us":412,"total_us":533,"bytes":14,"complete":true}
```

| Field | Meaning |
|---|---|
| `kind` | `access` for a request, `log_dropped` for the log's own note |
| `ts_ms` | Unix epoch milliseconds when the request was accepted by the handler path |
| `id` | Per-process request counter, starting at 1; the correlation key |
| `method`, `target`, `version` | As received; `target` is the path, plus the query only when configured |
| `host` | The authority the router validated. The only header value a record carries |
| `scheme` | `http` or `https` as established by the listener, never from a header |
| `peer` | The real socket address, not a forwarded one |
| `status` | The status actually sent |
| `head_us` | Microseconds from request start to response head |
| `total_us` | Microseconds to the last body byte, or to the point the body was dropped |
| `bytes` | Body bytes actually written to the client |
| `complete` | Whether the client received the whole body |
| `error` | The server's own label for an error status, when there is one |

`complete` is not read from the body's end-of-stream alone. Hyper stops
polling a body of declared length once those bytes are written and drops it
without a final end-of-stream, so a fully served file would otherwise be
recorded as incomplete; a response whose length was declared is complete when
that many bytes went out. A streaming body with no declared length is complete
only when it actually ended. This was found by the integration test, not by
reading the code.

`error` is the reason string the server already chose for that status —
`FastCGI Response Timeout`, `PHP Worker Queue Full`, `Not Found` — carried on
the response as an extension by the one constructor every error path uses
(`handler::simple_response`, and `static_files::text_response`) and taken off
again by the logger. No argument is threaded through routing, static files,
FastCGI, the worker pool or the proxy. It appears for any status at or above
400, so it is a label, not a fault signal: read `status` for severity. A 5xx
built outside those constructors still records `server error` rather than an
empty cause.

## What is never logged

No request or response headers other than the validated authority. No cookies,
no `Authorization`, no `User-Agent`, no request or response body. Query strings
are omitted entirely unless `include_query = true`.

With query logging enabled, values whose key is a credential are replaced with
`REDACTED`. Matching is case-insensitive and exact against a fixed list
(`token`, `access_token`, `refresh_token`, `id_token`, `api_key`, `apikey`,
`key`, `secret`, `password`, `passwd`, `pwd`, `signature`, `sig`, `auth`,
`authorization`, `session`, `sessionid`, `code`). Exact matching is deliberate:
a substring rule would redact `keyword` while still missing `x-token`.
Operators who need a different policy should leave query logging off — the list
is a safety net for a surface that is off by default, not a sanitizer.

## Not blocking the request path

The request path builds one owned record and offers it to a bounded queue with
a non-waiting send. A single writer task owns all formatting and every write,
so JSON building is not charged to the request either. Stdout goes through
Tokio's `io-std`, which performs the write on the blocking pool rather than on
a runtime worker; a stalled or closed log reader therefore cannot occupy a
thread that serves requests. This is the same failure shape the control-path
rehearsal already covers for lifecycle output.

When the queue is full the record is dropped and counted. It is never waited
on: a log is a report about traffic, never a condition for serving it. Drops
are then reported twice, so a gap in the log can never be mistaken for a gap in
traffic:

```json
{"kind":"log_dropped","ts_ms":1760000000456,"count":124}
```

and in the control endpoint:

```text
access_log=D:\site\logs\access.jsonl queued=0 written=5312 dropped=124 failed=0
```

`failed` counts records the target refused; the first such failure is also
reported once on stderr through `errln!`, never per batch.

Records queued when a graceful stop begins are flushed: the server drops its
sink only after every connection task has ended, and then waits for the
writer. A stop does not silently discard the last requests served.

## Rotation and ownership

Hamster does not rotate, truncate or reopen the log file; it appends. A
Service deployment already owns stream rotation through its wrapper, and the
WinSW example in `doc/WINDOWS_OPERATIONS.md` rotates the streams it owns. An
operator who wants Hamster's own file rotated must do it out of process, and a
rename or deletion under a running server leaves records going to the old
handle until restart. Reopen-on-signal is deliberately not implemented in this
slice; it belongs with the general configuration reload that 0.8 still owes.

## Evidence

Unit tests in `src/access_log.rs`:

- a client-controlled target cannot forge a second record (newline, quote,
  backslash, tab, control-character and non-ASCII escaping);
- credential-shaped query values are replaced and ordinary ones are kept,
  including `keyword=key`, `flag`, `token=` and `sig=a=b`;
- a query string is omitted unless configured, and included redacted when it
  is;
- a record is one line carrying every documented field;
- ids are unique per request;
- a full queue drops and counts instead of waiting;
- a drop note reports the count;
- only a server fault gets a default reason.

Integration tests in `tests/access_log.rs`, each against a real listener and
reading the file the server actually wrote:

- two requests become two records with ids 1 and 2, correct status, byte count,
  real peer, finite timings, and `error` only on the 404;
- an `Authorization` header, a `Cookie` and a `User-Agent` reach no record;
- `include_query` controls the query, and the token in it is replaced;
- a refused upstream records the reason the status was chosen;
- a client that abandons a 2 MiB response is recorded `complete:false` with
  fewer bytes than the file;
- a 200-request burst within the queue loses no record and reports no drop;
- a 200-request burst against the smallest queue (16) still answers every
  request 200, and written plus dropped equals the traffic, with the gap
  reported in the log itself.

Configuration tests in `src/config.rs` cover the absent section, the stdout
default, relative-path resolution and every rejected value.

Gates run at this working tree: `cargo fmt --all -- --check`; strict MSVC
Clippy `--all-targets --all-features -D warnings`; the default suite, 260
tests; ignored Windows suites `windows_control` 14/14, `windows_headless` 2/2,
`windows_containment` 4/4, `tls_reload_process` 3/3; the non-Windows cfg-flip
compile check; MSVC release build; and `cargo bench --no-run --features
profiling` linking all 11 benches.

## Measurement

`benchmarks/static_http.ps1` now takes `-ConfigPath`, which is what makes an
A/B possible at all; the default stays the built-in configuration so every
earlier run in `doc/BENCHMARKS.md` remains comparable. Two arms differing only
by the `[server.access_log]` section are checked in as
`benchmarks/access_log_off.toml` and `benchmarks/access_log_on.toml`.

The first comparison is recorded in `doc/BENCHMARKS.md` under *Structured
access log A/B — order-balanced paired run (2026-10-12)*: five pairs of
100,000-request runs with the arms alternating, three pairs running the
baseline first and two running the logging arm first.

That design was not the original plan. Three pairs run in one order made the
logging arm look 3-9% **faster**, which a change that only adds work cannot
be; the ten runs in execution order show a monotonic warm-up drift that
favours whichever arm ran second, and reversing the order reversed the sign.
Order-balanced, the RPS difference is **-0.21%** against an ordering bias of
**±5.6%**, CPU per request is **+19.8 µs (+2.0%)** on a 1,001 µs baseline, and
the working set is **+0.3-0.4 MiB** — the queue and the writer buffer, and the
only drift-free signal of the three.

So: no material static-path regression, and a small cost is not excluded. The
CPU figure is a direction, not a measurement. The machine was an i5-1235U at
81% background load, producing 3.0-4.0k RPS where this project's 0.5 runs
recorded 6.9-7.3k at the same concurrency, so **no absolute number from this
run may be compared with any earlier entry**. A clean machine at the
historical 500,000-request parameters is still owed before any cost claim.

Separately, and not a timing claim: the three logging runs wrote 303,000
records for 3 × (100,000 + 1,000 warm-up) requests, with zero `log_dropped`
records and every record `"status":200`. The default queue lost nothing at
that rate through the real release binary.

## Known limits

- One format (JSON Lines) and one record type for requests. No combined/NCSA
  format, no field selection.
- No lifecycle or error records of their own yet: lifecycle output still goes
  to stdout/stderr as operational lines. The `error` field is a request-scoped
  label, not a separate error stream.
- No generation identity, because there is no configuration reload to have
  generations. The `id` field is per process; it restarts at 1 on restart.
- No rotation, no reopen, no size or age bound inside Hamster.
- No sampling and no per-location control: a configured log logs every request.
- Linux is unverified, as everywhere else in this project.
