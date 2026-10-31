# Security Review (0.9), Windows first

The 0.9 scope asks for a "security review of HTTP parsing policy, forwarding
trust, filesystem containment, configuration secrets, admin exposure and
backend isolation". This is that review for the Windows target, which is the
qualified platform (`doc/AI_HANDOFF.md`, *Verified Platform*). Where a surface
behaves differently off Windows it says so; Linux is reviewed only where the
code differs, because nothing here was run there.

Each section states the policy, cites what enforces it, names the test that
proves it, and ends with what is **not** covered. A review that only listed
strengths would be an advertisement.

## One finding, fixed in this pass

**Configuration parse errors echoed the configuration.** A TOML syntax error
printed the offending source line verbatim, and
`[hosts.locations.php] env` is where an application's database password and
app key are written. An invalid escape inside such a value printed:

```text
14 | env = { DB_PASSWORD = "s3cret-production-password\q", APP_KEY = "x" }
```

to stderr — which a supervisor captures and a log shipper can forward off the
host. The diagnostic now keeps the position and the parser's reason and drops
the snippet:

```text
failed to parse configuration <path>: TOML parse error at line 14, column 51:
missing escaped value, expected `b`, `e`, `f`, `n`, `r`, `\`, `"`, `x`, `u`, `U`
```

The filter keeps the first and last lines of the parser's own text rather than
matching the snippet's shape, because a future `toml` release could render the
snippet differently and a filter that stopped matching would start echoing the
file again. Regression test:
`a_parse_failure_names_where_it_happened_without_echoing_the_file`.

## HTTP parsing policy

Hyper parses; Hamster decides what is admissible and bounds what a connection
may consume. Rejections, each with a test: path traversal in its three forms
(403), an unknown host (404), a missing `Host`, `Content-Length` together with
`Transfer-Encoding`, and a garbage request line (400), and an oversized body
(413). Bounds, all configurable and all compiled once: `max_connections`,
`header_read_timeout`, `body_read_timeout`, `write_progress_timeout`,
`max_body_memory` (one process-wide budget, shared by every pool) and
`http1_max_buffer_size`.

Two policies worth stating because they are easy to get wrong:

- **A request target is canonicalized per request**, and containment is
  decided after that, never before.
- **Peer-controlled connection errors are not logged synchronously.**
  `should_report_connection_error` filters parse errors, timeouts, incomplete
  messages, closed and cancelled connections and peer TLS faults, because
  logging every one would turn malformed traffic into a stderr backpressure
  path. The counters still move, so the traffic is visible without being a
  lever.

Not covered: HTTP/2 does not exist yet, so none of this has been reviewed for
it. No fuzzing of the request line, headers or chunked framing has been run.

## Forwarding trust

Nothing a client sends decides identity or scheme. `TransportInfo` carries the
accepted socket's peer and local addresses and `Scheme` is set by the listener
that accepted the connection, so `https` means Hamster completed a TLS
handshake on it — an absolute-form `https://` target or an
`X-Forwarded-Proto` header cannot produce it. Forwarded headers reach an
application as ordinary untrusted input, and the PHP worker's server bag gets
`REMOTE_ADDR`/`PORT` and `SERVER_ADDR`/`PORT` from the socket.

Tests: `laravel_receives_owned_metadata_and_does_not_trust_forged_forwarding_headers`
and `laravel_sees_https_from_the_tls_listener_and_not_from_a_forged_header`,
plus the protocol-v2 regressions in `tests/php_worker.rs` that drive real
`.2`/`.3` loopback peers.

Not covered: there is **no trusted-proxy policy**. Behind a reverse proxy an
application sees the proxy's address, and Hamster offers nothing to translate
that. `doc/APPLICATION_REQUEST_METADATA.md` records this as separate work.

## Filesystem containment

On Windows the decision is made on an **open handle**: the candidate is
opened, `GetFinalPathNameByHandleW(FILE_NAME_NORMALIZED | VOLUME_NAME_DOS)`
gives the final location, and that is compared against the canonical root. A
junction, a symlink or a rename between resolution and open therefore cannot
move the served file outside the root: whatever was opened is what is
validated. Index selection still uses a pathname lookup, which is selection
rather than containment.

Access rules on top of containment: dotfiles are denied except a root
`/.well-known`, PHP source is never served as static bytes, and host-wide
`deny_paths` and `deny_names` apply to static, FastCGI static and FastCGI
script resolution alike. `doc/STATIC_CONTAINMENT.md` is the full contract;
`tests/static_files.rs` and the FastCGI suites hold the tests, including
`rejects_path_traversal`.

**Off Windows the model is weaker and this is the review's standing item.**
The file is opened by its canonical pathname after the check, so a namespace
change between the two is not excluded. `doc/LINUX_VERIFICATION.md` and
`doc/ROADMAP_0_5_TO_1_0.md` both carry it. Closing it needs an
`openat`-style resolution or a post-open verification of the opened inode, and
neither is written.

Not covered: no test runs a concurrent namespace change against either path.
The Windows property is argued from the API contract, not from a race
harness.

## Configuration secrets

What the configuration can hold: TLS certificate and key **paths**, a PHP
worker's `env` table, and whatever an application's own files hold. What
Hamster does with them:

- **Keys and certificates are read, never printed.** Reload reports a field
  name, the names it covers, `not_after` and days left — never key material.
- **`env` values are never printed.** `--validate` reports a pool's label,
  worker count, queue capacity and timeout; `status` reports counts and PIDs;
  the exposition labels series by configured names. No surface prints `env`.
- **The access log never writes a header value other than the validated
  authority**, no cookies, no `Authorization`, no bodies, and no query string
  unless an operator asks — in which case credential-shaped keys are replaced
  (`doc/ACCESS_LOG_0_8.md`).
- **Parse diagnostics no longer echo the file**, as above.

Not covered: Hamster does not check the permissions of the files it reads. A
world-readable private key or configuration is the operator's to fix, and
nothing warns about it. There is no secret store integration and no way to
keep a worker's `env` out of the configuration file other than putting it in
the application's own environment.

## Admin exposure

Two administrative surfaces, both **off unless configured**:

- **The control endpoint** is a Windows named pipe whose DACL is set at
  creation and protected: `D:P(A;;GA;;;SY)(A;;GA;;;OW)`, LocalSystem and the
  pipe's owner only. Remote clients are rejected and the first instance claims
  the name. A thread impersonating a token restricted to `Everyone` is refused
  `ERROR_ACCESS_DENIED` while the owner keeps access
  (`the_endpoint_denies_a_token_restricted_to_everyone`). Anyone who can open
  it can stop the server, and the runbook says so.
- **The administrative listener** is a separate socket serving exactly
  `/health`, `/ready` and `/metrics`, never the application router — a test
  asks the application listener for all three and requires the application's
  own 404. It has **no TLS and no authentication**, and a non-loopback bind
  logs a warning naming what became reachable.

What a scrape reveals to whoever can reach that socket: pool labels, upstream
labels, FastCGI endpoint labels and certificate field names, all of which are
host and path names from the configuration, plus expiry days and traffic
counters. No request paths, no peers, no header values.

Not covered: no authentication, no IP allow-list and no TLS on the admin
listener; the boundary is the network's. Cross-account denial on the control
pipe has been tested with a restricted token, **not** with a second real user
account, and nothing has been run under a Windows Service account beyond the
scoped WinSW qualification (`doc/WINDOWS_SERVICE_LIFECYCLE_GAPS.md`).

## Backend isolation

A PHP worker is a separate process, spawned directly with a trusted
executable and script path — never through a shell, never resolved from
`PATH`. One worker serves one transaction at a time, state is reset between
requests, and a crash answers 502 for that request and replaces the slot
without touching other workers. Execution past `request_timeout` kills and
replaces the worker, and a transaction is **never retried**: at-most-once is
the contract (`doc/EXECUTION_TIMEOUT_POLICY.md`,
`doc/WORKER_LIFECYCLE_0_3.md`).

On Windows a kill-on-close Job Object is established **before** any worker is
spawned, and startup fails closed if it cannot be: no uncontained fallback.
Ordinary descendants of a worker are reclaimed with it.

FastCGI endpoints are operator-owned: Hamster connects to them and never
starts or supervises them, so they are outside the Job Object and need their
own containment and restart policy. With `keep_connections` a connection is
reused only after a clean END_REQUEST with nothing buffered after it and the
whole request body sent, checked with a non-blocking peek before reuse, so
reuse cannot splice two exchanges.

Not covered: Linux has **no equivalent of the Job Object**
(`doc/LINUX_VERIFICATION.md`): if Hamster is killed, its workers exit when
they next read EOF on stdin, and a worker inside a long request outlives the
server until that request ends — stated there by inference, not tested. No
privilege separation: Hamster and its workers run as the same account, and
nothing drops privileges.

## What this review did not do

- No fuzzing, no race harness, no penetration test and no third-party review.
- No audit of the dependency tree beyond what `Cargo.lock` pins.
- Nothing on Linux beyond reading the code paths that differ.
- No review of HTTP/2, which does not exist, or of TLS client authentication,
  which is not implemented.
