"""Node comparison fixture and exact-handle cleanup regressions."""
import hashlib
import sys
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

if sys.platform == "win32":
    import node_windows as driver


@unittest.skipUnless(sys.platform == "win32", "Windows measurement helper")
class NodeDriverTests(unittest.TestCase):
    def test_cpu_reference_uses_the_same_twenty_thousand_hex_rounds(self):
        expected = b"a" * 64
        for _ in range(20000):
            expected = hashlib.sha256(expected).hexdigest().encode()
        check = driver.expected_body("heavy", "/cpu", "unused")
        self.assertTrue(check(expected))
        self.assertFalse(check(b"a" * 64))

    def test_json_and_large_body_references_are_exact(self):
        check = driver.expected_body("worker", "/json", "unused")
        self.assertTrue(check(b'{"message":"Hello from PHP"}'))
        self.assertFalse(check(b'{"message":"Hello from Node"}'))
        check = driver.expected_body("heavy", "/body", "unused")
        self.assertTrue(check(hashlib.sha256(b"x" * 1024 * 1024).hexdigest().encode()))

    def test_cleanup_visits_every_retained_handle_before_reporting_failure(self):
        process = SimpleNamespace(stdin=Mock(), wait=Mock(), returncode=0,
                                  worker_handles=[101, 102])
        with (patch.object(driver.KERNEL, "WaitForSingleObject", side_effect=[258, 0, 0]),
              patch.object(driver.KERNEL, "TerminateProcess") as terminate,
              patch.object(driver.KERNEL, "CloseHandle") as close):
            with self.assertRaisesRegex(RuntimeError, "forced cleanup"):
                driver.finish_node(process)
        terminate.assert_called_once_with(101, 1)
        self.assertEqual([call.args[0] for call in close.call_args_list], [101, 102])
        process.stdin.close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
