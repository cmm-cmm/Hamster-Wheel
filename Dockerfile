# Hamster Wheel ships as one binary plus the assets it serves. The build stage
# never reaches the runtime image, so the deployed container carries no Rust
# toolchain, no sources, and no build cache.

# --- build -------------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /src

# Dependency layer: compiling a stub keeps `cargo build` cached whenever only
# first-party sources change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs \
    && for bench in static_pipeline blocking_pool windows_filesystem routing configuration; do \
        printf 'fn main() {}\n' > "benches/$bench.rs"; \
    done \
    && cargo build --release \
    && rm -rf src benches

# Only what `cargo build --release` reads: sources, plus the bench files the
# manifest declares (they are not compiled without the `profiling` feature).
COPY src ./src
COPY benches ./benches
# COPY preserves source mtimes, which can predate the stub build; touching the
# crate roots forces Cargo to rebuild them.
RUN touch src/main.rs src/lib.rs && cargo build --release

# --- development ------------------------------------------------------------
# `docker compose run --rm dev <cmd>` gets the full toolchain: rustfmt, clippy,
# and the PHP CLI the worker integration tests need.
FROM rust:1-bookworm AS dev
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates curl php-cli \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
ENV CARGO_TERM_COLOR=always \
    HAMSTER_PHP=/usr/bin/php
WORKDIR /workspace
CMD ["bash"]

# --- runtime ----------------------------------------------------------------
FROM php:8.4-cli AS runtime
RUN apt-get update \
    && apt-get install --no-install-recommends --yes curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /srv/hamster --shell /usr/sbin/nologin hamster

WORKDIR /srv/hamster
COPY --from=builder /src/target/release/hamster-wheel /usr/local/bin/hamster-wheel
COPY public ./public
COPY fixtures ./fixtures
COPY deploy/hamster.container.toml ./hamster.toml
RUN chown -R hamster:hamster /srv/hamster

USER hamster
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8080/ > /dev/null || exit 1
ENTRYPOINT ["hamster-wheel"]
CMD ["--config", "/srv/hamster/hamster.toml"]
