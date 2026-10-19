"""Controller-only packaging never probes or operates a local worker."""
import contextlib
import copy
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import ow_host as h
import upgrade as u
from test_ow_host import CONFIG, BUDGET
from test_upgrade import AT, entry, manifest


def controller(**changes):
    c = {**copy.deepcopy(CONFIG), "role": "controller", "developerImages": []}
    c.update(changes)
    return h.validate(c)


class Controller(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = mock.patch.object(h, "ROOT", Path(self.tmp.name))
        self.root.start()
        self.addCleanup(self.root.stop)
        h.write_json(h.CONFIG, controller())

    def test_no_compute_units_images_or_phantom_budget(self):
        c = controller()
        files = h.render(c, BUDGET)
        self.assertEqual(h.budgets(c), {"memory": 0, "cpus": 0, "slots": 0})
        text = files[f"/etc/systemd/system/{h.UNITS['agent']}"][0]
        self.assertIn("host up --controller-only", text)
        for absent in ("worker", "node-agent", "--memory", "--slots", "--cpus"):
            self.assertNotIn(absent, text)
        for path in files:
            self.assertNotIn("worker", path)
            self.assertNotIn("images", path)
            self.assertNotIn("recovery", path)
        self.assertIn("OW_BACKUP_SNAPSHOTS=0\n", files["/etc/open-workspaces/backup.env"][0])
        self.assertIn("OW_ASSET_DIR=/srv/ow/run/agent/assets", files["/usr/local/bin/ow"][0])
        self.assertEqual(h.role_pins(c), ("agent", "dashboard"))
        self.assertIsNone(h.local_node())
        for changes in ({"developerImages": ["ubuntu"]}, {"backup": {"url": "s3://bucket/prefix", "recovery": True}},
                        {"store": {"url": "s3://bucket/prefix", "images": {"ubuntu": {"version": 1, "attempt": "a" * 32, "digest": "b" * 64}}}}):
            with self.assertRaisesRegex(h.Refused, "no local images or recovery worker"):
                controller(**changes)

    def test_setup_and_boot_install_and_start_only_control_units(self):
        calls = []
        def run(cmd, **kw):
            calls.append(cmd)
            return ""
        with mock.patch.object(h, "run", side_effect=run), mock.patch.object(h, "chown"), \
             mock.patch.object(u, "fetch", return_value={}), mock.patch.object(u, "stage", return_value="0.5.1-abc"), \
             mock.patch.object(u, "migrate_layout_v1"), mock.patch.object(u, "recover"), mock.patch.object(u, "prune"), \
             mock.patch.object(h, "tailscale", return_value=("joined", False)), mock.patch.object(h, "ready"):
            h.boot()
        self.assertIsNone(h.pin("worker"))
        self.assertEqual(h.pin("agent"), "0.5.1-abc")
        self.assertTrue(h.unit_exists(h.UNITS["agent"]))
        self.assertTrue(h.unit_exists(h.UNITS["dashboard"]))
        self.assertFalse(h.unit_exists(h.UNITS["worker"]))
        self.assertFalse(h.p(h.RUNNER).exists())
        for cmd in calls:
            self.assertNotIn(h.UNITS["worker"], cmd)
        # Reject conversion before deleting any installed worker or its recovery data.
        h.write_atomic(f"/etc/systemd/system/{h.UNITS['worker']}", "original worker\n")
        with mock.patch.object(h, "run") as command, self.assertRaisesRegex(h.Refused, "fresh host"):
            h.setup(controller())
        command.assert_not_called()
        self.assertTrue(h.unit_exists(h.UNITS["worker"]))

    def test_apply_refuses_in_place_role_change_before_writes(self):
        old = h.validate(CONFIG)
        h.write_json(h.CONFIG, old)
        with mock.patch.object(h, "fetch_config", return_value=controller()), \
             mock.patch.object(h, "setup") as setup, self.assertRaisesRegex(h.Refused, "role changes"):
            h.apply_config()
        setup.assert_not_called()
        self.assertEqual(h.read_json(h.CONFIG), old)

    def test_readiness_without_worker_or_enrolled_nodes(self):
        def ow(*args, **kw):
            self.assertEqual(args, ("node", "list"))
            return []
        with mock.patch.object(h, "ow", side_effect=ow), mock.patch.object(h, "active", return_value=True), \
             mock.patch.object(h, "tailnet", return_value="100.64.0.1"), \
             mock.patch.object(h, "listening", return_value="100.64.0.1:8790"), \
             mock.patch.object(h, "bootstrap_done"), mock.patch.object(h, "unit_exists", return_value=True), \
             mock.patch.object(h, "dashboard_answers", return_value=True), mock.patch.object(h, "tailscale_status", return_value={}):
            h.ready(wait=1)
        with mock.patch.object(h, "active", return_value=True), mock.patch.object(h, "ow", return_value=[]), \
             mock.patch.object(h, "tailnet", return_value="none"), mock.patch.object(h, "listening", return_value=""), \
             self.assertRaisesRegex(RuntimeError, "controller not ready"):
            h.controller_ready(0, 0)

    def test_status_has_zero_local_capacity_without_proc_files(self):
        output = io.StringIO()
        with mock.patch.object(h, "run", return_value="inactive"), contextlib.redirect_stdout(output):
            h.status()
        value = json.loads(output.getvalue())
        self.assertEqual(value["budget"], {"memory": 0, "cpus": 0, "slots": 0})
        self.assertIsNone(value["node"])

    def test_health_checks_only_installed_control_units_and_the_listener(self):
        for unit in h.UNITS.values():
            h.write_atomic(f"/etc/systemd/system/{unit}", "unit")
        ops = u.HostOps()
        self.assertEqual(ops.units(), ["agent", "dashboard"])
        h.set_pin("agent", "0.5.0-old")
        h.set_pin("dashboard", "0.5.0-old")
        executable = str(h.p(f"{h.SRV}/releases/0.5.0-old/bin/ow"))
        with (
            mock.patch.object(ops, "active", return_value=True),
            mock.patch.object(h, "run", return_value="123"),
            mock.patch.object(u.os.path, "realpath", return_value=executable),
            mock.patch.object(ops, "worker_status", side_effect=AssertionError("worker health probe")),
            mock.patch.object(h, "tailnet", return_value="100.64.0.1"),
            mock.patch.object(h, "listening", return_value="100.64.0.1:8790"),
            mock.patch.object(h, "dashboard_answers", return_value=True),
        ):
            self.assertEqual(ops._check({"agent": "0.5.0", "dashboard": "0.5.0"}), "")
            with mock.patch.object(h, "listening", return_value="127.0.0.1:8790"):
                self.assertIn("not 100.64.0.1:8790", ops._check({}))

    def test_plan_never_adds_absent_units_even_for_incompatible_worker_release(self):
        m = manifest(entry("0.5.1", ["worker", "agent", "dashboard"], 2, worker=(99, 99, 99), schema=(4, 4)))
        for pins in ({"agent": AT["agent"]}, {k: v for k, v in AT.items() if k != "worker"}):
            plan = u.plan(m, pins, "0.5.1", pending=["worker"], rendered=["worker"])
            self.assertEqual(plan["class"], 1)
            self.assertEqual(set(plan["restart"]), set(pins))
            self.assertEqual(set(plan["move"]), set(pins))
            self.assertNotIn("gate", plan)
            self.assertTrue(plan["catalog_backup"])

    def test_controller_upgrade_and_rollback_do_not_call_worker(self):
        c = controller(dashboard={"logins": []})
        h.write_json(h.CONFIG, c)
        for name in h.role_pins(c):
            h.set_pin(name, "0.5.0-old")
        h.set_pin("current", "0.5.0-old")
        h.write_atomic(f"/etc/systemd/system/{h.UNITS['agent']}", "controller unit")
        target = {"version": "0.5.1", "release": "0.5.1-new", "manifest": manifest(entry("0.5.1", ["agent", "worker"], 2))}
        ops = u.HostOps()
        for fail in (False, True):
            calls = []
            with mock.patch.object(u, "fetch", return_value=target), mock.patch.object(u, "stage", return_value=target["release"]), \
                 mock.patch.object(u, "render_diff", return_value=set()), mock.patch.object(u, "host_cpu", side_effect=AssertionError("cpu probe")), \
                 mock.patch.object(u, "captures", side_effect=AssertionError("capture probe")), mock.patch.object(u, "prune"), \
                 mock.patch.object(h, "ow", side_effect=lambda *a, **k: [] if a == ("node", "list") else self.fail(str(a))), \
                 mock.patch.object(ops, "worker_status", side_effect=AssertionError("worker probe")), \
                 mock.patch.object(ops, "active", return_value=True), \
                 mock.patch.object(ops, "start", side_effect=lambda n: calls.append(("start", n))), \
                 mock.patch.object(ops, "stop", side_effect=lambda n: calls.append(("stop", n))), \
                 mock.patch.object(ops, "switch", side_effect=lambda pins: calls.append(("switch", pins))), \
                 mock.patch.object(ops, "verify", side_effect=[RuntimeError("fixture health failure"), None] if fail else None):
                if fail:
                    with self.assertRaisesRegex(RuntimeError, "fixture health failure"):
                        u.upgrade(ops=ops)
                else:
                    self.assertEqual(u.upgrade(ops=ops), 0)
            self.assertTrue(calls)
            self.assertNotIn("worker", json.dumps(calls))
            self.assertFalse(h.p(h.RUNNER).exists())
        with self.assertRaisesRegex(h.Refused, "no worker"):
            u.restart_worker()

    def test_backup_runs_only_the_role_targets(self):
        script = (h.HERE / "files/ow-backup").read_text()
        for role, flags, catalog, snapshots in (
            ("controller", "OW_BACKUP_SNAPSHOTS=0\n", True, False),
            ("host", "", True, True),
            ("runner", "OW_BACKUP_CATALOG=0\n", False, True),
        ):
            root = Path(self.tmp.name) / role
            root.mkdir()
            config = root / "backup.env"
            config.write_text("OW_BACKUP_URL=s3://bucket/prefix\n" + flags)
            calls = root / "calls"
            body = script.replace(". /etc/open-workspaces/backup.env", f'. "{config}"')
            body = body.replace("/run/ow-backup.lock", str(root / "lock")).replace("/srv/ow/.ow/backups", str(root / "backups"))
            standins = r'''
flock() { :; }
install() { mkdir -p "${@: -1}"; }
ow() { echo "ow $*" >> "$OW_TEST_CALLS"; if [[ $1 == backup && $2 == catalog ]]; then echo fixture > "$4"; fi; }
runuser() { echo upload >> "$OW_TEST_CALLS"; }
'''
            subprocess.run(["bash", "-euo", "pipefail", "-c", standins + body],
                           env={**os.environ, "OW_TEST_CALLS": str(calls)}, check=True, capture_output=True)
            out = calls.read_text()
            self.assertEqual("ow backup catalog" in out, catalog, role)
            self.assertEqual("upload" in out, catalog, role)
            self.assertEqual("ow backup snapshots" in out, snapshots, role)
            self.assertFalse(any((root / "backups").glob("*.sqlite3")), "private temporary copy cleaned")


if __name__ == "__main__":
    unittest.main()
