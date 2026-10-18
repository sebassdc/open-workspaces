"""Runner hosts (role runner): config, units, enrolment through the join secret, the
controller side of an invitation, readiness, and the upgrade planner's controller window.

Run: python3 -m unittest discover -s deploy/aws/host
"""
import copy
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ow_host as h  # noqa: E402
import upgrade as u  # noqa: E402
from ow_host import Refused  # noqa: E402
from test_ow_host import BUDGET, CONFIG  # noqa: E402

JOIN_ARN = "arn:aws:secretsmanager:us-east-1:111122223333:secret:open-workspaces/ow-big/join-AbCdEf"
RUNNER = {**copy.deepcopy(CONFIG), "role": "runner", "dashboard": {"logins": []},
          "backup": {"url": "s3://my-bucket/ow/runners/ow-big", "schedule": "*-*-* 02:00:00 UTC"},
          "runner": {"node": "ow-big", "controller": "ow-host", "joinSecretArn": JOIN_ARN}}
CA = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"


def runner(**changes):
    c = copy.deepcopy(RUNNER)
    for key, value in changes.items():
        section, _, field = key.partition("__")
        if field:
            c[section][field] = value
        else:
            c[section] = value
    return h.validate(c)


class Config(unittest.TestCase):
    def test_a_runner_config_needs_its_join_secret_the_tailnet_and_no_dashboard(self):
        self.assertEqual(runner()["runner"], RUNNER["runner"])
        for key, value, message in [
            ("runner__node", "bad name", "node ID"), ("runner__node", "local", "node ID"),
            ("runner__node", "x" * 33, "node ID"), ("runner__controller", "a b", "host name"),
            ("runner__joinSecretArn", "", "joinSecretArn"), ("runner__joinSecretArn", "arn:aws:iam::1:role/x", "joinSecretArn"),
            ("tailscale__secretArn", "", "tailnet"), ("dashboard__logins", ["you@example.com"], "no dashboard"),
        ]:
            with self.assertRaisesRegex(Refused, message, msg=key):
                runner(**{key: value})
        with self.assertRaisesRegex(Refused, "node ID"):
            h.validate({**RUNNER, "runner": None})
        with self.assertRaisesRegex(Refused, "not supported"):
            h.validate({**RUNNER, "role": "controller"})
        self.assertNotIn("runner", h.validate(CONFIG), "a host config carries no runner block")


class Render(unittest.TestCase):
    def setUp(self):
        self.files = {k: v[0] for k, v in h.render(runner(), BUDGET).items()}

    def test_the_agent_dials_the_recorded_controller_and_waits_for_an_identity(self):
        agent = self.files["/etc/systemd/system/open-workspaces.service"]
        self.assertIn("ExecStart=/srv/ow/run/agent/bin/ow --data-dir /srv/ow/.ow/runner node-agent "
                      "--controller ${OW_CONTROLLER} --credential /srv/ow/.ow/runner/node.json "
                      "--join /srv/ow/.ow/runner/join.json --ca-cert /srv/ow/.ow/controller-ca.pem", agent)
        self.assertIn("EnvironmentFile=/srv/ow/.ow/controller.env", agent)
        # Either file starts it ("|" conditions are ORed): unenrolled, it stays down quietly.
        self.assertIn("ConditionPathExists=|/srv/ow/.ow/runner/node.json\nConditionPathExists=|/srv/ow/.ow/runner/join.json\n", agent)
        self.assertNotIn("host up", agent, "no controller on a runner")
        self.assertIn("TimeoutStopSec=960", agent, "a stop lets jobs in flight finish")
        for word in ("Requires=", "BindsTo=", "PartOf="):
            self.assertNotIn(word, agent)
        worker = self.files["/etc/systemd/system/open-workspaces-worker.service"]
        self.assertEqual(worker, h.render(h.validate(CONFIG), BUDGET)["/etc/systemd/system/open-workspaces-worker.service"][0],
                         "the same worker unit as a host: upgrades treat both alike")

    def test_no_dashboard_and_backups_without_a_catalog(self):
        self.assertFalse(any("dashboard" in path for path in self.files))
        self.assertEqual(self.files["/etc/open-workspaces/backup.env"],
                         "OW_BACKUP_URL=s3://my-bucket/ow/runners/ow-big\nOW_BACKUP_CATALOG=0\n")
        host = h.render(h.validate(CONFIG), BUDGET)["/etc/open-workspaces/backup.env"][0]
        self.assertEqual(host, "OW_BACKUP_URL=s3://my-bucket/ow/host-1\n", "a host's file is unchanged")
        self.assertIn("/etc/systemd/system/ow-backup.timer", self.files)
        self.assertIn("/etc/systemd/system/open-workspaces-images.service", self.files, "golden images build on runners too")
        self.assertLessEqual(set(self.files), set(h.OWNED))


class Fake:
    """Commands on a runner and on its controller: the secret store, systemd and ow."""

    def __init__(self, root):
        self.root, self.calls, self.secrets, self.nodes = root, [], {}, []
        self.agent_online = True

    def __call__(self, cmd, **kw):
        self.calls.append(cmd)
        name = Path(cmd[0]).name
        if name == "mktemp":
            return str(self.root / f"run-{len(self.calls)}")
        if name == "aws" and cmd[2] == "get-secret-value":
            return self.secrets.get(cmd[cmd.index("--secret-id") + 1], "")
        if name == "aws" and cmd[2] == "put-secret-value":
            source = cmd[cmd.index("--secret-string") + 1]
            self.assertFile = Path(source.removeprefix("file://"))
            self.secrets[cmd[cmd.index("--secret-id") + 1]] = self.assertFile.read_text()
            return ""
        if cmd[:3] == ["systemctl", "restart", h.UNITS["agent"]] and self.agent_online:
            # The agent redeems the join: credential saved, join deleted, then a heartbeat.
            join = json.loads(h.p(h.JOIN).read_text())
            h.p(f"{h.RUNNER}/node.json").write_text(json.dumps({"node": join["node"], "credential": "c" * 64}))
            h.p(h.JOIN).unlink()
            h.p(f"{h.RUNNER}/agent.json").write_text(json.dumps(
                {"online": True, "updated": time.time(), "version": "0.5.0", "worker": {"version": "0.5.0"}}))
            return ""
        if name == "ow" and cmd[1:3] == ["node", "list"]:
            return json.dumps(self.nodes)
        if name == "ow" and "node-join" in cmd:
            out = cmd[cmd.index("--output") + 1]
            h.p(out).write_text(json.dumps({"node": cmd[cmd.index("node-join") + 1], "secret": "s" * 64,
                                            "expires": int(time.time()) + 600,
                                            "controller": cmd[cmd.index("--controller") + 1]}))
            return ""
        return ""


class Enrolment(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name)
        for d in (h.RUNNER, h.CONTROLLER, f"{h.DATA}/tls", "/etc/open-workspaces", "/proc"):
            h.p(d).mkdir(parents=True, exist_ok=True)
        h.p("/proc/meminfo").write_text("MemTotal:       16000000 kB\n")
        h.p(h.HOST_ENV).write_text("OW_CONFIG_PARAMETER=/open-workspaces/x/config\nOW_REGION=us-east-1\n")
        self.fake = Fake(h.ROOT)
        self.real_run = h.run
        h.run = self.fake

    def tearDown(self):
        h.run = self.real_run
        self.tmp.cleanup()

    def controller(self):
        h.write_json(h.CONFIG, h.validate(CONFIG))
        h.write_json(f"{h.DATA}/host.json", {"controller": "https://ow-host.tail1234.ts.net:8790", "node": "ow-host"})
        h.p(f"{h.DATA}/tls/ca.pem").write_text(CA)

    def test_the_controller_mints_into_the_secret_and_keeps_no_copy(self):
        self.controller()
        self.assertEqual(h.invite_runner("ow-big", 49152, 8, 16), 0)
        mint = next(c for c in self.fake.calls if "node-join" in c)
        self.assertEqual(mint[mint.index("--data-dir") + 1], h.CONTROLLER)
        self.assertEqual([mint[mint.index(f) + 1] for f in ("--ttl", "--memory", "--slots", "--cpus", "--controller")],
                         ["600", "49152", "8", "16", "https://ow-host.tail1234.ts.net:8790"])
        put = next(c for c in self.fake.calls if "put-secret-value" in c)
        self.assertEqual(put[put.index("--secret-id") + 1], "open-workspaces/ow-big/join")
        value = json.loads(self.fake.secrets["open-workspaces/ow-big/join"])
        self.assertEqual((value["join"]["node"], value["ca"]), ("ow-big", CA))
        self.assertFalse(any("s" * 64 in " ".join(c) for c in self.fake.calls), "the join never reaches a command line")
        self.assertFalse(self.fake.assertFile.exists(), "the secret file is deleted")
        self.assertEqual(list(h.p(h.DATA).glob("invite-*")), [], "no join left on the controller")

    def test_an_online_node_is_left_alone_and_a_loopback_controller_is_refused(self):
        self.controller()
        self.fake.nodes = [{"id": "ow-big", "online": True}]
        self.assertEqual(h.invite_runner("ow-big", 1024, 2, 2), 0)
        self.assertFalse(any("node-join" in c for c in self.fake.calls))
        self.fake.nodes = []
        h.write_json(f"{h.DATA}/host.json", {"controller": "https://127.0.0.1:8790"})
        with self.assertRaisesRegex(Refused, "no other host can reach"):
            h.invite_runner("ow-big", 1024, 2, 2)
        h.write_json(h.CONFIG, runner())
        with self.assertRaisesRegex(Refused, "controller host"):
            h.invite_runner("ow-big", 1024, 2, 2)

    def test_the_runner_redeems_the_join_once_and_clears_the_secret(self):
        self.controller()
        h.invite_runner("ow-big", 1024, 2, 2)
        self.fake.secrets[JOIN_ARN] = self.fake.secrets.pop("open-workspaces/ow-big/join")
        h.write_json(h.CONFIG, runner())
        self.assertEqual(h.enrol(wait=5), 0)
        self.assertEqual(h.p(h.CA).read_text(), CA)
        self.assertEqual(h.p(h.CONTROLLER_ENV).read_text(), "OW_CONTROLLER=https://ow-host.tail1234.ts.net:8790\n")
        for path in (h.CA, h.CONTROLLER_ENV):
            self.assertEqual(h.p(path).stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(self.fake.secrets[JOIN_ARN])["used"], "ow-big", "the spent join is overwritten")
        self.assertIn(["systemctl", "enable", h.UNITS["agent"]], self.fake.calls)
        self.assertTrue(h.node_online("0.5.0", "0.5.0"))
        self.fake.calls.clear()
        self.assertEqual(h.enrol(wait=5), 0, "enrolled already: nothing to do")
        self.assertEqual(self.fake.calls, [])

    def test_a_missing_expired_or_foreign_join_is_refused_and_nothing_starts(self):
        h.write_json(h.CONFIG, runner())
        with self.assertRaisesRegex(Refused, "no join waiting"):
            h.enrol(wait=5)
        self.fake.secrets[JOIN_ARN] = json.dumps({"used": "ow-big"})
        with self.assertRaisesRegex(Refused, "no join waiting"):
            h.enrol(wait=5)
        join = {"node": "ow-big", "secret": "s" * 64, "expires": int(time.time()) - 1,
                "controller": "https://ow-host.tail1234.ts.net:8790"}
        self.fake.secrets[JOIN_ARN] = json.dumps({"join": join, "ca": CA})
        with self.assertRaisesRegex(Refused, "expired"):
            h.enrol(wait=5)
        for bad in ({"node": "other"}, {"controller": "http://ow-host:8790"}, {"controller": "https://x:1/$(id)"}):
            self.fake.secrets[JOIN_ARN] = json.dumps({"join": {**join, "expires": time.time() + 600, **bad}, "ca": CA})
            with self.assertRaisesRegex(Refused, "not ow-big", msg=bad):
                h.enrol(wait=5)
        self.assertFalse(any(c[:2] == ["systemctl", "restart"] for c in self.fake.calls))
        self.assertFalse(h.p(h.JOIN).exists())

    def test_an_agent_that_never_comes_online_leaves_no_join_behind(self):
        h.write_json(h.CONFIG, runner())
        join = {"node": "ow-big", "secret": "s" * 64, "expires": int(time.time()) + 600,
                "controller": "https://ow-host.tail1234.ts.net:8790"}
        self.fake.secrets[JOIN_ARN] = json.dumps({"join": join, "ca": CA})
        self.fake.agent_online = False
        with self.assertRaisesRegex(RuntimeError, "did not come online"):
            h.enrol(wait=0)
        self.assertFalse(h.p(h.JOIN).exists())
        self.assertIn(["systemctl", "stop", h.UNITS["agent"]], self.fake.calls)
        self.assertIn("join", json.loads(self.fake.secrets[JOIN_ARN]), "the secret is cleared only after success")

    def test_an_expired_join_from_an_interrupted_enrolment_does_not_block_a_retry(self):
        h.write_json(h.CONFIG, runner())
        status = json.dumps({"internet": True})
        real = self.fake.__call__

        def fake(cmd, **kw):
            return status if cmd[:2] == ["/usr/local/bin/ow", "status"] else real(cmd, **kw)

        h.run = fake
        join = {"node": "ow-big", "secret": "s" * 64, "expires": int(time.time()) - 1,
                "controller": "https://ow-host.tail1234.ts.net:8790"}
        h.p(h.JOIN).write_text(json.dumps(join))
        h.runner_ready(time.monotonic() + 5, 5)  # returns: ready for runner-add, not a timeout
        self.assertFalse(h.p(h.JOIN).exists())
        self.assertIn(["systemctl", "stop", h.UNITS["agent"]], self.fake.calls)
        h.p(h.JOIN).write_text("not json")
        h.runner_ready(time.monotonic() + 5, 5)
        self.assertFalse(h.p(h.JOIN).exists(), "an unreadable join is dropped too")
        # A join still in date is the agent's: readiness waits for it (here: times out).
        h.p(h.JOIN).write_text(json.dumps({**join, "expires": int(time.time()) + 600}))
        self.fake.calls.clear()
        with self.assertRaisesRegex(RuntimeError, "not online"):
            h.runner_ready(time.monotonic(), 0)
        self.assertTrue(h.p(h.JOIN).exists())
        self.assertNotIn(["systemctl", "stop", h.UNITS["agent"]], self.fake.calls)

    def test_online_means_a_fresh_accepted_heartbeat_with_the_expected_releases(self):
        h.write_json(h.CONFIG, runner())
        status = h.p(f"{h.RUNNER}/agent.json")
        self.assertFalse(h.node_online())
        status.write_text(json.dumps({"online": True, "updated": time.time(), "version": "0.5.1",
                                      "worker": {"version": "0.5.0"}}))
        self.assertTrue(h.node_online("0.5.1", "0.5.0"))
        self.assertFalse(h.node_online("0.5.0", "0.5.0"), "the old agent's release")
        status.write_text(json.dumps({"online": True, "updated": time.time() - 60, "version": "0.5.1"}))
        self.assertFalse(h.node_online(), "a stale heartbeat")
        status.write_text(json.dumps({"online": False, "updated": time.time()}))
        self.assertFalse(h.node_online())


def release(version, speaks, accepts, changes=("agent",)):
    return {"version": version, "changes": list(changes), "class": 1, "catalog": {"schema": 3, "min_reader": 3},
            "protocol": {"speaks": speaks, "accepts": list(accepts)}, "worker_protocol": {"speaks": 1, "accepts": [1, 1]},
            "in_place_from": "0.5.0", "runtime": {"resumes": ["firecracker-v1.17.0"], "guest_kernel_sha256": "k"}}


class ControllerWindow(unittest.TestCase):
    """Controllers upgrade first: a runner's new agent must speak what its controller accepts."""

    MANIFEST = {"schema": 1, "releases": [
        release("0.7.0", 3, (2, 3)), release("0.6.0", 2, (1, 2)), release("0.5.0", 1, (1, 1), changes=())]}
    AT = {"worker": "0.5.0", "agent": "0.5.0", "dashboard": "0.5.0"}

    def test_the_runner_follows_its_controller(self):
        plan = u.plan(self.MANIFEST, self.AT, "0.6.0", controller="0.6.0")
        self.assertEqual(plan["move"], {"agent": "0.6.0"})
        self.assertEqual(u.plan(self.MANIFEST, self.AT, "0.6.0", controller="0.7.0")["class"], 1,
                         "one protocol behind its controller is accepted")

    def test_a_runner_ahead_of_its_controller_is_refused(self):
        with self.assertRaisesRegex(Refused, r"controller \(0.5.0\) accepts protocol 1..1.*upgrade the controller first"):
            u.plan(self.MANIFEST, self.AT, "0.6.0", controller="0.5.0")
        with self.assertRaisesRegex(Refused, "does not list"):
            u.plan(self.MANIFEST, self.AT, "0.6.0", controller="0.9.0")

    def test_a_runner_has_no_catalog_to_back_up(self):
        manifest = copy.deepcopy(self.MANIFEST)
        manifest["releases"][1]["catalog"] = {"schema": 4, "min_reader": 4}
        self.assertFalse(u.plan(manifest, self.AT, "0.6.0", controller="0.6.0")["catalog_backup"])
        self.assertTrue(u.plan(manifest, self.AT, "0.6.0")["catalog_backup"], "a controller host still backs up")


if __name__ == "__main__":
    unittest.main()
