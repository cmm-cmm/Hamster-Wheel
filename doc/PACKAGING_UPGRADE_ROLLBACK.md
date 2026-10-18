# Packaging, Upgrade and Rollback

Status: this is the 0.9 scope item "cross-platform packaging, upgrade/rollback
instructions and compatibility matrices", written from what was measured on a
Windows host and from `doc/LINUX_VERIFICATION.md` for Linux. There is no
installer, no package and no signing, and this document does not pretend
otherwise: it describes deploying a build, replacing it, and going back.

Every fact below was checked on this tree unless it is marked otherwise.

## What a deployment is

A deployment is a directory. For a static-only server it is three files:

```text
hamster-wheel.exe
hamster.toml
public/index.html
```

That was run: the release binary validated and served from a directory
containing exactly those files. What a configuration adds to the set:

| Configuration | Also needs deploying |
|---|---|
| `handler = "php_worker"` | the PHP distribution and the worker script named by `executable` and `script` (`fixtures/php-worker/worker.php`, or an application's own adapter) |
| `handler = "fastcgi"` | nothing of Hamster's; the operator starts and supervises `php-cgi` separately (`doc/FASTCGI_V1.md`) |
| `handler = "proxy"` | nothing; the upstream is someone else's process |
| `[server.tls]` | the certificate and key files |
| `[server.access_log]` with `path` | a writable directory for the file and its archives |
| A Windows Service through WinSW | the wrapper executable, its XML, and the compiled companion extension (`doc/WINDOWS_SERVICE_LIFECYCLE_GAPS.md`) |

Relative paths in a configuration resolve beside that configuration file, so a
deployment directory moves as a unit.

## The runtime the binary needs

The release binary imports, as linked by this tree on
`x86_64-pc-windows-msvc`:

```text
ADVAPI32  BCRYPTPRIMITIVES  DBGHELP  KERNEL32  NTDLL  WS2_32
API-MS-WIN-CORE-SYNCH-L1-2-0
API-MS-WIN-CRT-*  (eleven of them)
VCRUNTIME140.DLL
```

Everything there ships with Windows **except `VCRUNTIME140.dll`**, which comes
from the Visual C++ redistributable. A deployment therefore needs that
redistributable installed, or the DLL staged beside the executable.

A static CRT removes the dependency. Built with
`RUSTFLAGS=-Ctarget-feature=+crt-static`, the same tree imports only
`ADVAPI32`, `API-MS-WIN-CORE-SYNCH-L1-2-0`, `BCRYPTPRIMITIVES`, `DBGHELP`,
`KERNEL32`, `NTDLL` and `WS2_32` — no `VCRUNTIME140`, no `api-ms-win-crt-*` —
costs 208 KB (7,171,584 against 6,963,200 bytes), and validates and runs. That
is a measured option, not a recommendation this document can make on its own:
every benchmark in `doc/BENCHMARKS.md` was taken with the dynamic CRT, so
switching the shipped build is a performance-relevant change that needs its own
A/B before it is used for comparisons.

On Linux, `doc/LINUX_VERIFICATION.md` records a release build against glibc on
Ubuntu 24.04 with PHP built from source. musl and aarch64 are **NOT VERIFIED**.

## Identifying what is deployed

This is the weakest part of the current artifact, and it is a release blocker
rather than a detail:

- **The executable carries no version resource.** `icon/hamster-wheel.rc`
  contains only `1 ICON`, so Windows file properties show an empty
  `FileVersion`, `ProductVersion`, `CompanyName` and `FileDescription`. A
  deployed `hamster-wheel.exe` cannot be identified by looking at it.
- **The version it does report is stale.** `Cargo.toml` says `0.3.1`, so the
  `Server:` header, `--validate` output and the `started` lifecycle record all
  say `HamsterWheel/0.3.1` on a tree that contains the 0.5 through 0.8 work.
  Two different builds months apart report the same string.

Until both are fixed, record the commit you built from, outside the artifact,
and keep it with the deployment. The fixes are small and independent: a
`VERSIONINFO` block in the `.rc`, and an owner decision on the version number.

## Upgrading

The sequence below was run end to end on Windows with two builds.

```powershell
# 1. Stage the new build beside the running one. The running image cannot be
#    overwritten - "Device or resource busy" - but it can be renamed.
Move-Item .\hamster-wheel.exe .\hamster-wheel.exe.old
Copy-Item <new>\hamster-wheel.exe .\hamster-wheel.exe

# 2. Check the live configuration with the NEW binary, before anything stops.
.\hamster-wheel.exe --config .\hamster.toml --validate

# 3. Stop the running server gracefully and start the new one.
.\hamster-wheel.exe --config .\hamster.toml --control stop
.\hamster-wheel.exe --config .\hamster.toml

# 4. Confirm what is serving.
.\hamster-wheel.exe --config .\hamster.toml --control status
Invoke-WebRequest http://127.0.0.1:9090/ready    # if [server.admin] is configured
```

Three facts make that order the right one:

- the running image is locked against overwriting but not against renaming, so
  the old process keeps serving from the renamed file while the new binary is
  already in place;
- `--validate` compiles the file exactly as a start would and starts nothing,
  so a configuration the new build would reject is found while the old build is
  still serving (`doc/CONFIG_RELOAD_0_8.md`);
- the only downtime is between the stop completing and the new process binding.

Under a Windows Service, steps 1 and 2 are the same and the stop and start are
the Service's (`doc/WINDOWS_OPERATIONS.md`). On Linux there is no control
endpoint: stop with SIGINT or SIGTERM, both of which drain.

Argument order matters: `--config PATH` comes first, so
`--validate --config hamster.toml` is a usage error rather than a validation.

## Rolling back

```powershell
.\hamster-wheel.exe --config .\hamster.toml --control stop
Move-Item -Force .\hamster-wheel.exe.old .\hamster-wheel.exe
.\hamster-wheel.exe --config .\hamster.toml --validate
.\hamster-wheel.exe --config .\hamster.toml
```

Keep the previous binary until the new one has served real traffic; that is the
whole rollback plan, and it is why step 1 of the upgrade renames rather than
deletes.

**Roll the configuration back with the same care as the binary.** A
configuration rollback that only touches routing, static roots or the
per-connection timeouts can be published into a running server with
`--control reload` (SIGUSR1 off Windows), and it becomes the **next**
generation: generation numbers only increase, so "rolled back to the previous
configuration" appears in `status`, `/metrics` and the access log as a new
generation, not a lower one. A rollback that touches a listener, TLS, the
control endpoint, the admin listener, the access log, a PHP pool, a proxy
upstream or a FastCGI endpoint is refused by name and needs a restart; the
refusal list is in `doc/CONFIG_RELOAD_0_8.md`.

Rolling back **across** a binary change is a restart either way, so validate
the old configuration with the old binary before starting it.

## Compatibility

There is no release yet and **no backward compatibility is promised or
implemented**. The rule that follows is short:

> The server, the control client and the PHP worker script must come from one
> build. Deploy them together and upgrade them together.

That is not a style preference. Each of these was observed between this tree
and the oldest build that has a control endpoint:

| Surface | What a mismatch does |
|---|---|
| Control protocol | A newer client reading an older server's `status` fails with "control endpoint closed before a complete answer", because answers were not framed with a terminating blank line in the older build. `stop` happens to work, because its answer always carried the blank line |
| Control protocol | An older client against this server times out after about 5 s on **every** command — and the command still ran. A `stop` issued that way stops the server while the client reports failure, so a script that retries on failure would act on a false negative |
| Control commands | A command the server does not know is answered `ERR unknown_command` with the list it does know, so a newer client asking an older server to `reload` is refused cleanly rather than misunderstood |
| Configuration schema | Unknown keys are rejected (`deny_unknown_fields`), so a configuration written for a newer build fails to compile on an older one, and a key an older build required and a newer one removed fails the other way. Both fail at startup or at `--validate`, never silently |
| PHP worker protocol | Strict v2 with no fallback: a worker script from a mismatched build fails startup before the listener binds (`doc/APPLICATION_REQUEST_METADATA.md`) |
| Access log and metrics | Record kinds, event names and metric names are the compatibility surface and carry no stability promise before 1.0. A consumer should key on `kind` and `event` and tolerate new fields |

The upgrade sequence above is built around that rule: the new binary is used
for `--validate`, the **old** server is stopped with a client from its own
build where one is still available, and only the new client is used afterwards.

## Not covered

- No installer, no MSI, no package for any distribution, no container image,
  and no code signing. **NOT VERIFIED**: anything about signed binaries or
  SmartScreen.
- No automated upgrade: the sequences here are manual and were run by hand.
- No side-by-side running of two versions against one listener, and no
  connection handoff between processes. An upgrade has a gap.
- Linux packaging beyond "a release binary and a configuration directory":
  systemd units, a service user and distribution packages are **NOT VERIFIED**
  (`doc/LINUX_VERIFICATION.md`).
- aarch64 and musl, on either platform.
