# Bounded shadow scaler

The controller can opt in to a small planner. It records at most one launch or
soft-drain recommendation per pool per iteration, with its reason, queued shapes,
progress and maximum forecast liability in SQLite. **There is no live executor,
cloud API call, automatic drain, termination or enrollment in this release.**
Installation alone starts no scaler loop. Existing workers, boxes and CPU budgets
keep their current configuration.

## Planning and accounting

The planner consumes the existing unreserved, preparing wake intents in the
`default` pool, in durable transfer order. A failed create is not an accepted
queue entry. Other pools can plan count floors and safe idle surplus; this slice
does not add a new create queue or a pool selector to wake requests.

Each queued shape retains CPU, RAM, one running slot, logical disk and image/RAM
compatibility. Placement reuses the existing capability filters and individual
runner budgets. It subtracts committed and in-flight reservations, and reserves
twice the capture disk for wake staging. Several small runners cannot jointly
satisfy one large box. Unknown shape/compatibility or disk headroom never becomes
available capacity. This is deterministic, conservative first-fit planning,
not an optimal bin-packing claim. Existing wake admission still makes the final
reservation and remains authoritative.

A floor counts fresh active enrolled runners. A ceiling counts **all** provider
instances in that pool, including pending, retained and orphaned instances.
Global limits also count every billable disk, even without an instance or node.
A launch forecast reserves the configured maximum new cost, instance count and
logical disk for `1 + replacement_headroom` copies **before** recording intent.
All pool forecasts serialize in `BEGIN IMMEDIATE`, against the same global cost
and storage headroom. Rates/costs use the existing `limits.pool.hours` horizon;
this is a capacity forecast, not an elapsed-spend ledger or a billing guarantee.
Shadow reservations do not change existing invitation/owner admission.

Missing/incomplete/stale provider inventory, unresolved provider outcomes,
retention violations, cost/count/disk cap violations, missing/overdue durability
health, arithmetic overflow and malformed configuration disable recommendations.
The provider observation must cover every enrolled node, including revoked and
expired invitations, except explicitly revoked `limits.pool.retired_nodes` after
operator deprovisioning. Listed provider resources still count even when their
node is retired; forgetting a catalog node does not stop its billing.
Durability consumes the existing per-box `durability` observation and capture-age
objective; no second recovery timer or uploader is introduced. Missing or stale
observations fail closed. A stopped box is allowed to publish during the existing
drain workflow; a stale recovery point never grants permission to lose its disk.

A drain recommendation requires explicit existing drain eligibility, fresh
inventory, no unresolved operations/transfers/unmanaged reservations, and only
fresh stopped/hibernated, unpinned boxes. The base runner, `local`, configured
always-on/workstation exclusions, running boxes and unknown boxes are protected.
Reported worker workspace count must agree with catalog count. Returning demand
cancels a shadow soft drain and resets hysteresis; it cannot cancel a sealed or
effectful drain. A changed unsafe target invalidates the recommendation. Any
future executor must still use the [sealed empty certificate and retained-disk
boundary](DRAIN.md); a planner recommendation is not removal authority.

## Private configuration and status

Use the controller data directory containing `catalog.sqlite3`. Every input must
be an owned regular file, mode 0600, at most 256 KiB (the existing 64 KiB limit
still applies to `limits.json`). Replace files with atomic rename. Unknown fields
and any mode other than `shadow` are rejected.

Add explicit global elastic forecast caps to the existing `limits.json` pool:

```json
{
  "pool": {
    "max_estimated_cents": 10000,
    "hours": 24,
    "elastic": {
      "cents": 10000,
      "instances": 6,
      "disk_bytes": 4398046511104
    }
  }
}
```

Keep the existing node pricing and owner limits in that file. The effective
elastic cost ceiling is the smaller of `elastic.cents` and
`pool.max_estimated_cents`. There are no default elastic caps or prices.

Example `scaler.json` for a **synthetic shadow scenario**, with illustrative
budgets and forecast cost, not an instance recommendation:

```json
{
  "mode": "shadow",
  "pools": [{
    "name": "default",
    "floor": 1,
    "ceiling": 4,
    "cooldown_seconds": 300,
    "idle_seconds": 600,
    "join_timeout_seconds": 1200,
    "launch_allowed": true,
    "protected_nodes": ["base-runner", "always-on"],
    "replacement_headroom": 1,
    "presets": [{
      "name": "fixture-preset",
      "capacity": {
        "cpus": 4, "memory_mib": 8192, "slots": 4,
        "disk_bytes": 274877906944
      },
      "cost": {
        "cents": 2400, "instances": 1, "disk_bytes": 274877906944
      },
      "capabilities": {
        "backend": "firecracker", "worker_protocol": 3,
        "placement_fencing_v1": true, "managed_restore_v1": true,
        "cache_inventory_v1": true, "disk_resize_v1": true,
        "max_guest_memory_mib": 16384, "max_guest_vcpus": 16,
        "images": ["ubuntu"],
        "compat": {
          "runtime": "fixture-runtime", "cpu": "fixture-cpu",
          "kernel_sha256": "fixture-kernel"
        }
      }
    }]
  }]
}
```

Presets express effective operator admission budgets, not physical CPU ratios.
Use measured compatibility and current budgets; this feature does not change
existing overcommit ratios or derive an instance choice from a historic price
list. `launch_allowed` is an external scheduling input; no workweek, time zone,
instance type, price table or organization policy is built in. A null
`replacement_headroom` prevents launch forecasts because no finite maximum has
been established. A finite value is a shadow hypothesis, not provider proof.

`scaler-observations.json` is a private, complete provider snapshot supplied by an
operator or future concrete AWS adapter. This release does not fetch it. Example
empty-fixture observation (replace zero with the current Unix second):

```json
{
  "observed_at": 0,
  "hours": 24,
  "complete": true,
  "retention_ok": true,
  "outcome_resolved": true,
  "instances": [],
  "disks": [],
  "receipts": {}
}
```

Each instance has `id`, `pool`, nullable enrolled `node`, nullable `action`,
`state` (`ready`, `pending`, `retained`, `orphaned`) and `hourly_cents`.
Each disk has `id`, `pool`, full provisioned `logical_bytes` and `hourly_cents`.
Include attached, detached, pending, retained and orphaned billable volumes;
include other pools in the same global liability snapshot. Disk and instance
identities must be unique, and one node cannot map to several instances. The
snapshot expires after 15 seconds, including while the controller is offline.
An empty snapshot works only for an empty catalog. Complete provider accounting,
prices and retention assertions are trusted operator inputs, not cloud attestations.

Restart only the controller/gateway after installing these private inputs to
start five-second shadow iterations. The loop reloads inputs each time. It keeps
its prior intent on validation failure and exposes the error; removal of config
also disables iterations. No worker restart is needed. Local commands are:

```sh
ow --local --data-dir /private/controller scaler
ow --local --data-dir /private/controller scaler --tick
```

Status is local/operator-only and includes `mode`, error, plan/reason, action,
forecast reservation, cooldown and tick. Stale plans are marked `stale`; absent
config is reported through `config_present`. A duplicate or backward tick returns
the recorded plan without a second action. Shadow forecasts are reconsidered
after cooldown and can be cancelled without a provider receipt, because nothing
was dispatched. History remains in `scaler_actions`.

Effectful launch/join/drain phases are dormant reconciliation contracts exercised
by tests. There is no command that enters them or promotes shadow intent to live.
If a future executor enters one, an unknown reply or join timeout retains its
reservation and prohibits another action. Only exact-action positive absence or
a fresh capable enrolled runner correlated to one provider instance can settle
a launch. A heartbeat or desired-capacity value alone is insufficient. A matching
receipt can be `pending`, `unknown`, `absent`, or `joined` with an enrolled node;
receipts for another action do not advance this one. Controller restarts preserve
action identity, deadlines and cooldowns.

## Provider bound and live gates

AWS documents that retained ASG instances continue billing, leave desired
capacity accounting, and cause replacement launches. An ASG desired/max count
alone therefore does not enforce the planner's total billable-instance bound.
Repeated retention/replacement cannot be bounded by assuming just one spare.
An abandoned termination hook alone also does not retain an instance: the
instance lifecycle policy is required. See the official
[instance lifecycle policy](https://docs.aws.amazon.com/autoscaling/ec2/userguide/instance-lifecycle-policy.html)
and [retention configuration](https://docs.aws.amazon.com/autoscaling/ec2/userguide/configure-instance-retention.html).
This is documented upstream behavior, not a deployed-provider test.

**Live activation remains unavailable.** It requires the accepted replacement-
controller recovery drill; complete durable/cap accounting; a concrete provider
adapter whose rendered configuration and dedicated-fixture tests prove the total
replacement bound; and the existing sealed drain/dev-disk salvage gates. If ASG
autonomous replacements cannot enforce that bound, use a separately verified
bounded alternative or leave activation blocked. No cloud action, billing test,
live VM, new capacity, retention policy or infrastructure deployment was performed
for this feature.

## Compatibility and verification

Release 0.9.16 is class 1 (controller/catalog readers and operator client). Catalog
schema/reader floor 7 adds the action, health and cooldown journal. Migration
creates a private pre-upgrade SQLite backup and refuses older readers. The scaler
journal introduces no artifact roots; GC explicitly recognizes schemas 6 and 7
and still rejects unknown future root contracts. Preserve
the latest catalog when rolling forward; no downgrade writer is supported.
Worker protocol 3, accepted worker interfaces 1–3, host protocol 1, runtime,
guest kernel and host layout 2 remain unchanged. Keep running worker pins.

Run `cargo test --locked -p ow --bin ow scaler` on Linux for pure planning and
real-catalog fixtures; the pure subset also runs on macOS. Tests cover fragmented
mixed shapes, every admission dimension, unknown compatibility, replacement
headroom, floor/ceiling, queueing at caps, schedules, cooldown/flapping, protected
and unknown boxes, soft-drain cancellation, duplicate/concurrent ticks, reopen,
failed join, lost reply and positive reconciliation. Fixtures never call AWS or
start a VM. Release/version checks live in `deploy/aws/test`.
