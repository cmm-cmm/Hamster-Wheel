# Deployment

Hamster Wheel deploys as one binary plus the files it serves. Configuration is
parsed, validated, and compiled once at startup; nothing is re-read per
request. A configuration error therefore fails the process at boot instead of
degrading service later, which makes deployment a matter of shipping three
things: the binary, the document root, and one TOML file.

## Deployment targets

| Target | Artifact | Configuration | Suited to |
| --- | --- | --- | --- |
| Container | `Dockerfile` runtime stage | `deploy/hamster.container.toml` baked at `/srv/hamster/hamster.toml` | orchestrated environments, reproducible builds |
| Bare metal (Linux) | `target/release/hamster-wheel` | `/etc/hamster-wheel/hamster.toml` | single hosts, systemd |
| Windows | `target\release\hamster-wheel.exe` | `hamster.toml` beside the binary | the project's primary development platform |

Release builds are the only builds valid for performance work. Never publish
or benchmark a debug build.

## Building

```bash
cargo build --release
./scripts/smoke.sh          # verifies the release binary actually serves traffic
```

The smoke test builds, starts the server against a generated configuration,
asserts `GET /` → 200, `GET /api` → 404, `GET /php` → 200 when a PHP CLI is
present, and stops the process. Run it against an already-running instance to
verify a deployed host:

```bash
HAMSTER_SMOKE_PHP=yes ./scripts/smoke.sh https://internal-host:8080
```

## Container

```bash
docker build --tag hamster-wheel:0.1.0 .
docker run --detach --name hamster --publish 8080:8080 hamster-wheel:0.1.0
```

The image:

- builds the release binary in a `rust:1-bookworm` stage that never reaches the
  runtime layer,
- runs on `php:8.4-cli`, so the PHP worker has a real PHP CLI at
  `/usr/local/bin/php`,
- installs the server under `/srv/hamster` and runs as the unprivileged
  `hamster` user,
- declares a `HEALTHCHECK` that requests `/` every 30 seconds.

`docker compose up server` publishes the same image on `${HAMSTER_HOST_PORT}`
(see `.env.example`). To deploy a different configuration, mount over the baked
file rather than rebuilding:

```bash
docker run --detach --publish 8080:8080 \
  --volume /etc/hamster-wheel/hamster.toml:/srv/hamster/hamster.toml:ro \
  --volume /srv/www:/srv/hamster/public:ro \
  hamster-wheel:0.1.0
```

Every path inside a mounted configuration must be valid *inside* the
container, and the `hamster` user must be able to read them.

## Bare metal with systemd

```bash
install -m 0755 target/release/hamster-wheel /usr/local/bin/hamster-wheel
install -d -o hamster -g hamster /etc/hamster-wheel /srv/hamster/public
install -m 0644 deploy/hamster.container.toml /etc/hamster-wheel/hamster.toml
# then edit listen, roots, and the PHP executable path for this host
```

`/etc/systemd/system/hamster-wheel.service`:

```ini
[Unit]
Description=Hamster Wheel HTTP server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=hamster
Group=hamster
ExecStart=/usr/local/bin/hamster-wheel --config /etc/hamster-wheel/hamster.toml
Restart=on-failure
RestartSec=2
# The server needs no write access and no privileges beyond binding its port.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadOnlyPaths=/srv/hamster
# Only when listening below port 1024:
#AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now hamster-wheel
systemctl status hamster-wheel
```

The process handles `SIGINT` (Ctrl+C) by closing the listener, draining
in-flight connections, and shutting the PHP worker down through its protocol.
`systemctl stop` sends `SIGTERM`, which systemd's default `KillMode` applies to
the main process; expect an immediate stop rather than a drain until graceful
`SIGTERM` handling lands.

## Configuration per environment

`server.listen` must be a numeric socket address — hostnames are rejected.

| Environment | `listen` | Notes |
| --- | --- | --- |
| Local development | `127.0.0.1:8080` | default; never exposed |
| Container | `0.0.0.0:8080` | the host publishes the port |
| Behind a reverse proxy | `127.0.0.1:8080` | let the proxy own TLS and the public port |

Document roots and PHP paths are resolved relative to the configuration file's
directory, so a config file that travels with its assets keeps working after a
move. Absolute paths are used verbatim.

Startup fails, by design, when a document root cannot be canonicalized, the PHP
executable or script is missing, two hosts collide case-insensitively, or a
location path is malformed. Read the `hamster-wheel: ...` line on stderr: it
names the offending field.

See [`CONFIGURATION.md`](CONFIGURATION.md) for the full schema and
[`PHP_WORKER.md`](PHP_WORKER.md) for worker behaviour and failure modes.

## Health checking and observability

- **Liveness/readiness**: `GET /` on the static location returns 200 once the
  listener is accepting and the PHP worker has reported `READY`. The server
  only starts listening after every worker reaches `READY`, so a successful
  response means the whole pipeline is up.
- **Startup log**: `PHP worker <pid> ready after <ms> ms` and
  `Hamster Wheel listening on http://<address>`.
- **PHP worker failures** surface as `502 Bad Gateway` and a stderr line; the
  worker's own stderr is passed through, never mixed into responses.

## Upgrades and rollback

1. Build and test the new revision (`./scripts/check.sh`, `./scripts/smoke.sh`).
2. Ship the artifact: new image tag, or the new binary alongside the old one.
3. Restart. Startup validation rejects a bad configuration before the listener
   binds, so a failed upgrade leaves the port free rather than serving errors.
4. Roll back by restarting the previous image tag or binary — there is no
   persistent state and no migration to undo.

Requests in flight are dropped on restart. Run two instances behind a reverse
proxy if a deployment must be seamless.

## Security checklist

- Never commit certificates, private keys, `.env` files, or a `hamster.toml`
  containing secrets. `.gitignore` already excludes `php/`, `.env`, and
  `/target/`.
- Run as an unprivileged user; the container image already does.
- Keep document roots outside the deployment's writable paths. Path traversal
  is rejected by the static handler, and that is a correctness requirement, not
  optional hardening — treat any escape as a bug with a regression test.
- The PHP executable and script are trusted startup configuration. They are
  spawned directly, with no shell and no command-string expansion; keep both
  owned by root or a deploy user and not writable by the service account.
- Terminate TLS at a reverse proxy until the server offers TLS natively.
