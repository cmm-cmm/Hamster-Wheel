<#
.SYNOPSIS
Builds a disposable, PMS-shaped Laravel 12 + Octane application for the ignored
Laravel tests in tests/php_worker.rs, then prints how to run them.

.DESCRIPTION
    fixtures\php-worker\pms\prepare_scratch_app.ps1 [-Target <dir>] [-PreferSource]

Target defaults to www\pms-scratch. www\ is git-ignored: the application is a
whole third-party project with its own vendor tree and is never committed.

The application reproduces only what the tests exercise on the real PMS:
  - laravel/octane 2.x, which laravel_worker.php drives through its Worker;
  - tymon/jwt-auth, whose parser is resolved during provider boot so it
    becomes a worker-owned singleton, as PMS does;
  - App\Models\Accesstokenlink as the session-authenticated model, expiring
    through AccessLife and belonging to a taikhoan account;
  - the checked-in HamsterPmsProbeServiceProvider, a login route for the
    guest redirect, and a php.ini loaded through PHPRC.
It is a stand-in. It does not reproduce PMS schema, business logic, data or
infrastructure, and nothing measured against it is a PMS measurement.

This is the Windows counterpart of prepare_scratch_app.sh and writes the same
application. Composer runs with whatever PHP 8.2+ is on PATH; artisan and the
tests run with the repository's php\8.5\php.exe, which loads no extensions by
default, so the generated php.ini loads the ones Laravel needs.

-PreferSource clones packages with git instead of downloading GitHub archives,
for hosts whose network policy blocks codeload.github.com.
#>
param(
    [string] $Target,
    [switch] $PreferSource
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
if (-not $Target) {
    $Target = Join-Path $repoRoot 'www\pms-scratch'
}
$Target = [IO.Path]::GetFullPath($Target)
$provider = Join-Path $repoRoot 'fixtures\php-worker\pms\HamsterPmsProbeServiceProvider.php'
$php = Join-Path $repoRoot 'php\8.5\php.exe'
$extensionDir = Join-Path $repoRoot 'php\8.5\ext'
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Write-Text([string] $Path, [string] $Text) {
    # No byte-order mark: PHP would send it before the first frame.
    [IO.File]::WriteAllText($Path, $Text.Replace("`r`n", "`n"), $utf8)
}

function Invoke-Checked([string] $File, [string[]] $Arguments) {
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$File $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

foreach ($tool in 'composer', 'git') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is required on PATH"
    }
}
if (-not (Test-Path $php)) {
    throw "the repository PHP is missing: $php"
}
if (Test-Path $Target) {
    throw "$Target already exists; remove it to build a fresh application"
}

$prefer = @()
if ($PreferSource) {
    $prefer = @('--prefer-source')
}
$env:COMPOSER_NO_INTERACTION = '1'
# Tilde, not caret: composer is a .bat on Windows and cmd.exe strips `^`,
# which turns ^12.0 into an exact 12.0. ~12.0 and ~2.0 mean the same ranges.
Invoke-Checked 'composer' (@('create-project', 'laravel/laravel:~12.0', $Target) + $prefer + @('--no-progress'))
Push-Location $Target
try {
    Invoke-Checked 'composer' (@('require', 'laravel/octane:~2.0', 'tymon/jwt-auth:~2.0') + $prefer + @('--no-progress'))

    Copy-Item $provider (Join-Path $Target 'app\Providers\')

    Write-Text (Join-Path $Target 'app\Models\Taikhoan.php') @'
<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;

/** A PMS-shaped account row; access links authenticate on its behalf. */
class Taikhoan extends Model
{
    protected $table = 'taikhoan';

    protected $guarded = [];
}
'@

    Write-Text (Join-Path $Target 'app\Models\Accesstokenlink.php') @'
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
'@

    Write-Text (Join-Path $Target 'database\migrations\2026_01_01_000000_create_pms_access_tables.php') @'
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
'@

    Write-Text (Join-Path $Target 'database\seeders\DatabaseSeeder.php') @'
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
'@

    Write-Text (Join-Path $Target 'app\Providers\AppServiceProvider.php') @'
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
'@

    Write-Text (Join-Path $Target 'bootstrap\providers.php') @'
<?php

use App\Providers\AppServiceProvider;
use App\Providers\HamsterPmsProbeServiceProvider;

return [
    AppServiceProvider::class,
    // Scratch-only; refuses to boot unless HAMSTER_PMS_PROBE=1.
    HamsterPmsProbeServiceProvider::class,
];
'@

    $routes = Join-Path $Target 'routes\web.php'
    Write-Text $routes ((Get-Content -Raw $routes) + @'

// The `auth` middleware redirects guests to the route named `login`.
Route::get('/login', fn () => 'login')->name('login');
'@)

    # extension_dir must be absolute: a relative one resolves against the
    # working directory of whichever process loads this file.
    $extensions = 'openssl', 'mbstring', 'fileinfo', 'curl', 'intl', 'zip', 'sodium', 'sqlite3', 'pdo_sqlite'
    Write-Text (Join-Path $Target 'php.ini') (@"
; Loaded through PHPRC by the Hamster Laravel tests. stdout carries the worker
; protocol, so errors must never be displayed there.
extension_dir = "$extensionDir"
memory_limit = 512M
display_errors = stderr
display_startup_errors = stderr
log_errors = On
error_reporting = E_ALL

"@ + (($extensions | ForEach-Object { "extension=$_" }) -join "`n") + "`n")

    $environment = Join-Path $Target '.env'
    Write-Text $environment ((Get-Content -Raw $environment) + "`nAUTH_MODEL=App\Models\Accesstokenlink`n")

    # The probe provider refuses to boot without this, which includes artisan.
    $env:HAMSTER_PMS_PROBE = '1'
    $ini = Join-Path $Target 'php.ini'
    Invoke-Checked $php @('-c', $ini, 'artisan', 'jwt:secret', '--force')
    Invoke-Checked $php @('-c', $ini, 'artisan', 'migrate', '--force')
    Invoke-Checked $php @('-c', $ini, 'artisan', 'db:seed', '--force')
}
finally {
    Pop-Location
}

Write-Output @"

Scratch application ready: $Target

Run the Laravel tests from the repository root with:
  `$env:HAMSTER_LARAVEL_TEST_PATH = '$Target'
  cargo test --test php_worker -- --ignored laravel pms
"@
