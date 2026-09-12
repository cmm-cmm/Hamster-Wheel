#!/usr/bin/env bash
# The checks CI enforces: formatting, lints, tests. Run before every push.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/_common.sh"
root="$(repo_root)"
cd "${root}"

php="$(hamster_php "${root}")"
if [ -n "${php}" ]; then
    export HAMSTER_PHP="${php}"
    printf 'PHP worker tests will run against %s\n' "$(hamster_php_report "${php}")"
else
    printf 'no PHP CLI found; PHP worker tests will report themselves skipped\n' >&2
fi

set -x
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
