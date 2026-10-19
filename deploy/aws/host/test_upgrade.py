"""Planner, state machine (with a fake host) and staging (with a real git repository).

Run: python3 -m unittest discover -s deploy/aws/host
"""
import contextlib
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ow_host as h  # noqa: E402
import upgrade as u  # noqa: E402
from ow_host import Refused  # noqa: E402

KERNEL = "ea0e55d03dbaebc79a58644308e0517b7a33f1530a84848d9edf47ffa61f69c8"
RUNTIME = "firecracker-v1.17.0"
CPU = "model name: x\nflags: y\nhost-kernel:6.8.0-1"


def entry(v, changes, cls, in_place="0.5.0", protocol=(1, 1, 1), schema=(3, 3), kernel=KERNEL, worker=(1, 1, 1)):
    return {"version": v, "class": cls, "changes": changes,
            "protocol": {"speaks": protocol[0], "accepts": list(protocol[1:])},
            "worker_protocol": {"speaks": worker[0], "accepts": list(worker[1:])},
            "catalog": {"schema": schema[0], "min_reader": schema[1]},
            "runtime": {"vmm": RUNTIME, "guest_kernel_sha256": kernel, "resumes": [RUNTIME]},
            "host_layout": 2, "in_place_from": in_place}


BASE = entry("0.5.0", ["controller", "agent", "worker", "host-layout", "stage0"], 3, in_place=None)


def manifest(*entries):
    return {"schema": 1, "releases": [*reversed(entries), BASE]}


AT = {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.0"}
COMPAT = {"runtime": RUNTIME, "cpu": CPU, "kernel_sha256": KERNEL}
WARM = [{"name": "box-a", **COMPAT}]


class Planner(unittest.TestCase):
    def test_dashboard_only_release_touches_only_the_dashboard(self):
        plan = u.plan(manifest(entry("0.5.1", ["dashboard"], 1)), AT, "0.5.1")
        self.assertEqual((plan["class"], plan["move"], plan["restart"]), (1, {"dashboard": "0.5.1"}, ["dashboard"]))

    def test_class_is_the_highest_restart_and_skips_unchanged_units(self):
        plan = u.plan(manifest(entry("0.5.1", ["controller"], 1), entry("0.5.2", ["guest-agent", "component"], 0)),
                      AT, "0.5.2")
        self.assertEqual((plan["class"], sorted(plan["move"])), (1, ["agent"]))
        plan = u.plan(manifest(entry("0.5.1", ["component", "config"], 0)), AT, "0.5.1")
        self.assertEqual((plan["class"], plan["restart"]), (0, []))
        plan = u.plan(manifest(entry("0.5.1", ["worker"], 2)), AT, "0.5.1", captures=WARM, compat=COMPAT)
        self.assertEqual((plan["class"], plan["restart"], plan["cold"]), (2, ["worker"], []))

    def test_each_unit_counts_the_releases_it_has_not_run(self):
        # The dashboard already runs 0.5.1, so 0.5.1's dashboard change does not move it again.
        m = manifest(entry("0.5.1", ["dashboard"], 1), entry("0.5.2", ["agent"], 1))
        plan = u.plan(m, {**AT, "dashboard": "0.5.1"}, "0.5.2")
        self.assertEqual(plan["move"], {"agent": "0.5.2"})

    def test_placement_release_requires_backup_without_moving_the_worker(self):
        actual = json.loads((Path(__file__).resolve().parents[3] / "release-impact.json").read_text())
        pins = {name: "0.9.2" for name in AT}
        plan = u.plan(actual, pins, "0.9.3")
        self.assertEqual(plan["class"], 1)
        self.assertEqual(plan["move"], {"agent": "0.9.3", "dashboard": "0.9.3"})
        self.assertTrue(plan["catalog_backup"])
        self.assertFalse(plan["rollback_auto"])
        self.assertNotIn("worker", plan["restart"])

    def test_guest_kernel_only_release_is_refused_without_accept_cold(self):
        m = manifest(entry("0.5.1", ["guest-kernel"], 2, kernel="f" * 64))
        caps = [{"name": "box-a", **COMPAT}, {"name": "hib/sleep-1", **COMPAT}, {"name": "box-b/snap1", **COMPAT}]
        with self.assertRaisesRegex(Refused, "box-a, box-b/snap1, hib/sleep-1.*--accept-cold"):
            u.plan(m, AT, "0.5.1", captures=caps, compat=COMPAT)
        plan = u.plan(m, AT, "0.5.1", captures=caps, compat=COMPAT, accept_cold=True)
        self.assertEqual(plan["cold"], ["box-a", "box-b/snap1", "hib/sleep-1"])

    def test_another_cpu_or_runtime_is_cold_and_an_identical_tuple_is_warm(self):
        m = manifest(entry("0.5.1", ["worker"], 2))
        for bad in ({"cpu": "other"}, {"runtime": "firecracker-v1.16.0"}):
            with self.assertRaisesRegex(Refused, "box-a"):
                u.plan(m, AT, "0.5.1", captures=[{"name": "box-a", **COMPAT, **bad}], compat=COMPAT)
        self.assertEqual(u.plan(m, AT, "0.5.1", captures=WARM, compat=COMPAT)["cold"], [])

    def test_replacement_releases_and_old_sources_are_refused_in_place(self):
        with self.assertRaisesRegex(Refused, "host replacement"):
            u.plan(manifest(entry("0.5.1", ["stage0"], 3)), AT, "0.5.1")
        with self.assertRaisesRegex(Refused, "host replacement"):
            u.plan(manifest(entry("0.5.1", ["dashboard"], 1, in_place="0.5.1")), AT, "0.5.1")
        with self.assertRaisesRegex(Refused, "does not list"):
            u.plan(manifest(entry("0.5.1", ["dashboard"], 1)), {**AT, "worker": "0.4.9"}, "0.5.1")
        with self.assertRaisesRegex(Refused, "no entry"):
            u.plan(manifest(), AT, "0.6.0")

    def test_max_class(self):
        with self.assertRaisesRegex(Refused, "class 2.*exceeds --max-class 1"):
            u.plan(manifest(entry("0.5.1", ["worker"], 2)), AT, "0.5.1", captures=WARM, compat=COMPAT, max_class=1)

    def test_protocol_floor_needs_a_bridge_for_every_runner(self):
        # The controller speaks 2 and accepts 1..2; the target speaks 3 and accepts 2..3.
        m = {"schema": 1, "releases": [entry("0.7.0", ["controller"], 1, protocol=(3, 2, 3)),
                                       entry("0.6.0", ["controller"], 1, protocol=(2, 1, 2)), BASE]}
        at = {name: "0.6.0" for name in AT}
        runners = [{"id": "a", "protocol": 1}, {"id": "b", "protocol": 2}]
        with self.assertRaisesRegex(Refused, "upgrade runners a to 0.6.0 first"):
            u.plan(m, at, "0.7.0", runners=runners)
        self.assertEqual(u.plan(m, at, "0.7.0", runners=runners[1:])["class"], 1)
        m["releases"].remove(m["releases"][1])
        m["releases"][0]["in_place_from"] = "0.5.0"
        with self.assertRaisesRegex(Refused, "no release bridges"):
            u.plan(m, AT, "0.7.0", runners=runners)

    def test_catalog_change_moves_controller_and_dashboard_after_a_copy(self):
        m = manifest(entry("0.5.1", ["controller"], 1, schema=(4, 3)))
        plan = u.plan(m, AT, "0.5.1")
        self.assertEqual((plan["catalog_backup"], plan["rollback_auto"], sorted(plan["move"])),
                         (True, True, ["agent", "dashboard"]))
        self.assertFalse(u.plan(manifest(entry("0.5.1", ["controller"], 1, schema=(4, 4))), AT, "0.5.1")["rollback_auto"])

    def test_going_back_moves_the_units_whose_releases_are_undone(self):
        m = manifest(entry("0.5.1", ["controller"], 1), entry("0.5.2", ["dashboard"], 1), entry("0.5.3", ["worker"], 2))
        plan = u.plan(m, {"worker": "0.5.3", "agent": "0.5.1", "dashboard": "0.5.2"}, "0.5.0", captures=WARM, compat=COMPAT)
        self.assertEqual((plan["class"], sorted(plan["move"])), (2, ["agent", "dashboard", "worker"]))
        plan = u.plan(m, {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.2"}, "0.5.1")
        self.assertEqual(plan["move"], {"agent": "0.5.1", "dashboard": "0.5.1"})

    def test_agent_and_worker_move_together_when_their_protocols_part(self):
        # The new agent drives worker protocol 2 only: the old worker (1) must move with it.
        m = manifest(entry("0.5.1", ["agent"], 1, worker=(2, 2, 2)))
        plan = u.plan(m, AT, "0.5.1", captures=WARM, compat=COMPAT)
        self.assertEqual((plan["class"], sorted(plan["move"])), (2, ["agent", "worker"]))
        # A worker speaking 2 cannot run under the old agent (1..1): the agent moves too.
        m = manifest(entry("0.5.1", ["worker"], 2, worker=(2, 2, 2)))
        self.assertEqual(sorted(u.plan(m, AT, "0.5.1", captures=WARM, compat=COMPAT)["move"]), ["agent", "worker"])
        # Additive: an agent accepting 1..2 next to the old worker stays class 1.
        m = manifest(entry("0.5.1", ["agent"], 1, worker=(2, 1, 2)))
        self.assertEqual(u.plan(m, AT, "0.5.1")["move"], {"agent": "0.5.1"})
        with self.assertRaisesRegex(Refused, "worker protocol window"):
            u.plan(manifest(entry("0.5.1", ["worker"], 2, worker=(3, 1, 2))), AT, "0.5.1", captures=WARM, compat=COMPAT)

    def test_pending_config_and_rendered_units(self):
        plan = u.plan(manifest(), AT, "0.5.0", pending={"worker"})
        self.assertEqual((plan["class"], plan["move"], plan["restart"]), (2, {}, ["worker"]))
        plan = u.plan(manifest(entry("0.5.1", ["host-layout"], 0)), AT, "0.5.1", rendered={"agent"})
        self.assertEqual((plan["class"], plan["move"]), (1, {"agent": "0.5.1"}))


class Fake:
    """A host: three units, a worker with boxes, and failures to inject."""

    def __init__(self, boxes, fail=(), kill=(), present=("worker", "agent", "dashboard"), late=None):
        self.present = list(present)
        self.up = {name: True for name in present}
        self.box = dict(boxes)
        self.fail, self.kill = set(fail), set(kill)
        self.calls, self.drained, self.quiesced = [], False, False
        self.late = late  # a box someone starts right after the next inventory read

    def _hit(self, what):
        self.calls.append(what)
        if what in self.kill:
            raise KeyboardInterrupt(what)  # the upgrader dies here
        if what in self.fail:
            raise RuntimeError(f"injected failure at {what}")

    def units(self):
        return self.present

    def active(self, name):
        return self.up[name]

    def start(self, name):
        self._hit(f"start:{name}")
        self.up[name] = True
        if name == "worker":
            self.quiesced = False

    def stop(self, name):
        self._hit(f"stop:{name}")
        self.up[name] = False

    def worker_status(self):
        if not self.up["worker"]:
            return None
        return {"internet": True, "version": u.version_of(h.pin("worker")), "compat": COMPAT}

    def boxes(self):
        snapshot = dict(self.box)
        if self.late:
            # A start (or a wake on connect) racing the upgrade: refused once quiesced.
            if not self.quiesced:
                self.box[self.late] = "running"
            self.late = None
        return snapshot

    def quiesce(self):
        self.calls.append("quiesce")
        self.quiesced = True

    def captures(self):
        return [{"name": box, **COMPAT} for box, state in self.box.items() if state in ("running", "hibernated")]

    def hibernate_all(self, release):
        self.quiesced = True
        failed = []
        for box, state in self.box.items():
            if state != "running":
                continue
            if f"capture:{box}" in self.fail:
                failed.append(box)
                continue
            self.box[box] = "hibernated"
            self._hit(f"hibernated:{box}")
        if failed:
            self.quiesced = False
            raise RuntimeError(f"could not hibernate {', '.join(failed)}")
        self._hit("hibernate")
        self.up["worker"] = False

    def quiesce_off(self):
        self.quiesced = False

    def start_box(self, box):
        self._hit(f"resume:{box}")
        assert self.up["worker"] and not self.quiesced
        self.box[box] = "running"

    def drain(self, on):
        self.drained = on

    def backup_catalog(self, target):
        self._hit("catalog-backup")
        return "/srv/ow/.ow/backups/copy.sqlite3"

    def sync_images(self, release):
        self._hit("sync-images")

    def switch(self, to):
        self._hit("switch")
        for name, release in to.items():
            h.set_pin(name, release)

    def verify(self, expected):
        self._hit("verify")
        self.calls.append(("expected", dict(expected)))
        for name in self.present:
            assert self.up[name], name

    def canary(self):
        self.calls.append("canary")


class Machine(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name)
        for release, v in (("0.5.0-aaaaaaaaaaaa", "0.5.0"), ("0.5.1-bbbbbbbbbbbb", "0.5.1")):
            h.write_json(f"{h.SRV}/releases/{release}/STAGED", {"version": v, "commit": release[6:]})
        for name in (*h.PINS, "current"):
            h.set_pin(name, "0.5.0-aaaaaaaaaaaa")
        self.old, self.new = "0.5.0-aaaaaaaaaaaa", "0.5.1-bbbbbbbbbbbb"
        self.frm = h.pins()

    def tearDown(self):
        self.tmp.cleanup()

    def run_plan(self, ops, move, cls, restart, catalog=False, plan=None):
        to = {name: self.new if name in move else self.old for name in h.PINS}
        to["current"] = self.new
        plan = plan or {"class": cls, "move": dict.fromkeys(move, "0.5.1"), "restart": restart,
                        "catalog_backup": catalog, "rollback_auto": True}
        u.execute(plan, ops, self.frm, to, "0.5.1")

    def journal(self):
        return h.read_json(u.JOURNAL)

    def test_dashboard_only_upgrade_leaves_boxes_and_other_units_alone(self):
        ops = Fake({"a": "running"})
        self.run_plan(ops, ["dashboard"], 1, ["dashboard"])
        self.assertEqual(self.journal()["step"], "done")
        self.assertEqual(ops.box, {"a": "running"})
        self.assertNotIn("hibernate", ops.calls)
        self.assertEqual([c for c in ops.calls if isinstance(c, str) and c.startswith(("stop", "start"))],
                         ["stop:dashboard", "start:dashboard"])
        # Health expects each unit's own pin: worker and agent stay 0.5.0.
        self.assertIn(("expected", {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.1"}), ops.calls)
        self.assertEqual(h.pins(), {"worker": self.old, "agent": self.old, "dashboard": self.new, "current": self.new})

    def test_runner_upgrade_hibernates_swaps_and_resumes(self):
        ops = Fake({"a": "running", "b": "running", "c": "stopped"})
        self.run_plan(ops, ["worker"], 2, ["worker"])
        j = self.journal()
        self.assertEqual((j["step"], j["resume"]), ("done", ["a", "b"]))
        self.assertEqual(ops.box, {"a": "running", "b": "running", "c": "stopped"})
        self.assertFalse(ops.drained)
        self.assertNotIn("stop:agent", ops.calls, "the agent outlives a worker restart")
        self.assertEqual(h.pin("worker"), self.new)
        self.assertNotIn("canary", ops.calls)
        self.assertIn("seconds", j)

    def test_a_box_started_during_the_upgrade_is_not_left_hibernated(self):
        ops = Fake({"original": "running", "late": "stopped"}, late="late")
        self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(ops.box, {"original": "running", "late": "stopped"}, "the late start was refused, not captured")
        self.assertLess(ops.calls.index("quiesce"), ops.calls.index("hibernated:original"))
        self.assertEqual((self.journal()["step"], self.journal()["error"]), ("done", None))

    def test_a_box_started_after_the_last_plan_still_meets_the_capture_gate(self):
        # Planned with no box at all, for a release with another guest kernel.
        m = manifest(entry("0.5.1", ["guest-kernel"], 2, kernel="f" * 64))
        for accept_cold in (False, True):
            plan = u.plan(m, AT, "0.5.1", captures=[], compat=COMPAT, accept_cold=accept_cold)
            for name in (*h.PINS, "current"):
                h.set_pin(name, self.old)
            ops = Fake({"late": "running"})  # started between the last plan and the quiesce
            if not accept_cold:
                with self.assertRaisesRegex(Refused, "cannot resume late.*--accept-cold"):
                    self.run_plan(ops, ["worker"], 2, ["worker"], plan=plan)
                self.assertEqual((ops.box, self.journal()["step"], h.pin("worker")), ({"late": "running"}, "aborted", self.old))
                self.assertNotIn("hibernate", ops.calls)
                self.assertFalse(ops.quiesced or ops.drained)
            else:
                self.run_plan(ops, ["worker"], 2, ["worker"], plan=plan)
                self.assertEqual((self.journal()["step"], self.journal()["cold"]), ("done", ["late"]))

    def test_runner_upgrade_without_boxes_runs_a_canary(self):
        ops = Fake({})
        self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertIn("canary", ops.calls)

    def test_abort_after_a_partial_hibernate_resumes_the_boxes_already_hibernated(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"capture:b"})
        with self.assertRaisesRegex(RuntimeError, "could not hibernate b"):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(ops.box, {"a": "running", "b": "running"})
        self.assertEqual(self.journal()["step"], "aborted")
        self.assertFalse(ops.drained)
        self.assertEqual(h.pins(), self.frm, "no pin moved")

    def test_killed_after_one_box_hibernated_then_recovered(self):
        ops = Fake({"a": "running", "b": "running"}, kill={"hibernated:a"})
        with self.assertRaises(KeyboardInterrupt):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(self.journal()["step"], "hibernate")
        ops.kill.clear()
        u.recover(ops)
        self.assertEqual(ops.box, {"a": "running", "b": "running"})
        self.assertEqual(self.journal()["step"], "aborted")
        self.assertFalse(ops.quiesced or ops.drained)

    def test_a_failed_verify_rolls_back_and_resumes_on_the_old_worker(self):
        ops = Fake({"a": "running"}, fail={"verify"})
        real = ops.verify

        def verify_once(expected):  # the new release fails its checks; the old one passes
            if expected["worker"] == "0.5.0":
                ops.fail.discard("verify")
            real(expected)

        ops.verify = verify_once
        with self.assertRaisesRegex(RuntimeError, "injected failure at verify"):
            self.run_plan(ops, ["worker", "agent"], 2, ["agent", "worker"])
        self.assertEqual(self.journal()["step"], "rolled-back")
        self.assertEqual(h.pins(), self.frm)
        self.assertEqual(ops.box, {"a": "running"})
        self.assertTrue(all(ops.up.values()))

    def test_a_new_worker_that_does_not_start_is_rolled_back(self):
        ops = Fake({"a": "running"})
        starts = []

        def start(name):
            if name == "worker" and not starts:
                starts.append(name)
                raise RuntimeError("worker exited at start")
            Fake.start(ops, name)

        ops.start = start
        with self.assertRaisesRegex(RuntimeError, "exited at start"):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual((self.journal()["step"], h.pin("worker"), ops.box["a"]), ("rolled-back", self.old, "running"))

    def test_killed_at_start_rolls_back_on_the_next_run(self):
        ops = Fake({"a": "running"}, kill={"start:worker"})
        with self.assertRaises(KeyboardInterrupt):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual((self.journal()["step"], h.pin("worker")), ("start", self.new))
        ops.kill.clear()
        u.recover(ops)
        self.assertEqual((self.journal()["step"], h.pin("worker"), ops.box["a"]), ("rolled-back", self.old, "running"))

    def test_a_box_that_does_not_resume_is_reported_not_rolled_back(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"resume:b"})
        self.run_plan(ops, ["worker"], 2, ["worker"])
        j = self.journal()
        self.assertEqual(j["step"], "done")
        self.assertIn("b", j["error"])
        self.assertEqual((ops.box["a"], ops.box["b"], h.pin("worker")), ("running", "hibernated", self.new))

    def test_catalog_copy_comes_before_anything_stops(self):
        ops = Fake({})
        self.run_plan(ops, ["agent", "dashboard"], 1, ["agent", "dashboard"], catalog=True)
        self.assertLess(ops.calls.index("catalog-backup"), ops.calls.index("stop:dashboard"))
        self.assertTrue(self.journal()["catalog_backup"])

    def test_a_done_journal_needs_no_recovery(self):
        ops = Fake({"a": "running"})
        self.run_plan(ops, ["dashboard"], 1, ["dashboard"])
        ops.calls.clear()
        u.recover(ops)
        self.assertEqual(ops.calls, [])


class KernelStaging(unittest.TestCase):
    """A release whose guest kernel pin changed gets that kernel in its own asset set,
    though the copied set already holds a base image and the old kernel."""

    REPO = Path(__file__).resolve().parents[3]

    def test_setup_fetches_the_pinned_downloads_before_the_base_image_check(self):
        setup = (self.REPO / "scripts/setup-local.sh").read_text()
        fetch = setup.index("prepare-guest.py --downloads-only")
        self.assertLess(fetch, setup.index("if [[ ! -f data/runtime-spike/guest/base.ext4 ]]"))

    @unittest.skipUnless(sys.version_info >= (3, 11), "prepare-guest.py needs Python 3.11+ (hashlib.file_digest)")
    def test_a_stale_kernel_is_fetched_again_and_never_kept_on_a_bad_download(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            downloads = tmp / "data/downloads"
            downloads.mkdir(parents=True)
            for name in ("alpine-minirootfs-3.24.2-x86_64.tar.gz", "vmlinux-6.1.186"):
                (downloads / name).write_text("an older pin")
            (tmp / "bin").mkdir()
            # Stand-in curl: logs its URL, writes bytes that match no pin.
            (tmp / "bin/curl").write_text(f'#!/bin/sh\necho "$4" >> {tmp}/curl.log\necho wrong > "$6"\n')
            (tmp / "bin/curl").chmod(0o755)
            run = subprocess.run([sys.executable, str(self.REPO / "experiments/runtime-spike/prepare-guest.py"),
                                  "--downloads-only", "--data-dir", str(tmp / "data")],
                                 env={**os.environ, "PATH": f"{tmp}/bin:{os.environ['PATH']}"},
                                 capture_output=True, text=True)
            self.assertIn("Artifact hash mismatch", run.stderr + run.stdout)
            self.assertTrue((tmp / "curl.log").read_text().strip(), "the stale file is fetched again")
            self.assertEqual((downloads / "vmlinux-6.1.186").read_text(), "an older pin", "a bad download replaces nothing")
            self.assertEqual(sorted(p.name for p in downloads.iterdir()), ["alpine-minirootfs-3.24.2-x86_64.tar.gz", "vmlinux-6.1.186"])


class Quiesce(unittest.TestCase):
    """HostOps speaks the worker's control protocol: one JSON line each way."""

    def test_quiesce_and_lift_over_the_control_socket(self):
        import socket
        import threading
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            h.ROOT = Path(tmp)
            path = h.p(f"{h.RUNNER}/control.sock")
            path.parent.mkdir(parents=True)
            server = socket.socket(socket.AF_UNIX)
            server.bind(str(path))
            server.listen()
            seen = []

            def serve(answers):
                for ok in answers:
                    conn, _ = server.accept()
                    seen.append(json.loads(conn.makefile().readline()))
                    conn.sendall(json.dumps({"ok": ok, "result": {}}).encode() + b"\n")
                    conn.close()

            thread = threading.Thread(target=serve, args=([True, True, False],))
            thread.start()
            ops = u.HostOps()
            ops.quiesce()
            ops.quiesce_off()
            with self.assertRaisesRegex(RuntimeError, "did not accept"):
                ops.quiesce()
            thread.join()
            server.close()
            self.assertEqual(seen, [{"op": "quiesce", "closing": True}, {"op": "quiesce", "closing": False},
                                    {"op": "quiesce", "closing": True}])


class Staging(unittest.TestCase):
    """A release builds into its own directory; the live asset set and every running
    unit's binary stay byte-identical."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name) / "root"
        self.repo = Path(self.tmp.name) / "repo"
        git = lambda *a: subprocess.run(["git", "-C", str(self.repo), *a], check=True, capture_output=True)  # noqa: E731
        self.repo.mkdir()
        git("init", "-q")
        git("config", "user.email", "t@example.test")
        git("config", "user.name", "t")
        (self.repo / "Cargo.toml").write_text('[workspace]\n\n[workspace.package]\nversion = "0.5.1"\n')
        (self.repo / "release-impact.json").write_text(json.dumps(manifest(entry("0.5.1", ["worker"], 2))))
        git("add", ".")
        git("commit", "-q", "-m", "release")
        git("tag", "v0.5.1")
        live = h.p(f"{h.SRV}/releases/0.5.0-aaaaaaaaaaaa")
        (live / "assets/guest").mkdir(parents=True)
        (live / "assets/guest/ow-guest").write_text("old guest agent")
        (live / "bin").mkdir()
        (live / "bin/ow").write_text("old ow")
        h.write_json(f"{h.SRV}/releases/0.5.0-aaaaaaaaaaaa/STAGED", {"version": "0.5.0"})
        for name in (*h.PINS, "current"):
            h.set_pin(name, "0.5.0-aaaaaaaaaaaa")
        # Stand-in for setup-local.sh: rewrites the asset set it is given and builds ow.
        os.environ["OW_HOST_BUILD"] = ("sh -c 'echo new guest agent > data/runtime-spike/guest/ow-guest; "
                                       "mkdir -p target/release; printf \"#!/bin/sh\\necho ow 0.5.1\\n\" > target/release/ow; "
                                       "chmod +x target/release/ow'")

    def tearDown(self):
        os.environ.pop("OW_HOST_BUILD", None)
        self.tmp.cleanup()

    def test_fetch_and_stage_never_touch_live_files(self):
        config = {"release": {"repository": str(self.repo), "ref": "v0.5.1"}}
        target = u.fetch(config)
        self.assertEqual((target["version"], target["manifest"]["releases"][0]["version"]), ("0.5.1", "0.5.1"))
        live = h.p(f"{h.SRV}/run/worker")
        before = {f: f.read_bytes() for f in (live / "assets/guest/ow-guest", live / "bin/ow")}
        name = u.stage(target)
        self.assertEqual({f: f.read_bytes() for f in before}, before)
        rel = h.p(f"{h.SRV}/releases/{name}")
        self.assertEqual((rel / "assets/guest/ow-guest").read_text(), "new guest agent\n")
        self.assertEqual(h.staged(name)["version"], "0.5.1")
        self.assertEqual(u.stage(target), name, "a complete release is reused")
        self.assertEqual(h.pins()["worker"], "0.5.0-aaaaaaaaaaaa", "staging moves no pin")

    def test_controller_stage_uses_only_native_build_and_keeps_live_assets(self):
        script = self.repo / "scripts/setup-local.sh"
        script.parent.mkdir()
        script.write_text('set -eu\n[ "$1" = --controller-only ]\nmkdir -p target/release\nprintf "#!/bin/sh\\necho ow 0.5.1\\n" >target/release/ow\nchmod +x target/release/ow\n')
        subprocess.run(["git", "-C", str(self.repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(self.repo), "commit", "-qm", "native-only build"], check=True)
        config = {"role": "controller", "release": {"repository": str(self.repo), "ref": "HEAD"}}
        h.write_json(h.CONFIG, config)
        os.environ.pop("OW_HOST_BUILD")
        target = u.fetch(config)
        live = h.p(f"{h.SRV}/run/agent/assets/guest/ow-guest")
        before = live.read_bytes()
        name = u.stage(target)
        self.assertEqual(live.read_bytes(), before)
        self.assertEqual(h.staged(name)["version"], "0.5.1")
        self.assertEqual(h.pin("agent"), "0.5.0-aaaaaaaaaaaa")

    def test_a_build_reporting_another_version_is_refused(self):
        os.environ["OW_HOST_BUILD"] = os.environ["OW_HOST_BUILD"].replace("ow 0.5.1", "ow 0.5.0")
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        with self.assertRaisesRegex(Refused, "reports ow 0.5.0"):
            u.stage(target)
        self.assertIsNone(h.staged(target["release"]))

    def published(self, target, sums=None, source=None, drop=()):
        """A fake GitHub release of the tagged commit: its assets, SHA256SUMS and source commit."""
        assets = {"ow-linux-amd64": b"#!/bin/sh\necho ow 0.5.1\n", "ow-guest-linux-amd64": b"released guest agent",
                  "source-commit.txt": ((source or target["commit"]) + "\n").encode()}
        listed = {name: hashlib.sha256(data).hexdigest() for name, data in assets.items()}
        listed.update(sums or {})
        assets["SHA256SUMS"] = "".join(f"{v}  {k}\n" for k, v in listed.items() if k not in drop).encode()
        for name in drop:
            assets.pop(name)
        base = "https://github.example/releases/download/v0.5.1"
        seen = []

        def download(url):
            seen.append(url.rsplit("/", 1)[1])
            return assets.get(url.removeprefix(base + "/"))

        self.seen = seen
        return [mock.patch.object(u, "release_url", lambda repository, tag: tag == "v0.5.1" and base),
                mock.patch.object(u, "download", download),
                mock.patch.object(u.os, "uname", return_value=SimpleNamespace(machine="x86_64"))]

    def stage(self, target, patches, from_source=False):
        out = io.StringIO()
        with contextlib.ExitStack() as stack, contextlib.redirect_stdout(out):
            for patch in patches:
                stack.enter_context(patch)
            try:
                return u.stage(target, from_source)
            finally:
                self.log = out.getvalue()

    def test_released_binaries_are_verified_and_installed_without_a_build(self):
        # A stand-in for setup-local.sh that installs the guest agent it is given and builds nothing.
        os.environ["OW_HOST_BUILD"] = 'cp "$OW_PREBUILT_GUEST" data/runtime-spike/guest/ow-guest'
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        name = self.stage(target, self.published(target))
        rel = h.p(f"{h.SRV}/releases/{name}")
        self.assertEqual((rel / "bin/ow").read_bytes(), b"#!/bin/sh\necho ow 0.5.1\n")
        self.assertEqual((rel / "assets/guest/ow-guest").read_bytes(), b"released guest agent")
        self.assertEqual((rel / "assets/bin/ow-linux-amd64").read_bytes(), (rel / "bin/ow").read_bytes())
        self.assertFalse((h.p(f"{h.SRV}/build/target/release/ow")).exists(), "nothing was compiled")
        self.assertEqual(h.staged(name)["source"], "release")
        self.assertIn("from the released binaries of v0.5.1", self.log)
        self.assertEqual(self.seen, ["SHA256SUMS", "source-commit.txt", "ow-linux-amd64", "ow-guest-linux-amd64"])
        self.assertFalse(h.p(f"{h.SRV}/build/prebuilt").exists())

    def test_a_controller_installs_only_the_released_ow(self):
        h.write_json(h.CONFIG, {"role": "controller"})
        os.environ["OW_HOST_BUILD"] = "false"  # a controller runs no setup script at all
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        name = self.stage(target, self.published(target, drop=("ow-guest-linux-amd64",)))
        self.assertEqual(h.staged(name)["source"], "release")
        self.assertNotIn("ow-guest-linux-amd64", self.seen)

    def test_a_checksum_mismatch_is_refused_and_installs_nothing(self):
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        with self.assertRaisesRegex(Refused, "ow-guest-linux-amd64 does not match its SHA256SUMS"):
            self.stage(target, self.published(target, sums={"ow-guest-linux-amd64": "0" * 64}))
        self.assertFalse(h.p(f"{h.SRV}/releases/{target['release']}").exists())
        self.assertIsNone(h.staged(target["release"]))

    def test_a_commit_mismatch_is_refused_and_installs_nothing(self):
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        with self.assertRaisesRegex(Refused, "was built from '1111"):
            self.stage(target, self.published(target, source="1" * 40))
        self.assertFalse(h.p(f"{h.SRV}/releases/{target['release']}").exists())

    def test_a_missing_asset_falls_back_to_the_source_build(self):
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        name = self.stage(target, self.published(target, drop=("ow-guest-linux-amd64",)))
        self.assertEqual((h.p(f"{h.SRV}/releases/{name}/assets/guest/ow-guest")).read_text(), "new guest agent\n")
        self.assertEqual(h.staged(name)["source"], "build")
        self.assertIn("from source: release v0.5.1 publishes no ow-guest-linux-amd64", self.log)

    def test_an_untagged_commit_or_from_source_builds(self):
        subprocess.run(["git", "-C", str(self.repo), "commit", "-q", "--allow-empty", "-m", "unreleased"], check=True)
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "HEAD"}})
        self.assertEqual(h.staged(self.stage(target, self.published(target)))["source"], "build")
        self.assertIn("is not the release tag v0.5.1", self.log)
        self.assertEqual(self.seen, [], "no download for a commit that is not the tag")
        target = u.fetch({"release": {"repository": str(self.repo), "ref": "v0.5.1"}})
        self.assertEqual(h.staged(self.stage(target, self.published(target), from_source=True))["source"], "build")
        self.assertIn("from source: --from-source", self.log)
        self.assertEqual(self.seen, [])

    def test_release_urls_are_github_tags_only(self):
        self.assertEqual(u.release_url("https://github.com/o/r.git", "v1.2.3"),
                         "https://github.com/o/r/releases/download/v1.2.3")
        self.assertIsNone(u.release_url("/srv/mirror", "v1.2.3"))
        self.assertIsNone(u.release_url("https://example.com/o/r.git", "v1.2.3"))


if __name__ == "__main__":
    unittest.main()


class DrainAuthorities(unittest.TestCase):
    def test_upgrade_clear_preserves_the_retirement_seal(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seal = root / "drain-seal.json"
            seal.write_text('{"seal":"fixture-retirement"}')
            with patch.object(u, "p", side_effect=lambda _: root / "drain"), patch.object(h, "role", return_value="runner"):
                ops = u.HostOps()
                ops.drain(True)
                self.assertTrue((root / "drain").exists())
                ops.drain(False)
            self.assertFalse((root / "drain").exists())
            self.assertEqual(json.loads(seal.read_text()), {"seal": "fixture-retirement"})
