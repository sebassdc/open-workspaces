"""RunnerPool: a pool runner's derived identity and join request, and the controller's
adapter (runner_pool.py) against a fake AWS: joins, observations, launch, exact removal,
activation preflight and disk cleanup.

Run: python3 -m unittest discover -s deploy/aws/host
"""
import copy
import json
import re
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ow_host as h  # noqa: E402
import runner_pool as rp  # noqa: E402
from ow_host import Refused  # noqa: E402
from test_ow_host import CONFIG  # noqa: E402
from test_runner import CA, Fake  # noqa: E402

PREFIX = "arn:aws:secretsmanager:us-east-1:111122223333:secret:open-workspaces/pool/dev/"
POOL = {"name": "dev", "controller": "ow-host", "group": "open-workspaces-dev", "roleId": "AROAPOOLROLE1",
        "joinSecretPrefix": PREFIX, "hook": "open-workspaces-drain", "instanceHourlyCents": 21, "diskMonthlyCentsPerGib": 8}
POOL_CONFIG = {**copy.deepcopy(CONFIG), "role": "runner", "dashboard": {"logins": []}, "pool": POOL,
               "tailscale": {**CONFIG["tailscale"], "hostname": ""}}
VOL_A, VOL_B = "vol-0aaaaaaaaaaaaaaaa", "vol-0bbbbbbbbbbbbbbbb"
NODE_A, NODE_B = "dev-0aaaaaaaaaaaaaaaa", "dev-0bbbbbbbbbbbbbbbb"


def instance(iid, volume, state="running"):
    return {"InstanceId": iid, "State": {"Name": state},
            "BlockDeviceMappings": [{"DeviceName": "/dev/sda1", "Ebs": {"VolumeId": "vol-0ffffffffffffffff", "DeleteOnTermination": True}},
                                    {"DeviceName": "/dev/sdf", "Ebs": {"VolumeId": volume, "DeleteOnTermination": False}}]}


class FakeAws(Fake):
    """A runner/controller host plus the AWS APIs a pool uses, with denials on demand."""

    def __init__(self, root):
        super().__init__(root)
        self.volume = VOL_A
        self.denied, self.drain, self.tags, self.activities = set(), {}, {}, []
        self.group = {"AutoScalingGroupARN": "arn:aws:autoscaling:us-east-1:111122223333:autoScalingGroup:u:autoScalingGroupName/open-workspaces-dev",
                      "DesiredCapacity": 0, "MaxSize": 3, "MinSize": 0, "Instances": [],
                      "NewInstancesProtectedFromScaleIn": True,
                      "SuspendedProcesses": [{"ProcessName": n} for n in sorted(rp.SUSPENDED)],
                      "InstanceLifecyclePolicy": {"RetentionTriggers": {"TerminateHookAbandon": "retain"}}}
        self.hooks = [{"LifecycleHookName": "open-workspaces-drain", "LifecycleTransition": "autoscaling:EC2_INSTANCE_TERMINATING",
                       "DefaultResult": "ABANDON"}]
        self.instances, self.volumes, self.policies = [], [], []
        self.scaler = {"mode": "shadow", "pools": []}

    def launch(self, iid, volume, state="InService"):
        self.instances.append(instance(iid, volume))
        self.group["Instances"].append({"InstanceId": iid, "LifecycleState": state})
        self.volumes.append({"VolumeId": volume, "Size": 100, "State": "in-use", "CreateTime": "2026-10-19T00:00:00Z",
                             "Attachments": [{"InstanceId": iid}]})

    def member(self, iid):
        return next(m for m in self.group["Instances"] if m["InstanceId"] == iid)

    def __call__(self, cmd, **kw):
        name = Path(cmd[0]).name
        if "--secret-id" in cmd:  # a partial ARN and a name are the same secret
            cmd = list(cmd)
            i = cmd.index("--secret-id") + 1
            cmd[i] = re.sub(r"^arn:[^ ]*:secret:", "", cmd[i])
        if cmd[0] == "findmnt":
            return "/dev/nvme1n1\n" if self.volume else ""
        if cmd[0] == "lsblk":
            return f"vol{self.volume[4:]}\n" if self.volume else ""
        if name == "ow" and "drain" in cmd:
            self.calls.append(cmd)
            node, action = cmd[cmd.index("drain") + 1], cmd[cmd.index("--action") + 1]
            state = self.drain.get(node, "active")
            # begin -> (the workflow empties it) -> retain with evidence -> remove, each once
            step = {("begin", "active"): "empty", ("retain", "empty"): "retained", ("remove", "retained"): "removed"}
            if action != "status":
                if (action, state) not in step or (action != "begin" and "--certificate" not in cmd):
                    raise RuntimeError(f"ow node drain {action} refused in {state}")
                if action == "retain":
                    evidence = json.loads(h.p(cmd[cmd.index("--evidence") + 1]).read_text())
                    assert evidence["node"] == node and evidence["dev_disk_retained"] is True, evidence
                self.drain[node] = step[(action, state)]
                return "{}"
            return json.dumps({"state": state, "certificate": {"token": "t"} if state in ("empty", "retained") else None})
        if name == "ow" and "scaler" in cmd:
            return json.dumps(self.scaler)
        if name == "aws" and cmd[2] == "put-secret-value" and cmd[cmd.index("--secret-id") + 1] not in self.secrets:
            raise RuntimeError("An error occurred (ResourceNotFoundException) when calling PutSecretValue")
        if name != "aws" or cmd[1] not in ("autoscaling", "ec2", "ssm", "secretsmanager", "sts", "iam") \
                or cmd[2] in ("get-secret-value", "put-secret-value"):
            return super().__call__(cmd, **kw)
        self.calls.append(cmd)
        op, arg = cmd[2], lambda flag: cmd[cmd.index(flag) + 1]
        if op in self.denied:
            raise RuntimeError(f"aws {op}: An error occurred (AccessDenied) when calling the {op} operation")
        if op == "get-caller-identity":
            return json.dumps({"Arn": "arn:aws:sts::111122223333:assumed-role/ow-host-host/i-0ctl"})
        if op == "simulate-principal-policy":
            assert arg("--policy-source-arn") == "arn:aws:iam::111122223333:role/ow-host-host"
            resource = arg("--resource-arns")
            assert resource == self.group["AutoScalingGroupARN"] or (
                resource == PREFIX + "dev-probe-AbCdEf" and "ContextKeyName=aws:RequestTag/ow-pool,ContextKeyValues=dev,ContextKeyType=string" in cmd)
            actions = cmd[cmd.index("--action-names") + 1:cmd.index("--resource-arns")]
            return json.dumps({"EvaluationResults": [{"EvalActionName": a, "EvalDecision": "implicitDeny"
                                                      if f"simulate:{a.split(':')[1]}" in self.denied else "allowed"} for a in actions]})
        if op == "get-parameter":
            return json.dumps({"Parameter": {"Value": json.dumps({"pool": POOL})}})
        if op == "describe-auto-scaling-groups":
            return json.dumps({"AutoScalingGroups": [self.group]})
        if op == "describe-lifecycle-hooks":
            return json.dumps({"LifecycleHooks": self.hooks})
        if op == "describe-policies":
            return json.dumps({"ScalingPolicies": self.policies})
        if op == "describe-scaling-activities":
            return json.dumps({"Activities": self.activities})
        if op == "describe-instances":
            return json.dumps({"Reservations": [{"Instances": [i for i in self.instances if i["State"]["Name"] != "terminated"]}]})
        if op == "describe-volumes":
            return json.dumps({"Volumes": self.volumes})
        if op == "set-desired-capacity":
            self.group["DesiredCapacity"] = int(arg("--desired-capacity"))
            return ""
        if op == "terminate-instance-in-auto-scaling-group":
            self.member(arg("--instance-id"))["LifecycleState"] = "Terminating:Wait"
            if "--should-decrement-desired-capacity" in cmd:
                self.group["DesiredCapacity"] -= 1
            return ""
        if op == "complete-lifecycle-action":
            iid = arg("--instance-id")
            if self.member(iid)["LifecycleState"] != "Terminating:Wait":
                raise RuntimeError("ValidationError: No active Lifecycle Action found")
            self.group["Instances"] = [m for m in self.group["Instances"] if m["InstanceId"] != iid]
            next(i for i in self.instances if i["InstanceId"] == iid)["State"]["Name"] = "terminated"
            for v in self.volumes:  # the data volume stays, detached
                if v["Attachments"] and v["Attachments"][0]["InstanceId"] == iid:
                    v.update(State="available", Attachments=[])
            return ""
        if op == "terminate-instances":
            next(i for i in self.instances if i["InstanceId"] == arg("--instance-ids"))["State"]["Name"] = "terminated"
            return ""
        if op == "delete-volume":
            self.volumes = [v for v in self.volumes if v["VolumeId"] != arg("--volume-id")]
            return ""
        if op == "delete-secret":
            secret = arg("--secret-id")
            if secret not in self.secrets:
                raise RuntimeError("An error occurred (ResourceNotFoundException) when calling DeleteSecret")
            del self.secrets[secret]
            return ""
        if op == "describe-secret":
            secret = arg("--secret-id")
            if secret not in self.secrets:
                raise RuntimeError("An error occurred (ResourceNotFoundException) when calling DescribeSecret")
            return json.dumps({"Tags": [{"Key": k, "Value": v} for k, v in self.tags[secret].items()]})
        if op in ("create-secret", "tag-resource"):
            secret = arg("--name" if op == "create-secret" else "--secret-id")
            tags = cmd[cmd.index("--tags") + 1:cmd.index("--region")]
            self.tags[secret] = dict(t.removeprefix("Key=").split(",Value=") for t in tags)
            if op == "create-secret":
                self.secrets[secret] = arg("--secret-string")
            return ""
        raise AssertionError(f"unexpected call {cmd}")


class PoolCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        h.ROOT = Path(self.tmp.name)
        for d in (h.RUNNER, h.CONTROLLER, f"{h.DATA}/tls", "/etc/open-workspaces", "/proc"):
            h.p(d).mkdir(parents=True, exist_ok=True)
        h.p("/proc/meminfo").write_text("MemTotal:       16000000 kB\n")
        h.p(h.HOST_ENV).write_text("OW_CONFIG_PARAMETER=/open-workspaces/dev/config\nOW_REGION=us-east-1\n")
        h.write_json(f"{h.CONTROLLER}/limits.json", {"pool": {"hours": 24}})
        self.aws = FakeAws(h.ROOT)
        self.real = h.run, time.sleep
        h.run = self.aws
        time.sleep = lambda s: None

    def tearDown(self):
        h.run, time.sleep = self.real
        self.tmp.cleanup()

    def as_controller(self):
        h.write_json(h.CONFIG, h.validate(CONFIG))
        h.write_json(f"{h.DATA}/host.json", {"controller": "https://ow-host.tail1234.ts.net:8790", "node": "ow-host"})
        h.p(f"{h.DATA}/tls/ca.pem").write_text(CA)

    def observed(self):
        return json.loads(h.p(rp.OBSERVATIONS).read_text())


class PoolConfig(PoolCase):
    def test_a_pool_runner_is_named_by_its_data_volume(self):
        c = h.validate(POOL_CONFIG)
        self.assertEqual(c["runner"], {"node": NODE_A, "controller": "ow-host", "joinSecretArn": PREFIX + NODE_A})
        self.assertEqual(c["tailscale"]["hostname"], NODE_A)
        self.assertEqual(c["role"], "runner")
        self.assertEqual(h.validate(c)["runner"], c["runner"], "a stored config validates again")
        self.aws.volume = VOL_B  # the same config on another volume is another runner
        self.assertEqual(h.validate(POOL_CONFIG)["runner"]["node"], NODE_B)
        self.aws.volume = None
        with self.assertRaisesRegex(Refused, "EBS data volume"):
            h.validate(POOL_CONFIG)

    def test_an_invalid_pool_block_is_refused(self):
        for change in [{"name": "Dev"}, {"group": "other"}, {"roleId": "AIDAX"}, {"hook": "a b"},
                       {"joinSecretPrefix": PREFIX.replace("/dev/", "/other/")}, {"instanceHourlyCents": 0},
                       {"diskMonthlyCentsPerGib": "8"}, {"extra": 1}]:
            with self.assertRaisesRegex(Refused, "invalid pool", msg=change):
                h.validate({**POOL_CONFIG, "pool": {**POOL, **change}})


class Joins(PoolCase):
    def test_the_controller_and_a_new_instance_meet_through_a_secret_only_that_instance_reads(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        rp.sync(["dev"])
        secret = "open-workspaces/pool/dev/" + NODE_A
        self.assertEqual(self.aws.tags[secret], {"ow-pool": "dev", "ow-session": "AROAPOOLROLE1:i-0a"})
        self.assertEqual(json.loads(self.aws.secrets[secret]), {"pending": NODE_A}, "no join before the runner asks")
        controller_config = h.read_json(h.CONFIG)

        def controller_turn(_):  # the instance waits; meanwhile the controller syncs
            runner_config = h.read_json(h.CONFIG)
            h.write_json(h.CONFIG, controller_config)
            rp.sync(["dev"])
            h.write_json(h.CONFIG, runner_config)
        h.write_json(h.CONFIG, h.validate(POOL_CONFIG))
        time.sleep = controller_turn
        self.assertEqual(h.enrol(wait=60), 0)
        mint = next(c for c in self.aws.calls if "node-join" in c)
        self.assertEqual(mint[mint.index("node-join") + 1], NODE_A)
        budget = h.budgets(h.validate(POOL_CONFIG))
        self.assertEqual([mint[mint.index(f) + 1] for f in ("--memory", "--slots", "--cpus")],
                         [str(int(budget[k])) for k in ("memory", "slots", "cpus")], "the budget the runner asked for")
        self.assertEqual(json.loads(self.aws.secrets[secret])["used"], NODE_A)
        self.assertEqual(h.p(h.CA).read_text(), CA)

    def test_a_join_never_redeemed_is_answered_again_and_one_failure_does_not_stop_sync(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        self.aws.launch("i-0b", VOL_B)
        self.aws.nodes = [{"id": NODE_A, "online": False, "revoked": False, "last_seen_unix_s": 0, "capabilities": {}}]  # minted, never used
        secret = "open-workspaces/pool/dev/"
        self.aws.denied = {"create-secret"}  # a new secret's rights still settling
        rp.sync(["dev"])
        self.aws.denied = set()
        rp.sync(["dev"])
        self.assertEqual(set(self.aws.tags), {secret + NODE_A, secret + NODE_B}, "both answered once rights settle")
        self.aws.secrets[secret + NODE_A] = json.dumps({"ready": NODE_A, "budget": {"memory": 1024, "slots": 1, "cpus": 1}})
        rp.sync(["dev"])
        self.assertEqual(json.loads(self.aws.secrets[secret + NODE_A])["join"]["node"], NODE_A)

    def test_a_volume_salvaged_onto_a_new_instance_moves_its_secret_there(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        rp.sync(["dev"])
        secret = "open-workspaces/pool/dev/" + NODE_A
        self.aws.instances[0]["InstanceId"] = self.aws.group["Instances"][0]["InstanceId"] = "i-0c"
        rp.sync(["dev"])
        self.assertEqual(self.aws.tags[secret]["ow-session"], "AROAPOOLROLE1:i-0c")
        self.assertEqual(json.loads(self.aws.secrets[secret]), {"pending": NODE_A})

    def test_a_forged_or_absent_budget_gets_no_join_and_an_enrolled_runner_no_secret(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        rp.sync(["dev"])
        secret = "open-workspaces/pool/dev/" + NODE_A
        for bad in ({"ready": NODE_A}, {"ready": NODE_A, "budget": {"memory": 0, "slots": 1, "cpus": 1}},
                    {"ready": NODE_B, "budget": {"memory": 1024, "slots": 1, "cpus": 1}}, ["x"]):
            self.aws.secrets[secret] = json.dumps(bad)
            rp.sync(["dev"])
            self.assertFalse(any("node-join" in c for c in self.aws.calls), bad)
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0b", VOL_B, state="Pending")
        before = len(self.aws.secrets)
        rp.sync(["dev"])
        self.assertEqual(len(self.aws.secrets), before, "pending and enrolled instances get no new secret")

    def test_the_runner_asks_once_and_gives_up_at_its_deadline(self):
        h.write_json(h.CONFIG, h.validate(POOL_CONFIG))
        self.aws.secrets["open-workspaces/pool/dev/" + NODE_A] = json.dumps({"pending": NODE_A})
        clock = iter(range(0, 10000, 100))
        real = time.monotonic
        time.monotonic = lambda: next(clock)
        try:
            with self.assertRaisesRegex(Refused, "no join from ow-host"):
                h.enrol(wait=1000)
        finally:
            time.monotonic = real
        request = json.loads(self.aws.secrets["open-workspaces/pool/dev/" + NODE_A])
        self.assertEqual(request["ready"], NODE_A)
        self.assertEqual(set(request["budget"]), {"memory", "slots", "cpus"})
        writes = [c for c in self.aws.calls if "put-secret-value" in c]
        self.assertEqual(len(writes), 1, "one request, not one per poll")


class Observe(PoolCase):
    def test_every_instance_and_disk_counts_with_its_state(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}, {"id": "ow-host", "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0a", VOL_A)
        self.aws.launch("i-0b", VOL_B, state="Pending")
        self.aws.launch("i-0r", "vol-0cccccccccccccccc", state="Terminating:Retained")
        self.aws.instances.append(instance("i-0o", "vol-0dddddddddddddddd"))  # tagged, outside the group
        self.aws.volumes.append({"VolumeId": "vol-0eeeeeeeeeeeeeeee", "Size": 50, "State": "available",
                                 "CreateTime": "2026-10-01T00:00:00Z", "Attachments": []})
        rp.sync(["dev"])
        o = self.observed()
        self.assertEqual(h.p(rp.OBSERVATIONS).stat().st_mode & 0o777, 0o600)
        self.assertEqual({i["id"]: (i["state"], i["node"]) for i in o["instances"]},
                         {"i-0a": ("ready", NODE_A), "i-0b": ("pending", None), "i-0r": ("retained", None), "i-0o": ("orphaned", None)})
        self.assertTrue(all(i["hourly_cents"] == 21 and i["pool"] == "dev" for i in o["instances"]))
        self.assertEqual(len(o["disks"]), 4, "the orphaned instance's disk is not tagged here; the detached one is")
        detached = next(d for d in o["disks"] if d["id"] == "vol-0eeeeeeeeeeeeeeee")
        self.assertEqual((detached["logical_bytes"], detached["hourly_cents"]), (50 * 2**30, 1))
        self.assertEqual((o["hours"], o["complete"], o["retention_ok"], o["outcome_resolved"]), (24, True, True, True))
        self.assertEqual(set(o), {"observed_at", "hours", "complete", "retention_ok", "outcome_resolved", "instances", "disks", "receipts"})

    def test_a_missing_retention_policy_or_unresolved_outcome_is_reported(self):
        self.as_controller()
        self.aws.group["InstanceLifecyclePolicy"] = {}
        rp.sync(["dev"], hours=24)
        self.assertFalse(self.observed()["retention_ok"])
        self.aws.group["InstanceLifecyclePolicy"] = {"RetentionTriggers": {"TerminateHookAbandon": "retain"}}
        self.aws.launch("i-0a", VOL_A, state="Terminating:Wait")
        rp.sync(["dev"], hours=24)
        self.assertEqual((self.observed()["retention_ok"], self.observed()["outcome_resolved"]), (True, False))
        self.aws.group["Instances"] = []
        self.aws.activities = [{"StatusCode": "Failed", "StartTime": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())}]
        rp.sync(["dev"], hours=24)
        self.assertFalse(self.observed()["outcome_resolved"], "a failed launch is unresolved")
        self.aws.group["Instances"] = [{"InstanceId": "i-0a", "LifecycleState": "Mystery"}]
        self.aws.activities = []
        rp.sync(["dev"], hours=24)
        self.assertFalse(self.observed()["complete"])

    def test_fixed_hosts_count_and_a_failed_describe_writes_nothing(self):
        self.as_controller()
        self.aws.nodes = [{"id": "ow-host", "online": True, "capabilities": {"vcpus": 4}}]
        host = instance("i-0h", "vol-0hhhhhhhhhhhhhhhh".replace("h", "1"))
        self.aws.instances.append(host)
        orig = self.aws.__call__

        def named(cmd, **kw):  # the tag Name lookup finds only the host
            if Path(cmd[0]).name == "aws" and cmd[2] == "describe-instances" and "Name=tag:Name,Values=ow-host" in cmd:
                self.aws.calls.append(cmd)
                return json.dumps({"Reservations": [{"Instances": [host]}]})
            if Path(cmd[0]).name == "aws" and cmd[2] == "describe-volumes" and "Name=attachment.instance-id,Values=i-0h" in cmd:
                return json.dumps({"Volumes": [{"VolumeId": "vol-0host", "Size": 100}]})
            return orig(cmd, **kw)
        h.run = named
        self.aws.instances.remove(host)
        rp.sync(["dev"], fixed=["ow-host=ow-host:30"])
        o = self.observed()
        self.assertIn({"id": "i-0h", "pool": "fixed", "node": "ow-host", "state": "ready", "action": None, "hourly_cents": 30}, o["instances"])
        self.assertIn("vol-0host", [d["id"] for d in o["disks"]])
        before = h.p(rp.OBSERVATIONS).read_text()
        self.aws.denied.add("describe-volumes")
        with self.assertRaisesRegex(RuntimeError, "AccessDenied"):
            rp.sync(["dev"], fixed=["ow-host=ow-host:30"])
        self.assertEqual(h.p(rp.OBSERVATIONS).read_text(), before)
        for bad in ("ow-host", "ow-host=Bad Name:1", "x=ow-host:abc"):
            with self.assertRaisesRegex(Refused, "NODE=NAME:CENTS"):
                rp.fixed_instances([bad], {}, POOL)


class Actions(PoolCase):
    def test_launch_is_compare_and_set_and_bounded(self):
        self.as_controller()
        with self.assertRaisesRegex(Refused, "wants 0, not 1"):
            rp.launch("dev", 1)
        rp.launch("dev", 0)
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        rp.launch("dev", 0)  # a lost reply retried: nothing more
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        self.aws.group["DesiredCapacity"] = 3
        with self.assertRaisesRegex(Refused, "maximum 3"):
            rp.launch("dev", 3)
        self.aws.group["DesiredCapacity"] = 1
        self.aws.policies = [{"PolicyName": "target-tracking"}]
        with self.assertRaisesRegex(Refused, "scaling policies"):
            rp.launch("dev", 1)
        self.aws.policies = []
        self.aws.launch("i-0a", VOL_A, state="Terminating:Wait")
        with self.assertRaisesRegex(Refused, "unresolved"):
            rp.launch("dev", 1)
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)

    def test_remove_takes_exactly_the_drained_instance_and_keeps_its_disk(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": False, "capabilities": {"vcpus": 4}}, {"id": NODE_B, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0a", VOL_A)
        self.aws.launch("i-0b", VOL_B)
        self.aws.group["DesiredCapacity"] = 2
        with self.assertRaisesRegex(Refused, "not drained and removed"):
            rp.remove("dev", node=NODE_A)
        self.assertFalse(any("terminate-instance-in-auto-scaling-group" in c for c in self.aws.calls))
        self.aws.drain[NODE_A] = "removed"
        rp.remove("dev", node=NODE_A)
        terminate = [c for c in self.aws.calls if "terminate-instance-in-auto-scaling-group" in c]
        self.assertEqual([c[c.index("--instance-id") + 1] for c in terminate], ["i-0a"])
        self.assertIn("--should-decrement-desired-capacity", terminate[0])
        completes = [c for c in self.aws.calls if "complete-lifecycle-action" in c]
        self.assertEqual([(c[c.index("--instance-id") + 1], c[c.index("--lifecycle-action-result") + 1]) for c in completes],
                         [("i-0a", "CONTINUE")])
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        self.assertEqual([m["InstanceId"] for m in self.aws.group["Instances"]], ["i-0b"], "the other runner is untouched")
        self.assertEqual(next(v for v in self.aws.volumes if v["VolumeId"] == VOL_A)["State"], "available", "dev disk kept")

    def test_a_termination_the_controller_did_not_start_is_never_completed(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.drain[NODE_A] = "removed"
        self.aws.launch("i-0a", VOL_A, state="Terminating:Wait")  # someone else's termination
        rp.sync(["dev"])
        self.assertFalse(any("complete-lifecycle-action" in c for c in self.aws.calls), "left to abandon: retained")
        with self.assertRaisesRegex(Refused, "did not start"):
            rp.remove("dev", node=NODE_A)
        # A runner that stopped being removable after its removal began is not completed either.
        self.aws.member("i-0a")["LifecycleState"] = "InService"
        rp.remove("dev", node=NODE_A)
        self.aws.launch("i-0b", VOL_B)
        self.aws.nodes.append({"id": NODE_B, "online": True, "capabilities": {"vcpus": 4}})
        h.write_json(rp.REMOVALS, {"i-0b": {"node": NODE_B, "enrolled": True}})
        self.aws.member("i-0b")["LifecycleState"] = "Terminating:Wait"
        self.aws.drain[NODE_B] = "active"
        self.aws.calls.clear()
        rp.sync(["dev"])
        self.assertFalse(any("complete-lifecycle-action" in c for c in self.aws.calls))

    def test_removal_needs_a_resolvable_runner_and_rechecks_it_before_completing(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        self.aws.instances[0]["BlockDeviceMappings"] = self.aws.instances[0]["BlockDeviceMappings"][:1]
        with self.assertRaisesRegex(Refused, "cannot be proven empty"):
            rp.remove("dev", instance="i-0a")
        self.aws.launch("i-0b", VOL_B)
        h.write_json(rp.REMOVALS, {"i-0b": {"node": NODE_B, "enrolled": False}})  # never enrolled when removal began
        self.aws.member("i-0b")["LifecycleState"] = "Terminating:Wait"
        self.aws.nodes = [{"id": NODE_B, "online": True, "capabilities": {"vcpus": 4}}]  # ... but it joined since
        rp.sync(["dev"])
        self.assertFalse(any("complete-lifecycle-action" in c for c in self.aws.calls), "abandoned and retained instead")

    def test_launch_rereads_desired_capacity_right_before_writing(self):
        self.as_controller()
        real = rp.describe
        reads = []

        def racing(pool):
            out = real(pool)
            reads.append(1)
            if len(reads) == 2:
                self.aws.group["DesiredCapacity"] = 1  # another launch landed in between
                out = real(pool)
            return out
        rp.describe = racing
        try:
            with self.assertRaisesRegex(Refused, "changed while launching"):
                rp.launch("dev", 0)
        finally:
            rp.describe = real
        self.assertFalse(any("set-desired-capacity" in c for c in self.aws.calls))

    def test_a_retained_instance_is_removed_without_lowering_desired_capacity(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A, state="Terminating:Retained")
        self.aws.group["DesiredCapacity"] = 1  # its replacement's
        rp.remove("dev", instance="i-0a")  # never enrolled
        terminate = next(c for c in self.aws.calls if "terminate-instance-in-auto-scaling-group" in c)
        self.assertIn("--no-should-decrement-desired-capacity", terminate)
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        with self.assertRaisesRegex(Refused, "found 0"):
            rp.remove("dev", instance="i-0zz")
        with self.assertRaisesRegex(Refused, "not a runner"):
            self.aws.launch("i-0b", VOL_B)
            rp.remove("dev", node=NODE_B)

    def test_missing_permission_or_shape_refuses_activation(self):
        self.as_controller()
        self.assertEqual(rp.check("dev"), 0)
        recorded = json.loads(h.p(rp.PROVIDER_CHECK).read_text())["pools"]["dev"]
        self.assertLessEqual(abs(recorded - time.time()), 5, "a pass is recorded for live mode")
        h.p(rp.PROVIDER_CHECK).unlink()
        for op, right in [("simulate:SetDesiredCapacity", "SetDesiredCapacity"), ("simulate:CompleteLifecycleAction", "CompleteLifecycleAction"),
                          ("simulate:TerminateInstanceInAutoScalingGroup", "TerminateInstanceInAutoScalingGroup"),
                          ("simulate:CreateSecret", "CreateSecret"), ("simulate:PutSecretValue", "PutSecretValue")]:
            self.aws.denied = {op}
            with self.assertRaisesRegex(Refused, f"activation refused.*missing permission: {right}"):
                rp.check("dev")
        self.aws.denied = {"simulate-principal-policy"}
        with self.assertRaisesRegex(Refused, "cannot verify the controller's rights"):
            rp.check("dev")
        self.assertFalse(any(op in c for c in self.aws.calls for op in ("set-desired-capacity", "complete-lifecycle-action",
                                                                         "terminate-instance-in-auto-scaling-group")), "check writes nothing")
        self.assertNotIn("dev", h.read_json(rp.PROVIDER_CHECK)["pools"], "a refused check records no pass")
        self.aws.denied = set()
        self.aws.hooks[0]["DefaultResult"] = "CONTINUE"
        with self.assertRaisesRegex(Refused, "abandons by default"):
            rp.check("dev")
        self.aws.hooks[0]["DefaultResult"] = "ABANDON"
        self.aws.group["SuspendedProcesses"] = []
        with self.assertRaisesRegex(Refused, "ReplaceUnhealthy"):
            rp.check("dev")

    def test_only_a_detached_disk_of_a_removed_or_never_enrolled_runner_is_deleted(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": False, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0a", VOL_A)
        with self.assertRaisesRegex(Refused, "in-use"):
            rp.delete_disk("dev", VOL_A)
        self.aws.volumes[0].update(State="available", Attachments=[])
        with self.assertRaisesRegex(Refused, "runner active: salvage it first"):
            rp.delete_disk("dev", VOL_A)
        self.assertFalse(any("delete-volume" in c for c in self.aws.calls))
        drain = next(c for c in self.aws.calls if "drain" in c)
        self.assertEqual(drain[drain.index("--data-dir") + 1], h.DATA, "ow node drain takes the base directory")
        self.aws.nodes = [{"id": NODE_A, "online": False, "last_seen_unix_s": 0, "capabilities": {"vcpus": 4}}]
        with self.assertRaisesRegex(Refused, "salvage it first"):  # offline runners report last_seen 0
            rp.delete_disk("dev", VOL_A)
        self.aws.drain[NODE_A] = "removed"
        listed = rp.disks("dev", show=False)
        self.assertEqual((listed[0]["runner"], listed[0]["deletable"], listed[0]["monthly_cents"]), ("removed", True, 800))
        self.aws.secrets["open-workspaces/pool/dev/" + NODE_A] = json.dumps({"used": NODE_A})
        rp.delete_disk("dev", VOL_A)
        self.assertEqual(self.aws.volumes, [])
        self.assertNotIn("open-workspaces/pool/dev/" + NODE_A, self.aws.secrets, "its spent join secret goes too")
        self.aws.volumes = [{"VolumeId": VOL_B, "Size": 40, "State": "available", "CreateTime": "x", "Attachments": []}]
        self.aws.nodes = [{"id": NODE_B, "online": False, "last_seen_unix_s": 0, "capabilities": {}}]
        self.assertEqual(rp.disks("dev", show=False)[0]["runner"], "join never used")
        rp.delete_disk("dev", VOL_B)
        self.assertEqual(self.aws.volumes, [])
        with self.assertRaisesRegex(Refused, "not a volume ID"):
            rp.delete_disk("dev", "vol-1; rm -rf /")

    def test_the_adapter_runs_only_on_a_controller_host(self):
        h.write_json(h.CONFIG, h.validate(POOL_CONFIG))
        with self.assertRaisesRegex(Refused, "controller host"):
            rp.main(["disks", "dev"])


if __name__ == "__main__":
    unittest.main()


class Executor(PoolCase):
    def live(self, kind, target, phase):
        self.aws.scaler = {"mode": "live", "error": None, "pools": [{"stale": False, "execution_allowed": True, "state": {"action": {
            "kind": kind, "id": "dev-7", "target": target, "phase": phase, "deadline": 9, "reservation": {}}}}]}

    def test_a_live_launch_raises_desired_once_and_reports_the_joined_instance(self):
        self.as_controller()
        self.live("launch", "preset", "launching")
        rp.sync(["dev"])
        rp.sync(["dev"])  # a retry or restart never launches twice
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        self.assertEqual(self.observed()["receipts"]["dev"], {"action": "dev-7", "outcome": "pending", "node": None})
        self.aws.launch("i-0a", VOL_A)
        rp.sync(["dev"])
        o = self.observed()
        self.assertEqual(o["receipts"]["dev"]["outcome"], "pending", "not joined until enrolled and online")
        self.assertEqual(next(i for i in o["instances"] if i["id"] == "i-0a")["action"], "dev-7")
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        rp.sync(["dev"])
        self.assertEqual(self.observed()["receipts"]["dev"], {"action": "dev-7", "outcome": "joined", "node": NODE_A})
        self.assertEqual(self.aws.group["DesiredCapacity"], 1)
        self.aws.launch("i-0b", VOL_B)  # a second instance for one launch is never claimed
        rp.sync(["dev"])
        self.assertEqual(self.observed()["receipts"]["dev"]["outcome"], "unknown")

    def test_shadow_or_uncertain_actions_are_never_executed(self):
        self.as_controller()
        for phase in ("shadow_launch", "uncertain"):
            self.live("launch", "preset", phase)
            rp.sync(["dev"])
            self.assertEqual(self.aws.group["DesiredCapacity"], 0, phase)
            self.assertEqual(self.observed()["receipts"], {})

    def test_stored_live_actions_require_current_healthy_live_status(self):
        self.as_controller()
        for kind, target, phase in (("launch", "preset", "launching"), ("drain", NODE_A, "draining")):
            for fault in ("shadow", "error", "stale", "safety_hold", "missing_mode", "missing_error", "missing_stale", "missing_permission"):
                with self.subTest(kind=kind, fault=fault):
                    self.live(kind, target, phase)
                    status = self.aws.scaler
                    if fault == "shadow":
                        status["mode"] = "shadow"
                    elif fault == "error":
                        status["error"] = "no passing provider check"
                    elif fault == "stale":
                        status["pools"][0]["stale"] = True
                    elif fault == "missing_stale":
                        del status["pools"][0]["stale"]
                    elif fault == "safety_hold":
                        status["pools"][0]["execution_allowed"] = False
                        status["pools"][0]["reason"] = "provider inventory missing, incomplete or stale; actions disabled"
                    elif fault == "missing_permission":
                        del status["pools"][0]["execution_allowed"]
                    else:
                        del status[fault.removeprefix("missing_")]
                    self.aws.calls.clear()
                    rp.sync(["dev"])
                    self.assertEqual(self.aws.group["DesiredCapacity"], 0)
                    self.assertEqual(self.observed()["receipts"], {})
                    self.assertFalse(h.read_json(rp.ACTIONS), "no executor journal is written")
                    self.assertFalse(any("drain" in c for c in self.aws.calls))
                    self.assertFalse(any("set-desired-capacity" in c for c in self.aws.calls))

    def test_paused_sync_does_not_complete_a_journaled_termination(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.drain[NODE_A] = "removed"
        self.aws.launch("i-0a", VOL_A, state="Terminating:Wait")
        h.write_json(rp.REMOVALS, {"i-0a": {"node": NODE_A, "enrolled": True}})
        for fault in ("shadow", "error", "safety_hold"):
            self.live("drain", NODE_A, "draining")
            if fault == "shadow":
                self.aws.scaler = {"mode": "shadow", "pools": []}
            elif fault == "error":
                self.aws.scaler["error"] = "activation refused"
            else:
                self.aws.scaler["pools"][0]["execution_allowed"] = False
            self.aws.calls.clear()
            rp.sync(["dev"])
            self.assertEqual(self.aws.member("i-0a")["LifecycleState"], "Terminating:Wait", fault)
            self.assertFalse(any("complete-lifecycle-action" in c for c in self.aws.calls), fault)

    def test_failed_preflight_revokes_only_its_previous_pass(self):
        self.as_controller()
        for failure in ("simulate:PutSecretValue", "describe-auto-scaling-groups", "simulate-principal-policy"):
            self.aws.denied = set()
            rp.check("dev")
            checks = h.read_json(rp.PROVIDER_CHECK)
            checks["pools"]["other"] = 123
            h.write_json(rp.PROVIDER_CHECK, checks)
            self.aws.denied = {failure}
            with self.assertRaises((Refused, RuntimeError)):
                rp.check("dev")
            self.assertEqual(h.read_json(rp.PROVIDER_CHECK)["pools"], {"other": 123})

    def test_a_live_drain_seals_retains_removes_and_reports_absent(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0a", VOL_A)
        self.aws.group["DesiredCapacity"] = 1
        self.live("drain", NODE_A, "draining")
        for _ in range(4):
            rp.sync(["dev"])
            self.assertEqual(self.observed()["receipts"]["dev"]["outcome"], "pending")
        self.assertEqual(self.aws.drain[NODE_A], "removed")
        rp.sync(["dev"])
        self.assertEqual(self.observed()["receipts"]["dev"], {"action": "dev-7", "outcome": "absent", "node": None})
        self.assertEqual((self.aws.group["DesiredCapacity"], self.aws.group["Instances"]), (0, []))
        self.assertEqual(next(v for v in self.aws.volumes if v["VolumeId"] == VOL_A)["State"], "available")

    def test_drain_refuses_retention_evidence_when_the_disk_would_be_deleted(self):
        self.as_controller()
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        self.aws.launch("i-0a", VOL_A)
        self.aws.instances[0]["BlockDeviceMappings"][1]["Ebs"]["DeleteOnTermination"] = True
        self.aws.drain[NODE_A] = "empty"
        self.live("drain", NODE_A, "draining")
        rp.sync(["dev"])
        self.assertEqual(self.observed()["receipts"]["dev"]["outcome"], "unknown")
        self.assertEqual(self.aws.drain[NODE_A], "empty")


class Faults(PoolCase):
    def test_an_expired_join_is_renewed_on_request(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        rp.sync(["dev"])
        secret = "open-workspaces/pool/dev/" + NODE_A
        self.aws.secrets[secret] = json.dumps({"ready": NODE_A, "budget": {"memory": 1024, "slots": 1, "cpus": 1}})
        rp.sync(["dev"])
        value = json.loads(self.aws.secrets[secret])
        value["join"]["expires"] = int(time.time()) - 1
        self.aws.secrets[secret] = json.dumps(value)
        rp.sync(["dev"])
        self.assertEqual(json.loads(self.aws.secrets[secret]), {"pending": NODE_A}, "reset for a new request")
        controller = h.read_json(h.CONFIG)
        h.write_json(h.CONFIG, h.validate(POOL_CONFIG))
        self.aws.secrets[secret] = json.dumps(value)  # the runner still sees the expired join

        def controller_turn(_):
            mine = h.read_json(h.CONFIG)
            h.write_json(h.CONFIG, controller)
            rp.sync(["dev"])
            h.write_json(h.CONFIG, mine)
        time.sleep = controller_turn
        self.assertEqual(h.enrol(wait=60), 0)
        self.assertEqual(sum("node-join" in c for c in self.aws.calls), 2, "a fresh join replaced the expired one")

    def test_removal_refuses_when_retention_was_lost(self):
        self.as_controller()
        self.aws.launch("i-0a", VOL_A)
        self.aws.group["InstanceLifecyclePolicy"] = {}
        with self.assertRaisesRegex(Refused, "removal refused.*lifecycle policy"):
            rp.remove("dev", instance="i-0a")
        self.assertFalse(any("terminate-instance-in-auto-scaling-group" in c for c in self.aws.calls))

    def test_a_salvage_replacement_outside_the_group_can_be_removed(self):
        self.as_controller()
        self.aws.instances.append(instance("i-0s", VOL_A))  # tagged, not a member
        self.aws.volumes.append({"VolumeId": VOL_A, "Size": 40, "State": "in-use", "CreateTime": "x",
                                 "Attachments": [{"InstanceId": "i-0s"}]})
        self.aws.nodes = [{"id": NODE_A, "online": True, "capabilities": {"vcpus": 4}}]
        with self.assertRaisesRegex(Refused, "not drained and removed"):
            rp.remove("dev", node=NODE_A)
        self.aws.drain[NODE_A] = "removed"
        self.assertEqual(rp.main(["remove", "dev", "--node", NODE_A]), 0)
        self.assertEqual(self.aws.instances[0]["State"]["Name"], "terminated")
        self.assertFalse(any("terminate-instance-in-auto-scaling-group" in c for c in self.aws.calls))
        self.assertTrue(rp.remove("dev", node=NODE_A), "a rerun after success is done")
