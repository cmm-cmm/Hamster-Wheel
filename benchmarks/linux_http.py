#!/usr/bin/env python3
"""Closed-loop HTTP release benchmark for Linux, the counterpart of the
PowerShell drivers in this directory, which need .NET.

Each run starts a fresh server (and, for FastCGI, a fresh `php-cgi -b`),
warms up, then measures one `oha` run. Server and PHP CPU come from
/proc/<pid>/stat over the measured run (user+system, divided by requests);
RSS is VmRSS at the end of the run. With two or more --bin arguments the
binaries alternate A B B A A B per cell, and one line is printed per binary.

  benchmarks/linux_http.py [--runs N] [--oha PATH] [--bin PATH ...] [GROUP ...]

GROUP is static, simple, php, fcgi or all (the default). Every result is one
JSON line on stdout. Build release first; `cargo install oha` provides the
client. Port 8080 (and 9101 for FastCGI) must be free.
"""
import argparse
import json
import os
import socket
import subprocess
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TICK = os.sysconf("SC_CLK_TCK")
FASTCGI_PORT = 9101


def configs(directory):
    head = '[server]\nlisten = "127.0.0.1:8080"\n[[hosts]]\nname = "localhost"\naliases = ["127.0.0.1"]\n[[hosts.locations]]\npath = "/"\n'
    php = '[hosts.locations.php]\nexecutable = "{php}"\nscript = "{script}"\nworkers = {workers}\nqueue_capacity = 64\n'
    files = {
        "static": head + f'handler = "static"\nroot = "{REPO}/public"\n',
        "simple": head + 'handler = "not_found"\n',
        "fcgi": head + f'handler = "fastcgi"\nroot = "{REPO}/fixtures/php-cgi"\n'
        f'[hosts.locations.fastcgi]\nservers = ["127.0.0.1:{FASTCGI_PORT}"]\nconcurrency = 4\nindex = "bench.php"\n',
    }
    for workers in (1, 4):
        files[f"php_w{workers}"] = head + 'handler = "php_worker"\n' + php.format(
            php=f"{REPO}/php/8.5/php", script=f"{REPO}/fixtures/php-worker/worker.php", workers=workers)
    paths = {}
    for name, text in files.items():
        paths[name] = os.path.join(directory, f"{name}.toml")
        with open(paths[name], "w") as handle:
            handle.write(text)
    return paths


# name, config, path, requests, concurrency, expected status, method, body
PLAN = {
    "static": [("static 22 B C1", "static", "/", 50000, 1, 200),
               ("static 22 B C32", "static", "/", 200000, 32, 200)],
    "simple": [("not_found C1", "simple", "/", 100000, 1, 404),
               ("not_found C32", "simple", "/", 500000, 32, 404)],
    "php": [("php_hello W1 C1", "php_w1", "/hello", 20000, 1, 200),
            ("php_json W1 C1", "php_w1", "/json", 20000, 1, 200),
            ("php_echo 12 B W1 C1", "php_w1", "/echo", 20000, 1, 201, "POST", "hello world!"),
            ("php_hello W4 C4", "php_w4", "/hello", 50000, 4, 200),
            ("php_hello W4 C32", "php_w4", "/hello", 50000, 32, 200)],
    "fcgi": [("fcgi_hello K4 C1", "fcgi", "/bench.php", 5000, 1, 200),
             ("fcgi_hello K4 C4", "fcgi", "/bench.php", 10000, 4, 200),
             ("fcgi_json K4 C16", "fcgi", "/bench.php?workload=json", 10000, 16, 200)],
}


def process_table():
    table = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as handle:
                stat = handle.read()
        except OSError:
            continue
        fields = stat[stat.rindex(")") + 2:].split()
        table[int(entry)] = (int(fields[1]), int(fields[11]) + int(fields[12]))
    return table


def cpu_ticks(root):
    """CPU ticks of root and every descendant, by pid."""
    table = process_table()
    children = {}
    for pid, (parent, _) in table.items():
        children.setdefault(parent, []).append(pid)
    ticks, pending = {}, [root]
    while pending:
        pid = pending.pop()
        if pid in table:
            ticks[pid] = table[pid][1]
            pending.extend(children.get(pid, []))
    return ticks


def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status") as handle:
            for line in handle:
                if line.startswith("VmRSS"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def wait_for_port(port, seconds=15):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return
        except OSError:
            time.sleep(0.05)
    raise SystemExit(f"port {port} never opened")


def load(oha, path, requests, concurrency, method, body):
    command = [oha, "--no-tui", "--output-format", "json", "-n", str(requests),
               "-c", str(concurrency), "-m", method]
    if body is not None:
        command += ["-d", body]
    command.append(f"http://127.0.0.1:8080{path}")
    return json.loads(subprocess.run(command, check=True, capture_output=True, text=True).stdout)


def measure(args, binary, config, path, requests, concurrency, expected, method, body, fastcgi):
    cgi = None
    if fastcgi:
        environment = dict(os.environ, PHP_FCGI_CHILDREN="4", PHP_FCGI_MAX_REQUESTS="0")
        cgi = subprocess.Popen([f"{REPO}/php/8.5/php-cgi", "-d", "output_buffering=0", "-b",
                                f"127.0.0.1:{FASTCGI_PORT}"], env=environment,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for_port(FASTCGI_PORT)
    server = subprocess.Popen([binary, "--config", config], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    try:
        wait_for_port(8080)
        load(args.oha, path, 200 if fastcgi else 1000, min(concurrency, 8), method, body)
        time.sleep(0.3)
        before = cpu_ticks(server.pid)
        cgi_before = cpu_ticks(cgi.pid) if cgi else {}
        result = load(args.oha, path, requests, concurrency, method, body)
        after = cpu_ticks(server.pid)
        cgi_after = cpu_ticks(cgi.pid) if cgi else {}
        hamster = (after.get(server.pid, 0) - before.get(server.pid, 0)) / TICK
        php = sum(v - before.get(p, 0) for p, v in after.items() if p != server.pid) / TICK
        php += sum(v - cgi_before.get(p, 0) for p, v in cgi_after.items()) / TICK
        php_rss = sum(rss_kib(p) for p in after if p != server.pid)
        php_rss += sum(rss_kib(p) for p in cgi_after)
        percentiles = result["latencyPercentiles"]
        return dict(rps=result["summary"]["requestsPerSec"],
                    p50=percentiles["p50"] * 1e3, p95=percentiles["p95"] * 1e3,
                    p99=percentiles["p99"] * 1e3,
                    hamster_us=hamster * 1e6 / requests, php_us=php * 1e6 / requests,
                    hamster_mib=rss_kib(server.pid) / 1024, php_mib=php_rss / 1024,
                    unexpected=requests - result["statusCodeDistribution"].get(str(expected), 0),
                    errors=sum(result.get("errorDistribution", {}).values()))
    finally:
        server.terminate()
        server.wait(10)
        if cgi:
            cgi.terminate()
            cgi.wait(10)
        time.sleep(0.3)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("groups", nargs="*", default=["all"])
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--bin", action="append", dest="bins")
    parser.add_argument("--oha", default="oha")
    args = parser.parse_args()
    bins = [os.path.abspath(b) for b in (args.bins or [f"{REPO}/target/release/hamster-wheel"])]
    groups = list(PLAN) if "all" in args.groups else args.groups

    with tempfile.TemporaryDirectory(prefix="hamster-linux-bench-") as directory:
        paths = configs(directory)
        for group in groups:
            for name, config, path, requests, concurrency, expected, *rest in PLAN[group]:
                method, body = (rest[0], rest[1]) if rest else ("GET", None)
                rows = {b: [] for b in bins}
                for index in range(args.runs * len(bins)):
                    binary = bins[(index + index // len(bins)) % len(bins)]
                    rows[binary].append(measure(args, binary, paths[config], path, requests,
                                                concurrency, expected, method, body,
                                                group == "fcgi"))
                for binary in bins:
                    runs = rows[binary]
                    median = sorted(runs, key=lambda r: r["rps"])[len(runs) // 2]
                    print(json.dumps(dict(
                        cell=name, binary=binary, requests=requests, concurrency=concurrency,
                        rps_min=min(r["rps"] for r in runs), rps_median=median["rps"],
                        rps_max=max(r["rps"] for r in runs),
                        **{k: median[k] for k in ("p50", "p95", "p99", "hamster_us", "php_us",
                                                  "hamster_mib", "php_mib")},
                        unexpected=sum(r["unexpected"] for r in runs),
                        errors=sum(r["errors"] for r in runs))), flush=True)


if __name__ == "__main__":
    main()
