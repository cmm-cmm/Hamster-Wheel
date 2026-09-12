#!/usr/bin/env bash
# End-to-end check that a build actually serves traffic.
#
#   ./scripts/smoke.sh                       build, start, probe, stop
#   ./scripts/smoke.sh http://127.0.0.1:8080 probe an already running server
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_common.sh"
root="$(repo_root)"
cd "${root}"

probe() {
    local base="$1" expect_php="$2"
    printf 'probing %s\n' "${base}"
    curl --fail --silent --show-error "${base}/" > /dev/null
    printf '  GET /        200\n'
    if [ "$(curl --silent --output /dev/null --write-out '%{http_code}' "${base}/api")" != "404" ]; then
        printf 'expected 404 from /api\n' >&2
        return 1
    fi
    printf '  GET /api     404\n'
    if [ "${expect_php}" = "yes" ]; then
        curl --fail --silent --show-error "${base}/php" > /dev/null
        printf '  GET /php     200\n'
    fi
}

if [ "$#" -ge 1 ]; then
    wait_for_http "${1%/}/"
    probe "${1%/}" "${HAMSTER_SMOKE_PHP:-no}"
    printf 'smoke test passed\n'
    exit 0
fi

php="$(hamster_php "${root}")"
listen="${HAMSTER_LISTEN:-127.0.0.1:8080}"
mkdir -p target
config="${root}/target/hamster.smoke.toml"
hamster_dev_config "${root}" "${listen}" "${php}" > "${config}"

cargo build --release
"${root}/target/release/hamster-wheel" --config "${config}" &
server_pid=$!
trap 'kill "${server_pid}" 2>/dev/null || true; wait "${server_pid}" 2>/dev/null || true' EXIT

base="http://${listen/0.0.0.0/127.0.0.1}"
wait_for_http "${base}/"
if [ -n "${php}" ]; then
    probe "${base}" yes
else
    probe "${base}" no
fi
printf 'smoke test passed\n'
