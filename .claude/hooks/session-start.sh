#!/bin/bash
# Prepares a Claude Code on the web container so `./scripts/check.sh` works:
# rustfmt and clippy components, a PHP CLI for the worker integration tests,
# and a warm dependency cache.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
    exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"

as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif command -v sudo > /dev/null 2>&1; then
        sudo "$@"
    else
        return 1
    fi
}

# The PHP worker tests skip themselves without a PHP CLI, so a failed install
# must not fail the session.
if ! command -v php > /dev/null 2>&1; then
    if as_root apt-get update && as_root apt-get install --no-install-recommends --yes php-cli; then
        echo "installed php-cli"
    else
        echo "warning: could not install php-cli; PHP worker tests will skip" >&2
    fi
fi

rustup component add rustfmt clippy
cargo fetch --locked

if php_path="$(command -v php 2>/dev/null)" && [ -n "${CLAUDE_ENV_FILE:-}" ]; then
    echo "export HAMSTER_PHP=${php_path}" >> "${CLAUDE_ENV_FILE}"
fi

echo "environment ready: $(rustc --version); $(php --version 2>/dev/null | head -1)"
