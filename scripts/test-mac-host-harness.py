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
            guest.stop()
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

    def test_excessive_startup_output_cleans_up(self):
        helper = self.helper("import os, time\nfor _ in range(64): os.write(1, b'a' * 65536)\ntime.sleep(30)\n")
        with patch.object(harness.subprocess, "Popen", side_effect=self.spawn):
            with self.assertRaises(RuntimeError) as error:
                harness.Guest(helper, self.root)
        self.assertIn("output limit exceeded", str(error.exception.__cause__))
        self.assert_clean()

if __name__ == "__main__": unittest.main()
