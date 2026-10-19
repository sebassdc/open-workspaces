"""ow-host pool: the controller host's adapter for a RunnerPool (deploy/aws/pool.ts).

  pool check POOL               activation preflight: the group's shape and this host's
                                rights; any gap refuses activation (exit 1)
  pool sync POOL.. [--fixed NODE=NAME:CENTS].. [--hours H]
                                answer join requests, complete this controller's own
                                removals, and write scaler-observations.json for every
                                pool named (one global snapshot)
  pool launch POOL --expect N   desired capacity N -> N+1, once (compare and set)
  pool remove POOL (--node NODE | --instance ID)
                                remove exactly one instance whose runner was drained empty
                                and removed (ow node drain), or that never enrolled
  pool disks POOL               every billable disk of the pool, with its runner's state
  pool delete-disk POOL VOLUME  delete one detached disk whose runner was drained empty
                                and removed (and its join secret); never by age or automatically

Every AWS call runs with this host's role, which the RunnerPool scopes to its group,
its join secrets and its tagged disks. Nothing here runs on a timer.
"""
import argparse
import json
import calendar
import math
import re
import time

import ow_host as h
from ow_host import Refused

OBSERVATIONS = f"{h.CONTROLLER}/scaler-observations.json"
# Terminations this controller asked for: instance -> {"node", "enrolled"}.
REMOVALS = f"{h.CONTROLLER}/pool-removals.json"
SUSPENDED = {"AZRebalance", "AlarmNotification", "InstanceRefresh", "ReplaceUnhealthy", "ScheduledActions"}
GROUP_ACTIONS = ("SetDesiredCapacity", "TerminateInstanceInAutoScalingGroup", "CompleteLifecycleAction")
INSTANCE = re.compile(r"^i-[0-9a-f]{8,17}$")
VOLUME = re.compile(r"^vol-([0-9a-f]{17})$")
LIVE = "pending,running,stopping,stopped"


def aws(*args):
    out = h.run(["/usr/local/bin/aws", *args, "--region", h.region(), "--output", "json"], timeout=120)
    return json.loads(out) if out.strip() else {}


def denied(error):
    return any(code in str(error) for code in ("AccessDenied", "UnauthorizedOperation", "not authorized"))


def load(name):
    if not h.POOL.match(name):
        raise Refused(f"{name!r} is not a pool name")
    raw = aws("ssm", "get-parameter", "--name", f"/open-workspaces/{name}/config")["Parameter"]["Value"]
    pool = h.check_pool((json.loads(raw) or {}).get("pool"))
    if pool["name"] != name:
        raise Refused(f"/open-workspaces/{name}/config is pool {pool['name']!r}")
    return pool


def node_of(pool, instance):
    for mapping in instance.get("BlockDeviceMappings") or []:
        match = VOLUME.match(str((mapping.get("Ebs") or {}).get("VolumeId", "")))
        if mapping.get("DeviceName") == "/dev/sdf" and match:
            return f"{pool['name']}-{match.group(1)}"
    return None


def catalog():
    """The controller's runners that ever connected, by ID, revoked ones included. Offline
    runners keep their reported capabilities; a join minted but never redeemed has none
    (and may be asked for again)."""
    return {entry["id"]: entry for entry in h.ow("node", "list", timeout=30)
            if entry.get("revoked") or entry.get("capabilities")}


def drain_state(node):
    # The base directory: `ow node drain` opens BASE/controller.
    return h.ow("--data-dir", h.DATA, "node", "drain", node, "--action", "status", timeout=60).get("state")


def describe(pool):
    """The group, its hook and policies, every live instance and disk tagged with the pool."""
    groups = aws("autoscaling", "describe-auto-scaling-groups", "--auto-scaling-group-names", pool["group"])["AutoScalingGroups"]
    if len(groups) != 1:
        raise Refused(f"no Auto Scaling group {pool['group']}")
    hooks = aws("autoscaling", "describe-lifecycle-hooks", "--auto-scaling-group-name", pool["group"])["LifecycleHooks"]
    policies = aws("autoscaling", "describe-policies", "--auto-scaling-group-name", pool["group"])["ScalingPolicies"]
    activities = aws("autoscaling", "describe-scaling-activities", "--auto-scaling-group-name", pool["group"],
                     "--max-items", "20")["Activities"]
    instances = [i for r in aws("ec2", "describe-instances", "--filters", f"Name=tag:ow-pool,Values={pool['name']}",
                                f"Name=instance-state-name,Values={LIVE}")["Reservations"] for i in r["Instances"]]
    volumes = aws("ec2", "describe-volumes", "--filters", f"Name=tag:ow-pool,Values={pool['name']}")["Volumes"]
    return groups[0], hooks, policies, activities, instances, volumes


def shape_problems(pool, group, hooks, policies):
    """Why this group could launch, terminate or keep instances on its own."""
    problems = []
    hook = [k for k in hooks if k.get("LifecycleHookName") == pool["hook"]]
    if not hook or hook[0].get("LifecycleTransition") != "autoscaling:EC2_INSTANCE_TERMINATING" \
            or hook[0].get("DefaultResult") != "ABANDON":
        problems.append(f"no termination hook {pool['hook']} that abandons by default")
    if ((group.get("InstanceLifecyclePolicy") or {}).get("RetentionTriggers") or {}).get("TerminateHookAbandon") != "retain":
        problems.append("no instance lifecycle policy retaining abandoned terminations (ABANDON alone terminates)")
    if policies:
        problems.append(f"scaling policies would size the group: {[p.get('PolicyName') for p in policies]}")
    suspended = {s.get("ProcessName") for s in group.get("SuspendedProcesses") or []}
    if not SUSPENDED <= suspended:
        problems.append(f"processes not suspended: {sorted(SUSPENDED - suspended)}")
    if group.get("NewInstancesProtectedFromScaleIn") is not True or group.get("MinSize") != 0:
        problems.append("new instances must be protected from scale-in and the minimum 0")
    return problems


def check(name):
    """Activation preflight, writing nothing: IAM's simulation of this role for the group's
    three actions (a write or a made-up instance cannot test them safely), and a lookup of
    a secret that does not exist. Missing rights, shape problems or an unanswered
    simulation refuse activation."""
    pool = load(name)
    group, hooks, policies, _, _, _ = describe(pool)
    problems = shape_problems(pool, group, hooks, policies)
    try:  # No write and no fake instance answers for these honestly: ask IAM about this role.
        me = aws("sts", "get-caller-identity")["Arn"]
        match = re.fullmatch(r"arn:(aws[a-z-]*):sts::(\d{12}):assumed-role/([\w+=,.@-]+)/[^/]+", me)
        verdict = aws("iam", "simulate-principal-policy", "--policy-source-arn",
                      f"arn:{match.group(1)}:iam::{match.group(2)}:role/{match.group(3)}", "--action-names",
                      *[f"autoscaling:{a}" for a in GROUP_ACTIONS], "--resource-arns", group["AutoScalingGroupARN"])
        decisions = {r.get("EvalActionName"): r.get("EvalDecision") for r in verdict["EvaluationResults"]}
        problems += [f"missing permission: {a}" for a in GROUP_ACTIONS if decisions.get(f"autoscaling:{a}") != "allowed"]
    except (RuntimeError, AttributeError, KeyError) as e:
        problems.append(f"cannot verify the group rights: {str(e)[-200:]}")
    probes = {"join secrets": ["secretsmanager", "describe-secret", "--secret-id", f"open-workspaces/pool/{name}/probe"]}
    for right, call in probes.items():
        try:
            aws(*call)
        except RuntimeError as e:
            if denied(e):
                problems.append(f"missing permission: {right}")
    out = {"pool": name, "group": pool["group"], "ok": not problems, "problems": problems}
    print(json.dumps(out, indent=2))
    if problems:
        raise Refused(f"activation refused for pool {name}: {'; '.join(problems)}")
    return 0


# ---------------------------------------------------------------- sync


def answer_joins(pool, group, instances, known):
    """Each running group instance whose volume has no runner gets a join secret only it
    can read (tagged with its role session); a ready request there gets a join."""
    members = {i["InstanceId"]: i for i in group.get("Instances") or []}
    for instance in instances:
        iid, node = instance["InstanceId"], node_of(pool, instance)
        state = (members.get(iid) or {}).get("LifecycleState")
        if not node or node in known or state != "InService" or instance["State"]["Name"] != "running":
            continue
        try:
            answer_join(pool, iid, node)
        except RuntimeError as e:  # one instance's failure (new secrets settle slowly) waits for the next sync
            h.say(f"{node}: join not answered yet: {str(e)[-300:]}")


def answer_join(pool, iid, node):
    name, session = f"open-workspaces/pool/{pool['name']}/{node}", f"{pool['roleId']}:{iid}"
    tags = [f"Key=ow-pool,Value={pool['name']}", f"Key=ow-session,Value={session}"]
    try:
        described = aws("secretsmanager", "describe-secret", "--secret-id", name)
    except RuntimeError as e:
        if "ResourceNotFoundException" not in str(e):
            raise
        aws("secretsmanager", "create-secret", "--name", name, "--description",
            f"Open Workspaces: join of pool runner {node}", "--secret-string", json.dumps({"pending": node}), "--tags",
            *[f"Key={k},Value={v}" for k, v in (("ow-pool", pool["name"]), ("ow-session", session))])
        h.say(f"{node}: join secret for {iid}")
        return
    if {t["Key"]: t["Value"] for t in described.get("Tags") or []}.get("ow-session") != session:
        # A volume back on a new instance that never enrolled: only the new one may read.
        aws("secretsmanager", "tag-resource", "--secret-id", name, "--tags", *tags)
        h.put_secret(name, {"pending": node}, h.region())
        h.say(f"{node}: join secret moved to {iid}")
        return
    try:
        value = json.loads(h.secret(name, h.region()) or "{}")
    except ValueError:
        return
    if not isinstance(value, dict):
        return
    budget = value.get("budget") if value.get("ready") == node else None
    if not isinstance(budget, dict) or not all(type(budget.get(k)) is int and 1 <= budget[k] <= high
                                               for k, high in (("memory", 1048576), ("slots", 64), ("cpus", 64))):
        return
    h.invite_runner(node, budget["memory"], budget["slots"], budget["cpus"], secret_id=name)


def removals():
    return h.read_json(REMOVALS) or {}


def complete_own(pool, group):
    """Completes (CONTINUE) only the hooks of terminations this controller started for
    a runner still removed or never enrolled. Anything else waits out its hook and is retained."""
    mine = removals()
    for member in group.get("Instances") or []:
        iid = member["InstanceId"]
        if member.get("LifecycleState") != "Terminating:Wait" or iid not in mine:
            continue
        node, enrolled = mine[iid].get("node"), mine[iid].get("enrolled")
        # Read again now: a runner may have enrolled or been undrained since the removal began.
        if node and ((enrolled and drain_state(node) == "removed") or (enrolled is False and node not in catalog())):
            aws("autoscaling", "complete-lifecycle-action", "--auto-scaling-group-name", pool["group"],
                "--lifecycle-hook-name", pool["hook"], "--instance-id", iid, "--lifecycle-action-result", "CONTINUE")
            h.say(f"{iid}: removal completed")
        else:
            h.say(f"{iid}: runner {node} is no longer removable; its removal will be abandoned and the instance retained")


def disk_cents(pool, gib):
    return math.ceil(gib * pool["diskMonthlyCentsPerGib"] / 730)


def observe(pool, group, hooks, policies, activities, instances, volumes, known, now):
    members = {i["InstanceId"]: i for i in group.get("Instances") or []}
    out, complete = [], True
    for instance in instances:
        iid, node = instance["InstanceId"], node_of(pool, instance)
        state = (members.get(iid) or {}).get("LifecycleState")
        entry = known.get(node) if node else None
        if iid not in members:
            kind = "orphaned"
        elif state == "Terminating:Retained":
            kind = "retained"
        elif state == "InService" and entry and entry.get("online") is True and not entry.get("revoked"):
            kind = "ready"
        elif state in ("Pending", "Pending:Wait", "Pending:Proceed", "InService", "Terminating", "Terminating:Wait",
                       "Terminating:Proceed"):
            kind = "pending"
        else:
            kind, complete = "pending", False
        out.append({"id": iid, "pool": pool["name"], "node": node if entry else None, "state": kind, "action": None,
                    "hourly_cents": pool["instanceHourlyCents"]})
    disks = [{"id": v["VolumeId"], "pool": pool["name"], "logical_bytes": v["Size"] * 2**30,
              "hourly_cents": disk_cents(pool, v["Size"])} for v in volumes]
    recent = [a for a in activities if a.get("StatusCode") == "Failed"
              and calendar.timegm(time.strptime(a["StartTime"][:19], "%Y-%m-%dT%H:%M:%S")) > now - 3600]
    resolved = not recent and not any(str(m.get("LifecycleState", "")).startswith(("Terminating:Wait", "Terminating:Proceed"))
                                      or m.get("LifecycleState") == "Terminating" for m in members.values())
    return out, disks, complete, not shape_problems(pool, group, hooks, policies), resolved


def fixed_instances(specs, known, pool):
    """Hosts and runners outside any pool still count: NODE=NAME:CENTS, found by tag Name."""
    instances, disks = [], []
    for spec in specs:
        match = re.fullmatch(r"([A-Za-z0-9_-]{1,32})=([a-z0-9][a-z0-9-]*):(\d{1,6})", spec)
        if not match:
            raise Refused(f"--fixed takes NODE=NAME:CENTS, not {spec!r}")
        node, name, cents = match.group(1), match.group(2), int(match.group(3))
        found = [i for r in aws("ec2", "describe-instances", "--filters", f"Name=tag:Name,Values={name}",
                                f"Name=instance-state-name,Values={LIVE}")["Reservations"] for i in r["Instances"]]
        if len(found) != 1:
            raise Refused(f"expected one instance named {name}, found {len(found)}")
        iid = found[0]["InstanceId"]
        instances.append({"id": iid, "pool": "fixed", "node": node if node in known else None,
                          "state": "ready" if found[0]["State"]["Name"] == "running" else "pending", "action": None,
                          "hourly_cents": cents})
        for v in aws("ec2", "describe-volumes", "--filters", f"Name=attachment.instance-id,Values={iid}")["Volumes"]:
            disks.append({"id": v["VolumeId"], "pool": "fixed", "logical_bytes": v["Size"] * 2**30,
                          "hourly_cents": disk_cents(pool, v["Size"])})
    return instances, disks


def sync(names, fixed=(), hours=None):
    """One pass for every pool: answer joins, complete this controller's own removals, then
    write the complete observation the scaler reads. A failed describe writes nothing."""
    if hours is None:
        hours = ((h.read_json(f"{h.CONTROLLER}/limits.json") or {}).get("pool") or {}).get("hours")
    if type(hours) is not int or hours < 1:
        raise Refused("no liability horizon: set limits.pool.hours or pass --hours")
    known = catalog()
    snapshot = {"observed_at": 0, "hours": hours, "complete": True, "retention_ok": True, "outcome_resolved": True,
                "instances": [], "disks": [], "receipts": {}}
    pools = [load(name) for name in dict.fromkeys(names)]
    for pool in pools:
        group, hooks, policies, activities, instances, volumes = described = describe(pool)
        answer_joins(pool, group, instances, known)
        complete_own(pool, group)
        found, disks, complete, retention, resolved = observe(pool, *described, known, int(time.time()))
        snapshot["instances"] += found
        snapshot["disks"] += disks
        snapshot["complete"] &= complete
        snapshot["retention_ok"] &= retention
        snapshot["outcome_resolved"] &= resolved
    # Hosts outside the pools are priced at the highest pool disk rate: an overestimate, never under.
    extra, disks = fixed_instances(fixed, known, max(pools, key=lambda p: p["diskMonthlyCentsPerGib"]))
    snapshot["instances"] += extra
    snapshot["disks"] += [d for d in disks if d["id"] not in {x["id"] for x in snapshot["disks"]}]
    snapshot["observed_at"] = int(time.time())
    h.write_atomic(OBSERVATIONS, json.dumps(snapshot, indent=2, sort_keys=True) + "\n", 0o600, owner="ow")
    print(json.dumps({k: snapshot[k] for k in ("complete", "retention_ok", "outcome_resolved")}
                     | {"instances": len(snapshot["instances"]), "disks": len(snapshot["disks"])}))
    return 0


# ---------------------------------------------------------------- actions


def launch(name, expect):
    """Desired capacity EXPECT -> EXPECT+1. A repeat after success is a no-op, so a lost
    reply never launches twice; anything else unexpected refuses."""
    pool = load(name)
    group, hooks, policies, activities, instances, volumes = described = describe(pool)
    problems = shape_problems(pool, group, hooks, policies)
    if problems:
        raise Refused(f"launch refused: {'; '.join(problems)}")
    _, _, complete, _, resolved = observe(pool, *described, catalog(), int(time.time()))
    desired = group["DesiredCapacity"]
    if desired == expect + 1:
        h.say(f"{pool['group']} already wants {desired}; nothing launched")
        return 0
    if desired != expect:
        raise Refused(f"{pool['group']} wants {desired}, not {expect}; nothing launched")
    if not complete or not resolved:
        raise Refused("a removal or failed launch is unresolved; nothing launched")
    if expect + 1 > group["MaxSize"]:
        raise Refused(f"{pool['group']} is at its maximum {group['MaxSize']}")
    # Auto Scaling has no compare-and-set: read again right before the write to narrow the window.
    if describe(pool)[0]["DesiredCapacity"] != expect:
        raise Refused(f"{pool['group']} changed while launching; nothing launched")
    aws("autoscaling", "set-desired-capacity", "--auto-scaling-group-name", pool["group"], "--desired-capacity", str(expect + 1))
    h.say(f"{pool['group']}: desired {expect} -> {expect + 1}")
    return 0


def remove(name, node=None, instance=None):
    """Exactly one instance: a runner the controller drained empty and removed, or an
    instance that never enrolled. Idempotent: rerun after an interruption."""
    pool = load(name)
    group, *_ = describe(pool)
    members = {i["InstanceId"]: i for i in group.get("Instances") or []}
    found = [i for r in aws("ec2", "describe-instances", "--filters", f"Name=tag:ow-pool,Values={name}",
                            f"Name=instance-state-name,Values={LIVE}")["Reservations"] for i in r["Instances"]
             if (node and node_of(pool, i) == node) or (instance and i["InstanceId"] == instance)]
    if len(found) != 1 or found[0]["InstanceId"] not in members:
        raise Refused(f"expected one instance of group {pool['group']} for {node or instance}, found {len(found)}")
    iid, runner = found[0]["InstanceId"], node_of(pool, found[0])
    if not runner:
        raise Refused(f"{iid} has no pool data volume at /dev/sdf, so its runner cannot be proven empty")
    known = catalog()
    if runner in known:
        if drain_state(runner) != "removed":
            raise Refused(f"runner {runner} is not drained and removed: ow node drain {runner} first")
    elif node:
        raise Refused(f"{node} is not a runner of this controller")
    state = members[iid].get("LifecycleState")
    mine = removals()
    if state in ("InService", "Terminating:Retained"):
        h.write_json(REMOVALS, {**mine, iid: {"node": runner, "enrolled": runner in known}})
        # A retained instance already left desired capacity; a live one leaves it now.
        aws("autoscaling", "terminate-instance-in-auto-scaling-group", "--instance-id", iid,
            "--should-decrement-desired-capacity" if state == "InService" else "--no-should-decrement-desired-capacity")
    elif iid not in mine:
        raise Refused(f"{iid} is {state}, a termination this controller did not start")
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        group, *_ = describe(pool)
        state = next((m.get("LifecycleState") for m in group.get("Instances") or [] if m["InstanceId"] == iid), "gone")
        if state == "Terminating:Wait":
            complete_own(pool, group)
        elif state in ("gone", "Terminating:Proceed", "Terminated"):
            h.say(f"{iid} removed ({runner}); its data volume is kept: ow-host pool disks {name}")
            return 0
        time.sleep(5)
    raise RuntimeError(f"{iid} still {state} after 300 s; rerun pool remove")


def disks(name, show=True):
    pool = load(name)
    listed = {entry["id"]: entry for entry in h.ow("node", "list", timeout=30)}
    known = catalog()
    out = []
    for v in aws("ec2", "describe-volumes", "--filters", f"Name=tag:ow-pool,Values={name}")["Volumes"]:
        node = f"{name}-{VOLUME.match(v['VolumeId']).group(1)}"
        attached = [a["InstanceId"] for a in v.get("Attachments") or []]
        # Deletable only with positive evidence: no catalog row, or a join that never connected.
        runner = drain_state(node) if node in known else "join never used" if node in listed else "never enrolled"
        out.append({"volume": v["VolumeId"], "state": v["State"], "gib": v["Size"], "created": v["CreateTime"],
                    "instances": attached, "node": node, "runner": runner, "monthly_cents": v["Size"] * pool["diskMonthlyCentsPerGib"],
                    "deletable": v["State"] == "available" and runner in ("removed", "join never used", "never enrolled")})
    if show:
        print(json.dumps(out, indent=2))
    return out


def delete_disk(name, volume):
    """Deletes one detached disk whose runner holds nothing: drained empty and removed, or
    never enrolled (no box was ever placed on it). Any other disk is salvage material."""
    if not VOLUME.match(volume):
        raise Refused(f"{volume!r} is not a volume ID")
    match = [d for d in disks(name, show=False) if d["volume"] == volume]
    if not match:
        raise Refused(f"{volume} is not a disk of pool {name}")
    if not match[0]["deletable"]:
        raise Refused(f"{volume} is {match[0]['state']} and its runner {match[0]['runner']}: salvage it first")
    aws("ec2", "delete-volume", "--volume-id", volume)
    h.say(f"deleted {volume} ({match[0]['node']}, {match[0]['runner']})")
    try:  # the runner's spent join secret goes with its disk; each secret is billed
        aws("secretsmanager", "delete-secret", "--secret-id", f"open-workspaces/pool/{name}/{match[0]['node']}",
            "--force-delete-without-recovery")
    except RuntimeError as e:
        if "ResourceNotFoundException" not in str(e):
            raise
    return 0


def main(argv):
    parser = argparse.ArgumentParser(prog="ow-host pool", description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check").add_argument("pool")
    s = sub.add_parser("sync")
    s.add_argument("pools", nargs="+")
    s.add_argument("--fixed", action="append", default=[])
    s.add_argument("--hours", type=int)
    lp = sub.add_parser("launch")
    lp.add_argument("pool")
    lp.add_argument("--expect", type=int, required=True)
    r = sub.add_parser("remove")
    r.add_argument("pool")
    which = r.add_mutually_exclusive_group(required=True)
    which.add_argument("--node")
    which.add_argument("--instance")
    sub.add_parser("disks").add_argument("pool")
    d = sub.add_parser("delete-disk")
    d.add_argument("pool")
    d.add_argument("volume")
    args = parser.parse_args(argv)
    if h.role() not in ("host", "controller"):
        raise Refused("ow-host pool runs on a controller host")
    if args.command == "check":
        return check(args.pool)
    if args.command == "sync":
        return sync(args.pools, args.fixed, args.hours)
    if args.command == "launch":
        if args.expect < 0:
            raise Refused("--expect takes the current desired capacity")
        return launch(args.pool, args.expect)
    if args.command == "remove":
        if args.instance and not INSTANCE.match(args.instance) or args.node and not h.NODE.match(args.node):
            raise Refused("remove takes a node ID or an instance ID")
        return remove(args.pool, args.node, args.instance)
    if args.command == "disks":
        disks(args.pool)
        return 0
    return delete_disk(args.pool, args.volume)
