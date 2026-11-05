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
import time
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


def entry(v, changes, cls, in_place="0.5.0", protocol=(1, 1, 1), schema=(3, 3), kernel=KERNEL, worker=(1, 1, 1),
          handover=None, vmm=RUNTIME):
    e = {"version": v, "class": cls, "changes": changes,
         "protocol": {"speaks": protocol[0], "accepts": list(protocol[1:])},
         "worker_protocol": {"speaks": worker[0], "accepts": list(worker[1:])},
         "catalog": {"schema": schema[0], "min_reader": schema[1]},
         "runtime": {"vmm": vmm, "guest_kernel_sha256": kernel, "resumes": [vmm]},
         "host_layout": 2, "in_place_from": in_place}
    if handover:
        e["runtime"]["handover"] = {"speaks": handover[0], "accepts": list(handover[1:])}
    return e


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
        self.assertEqual((plan["class"], sorted(plan["move"])), (1, ["agent", "dashboard"]))
        plan = u.plan(manifest(entry("0.5.1", ["component", "config"], 0)), AT, "0.5.1")
        self.assertEqual((plan["class"], plan["restart"]), (0, []))
        plan = u.plan(manifest(entry("0.5.1", ["worker"], 2)), AT, "0.5.1", captures=WARM, compat=COMPAT)
        self.assertEqual((plan["class"], plan["restart"], plan["cold"]), (2, ["worker"], []))

    def test_each_unit_counts_the_releases_it_has_not_run(self):
        # The worker already runs 0.5.1, so 0.5.1's worker change does not move it again.
        m = manifest(entry("0.5.1", ["worker"], 2), entry("0.5.2", ["agent"], 1))
        plan = u.plan(m, {**AT, "worker": "0.5.1"}, "0.5.2")
        self.assertEqual(plan["move"], {"agent": "0.5.2", "dashboard": "0.5.2"})

    def test_the_dashboard_moves_with_the_agent(self):
        # It serves the admin API (controller settings) and /api/version, which clients follow;
        # the agent's restart already drops its streams, so it adds no disconnect.
        plan = u.plan(manifest(entry("0.5.1", ["controller"], 1)), {"agent": "0.5.0", "dashboard": "0.5.0"}, "0.5.1")
        self.assertEqual(plan["move"], {"agent": "0.5.1", "dashboard": "0.5.1"})
        plan = u.plan(manifest(entry("0.5.1", ["component"], 0)), AT, "0.5.1")
        self.assertEqual((plan["move"], plan["restart"]), ({}, []))
        # A worker-only release keeps the dashboard's streams.
        plan = u.plan(manifest(entry("0.5.1", ["worker"], 2)), AT, "0.5.1", captures=WARM, compat=COMPAT)
        self.assertEqual(plan["move"], {"worker": "0.5.1"})
        plan = u.plan(manifest(entry("0.5.1", ["agent"], 1)), {"worker": "0.5.0", "agent": "0.5.0"}, "0.5.1")
        self.assertEqual(plan["move"], {"agent": "0.5.1"})

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
        self.assertEqual((plan["class"], sorted(plan["move"])), (2, ["agent", "dashboard", "worker"]))
        # A worker speaking 2 cannot run under the old agent (1..1): the agent moves too.
        m = manifest(entry("0.5.1", ["worker"], 2, worker=(2, 2, 2)))
        self.assertEqual(sorted(u.plan(m, AT, "0.5.1", captures=WARM, compat=COMPAT)["move"]), ["agent", "dashboard", "worker"])
        # Additive: an agent accepting 1..2 next to the old worker stays class 1.
        m = manifest(entry("0.5.1", ["agent"], 1, worker=(2, 1, 2)))
        self.assertEqual(u.plan(m, AT, "0.5.1")["move"], {"agent": "0.5.1", "dashboard": "0.5.1"})
        with self.assertRaisesRegex(Refused, "worker protocol window"):
            u.plan(manifest(entry("0.5.1", ["worker"], 2, worker=(3, 1, 2))), AT, "0.5.1", captures=WARM, compat=COMPAT)

    def test_pending_config_and_rendered_units(self):
        plan = u.plan(manifest(), AT, "0.5.0", pending={"worker"})
        self.assertEqual((plan["class"], plan["move"], plan["restart"]), (2, {}, ["worker"]))
        plan = u.plan(manifest(entry("0.5.1", ["host-layout"], 0)), AT, "0.5.1", rendered={"agent"})
        self.assertEqual((plan["class"], plan["move"]), (1, {"agent": "0.5.1", "dashboard": "0.5.1"}))


class HandOver(unittest.TestCase):
    """The worker restarts by handing its running VMMs over only when nothing they run changes."""

    def path(self, *entries, at="0.5.1", to=None, **kw):
        m = manifest(*entries)
        plan = u.plan(m, {**AT, "worker": at}, to or m["releases"][0]["version"], captures=WARM, compat=COMPAT, **kw)
        return plan["worker_path"], plan["worker_path_why"]

    def test_a_worker_only_release_hands_over(self):
        self.assertEqual(self.path(entry("0.5.1", ["worker"], 2, handover=(1, 1, 1)),
                                   entry("0.5.2", ["worker"], 2, handover=(1, 1, 1)))[0], "handover")

    def test_a_vmm_or_guest_kernel_change_hibernates(self):
        old = entry("0.5.1", ["worker"], 2, handover=(1, 1, 1))
        path, why = self.path(old, entry("0.5.2", ["vmm", "worker"], 2, handover=(1, 1, 1), vmm="firecracker-v9"),
                              accept_cold=True)
        self.assertEqual((path, why), ("hibernate", "vmm change"))
        path, why = self.path(old, entry("0.5.2", ["guest-kernel"], 2, handover=(1, 1, 1), kernel="f" * 64),
                              accept_cold=True)
        self.assertEqual((path, why), ("hibernate", "guest-kernel change"))
        # A release in between that changed the VMM counts too, even if the target changed it back.
        path, _ = self.path(old, entry("0.5.2", ["vmm"], 2, handover=(1, 1, 1)),
                            entry("0.5.3", ["worker"], 2, handover=(1, 1, 1)))
        self.assertEqual(path, "hibernate")

    def test_a_worker_that_cannot_detach_or_adopt_hibernates(self):
        # The release that brings hand-over: the running worker cannot detach yet.
        path, why = self.path(entry("0.5.1", ["worker"], 2), entry("0.5.2", ["worker"], 2, handover=(1, 1, 1)))
        self.assertEqual((path, why), ("hibernate", "the 0.5.1 worker cannot hand over its VMMs"))
        path, why = self.path(entry("0.5.1", ["worker"], 2, handover=(1, 1, 1)),
                              entry("0.5.2", ["worker"], 2, handover=(2, 2, 2)))
        self.assertEqual(path, "hibernate")
        self.assertIn("cannot adopt", why)

    def test_the_switch_the_flag_and_a_unit_change_hibernate(self):
        entries = (entry("0.5.1", ["worker"], 2, handover=(1, 1, 1)), entry("0.5.2", ["worker"], 2, handover=(1, 1, 1)))
        self.assertEqual(self.path(*entries, handover=False)[0], "hibernate")
        self.assertEqual(self.path(*entries, rendered={"worker"}),
                         ("hibernate", "the worker's unit files or config change"))
        # `ow-host apply` already wrote them and left the restart pending.
        self.assertEqual(self.path(*entries, pending={"worker"})[0], "hibernate")

    def test_a_restart_without_a_worker_restart_has_no_path(self):
        plan = u.plan(manifest(entry("0.5.1", ["dashboard"], 1)), AT, "0.5.1")
        self.assertNotIn("worker_path", plan)

    def test_the_release_that_brings_hand_over_still_hibernates_and_the_next_hands_over(self):
        actual = json.loads((Path(__file__).resolve().parents[3] / "release-impact.json").read_text())
        e = next(e for e in actual["releases"] if e["version"] == "0.9.59")
        compat = {"runtime": e["runtime"]["vmm"], "cpu": CPU, "kernel_sha256": e["runtime"]["guest_kernel_sha256"]}
        pins = dict.fromkeys(AT, "0.9.58")
        plan = u.plan(actual, pins, "0.9.59", captures=[{"name": "a", **compat}], compat=compat)
        self.assertEqual((plan["worker_path"], plan["worker_path_why"]),
                         ("hibernate", "the 0.9.58 worker cannot hand over its VMMs"))
        later = {**json.loads(json.dumps(e)), "version": "0.9.99", "changes": ["worker"]}
        actual["releases"].insert(0, later)
        plan = u.plan(actual, dict.fromkeys(AT, "0.9.59"), "0.9.99", captures=[{"name": "a", **compat}], compat=compat)
        self.assertEqual(plan["worker_path"], "handover")

    def test_the_setting_comes_from_the_agent_copy_and_defaults_on(self):
        with tempfile.TemporaryDirectory() as tmp:
            h.ROOT = Path(tmp)
            self.assertTrue(u.handover_enabled())
            self.assertEqual(u.running_boxes_setting(), "hibernate")
            h.write_json(f"{h.RUNNER}/controller-settings.json",
                         {"upgrade.worker_handover": False, "upgrade.running_boxes": "stop"})
            self.assertFalse(u.handover_enabled())
            self.assertEqual(u.running_boxes_setting(), "stop")

    def test_running_boxes_stop_replaces_hibernation_and_hand_over_still_wins(self):
        entries = (entry("0.5.1", ["worker"], 2, handover=(1, 1, 1)), entry("0.5.2", ["worker"], 2, handover=(1, 1, 1)))
        self.assertEqual(self.path(*entries, running_boxes="stop")[0], "handover")
        path, why = self.path(*entries, running_boxes="stop", rendered={"worker"})
        self.assertEqual((path, why), ("stop", "the worker's unit files or config change"))
        m = manifest(*entries)
        plan = u.plan(m, {**AT, "worker": "0.5.1"}, "0.5.2", captures=WARM, compat=COMPAT, running_boxes="stop")
        self.assertEqual(plan["running_boxes"], "stop", "the path a refused hand-over falls back to")


class HandOverBlocker(unittest.TestCase):
    def test_the_bytes_of_firecracker_and_the_kernel_and_the_worker_capability_decide(self):
        with tempfile.TemporaryDirectory() as tmp:
            h.ROOT = Path(tmp)
            for release in ("old", "new"):
                assets = h.p(f"{h.SRV}/releases/{release}/assets")
                (assets / "official/release-v1").mkdir(parents=True)
                (assets / "downloads").mkdir()
                (assets / "official/release-v1/firecracker-v1").write_bytes(b"vmm")
                (assets / "downloads/vmlinux-6").write_bytes(b"kernel")
            ops = u.HostOps()
            with mock.patch.object(u.HostOps, "worker_status", return_value={"worker_handover_v1": True}):
                self.assertIsNone(ops.handover_blocker("old", "new"))
                h.p(f"{h.SRV}/releases/new/assets/downloads/vmlinux-6").write_bytes(b"another kernel")
                self.assertIn("guest kernel differs", ops.handover_blocker("old", "new"))
            with mock.patch.object(u.HostOps, "worker_status", return_value={"worker_handover_v1": False}):
                self.assertIn("cannot hand over on this host", ops.handover_blocker("old", "old"))

    def test_no_firecracker_or_kernel_found_on_either_side_is_not_a_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            h.ROOT = Path(tmp)
            ops = u.HostOps()
            with mock.patch.object(u.HostOps, "worker_status", return_value={"worker_handover_v1": True}):
                self.assertIn("not found", ops.handover_blocker("old", "new"))


class Fake:
    """A host: three units, a worker with boxes, and failures to inject."""

    def __init__(self, boxes, fail=(), kill=(), present=("worker", "agent", "dashboard"), late=None,
                 timers=("ow-recovery.timer",)):
        self.present = list(present)
        self.ticking = set(timers)  # the timers systemd runs
        self.up = {name: True for name in present}
        self.box = dict(boxes)
        self.fail, self.kill = set(fail), set(kill)
        self.calls, self.drained, self.quiesced = [], False, False
        self.late = late  # a box someone starts right after the next inventory read
        self.pids = {box: 100 + n for n, box in enumerate(sorted(boxes))}  # each box's VMM
        self.handed, self.report = [], []
        self.managed = set()  # boxes only their controller may start: never stopped by the upgrader
        self.silent = 0  # status polls a worker finishing its shutdown leaves unanswered, then exits
        self.lists_failing = 0  # `ow list` calls that meet a worker just exiting

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
            # The new worker adopts what was handed over; one it cannot is hibernated.
            self.report = []
            for box in self.handed:
                if f"gone:{box}" in self.fail:
                    # Its VMM died while no worker ran: stopped with its disk.
                    self.box[box] = "stopped"
                    self.pids.pop(box)
                    self.report.append({"id": box, "result": "gone", "detail": None})
                elif f"adopt:{box}" in self.fail:
                    # Adopted, but its guest does not answer: it keeps running, reported.
                    self.report.append({"id": box, "result": "unresponsive", "detail": "guest did not answer"})
                else:
                    self.report.append({"id": box, "result": "adopted", "detail": None})
            self.handed = []

    def stop(self, name):
        self._hit(f"stop:{name}")
        if name == "worker" and self.up[name]:
            # ExecStop: whatever still runs on the worker is hibernated.
            for box, state in self.box.items():
                if state == "running":
                    self.box[box] = "hibernated"
                    self.pids.pop(box, None)
        self.up[name] = False

    blocker = None

    def handover_blocker(self, frm, to):
        return self.blocker

    def hibernate_unresponsive(self):
        out = {}
        for entry in self.report:
            box = entry["id"]
            if entry["result"] != "unresponsive":
                continue
            if f"hibernate-box:{box}" in self.fail:
                out[box] = "injected capture failure"
                continue
            self._hit(f"hibernate-box:{box}")
            self.box[box] = "hibernated"
            self.pids.pop(box, None)
            out[box] = "hibernated"
        return out

    def handover(self, release):
        self._hit("handover")
        self.handed = [box for box, state in self.box.items() if state == "running"]
        self.up["worker"] = False

    def vmm_pids(self, boxes):
        return {box: self.pids[box] for box in boxes if box in self.pids}

    def timers(self):
        return [timer for timer in u.TIMERS if timer in self.ticking]

    def pause_timers(self, timers):
        self.calls.append("pause-timers")
        self.ticking -= set(timers)

    def resume_timers(self, timers):
        self.calls.append("resume-timers")
        self.ticking |= set(timers)

    def worker_status(self):
        if self.silent and self.up["worker"]:
            self.silent -= 1
            if not self.silent:
                self.up["worker"] = False
            return None
        if not self.up["worker"]:
            return None
        return {"internet": True, "version": u.version_of(h.pin("worker")), "compat": COMPAT,
                "worker_handover_v1": True, "adoption": self.report}

    def boxes(self):
        if self.lists_failing:
            self.lists_failing -= 1
            self.up["worker"] = False
            raise RuntimeError("ow: No such file or directory (os error 2)")
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

    def stop_boxes(self, boxes):
        stopped = []
        for box in boxes:
            if self.box.get(box) == "running" and box not in self.managed:
                self._hit(f"stop-box:{box}")
                self.box[box] = "stopped"
                self.pids.pop(box, None)
                stopped.append(box)
        return stopped

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
        if "lift" in self.fail:
            return False
        self.quiesced = False
        return True

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
        assert not self.quiesced, "the canary needs an open worker"
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

    def handover_plan(self):
        return {"class": 2, "move": {"worker": "0.5.1"}, "restart": ["worker"], "catalog_backup": False,
                "rollback_auto": True, "worker_path": "handover"}

    def test_a_handover_keeps_every_box_running_with_its_vmm(self):
        ops = Fake({"a": "running", "b": "running", "c": "stopped"})
        pids = dict(ops.pids)
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual((j["step"], j["error"]), ("done", None))
        self.assertEqual(j["adoption"], {"a": "kept", "b": "kept"})
        self.assertIn("handover", j["seconds"])
        self.assertNotIn("hibernate", ops.calls)
        self.assertFalse([c for c in ops.calls if isinstance(c, str) and c.startswith(("resume:", "hibernated:"))])
        self.assertEqual((ops.box, ops.pids), ({"a": "running", "b": "running", "c": "stopped"}, pids))
        self.assertEqual(h.pin("worker"), self.new)
        self.assertNotIn("canary", ops.calls)
        events = {e["attrs"]["step"]: e["attrs"] for e in u.events({**j, "reported": []})[:-1]}
        self.assertEqual(events["handover"]["boxes"], 2)
        self.assertIn("adoption", events)

    def test_an_unresponsive_ephemeral_box_is_handed_to_the_worker_never_resumed(self):
        # Ephemeral boxes are left out of the upgrader's box list; the worker adopted one unresponsive.
        ops = Fake({"a": "running"})
        real = ops.start

        def start(name):
            real(name)
            if name == "worker":
                ops.report.append({"id": "job", "result": "unresponsive", "detail": "guest did not answer"})

        ops.start = start
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual((j["step"], j["adoption"], j["error"]), ("done", {"a": "kept"}, None))
        self.assertIn("hibernate-box:job", ops.calls, "the worker is asked (it stops an ephemeral box)")
        self.assertNotIn("resume:job", ops.calls)

    def test_with_only_an_ephemeral_box_the_adoption_still_runs(self):
        ops = Fake({})
        real = ops.start

        def start(name):
            real(name)
            if name == "worker":
                ops.report.append({"id": "job", "result": "unresponsive", "detail": "guest did not answer"})

        ops.start = start
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual((j["step"], j["handover"], j["adoption"], j["error"]), ("done", {}, {}, None))
        self.assertIn("hibernate-box:job", ops.calls, "the worker is asked to stop it")

    def test_the_report_is_read_from_a_worker_that_is_not_busy(self):
        ops = Fake({"a": "running"})
        real, answers = ops.worker_status, []

        def worker_status():
            status = real()
            if not answers and "verify" in ops.calls:
                # Still busy right after verify: its last answer predates the adoption.
                answers.append(1)
                return {**status, "busy": True, "adoption": []}
            return status

        ops.worker_status = worker_status
        with mock.patch.object(u.time, "sleep"):
            self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        self.assertEqual(answers, [1])
        self.assertEqual((self.journal()["adoption"], self.journal()["error"]), ({"a": "kept"}, None))

    def test_a_box_the_new_worker_cannot_adopt_is_hibernated_and_woken_not_lost(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"adopt:b"})
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual(j["step"], "done")
        self.assertEqual(j["adoption"]["a"], "kept")
        self.assertEqual(j["adoption"]["b"], "unresponsive: guest did not answer; hibernated")
        self.assertIn("not kept running through the hand-over: b", j["error"])
        # The upgrader saved it after verify (the new worker started without waiting on it).
        self.assertLess(ops.calls.index("verify"), ops.calls.index("hibernate-box:b"))
        self.assertIn("resume:b", ops.calls)
        self.assertEqual(ops.box, {"a": "running", "b": "running"})

    def test_a_box_whose_vmm_was_lost_boots_again_cold(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"gone:b"})
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual(j["adoption"], {"a": "kept", "b": "gone: no report"})
        self.assertIn("resume:b", ops.calls)
        self.assertEqual(ops.box, {"a": "running", "b": "running"})

    def test_killed_while_reporting_the_adoption_finishes_never_rolls_back(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"adopt:b"}, kill={"hibernate-box:b"})
        with self.assertRaises(KeyboardInterrupt):
            self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        self.assertEqual(self.journal()["step"], "adoption")
        ops.kill.clear()
        u.recover(ops)
        self.assertEqual((self.journal()["step"], h.pin("worker")), ("done", self.new))
        self.assertEqual(ops.box["a"], "running")

    def test_an_unresponsive_box_that_cannot_be_hibernated_keeps_running_reported(self):
        ops = Fake({"a": "running"}, fail={"adopt:a", "hibernate-box:a"})
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual(j["step"], "done")
        self.assertIn("hibernating failed", j["adoption"]["a"])
        self.assertEqual(ops.box["a"], "running")

    def test_the_bytes_decide_when_the_label_hides_a_runtime_change(self):
        ops = Fake({"a": "running", "b": "running"})
        ops.blocker = "the staged Firecracker binary or guest kernel differs from the running one"
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        j = self.journal()
        self.assertEqual((j["step"], j["handover_refused"]), ("done", ops.blocker))
        self.assertIn("hibernate", ops.calls)
        self.assertNotIn("handover", ops.calls)
        self.assertEqual(ops.box, {"a": "running", "b": "running"})

    def test_a_failed_detach_aborts_and_the_old_worker_keeps_its_boxes(self):
        ops = Fake({"a": "running"}, fail={"handover"})
        with self.assertRaisesRegex(RuntimeError, "injected failure at handover"):
            self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        self.assertEqual((self.journal()["step"], h.pin("worker"), ops.box["a"]), ("aborted", self.old, "running"))
        self.assertFalse(ops.quiesced or ops.drained)

    def test_killed_after_the_handover_the_old_worker_adopts_on_recover(self):
        ops = Fake({"a": "running"}, kill={"stop:agent"})
        pid = ops.pids["a"]
        with self.assertRaises(KeyboardInterrupt):
            self.run_plan(ops, ["worker", "agent"], 2, ["agent", "worker"],
                          plan={**self.handover_plan(), "move": {"worker": "0.5.1", "agent": "0.5.1"},
                                "restart": ["agent", "worker"]})
        self.assertEqual(self.journal()["step"], "stop")
        ops.kill.clear()
        u.recover(ops)
        self.assertEqual((self.journal()["step"], ops.box["a"], ops.pids["a"]), ("aborted", "running", pid))

    def test_a_failed_verify_after_a_handover_rolls_back_through_hibernation(self):
        ops = Fake({"a": "running"}, fail={"verify"})
        real = ops.verify

        def verify_once(expected):
            if expected["worker"] == "0.5.0":
                ops.fail.discard("verify")
            real(expected)

        ops.verify = verify_once
        with self.assertRaisesRegex(RuntimeError, "injected failure at verify"):
            self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.handover_plan())
        self.assertEqual((self.journal()["step"], h.pin("worker")), ("rolled-back", self.old))
        # The new worker hibernated it on stop; the old one woke it.
        self.assertIn("resume:a", ops.calls)
        self.assertEqual(ops.box["a"], "running")

    def test_a_worker_restart_hands_over_when_the_worker_can(self):
        ops = Fake({"a": "running"})
        with mock.patch.object(h, "role", return_value="runner"):
            u.restart_worker(ops)
            self.assertEqual(self.journal()["adoption"], {"a": "kept"})
            self.assertIn("handover", ops.calls)
            ops = Fake({"a": "running"})
            u.restart_worker(ops, hibernate=True)
            self.assertNotIn("handover", ops.calls)
            self.assertIn("hibernate", ops.calls)

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

    def test_disk_only_recovery_points_never_meet_the_capture_gate(self):
        for name, manifest in (("sleep-recovery-a", {"disk_only": True, "cpu": "none: disk-only recovery point"}),
                               ("sleep-b", {"cpu": COMPAT["cpu"]})):
            h.write_json(f"{h.RUNNER}/snapshots/{name}/manifest.json",
                         {"workspace": "m-1", "name": name, "runtime": COMPAT["runtime"],
                          "kernel_sha256": COMPAT["kernel_sha256"], **manifest})
        self.assertEqual([c["name"] for c in u.captures(COMPAT)], ["m-1/sleep-b"])

    def test_a_closed_journal_becomes_upgrade_events_on_the_intake(self):
        import socket
        import threading
        ops = Fake({"a": "running", "b": "stopped"})
        with tempfile.TemporaryDirectory(dir="/tmp") as short:
            path = f"{short}/telemetry.sock"
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            listener.listen(64)
            lines = []

            def accept():
                while True:
                    try:
                        conn, _ = listener.accept()
                    except OSError:
                        return
                    with conn:
                        lines.append(json.loads(conn.makefile().readline()))
            threading.Thread(target=accept, daemon=True).start()
            with mock.patch.object(u, "p", lambda name: Path(path) if name.endswith("telemetry.sock") else h.p(name)):
                self.run_plan(ops, ["worker"], 2, ["worker"])
            for _ in range(100):
                if lines and lines[-1]["event"] == "ow.host.upgrade":
                    break
                time.sleep(0.05)
            listener.close()
        steps = {e["attrs"]["step"]: e["attrs"] for e in lines if e["event"] == "ow.host.upgrade.step"}
        self.assertTrue({"drain", "hibernate", "switch", "verify", "resume", "undrain"} <= set(steps), steps)
        self.assertEqual((steps["hibernate"]["boxes"], steps["hibernate"]["cold"]), (1, 0))
        whole = lines[-1]["attrs"]
        self.assertEqual((whole["outcome"], whole["class"], whole["journal"], whole["resumed"]),
                         ("done", 2, self.journal()["id"], 1))
        # Steps went out live while the intake listened; the close sends the last and the whole.
        self.assertEqual(set(self.journal()["reported"]), set(steps) - {"undrain"})
        self.assertEqual(u.events(self.journal()), lines[-2:])

    def test_a_failed_hibernation_counts_only_the_boxes_that_went_to_sleep(self):
        ops = Fake({"a": "running", "b": "running"})
        real = ops.hibernate_all

        def first_only(release):  # a hibernated, then b overruns its budget
            ops.box["a"] = "hibernated"
            raise RuntimeError("could not hibernate b within its 200 s budget")
        ops.hibernate_all = first_only
        with self.assertRaises(RuntimeError):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        ops.hibernate_all = real
        j = self.journal()
        self.assertEqual((j["step"], j["hibernated"], j["resumed"]), ("aborted", 1, 1))
        events = {e["attrs"].get("step", "whole"): e["attrs"] for e in u.events({**j, "reported": []})}
        self.assertEqual((events["hibernate"]["boxes"], events["whole"]["outcome"]), (1, "aborted"))

    def test_every_event_attribute_is_one_the_intake_accepts(self):
        import re
        rust = (Path(__file__).resolve().parents[3] / "crates/ow/src/telemetry.rs").read_text()
        table = rust[rust.index("const HOST_EVENTS"):rust.index("];", rust.index("const HOST_EVENTS"))]
        accepted = {name: set(re.findall(r'"([a-z_]+)"', keys))
                    for name, keys in re.findall(r'"(ow\.host\.[a-z.]+)",\s*&\[([^\]]*)\]', table)}
        journal = {"id": "u-1", "class": 2, "from": {"current": "a"}, "to": {"current": "b"}, "step": "done",
                   "seconds": {"drain": 1.0, "hibernate": 2.0}, "hibernated": 1, "cold": [], "resumed": 1}
        for event in u.events(journal):
            self.assertLessEqual(set(event["attrs"]), accepted[event["event"]], event)
            self.assertLessEqual(len(event["attrs"]), 7, "with event.name, at most 8 attributes")

    def test_without_a_listener_the_upgrade_still_closes(self):
        ops = Fake({"a": "running"})
        self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(self.journal()["step"], "done")

    def test_an_ephemeral_box_never_meets_the_capture_gate(self):
        h.write_json(f"{h.RUNNER}/state.json", {"workspaces": {
            "m-job": {"id": "m-job", "state": "running", "ephemeral": True},
            "m-dev": {"id": "m-dev", "state": "running"}}})
        self.assertEqual([c["name"] for c in u.captures(COMPAT)], ["m-dev"])

    def test_the_hibernate_cap_grows_with_each_running_box(self):
        gib = 1 << 30
        self.assertEqual(u.hibernate_budget([]), 600)
        self.assertEqual(u.hibernate_budget([
            {"state": "running", "memory_mib": 16384, "disk_allocated_bytes": 256 * gib},
            {"state": "running", "memory_mib": 4096, "disk_allocated_bytes": None},
            {"state": "hibernated", "memory_mib": 65536}]), 600 + (120 + 320 + 1024) + (120 + 80))

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

    def test_the_new_worker_takes_no_wake_until_it_is_verified(self):
        ops = Fake({"a": "running", "b": "stopped"})
        real, seen = ops.verify, []

        def verify(expected):  # a connection tries to wake b while the new worker is checked
            seen.append(ops.quiesced)
            real(expected)

        ops.verify = verify
        self.run_plan(ops, ["worker", "agent"], 2, ["agent", "worker"])
        self.assertEqual(seen, [True], "wakes are refused during verify")
        self.assertLess(ops.calls.index("start:worker"), len(ops.calls) - ops.calls[::-1].index("quiesce") - 1)
        self.assertLess(len(ops.calls) - ops.calls[::-1].index("quiesce") - 1, ops.calls.index("start:agent"))
        self.assertEqual((ops.box, ops.quiesced, self.journal()["step"]), ({"a": "running", "b": "stopped"}, False, "done"))

    def test_a_lift_that_fails_leaves_the_journal_for_recover(self):
        ops = Fake({"a": "running"}, fail={"lift"})
        with self.assertRaisesRegex(RuntimeError, "did not lift"):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual((self.journal()["step"], ops.box["a"]), ("resume", "hibernated"))
        ops.fail.clear()
        u.recover(ops)
        self.assertEqual((self.journal()["step"], ops.box["a"], ops.quiesced), ("done", "running", False))

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
            seen.append(ops.quiesced)
            if expected["worker"] == "0.5.0":
                ops.fail.discard("verify")
            real(expected)

        seen = []
        ops.verify = verify_once
        with self.assertRaisesRegex(RuntimeError, "injected failure at verify"):
            self.run_plan(ops, ["worker", "agent"], 2, ["agent", "worker"])
        self.assertEqual(self.journal()["step"], "rolled-back")
        self.assertEqual(seen, [True, True], "both workers take no wake while verified")
        self.assertFalse(ops.quiesced)
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

    def test_timers_stay_stopped_while_the_journal_is_open(self):
        ops = Fake({"a": "running"}, timers=("ow-recovery.timer",))
        during = []
        real = ops.hibernate_all
        ops.hibernate_all = lambda release: (during.append(set(ops.ticking)), real(release))
        self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(during, [set()], "no recovery run meets the worker restart")
        self.assertLess(ops.calls.index("pause-timers"), ops.calls.index("hibernated:a"))
        self.assertEqual((self.journal()["paused"], ops.ticking), (["ow-recovery.timer"], {"ow-recovery.timer"}),
                         "exactly the timers that ran start again; the stopped backup timer stays stopped")

    def test_abort_and_rollback_start_the_paused_timers_again(self):
        for fail, end in (({"capture:a"}, "aborted"), ({"verify"}, "rolled-back")):
            for name in (*h.PINS, "current"):
                h.set_pin(name, self.old)
            ops = Fake({"a": "running"}, fail=fail, timers=u.TIMERS)
            if fail == {"verify"}:
                real = ops.verify
                ops.verify = lambda expected: (expected["worker"] == "0.5.0" and ops.fail.clear(), real(expected))
            with self.assertRaises(RuntimeError):
                self.run_plan(ops, ["worker"], 2, ["worker"])
            self.assertEqual((self.journal()["step"], ops.ticking), (end, set(u.TIMERS)))

    def test_a_reboot_mid_upgrade_pauses_the_timers_again_until_recovered(self):
        ops = Fake({"a": "running"}, kill={"start:worker"}, timers=("ow-recovery.timer",))
        with self.assertRaises(KeyboardInterrupt):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual(ops.ticking, set())
        ops.kill.clear()
        ops.ticking = set(u.TIMERS)  # the reboot: systemd starts every enabled timer
        during = []
        real = ops.verify
        ops.verify = lambda expected: (during.append(set(ops.ticking)), real(expected))
        u.recover(ops)
        self.assertEqual(during, [set()], "the rollback's worker meets no recovery run")
        self.assertEqual((self.journal()["step"], ops.ticking), ("rolled-back", set(u.TIMERS)),
                         "recover pauses and restarts the timers it found running too")

    def test_a_journal_from_an_older_upgrader_pauses_the_running_timers_while_recovered(self):
        ops = Fake({"a": "hibernated"}, timers=("ow-recovery.timer",))
        h.write_json(u.JOURNAL, {"step": "hibernate", "from": self.frm, "to": self.frm, "class": 2,
                                 "restart": ["worker"], "resume": ["a"], "error": None})
        ops.up["worker"] = False
        during = []
        real = ops.start
        ops.start = lambda name: (during.append(set(ops.ticking)), real(name))
        u.recover(ops)
        self.assertEqual(during, [set()])
        self.assertEqual((self.journal()["step"], ops.box["a"], ops.ticking),
                         ("aborted", "running", {"ow-recovery.timer"}))

    def stop_plan(self):
        return {"class": 2, "move": {"worker": "0.5.1"}, "restart": ["worker"], "catalog_backup": False,
                "rollback_auto": True, "worker_path": "stop", "running_boxes": "stop"}

    def test_running_boxes_stop_stops_them_and_starts_them_after_verify(self):
        ops = Fake({"a": "running", "b": "running", "c": "stopped", "m": "running"})
        ops.managed = {"m"}
        self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.stop_plan())
        j = self.journal()
        self.assertEqual((j["step"], j["error"], j["stopped"], j["hibernated"], j["resumed"]), ("done", None, 2, 1, 3))
        self.assertEqual(ops.box, {"a": "running", "b": "running", "c": "stopped", "m": "running"})
        self.assertNotIn("hibernated:a", ops.calls)
        self.assertIn("hibernated:m", ops.calls, "a managed box is hibernated: only its controller may start it")
        self.assertLess(ops.calls.index("stop-box:b"), ops.calls.index("hibernate"))
        self.assertLess(ops.calls.index("verify"), ops.calls.index("resume:a"))
        self.assertNotIn("resume:c", ops.calls, "a box stopped before the upgrade stays stopped")
        events = {e["attrs"]["step"]: e["attrs"] for e in u.events({**j, "reported": []}) if "step" in e["attrs"]}
        self.assertEqual((events["stop-boxes"]["boxes"], events["hibernate"]["boxes"]), (2, 1))

    def test_a_refused_hand_over_falls_back_to_stopping_the_boxes(self):
        ops = Fake({"a": "running"})
        ops.blocker = "the staged Firecracker binary or guest kernel differs from the running one"
        self.run_plan(ops, ["worker"], 2, ["worker"], plan={**self.handover_plan(), "running_boxes": "stop"})
        self.assertEqual((self.journal()["step"], ops.box["a"]), ("done", "running"))
        self.assertIn("stop-box:a", ops.calls)
        self.assertNotIn("handover", ops.calls)

    def test_a_worker_restart_stops_the_boxes_when_the_setting_says_so(self):
        h.write_json(f"{h.RUNNER}/controller-settings.json",
                     {"upgrade.worker_handover": False, "upgrade.running_boxes": "stop"})
        with mock.patch.object(h, "role", return_value="runner"):
            ops = Fake({"a": "running"})
            u.restart_worker(ops)
            self.assertIn("stop-box:a", ops.calls)
            ops = Fake({"a": "running"})
            u.restart_worker(ops, hibernate=True)
            self.assertNotIn("stop-box:a", ops.calls)
            self.assertIn("hibernated:a", ops.calls)
        self.assertEqual(ops.box["a"], "running")

    def test_running_boxes_stop_abort_starts_the_stopped_boxes_again(self):
        for fail in ("stop-box:b", "sync-images"):
            ops = Fake({"a": "running", "b": "running"}, fail={fail})
            with self.assertRaisesRegex(RuntimeError, f"injected failure at {fail}"):
                self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.stop_plan())
            self.assertEqual((self.journal()["step"], ops.box), ("aborted", {"a": "running", "b": "running"}), fail)
            self.assertEqual(h.pins(), self.frm, "no pin moved")
            self.assertFalse(ops.drained or ops.quiesced)

    def test_running_boxes_stop_rollback_starts_the_stopped_boxes_on_the_old_worker(self):
        ops = Fake({"a": "running"}, fail={"verify"})
        real = ops.verify
        ops.verify = lambda expected: (expected["worker"] == "0.5.0" and ops.fail.clear(), real(expected))
        with self.assertRaisesRegex(RuntimeError, "injected failure at verify"):
            self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.stop_plan())
        self.assertEqual((self.journal()["step"], h.pin("worker"), ops.box["a"]), ("rolled-back", self.old, "running"))

    def test_killed_while_stopping_the_boxes_then_recovered(self):
        for kill, end in (("stop-box:b", "aborted"), ("start:worker", "rolled-back")):
            for name in (*h.PINS, "current"):
                h.set_pin(name, self.old)
            ops = Fake({"a": "running", "b": "running"}, kill={kill})
            with self.assertRaises(KeyboardInterrupt):
                self.run_plan(ops, ["worker"], 2, ["worker"], plan=self.stop_plan())
            self.assertIn("stopped", ops.box.values())
            ops.kill.clear()
            u.recover(ops)
            self.assertEqual((self.journal()["step"], ops.box), (end, {"a": "running", "b": "running"}), kill)
            self.assertFalse(ops.drained or ops.quiesced)

    def test_an_abort_meeting_the_old_worker_as_it_exits_still_restores_the_host(self):
        def gave_up(ops):
            def hibernate_all(release):
                # The down client timed out (os error 11) while the old worker finishes its
                # shutdown: silent, then gone, with every box hibernated.
                ops.box.update(dict.fromkeys(ops.box, "hibernated"))
                ops.silent = 2
                raise RuntimeError("Resource temporarily unavailable (os error 11)")
            return hibernate_all

        def exits_at_list(ops):
            def hibernate_all(release):
                # The worker still answers; it exits just as the abort lists the boxes.
                ops.box.update(dict.fromkeys(ops.box, "hibernated"))
                ops.lists_failing = 1
                raise RuntimeError("Resource temporarily unavailable (os error 11)")
            return hibernate_all

        for race in (gave_up, exits_at_list):
            ops = Fake({"a": "running", "b": "running"}, timers=u.TIMERS)
            ops.hibernate_all = race(ops)
            with mock.patch.object(u.time, "sleep"), self.assertRaisesRegex(RuntimeError, "os error 11"):
                self.run_plan(ops, ["worker"], 2, ["worker"])
            self.assertEqual((self.journal()["step"], ops.box), ("aborted", {"a": "running", "b": "running"}),
                             race.__name__)
            self.assertIn("start:worker", ops.calls)
            self.assertEqual((ops.drained, ops.ticking), (False, set(u.TIMERS)))

    def test_an_abort_whose_old_worker_exits_while_it_resumes_starts_it_and_resumes_again(self):
        ops = Fake({"a": "running", "b": "running"}, fail={"sync-images"}, timers=u.TIMERS)
        real = ops.start_box
        exits = [1]

        def start_box(box):
            if exits:  # a `down` that outlived its client shuts the worker down now
                exits.pop()
                ops.up["worker"] = False
            real(box)
        ops.start_box = start_box
        with mock.patch.object(u.time, "sleep"), self.assertRaisesRegex(RuntimeError, "sync-images"):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        self.assertEqual((self.journal()["step"], ops.box, ops.up["worker"]),
                         ("aborted", {"a": "running", "b": "running"}, True))
        self.assertEqual((ops.drained, ops.ticking), (False, set(u.TIMERS)))

    def test_an_abort_whose_restores_fail_still_restores_the_rest_and_closes(self):
        ops = Fake({"a": "running"}, fail={"sync-images", "start:dashboard"}, timers=u.TIMERS)
        with mock.patch.object(u.time, "sleep"), self.assertRaisesRegex(RuntimeError, "sync-images"):
            self.run_plan(ops, ["worker", "dashboard"], 2, ["worker", "dashboard"])
        j = self.journal()
        self.assertEqual(j["step"], "aborted")
        self.assertIn("start dashboard failed", j["error"])
        self.assertEqual((ops.drained, ops.ticking), (False, set(u.TIMERS)))

    def test_an_abort_that_cannot_list_the_boxes_records_it_and_still_closes(self):
        ops = Fake({"a": "running"}, fail={"sync-images"}, timers=u.TIMERS)
        real = ops.sync_images

        def sync_images(release):
            ops.lists_failing = 99
            real(release)
        ops.sync_images = sync_images
        with mock.patch.object(u.time, "sleep"), self.assertRaisesRegex(RuntimeError, "sync-images"):
            self.run_plan(ops, ["worker"], 2, ["worker"])
        j = self.journal()
        self.assertEqual(j["step"], "aborted")
        self.assertIn("boxes not resumed (ow: No such file or directory", j["error"])
        self.assertEqual((ops.drained, ops.ticking, h.pins()), (False, set(u.TIMERS), self.frm))

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


class Timers(unittest.TestCase):
    def test_host_ops_stop_the_recovery_run_and_start_only_timers_that_exist(self):
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(h, "run", return_value="") as run:
            h.ROOT = Path(tmp)
            ops = u.HostOps()
            ops.pause_timers(["ow-recovery.timer"])
            h.write_json("/etc/systemd/system/ow-backup.timer", {})
            ops.resume_timers(["ow-recovery.timer", "ow-backup.timer"])
            self.assertEqual([c.args[0] for c in run.call_args_list],
                             [["systemctl", "stop", "ow-recovery.timer"],
                              ["systemctl", "stop", "ow-recovery.service"],
                              ["systemctl", "start", "ow-backup.timer"]])

    def test_apply_leaves_the_timers_stopped_while_a_journal_is_open(self):
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(h, "run", return_value="") as run:
            h.ROOT = Path(tmp)
            for timer in u.TIMERS:
                h.write_json(f"/etc/systemd/system/{timer}", {})
            for step, flag in (("hibernate", []), ("done", ["--now"])):
                h.write_json(u.JOURNAL, {"step": step})
                run.reset_mock()
                h.services()
                self.assertEqual([c.args[0] for c in run.call_args_list],
                                 [["systemctl", "enable", *flag, timer] for timer in u.TIMERS])


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

    def test_a_failed_download_is_refused_not_a_traceback(self):
        def fail(*a, **k):
            raise u.urllib.error.URLError("network unreachable")

        with mock.patch.object(u.urllib.request, "urlopen", fail):
            with self.assertRaisesRegex(Refused, "cannot download .*--from-source builds instead"):
                u.download("https://github.com/o/r/releases/download/v1.2.3/SHA256SUMS")

        def missing(*a, **k):
            raise u.urllib.error.HTTPError(a[0], 404, "Not Found", None, None)

        with mock.patch.object(u.urllib.request, "urlopen", missing):
            self.assertIsNone(u.download("https://github.com/o/r/releases/download/v1.2.3/SHA256SUMS"))

    def test_release_urls_are_github_tags_only(self):
        self.assertEqual(u.release_url("https://github.com/o/r.git", "v1.2.3"),
                         "https://github.com/o/r/releases/download/v1.2.3")
        self.assertIsNone(u.release_url("/srv/mirror", "v1.2.3"))
        self.assertIsNone(u.release_url("https://example.com/o/r.git", "v1.2.3"))


class Unchanged(unittest.TestCase):
    """ci-post-up's fast path: 0 only when the release, every pin and each image already match."""

    RELEASE = "0.5.1-bbbbbbbbbbbb"
    SHA = "a" * 64

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name)
        h.write_json(h.CONFIG, {"schema": 1, "release": {"repository": "/srv/seed.bundle", "ref": "main"}})
        for release, v in (("0.5.0-aaaaaaaaaaaa", "0.5.0"), (self.RELEASE, "0.5.1")):
            h.write_json(f"{h.SRV}/releases/{release}/STAGED", {"version": v})
        for name in (*h.PINS, "current"):
            h.set_pin(name, self.RELEASE)
        self.target = {"release": self.RELEASE, "version": "0.5.1",
                       "manifest": manifest(entry("0.5.1", ["worker", "agent"], 2))}
        self.images = [{"name": "ubuntu"}, {"name": "team", "recipe_sha256": self.SHA}]
        self.developer = "inactive"  # the developer image unit's ActiveState

    def tearDown(self):
        self.tmp.cleanup()

    def check(self, images=(f"team={'a' * 64}",)):
        with mock.patch.object(u, "fetch", return_value=self.target), \
                mock.patch.object(h, "ow", return_value=self.images), \
                mock.patch.object(h, "run", side_effect=lambda *a, **k: self.developer + "\n"), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            return u.unchanged(list(images)), out.getvalue()

    def test_everything_in_place_is_unchanged(self):
        self.assertEqual(self.check(), (0, ""))

    def test_each_difference_takes_the_full_path(self):
        cases = {
            "release": lambda: h.set_pin("current", "0.5.0-aaaaaaaaaaaa"),
            # Current is the target, but the worker still runs a release whose change it has not taken.
            "pins to move: worker": lambda: h.set_pin("worker", "0.5.0-aaaaaaaaaaaa"),
            "restarts pending": lambda: h.mark_pending({"agent"}, "config changed"),
            "interrupted upgrade": lambda: h.write_json(u.JOURNAL, {"step": "hibernate"}),
            "image team": lambda: self.images[1].update(recipe_sha256="b" * 64),
            "developer image build is activating": lambda: setattr(self, "developer", "activating"),
            "developer image build is failed": lambda: setattr(self, "developer", "failed"),
        }
        for what, change in cases.items():
            with self.subTest(what):
                self.tearDown()
                self.setUp()
                change()
                code, out = self.check()
                self.assertEqual(code, 1)
                self.assertIn(what, out)

    def test_an_image_never_built_or_a_failed_check_is_never_unchanged(self):
        self.assertEqual(self.check([f"other={self.SHA}"])[0], 1)
        with mock.patch.object(u, "fetch", side_effect=RuntimeError("git fetch failed")), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(u.unchanged([]), 1)
        self.assertIn("could not check (git fetch failed)", out.getvalue())

    def test_a_pin_on_an_older_commit_of_the_same_version_is_not_a_change(self):
        # Only `current` moves to a new commit of the same version; upgrade then has nothing to move.
        h.write_json(f"{h.SRV}/releases/0.5.1-cccccccccccc/STAGED", {"version": "0.5.1"})
        h.set_pin("worker", "0.5.1-cccccccccccc")
        self.assertEqual(self.check()[0], 0)


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
