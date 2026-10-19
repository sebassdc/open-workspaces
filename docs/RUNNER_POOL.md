# AWS runner pool

`RunnerPool` (deploy/aws/pool.ts) is an opt-in group of on-demand runner hosts that
one controller sizes by exact actions. It reuses the Host/Runner stage 0
(`bootstrap.sh`, byte for byte), the release build, SSM and the controller's
existing join. **Creating it starts no instance and changes no existing host:** the
group starts at desired capacity 0 and only the controller's role gains rights.
There is no Spot, no AWS scaling policy, no emergency save path and no forced move
of a workstation. In the scaler's live mode ([SCALER.md](SCALER.md)) the adapter's
`sync` executes the scaler's one in-flight action per pool and reports its receipt;
otherwise every action below is an explicit operator command.

## Component

```ts
import { Host, RunnerPool } from "open-workspaces-aws";

const host = new Host("ow-ctl", { /* ... */ });
new RunnerPool("dev", {
    controller: { hostName: host.hostName, roleName: host.roleName },
    amiId: "ami-...",                 // pinned; never a moving "current" image
    instanceType: "m8i-flex.xlarge",  // nested virtualization
    subnetIds: ["subnet-..."],
    maxSize: 2,
    instanceHourlyCents: 21,          // your price, for liability accounting only
    diskMonthlyCentsPerGib: 8,
    tailscaleAuthKeySecretArn: "arn:aws:secretsmanager:...",
    drainTimeoutSeconds: 3600,
});
```

The name (1 to 14 lowercase letters, digits or `-`) prefixes each runner's node ID.
The release is the package's tag unless `ref` pins another tag or commit; the AMI
is required. Instance selection, prices, the maximum, schedules and IAM targets
belong to the consuming stack, not to this package.

What it creates:

- a launch template: pinned AMI, IMDSv2 only, nested virtualization, encrypted root,
  and an encrypted gp3 data volume at `/dev/sdf` with **delete on termination off**;
- an Auto Scaling group `open-workspaces-NAME`: minimum 0, desired 0 (never managed
  by Pulumi afterwards), scale-in protection for new instances, the processes
  `AZRebalance`, `AlarmNotification`, `InstanceRefresh`, `ReplaceUnhealthy` and
  `ScheduledActions` suspended, a termination hook `open-workspaces-drain` with
  default result `ABANDON`, and the instance lifecycle policy that retains an
  instance whose termination is abandoned;
- a security group with no inbound rules, an instance role, and the shared config
  parameter `/open-workspaces/NAME/config` (no secret values);
- a policy on the controller host's role (below).

AWS documents that `ABANDON` alone terminates the instance; only the lifecycle
policy retains it ([instance lifecycle policy](https://docs.aws.amazon.com/autoscaling/ec2/userguide/instance-lifecycle-policy.html),
[retention](https://docs.aws.amazon.com/autoscaling/ec2/userguide/configure-instance-retention.html)).
The provider pinned by this package (`@pulumi/aws` 7.48.0) renders
`instanceLifecyclePolicy.retentionTriggers.terminateHookAbandon`; the component tests
and the package schema check cover it. An organization policy that denies
`autoscaling:CreateAutoScalingGroup` refuses the component at `pulumi up`.

## Rights

| Principal | May | Scope |
|---|---|---|
| controller role | `SetDesiredCapacity`, `TerminateInstanceInAutoScalingGroup`, `CompleteLifecycleAction` | this group's ARN only |
| controller role | describe groups, hooks, policies, activities, instances, volumes | describe calls only |
| controller role | `iam:SimulatePrincipalPolicy` | its own role only (`pool check`) |
| controller role | create/tag join secrets | `open-workspaces/pool/NAME/*`, request tag `ow-pool=NAME`, tag keys `ow-pool`/`ow-session` only |
| controller role | read/write/delete join secrets | `open-workspaces/pool/NAME/*` |
| controller role | `ec2:DeleteVolume` | volumes tagged `ow-pool=NAME` (used only by `delete-disk`) |
| controller role | `ec2:TerminateInstances` | instances tagged `ow-pool=NAME` (only a salvage replacement outside the group) |
| pool instance role | read/answer a join secret | only a secret tagged `ow-session` equal to its own `${aws:userid}` (role ID and instance ID) |
| pool instance role | config parameter, Tailscale key, store/backup prefixes | as a Runner, with backups under `backupUrl/runners/NAME` |

No secret value passes through Pulumi, a command line or SSM command history: the
Tailscale key and every join stay in Secrets Manager.

## Lifecycle

All commands run on the controller host, as root (`ow-aws --host CONTROLLER pool ...`
from an operator machine):

```sh
ow-host pool check dev                 # activation preflight; exit 1 refuses
ow-host pool launch dev --expect 0     # desired 0 -> 1, once
ow-host pool sync dev [--fixed NODE=NAME:CENTS].. [--hours H]
ow-host pool remove dev --node NODE    # or --instance ID for one that never enrolled
ow-host pool disks dev
ow-host pool delete-disk dev VOLUME
```

**Live scaler actions.** With the scaler in live mode, each `sync` reads the
controller's in-flight action (`ow scaler`) for every pool it observes and performs
one idempotent step, journaled before any effect in `pool-actions.json`:

- *launch*: records the current desired capacity N and members, runs `launch --expect N`
  (a no-op once it is N+1), and attributes the one new member to the action. The
  receipt is `pending` until that instance's runner is enrolled, online and in service,
  then `joined` with its node; two new members for one launch report `unknown`.
- *drain*: runs `ow node drain` `begin`; once `empty`, writes provider evidence that the
  runner's data volume has delete-on-termination off and `retain`s the exact
  certificate; then `remove`s the enrollment and the instance (`pool remove`). The
  receipt is `absent` once the instance is gone.
- any refusal or AWS error reports `unknown`: the controller holds the reservation and
  takes no further action. Shadow and uncertain actions are never executed.
  Persisted live actions also wait when the current mode is shadow, status has an
  error, or their plan is stale. Status rechecks activation evidence on every read
  so a previous successful tick grants no authority after its gates are withdrawn.
  The plan must explicitly set `execution_allowed`: a fresh tick alone is not
  permission when inventory, retention, provider outcome, durability or caps hold
  execution. Older plans without this field wait for a new controller tick.
  Sync also pauses journaled termination-hook completion without an authorized
  drain action; the hook may time out into retention. Explicit operator `remove`
  remains a separate action with its own drain and retention checks.
  A delayed termination hook for the adapter's journaled drain is known pending
  work, so it does not itself set the observation's unresolved-outcome hold.
  Unjournaled terminations and failed provider activities still do. Each retry
  runs the selected removal's checks and completes only that instance's hook;
  sync has no separate loop that completes other journaled removals.

The scaler's pool name must be the RunnerPool's name and the runners' placement pool;
today every runner's placement pool is `default`, so a pool that serves queued demand
is named `default`. Run `sync` every few seconds (the controller ignores observations
older than 15 seconds); nothing here installs the timer.

**Activation.** `check` verifies the group shape (hook, retention policy, no scaling
policy, suspended processes, scale-in protection, minimum 0) and checks the rights
without changing anything in AWS: IAM's simulation of the controller's own role for
`SetDesiredCapacity`, `TerminateInstanceInAutoScalingGroup` and
`CompleteLifecycleAction` on this group (a termination of a made-up instance is refused
even when allowed, and a desired-capacity write could race a launch), and for
creating, tagging, reading and answering a join secret as `sync` does (with the
pool's request tag). A missing right, or a simulation that cannot be run, refuses
activation. A pass is recorded in `scaler-provider-check.json`, which the controller's
live mode requires per pool within the last day. `launch` and `remove` repeat the shape check.
Each preflight revokes that pool's previous pass before checking AWS; failure or
interruption leaves it revoked while preserving other pools' evidence.

**Launch.** `launch --expect N` sets desired capacity to N+1 only while it is N (read twice,
the second time right before the write: Auto Scaling has no compare-and-set); a
repeat after success is a no-op, so a lost reply never launches twice. It refuses at
the maximum, while a removal is in progress, or after a failed launch activity in
the last hour.

**Join.** A new instance runs stage 0, finds the one EBS disk with nothing mounted,
builds the pinned release and derives its node ID `NAME-VOLUME` from that disk. It
then asks for a join by writing its budget into `open-workspaces/pool/NAME/NODE`,
which `sync` created for exactly that instance's role session. The next `sync` mints
the existing single-use join (10 minutes) into the same secret; the instance redeems
it and marks it used. A runner counts as `ready` only once enrolled and reporting a
fresh heartbeat; placement still checks its capabilities as for any runner. An
unused join expires after 10 minutes; the next `sync` resets it and the waiting instance
asks again. An instance asks for 30 minutes during its first boot; one that never joins stays
`pending` and billable until removed with `pool remove --instance`.

**Observation.** `sync` writes `scaler-observations.json` (mode 0600, owned by `ow`)
in the controller data directory: every pool instance (`ready`, `pending`,
`retained`, or `orphaned` when tagged but outside the group), every tagged disk
including detached ones, and any `--fixed` host so that every enrolled node is
covered. `retention_ok` is false when the hook or lifecycle policy is missing;
`outcome_resolved` is false while a termination is in progress or after a failed
launch. A failed describe call writes nothing. Run `sync` often enough for the
scaler's 15-second freshness; nothing here installs a timer.

**Removal.** First drain the runner ([DRAIN.md](DRAIN.md)) through `retain` and
`remove`. Then `pool remove --node NODE` checks the group shape again (a removal without
the hook and retention policy is refused) and terminates exactly that instance with a
desired-capacity decrement, records the removal, and completes the hook with
`CONTINUE` after checking again that the runner is still removed. A termination
the controller did not start (or whose runner stopped being removable) is never
completed: it waits out `drainTimeoutSeconds`, is abandoned and the instance is
retained. Rerunning `remove` on a retained instance terminates it again without a
second decrement. An instance without its pool data volume is refused: its runner
cannot be proven empty.

## Retention, replacement and cost bound

Retained instances keep billing, leave desired capacity and, if desired capacity
was not lowered, cause a replacement launch. This adapter always lowers desired
capacity when it starts a termination, and the suspended processes stop AWS from
terminating instances on its own (health replacement, rebalancing, refresh,
schedules). Scale-in protection keeps a desired-capacity decrease from picking an
instance. Under those settings an instance is retained only when a termination the
controller started times out, and that instance is reported as `retained` and
counted in liability until it is removed. The total is at most `maxSize` group
instances plus the retained ones shown in the observation. Someone else's
`TerminateInstanceInAutoScalingGroup` without a decrement still causes a
replacement; keep such rights away from other principals.

## Dev disks and salvage

A data volume is never deleted with its instance. `pool disks` lists every disk
with its runner's drain state and monthly cost; `delete-disk` deletes one only when
it is detached and its runner was drained empty and removed, or never connected
(no catalog row, or a join that was never used); it then also deletes that runner's
spent join secret.
There is no age-based or automatic deletion.

To salvage a disk whose instance failed, launch a replacement from the pool's launch
template data **without** its `/dev/sdf` mapping, then attach the retained disk. A
`NoDevice` override at `run-instances` does not remove a launch-template volume (a
fresh empty disk is attached instead), so copy the data:

```sh
aws ec2 describe-launch-template-versions --launch-template-id LT --versions '$Latest' \
  --query 'LaunchTemplateVersions[0].LaunchTemplateData' > lt.json
python3 - lt.json SUBNET <<'PY'
import base64, json, sys
d = json.load(open(sys.argv[1]))
open("user-data", "wb").write(base64.b64decode(d.pop("UserData")))
d["BlockDeviceMappings"] = [m for m in d["BlockDeviceMappings"] if m["DeviceName"] != "/dev/sdf"]
d["NetworkInterfaces"][0]["SubnetId"] = sys.argv[2]
json.dump({**d, "MinCount": 1, "MaxCount": 1}, open("input.json", "w"))
PY
aws ec2 run-instances --cli-input-json file://input.json --user-data file://user-data
aws ec2 attach-volume --volume-id VOLUME --instance-id NEW --device /dev/sdf
```

Stage 0 waits for the disk; the node ID, runner credential and tailnet identity on
it bring the same runner back to the controller. Its boxes then start as on any
runner restart. The replacement is outside the group (`orphaned` in observations).
When done, drain it like any runner and run `pool remove --node NODE`: an instance
outside the group has no hook, so it is terminated directly (`ec2:TerminateInstances`,
allowed only for instances tagged with this pool); its data volume stays.

## Status and verification

Mock and component tests: `cd deploy/aws && npm run check` (Pulumi mocks in
`test/pool.test.mjs`, the package schema in `test/schema.test.mjs`, the host side
in `host/test_pool.py`). They cover the pinned AMI and release, unchanged stage 0,
no inbound rules, no secret in Pulumi, the exact rights, compare-and-set launch,
duplicate and failed launches, join requests and retries, exact removal, retained
instances, observations, preflight refusals and disk cleanup.

On a dedicated disposable fixture (release 0.9.18 candidate, `@pulumi/aws` 7.48.0,
Ubuntu 24.04 `m8i-flex.xlarge`, Firecracker v1.17.0) the following was observed:
adding the component to an existing Host stack previewed only creations (no update,
replacement or instance); a launch-template instance found its data volume, derived
its node ID, asked for and redeemed a join and came online (cold launch to online
6 min 0 s, n=1); the instance could read and answer only the secret tagged with its
own session, and the controller's secret and disk rights refused untagged, foreign
or extra-tag requests; terminated instances left their data volumes available (n=3);
a stopped Alpine box's 1 MiB random file survived instance loss and came back through
a salvage replacement as the same runner, cold-booted, with an identical SHA-256
(replacement boot 34 s, n=1); detached and orphaned resources appeared in the
observation; and `pool check` refused when the controller's rights were removed and
passed when present.

Not yet run on real AWS: the fixture account's organization policy denies
`CreateAutoScalingGroup`, so a hook-timeout retention, exact removal through the
group, desired-capacity changes and replacement accounting remain to be proven
before activation. Cold image pulls and warm moves were not measured. Live scaler
activation stays blocked as described in [SCALER.md](SCALER.md).
