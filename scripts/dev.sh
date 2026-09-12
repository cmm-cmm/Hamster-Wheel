#!/usr/bin/env bash
# Runs the server against a generated development configuration.
#
#   ./scripts/dev.sh              debug build on 127.0.0.1:8080
#   ./scripts/dev.sh --release    release build
#   HAMSTER_LISTEN=0.0.0.0:8080 ./scripts/dev.sh
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_common.sh"
root="$(repo_root)"
cd "${root}"

listen="${HAMSTER_LISTEN:-127.0.0.1:8080}"
php="$(hamster_php "${root}")"
if [ -z "${php}" ]; then
    printf 'no PHP CLI found; starting without the /php location\n' >&2
fi

mkdir -p target
config="${root}/target/hamster.dev.toml"
hamster_dev_config "${root}" "${listen}" "${php}" > "${config}"

printf 'configuration: %s\n' "${config}"
exec cargo run "$@" -- --config "${config}"
