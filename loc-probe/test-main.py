#!/usr/bin/env python3
"""Only run the stub-linked CLI executable; never the real QRTR transport."""
import os
from pathlib import Path
import stat
import subprocess
import sys
import unittest


BINARY, ROOT = sys.argv[1], Path(sys.argv[2])


def invoke(path, fail=False):
    env = os.environ.copy()
    if fail:
        env["LOC_TEST_FDOPEN_FAIL"] = "1"
    return subprocess.run([BINARY, "--qrtr", "--seconds", "1", "--log", str(path)],
                          capture_output=True, text=True, env=env, umask=0, timeout=3)


class PrivateLog(unittest.TestCase):
    def test_delete_aiding_all_private_log_and_exclusivity(self):
        path = ROOT / "delete-aiding-all.log"
        result = subprocess.run([BINARY, "--qrtr", "--delete-aiding-all", "--log", str(path)],
                                capture_output=True, text=True, umask=0, timeout=3)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for extra in (["--query-only"], ["--receiver-info"], ["--acquisition-config"],
                      ["--seconds", "1"], ["--rf-metrics"],
                      ["--seconds", "1", "--rf-metrics"]):
            invalid = ROOT / "invalid-delete-aiding.log"
            result = subprocess.run([BINARY, "--qrtr", "--delete-aiding-all", *extra,
                                     "--log", str(invalid)], capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("QRTR_STUB", result.stderr)
            self.assertFalse(invalid.exists())
        result = subprocess.run([BINARY, "--delete-aiding-all", "--log", str(path)],
                                capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("QRTR_STUB", result.stderr)

    def test_rf_metrics_requires_bounded_session(self):
        path = ROOT / "rf-metrics.log"
        result = subprocess.run([BINARY, "--qrtr", "--rf-metrics", "--seconds", "1",
                                 "--log", str(path)], capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for extra in ([], ["--query-only"], ["--receiver-info"],
                      ["--seconds", "1", "--query-only"]):
            invalid = ROOT / "invalid-rf-metrics.log"
            result = subprocess.run([BINARY, "--qrtr", "--rf-metrics", *extra,
                                     "--log", str(invalid)], capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("QRTR_STUB", result.stderr)
            self.assertFalse(invalid.exists())

    def test_receiver_info_private_log_and_exclusivity(self):
        path = ROOT / "receiver-info.log"
        result = subprocess.run([BINARY, "--qrtr", "--receiver-info", "--log", str(path)],
                                capture_output=True, text=True, umask=0, timeout=3)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for extra in (["--query-only"], ["--seconds", "1"]):
            invalid = ROOT / "invalid-receiver-info.log"
            result = subprocess.run([BINARY, "--qrtr", "--receiver-info", *extra,
                                     "--log", str(invalid)], capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("QRTR_STUB", result.stderr)
            self.assertFalse(invalid.exists())

    def test_acquisition_config_private_log_and_exclusivity(self):
        path = ROOT / "acquisition-config.log"
        result = subprocess.run([BINARY, "--qrtr", "--acquisition-config", "--log", str(path)],
                                capture_output=True, text=True, umask=0, timeout=3)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for extra in (["--query-only"], ["--receiver-info"], ["--seconds", "1"],
                      ["--rf-metrics"], ["--seconds", "1", "--rf-metrics"]):
            invalid = ROOT / "invalid-acquisition-config.log"
            result = subprocess.run([BINARY, "--qrtr", "--acquisition-config", *extra,
                                     "--log", str(invalid)], capture_output=True, text=True, timeout=3)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn("QRTR_STUB", result.stderr)
            self.assertFalse(invalid.exists())
        result = subprocess.run([BINARY, "--acquisition-config", "--log", str(path)],
                                capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("QRTR_STUB", result.stderr)

    def test_query_only_private_log(self):
        path = ROOT / "query.log"
        result = subprocess.run([BINARY, "--qrtr", "--query-only", "--log", str(path)],
                                capture_output=True, text=True, umask=0, timeout=3)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_query_and_session_are_exclusive(self):
        path = ROOT / "invalid-query.log"
        result = subprocess.run([BINARY, "--qrtr", "--query-only", "--seconds", "1", "--log", str(path)],
                                capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("QRTR_STUB", result.stderr)
        self.assertFalse(path.exists())

    def test_private_mode_with_permissive_umask(self):
        path = ROOT / "private.log"
        result = invoke(path)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST QRTR_STUB no-socket", result.stderr)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_no_overwrite_or_symlink_follow(self):
        path = ROOT / "preserve.log"
        path.write_text("preserve precise data")
        link = ROOT / "linked.log"
        link.symlink_to(path)
        dangling = ROOT / "dangling.log"
        target = ROOT / "must-not-create.log"
        dangling.symlink_to(target)
        for candidate in (path, link, dangling):
            result = invoke(candidate)
            self.assertEqual(result.returncode, 1)
            self.assertNotIn("QRTR_STUB", result.stderr)
        self.assertEqual(path.read_text(), "preserve precise data")
        self.assertFalse(target.exists())

    def test_fdopen_failure_closes_descriptor(self):
        result = invoke(ROOT / "fdopen-failure.log", fail=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("TEST fdopen-failure descriptor-closed", result.stderr)
        self.assertNotIn("QRTR_STUB", result.stderr)


unittest.main(argv=[sys.argv[0]], verbosity=2)
