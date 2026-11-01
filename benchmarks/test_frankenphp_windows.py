"""Cheap driver regressions; no servers or load generators are started."""
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

if sys.platform == "win32":
    import frankenphp_windows as driver


@unittest.skipUnless(sys.platform == "win32", "Windows benchmark driver")
class DriverTests(unittest.TestCase):
    def test_exact_cells_select_a_predeclared_matrix_without_substring_overlap(self):
        names = ["worker hello W1 C1", "worker json W4 C32"]
        rows = driver.select_plan(["worker"], cells=names)
        self.assertEqual([row[1] for row in rows], names)
        with self.assertRaisesRegex(ValueError, "unknown cells"):
            driver.select_plan(["worker"], cells=["worker json W4 C33"])
        with self.assertRaisesRegex(ValueError, "unknown cells"):
            driver.select_plan(["static"], cells=names)

    def test_static_body_check_uses_the_actual_fixture_bytes(self):
        with tempfile.TemporaryDirectory(prefix="hamster-bench-config-") as directory:
            with open(f"{directory}/index.html", "wb") as handle:
                handle.write(b"\x00\xfffixture\r\n")
            check = driver.body_verifier("static", "/", None, directory)
        self.assertTrue(check(b"\x00\xfffixture\r\n"))
        self.assertFalse(check(b"fixture\n"))

    def test_worker_and_classic_checks_reject_wrong_bodies_despite_correct_status(self):
        for group, path in (("worker", "/json"), ("classic", "/bench.php?workload=json")):
            check = driver.body_verifier(group, path, None, "unused")
            self.assertTrue(check(b'{"message":"Hello from PHP"}'))
            self.assertFalse(check(b"Hello from PHP"))
        check = driver.body_verifier("worker", "/echo", "hello world!", "unused")
        self.assertTrue(check(b"hello world!"))
        self.assertFalse(check(b"hello"))

    def test_caddy_environment_values_include_explicit_empty_strings(self):
        with tempfile.TemporaryDirectory(prefix="hamster-bench-config-") as temporary:
            directory = temporary.replace("\\", "/")
            _, franken = driver.configs(directory, directory, directory, f"{directory}/php.ini")
            with open(franken["laravel"], encoding="utf-8") as handle:
                config = handle.read()
            self.assertIn('env DB_URL ""\n', config)
            self.assertIn('env DB_DATABASE ":memory:"\n', config)

    def test_readiness_reports_process_exit_instead_of_waiting_for_a_dead_server(self):
        process = SimpleNamespace(poll=lambda: 0xC0000005,
                                  returncode=0xC0000005, benchmark_log="scratch.log")
        with patch.object(driver.socket, "create_connection") as connect:
            with self.assertRaisesRegex(RuntimeError, "0xC0000005.*scratch.log"):
                driver.wait_for_port(8080, process=process)
        connect.assert_not_called()

    def test_timed_load_waits_for_requests_at_deadline(self):
        with patch.object(driver.subprocess, "run", return_value=SimpleNamespace(stdout="{}")) as run:
            driver.load("oha", "/", 999, 4, "GET", None, 10)
        command = run.call_args.args[0]
        self.assertIn("-w", command)
        self.assertNotIn("-n", command)
        self.assertEqual(command[command.index("-z") + 1], "10s")

    def test_warmup_stays_request_counted(self):
        with patch.object(driver.subprocess, "run", return_value=SimpleNamespace(stdout="{}")) as run:
            driver.load("oha", "/", 200, 4, "GET", None)
        command = run.call_args.args[0]
        self.assertNotIn("-z", command)
        self.assertEqual(command[command.index("-n") + 1], "200")

    def test_large_body_uses_a_file_not_a_windows_command_line_argument(self):
        with patch.object(driver.subprocess, "run", return_value=SimpleNamespace(stdout="{}")) as run:
            driver.load("oha", "/body", 200, 16, "POST", {"file": "request-1m.bin"}, 10)
        command = run.call_args.args[0]
        self.assertNotIn("-d", command)
        self.assertEqual(command[command.index("-D") + 1], "request-1m.bin")

    def test_cpu_uses_actual_requests_not_the_plan_count(self):
        warmup = dict(statusCodeDistribution={"200": 10})
        measured = dict(summary=dict(requestsPerSec=10, total=10),
                        statusCodeDistribution={"200": 100},
                        latencyPercentiles=dict(p50=.01, p95=.02, p99=.03))
        with (patch.object(driver, "start", return_value=SimpleNamespace(pid=10)),
              patch.object(driver, "stop"), patch.object(driver, "wait_for_free"),
              patch.object(driver, "wait_for_port"), patch.object(driver.time, "sleep"),
              patch.object(driver, "tree", return_value={10}),
              patch.object(driver, "cpu", return_value={10: 1.0}),
              patch.object(driver, "sample", return_value=(1.2, 2**20, 2**19)),
              patch.object(driver, "created", return_value=123),
              patch.object(driver, "load", side_effect=[warmup, measured])):
            row = driver.measure("oha", ["server"], None, "/", 9999, 1, 200,
                                 "GET", None, False, 10, 10)
        self.assertEqual(row["requests"], 100)
        self.assertAlmostEqual(row["cpu_us"], 2000)
        self.assertEqual((row["errors"], row["unexpected"]), (0, 0))
        self.assertEqual(row["goodput_rps"], 10)
        self.assertAlmostEqual(row["cpu_us_per_success"], 2000)

    def test_goodput_does_not_count_busy_responses(self):
        warmup = dict(statusCodeDistribution={"200": 10})
        measured = dict(summary=dict(requestsPerSec=10, total=10),
                        statusCodeDistribution={"200": 20, "503": 80},
                        latencyPercentiles=dict(p50=.01, p95=.02, p99=.03))
        with (patch.object(driver, "start", return_value=SimpleNamespace(pid=10)),
              patch.object(driver, "stop"), patch.object(driver, "wait_for_free"),
              patch.object(driver, "wait_for_port"), patch.object(driver.time, "sleep"),
              patch.object(driver, "tree", return_value={10}),
              patch.object(driver, "cpu", return_value={10: 1.0}),
              patch.object(driver, "sample", return_value=(1.2, 2**20, 2**19)),
              patch.object(driver, "created", return_value=123),
              patch.object(driver, "load", side_effect=[warmup, measured])):
            row = driver.measure("oha", ["server"], None, "/", 9999, 128, 200,
                                 "GET", None, False, 10, 10)
        self.assertEqual(row["goodput_rps"], 2)
        self.assertEqual(row["status_codes"]["503"], 80)
        self.assertAlmostEqual(row["cpu_us_per_success"], 10000)


if __name__ == "__main__":
    unittest.main()
