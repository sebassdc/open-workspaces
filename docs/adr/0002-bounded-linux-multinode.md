# ADR 0002: Outbound authenticated Linux nodes with fixed placement

Date: 2026-10-03. Status: implemented for the bounded prototype; independent
acceptance passed; final stable-commit source review and planner deployment remain pending.
Pattern: Rust gateway + SQLite ownership + private Unix worker, under ADR 0001.

## Decision

Reuse the existing Firecracker/Btrfs runtime, guest PTY protocol and human gateway.
Add an isolated native TLS node controller and outbound node agent to the same
Rust binary. Node authority is separate from human Access assertions. Enrollment
uses an operator-minted node-bound expiring one-use 256-bit secret; persistent
node credentials are scoped bearer secrets whose digests/revocation live in SQLite.
No third-party broker, paid/vendor transport, SSH management or new hypervisor.

A bounded control WebSocket dispatches one-use node/generation-bound job IDs.
Outbound job WebSockets bridge bounded private Unix requests/PTY streams. Host
sockets and credentials never enter guest workloads. Latest authenticated session
wins; old input/output, results, heartbeats and cleanup cannot operate in the new
generation. Revocation denies dispatch/reconnect and closes bridges within bounded
poll/write time; it cannot cancel effects already accepted on a disconnected host.

Schema v3 adds immutable resource placement and CPU/RAM reservations, owner-scoped
operation keys/fingerprints/results and safe status visibility. Commit intent and
placement before dispatch. Never relocate an offline/unknown resource. Snapshot,
restore and fork require matching owner/node/source; fork children remain local to
the source host. Agent persist-before-execute result journaling prevents replay of
completed effects and exposes crash-pending uncertainty. Fixed-ID create/fork/
capture can reconcile through existing runtime checks; arbitrary unknown exec
cannot be inferred from inventory. A lifetime gateway flock makes abandoned
pending-operation recovery safe for this single-controller slice.

Use a homogeneous Linux x86-64 Firecracker v1.17.0 pool, bounded typed prepared-image
capabilities and conservative scheduler reservations. Reconcile only resources
registered on the reporting node; account unknown local guests without adopting
ownership. Runtime admission remains authoritative. Shared physical hosts require
operator-configured static disjoint worker budgets whose total leaves host reserve;
no physical-host auto-discovery/scheduling claim. Free-space preflight preserves a
minimum reserve, but quotas and atomic cross-process disk admission are deferred.

## Why this fits the vertical slice

Private Unix proxies preserve existing request/terminal APIs and legacy local
behavior while limiting new transport code. SQLite transactions plus fixed IDs
provide reviewable recovery semantics without a distributed database or scheduler
framework. A dedicated TLS listener avoids relying on browser cookies or broad
Access exceptions. The node channel is self-hostable with ordinary certificates.
The shared private-pool visibility policy lets all admitted humans select safe node
labels/capabilities; only local OS operators enroll or revoke nodes.

## Evidence and consequences

See [MULTINODE](../MULTINODE.md) for commands, bounds, security/retry/migration
contracts and exclusions, and [builder report](../plans/node-core-report.md) for
exact test results. Two isolated same-host workers passed a real private-CA TLS
slice with at most three guests/768 MiB and static total capacity 1024 MiB. This
establishes routing/recovery logic on one host, not physical/NAT acceptance or
cross-host snapshot portability. Focused mock/transport evidence is kept distinct
from actual guest behavior. Independent source review and human HTTP/VM acceptance
are separate release gates.

Catalog upgrades preserve local IDs/ownership and reject newer schemas. Rollback
uses the private pre-upgrade SQLite backup plus prior binary; automatic downgrade
is unsupported. No existing catalog or service was deployed/migrated by node-core.
Bearer theft permits node impersonation until revocation; node authentication is
not attestation or confidentiality from the host operator. Existing confinement,
storage retention and delayed human JWT revocation gaps remain. No migration,
SSH/Mac runtime, marketplace, production-readiness or benchmark claim is added.
