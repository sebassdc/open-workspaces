# Mac node — bounded integration rollout
Updated: 2026-10-05 | Size: L | State: authorized, in progress
Pattern: ADR0002 outbound authenticated nodes + ADR0004 explicit backend capabilities.

## Goal
Merge reviewed PR6 and prepare the live private pool for a dedicated Apple Silicon
node, preserving existing Linux guests and all pending management files.

## Rollout
```mermaid
flowchart LR
 R[Reviewed source] -->|merge and build| B[Release binary]
 B -->|backup then replace| C[Controller and dashboard]
 C -->|one-use invitation| M[Native Mac node]
 M -->|owner-scoped acceptance| P[Private pool accepted]
```
Each box is a rollout stage; arrows name the required transition. The final
acceptance is pending until the owner/Mac agent executes it.

## Steps
1. Merge pinned PR6 head829b7fe; compile the release from merged source.
2. Save private SQLite/config/process/binary rollback and guest identity inventory.
3. Restart only gateway/controller with the verified binary; verify authenticated
   public ingress, existing nodes and unchanged Linux guest identities.
4. Publish Mac build/enrollment/acceptance instructions on main and mint a private
   one-use invitation, never committing its contents.
5. Mac agent joins a dedicated root and proves real dashboard placement, terminal,
   binary files, persistence, reconnect, owner denials and capacity.

## Blast radius and stop gates
Existing browser terminals and node control channels reconnect on service restart.
No worker/guest/tunnel restart, asset replacement, unrelated cloud change or spend.
BUILD: do not deploy a failing/unattributed build. PRESERVATION: rollback service
binary/config on failure; never restart an owner's guest to conceal it.
ACCESS: transfer invitation privately; never put credentials in GitHub or commits.
RESOURCES: Mac acceptance uses one guest <=1024MiB/2vCPU initially, one-slot owner
cap, <=20GiB dedicated storage, preserving prior fixtures and headroom.

## Done when
Scoped merge/build/live-service checks and resumable Mac instructions are published.
The Mac is only marked accepted after actual human gateway/dashboard evidence.
