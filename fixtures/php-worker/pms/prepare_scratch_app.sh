#!/usr/bin/env bash
# Builds a disposable, PMS-shaped Laravel 12 + Octane application for the
# ignored Laravel tests in tests/php_worker.rs, then prints how to run them.
#
#   fixtures/php-worker/pms/prepare_scratch_app.sh [--prefer-source] [TARGET]
#
# TARGET defaults to www/pms-scratch. www/ is git-ignored: the application is a
# whole third-party project with its own vendor tree and is never committed.
#
# The application reproduces only what the tests exercise on the real PMS:
#   - laravel/octane 2.x, which laravel_worker.php drives through its Worker;
#   - tymon/jwt-auth, whose parser is resolved during provider boot so it
#     becomes a worker-owned singleton, as PMS does;
#   - App\Models\Accesstokenlink as the session-authenticated model, expiring
#     through `AccessLife` and belonging to a `taikhoan` account;
#   - the checked-in HamsterPmsProbeServiceProvider, a `login` route for the
#     guest redirect, and a php.ini loaded through PHPRC.
#
# --prefer-source clones packages with git instead of downloading GitHub
# archives, for hosts whose network policy blocks codeload.github.com.
set -euo pipefail

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

prefer=()
target=""
for argument in "$@"; do
    case "$argument" in
        --prefer-source) prefer=(--prefer-source) ;;
        -h | --help) usage; exit 0 ;;
        -*) echo "unknown option: $argument" >&2; exit 2 ;;
        *) target="$argument" ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
target="${target:-$repo_root/www/pms-scratch}"
provider="$repo_root/fixtures/php-worker/pms/HamsterPmsProbeServiceProvider.php"

for tool in php composer git; do
    command -v "$tool" > /dev/null || { echo "$tool is required" >&2; exit 1; }
done
php -r 'exit(version_compare(PHP_VERSION, "8.2", ">=") ? 0 : 1);' \
    || { echo "Laravel 12 needs PHP 8.2 or newer" >&2; exit 1; }
for extension in pdo_sqlite mbstring xml ctype tokenizer; do
    php -m | grep -qix "$extension" \
        || { echo "PHP extension $extension is required" >&2; exit 1; }
done
if [ -e "$target" ]; then
    echo "$target already exists; remove it to build a fresh application" >&2
    exit 1
fi

export COMPOSER_NO_INTERACTION=1
composer create-project "laravel/laravel:^12.0" "$target" ${prefer[@]+"${prefer[@]}"} --no-progress
cd "$target"
composer require "laravel/octane:^2" "tymon/jwt-auth:^2" ${prefer[@]+"${prefer[@]}"} --no-progress

mkdir -p app/Models app/Providers
cp "$provider" app/Providers/

cat > app/Models/Taikhoan.php <<'PHP'
<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;

/** A PMS-shaped account row; access links authenticate on its behalf. */
class Taikhoan extends Model
{
    protected $table = 'taikhoan';

    protected $guarded = [];
}
PHP

cat > app/Models/Accesstokenlink.php <<'PHP'
<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Relations\BelongsTo;
use Illuminate\Foundation\Auth\User as Authenticatable;
use Tymon\JWTAuth\Contracts\JWTSubject;

/**
 * The authenticatable PMS row the Hamster probe expects: an access link that
 * expires through `AccessLife` and must belong to an account (`taikhoan`).
 */
class Accesstokenlink extends Authenticatable implements JWTSubject
{
    protected $table = 'accesstokenlink';

    protected $guarded = [];

    protected $hidden = ['password', 'remember_token'];

    protected $casts = ['AccessLife' => 'datetime'];

    public function taikhoan(): BelongsTo
    {
        return $this->belongsTo(Taikhoan::class, 'taikhoan_id');
    }

    public function getJWTIdentifier(): mixed
    {
        return $this->getKey();
    }

    public function getJWTCustomClaims(): array
    {
        return [];
    }
}
PHP

cat > database/migrations/2026_01_01_000000_create_pms_access_tables.php <<'PHP'
<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    public function up(): void
    {
        Schema::create('taikhoan', function (Blueprint $table): void {
            $table->id();
            $table->string('TenDangNhap')->unique();
            $table->timestamps();
        });

        Schema::create('accesstokenlink', function (Blueprint $table): void {
            $table->id();
            $table->foreignId('taikhoan_id')->constrained('taikhoan');
            $table->string('password')->nullable();
            $table->rememberToken();
            $table->dateTime('AccessLife')->nullable();
            $table->timestamps();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('accesstokenlink');
        Schema::dropIfExists('taikhoan');
    }
};
PHP

cat > database/seeders/DatabaseSeeder.php <<'PHP'
<?php

namespace Database\Seeders;

use App\Models\Accesstokenlink;
use App\Models\Taikhoan;
use Illuminate\Database\Seeder;

class DatabaseSeeder extends Seeder
{
    public function run(): void
    {
        // An expired link first, so the probe's AccessLife filter must skip it,
        // then the valid link the probe has to select.
        $expired = Taikhoan::create(['TenDangNhap' => 'expired']);
        Accesstokenlink::create(['taikhoan_id' => $expired->id, 'AccessLife' => now()->subDay()]);

        $account = Taikhoan::create(['TenDangNhap' => 'hamster']);
        Accesstokenlink::create(['taikhoan_id' => $account->id, 'AccessLife' => now()->addYear()]);
    }
}
PHP

cat > app/Providers/AppServiceProvider.php <<'PHP'
<?php

namespace App\Providers;

use Illuminate\Support\ServiceProvider;

class AppServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        //
    }

    public function boot(): void
    {
        // PMS resolves the JWT parser during provider boot, which makes it a
        // worker-owned singleton under Octane. Reproduce that on purpose so the
        // Hamster lifecycle checks exercise the real warmed-parser pattern.
        $this->app->make('tymon.jwt.parser');
    }
}
PHP

cat > bootstrap/providers.php <<'PHP'
<?php

use App\Providers\AppServiceProvider;
use App\Providers\HamsterPmsProbeServiceProvider;

return [
    AppServiceProvider::class,
    // Scratch-only; refuses to boot unless HAMSTER_PMS_PROBE=1.
    HamsterPmsProbeServiceProvider::class,
];
PHP

cat >> routes/web.php <<'PHP'

// The `auth` middleware redirects guests to the route named `login`.
Route::get('/login', fn () => 'login')->name('login');
PHP

cat > php.ini <<'INI'
; Loaded through PHPRC by the Hamster Laravel tests. stdout carries the worker
; protocol, so errors must never be displayed there.
memory_limit = 512M
display_errors = stderr
display_startup_errors = stderr
log_errors = On
error_reporting = E_ALL
INI

printf '\nAUTH_MODEL=App\\Models\\Accesstokenlink\n' >> .env

# The probe provider refuses to boot without this, which includes artisan.
export HAMSTER_PMS_PROBE=1
php artisan jwt:secret --force
php artisan migrate --force
php artisan db:seed --force

cat <<EOF

Scratch application ready: $target

Run the Laravel tests from the repository root with:
  HAMSTER_LARAVEL_TEST_PATH="$target" cargo test --test php_worker -- --ignored laravel pms
EOF
