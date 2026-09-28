# Local Test Applications (`www/`)

Several ignored tests need a real application, not a fixture. `www/` is
git-ignored for that purpose: whole third-party projects with their own vendor
trees stay on the machine, so what `www/` holds differs from machine to
machine. This document says which applications the tests need, which of them
the repository can rebuild, and which evidence each one can support.

Nothing here is part of the product. The server treats these exactly like any
other application.

## Three kinds of application

| Application | Source | Rebuildable from the repository | What it can support |
|---|---|---|---|
| Laravel stand-in (`pms-scratch`) | `fixtures/php-worker/pms/prepare_scratch_app.ps1` | Yes, in about 2.5 minutes with network access | Every Laravel test: Octane lifecycle, auth/session isolation, metadata, TLS scheme. Synthetic data only. |
| PHP application workspace (`php-apps`): Symfony Demo 3.1.0, Slim skeleton 4.5.0 and DokuWiki 2026-07-14c, all external and unmodified, plus the simulated native application | `fixtures/php-apps/prepare_php_apps.ps1` | Yes, in about 3 minutes with network access | `tests/php_applications.rs`: other frameworks through persistent workers, request/response semantics, failures, pools side by side (`doc/PHP_APPLICATION_COMPATIBILITY.md`). `tests/php_cgi.rs`: DokuWiki as a classic SAPI application through FastCGI (`doc/FASTCGI_V1.md`). |
| The real PMS, Octane-enabled scratch copy | A copy of the owner's PMS checkout plus Octane | No: needs the owner's source, database and Redis | PMS behavior, schema and performance. |
| Plain PHP application (`plain-php`) | Not in the repository | No | Manual runs only; no test uses it. |

### PHP application workspace

```powershell
./fixtures/php-apps/prepare_php_apps.ps1 -Target D:\scratch\php-apps
$env:HAMSTER_PHP_APPS = 'D:\scratch\php-apps'
$env:HAMSTER_LARAVEL_TEST_PATH = 'D:\scratch\pms-scratch'   # optional
cargo test --test php_applications -- --ignored
cargo test --test php_cgi -- --ignored        # FastCGI; needs php\8.5\php-cgi.exe
```

`tests/php_cgi.rs` starts `php-cgi.exe -b` processes itself, as an operator
would, on fixed loopback ports; eight of its nine tests use only
`fixtures/php-cgi`, and the DokuWiki test needs `HAMSTER_PHP_APPS`. DokuWiki is
downloaded as the published release tarball (`download.dokuwiki.org`) and
extracted with the Windows `tar.exe`; it needs no Composer step, and the page
and media file the test writes stay in the workspace's `dokuwiki/data`.

Composer runs on the repository PHP through `composer.phar` (found beside the
`composer` command, or `-Composer <path>`), with exact package versions.
Symfony Demo is prepared as for production (`APP_ENV=prod` cache and compiled
assets); its SQLite database comes with the package and stays in the
workspace, so the comment the tests post is written there and nowhere else.
Built from an empty directory on 2026-09-28 in 3 min 14 s. The first test run
right after building can exceed the pool's five-second READY budget while the
new files are read for the first time; see the compatibility document.

**The stand-in is not the PMS.** It reproduces only what the tests touch: the
`Accesstokenlink`/`taikhoan` session model with an expiring `AccessLife`, a
`tymon/jwt-auth` parser resolved during provider boot so it becomes
worker-owned as on PMS, the checked-in probe provider, a `login` route for the
guest redirect, and a `php.ini` loaded through `PHPRC`. It has no PMS schema,
business logic, data volume, MySQL or Redis. A test that passes against it
proves the Laravel/Octane contract, not PMS compatibility, and no number
measured against it may be reported as a PMS measurement.

## Laravel stand-in

Requirements: Composer and git on `PATH` (Composer may run on any PHP 8.2 or
newer), network access to Packagist and GitHub, and the repository PHP at
`php/8.5/php.exe`. From the repository root:

```powershell
./fixtures/php-worker/pms/prepare_scratch_app.ps1                 # builds www\pms-scratch
./fixtures/php-worker/pms/prepare_scratch_app.ps1 -Target D:\scratch\pms-scratch
./fixtures/php-worker/pms/prepare_scratch_app.ps1 -PreferSource   # if GitHub archives are blocked
```

The script refuses to overwrite an existing directory. It runs
`composer create-project` for Laravel 12, requires `laravel/octane` 2.x and
`tymon/jwt-auth` 2.x, writes the stand-in models, migration, seeder and
provider list, and runs `jwt:secret`, `migrate` and `db:seed` with the
repository PHP. The Linux counterpart, `prepare_scratch_app.sh`, writes the
same application and currently lives on the `dev-linux` branch.

Two Windows details are handled by the script and matter for anyone doing
this by hand:

- Composer is `composer.bat`, so `cmd.exe` parses its arguments and strips
  `^`. `laravel/laravel:^12.0` typed in PowerShell reaches Composer as an
  exact `12.0`, and `laravel/octane:^2` as exactly 2.0.0, which cannot be
  installed with Laravel 12. Use `~12.0` and `~2.0`, which mean the same
  ranges.
- The repository PHP loads no extensions by default. The generated `php.ini`
  sets an absolute `extension_dir` (a relative one resolves against whichever
  directory the loading process runs in) and loads openssl, mbstring,
  fileinfo, curl, intl, zip, sodium, sqlite3 and pdo_sqlite. Every artisan
  command must run as `php\8.5\php.exe -c <app>\php.ini artisan ...` with
  `HAMSTER_PMS_PROBE=1`, because the probe provider refuses to boot without it.

Verified on 2026-09-28 on Windows: built from an empty directory in 2 min 27 s
(Laravel framework 12.69.2, Octane 2.20.0, jwt-auth 2.3.0), after which the
complete `php_worker` ignored suite passed with it, including all eleven
Laravel tests.

## Running the tests

```powershell
$env:HAMSTER_LARAVEL_TEST_PATH = "$PWD\www\pms-scratch"
cargo test --test php_worker -- --ignored
```

`HAMSTER_LARAVEL_TEST_PHP_INI` overrides the ini path, which otherwise defaults
to `php.ini` inside the application.

Eleven ignored tests need this application: the Octane ownership contract, the
`/up` round trip, owned metadata against forged forwarding headers, the HTTPS
scheme from the TLS listener, the fresh-worker controller baseline, both PMS
isolation tests, and the four script-level Laravel checks.

Without `HAMSTER_LARAVEL_TEST_PATH` those tests fail rather than skip, and
because Cargo stops after the first failing test binary, the suites that would
run after `php_worker` never execute. On a machine without the application, run
the other suites explicitly:

```powershell
cargo test --test tls_reload_process -- --ignored
cargo test --test windows_containment -- --ignored
cargo test --test timeout_probe -- --ignored
cargo test --test pool_visibility_probe -- --ignored
```

## The real PMS

The same tests can be pointed at an Octane-enabled scratch copy of the owner's
PMS; `doc/LARAVEL_WORKER_0_4.md` records how that copy was prepared (Octane
added, package discovery, `artisan optimize`, an explicit Composer platform
ignore for PhpSpreadsheet's `<8.5` constraint). Such a copy talks to the PMS
MySQL and Redis servers, so its results depend on that infrastructure and its
runs touch the owner's data. Never modify the original checkout; copy it.

State observed on 2026-09-28 on this machine: `www/pms` is a plain clone of the
PMS repository without Octane or the probe provider, so it cannot run these
tests as it is. The partial scratch copies earlier sessions left under
`%TEMP%` (incomplete vendor trees, some holding a copy of the PMS `.env`) were
deleted on 2026-09-28; no Octane-enabled PMS copy exists on this machine. No
real-PMS test was run in this pass.

After a run, make sure no `hamster-wheel`, `php-cgi` or repository `php.exe`
process the tests started is still running and no test port is listening.
Workspaces and `www/` are kept.

## Plain PHP application

A persistent PHP application with no framework, used for manual runs where
Laravel would hide worker lifecycle behavior. Its source (`app.php`,
`hamster-worker.php`) and the manual-run configuration `hamster-www.toml` are
not in the repository; the description below is what a rebuilt one should
offer. No test depends on it.

`app_handle()` returns `[status, headers, body]`; the worker never calls
`header()` or `echo`, because the CLI SAPI discards both.

| Route | Purpose |
|---|---|
| `/` | HTML page naming the PID, request count and uptime |
| `/state` | JSON process identity: PID, requests served, uptime, memory |
| `/echo` | Method, URI, scheme, peer/local endpoints, headers, body length and SHA-256 |
| `/sleep?ms=` | Delay, for request-timeout and queueing work |
| `/status?code=` | Any status 200-599 |
| `/bytes?n=` | Response body of N bytes, up to 4 MiB |
| `/kv?key=&set=` | Worker-local state, which is per PID and not shared |
| `/error` | A handled 500: the worker must stay healthy |
| `/fatal` | `exit(9)`: Hamster must replace the generation and must not retry |

The adapter finds `fixtures/php-worker/protocol.php` relative to the repository,
or through `HAMSTER_PROTOCOL_PATH`. Hamster does not strip a location prefix,
so a static location at `/static` needs its files in `<root>/static/`.
