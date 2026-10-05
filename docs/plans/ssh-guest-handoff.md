# Handoff: guest OpenSSH
Updated: 2026-10-04 | Size: L | State: done; planner RELEASE review pending

## Goal
Provide ordinary guest OpenSSH through local and outbound Linux workers, with owner authority, bounded fenced streams and safe key/fork identity lifecycle.

## Where it stands
- Done: implementation and commands in [docs/SSH.md](../SSH.md); contract in [ssh-guest-contract.md](ssh-guest-contract.md).
- Done: frozen native binary tested with 49 serialized offline tests, Ubuntu 23 real SSH checks, Arch 23 and legacy Alpine 5. Exact methods, failures and limits: [ssh-guest-report.md](ssh-guest-report.md).
- Done: private workers/VMs/helpers stopped; shared process start identities and owner disk inode unchanged. Exclusive host turn released to planner.
- Not started: integration, versioned image publishing, deployment, remote/NAT or Mac hardware validation. These require planner review and appropriate authorization.

## Next step
Planner reviews this lane's scoped commit against the brief and Mac capability contract before integration. Use `git log -1` for the commit containing this note; keep source stable during review.

## Open decision
None for implementation. RELEASE remains the planner's review gate.

## Files
- Brief: `docs/plans/ssh-guest-brief.md`; coordination: `docs/plans/mac-ssh-coordination.md`.
- Branch: `work/ssh-guest`, baseline `b1703f1`; changed Rust SSH/client/gateway/catalog/node/runtime modules, guest recipes, acceptance scripts and scoped documentation.
- Private evidence: `data/ssh-evidence/final-checks.json`, `pins.json`, logs; final receipts `v/v/result.json`, `v/w/result.json`, `v/x/result.json`.
- Frozen binary: `data/ssh-evidence/frozen/ow`, SHA256 `38a6303328ebdb718efa45963cfb8f83241f306880b294ff79ecf0cc19d3a479`. Source-map digest and per-file pins are in the report/private evidence.

## Gotchas
Never upload ignored evidence: it contains private keys, synthetic credentials and guest snapshots. Never reuse retained fixture roots or boot without an exclusive planner host turn. One guest maximum; parent/child acceptance is sequential. Keep owner guests/assets, live services and main management files untouched; no push/merge/deployment occurred. Existing images need explicit opt-in provisioning; missing `guest_ssh_v1` denies remote SSH. Native debug evidence does not establish Mac/static/public ingress/editor support. A parallel existing recovery test flaked; the complete serialized suite passed. VMM logging must remain separate from guest serial to preserve command completion framing.
