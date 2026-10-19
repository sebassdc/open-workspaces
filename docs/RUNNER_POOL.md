# AWS runner pool

`RunnerPool` (deploy/aws/pool.ts) is an opt-in group of on-demand runner hosts that
one controller sizes by exact actions. It reuses the Host/Runner stage 0
(`bootstrap.sh`, byte for byte), the release build, SSM and the controller's
existing join. **Creating it starts no instance and changes no existing host:** the
group starts at desired capacity 0 and only the controller's role gains rights.
There is no Spot, no AWS scaling policy, no emergency save path and no forced move
of a workstation. The shadow scaler ([SCALER.md](SCALER.md)) does not call this
adapter yet; every action below is an explicit operator or controller command.

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
| controller role | create/tag join secrets | `open-workspaces/pool/NAME/*`, request tag `ow-pool=NAME`, tag keys `ow-pool`/`ow-session` only |
| controller role | read/write/delete join secrets | `open-workspaces/pool/NAME/*` |
| controller role | `ec2:DeleteVolume` | volumes tagged `ow-pool=NAME` (used only by `delete-disk`) |
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

**Activation.** `check` verifies the group shape (hook, retention policy, no scaling
policy, suspended processes, scale-in protection, minimum 0) and probes each right
with a call that changes nothing: the current desired capacity, a lifecycle
completion and a termination for an instance that does not exist, and a lookup of a
secret that does not exist. An access refusal is reported as a missing permission
and refuses activation. `launch` and `remove` repeat the shape check.

**Launch.** `launch --expect N` sets desired capacity to N+1 only while it is N; a
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
instance that never joins stays `pending` and billable until removed.

**Observation.** `sync` writes `scaler-observations.json` (mode 0600, owned by `ow`)
in the controller data directory: every pool instance (`ready`, `pending`,
`retained`, or `orphaned` when tagged but outside the group), every tagged disk
including detached ones, and any `--fixed` host so that every enrolled node is
covered. `retention_ok` is false when the hook or lifecycle policy is missing;
`outcome_resolved` is false while a termination is in progress or after a failed
launch. A failed describe call writes nothing. Run `sync` often enough for the
scaler's 15-second freshness; nothing here installs a timer.

**Removal.** First drain the runner ([DRAIN.md](DRAIN.md)) through `retain` and
`remove`. Then `pool remove --node NODE` terminates exactly that instance with a
desired-capacity decrement, records the removal, and completes the hook with
`CONTINUE` after checking again that the runner is still removed. A termination
the controller did not start (or whose runner stopped being removable) is never
completed: it waits out `drainTimeoutSeconds`, is abandoned and the instance is
retained. Rerunning `remove` on a retained instance terminates it again without a
second decrement.

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
it is detached and its runner was drained empty and removed, or never enrolled.
There is no age-based or automatic deletion.

To salvage a disk whose instance failed, launch a replacement from the pool's launch
template without its own data volume and attach the retained one:

```sh
aws ec2 run-instances --launch-template LaunchTemplateId=LT,Version='$Latest' \
  --block-device-mappings 'DeviceName=/dev/sdf,NoDevice=""' --network-interfaces ...
aws ec2 attach-volume --volume-id VOLUME --instance-id NEW --device /dev/sdf
```

Stage 0 waits for the disk; the node ID, runner credential and tailnet identity on
it bring the same runner back to the controller. Its boxes then start as on any
runner restart. The replacement is outside the group (`orphaned` in observations)
until you drain it and remove it with `pool remove --instance`, or attach it to the
group yourself.

## Status and verification

Mock and component tests: `cd deploy/aws && npm run check` (Pulumi mocks in
`test/pool.test.mjs`, the package schema in `test/schema.test.mjs`, the host side
in `host/test_pool.py`). Dedicated-fixture results, with what could not be run,
are recorded with the release that introduced the component. Live scaler
activation stays blocked as described in [SCALER.md](SCALER.md).
