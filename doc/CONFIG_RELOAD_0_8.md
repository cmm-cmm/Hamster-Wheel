# Transactional Configuration Reload (0.8 slice)

Status: implemented in the working tree for the changes a reload can make
safely, and refusing every other change by name. Gated on Windows
`x86_64-pc-windows-msvc`. This is the third slice of the 0.8 scope in
`doc/ROADMAP_0_5_TO_1_0.md`, after `doc/ACCESS_LOG_0_8.md` and
`doc/OBSERVABILITY_0_8.md`, and it closes the "CLI operations for validate,
serve, status, reload and stop" item together with `--validate`.

## What a reload is here

```powershell
hamster-wheel.exe --config D:\site\hamster.toml --control reload
```

The server re-reads the file it was started from, compiles it, and either
publishes it as the next **generation** or refuses it and keeps serving the
one it has. A refusal costs the serving generation nothing: it happens before
anything is built.

```text
OK
pid=21652
generation=2
previous_generation=1
```

```text
ERR reload_failed
message=cannot reload in place: server.listen changed, which owns a socket, a pipe, a file or a client that a reload must not replace; restart to apply it
active_generation=2
```

## The boundary, and why it is where it is

The roadmap asks to "define which listener changes require a handoff rather
than an in-process swap". The answer this slice implements is wider than
listeners and simple to state:

> A reload may change the route graph and the HTTP limits that are read per
> connection. It may not create, destroy or alter anything that owns
> operating-system state.

Everything in the second category is **carried** from the serving generation
into the published one, as the same object: PHP worker pools (so warm workers
stay warm and in-flight transactions keep their at-most-once guarantee),
proxy upstreams with their idle connection pools and counters, FastCGI
endpoints, the TLS listener with its certificate generation and handshake
counters, the control pipe, the administrative listener, and the access log's
file and queue. Each is fingerprinted from the settings it was built with, and
a configuration that disagrees is refused naming the resource:

| Change | Answer |
|---|---|
| static root, a new or removed static/not-found location, `deny_paths`, `deny_names`, host aliases, a new host whose locations own nothing | published |
| `header_read_timeout`, `body_read_timeout`, `write_progress_timeout` | published, and read by connections accepted afterwards |
| `server.listen`, `server.tls`, `server.control`, `server.admin`, `server.access_log`, `server.max_body_memory`, `server.http1_max_buffer_size` | refused, naming the field |
| a PHP pool, proxy upstream or FastCGI endpoint that is new, removed, or whose settings changed | refused, naming the resource |
| `max_connections` | parsed, but the accept loop reads it once at startup, so a change needs a restart |

So a reload is for routing and static serving, which is the half that does not
need a process restart; anything that would start, stop or re-point a process,
socket, file or client is deliberately a restart. That is not a limitation
discovered late: creating or retiring those resources safely needs drain
semantics for in-flight requests that this slice does not implement, and
pretending otherwise would put the at-most-once guarantee at risk.

`max_body_memory` is in the refused list because the body budget is one
allocation shared by every pool for the life of the process, and
`http1_max_buffer_size` because it is baked into each upstream's client at
construction.

## Generations

`RuntimeConfig` carries a generation number, starting at 1 and incrementing
with each published reload. A connection reads the generation that is current
**when it is accepted** and keeps it until it ends, so a request always
finishes under the configuration that admitted it, and a keep-alive connection
does not change behaviour mid-conversation. Connections accepted after a
publish see the new generation.

The generation is reported everywhere a reader might need to place a report in
a configuration:

- `--control status` reports `generation=`;
- `/metrics` reports `hamster_config_generation`;
- every access-log record carries `"generation"`, beside its `"id"`.

A reload is serialised by its own gate, so two concurrent reloads cannot both
read the serving generation and publish over each other.

## Certificates are a separate reload

`--control reload-certificates` and Ctrl+Break still reload the TLS
certificate *contents* transactionally (`doc/TLS_CERTIFICATE_RELOAD.md`). A
configuration reload carries the TLS listener unchanged and refuses any change
to the `[server.tls]` section, including adding or removing a certificate
entry or changing a path. The two operations are independent on purpose: one
changes files the listener reads, the other changes the route graph above it.

## Operating it

Validate first. `--validate` compiles the file without starting anything, so
it catches the errors a reload would refuse anyway, while the server is
serving:

```powershell
hamster-wheel.exe --config D:\site\hamster.toml --validate
hamster-wheel.exe --config D:\site\hamster.toml --control reload
```

Keep `[server.control]` intact in the file. The control client reads the
endpoint name **from the configuration file**, so a file broken badly enough
to lose that section — or to stop parsing as TOML — cannot be used to reach
the server at all, including to stop it. That is a property of the client, not
of the reload, and it is the strongest reason to validate before writing over
a serving configuration.

A refused reload is not a degraded state: the answer names the serving
generation, the server is unchanged, and the operator can fix the file and try
again, or plan a restart for the change that needs one.

## Evidence

Unit tests in `src/config.rs`:

- a reload publishes generation 2, then 3, while the generation that asked for
  it stays complete; a file that no longer compiles is an ordinary validation
  error;
- every process-owned field is refused by name (`server.listen`,
  `server.admin`, `server.access_log`, `server.max_body_memory`), and a
  refusal is one sentence on one line with no run of spaces — a lost string
  continuation once left indentation inside that message;
- a proxy upstream is **the same object** after a reload that changed a static
  root beside it (`Arc::ptr_eq`), while changing it, removing it or adding one
  is refused with the label and whether it was new, changed or retired;
- the built-in configuration, and any in-memory compilation, has no file to
  re-read and says so.

Integration tests in `tests/reload.rs`, against a real listener and reading
the access log the server wrote:

- a connection kept open across a publish still serves the old root, a
  connection opened afterwards serves the new one, and the three records read
  `generation` 1, 1, 2 — which is the 0.8 exit evidence that accepted requests
  finish under the generation that admitted them;
- an invalid file and a listener change both leave the serving generation at 1
  and the server answering as before.

Process-level, in `tests/windows_control.rs` (ignored, starts a detached
server): `status` reports `generation=1`; a static-root change is published
and answered `generation=2 previous_generation=1`; `status` then reports
generation 2 and HTTP serves the new root; an invalid file is answered
`ERR reload_failed` with `active_generation=2`; a listener change is refused
naming `server.listen`; the server keeps serving generation 2 throughout and
then stops with exit code 0.

A release-binary probe confirmed the same sequence end to end, including
`hamster_config_generation` moving from 1 to 2 in a scrape.

Gates at this working tree: `cargo fmt --all -- --check`; strict MSVC Clippy
`--all-targets --all-features -D warnings`; the default suite, 296 tests;
ignored suites `windows_control` 15/15, `windows_headless` 2/2,
`windows_containment` 4/4, `tls_reload_process` 3/3, `validate_cli` 1/1; MSVC
release build.

## Known limits

- No resource lifecycle: a reload cannot start, stop or re-point a pool,
  upstream, FastCGI endpoint, listener, log or pipe. Those are restarts.
- `max_connections` is read once by the accept loop.
- No reload over the administrative listener and no signal-triggered
  configuration reload: Ctrl+Break remains certificates-only.
- No automatic rollback after a publish, because a published generation owns
  nothing new to roll back; the previous generation's route graph is dropped
  when its last connection ends.
- The control client needs the configuration file to find the endpoint, as
  above.
- Linux is unverified, as everywhere else in this project.
