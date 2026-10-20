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

The catalog uses schema version 5 (minimum reader 5), foreign keys, uniqueness constraints, WAL and
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
multi-worker scheduler. Per-owner controller admission is described in [limits](LIMITS.md).
PostgreSQL, fair scheduling, audit
retention and full database/artifact backup recovery remain tracked follow-ups.

Dormant placement authority, transfer reservations and the private pre-migration
backup gate are described in [the lifecycle contract](LIFECYCLE.md). They do not
activate movement for existing fixed-placement boxes.

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
worker IDs. Local host access is not a restricted tenant login. Machine sharing
and pool admins are below; teams and workload tokens are not implemented.

## Sharing a machine

An owner lets a teammate use one machine:

```sh
ow share demo --with bob@example.com [--role use|admin]
ow unshare demo --with bob@example.com
```

Bob addresses it as `alice@example.com/demo` (the owner's login email, a slash,
the machine name): `ow shell alice@example.com/demo`, `ow exec …`, `ow ssh …`.
`ow list` and the dashboard's "Shared with me" section list machines shared with
you, with their `owner` and `role`; the owner's own list shows `shared_with` for
each machine, and the dashboard has Share and Unshare buttons.

| Role | Allows |
| --- | --- |
| `use` (default) | exec, shell, SSH stream, `put`/`get`, `ssh-info`, start, hibernate, fork into their own namespace |
| `admin` | `use`, plus stop, snapshot, restore, deleting their own snapshots, `ssh-keys` and the box's [AWS role](AWS_ACCESS.md) |

Resize, rename, delete and sharing stay with the owner. After `ow rename demo
web`, Bob uses `alice@example.com/web`; his grant, open shells and SSH sessions
carry over. Delete removes the machine, its
disk, every snapshot taken from it (a grantee's too) and its grants; past
operations stay in the history as `deleted:NAME`, and the name can be used again.
The catalog keeps grants in `grants(physical, grantee, role)` (an additive table:
an older binary ignores it, so grantees lose access rather than gain it) and resolves every request to a physical ID through the owner or a grant before
any worker contact, as for owned machines. Sharing with an email nobody has used
yet reserves it: the first verified login with that email (which must still be
admitted by the Access policy and allowlist) binds it. Sharing grants no login.

`ow unshare` deletes the grant and closes the teammate's open terminal and SSH
streams to that machine at once; their next request is refused. It does not wait
for requests already running (an exec the teammate started still finishes). It does not
remove SSH keys an owner or admin registered for them (they can no longer reach
the guest's SSH through the gateway, but remove them with `ow ssh-authorize` if
the guest is reachable another way), and it does not undo what they already did
inside the guest.

**A fork copies the owner's credentials.** A fork is a full copy of the
machine's disk and memory: tokens, SSH keys, cloud credentials, browser
sessions and anything else on it. A grantee's fork (and an admin's snapshot)
lives in the grantee's namespace and stays theirs after `ow unshare`. Only share
machines whose contents you would hand to that person, and rotate credentials
on the machine after unsharing if that matters.

For SSH, the owner registers the teammate's public key next to their own:
`ow ssh-authorize demo --key ~/.ssh/id_ed25519.pub --key bob.pub`
(`ssh-authorize` replaces the whole key list). `use` grantees cannot change keys.

Evidence (2026-10-16): `catalog::multinode_tests::sharing_roles_owner_addresses_and_revocation`
(roles, addressing, reserved email binding, restart) and
`remote::tests::unshare_closes_a_grantees_live_terminal` (gateway, signed
tokens, real WebSocket terminal, fake worker, unshare while the grantee's slow exec
is in flight; terminal confirmed open before revocation) run with `cargo test`. The ignored
`remote::tests::sharing_real_vm` passed on two throwaway `deploy/aws` hosts
(m8i-flex.xlarge, Ubuntu 24.04, Firecracker 1.17.0, Rust 1.97.0): the grantee
got nothing before the share; after it, saw `owner@example.test/box` under
`shared`, ran exec and an interactive terminal in the owner's guest, was refused
resize, and forked a copy holding the owner's file; `unshare` closed the open
terminal in under 1 ms, then exec and terminal were refused, the fork stayed
the grantee's and the owner was unaffected. One sample per host; signed test tokens
stand in for two real Cloudflare or tailnet logins.

## Pool admins and transfers

Owners are the unit of ownership; there are no teams or tenants. Moving a box
"to another team" means transferring or forking it to an owner there. Pool admins
are login emails in the gateway config's `pool_admins` list (default empty: no
admins). Their verified login, bound in the catalog like any other, is checked;
listing an email admits nobody who is not already admitted. A pool admin is
different from a machine's `admin` share role.

| Action | Owner | Pool admin | Shared `admin` / `use` | Stranger |
| --- | --- | --- | --- | --- |
| `ow transfer BOX --to EMAIL [--keep-access]` | yes | yes, any box | no | no |
| `ow fork BOX CHILD --owner EMAIL` | yes | yes, any box | no | no |
| `ow create NAME --owner EMAIL` | no | yes | no | no |

A pool admin reaches another owner's box (`alice@example.com/BOX`) to transfer,
fork or manage its [AWS role](AWS_ACCESS.md). The dashboard lists those actions
under Pool machines; AWS roles must still be allowed for the box owner. Exec,
shells, SSH, lifecycle and sharing still need ownership or a share. A pool admin's fork of another owner's box copies everything on it into a
box the admin chooses the owner of, so the list should hold only people trusted
with every box in the pool.

**Transfer** changes only catalog rows, in one SQLite transaction, with no worker
contact: the box keeps its physical ID, disk, placement, published ports, SSH keys
and running state, and the snapshots its owner took of it move with it, as do
the stored recovery captures and snapshot artifacts in the artifact store that
carry the owner, so store GC keeps them and the new owner can restore or move
from them. The new
owner gets full owner rights and addresses it as `NAME`; others use
`new@example.com/NAME`. Every share ends (a teammate's or the new owner's), and
the open terminals, SSH streams and preview connections of everyone who lost
access close; with
`--keep-access` the old owner gets an `admin` share. The old owner's name is free
again; their operation history stays, renamed `transferred:NAME`. Snapshots a
grantee took stay theirs. It is refused when the new owner already has a machine
or snapshot of the same name, when the box is still being created, or has an
unfinished move, park, wake, fork, capture, start, resize, restore or delete, and
when the box would push the new owner past any of their CPU, RAM, disk or box
limits (the refusal names the limit, not their usage). An
email nobody has used yet is reserved, as for sharing.

**Create and fork for someone else** reserve the new box, its operation and its
quota in that owner's name space; the result names the `owner`. The actor's
`--operation-key` is scoped to the actor, so it never replays the owner's own
operations; a retry without a key is a new request. A fork for another owner
works like any fork, also for a moved, parked-and-woken box or with `--node`
(the cross-runner path); `--snapshot` names the forker's own snapshot, or for a
pool admin the box owner's. Like every fork, it inherits no shares or SSH keys.

**Audit.** Every transfer, create for someone else, fork into someone else's name
space and pool admin fork is a row in the catalog's `audit` table (actor, action,
machine, source, old and new owner, keep-access, time). The actor and both owners
see it in `ow audit` and the dashboard's Ownership changes list. The table is
additive: catalog schema 7 is unchanged and an older controller ignores it.

Evidence (2026-10-20): `catalog::multinode_tests::transfer_by_owner_or_pool_admin_keeps_the_box_and_moves_its_owner`
and `catalog::multinode_tests::pool_admins_create_and_fork_for_others_and_owners_fork_for_others`
(permissions for owner, pool admin, shared admin and stranger; quotas; a running
box keeps running with no worker call; name collisions; streams closed; restart),
and the dashboard tests in `e2e/tests/admin.spec.mjs` against a fake worker.
No real VM or second real login was used.

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

Guest SSH streams and SSH key/info operations follow the same owner-first
SQLite lookup and fixed node placement as terminal/exec access. Keys are
registered explicitly per machine; the host-side policy overrides older
snapshot authorization and is not inherited by fork children. The trusted local
operator interface retains maintenance authority. See [SSH](SSH.md).
