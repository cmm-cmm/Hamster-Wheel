#!/usr/bin/env python3
"""Hamster against FrankenPHP on the same Linux host, same cells, interleaved.

Each cell runs both servers three times in the order H F F H H F, a fresh
server per run, with the measurement method of linux_http.py (oha, CPU from
/proc for the whole process tree, RSS at the end of the run). The FrankenPHP
configuration and worker script are written to a temporary directory; the
worker answers /hello, /json and /echo exactly as fixtures/php-worker/worker.php
does.

  benchmarks/frankenphp_linux.py --frankenphp PATH [--oha PATH]
      [--laravel APP] [GROUP ...]

GROUP is static, worker, classic, laravel or all (the default; laravel only
with --laravel). --laravel names an Octane-enabled Laravel application such as
the one prepare_scratch_app.sh builds; the run copies Octane's FrankenPHP
worker stub into its public/ directory and removes it afterwards. Build
release first. Ports 8080 and 9101 must be free.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import linux_http as lh  # noqa: E402

REPO = lh.REPO
HAMSTER = f"{REPO}/target/release/hamster-wheel"
SITE = "http://127.0.0.1:8080, http://localhost:8080"

WORKER = r"""<?php

declare(strict_types=1);

// FrankenPHP counterpart of the hello, json and echo routes of
// fixtures/php-worker/worker.php: same bodies, statuses and headers.
$requestCount = 0;
$handler = static function () use (&$requestCount): void {
    $requestCount++;
    $path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
    parse_str($_SERVER['QUERY_STRING'] ?? '', $query);
    switch ($path) {
        case '/json':
            header('Content-Type: application/json');
            echo json_encode(['message' => 'Hello from PHP'], JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);
            break;
        case '/echo':
            http_response_code(201);
            header('Content-Type: application/octet-stream');
            header('X-Php-Worker: persistent');
            header('X-Repeat: one');
            header('X-Repeat: two', false);
            echo file_get_contents('php://input');
            break;
        default:
            header('Content-Type: text/plain; charset=utf-8');
            echo 'Hello from PHP';
    }
};
while (frankenphp_handle_request($handler)) {
}
"""

LARAVEL_ENV = {
    "APP_ENV": "production",
    "APP_DEBUG": "false",
    "SESSION_DRIVER": "array",
    "CACHE_STORE": "array",
    "LOG_CHANNEL": "stderr",
    "LOG_LEVEL": "error",
    "HAMSTER_PMS_PROBE": "1",
}

PHP_INI = """memory_limit = 512M
display_errors = stderr
display_startup_errors = stderr
log_errors = Off
error_reporting = E_ALL
; Laravel cells only. OPcache on for both servers: the CLI worker needs enable_cli,
; FrankenPHP's embed SAPI uses opcache.enable.
opcache.enable = 1
opcache.enable_cli = 1
"""


def write(path, text):
    with open(path, "w") as handle:
        handle.write(text)
    return path


def caddyfile(global_block, site_block):
    return f"{{\n\tadmin off\n\tauto_https off\n{global_block}}}\n{SITE} {{\n{site_block}}}\n"


def frankenphp_configs(directory, laravel):
    worker_root = os.path.join(directory, "worker")
    os.makedirs(worker_root)
    write(os.path.join(worker_root, "index.php"), WORKER)
    paths = {
        "static": caddyfile("", f"\troot * {REPO}/public\n\tfile_server\n"),
        "classic": caddyfile("\tfrankenphp {\n\t\tnum_threads 4\n\t}\n",
                             f"\troot * {REPO}/fixtures/php-cgi\n\tphp_server {{\n\t\tindex bench.php\n\t}}\n"),
    }
    for workers in (1, 4):
        paths[f"worker_w{workers}"] = caddyfile(
            f"\tfrankenphp {{\n\t\tnum_threads {workers + 1}\n\t\tworker {{\n"
            f"\t\t\tfile {worker_root}/index.php\n\t\t\tnum {workers}\n\t\t}}\n\t}}\n",
            f"\troot * {worker_root}\n\tphp_server\n")
    if laravel:
        env = "".join(f"\t\t\tenv {k} {v}\n" for k, v in LARAVEL_ENV.items())
        paths["laravel"] = caddyfile(
            "\tfrankenphp {\n\t\tnum_threads 5\n\t\tworker {\n"
            f"\t\t\tfile \"{laravel}/public/frankenphp-worker.php\"\n\t\t\tnum 4\n"
            f"\t\t\tenv APP_BASE_PATH \"{laravel}\"\n\t\t\tenv APP_PUBLIC_PATH \"{laravel}/public\"\n"
            f"\t\t\tenv LARAVEL_OCTANE 1\n{env}\t\t}}\n\t}}\n",
            f"\troute {{\n\t\troot * \"{laravel}/public\"\n\t\tphp_server {{\n"
            "\t\t\tindex frankenphp-worker.php\n\t\t\ttry_files {path} frankenphp-worker.php\n"
            "\t\t\tresolve_root_symlink\n\t\t}\n\t}\n")
    return {name: write(os.path.join(directory, f"{name}.Caddyfile"), text)
            for name, text in paths.items()}


def hamster_laravel_config(directory, laravel, php_ini):
    env = "".join(f'{k} = "{v}"\n' for k, v in LARAVEL_ENV.items())
    return write(os.path.join(directory, "laravel.toml"),
                 '[server]\nlisten = "127.0.0.1:8080"\n[[hosts]]\nname = "localhost"\n'
                 'aliases = ["127.0.0.1"]\n[[hosts.locations]]\npath = "/"\nhandler = "php_worker"\n'
                 f'[hosts.locations.php]\nexecutable = "{REPO}/php/8.5/php"\n'
                 f'script = "{REPO}/fixtures/php-worker/laravel_worker.php"\nworkers = 4\n'
                 f'queue_capacity = 64\n[hosts.locations.php.env]\nHAMSTER_LARAVEL_PATH = "{laravel}"\n'
                 f'PHPRC = "{php_ini}"\n{env}')


def measure(oha, command, environment, path, requests, concurrency, expected, method, body,
            fastcgi, warmup):
    cgi = None
    if fastcgi:
        cgi_environment = dict(os.environ, PHP_FCGI_CHILDREN="4", PHP_FCGI_MAX_REQUESTS="0")
        cgi = subprocess.Popen([f"{REPO}/php/8.5/php-cgi", "-d", "output_buffering=0", "-b",
                                f"127.0.0.1:{lh.FASTCGI_PORT}"], env=cgi_environment,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        lh.wait_for_port(lh.FASTCGI_PORT)
    server = subprocess.Popen(command, env=environment, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    try:
        lh.wait_for_port(8080)
        time.sleep(0.5)
        lh.load(oha, path, warmup, min(concurrency, 8), method, body)
        time.sleep(0.3)
        before = lh.cpu_ticks(server.pid)
        cgi_before = lh.cpu_ticks(cgi.pid) if cgi else {}
        result = lh.load(oha, path, requests, concurrency, method, body)
        after = lh.cpu_ticks(server.pid)
        cgi_after = lh.cpu_ticks(cgi.pid) if cgi else {}
        cpu = sum(v - before.get(p, 0) for p, v in after.items()) / lh.TICK
        cpu += sum(v - cgi_before.get(p, 0) for p, v in cgi_after.items()) / lh.TICK
        rss = sum(lh.rss_kib(p) for p in after) + sum(lh.rss_kib(p) for p in cgi_after)
        percentiles = result["latencyPercentiles"]
        return dict(rps=result["summary"]["requestsPerSec"], p50=percentiles["p50"] * 1e3,
                    p95=percentiles["p95"] * 1e3, p99=percentiles["p99"] * 1e3,
                    cpu_us=cpu * 1e6 / requests, rss_mib=rss / 1024,
                    unexpected=requests - result["statusCodeDistribution"].get(str(expected), 0),
                    errors=sum(result.get("errorDistribution", {}).values()))
    finally:
        server.terminate()
        server.wait(15)
        if cgi:
            cgi.terminate()
            cgi.wait(10)
        time.sleep(0.5)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("groups", nargs="*", default=["all"])
    parser.add_argument("--frankenphp", required=True)
    parser.add_argument("--oha", default="oha")
    parser.add_argument("--laravel")
    args = parser.parse_args()
    frankenphp = os.path.abspath(args.frankenphp)
    laravel = os.path.abspath(args.laravel) if args.laravel else None
    groups = args.groups
    if "all" in groups:
        groups = ["static", "worker", "classic"] + (["laravel"] if laravel else [])
    if "laravel" in groups and not laravel:
        parser.error("the laravel group needs --laravel")

    with tempfile.TemporaryDirectory(prefix="hamster-vs-frankenphp-") as directory:
        hamster_configs = lh.configs(directory)
        caddy = frankenphp_configs(directory, laravel)
        php_ini = write(os.path.join(directory, "php.ini"), PHP_INI)
        stub = None
        if laravel:
            hamster_configs["laravel"] = hamster_laravel_config(directory, laravel, php_ini)
            stub = os.path.join(laravel, "public", "frankenphp-worker.php")
            if os.path.exists(stub):
                stub = None  # the application ships its own; leave it alone
            else:
                shutil.copy(os.path.join(laravel, "vendor/laravel/octane/src/Commands/stubs/"
                                                  "frankenphp-worker.php"), stub)
        # Only the Laravel cells load php.ini; the Hamster worker gets it through
        # php.env, FrankenPHP through its own environment.
        laravel_environment = dict(os.environ, PHPRC=php_ini)

        def hamster(name):
            return [HAMSTER, "--config", hamster_configs[name]]

        def franken(name):
            return [frankenphp, "run", "--config", caddy[name]]

        # group, cell, Hamster config, FrankenPHP config, path, requests, concurrency,
        # status, method, body
        plan = [
            ("static", "static 22 B C1", "static", "static", "/", 50000, 1, 200),
            ("static", "static 22 B C32", "static", "static", "/", 200000, 32, 200),
            ("worker", "worker hello W1 C1", "php_w1", "worker_w1", "/hello", 20000, 1, 200),
            ("worker", "worker json W1 C1", "php_w1", "worker_w1", "/json", 20000, 1, 200),
            ("worker", "worker echo 12 B W1 C1", "php_w1", "worker_w1", "/echo", 20000, 1, 201,
             "POST", "hello world!"),
            ("worker", "worker hello W4 C4", "php_w4", "worker_w4", "/hello", 50000, 4, 200),
            ("worker", "worker hello W4 C32", "php_w4", "worker_w4", "/hello", 50000, 32, 200),
            ("classic", "classic hello 4 C1", "fcgi", "classic", "/bench.php", 5000, 1, 200),
            ("classic", "classic json 4 C16", "fcgi", "classic", "/bench.php?workload=json",
             10000, 16, 200),
            ("laravel", "laravel /login W4 C1", "laravel", "laravel", "/login", 3000, 1, 200),
            ("laravel", "laravel /login W4 C16", "laravel", "laravel", "/login", 10000, 16, 200),
            ("laravel", "laravel welcome W4 C1", "laravel", "laravel", "/", 2000, 1, 200),
            ("laravel", "laravel welcome W4 C16", "laravel", "laravel", "/", 5000, 16, 200),
        ]
        try:
            for group, cell, ham, fr, path, requests, concurrency, expected, *rest in plan:
                if group not in groups:
                    continue
                method, body = (rest[0], rest[1]) if rest else ("GET", None)
                warmup = 200 if group == "classic" else 1000
                frankenphp_environment = laravel_environment if group == "laravel" else None
                arms = {"hamster": (hamster(ham), None),
                        "frankenphp": (franken(fr), frankenphp_environment)}
                rows = {arm: [] for arm in arms}
                for arm in ("hamster", "frankenphp", "frankenphp", "hamster", "hamster", "frankenphp"):
                    command, environment = arms[arm]
                    rows[arm].append(measure(args.oha, command, environment, path, requests,
                                             concurrency,
                                             expected, method, body,
                                             group == "classic" and arm == "hamster", warmup))
                for arm, runs in rows.items():
                    runs.sort(key=lambda r: r["rps"])
                    median = runs[len(runs) // 2]
                    print(json.dumps(dict(
                        cell=cell, server=arm, requests=requests, concurrency=concurrency,
                        rps=[round(r["rps"]) for r in runs],
                        **{k: median[k] for k in ("p50", "p95", "p99", "cpu_us", "rss_mib")},
                        unexpected=sum(r["unexpected"] for r in runs),
                        errors=sum(r["errors"] for r in runs))), flush=True)
        finally:
            if stub:
                os.remove(stub)


if __name__ == "__main__":
    main()
