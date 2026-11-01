#!/usr/bin/env python3
"""Interleaved Windows release Hamster/PHP vs Node core HTTP, no npm packages.

Three fresh process trees per arm, H N N H H N, 1000 warm-up requests,
10 seconds oha -w per run. CPU/RAM include Node cluster primary + workers.
No Laravel/Express equivalence or matched admission policy is claimed.
"""
import argparse
import ctypes
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from ctypes import wintypes

import frankenphp_windows as shared

REPO = shared.REPO
FIXTURE = f"{REPO}/benchmarks/fixtures/node_http.cjs"
KERNEL = shared.kernel32
KERNEL.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
KERNEL.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
KERNEL.CloseHandle.argtypes = [wintypes.HANDLE]
KERNEL.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]

# name, handler mode, workers, concurrency, path, expected status
PLAN = [
    ("not_found C1", "not_found", 1, 1, "/", 404),
    ("not_found C32", "not_found", 1, 32, "/", 404),
    ("static 23 B C32", "static", 1, 32, "/", 200),
    ("static 1 MiB C8", "static", 1, 8, "/1m.bin", 200),
    ("hello W1 C1", "worker", 1, 1, "/hello", 200),
    ("hello W4 C32", "worker", 4, 32, "/hello", 200),
    ("json W4 C32", "worker", 4, 32, "/json", 200),
    ("CPU SHA256x20000 W4 C16", "heavy", 4, 16, "/cpu", 200),
    ("POST 1 MiB digest W4 C16", "heavy", 4, 16, "/body", 200),
]


def digest(path):
    with open(path, "rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def launch_node(command, environment):
    manifest = command[-2]
    fd, log_path = tempfile.mkstemp(prefix="node-", suffix=".log", dir=shared.LOG_DIR)
    with os.fdopen(fd, "wb") as log:
        process = subprocess.Popen(command, env=environment, stdin=subprocess.PIPE,
                                   stdout=log, stderr=subprocess.STDOUT,
                                   creationflags=shared.CREATE_NO_WINDOW)
    process.benchmark_log = log_path
    process.worker_handles = []
    try:
        deadline = time.monotonic() + 30
        while not os.path.exists(manifest):
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(f"Node cluster not ready; log={log_path}")
            time.sleep(.02)
        # Writer closes the file immediately; retry partial publication only.
        while True:
            try:
                with open(manifest, encoding="utf-8") as handle:
                    process.cluster_manifest = json.load(handle)
                break
            except (json.JSONDecodeError, PermissionError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(.01)
        for pid in process.cluster_manifest["workers"]:
            # Retain exact process handles; never terminate from a PID snapshot.
            handle = KERNEL.OpenProcess(0x100000 | 0x1000 | 0x0001, False, pid)
            if not handle:
                raise ctypes.WinError(ctypes.get_last_error())
            process.worker_handles.append(handle)
        return process
    except BaseException:
        finish_node(process)
        raise


def finish_node(process):
    forced = False
    try:
        if process.stdin:
            process.stdin.close()  # EOF -> primary disconnects and waits for workers.
        try:
            process.wait(12)
        except subprocess.TimeoutExpired:
            shared.stop(process)
            forced = True
        for handle in process.worker_handles:
            if KERNEL.WaitForSingleObject(handle, 10000) != 0:
                # Safety cleanup only, using a retained handle, then fail the run.
                KERNEL.TerminateProcess(handle, 1)
                KERNEL.WaitForSingleObject(handle, 10000)
                forced = True
    finally:
        for handle in process.worker_handles:
            KERNEL.CloseHandle(handle)
    if forced or process.returncode:
        raise RuntimeError(f"Node needed forced cleanup or exited unsuccessfully: {process.returncode}")


def expected_body(mode, path, static_root):
    if mode == "not_found":
        return lambda body: True  # Hamster has an error body; Node's 404 is empty.
    if mode == "static":
        with open(f"{static_root}/" + ("index.html" if path == "/" else "1m.bin"), "rb") as handle:
            expected = handle.read()
    elif path == "/json":
        expected = b'{"message":"Hello from PHP"}'
    elif path == "/body":
        expected = hashlib.sha256(b"x" * 1024 * 1024).hexdigest().encode()
    elif path == "/cpu":
        expected = b"a" * 64
        for _ in range(20000):
            expected = hashlib.sha256(expected).hexdigest().encode()
    else:
        expected = b"Hello from PHP"
    return lambda body: body == expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--node", default=shutil.which("node"))
    parser.add_argument("--bin", default=f"{REPO}/target/x86_64-pc-windows-msvc/release/hamster-wheel.exe")
    parser.add_argument("--php-dir", required=True)
    parser.add_argument("--oha", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--duration", type=float, default=10)
    parser.add_argument("--only")
    args = parser.parse_args()
    if args.duration <= 0:
        parser.error("duration must be positive")
    selected = [row for row in PLAN if not args.only or args.only in row[0]]
    if not selected:
        parser.error("no matching cells")
    for name in ("node", "bin", "oha"):
        value = getattr(args, name)
        if not value or not os.path.isfile(value):
            parser.error(f"not found: {value}")
        setattr(args, name, os.path.abspath(value))
    shared.PHP_DIR = os.path.abspath(args.php_dir).replace("\\", "/")
    shared.LOG_DIR = os.path.abspath(args.output + ".logs")
    # Reserve exclusive artifacts before any server starts.
    with open(args.output, "x", encoding="utf-8"):
        pass
    os.makedirs(shared.LOG_DIR, exist_ok=True)
    metadata = dict(platform=platform.platform(), head=subprocess.check_output(
        ["git", "rev-parse", "HEAD"], text=True).strip(), duration=args.duration,
        node=subprocess.check_output([args.node, "--version"], text=True).strip(),
        node_versions=json.loads(subprocess.check_output([args.node, "-p", "JSON.stringify(process.versions)"], text=True)),
        php=subprocess.check_output([f"{shared.PHP_DIR}/php.exe", "-n", "-v"], text=True),
        oha=subprocess.check_output([args.oha, "--version"], text=True).strip(),
        binaries={p: digest(p) for p in (args.bin, args.node, args.oha, f"{shared.PHP_DIR}/php.exe")},
        sources={p: digest(f"{REPO}/{p}") for p in (
            "src/php_protocol.rs", "src/php_worker.rs", "benchmarks/fixtures/node_http.cjs",
            "benchmarks/fixtures/heavy_workload.php", "benchmarks/node_windows.py",
            "benchmarks/frankenphp_windows.py")})
    with open(args.output + ".meta.json", "x", encoding="utf-8") as handle:
        json.dump(metadata, handle, indent=2)
    with tempfile.TemporaryDirectory(prefix="hamster-vs-node-") as temporary:
        directory = temporary.replace("\\", "/")
        root = f"{directory}/static"
        os.makedirs(root)
        shutil.copy(f"{REPO}/public/index.html", f"{root}/index.html")
        # Deterministic bytes shared by both servers, validated before/after.
        payload = b"x" * 1024 * 1024
        with open(f"{root}/1m.bin", "wb") as handle:
            handle.write(payload)
        ini_dir = f"{directory}/ini"
        os.makedirs(ini_dir)
        shared.write(f"{ini_dir}/php.ini", shared.php_ini(f"{shared.PHP_DIR}/ext", False))
        environment = dict(os.environ, PHPRC=ini_dir, PHP_INI_SCAN_DIR="")
        for cell, mode, workers, concurrency, path, expected in selected:
            config = shared.hamster_head("php_worker" if mode in ("worker", "heavy") else mode)
            if mode == "static":
                config += f'root = "{root}"\n'
            elif mode in ("worker", "heavy"):
                script = ("benchmarks/fixtures/heavy_worker.php" if mode == "heavy"
                          else "fixtures/php-worker/worker.php")
                config += (f'[hosts.locations.php]\nexecutable = "{shared.PHP_DIR}/php.exe"\n'
                           f'script = "{REPO}/{script}"\nworkers = {workers}\nqueue_capacity = 64\n')
            config_path = shared.write(f"{directory}/hamster.toml", config)
            method, body = ("POST", {"file": f"{root}/1m.bin"}) if path == "/body" else ("GET", None)
            verify = expected_body(mode, path, root)
            rows = {"hamster": [], "node": []}
            for sequence, arm in enumerate(("hamster", "node", "node", "hamster", "hamster", "node"), 1):
                # A previous cell's READY file must never admit the next tree.
                cell_id = hashlib.sha256(cell.encode()).hexdigest()[:12]
                manifest = f"{directory}/node-{cell_id}-{sequence}.json"
                command = ([args.bin, "--config", config_path] if arm == "hamster" else
                           [args.node, FIXTURE, mode, str(workers), root, manifest, "8080"])
                row = shared.measure(args.oha, command, environment, path, 1000, concurrency,
                                     expected, method, body, False, 1000, args.duration, verify,
                                     launch=launch_node if arm == "node" else None,
                                     finish=finish_node if arm == "node" else None)
                row.update(cell=cell, server=arm, sequence=sequence, workers=workers,
                           concurrency=concurrency, warmup=1000, target_seconds=args.duration)
                rows[arm].append(row)
                with open(args.output, "a", encoding="utf-8") as handle:
                    handle.write(json.dumps(row) + "\n")
                print(json.dumps(dict(cell=cell, server=arm, sequence=sequence,
                                      rps=round(row["goodput_rps"]), errors=row["errors"],
                                      unexpected=row["unexpected"])), flush=True)
            for arm, runs in rows.items():
                print(json.dumps(dict(cell=cell, server=arm,
                    rps=sorted(round(r["goodput_rps"]) for r in runs),
                    medians={k: round(statistics.median(r[k] for r in runs), 3)
                             for k in ("p50", "p95", "p99", "cpu_us", "ws_mib", "private_mib")})), flush=True)


if __name__ == "__main__":
    main()
