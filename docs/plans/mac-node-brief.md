# Mac pool node — authorized implementation brief
Size: L | Pattern: ADR0002 outbound authenticated nodes, owner/fixed placement; ADR0003 native Apple Virtualization.
State: implementation authorized by mac-node-integration.md. Baseline 9acb963.

## Goal
Persistent Ubuntu ARM64 machines on a managed native Mac worker, routed through the existing authenticated private pool.

## Data flow
```mermaid
flowchart LR
 C[Existing dashboard and CLI]:::old -->|owner scoped requests| G[Gateway scheduler]:::new
 G -->|fenced outbound jobs| A[Native node agent]:::new
 A -->|private Unix operations| W[Mac worker and journal]:::new
 W -->|bounded helper requests| V[Signed Apple VM helper]:::new
 V -->|private virtio socket| U[Persistent Ubuntu ARM64]:::new
 classDef old fill:#eee,stroke:#aaa,color:#777
 classDef new fill:#dfd,stroke:#282,stroke-width:3px
```
Grey existing; green new/extended. No inbound Mac network listener or shared host directory.

## Steps
1. Attribute previous evidence to merged source; inventory/headroom, native tool/runtime provenance.
2. Signed Ubuntu ARM64 persistent image and private guest command/file/PTY transport.
3. Managed worker with durable conservative demand, local consent, bounded lifecycle and recovery.
4. Reuse node transport; backward-compatible backend/image/operation contracts and focused compatibility/denial tests.
5. Stable scoped commit and native evidence; planner invitation/revision and Linux review precede live pool acceptance.

## Blast radius / acceptance
One dedicated private root, one new guest <=2 GiB/2 vCPU, <=20 GiB retained storage. No other services affected. Verify real exec status, binary files, real PTY/resize/interrupt, /home/dev cold persistence/recovery, admission and unsupported operations; node TLS/reconnect/fencing/ownership require separate pool evidence. Local state capture is not enabled as product capability.

## Named gates
AUTHORITY: weaker TLS/auth/fencing, unsafe networking or host exposure. RESOURCES: over-budget or existing service effects. CONTRACT: shared wire conflict, refer bounded changes to planner. RELEASE: no push/merge/deployment or live asset/controller changes. ACCESS: live acceptance waits for private invitation and exact coordinated controller revision. Tahoe outside scope.
