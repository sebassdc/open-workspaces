# Per-user machines and control-plane metadata

The dashboard and remote HTTPS/WSS CLI enforce ownership using a private SQLite
catalog on the single-host prototype. Authentication still verifies Cloudflare
Access signature, issuer, audience, subject, email allowlist and expiry. A user is
identified by verified issuer + subject. Email is a display/admission field, not
a caller-supplied owner ID. An already-bound email cannot claim a different
subject's machines.

Each user has a namespace for machine and snapshot names. Two users can both
create `demo`, capture `prepared` and fork `branch`; these names map to distinct
random worker IDs. SQL lookups require the authenticated owner before any exec,
lifecycle, snapshot/fork/restore or terminal request reaches the worker. Guessing
another user's logical name or physical worker ID grants no access. Lists,
snapshots and per-machine/total usage include only the current user's machines.
Machine names are logical API names; newly created guests use their opaque
worker ID as their hostname. Global configured worker admission limits remain visible; other users' usage does
not. The dashboard shows the account associated with its catalog identity.

Guest root remains guest-only. The existing network policy blocks peer, private,
LAN and host destinations. This feature does not grant guest workloads Cloudflare
administration, database access, host filesystem access or hypervisor sockets.

## Database and recovery boundary

`catalog.sqlite3` under the private worker data directory stores:

- Users and their verified identity binding.
- Immutable ownership and user-name/worker-ID mappings for machines and snapshots.
- Machine metadata including image, CPU, RAM, state, source and hibernation checkpoint.
- Snapshot metadata, with its source workspace and resource shape.
- Operation IDs, owner, operation/resource name, timestamps and pending/succeeded/failed status.

The catalog uses schema version 2, foreign keys, uniqueness constraints, WAL and
FULL synchronous writes. Its database must be an owned regular file with mode
600; SQLite sidecars inherit private permissions. SQL parameters are bound,
and request bodies cannot specify an owner. Ownership is committed before
calling the worker. A failed create/capture leaves its reservation available for
the same owner's retry; worker inventory refresh reconciles observed metadata.
Worker-generated hibernation/fork checkpoints inherit their registered parent
owner. New locally-created unregistered machines are hidden from remote users.

The worker's `state.json` and snapshot manifests are **still its private runtime
recovery journal**. Machine and snapshot worker IDs are unique within their separate kinds, so
legacy resources may safely share the same name. SQLite is authoritative for remote ownership/names and the
control-plane catalog; it does not replace paired RAM/disk snapshot artifacts.
Stopping/restarting the gateway retains ownership. Do not delete the database to
reset login or copy a live WAL database as an ordinary file backup: use SQLite's
backup API or an offline copy including a clean checkpoint. All database files,
identities and VM artifacts stay out of GitHub.

An operation spans SQLite and the worker, so it is not one atomic transaction
across both. A gateway crash after dispatch may leave `pending` work with an
unknown outcome. Reconciliation and stable create/snapshot IDs prevent a missing
ownership mapping, but exactly-once exec and comprehensive operation recovery
remain open. The current gateway serializes catalog operations; it is not a
multi-worker scheduler. PostgreSQL, per-user quotas, fair scheduling, audit
retention and full database/artifact backup recovery remain tracked follow-ups.

## Existing machines and adding users

Before the first catalog startup with existing machines, put an explicit
`bootstrap_owner_email` in the private gateway config. It must be an allowed
email. A transaction assigns existing machines and checkpoints exclusively to
that reserved owner; the first valid login for that email binds the verified
subject. Another allowed user logging in first sees an empty personal namespace.
Later starts never reassign existing ownership based on config order. An
identity-provider change that changes subject needs a deliberate operator
migration; it cannot reclaim machines by recycling the email.

Keep new admission explicit. Add an intended email both to this project's Access
policy and the gateway allowlist using the existing Access configuration helper
(`--email` repeats the full intended list). That helper preserves the bootstrap
owner when adding users. Restart only the project gateway after config changes;
no VM restart or extra domain is needed. No additional real user was invited as
part of implementing this feature.

`ow login <HTTPS origin>` selects the remote authenticated account; `ow list`,
`ow exec`, `ow shell` and lifecycle operations use that account's logical names.
The local operator CLI/socket remains trusted maintenance access to all physical
worker IDs. Local host access is not a restricted tenant login. Per-user sharing,
teams, administration roles and workload tokens are not implemented.

## Acceptance evidence

Automated tests verify explicit legacy migration, identity binding, rejection of
email reuse with a different subject, and ownership persistence after reopening
the database. Schema-upgrade tests also check that a machine and checkpoint
can share a legacy name while retaining separate metadata. A signed two-identity gateway regression boots real microVMs and
checks same-name machines/checkpoints/forks with independent guest files, all
cross-owner lifecycle/exec denials, private snapshot/fork/restore denial, physical
ID denial, WSS ownership denial before a worker connection, scoped statistics
and catalog restart persistence. Artifacts: `data/users-0c8bda74` (ignored).

The real VM browser/TLS/WSS CLI regression also passed after ownership was
introduced (`data/ui-32df3eb7`, 21 checks). These local signed-token tests do not
claim a second real person's Cloudflare/browser login was exercised.

```bash
export RUSTUP_HOME="$PWD/data/runtime-spike/rustup"
export CARGO_HOME="$PWD/data/runtime-spike/cargo"
"$CARGO_HOME/bin/cargo" +1.97.0 test --workspace
"$CARGO_HOME/bin/cargo" +1.97.0 test -p ow users_isolated_real_vm -- --ignored --nocapture
"$CARGO_HOME/bin/cargo" +1.97.0 test -p ow dashboard_browser_real_vm -- --ignored --nocapture
```
