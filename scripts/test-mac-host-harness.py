#!/usr/bin/env python3
"""Harness regressions using owned real subprocesses; no VM required."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("mac_host_test", Path(__file__).with_name("test-mac-host.py"))
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)

class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.children = []
        self.real_popen = subprocess.Popen
        self.stack = []
        for name, value in [("READY_TIMEOUT", 3), ("STOP_TIMEOUT", 0.05),
                            ("TERMINATE_TIMEOUT", 0.1), ("KILL_TIMEOUT", 1)]:
            context = patch.object(harness.Guest, name, value)
            context.start(); self.stack.append(context)

    def tearDown(self):
        for context in reversed(self.stack): context.stop()
        for child in self.children:
            if child.poll() is None: child.kill(); child.wait(timeout=2)
            for stream in (child.stdin, child.stdout):
                if stream: stream.close()
        self.temporary.cleanup()

    def spawn(self, *args, **kwargs):
        child = self.real_popen(*args, **kwargs)
        self.children.append(child)
        return child

    def helper(self, code):
        path = self.root / "helper"
        path.write_text("#!" + sys.executable + "\n" + code)
        path.chmod(0o700)
        return path

    def assert_clean(self):
        self.assertEqual(len(self.children), 1)
        child = self.children[0]
        self.assertIsNotNone(child.poll(), "owned child still running")
        self.assertTrue(child.stdin.closed)
        self.assertTrue(child.stdout.closed)

    def test_closed_stdin_stop_escalates_to_kill(self):
        helper = self.helper("import os, signal, time\nos.close(0)\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nprint('OW_READY arch=aarch64', flush=True)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            shutdown = guest.stop()
            self.assertEqual(shutdown["mode"], "sigkill")
            with self.assertRaises(AssertionError): harness.require_guest_poweroff(shutdown)
            guest.stop()  # Cleanup stays safe after constructor/read failure.
        self.assert_clean()
        self.assertLess(self.children[0].returncode, 0)

    def test_full_stdin_does_not_block_cleanup(self):
        helper = self.helper("import time\nprint('OW_READY arch=aarch64', flush=True)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            import os
            fd = guest.proc.stdin.fileno()
            os.set_blocking(fd, False)
            with self.assertRaises(BlockingIOError):
                for _ in range(1024): os.write(fd, b'x' * 65536)
            guest.stop()
        self.assert_clean()

    def test_failed_readiness_with_closed_stdin_confirms_exit(self):
        helper = self.helper("import os, time\nos.close(0)\nprint('not ready', flush=True)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            with self.assertRaises(RuntimeError) as error:
                harness.Guest(helper, self.root)
        self.assertIsInstance(error.exception.__cause__, TimeoutError)
        self.assert_clean()

    def test_excessive_output_bounds_buffers_and_cleans_up(self):
        helper = self.helper("import os, time\nprint('OW_READY arch=aarch64', flush=True)\nwhile os.read(0, 1) != b'x': pass\nfor _ in range(64): os.write(1, b'a' * 65536)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            guest.proc.stdin.write(b'x')
            with self.assertRaisesRegex(RuntimeError, "output limit exceeded"):
                guest.read_until(rb"NEVER_EMITTED", timeout=2)
        self.assertLessEqual(len(guest.pending), guest.MATCH_LIMIT)
        self.assertLessEqual(len(guest.log), guest.LOG_LIMIT)
        self.assert_clean()

    def test_confirmed_guest_poweroff_requires_marker_and_exit0(self):
        helper = self.helper("import sys\nprint('OW_READY arch=aarch64', flush=True)\nsys.stdin.readline()\nprint('ow-vz: guest stopped', flush=True)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            harness.require_guest_poweroff(guest.stop())
        self.assert_clean()

    def test_exit0_without_guest_marker_is_not_poweroff(self):
        helper = self.helper("import sys\nprint('OW_READY arch=aarch64', flush=True)\nsys.stdin.readline()\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            shutdown = guest.stop()
            self.assertEqual(shutdown["returncode"], 0)
            with self.assertRaises(AssertionError): harness.require_guest_poweroff(shutdown)
        self.assert_clean()

    def test_guest_marker_with_crash_is_not_poweroff(self):
        helper = self.helper("import sys\nprint('OW_READY arch=aarch64', flush=True)\nsys.stdin.readline()\nprint('ow-vz: guest stopped', flush=True)\nsys.exit(1)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            shutdown = guest.stop()
            self.assertTrue(shutdown["guest_stop_marker"])
            with self.assertRaises(AssertionError): harness.require_guest_poweroff(shutdown)
        self.assert_clean()

    def test_sigterm_cleanup_is_not_guest_poweroff(self):
        helper = self.helper("import os, time\nos.close(0)\nprint('OW_READY arch=aarch64', flush=True)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            guest = harness.Guest(helper, self.root)
            shutdown = guest.stop()
            self.assertEqual(shutdown["mode"], "sigterm")
            with self.assertRaises(AssertionError): harness.require_guest_poweroff(shutdown)
        self.assert_clean()

    def test_excessive_startup_output_cleans_up(self):
        helper = self.helper("import os, time\nfor _ in range(64): os.write(1, b'a' * 65536)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            with self.assertRaises(RuntimeError) as error:
                harness.Guest(helper, self.root)
        self.assertIn("output limit exceeded", str(error.exception.__cause__))
        self.assert_clean()

class RestoreEvidenceTests(unittest.TestCase):
    def test_exact_separate_values_pass(self):
        harness.verify_restored_state("\nOW_RAM=captured\r\nOW_DISK=captured\r\n", 0)

    def test_restored_ram_with_parent_disk_fails(self):
        with self.assertRaises(AssertionError):
            harness.verify_restored_state("\nOW_RAM=captured\nOW_DISK=parent\n", 0)

    def test_ram_marker_cannot_replace_disk_probe(self):
        with self.assertRaises(AssertionError):
            harness.verify_restored_state("RAM=captured\nparent\n", 0)

    def test_substring_and_duplicate_probes_fail(self):
        for output in ["OW_RAM=captured\nOW_DISK=captured-extra\n",
                       "OW_RAM=captured\nOW_DISK=parent\nOW_DISK=captured\n"]:
            with self.subTest(output=output), self.assertRaises(AssertionError):
                harness.verify_restored_state(output, 0)

if __name__ == "__main__": unittest.main()
