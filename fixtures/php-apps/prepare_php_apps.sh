#!/usr/bin/env bash
# Builds the PHP application workspace used by tests/php_applications.rs on
# Linux and macOS; prepare_php_apps.ps1 is the Windows counterpart.
#
#   fixtures/php-apps/prepare_php_apps.sh [--prefer-source] [--no-dev] [--without-dokuwiki] [TARGET]
#
# TARGET defaults to www/php-apps (git-ignored). The workspace holds:
#
#   php.ini        error handling for the repository PHP; the extensions are
#                  the ones the host PHP already loads
#   symfony-demo   symfony/symfony-demo 3.1.0, external and unmodified, prod
#                  container cache warmed and AssetMapper assets compiled
#   slim-skeleton  slim/slim-skeleton 4.5.0, external and unmodified
#   native         fixtures/php-apps/native, written for Hamster
#   dokuwiki       DokuWiki 2026-07-14c "Mort", the published release tarball,
#                  for tests/php_cgi.rs; --without-dokuwiki leaves it out
#
# Composer runs on the repository PHP (php/8.5/php) with the generated php.ini,
# so dependencies are resolved for the PHP that serves them. Versions are exact.
# --prefer-source clones packages with git instead of downloading GitHub
# archives, for hosts whose network policy blocks codeload.github.com.
# --no-dev skips the Slim skeleton's development packages, which the tests do
# not load; its phpstan/phpstan is published only as a GitHub archive. Symfony
# Demo always installs them: its auto-scripts boot the dev kernel.
#
# The Laravel stand-in is built separately by
# fixtures/php-worker/pms/prepare_scratch_app.sh.
set -euo pipefail

usage() {
    sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
}

composer_options=()
slim_options=()
dokuwiki=1
target=""
for argument in "$@"; do
    case "$argument" in
        --prefer-source) composer_options+=("$argument") ;;
        --no-dev) slim_options+=("$argument") ;;
        --without-dokuwiki) dokuwiki=0 ;;
        -h | --help) usage; exit 0 ;;
        -*) echo "unknown option: $argument" >&2; exit 2 ;;
        *) target="$argument" ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target="${target:-$repo_root/www/php-apps}"
php="$repo_root/php/8.5/php"

[ -x "$php" ] || { echo "the repository PHP is missing: $php" >&2; exit 1; }
composer="$(command -v composer)" || { echo "composer is required" >&2; exit 1; }
for tool in git curl tar; do
    command -v "$tool" > /dev/null || { echo "$tool is required" >&2; exit 1; }
done
"$php" -r 'exit(version_compare(PHP_VERSION, "8.4", ">=") ? 0 : 1);' \
    || { echo "symfony/symfony-demo 3.1.0 needs PHP 8.4 or newer" >&2; exit 1; }
for extension in openssl mbstring fileinfo curl intl zip sodium sqlite3 pdo_sqlite; do
    "$php" -m | grep -qix "$extension" \
        || { echo "PHP extension $extension is required" >&2; exit 1; }
done
# Composer disables plugins for root, and Symfony Demo's auto-scripts need
# symfony/flex. Allowing plugins as root is the operator's decision.
if [ "$(id -u)" -eq 0 ] && [ "${COMPOSER_ALLOW_SUPERUSER:-}" != 1 ]; then
    echo "running as root: Symfony Demo needs Composer plugins; set COMPOSER_ALLOW_SUPERUSER=1 to allow them" >&2
    exit 1
fi
if [ -e "$target" ]; then
    echo "$target already exists; remove it to build a fresh workspace" >&2
    exit 1
fi

mkdir -p "$target"
target="$(cd "$target" && pwd)"
ini="$target/php.ini"
# stdout carries the worker protocol, so errors must never be displayed there.
cat > "$ini" << 'INI'
; PHPRC for the PHP applications behind Hamster. stdout carries the worker
; protocol, so errors must never be displayed there.
memory_limit = 512M
display_errors = stderr
display_startup_errors = stderr
log_errors = Off
error_reporting = E_ALL
INI

run_php() {
    "$php" -c "$ini" "$@"
}

export COMPOSER_NO_INTERACTION=1
cd "$target"
run_php "$composer" create-project symfony/symfony-demo:3.1.0 symfony-demo \
    ${composer_options[@]+"${composer_options[@]}"} --no-progress
run_php "$composer" create-project slim/slim-skeleton:4.5.0 slim-skeleton \
    ${composer_options[@]+"${composer_options[@]}"} ${slim_options[@]+"${slim_options[@]}"} --no-progress

if [ "$dokuwiki" -eq 1 ]; then
    release=dokuwiki-2026-07-14c
    curl -sSfL -o "$release.tgz" "https://download.dokuwiki.org/src/dokuwiki/$release.tgz"
    tar -xzf "$release.tgz"
    mv "$release" dokuwiki
    rm "$release.tgz"
fi

cp -R "$repo_root/fixtures/php-apps/native" native
(cd native && run_php "$composer" dump-autoload)

# Deployment steps from the Symfony Demo README. The worker boots the kernel
# before READY, so the container cache must exist beforehand.
(
    cd symfony-demo
    export APP_ENV=prod APP_DEBUG=0
    run_php bin/console cache:clear
    run_php bin/console asset-map:compile
)

cat << EOF

PHP application workspace ready: $target

Run the application tests from the repository root with:
  HAMSTER_PHP_APPS='$target' cargo test --test php_applications -- --ignored
Set HAMSTER_LARAVEL_TEST_PATH as well to add the Laravel stand-in.
EOF
