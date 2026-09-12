# Hamster Wheel ships as one binary plus the assets it serves. The build stage
# never reaches the runtime image, so the deployed container carries no Rust
# toolchain, no sources, and no build cache.
#
# The PHP worker targets PHP 8.5, non-thread-safe, 64-bit. The official php
# images are exactly that (the thread-safe build is the separate -zts variant),
# and each stage below asserts it rather than trusting the tag.

# --- build -------------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /src

# Dependency layer: compiling a stub keeps `cargo build` cached whenever only
# first-party sources change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf '\n' > src/lib.rs \
    && for bench in static_pipeline blocking_pool windows_filesystem routing configuration php_ipc; do \
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
# and the PHP 8.5 CLI the worker tests need.
FROM php:8.5-cli AS dev
RUN php -r 'exit((version_compare(PHP_VERSION, "8.5", ">=") && !PHP_ZTS && PHP_INT_SIZE === 8) ? 0 : 1);' \
    || { echo "dev image must provide PHP 8.5 NTS 64-bit"; exit 1; }
RUN apt-get update \
    && apt-get install --no-install-recommends --yes build-essential ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    CARGO_TERM_COLOR=always \
    HAMSTER_PHP=/usr/local/bin/php
RUN curl --proto '=https' --tlsv1.2 --silent --show-error --fail https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable -c clippy -c rustfmt \
    && chmod -R a+w "${CARGO_HOME}" "${RUSTUP_HOME}"
WORKDIR /workspace
CMD ["bash"]

# --- runtime ----------------------------------------------------------------
FROM php:8.5-cli AS runtime
RUN php -r 'exit((version_compare(PHP_VERSION, "8.5", ">=") && !PHP_ZTS && PHP_INT_SIZE === 8) ? 0 : 1);' \
    || { echo "runtime image must provide PHP 8.5 NTS 64-bit"; exit 1; }
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
