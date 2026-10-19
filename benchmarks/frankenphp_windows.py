#!/usr/bin/env python3
"""Hamster against FrankenPHP on the same Windows host, same cells, interleaved.

The Windows counterpart of frankenphp_linux.py. Each cell runs both servers
three times in the order H F F H H F, a fresh server per run. The client is
oha on the same host. CPU is user+kernel time of the server's whole process
tree (and of the php-cgi processes for Hamster's classic cells) over the
measured run, divided by requests. Memory is reported twice at the end of the
run, summed over the same processes: working set, which counts shared pages
once per process, and private bytes (commit charge), which does not.

  python benchmarks/frankenphp_windows.py [--frankenphp PATH] [--oha PATH]
      [--laravel APP] [--runs N] [--bin PATH] [--against PATH] [--only TEXT]
      [GROUP ...]

--against replaces FrankenPHP with a second Hamster binary, for an
interleaved A/B between two builds.

GROUP is baseline, static, worker, classic, laravel or all (the default;
laravel only with --laravel). baseline answers 404 without a file or PHP on
both servers: the cost of the HTTP layer alone. php-cgi on Windows serves one connection per process, so the
classic cells start four php-cgi processes on four ports for Hamster, matching
FrankenPHP's four PHP threads. FrankenPHP gets a php.ini written to a temporary
directory through PHPRC, so no php.ini beside its executable is used. Build
release first. Ports 8080 and 9101-9104 must be free.
"""
import argparse
import ctypes
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from ctypes import wintypes

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__))).replace("\\", "/")
PHP_DIR = f"{REPO}/php/8.5"
FASTCGI_PORTS = (9101, 9102, 9103, 9104)
CREATE_NO_WINDOW = 0x08000000
SITE = "http://127.0.0.1:8080, http://localhost:8080"
LARAVEL_EXTENSIONS = ("openssl", "mbstring", "fileinfo", "curl", "intl", "zip", "sodium",
                      "sqlite3", "pdo_sqlite")

# ---------------------------------------------------------------- process data

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
psapi = ctypes.WinDLL("psapi", use_last_error=True)
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_VM_READ = 0x0010
TH32CS_SNAPPROCESS = 0x2


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_void_p),
                ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
                ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", ctypes.c_long),
                ("dwFlags", wintypes.DWORD), ("szExeFile", ctypes.c_wchar * 260)]


class PROCESS_MEMORY_COUNTERS_EX(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
                ("PrivateUsage", ctypes.c_size_t)]


kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
kernel32.OpenProcess.restype = wintypes.HANDLE


def parents():
    """pid -> parent pid for every process on the machine."""
    snapshot = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    entry = PROCESSENTRY32W()
    entry.dwSize = ctypes.sizeof(entry)
    table = {}
    try:
        ok = kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
        while ok:
            table[entry.th32ProcessID] = entry.th32ParentProcessID
            ok = kernel32.Process32NextW(snapshot, ctypes.byref(entry))
    finally:
        kernel32.CloseHandle(snapshot)
    return table


def created(pid):
    """Creation time of a process in FILETIME units, or None."""
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return None
    try:
        times = [wintypes.FILETIME() for _ in range(4)]
        if not kernel32.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)):
            return None
        return (times[0].dwHighDateTime << 32) | times[0].dwLowDateTime
    finally:
        kernel32.CloseHandle(handle)


def tree(roots):
    """The roots and every live descendant.

    Windows reuses process IDs and keeps a child's parent ID after the parent
    exits, so a process whose recorded parent ID now names one of ours may be
    unrelated. A descendant counts only if it was created after its parent.
    """
    table = parents()
    children = {}
    for pid, parent in table.items():
        if pid != parent:
            children.setdefault(parent, []).append(pid)
    found, pending = set(), [r for r in roots if r in table]
    while pending:
        pid = pending.pop()
        if pid in found:
            continue
        found.add(pid)
        born = created(pid)
        for child in children.get(pid, []):
            child_born = created(child)
            if born is not None and child_born is not None and child_born >= born:
                pending.append(child)
    return found


def sample(pid):
    """(cpu seconds, working set bytes, private bytes) of one process."""
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, False, pid)
    if not handle:
        return 0.0, 0, 0
    try:
        creation, exit_, kernel, user = (wintypes.FILETIME() for _ in range(4))
        kernel32.GetProcessTimes(handle, ctypes.byref(creation), ctypes.byref(exit_),
                                 ctypes.byref(kernel), ctypes.byref(user))
        ticks = sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in (kernel, user))
        memory = PROCESS_MEMORY_COUNTERS_EX()
        memory.cb = ctypes.sizeof(memory)
        psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb)
        return ticks / 1e7, memory.WorkingSetSize, memory.PrivateUsage
    finally:
        kernel32.CloseHandle(handle)


def cpu(pids):
    return {pid: sample(pid)[0] for pid in pids}


# ------------------------------------------------------------------- utilities

def write(path, text):
    with open(path, "w", newline="\n") as handle:
        handle.write(text)
    return path


def wait_for_port(port, seconds=60):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return
        except OSError:
            time.sleep(0.05)
    raise SystemExit(f"port {port} never opened")


def wait_for_free(port, seconds=15):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            time.sleep(0.1)
        except OSError:
            return
    raise SystemExit(f"port {port} is still in use")


def start(command, environment=None):
    return subprocess.Popen(command, env=environment, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, creationflags=CREATE_NO_WINDOW)


def stop(process):
    """End exactly the process this script started.

    Never kill by a process tree inferred from parent IDs: Windows reuses
    process IDs, and an earlier version of this function killed an unrelated
    application (VS Code) whose recorded parent ID had been reused by a server
    under test. Hamster's PHP workers end with Hamster through its Job Object;
    FrankenPHP and php-cgi are single processes.
    """
    process.kill()
    process.wait(15)


def load(oha, path, requests, concurrency, method, body):
    command = [oha, "--no-tui", "--output-format", "json", "-n", str(requests),
               "-c", str(concurrency), "-m", method]
    if body is not None:
        command += ["-d", body]
    command.append(f"http://127.0.0.1:8080{path}")
    output = subprocess.run(command, check=True, capture_output=True, text=True,
                            creationflags=CREATE_NO_WINDOW).stdout
    return json.loads(output)


# --------------------------------------------------------------- configuration

WORKER = r"""<?php

declare(strict_types=1);

// FrankenPHP counterpart of the hello, json and echo routes of
// fixtures/php-worker/worker.php: same bodies, statuses and headers.
$handler = static function (): void {
    $path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
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


def php_ini(extension_dir, laravel):
    text = "memory_limit = 512M\ndisplay_errors = stderr\nlog_errors = Off\n"
    if laravel:
        # OPcache on for both servers: the CLI worker needs enable_cli, the
        # embed SAPI uses opcache.enable.
        text += (f'extension_dir = "{extension_dir}"\nopcache.enable = 1\n'
                 "opcache.enable_cli = 1\n")
        text += "".join(f"extension={name}\n" for name in LARAVEL_EXTENSIONS)
    return text


def caddyfile(global_block, site_block):
    return f"{{\n\tadmin off\n\tauto_https off\n{global_block}}}\n{SITE} {{\n{site_block}}}\n"


def hamster_head(handler):
    return ('[server]\nlisten = "127.0.0.1:8080"\n[[hosts]]\nname = "localhost"\n'
            f'aliases = ["127.0.0.1"]\n[[hosts.locations]]\npath = "/"\nhandler = "{handler}"\n')


def configs(directory, static_root, laravel, hamster_ini):
    worker_root = f"{directory}/worker"
    os.makedirs(worker_root)
    write(f"{worker_root}/index.php", WORKER)
    servers = ", ".join(f'"127.0.0.1:{port}"' for port in FASTCGI_PORTS)
    php = ('[hosts.locations.php]\nexecutable = "{0}/php.exe"\n'
           'script = "{1}/fixtures/php-worker/worker.php"\nworkers = {2}\nqueue_capacity = 64\n')
    hamster = {
        "static": hamster_head("static") + f'root = "{static_root}"\n',
        "not_found": hamster_head("not_found"),
        "fcgi": hamster_head("fastcgi") + f'root = "{REPO}/fixtures/php-cgi"\n'
        f'[hosts.locations.fastcgi]\nservers = [{servers}]\nconcurrency = 1\nindex = "bench.php"\n',
    }
    franken = {
        # Paths are quoted: the repository path may contain spaces.
        "static": caddyfile("", f"\troot * \"{static_root}\"\n\tfile_server\n"),
        "not_found": caddyfile("", "\trespond 404\n"),
        "classic": caddyfile("\tfrankenphp {\n\t\tnum_threads 4\n\t}\n",
                             f"\troot * \"{REPO}/fixtures/php-cgi\"\n\tphp_server {{\n"
                             "\t\tindex bench.php\n\t}\n"),
    }
    for workers in (1, 4):
        hamster[f"php_w{workers}"] = hamster_head("php_worker") + php.format(PHP_DIR, REPO, workers)
        franken[f"worker_w{workers}"] = caddyfile(
            f"\tfrankenphp {{\n\t\tnum_threads {workers + 1}\n\t\tworker {{\n"
            f"\t\t\tfile \"{worker_root}/index.php\"\n\t\t\tnum {workers}\n\t\t}}\n\t}}\n",
            f"\troot * \"{worker_root}\"\n\tphp_server\n")
    if laravel:
        env = "".join(f'{k} = "{v}"\n' for k, v in LARAVEL_ENV.items())
        hamster["laravel"] = (hamster_head("php_worker") + php.format(PHP_DIR, REPO, 4).replace(
            "fixtures/php-worker/worker.php", "fixtures/php-worker/laravel_worker.php")
            + f'[hosts.locations.php.env]\nHAMSTER_LARAVEL_PATH = "{laravel}"\n'
            f'PHPRC = "{hamster_ini}"\n{env}')
        franken_env = "".join(f"\t\t\tenv {k} {v}\n" for k, v in LARAVEL_ENV.items())
        franken["laravel"] = caddyfile(
            "\tfrankenphp {\n\t\tnum_threads 5\n\t\tworker {\n"
            f"\t\t\tfile \"{laravel}/public/frankenphp-worker.php\"\n\t\t\tnum 4\n"
            f"\t\t\tenv APP_BASE_PATH \"{laravel}\"\n\t\t\tenv APP_PUBLIC_PATH \"{laravel}/public\"\n"
            f"\t\t\tenv LARAVEL_OCTANE 1\n{franken_env}\t\t}}\n\t}}\n",
            f"\troute {{\n\t\troot * \"{laravel}/public\"\n\t\tphp_server {{\n"
            "\t\t\tindex frankenphp-worker.php\n\t\t\ttry_files {path} frankenphp-worker.php\n"
            "\t\t}\n\t}\n")
    return ({name: write(f"{directory}/{name}.toml", text) for name, text in hamster.items()},
            {name: write(f"{directory}/{name}.Caddyfile", text) for name, text in franken.items()})


# ----------------------------------------------------------------- measurement

def measure(oha, command, environment, path, requests, concurrency, expected, method, body,
            fastcgi, warmup):
    for port in (8080,) + (FASTCGI_PORTS if fastcgi else ()):
        wait_for_free(port)
    cgis = []
    server = None
    try:
        if fastcgi:
            cgi_environment = dict(os.environ, PHP_FCGI_MAX_REQUESTS="0")
            for port in FASTCGI_PORTS:
                cgis.append(start([f"{PHP_DIR}/php-cgi.exe", "-d", "output_buffering=0", "-b",
                                   f"127.0.0.1:{port}"], cgi_environment))
            for port in FASTCGI_PORTS:
                wait_for_port(port)
        server = start(command, environment)
        roots = [server.pid] + [c.pid for c in cgis]
        wait_for_port(8080)
        time.sleep(0.5)
        load(oha, path, warmup, min(concurrency, 8), method, body)
        time.sleep(0.3)
        before = cpu(tree(roots))
        result = load(oha, path, requests, concurrency, method, body)
        pids = tree(roots)
        samples = {pid: sample(pid) for pid in pids}
        seconds = sum(s[0] - before.get(pid, 0.0) for pid, s in samples.items())
        percentiles = result["latencyPercentiles"]
        own = samples.get(server.pid, (0.0,))[0] - before.get(server.pid, 0.0)
        return dict(rps=result["summary"]["requestsPerSec"], p50=percentiles["p50"] * 1e3,
                    p95=percentiles["p95"] * 1e3, p99=percentiles["p99"] * 1e3,
                    cpu_us=seconds * 1e6 / requests,
                    # The server process alone, without PHP children or php-cgi.
                    server_cpu_us=own * 1e6 / requests,
                    ws_mib=sum(s[1] for s in samples.values()) / 2**20,
                    private_mib=sum(s[2] for s in samples.values()) / 2**20,
                    processes=len(pids),
                    unexpected=requests - result["statusCodeDistribution"].get(str(expected), 0),
                    errors=sum(result.get("errorDistribution", {}).values()))
    finally:
        # Whatever failed, nothing this run started outlives it.
        if server is not None:
            stop(server)
        for cgi in cgis:
            stop(cgi)
        time.sleep(0.5)


# name, group, Hamster config, FrankenPHP config, path, requests, concurrency, status
# [, method, body]
PLAN = [
    ("baseline", "not_found C1", "not_found", "not_found", "/", 50000, 1, 404),
    ("baseline", "not_found C32", "not_found", "not_found", "/", 200000, 32, 404),
    ("static", "static 22 B C1", "static", "static", "/", 50000, 1, 200),
    ("static", "static 22 B C32", "static", "static", "/", 200000, 32, 200),
    ("static", "static 64 KiB C32", "static", "static", "/64k.bin", 50000, 32, 200),
    ("static", "static 1 MiB C8", "static", "static", "/1m.bin", 5000, 8, 200),
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


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("groups", nargs="*", default=["all"])
    parser.add_argument("--frankenphp",
                        default=os.path.expanduser("~/.frankenphp/frankenphp.exe"))
    parser.add_argument("--oha", default=shutil.which("oha") or
                        os.path.expanduser("~/.cargo/bin/oha.exe"))
    parser.add_argument("--laravel")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--bin", default=f"{REPO}/target/x86_64-pc-windows-msvc/release/"
                                         "hamster-wheel.exe")
    parser.add_argument("--only", help="run only the cells whose name contains this text")
    parser.add_argument("--against", help="a second Hamster binary to run in place of "
                                          "FrankenPHP, for a build-against-build A/B")
    args = parser.parse_args()
    frankenphp = os.path.abspath(args.frankenphp)
    # CreateProcess does not resolve a relative path written with '/'.
    args.bin = os.path.abspath(args.bin)
    args.against = os.path.abspath(args.against) if args.against else None
    laravel = os.path.abspath(args.laravel).replace("\\", "/") if args.laravel else None
    groups = args.groups
    if "all" in groups:
        groups = ["baseline", "static", "worker", "classic"] + (["laravel"] if laravel else [])
    if "laravel" in groups and not laravel:
        parser.error("the laravel group needs --laravel")
    for path in (args.bin, args.against or frankenphp, args.oha):
        if not os.path.isfile(path):
            parser.error(f"not found: {path}")

    with tempfile.TemporaryDirectory(prefix="hamster-vs-frankenphp-") as directory:
        directory = directory.replace("\\", "/")
        static_root = f"{directory}/static"
        os.makedirs(static_root)
        shutil.copy(f"{REPO}/public/index.html", f"{static_root}/index.html")
        for name, size in (("64k.bin", 64 * 1024), ("1m.bin", 1024 * 1024)):
            with open(f"{static_root}/{name}", "wb") as handle:
                handle.write(os.urandom(size))
        franken_ext = os.path.dirname(frankenphp).replace("\\", "/") + "/ext"
        for name, ext, laravel_ini in (("hamster-ini", f"{PHP_DIR}/ext", True),
                                       ("franken-ini", franken_ext, False),
                                       ("franken-laravel-ini", franken_ext, True)):
            os.makedirs(f"{directory}/{name}")
            write(f"{directory}/{name}/php.ini", php_ini(ext, laravel_ini))
        hamster, franken = configs(directory, static_root, laravel,
                                   f"{directory}/hamster-ini/php.ini")
        stub = None
        if laravel and "laravel" in groups:
            stub = f"{laravel}/public/frankenphp-worker.php"
            if os.path.exists(stub):
                stub = None  # the application ships its own; leave it alone
            else:
                shutil.copy(f"{laravel}/vendor/laravel/octane/src/Commands/stubs/"
                            "frankenphp-worker.php", stub)
        try:
            for group, cell, ham, fr, path, requests, concurrency, expected, *rest in PLAN:
                if group not in groups or (args.only and args.only not in cell):
                    continue
                method, body = (rest[0], rest[1]) if rest else ("GET", None)
                warmup = 200 if group == "classic" else 1000
                ini = "franken-laravel-ini" if group == "laravel" else "franken-ini"
                if args.against:
                    # Two Hamster builds in the same cells: a direct A/B.
                    other = ("against", ([args.against, "--config", hamster[ham]], None))
                else:
                    other = ("frankenphp", ([frankenphp, "run", "--config", franken[fr]],
                                            dict(os.environ, PHPRC=f"{directory}/{ini}")))
                arms = {"hamster": ([args.bin, "--config", hamster[ham]], None),
                        other[0]: other[1]}
                order = ["hamster", other[0], other[0], "hamster"] * args.runs
                order = order[:2 * args.runs]
                rows = {arm: [] for arm in arms}
                for arm in order:
                    command, environment = arms[arm]
                    rows[arm].append(measure(args.oha, command, environment, path, requests,
                                             concurrency, expected, method, body,
                                             group == "classic" and arm != "frankenphp",
                                             warmup))
                for arm, runs in rows.items():
                    runs.sort(key=lambda r: r["rps"])
                    median = runs[len(runs) // 2]
                    print(json.dumps(dict(
                        cell=cell, server=arm, requests=requests, concurrency=concurrency,
                        rps=[round(r["rps"]) for r in runs],
                        **{k: round(median[k], 3) for k in ("p50", "p95", "p99", "cpu_us",
                                                            "server_cpu_us", "ws_mib",
                                                            "private_mib")},
                        processes=median["processes"],
                        unexpected=sum(r["unexpected"] for r in runs),
                        errors=sum(r["errors"] for r in runs))), flush=True)
        finally:
            if stub:
                os.remove(stub)


if __name__ == "__main__":
    main()
