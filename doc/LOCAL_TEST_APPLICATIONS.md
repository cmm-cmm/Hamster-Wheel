# Local Test Applications (`www/`)

Several ignored tests need a real application, not a fixture. `www/` is
git-ignored for that purpose: whole third-party projects with their own vendor
trees stay on the machine. This document records how the applications used on
the current Windows machine were built, so the same coverage can be reproduced
elsewhere.

Nothing here is part of the product. The server treats these exactly like any
other application.

## Layout

```text
www/
  hamster-www.toml      manual-run configuration for both applications
  public/static/        static assets (index.html, style.css, asset-4mib.bin)
  plain-php/
    app.php             a persistent PHP application, no framework
    hamster-worker.php  protocol v3 adapter that boots it once
  laravel/              Laravel 12 + Octane + the checked-in probe provider
```

## Plain PHP application

`plain-php` exists because the Laravel adapter and the framework hide worker
lifecycle behaviour behind a large amount of code. This application boots once
and answers frames until SHUTDOWN, so process identity, request counting and
failure handling can be observed on their own.

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
or through `HAMSTER_PROTOCOL_PATH`.

## Laravel application

Built on 2026-09-27 with Laravel 12.69.2 and `laravel/octane` ^2.20, running
under the repository's PHP 8.5.10 build.

```powershell
mkdir www; cd www
composer create-project laravel/laravel:^12.0 laravel --no-interaction --no-scripts
cd laravel
composer require laravel/octane --no-interaction --no-scripts
copy .env.example .env
```

`php.ini` beside the application is what Hamster passes to the worker through
`PHPRC`, and every artisan command below must use the same file with `-c`. This
PHP build has OPcache compiled in but loads no extensions by default, so the
file sets `extension_dir` and loads openssl, mbstring, fileinfo, curl, intl,
zip, sodium, sqlite3 and pdo_sqlite.

```powershell
$php = "..\..\php\8.5\php.exe -c php.ini"
& $php artisan key:generate
copy ..\..\fixtures\php-worker\pms\HamsterPmsProbeServiceProvider.php app\Providers\
# register App\Providers\HamsterPmsProbeServiceProvider::class in bootstrap/providers.php
$env:HAMSTER_PMS_PROBE = "1"
& $php artisan package:discover
& $php artisan migrate --force
& $php artisan optimize
```

The probe provider throws unless `HAMSTER_PMS_PROBE=1`, so artisan needs it too.

### PMS stand-ins

The checked-in probe provider logs in an `App\Models\Accesstokenlink` that has
a `taikhoan` relation, because it was written for the production PMS. A fresh
Laravel has neither, so this application adds:

- `app/Models/Accesstokenlink.php`, an `Authenticatable` on table
  `accesstokenlink` with a nullable `AccessLife` and a `taikhoan()` relation;
- `app/Models/Taikhoan.php` on table `taikhoan`;
- one migration creating both tables with a single seeded row;
- `config/auth.php` pointing the `users` provider at `Accesstokenlink`, so the
  session round trip resolves the same model it logged in;
- a route named `login`, because the `auth` middleware redirects unauthenticated
  browser requests to it and the test asserts that redirect.

**These are stand-ins, not the PMS.** They give the auth-isolation test a real
Laravel guard, session and database round trip on synthetic data. They do not
reproduce PMS schema, business logic or data volume, and no performance number
taken against this application may be reported as a PMS measurement.

## Running the tests

```powershell
$env:HAMSTER_LARAVEL_TEST_PATH = "D:\Project\Hamster-Wheel\www\laravel"
cargo test --test php_worker -- --ignored
```

`HAMSTER_LARAVEL_TEST_PHP_INI` overrides the ini path, which otherwise defaults
to `php.ini` inside the application.

Eleven ignored tests need this application: the Octane ownership contract, the
`/up` round trip, owned metadata against forged forwarding headers, the HTTPS
scheme from the TLS listener, the fresh-worker controller baseline, both PMS
isolation tests, and the four script-level Laravel checks. All of them pass
against the application described here.

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

## Manual runs

```powershell
cargo run -- --config www/hamster-www.toml
```

`localhost:8080` serves the plain application with static files under
`/static/`; `laravel.localhost:8080` serves Laravel through the Octane adapter.
Hamster does not strip a location prefix, so a static location at `/static`
needs its files in `<root>/static/`.
