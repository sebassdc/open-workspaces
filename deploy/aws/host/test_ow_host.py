"""Rendered units, config validation and setup, in a temporary root.

Run: python3 -m unittest discover -s deploy/aws/host
"""
import copy
import contextlib
import io
import json
from unittest import mock
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ow_host as h  # noqa: E402
from ow_host import Refused  # noqa: E402

CONFIG = {
    "schema": 1,
    "role": "host",
    "release": {"repository": "https://github.com/example/open-workspaces.git", "ref": "v0.5.0"},
    "tailscale": {"secretArn": "arn:aws:secretsmanager:us-east-1:111122223333:secret:ow/ts-AbCdEf",
                  "hostname": "ow-host", "ssh": False},
    "dashboard": {"logins": ["you@example.com"]},
    "developerImages": ["ubuntu"],
    "backup": {"url": "s3://my-bucket/ow/host-1", "schedule": "*-*-* 01:00:00 UTC"},
    "idleMinutes": 30,
}
BUDGET = {"memory": 11776, "cpus": 4, "slots": 8, "hostMemory": 16384}


def config(**changes):
    c = copy.deepcopy(CONFIG)
    for key, value in changes.items():
        section, _, field = key.partition("__")
        if field:
            c[section][field] = value
        else:
            c[section] = value
    return h.validate(c)


class Render(unittest.TestCase):
    def setUp(self):
        self.files = {k: v[0] for k, v in h.render(config(), BUDGET).items()}

    def test_overcommit_changes_both_admission_consumers_only(self):
        # The host reserve keeps one of the four CPUs before the ratio applies.
        for ratio, expected in [(1, 3), (2, 6)]:
            files = {k: v[0] for k, v in h.render(config(vcpuOvercommit=ratio), BUDGET).items()}
            self.assertIn(f"OW_MAX_VCPUS={expected}\n", files["/etc/open-workspaces/worker.env"])
            self.assertIn(f"--memory 11776 --slots 8 --cpus {expected}", files["/etc/systemd/system/open-workspaces.service"])
            self.assertIn("OW_MAX_MEMORY_MIB=11776\n", files["/etc/open-workspaces/worker.env"])
            self.assertIn("OW_MAX_RUNNING=8\n", files["/etc/open-workspaces/worker.env"])
        files = h.render(config(vcpuOvercommit=2), {**BUDGET, "cpus": 48})
        self.assertIn("OW_MAX_VCPUS=64\n", files["/etc/open-workspaces/worker.env"][0])
        self.assertEqual(config()["vcpuOvercommit"], 1)
        for ratio in [0, 3, 1.5, "2", None, True]:
            with self.assertRaisesRegex(Refused, "vcpuOvercommit"):
                config(vcpuOvercommit=ratio)
        old = h.render(config(), BUDGET)
        new = h.render(config(vcpuOvercommit=2), BUDGET)
        changed = [path for path in old if old[path] != new[path]]
        self.assertEqual(h.units_changed(changed), {"worker", "agent"})

    def test_status_reports_the_effective_budget_used_by_runner_enrollment(self):
        with mock.patch.object(h, "versions", return_value={}), \
             mock.patch.object(h, "pins", return_value={}), \
             mock.patch.object(h, "role", return_value="runner"), \
             mock.patch.object(h, "local_node", return_value=None), \
             mock.patch.object(h, "pending", return_value={}), \
             mock.patch.object(h, "unit_exists", return_value=False), \
             mock.patch.object(h, "budgets", return_value=BUDGET.copy()), \
             mock.patch.object(h, "read_json", side_effect=lambda path, *a: config(vcpuOvercommit=2) if path == h.CONFIG else None):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                h.status()
            self.assertEqual(json.loads(output.getvalue())["budget"], {**BUDGET, "cpus": 6})

    def test_worker_unit_hibernates_and_control_plane_never_takes_it_down(self):
        worker = self.files["/etc/systemd/system/open-workspaces-worker.service"]
        self.assertIn("ExecStart=/srv/ow/run/worker/bin/ow --data-dir /srv/ow/.ow/runner supervisor", worker)
        self.assertIn("ExecStop=/srv/ow/run/worker/bin/ow --data-dir /srv/ow/.ow/runner down --hibernate", worker)
        self.assertIn("TimeoutStopSec=900", worker)
        cp = self.files["/etc/systemd/system/open-workspaces.service"]
        self.assertIn("ExecStart=/srv/ow/run/agent/bin/ow host up --external-worker --memory 11776 --slots 8 --cpus 3", cp)
        self.assertNotIn("ExecStop", cp)
        self.assertIn("Wants=open-workspaces-worker.service", cp)
        self.assertIn("After=open-workspaces-worker.service", cp)
        for word in ("Requires=", "BindsTo=", "PartOf="):
            self.assertNotIn(word, cp + worker, "a restart of one unit must not propagate to the other")
        self.assertIn("ExecStartPre=/usr/local/lib/open-workspaces/wait-tailnet", cp)
        env = self.files["/etc/open-workspaces/worker.env"]
        self.assertIn("OW_ASSET_DIR=/srv/ow/run/worker/assets\n", env)
        self.assertIn("OW_IDLE_HIBERNATE_SECS=1800\n", env)
        self.assertIn("OW_MAX_MEMORY_MIB=11776\n", env)

    def test_dashboard_stays_on_loopback_and_is_served_to_the_tailnet_only(self):
        unit = self.files["/etc/systemd/system/open-workspaces-dashboard.service"]
        self.assertIn("--listen 127.0.0.1:8787", unit)
        self.assertIn("ExecStart=/srv/ow/run/dashboard/bin/ow", unit)
        prepare = self.files["/usr/local/lib/open-workspaces/dashboard-prepare"]
        self.assertIn("tailscale serve --bg http://127.0.0.1:8787", prepare)
        self.assertNotIn("funnel", prepare)
        self.assertEqual(self.files["/etc/sudoers.d/ow-preview"], "ow ALL=(root) NOPASSWD: /usr/local/lib/open-workspaces/preview-serve *\n")
        self.assertIn("/usr/bin/tailscale", self.files["/usr/local/lib/open-workspaces/preview-serve"])
        self.assertEqual(h.units_changed(["/usr/local/lib/open-workspaces/preview-serve"]), {"dashboard"})
        self.assertEqual(self.files["/etc/open-workspaces/dashboard.env"], "OW_DASHBOARD_LOGINS=you@example.com\nOW_DASHBOARD_CAPABILITY=\n")

    def test_capability_alone_enables_dashboard_and_changes_only_dashboard_config(self):
        c = config(dashboard={"capability": "example.test/cap/open-workspaces"})
        self.assertEqual(c["dashboard"], {"logins": [], "capability": "example.test/cap/open-workspaces"})
        files = {k: v[0] for k, v in h.render(c, BUDGET).items()}
        self.assertIn("OW_DASHBOARD_LOGINS=\nOW_DASHBOARD_CAPABILITY=example.test/cap/open-workspaces\n",
                      files["/etc/open-workspaces/dashboard.env"])
        changed = {path for path in files if files[path] != self.files[path]}
        self.assertEqual(changed, {"/etc/open-workspaces/dashboard.env"})
        for path in ("/usr/local/lib/open-workspaces/preview-serve", "/etc/sudoers.d/ow-preview"):
            self.assertEqual(files[path], self.files[path], "capability-only dashboards retain preview support")
        self.assertEqual(h.units_changed(changed), {"dashboard"})

    def test_dashboard_prepare_writes_capability_only_and_email_configs(self):
        import json
        import os
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            # Run the real shell/Python generator with a private path and local tool stand-ins.
            script = self.files["/usr/local/lib/open-workspaces/dashboard-prepare"].replace(
                "/srv/ow/.ow/dashboard.json", str(root / "dashboard.json"))
            (root / "tailscale").write_text('#!/bin/sh\n[ "$1" != status ] || echo \'{"BackendState":"Running","Self":{"DNSName":"host.example.ts.net."}}\'\n')
            (root / "runuser").write_text('#!/bin/sh\nshift 3\nexec "$@"\n')
            for tool in ("tailscale", "runuser"):
                (root / tool).chmod(0o755)
            for logins, capability in [("", "example.test/cap/open-workspaces"), ("you@example.com", ""),
                                       ("you@example.com", "example.test/cap/open-workspaces")]:
                subprocess.run(["bash", "-c", script], check=True, capture_output=True,
                               env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                                    "OW_DASHBOARD_LOGINS": logins, "OW_DASHBOARD_CAPABILITY": capability})
                output = json.loads((root / "dashboard.json").read_text())
                self.assertEqual(output["allowed_emails"], logins.split(",") if logins else [])
                self.assertEqual(output.get("allowed_capability", ""), capability)
                self.assertEqual((root / "dashboard.json").stat().st_mode & 0o777, 0o600)

    def test_optional_features_render_only_when_configured(self):
        bare = h.render(config(dashboard={"logins": []}, developerImages=[], backup={"url": ""}), BUDGET)
        for path in ("/etc/systemd/system/open-workspaces-dashboard.service",
                     "/etc/systemd/system/open-workspaces-images.service", "/usr/local/bin/ow-backup",
                     "/etc/systemd/system/ow-backup.timer"):
            self.assertIn(path, self.files)
            self.assertNotIn(path, bare)
        self.assertIn("OnCalendar=*-*-* 01:00:00 UTC", self.files["/etc/systemd/system/ow-backup.timer"])
        self.assertNotIn("/etc/systemd/system/ow-backup.timer", h.render(config(backup__schedule=""), BUDGET))
        images = self.files["/etc/systemd/system/open-workspaces-images.service"]
        self.assertNotIn("ExecStartPost", images, "a new developer image no longer restarts anything")

    def test_recovery_is_opt_in_and_never_restarts_worker(self):
        from unittest.mock import patch
        self.assertNotIn("/etc/systemd/system/ow-recovery.timer", self.files)
        files = h.render(config(backup__recovery=True), BUDGET)
        unit = files["/etc/systemd/system/ow-recovery.service"][0]
        self.assertIn("ExecStart=/usr/local/bin/ow-recovery", unit)
        self.assertIn("OnUnitInactiveSec=1min", files["/etc/systemd/system/ow-recovery.timer"][0])
        self.assertEqual(files["/etc/systemd/system/open-workspaces-worker.service"],
                         h.render(config(), BUDGET)["/etc/systemd/system/open-workspaces-worker.service"])
        self.assertNotIn("worker", h.units_changed({"/etc/systemd/system/ow-recovery.timer"}))
        for backup in ({"url": "", "recovery": True}, {"url": "s3://b/p", "recovery": "true"}):
            with self.assertRaises(Refused):
                config(backup=backup)

    def test_scripts_are_valid_shell_and_the_wrapper_runs_the_current_release(self):
        for path, text in self.files.items():
            if text.startswith("#!/bin/"):
                subprocess.run(["bash", "-n"], input=text, text=True, check=True)
        self.assertIn("exec /srv/ow/current/bin/ow", self.files["/usr/local/bin/ow"])
        self.assertIn("/srv/ow/current/src/deploy/aws/host/ow_host.py", self.files["/usr/local/sbin/ow-host"])
        self.assertIn("/srv/ow/{releases/*/bin,build/target/release}/ow", self.files["/etc/apparmor.d/open-workspaces"])
        self.assertIn("cd /srv/ow/run/worker/src", self.files["/usr/local/lib/open-workspaces/developer-images"])

    def test_every_rendered_path_is_owned_and_restarts_map_to_pins(self):
        self.assertLessEqual(set(self.files), set(h.OWNED))
        self.assertLessEqual(set(h.RESTARTS.values()), set(h.PINS))


    def test_direct_tailscale_routes_only_local_wireguard_and_pins_port(self):
        direct = config(tailscale__direct={"mac": "02:00:00:00:00:01"})
        files = {k: v[0] for k, v in h.render(direct, BUDGET).items()}
        net = files[h.DIRECT_NETWORK]
        for line in ("MACAddress=02:00:00:00:00:01", "RouteTable=41641", "IncomingInterface=lo",
                     "IPProtocol=udp", "SourcePort=41641", "Priority=5100", "IPv4ReversePathFilter=loose",
                     "UseDNS=no", "UseNTP=no"):
            self.assertIn(line, net)
        self.assertIn(f"EnvironmentFile={h.DIRECT_ENV}", files[h.DIRECT_PORT])
        self.assertEqual(files[h.DIRECT_ENV], "PORT=41641\n")
        for path in (h.DIRECT_NETWORK, h.DIRECT_PORT, h.DIRECT_ENV):
            self.assertNotIn(path, self.files)
        self.assertEqual(h.units_changed({h.DIRECT_NETWORK, h.DIRECT_PORT, h.DIRECT_ENV}), set())
        self.assertEqual(files["/etc/systemd/system/open-workspaces-worker.service"],
                         self.files["/etc/systemd/system/open-workspaces-worker.service"])


class HostReserve(unittest.TestCase):
    def test_admission_math_keeps_the_reserve_before_the_ratio(self):
        host, runner = config(), config(role="runner", dashboard={}, runner={
            "node": "r1", "controller": "ow-host", "joinSecretArn": CONFIG["tailscale"]["secretArn"]})
        self.assertEqual(host["hostReserve"], {"cpuMillis": 1000, "memoryMib": 2048})
        self.assertEqual(runner["hostReserve"], {"cpuMillis": 500, "memoryMib": 1024})
        physical = {"memory": 6144, "cpus": 2, "slots": 8, "hostMemory": 8192}
        self.assertEqual(h.effective(host, physical), {**physical, "memory": 6144, "cpus": 1})
        self.assertEqual(h.effective({**runner, "vcpuOvercommit": 2}, physical), {**physical, "cpus": 3})
        small = {**physical, "memory": 3072, "hostMemory": 4096}
        self.assertEqual(h.effective(host, small)["memory"], 2048, "RAM less the reserve, under three quarters")
        self.assertEqual(h.effective(config(hostReserve={"cpuMillis": 0, "memoryMib": 0}), physical), physical)
        tiny = {"memory": 768, "cpus": 1, "slots": 8, "hostMemory": 1024}
        self.assertEqual(h.effective(host, tiny), {**tiny, "memory": 0, "cpus": 0}, "never below zero")
        self.assertEqual(h.effective({**host, "vcpuOvercommit": 2}, {**physical, "cpus": 64})["cpus"], 64)
        self.assertEqual(config(hostReserve={"memoryMib": 4096})["hostReserve"], {"cpuMillis": 1000, "memoryMib": 4096})
        for bad in [{"cpuMillis": -1}, {"cpuMillis": 64001}, {"memoryMib": 1.5}, {"memoryMib": True}, {"other": 1}, []]:
            with self.assertRaisesRegex(Refused, "hostReserve"):
                config(hostReserve=bad)

    def test_a_reserve_leaving_no_room_for_boxes_is_refused_before_writing(self):
        physical = {"memory": 6144, "cpus": 2, "slots": 8, "hostMemory": 8192}
        with self.assertRaisesRegex(Refused, "no room for boxes"):
            h.render(config(hostReserve={"cpuMillis": 2000}), physical)
        tiny = {"memory": 768, "cpus": 1, "slots": 8, "hostMemory": 1024}
        with self.assertRaisesRegex(Refused, "no room for boxes"):
            h.render(config(), tiny)
        self.assertIn("--cpus 1\n", h.render(config(), physical)["/etc/systemd/system/open-workspaces.service"][0])

    def test_units_protect_the_control_path_and_cap_the_guests(self):
        files = {k: v[0] for k, v in h.render(config(), BUDGET).items()}
        control = files["/etc/systemd/system/open-workspaces.service"]
        for line in ("Slice=owcontrol.slice", "CPUWeight=1000", "MemoryMin=1024M", "MemoryLow=2048M", "OOMScoreAdjust=-900"):
            self.assertIn(line + "\n", control)
        self.assertIn("CPUWeight=1000\nMemoryMin=1024M\nMemoryLow=2048M\n", files["/etc/systemd/system/owcontrol.slice"])
        worker = files["/etc/systemd/system/open-workspaces-worker.service"]
        # The worker is protected from its own VMs: it sits at -900, they raise themselves to 500.
        for line in ("Slice=owguests.slice", "Delegate=cpu memory", "DelegateSubgroup=control",
                     "MemoryLow=256M", "OOMScoreAdjust=-900"):
            self.assertIn(line + "\n", worker)
        guests = files["/etc/systemd/system/owguests.slice"]
        self.assertIn("MemoryMax=14336M\n", guests)
        self.assertIn("MemoryLow=256M\n", guests)
        env = files["/etc/open-workspaces/worker.env"]
        self.assertIn("OW_HOST_RESERVE_CPU_MILLIS=1000\nOW_HOST_RESERVE_MEMORY_MIB=2048\n"
                      "OW_VCPU_OVERCOMMIT=1\nOW_DELEGATED_CGROUP=1\n", env)
        for path in ("/etc/systemd/system/owcontrol.slice", "/etc/systemd/system/owguests.slice"):
            self.assertIn(path, h.OWNED)
        # A reserve change reaches the worker's budget and the control unit: class 2.
        old, new = h.render(config(), BUDGET), h.render(config(hostReserve={"cpuMillis": 2000}), BUDGET)
        self.assertEqual(h.units_changed([path for path in old if old[path] != new.get(path)]), {"worker", "agent"})


class Validate(unittest.TestCase):
    def test_capability_is_validated_and_requires_tailnet(self):
        for bad in ["no-domain", "example.test/", "https://example.test/cap", "example.test/cap\nX=1",
                    "-bad.test/cap", "example.test/" + "a" * 512, 42]:
            with self.assertRaisesRegex(Refused, "dashboard.capability"):
                config(dashboard={"capability": bad})
        with self.assertRaisesRegex(Refused, "need tailscale"):
            config(dashboard={"capability": "example.test/cap/open-workspaces"}, tailscale__secretArn="")

    def test_bad_values_are_refused(self):
        for key, value in [("dashboard__logins", ["no-at-sign"]), ("dashboard__logins", ["$(id)@x"]),
                           ("developerImages", ["arch"]), ("backup__url", "s3://b/ow"),
                           ("backup__url", "s3://my-bucket/a/../b"), ("backup__schedule", "daily; id"),
                           ("idleMinutes", -1), ("idleMinutes", True), ("release__ref", "v1\nx"),
                           ("release__ref", "a..b"), ("release__repository", "http://plain.example/x"),
                           ("tailscale__hostname", "bad name"), ("tailscale__ssh", "yes"),
                           ("tailscale__secretArn", "arn:aws:iam::1:role/x")]:
            with self.assertRaises(Refused, msg=key):
                config(**{key: value})
        with self.assertRaisesRegex(Refused, "need tailscale"):
            config(tailscale__secretArn="")
        with self.assertRaisesRegex(Refused, "schema"):
            h.validate({"schema": 2})

    def test_direct_tailscale_validates_untrusted_config(self):
        for bad in ({}, {"mac": "$(id)"}, {"mac": "02:00:00:00:00:01", "port": 22}, "bad"):
            with self.assertRaisesRegex(Refused, "tailscale.direct"):
                config(tailscale__direct=bad)
        with self.assertRaisesRegex(Refused, "tailscale.direct"):
            config(tailscale__direct={"mac": "02:00:00:00:00:01"}, tailscale__secretArn="")

    def test_defaults(self):
        c = h.validate({"schema": 1, "release": {"repository": "/srv/seed.bundle", "ref": "main"}})
        self.assertEqual((c["idleMinutes"], c["dashboard"]["logins"], c["backup"]["url"]), (30, [], ""))


class Setup(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name)
        (h.ROOT / "proc").mkdir()
        (h.ROOT / "proc/meminfo").write_text("MemTotal:       16000000 kB\n")
        self.calls = []
        self.real_run = h.run
        self.real_fetch = h.fetch_config
        h.run = lambda cmd, **kw: self.calls.append(cmd) or ""

    def tearDown(self):
        h.run = self.real_run
        h.fetch_config = self.real_fetch
        self.tmp.cleanup()

    def test_setup_writes_only_differences_and_removes_turned_off_features(self):
        first = h.setup(config())
        self.assertIn("/etc/systemd/system/open-workspaces-dashboard.service", first)
        self.assertIn(["systemctl", "daemon-reload"], self.calls)
        self.assertIn(["apparmor_parser", "-r", "/etc/apparmor.d/open-workspaces"], self.calls)
        self.calls.clear()
        self.assertEqual(h.setup(config()), set(), "a second setup with the same config changes nothing")
        self.assertEqual(self.calls, [])
        changed = h.setup(config(dashboard={"logins": ["other@example.com"]}))
        self.assertEqual(h.units_changed(changed), {"dashboard"})
        changed = h.setup(config(dashboard={"logins": []}, idleMinutes=10))
        self.assertFalse(h.p("/etc/systemd/system/open-workspaces-dashboard.service").exists())
        self.assertIn(["systemctl", "disable", "--now", "open-workspaces-dashboard.service"], self.calls)
        self.assertEqual(h.units_changed(changed), {"dashboard", "worker"})
        self.assertEqual(h.diff(h.render(config(dashboard={"logins": []}, idleMinutes=10), h.budgets())), set())

    def test_a_failed_apply_keeps_its_restarts_for_the_retry(self):
        import upgrade as u
        h.setup(config())
        h.write_json(h.CONFIG, config())
        h.fetch_config = lambda: {**copy.deepcopy(CONFIG), "idleMinutes": 10,
                                  "tailscale": {**CONFIG["tailscale"], "hostname": "renamed"}}
        failures = ["tailscale set"]

        def flaky(cmd, **kw):
            self.calls.append(cmd)
            if cmd[:2] == ["tailscale", "status"]:
                return '{"BackendState": "Running"}'
            if failures and " ".join(cmd).startswith(failures[0]):
                failures.pop()
                raise RuntimeError("transient tailscale failure")
            return ""

        h.run = flaky
        with self.assertRaisesRegex(RuntimeError, "transient"):
            h.apply_config()
        self.assertEqual(h.read_json(h.CONFIG)["idleMinutes"], 30, "the stored config moves only after success")
        self.assertEqual(h.apply_config(), 0)
        self.assertEqual(set(h.pending()), {"worker", "agent"}, "the idle change and the tailnet rename survive")
        self.assertEqual(h.read_json(h.CONFIG)["idleMinutes"], 10)
        at = {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.0"}
        manifest = {"schema": 1, "releases": [{"version": "0.5.0", "changes": [], "class": 0,
                    "catalog": {"schema": 3, "min_reader": 3}, "protocol": {"speaks": 1, "accepts": [1, 1]},
                    "worker_protocol": {"speaks": 1, "accepts": [1, 1]}, "in_place_from": None}]}
        plan = u.plan(manifest, at, "0.5.0", pending=set(h.pending()))
        self.assertEqual((plan["class"], plan["restart"]), (2, ["agent", "worker"]))

    def test_a_failure_inside_setup_keeps_the_reload_and_restarts_for_the_retry(self):
        import upgrade as u
        h.setup(config(dashboard={"logins": []}))
        h.write_json(h.CONFIG, config(dashboard={"logins": []}))
        h.fetch_config = lambda: {**copy.deepcopy(CONFIG), "idleMinutes": 10}
        failures = ["systemctl daemon-reload"]

        def flaky(cmd, **kw):
            self.calls.append(cmd)
            if cmd[:2] == ["tailscale", "status"]:
                return '{"BackendState": "Running"}'
            if failures and " ".join(cmd) == failures[0]:
                failures.pop()
                raise RuntimeError("transient daemon-reload failure")
            return ""

        h.run = flaky
        with self.assertRaisesRegex(RuntimeError, "daemon-reload"):
            h.apply_config()
        self.assertEqual(set(h.pending()), {"worker", "dashboard"}, "recorded before setup wrote anything")
        self.calls.clear()
        self.assertEqual(h.apply_config(), 0)
        self.assertIn(["systemctl", "daemon-reload"], self.calls, "the reload the first try missed")
        self.assertIn(["systemctl", "restart", "--no-block", "open-workspaces-dashboard.service"], self.calls)
        self.assertEqual(set(h.pending()), {"worker"})
        self.assertFalse(h.p(h.SETUP_DUE).exists())
        at = {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.0"}
        manifest = {"schema": 1, "releases": [{"version": "0.5.0", "changes": [], "class": 0,
                    "catalog": {"schema": 3, "min_reader": 3}, "protocol": {"speaks": 1, "accepts": [1, 1]},
                    "worker_protocol": {"speaks": 1, "accepts": [1, 1]}, "in_place_from": None}]}
        self.assertEqual(u.plan(manifest, at, "0.5.0", pending=set(h.pending()))["restart"], ["worker"])

    def test_a_login_change_restarts_the_dashboard_even_after_a_failed_try(self):
        h.setup(config())
        h.write_json(h.CONFIG, config())
        h.fetch_config = lambda: {**copy.deepcopy(CONFIG), "dashboard": {"logins": ["new@example.com"]}}

        def failing(cmd, **kw):
            self.calls.append(cmd)
            if cmd[:2] == ["tailscale", "status"]:
                return '{"BackendState": "Running"}'
            if cmd[:2] == ["systemctl", "restart"]:
                raise RuntimeError("transient systemd failure")
            return ""

        h.run = failing
        with self.assertRaises(RuntimeError):
            h.apply_config()
        h.run = lambda cmd, **kw: self.calls.append(cmd) or ('{"BackendState": "Running"}' if cmd[:2] == ["tailscale", "status"] else "")
        self.calls.clear()
        h.apply_config()
        self.assertIn(["systemctl", "restart", "--no-block", "open-workspaces-dashboard.service"], self.calls)
        self.assertNotIn("dashboard", h.pending())

    def test_direct_setup_is_targeted_idempotent_and_retries_after_network_failure(self):
        h.setup(config())
        direct = config(tailscale__direct={"mac": "02:00:00:00:00:01"})
        failures = [True]
        dhcp_pending = [True]

        def fake(cmd, **kw):
            self.calls.append(cmd)
            if cmd == ["networkctl", "reload"] and failures:
                failures.pop()
                raise RuntimeError("network reload failed")
            if cmd == ["ip", "route", "flush", "table", "41641"]:
                # A detached interface (or an apply that failed before DHCP) can leave no table.
                if kw.get("check", True):
                    raise RuntimeError("FIB table does not exist")
                return ""
            if cmd == ["systemctl", "is-active", "tailscaled.service"]:
                return "active"
            if cmd == ["ip", "-j", "link", "show"]:
                return '[{"address":"02:00:00:00:00:01","ifname":"ens6"},{"address":"02:00:00:00:00:02","ifname":"ens5"}]'
            if cmd[:4] == ["ip", "-j", "route", "get"]:
                if dhcp_pending:
                    if kw.get("check", True):
                        raise RuntimeError("RTNETLINK answers: Network is unreachable")
                    dhcp_pending.pop()
                    return ""
                return '[{"dev":"ens6"}]'
            return ""

        h.run = fake
        with self.assertRaisesRegex(RuntimeError, "network reload"):
            h.setup(direct)
        self.assertTrue(h.p(h.SETUP_DUE).exists())
        self.calls.clear()
        h.setup(direct)
        self.assertFalse(dhcp_pending)
        self.assertEqual(sum(c[:4] == ["ip", "-j", "route", "get"] for c in self.calls), 2)
        self.assertIn(["networkctl", "reload"], self.calls)
        self.assertIn(["networkctl", "reconfigure", "ens6"], self.calls)
        self.assertNotIn(["networkctl", "reconfigure", "ens5"], self.calls)
        self.assertIn(["systemctl", "restart", "tailscaled.service"], self.calls)
        self.assertFalse(any("open-workspaces-worker.service" in c for c in self.calls))
        self.calls.clear()
        self.assertEqual(h.setup(direct), set())
        self.assertEqual(self.calls, [])
        h.setup(config())
        self.assertIn(["networkctl", "reconfigure", "ens6"], self.calls)
        self.assertIn(["ip", "route", "flush", "table", "41641"], self.calls)
        self.assertFalse(h.p(h.DIRECT_NETWORK).exists())
        self.assertFalse(h.p(h.DIRECT_PORT).exists())

    def test_tailscale_install_waits_for_the_package_lock_but_keeps_install_errors_fatal(self):
        h.p("/etc").mkdir(parents=True, exist_ok=True)
        h.p("/etc/os-release").write_text("VERSION_CODENAME=noble\n")
        install_error = None

        def fake(cmd, **kw):
            self.calls.append(cmd)
            if cmd == ["sh", "-c", "command -v tailscale"]:
                raise RuntimeError("not installed")
            if cmd[0] == "apt-get" and "install" in cmd:
                if "DPkg::Lock::Timeout=300" not in cmd:
                    raise RuntimeError("dpkg frontend lock held by another package process")
                if install_error:
                    raise RuntimeError(install_error)
            if cmd[:2] == ["tailscale", "status"]:
                return '{"BackendState": "Running"}'
            return ""

        h.run = fake
        self.assertEqual(h.tailscale(config()), ("joined", False))
        install_error = "package repository unavailable"
        self.calls.clear()
        with self.assertRaisesRegex(RuntimeError, install_error):
            h.tailscale(config())
        self.assertFalse(any(c[0] == "tailscale" for c in self.calls))

    def test_tailscale_join_passes_the_key_by_file_and_deletes_it(self):
        key = h.ROOT / "key"
        outputs = {"status": '{"BackendState": "NeedsLogin"}', "mktemp": str(key), "aws": "tskey-secret\n"}

        def fake(cmd, **kw):
            self.calls.append(cmd)
            if cmd[0] == "tailscale" and cmd[1] == "up":
                self.assertEqual(key.read_text(), "tskey-secret")
                self.assertIn(f"--auth-key=file:{key}", cmd)
                self.assertIn("--ssh=false", cmd)
            return outputs.get(Path(cmd[0]).name if cmd[0] != "tailscale" else cmd[1], "")

        h.run = fake
        self.assertEqual(h.tailscale(config()), ("joined", False))
        self.assertFalse(key.exists())
        self.assertFalse(any("tskey-secret" in " ".join(c) for c in self.calls), "the key never reaches a command line")
        outputs["aws"] = ""
        self.calls.clear()
        self.assertEqual(h.tailscale(config()), ("failed", False))
        self.assertFalse(any(c[:2] == ["tailscale", "up"] for c in self.calls), "an empty key never reaches tailscale up")

    def test_a_joined_host_applies_hostname_and_ssh_and_flags_a_rename(self):
        h.run = lambda cmd, **kw: self.calls.append(cmd) or ('{"BackendState": "Running"}' if cmd[:2] == ["tailscale", "status"] else "")
        state = h.tailscale(config(tailscale__hostname="new-name", tailscale__ssh=True), old=config())
        self.assertEqual(state, ("joined", True))
        self.assertIn(["tailscale", "set", "--ssh=true", "--hostname=new-name"], self.calls)


class StoreImages(unittest.TestCase):
    def test_opt_in_store_pin_is_class_zero_and_existing_config_stays_unchanged(self):
        pin = {"version": 1, "attempt": "a" * 32, "digest": "b" * 64}
        c = config(store={"url": "s3://my-bucket/ow", "images": {"ubuntu": pin}})
        files = h.render(c, BUDGET)
        path = "/etc/open-workspaces/store.json"
        self.assertEqual(json.loads(files[path][0]), c["store"])
        self.assertEqual(h.units_changed({path}), set())
        self.assertNotIn(path, h.render(config(), BUDGET))
        overcommitted = h.render(config(store=c["store"], vcpuOvercommit=2), BUDGET)
        self.assertEqual(overcommitted[path], files[path])
        self.assertIn("OW_MAX_VCPUS=6\n", overcommitted["/etc/open-workspaces/worker.env"][0])
        self.assertIn("--memory 11776 --slots 8 --cpus 6",
                      overcommitted["/etc/systemd/system/open-workspaces.service"][0])
        for bad in [dict(pin, version=0), dict(pin, attempt="../x"), dict(pin, digest="z" * 64)]:
            with self.assertRaises(h.Refused):
                config(store={"url": "s3://my-bucket/ow", "images": {"ubuntu": bad}})
        with self.assertRaises(h.Refused):
            config(store={"images": {"ubuntu": pin}})


if __name__ == "__main__":
    unittest.main()
