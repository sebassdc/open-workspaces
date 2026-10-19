# Controller owner limits and pool cost admission

The controller enforces per-owner provisioned CPU, RAM, logical root disk and box
count before create, fork, resize and restore are dispatched. All owned boxes
count, including stopped and hibernated boxes. A fork belongs to its recipient;
a shared machine's restore charges its original owner. Sharing grants no quota
bypass. The trusted local worker socket remains operator maintenance access.
Worker/host physical admission still applies independently.

Defaults per owner are **16 CPUs, 32768 MiB RAM, 256 GiB disk and 8 boxes**.
Create reserves the requested disk or the normal 32 GiB default; forks use the
source's resource shape, or the named snapshot's CPU/RAM shape. Legacy snapshots
do not report disk size: the source's current grow-only disk is a conservative
upper bound. Unknown disk telemetry is charged at the maximum supported 16384 GiB
until a positive inventory observation is available. Unknown CPU/RAM fields
likewise use the maximum supported shape. Pre-policy unresolved effects without
an allocation record retain that maximum until reconciliation resolves them.
Native Mac creates lack
disk-capacity advertisement and reserve that same conservative maximum; they
need an explicit administrator disk allowance rather than assuming a Linux
default applies. Named snapshots and backups
are not writable boxes and are outside these logical root disk limits; host
headroom and retention must bound their storage separately.

Reservations commit in the same SQLite `BEGIN IMMEDIATE` transaction as placement
and the operation journal. Retries count once. Pending/uncertain effects retain
demand across controller restarts and offline runners; only positive inventory
and resolved effects release it. A rejected operation with confirmed absence
releases its demand. Confirmed deletion releases the allocation; forgetting an
offline/missing catalog row is refused while an effect remains unresolved. Reducing limits never
stops or resizes a box; existing starts, exec and same-size/reducing changes keep
working. Additional resource growth must fit the corresponding limit.

## Private operator configuration

Place `limits.json` in the **controller data directory** (the directory containing
`catalog.sqlite3`, normally `~/.ow/controller` for `ow host up`). It must be an
owned regular file, mode 0600. It is read on each admission/state request; no
worker, host or gateway restart is needed to change it. Use an atomic rename
inside that private directory when replacing it. Malformed, public-readable,
unknown-field and out-of-range configuration fails closed, never silently
reverting to defaults. A missing file uses defaults; new runner authorization
still requires explicit pricing below.

```json
{
  "owner_defaults": {
    "cpus": 16, "memory_mib": 32768, "disk_gib": 256, "boxes": 8
  },
  "owners": {
    "7": { "cpus": 32, "memory_mib": 65536, "disk_gib": 512, "boxes": 12 }
  },
  "pool": {
    "max_estimated_cents": 10000,
    "hours": 24,
    "hourly_cents": { "example-instance": 100, "self-hosted": 0 },
    "node_types": { "runner-a": "example-instance", "local-host": "self-hosted" },
    "retired_nodes": []
  }
}
```

These are illustrative prices, not current provider prices. Rates and caps are
integer cents in one operator-selected currency; round fractional hourly cents
up. `hours` is a forecast horizon (1–87600), not a billing period that resets.
Each estimate is `hourly_cents[instance_type] * hours`. Default pool cap is
**10000 cents over 24 hours**. There is no default price or instance type: unknown
pricing refuses new runner authorization. For a wholly self-hosted pool, an
operator may explicitly set `pool.default_type` to `self-hosted` with that type's
rate set to zero. A mixed/cloud pool should use explicit node-to-type mappings.
The operator must keep declared types/rates aligned with actual capacity.

`owners` keys are positive durable catalog user IDs, not emails or caller-supplied
IDs. Each entry replaces all four default caps; zero denies new demand in that
dimension. A local administrator can inspect bindings with a read-only query:

```sh
sqlite3 -readonly /path/to/controller/catalog.sqlite3 'SELECT id,issuer,email FROM users;'
```

Only trusted configuration can override limits. API request fields cannot grant
an override. `GET /api/state` adds `owner_limits` with `caps`, `usage` (the same
four fields) and an accounting description for the authenticated caller alone;
it never returns other owners' overrides or usage. Limit errors identify the
resource, proposed total, cap and the next action.

## Adding pool capacity

Invitations reserve an authorized node slot in the existing node catalog.
Controller invitation minting, consumption and increases to enrolled-node budgets
check the total pool forecast under a SQLite immediate transaction. A refusal
rolls back invitation state before publishing a secret. Concurrent invitations
cannot each consume the last available budget, and a cap changed after minting
is checked again at enrollment. Replacing an unused invitation counts its node
once. Failed publication rolls back its reservation.

Every catalog node counts: the controller's local runner, unused or expired
invitations, offline runners and revoked nodes. Hibernating a guest or revoking a
runner does not establish that its host stopped costing money. After the
operator deprovisions a host, revoke its node and explicitly list its ID under
`retired_nodes` to release its forecast. An active node cannot be retired; retired
IDs cannot be enrolled again. A lowered spend cap blocks additions and increases,
without stopping existing capacity or preventing its normal reconnect.

This is a **capacity admission estimate**, not observed provider billing, an
elapsed-spend ledger or an autoscaler. It excludes storage, transfer, taxes and
other provider charges. It does not intercept manual cloud/IaC provisioning;
future capacity provisioners must obtain the controller invitation reservation
**before** creating billable capacity and must retire it only after deprovisioning.
Existing cloud deployment costs must be configured accurately before adding
runners. No cloud resource, pricing lookup or actual spend is needed for tests.

## Upgrade and verification

This slice changes only controller code and adds `owner_allocations`; runtime
reservations, worker interfaces and guest state are unchanged. Under the v0.5.0
split-unit layout it is a **class 1 controller upgrade**, and subsequent policy
edits are class 0. Upgrade the controller/gateway only; preserve the worker and
VM service. Existing usage is accounted for, never evicted. Downgrading to an
older controller loses these admission guards (its additive schema remains
readable), so do not downgrade an untrusted multi-owner pool without another
admission gate. Release version/tag selection belongs to the release owner.

Run `cargo test --locked --workspace` on Linux. Focused cases are
`cargo test --locked -p ow owner_quota` and
`cargo test --locked -p ow pool_spend`. They cover exact boundaries, every cap,
overrides, config validation, transaction concurrency, stopped demand, failure
versus uncertain outcomes, restart, sharing, snapshot resource shape, unknown
prices, stale invitations, retirement and arithmetic overflow. These tests use
fake workers and no provider credentials. They do not claim real VM or cloud
billing acceptance; controller policy does not require a worker protocol change.
