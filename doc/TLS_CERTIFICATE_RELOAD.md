# Transactional certificate reload

Status: implemented in the working tree after the Windows TLS v1 technical
review. This capability changes only the certificates and private keys used by
the HTTPS listener. It does not reload listeners, virtual hosts, routes, names,
timeouts, workers or upstreams. No commit, tag or release is implied.

## Operator contract

Replace every PEM certificate and key at the paths already configured in
`[[server.tls.certificates]]`, then request a reload. On Windows, press
**Ctrl+Break** in the server's attached console; **Ctrl+C** still shuts down.
On Linux, send **SIGHUP** to the server process; **SIGINT** still shuts down.
The trigger is explicit, so writing several files does not publish a partial
set. There is no polling. On Windows without a console, configure
`[server.control]` and use `hamster-wheel.exe --config <file> --control
reload-certificates`; see `WINDOWS_OPERATIONS.md` for pipe permissions and
`ERR reload_failed` rollback evidence. Native Linux remains NOT VERIFIED.

A console delivers Ctrl+Break to every process attached to it. PHP workers are
spawned with their own windowless console (`CREATE_NO_WINDOW`), so the event
reaches Hamster only: workers and the requests they are executing continue
across a reload. Until 2026-09-28 workers shared Hamster's console and ended on
the same key press, so every reload on a server with PHP pools answered its
in-flight PHP requests with 502 and replaced every worker.

The server logs either `TLS certificate reload succeeded` followed by each
certificate's names and new expiry, or `TLS certificate reload failed; previous
set remains active` with the offending field and file. A failure does not
close the listener. Repair the files and trigger again. Private key contents
are never included in diagnostics.

## Snapshot and rollback

`TlsListener` retains the validated certificate paths and configured names.
Reload runs `compile_set` on a blocking worker: it reads every certificate and
key, checks key matching, configured SAN coverage, dates, leaf EKU, bundled
issuer CA status and chain linkage with the same loader used at startup, then
builds a complete rustls `ServerConfig`, SNI resolver, Host authority scopes and
expiry reports. If any step fails, the candidate is dropped and the current
set is unchanged. Reload requests are serialized by an async mutex whose
owned guard travels with the blocking job. Cancelling the caller cannot
release serialization while queued/running certificate work remains; on
success the guard is retained through adoption to prevent out-of-order swaps.

The serving set is one `Arc<CertificateSet>` behind a short `RwLock`. The lock
is held only to clone or swap this pointer; no file I/O, parsing or handshake
is performed while holding it, and the previous generation is dropped after
the swap lock is released. A handshake takes one snapshot before reading
the ClientHello and holds it through scope selection. The acceptor's resolver
and the scope lookup therefore belong to the same generation. Existing TLS
connections keep their stream and authority scope; new handshakes after a
successful swap take the new set. Shutdown cancels the signal task; a blocked
file load cannot publish after its caller is cancelled. Cancellation does not
cancel native file I/O or queued `spawn_blocking` work; at most one compilation
per listener remains outstanding, and later reloads wait instead of enqueueing
more. Keep certificate files on reliable local storage; stalled native I/O
and runtime termination are not claimed to have a hard real-time bound.

Each new rustls `ServerConfig` starts with a fresh in-memory session cache;
resumption from the previous generation is not retained. The listener
address, total handshake deadline, connection limit, write progress timeout,
ALPN, SNI/Host policy, graceful shutdown and cleartext path are unchanged.

## Verification

Windows real-socket tests in `src/server/tls_tests.rs` cover:

- a valid reload, a verifying client seeing the new certificate, an older TLS
  keep-alive connection still serving, and cleartext still serving;
- two SNI entries, a mismatched key or SAN in one entry rolling back the entire
  set, default certificate selection, authority scope on an older connection,
  and two successful generations;
- a handshake paused after the server captures its snapshot but before the
  ClientHello completes: it finishes with the old certificate while a new
  handshake receives the new one.

The reload signal is registered in `server::run` before the readiness lines.
The Windows process test `tests/tls_reload_process.rs` launches the actual
server binary with `CREATE_NEW_CONSOLE`, then starts a helper that calls
`AttachConsole(server_pid)` and
`GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0)`. The helper installs a console
handler for its own Ctrl+Break event. Run it with:

```text
cargo test --test tls_reload_process ctrl_break_reloads_a_real_server_process_and_rolls_back_on_error -- --ignored --nocapture
```

This process test is ignored by the default suite because it creates a child
console; the command above was run explicitly and passed.

Observed on Windows: after the first event, stdout reported `TLS certificate
reload succeeded (1 certificate(s))`; a verifying TLS handshake presented the
new leaf, while a keep-alive TLS connection established before reload still
served a second HTTP request. With a mismatched key, stderr reported `TLS
certificate reload failed; previous set remains active` and included both
configured file paths plus `cannot be used together`. With malformed cert PEM,
stderr again reported the rollback and identified the cert file and missing
`CERTIFICATE` block. After each failure the process remained alive, new
handshakes still presented the last good leaf, and the old connection served
another request. The test killed and reaped its child after assertions.

The first test attempt failed because its helper used
`SetConsoleCtrlHandler(None, 1)`, which ignores Ctrl+C but not Ctrl+Break; the
helper itself exited on the event. Installing an explicit handler fixed the
test harness. No production reload defect was found. Linux SIGHUP has not
been exercised in an end-to-end process test.

Two later process regressions (2026-09-28) found two defects outside the reload
logic itself, both now fixed:

- `ctrl_break_reloads_after_stdout_is_closed` closes the server's stdout and
  sends Ctrl+Break twice. Before the fix, the first reload's success line
  panicked the reload task (`failed printing to stdout: The pipe is being
  closed`); with that task gone nothing handled the second Ctrl+Break, and
  Windows ended the process with `STATUS_CONTROL_C_EXIT` (0xC000013A).
  Operational output now drops a line it cannot write (`src/output.rs`).
- `ctrl_break_reload_leaves_php_workers_serving` in `tests/php_worker.rs`
  reloads a server with a two-worker PHP pool while a request is executing.
  Before the fix that request returned `502 PHP Worker Unavailable` because the
  console event ended `php.exe`; now it returns 200, both workers keep their
  process, and on the following Ctrl+C each exits 0 through SHUTDOWN.

Windows gates on the final implementation: `cargo fmt --all -- --check`,
`cargo test` (144 library passed, 9 ignored; 30 configuration, 7 routing and 5
static tests passed), strict `cargo clippy --all-targets --all-features -- -D
warnings`, MSVC release build and `cargo bench --no-run --features profiling`
all passed. The ignored PHP listener-scheme regression also passed with the
local PHP distribution. No benchmark was run: reload loads files on a
blocking worker and the request path is unchanged. The snapshot adds only one
short pointer read per TLS handshake; its performance has not been measured.

Native Linux build and tests remain **NOT VERIFIED** on the current machine:
only the `x86_64-pc-windows-msvc` Rust target is installed. The available
`docker-desktop` WSL distribution has no Cargo, and its Docker daemon is not
running. No usable native Linux build environment was available.

## Limits and next gate

Configured names, certificate count and file paths remain fixed until restart.
An existing connection can continue to use its prior certificate and scope
until it closes, as required by the no-disruption contract. Certificate
validity is checked when the candidate is loaded; a later expiry still needs a
new reload. The operator must finish replacing all files before triggering.
Reload is not an ACME client, a file watcher or a general configuration reload.
Before Windows Service support, choose a service-capable trigger; Ctrl+Break
requires an attached console. Native Linux verification and a Linux SIGHUP
process test remain qualification work for this capability.
