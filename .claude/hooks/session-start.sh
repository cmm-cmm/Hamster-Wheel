#!/bin/bash
# Prepares a Claude Code on the web container so `./scripts/check.sh` works:
# rustfmt and clippy components, a PHP CLI for the worker integration tests,
# and a warm dependency cache.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
    exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"

# Kept in step with scripts/_common.sh and .github/workflows/ci.yml.
HAMSTER_PHP_VERSION="8.5"

as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif command -v sudo > /dev/null 2>&1; then
        sudo "$@"
    else
        return 1
    fi
}

# The project targets PHP 8.5 NTS 64-bit. The worker tests skip themselves
# without any PHP CLI and still run against an older one, so neither install
# below may fail the session.
if ! command -v "php${HAMSTER_PHP_VERSION}" > /dev/null 2>&1 && ! command -v php > /dev/null 2>&1; then
    if as_root apt-get update \
        && as_root apt-get install --no-install-recommends --yes "php${HAMSTER_PHP_VERSION}-cli"; then
        echo "installed php${HAMSTER_PHP_VERSION}-cli"
    elif as_root apt-get install --no-install-recommends --yes php-cli; then
        echo "warning: installed the distribution php-cli; this project targets PHP ${HAMSTER_PHP_VERSION}" >&2
    else
        echo "warning: could not install a PHP CLI; PHP worker tests will skip" >&2
    fi
fi

rustup component add rustfmt clippy
cargo fetch --locked

php_path="$(command -v "php${HAMSTER_PHP_VERSION}" 2>/dev/null || command -v php 2>/dev/null || true)"
if [ -n "${php_path}" ] && [ -n "${CLAUDE_ENV_FILE:-}" ]; then
    echo "export HAMSTER_PHP=${php_path}" >> "${CLAUDE_ENV_FILE}"
fi

echo "environment ready: $(rustc --version); $("${php_path:-php}" --version 2>/dev/null | head -1)"
