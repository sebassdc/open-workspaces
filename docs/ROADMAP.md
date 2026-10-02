# Roadmap and acceptance gates

All implementation is pending. Estimates are planning ranges for 2–3 experienced engineers, not commitments; runtime, security, staffing, and product decisions can materially change them.

## Phase 0 — Feasibility (1–2 weeks)

- Inspect actual development machine and record capabilities.
- Pin and evaluate Firecracker and Ignition in isolated experiments.
- Demonstrate boot, exec, file transfer, HTTP readiness, persistent disk, full-state capture/restore, independent clone.
- Record runtime compatibility, identity/entropy design, storage mechanism, latency and resource use.
- Decide runtime/license/storage in an ADR. If memory branching is blocked, identify the blocker and keep disk-only capabilities honest.

Gate: reproducible evidence and runtime decision, per HANDOFF.md.

## Phase 1 — Usable alpha (6–10 additional weeks)

- Rust workspace, API/worker/guest-agent/CLI; reproducible local build and host setup.
- One authenticated operator and one worker; persistent desired/observed state and idempotent operations.
- Create/list/inspect/exec/files/start/stop/reboot/delete, streamed output, exit codes, readiness checks.
- Resource limits, isolated guest networks, scoped credentials and basic host-enforced network policy.
- Persistent disk, initial immutable base images, protected HTTP publishing and explicit public routes.
- Off-worker disk backup and restore onto a replacement worker; basic logs and operational metrics.

Gate: create -> usable shell -> run HTTP app -> publish URL -> write file -> restart -> verify file -> backup -> replace worker -> restore data. Verify unauthorized operations are rejected and API/worker restart does not duplicate machines.

## Phase 2 — Stateful beta (6–10 additional weeks)

- Named versioned RAM+disk snapshots, full-state forks, in-place checkpoints.
- Identity refresh, clone write independence, artifact integrity and compatibility checks.
- Pause/resume, hibernation, coalesced wake-on-request, keep-running task leases.
- Golden environments, disk lineage/GC, additional disks with single-writer leases.
- Backup scheduling/retention, raw forwarding, network labels, route-specific wake policy.
- TypeScript SDK, Python SDK, SSH/editor integration and minimal dashboard.

Gate: fork a service with an in-memory counter, prove restored process state and independent disk writes; hibernate and resume; ensure active task leases prevent inappropriate sleep; reject corrupt/incompatible captures; test capacity exhaustion and safe artifact cleanup.

## Phase 3 — Teams and enterprise (8–12 additional weeks)

- Organizations/teams/roles/invites, scoped API keys, private/shared access.
- OIDC SSO, audit export, project policy, configurable encrypted storage, private networking.
- Credential sharing/restore policy across memory and stored artifacts.
- Multi-worker placement, fencing, recovery and draining; replicated control plane where needed.
- AWS infrastructure module and Helm chart, upgrade/rollback documentation, backup recovery objectives.
- Usage metering and quotas. Billing/payment integration only if a hosted commercial offering requires it.

Gate: demonstrated tenant separation, revocation behavior, worker-loss recovery, safe upgrade, backup restoration and documented RPO/RTO. No high-availability claim without failure testing.

## Phase 4 — Extended product parity (several further months)

- Remote desktop, browser automation, persistent browser profile, human takeover.
- Opt-in local filesystem/clipboard/browser bridge with explicit capabilities and device selection.
- GitHub integration first; then selected services through a credential broker and connector interface.
- TypeScript scripts, event/cron triggers, durable state, logs/retries/connection waiting.
- Host-bound secret injection only after a separate protocol/security design.
- Optional agent framework adapters and CLI skills; prioritize actual user demand.

Gate: end-to-end automation survives sleeping guests and service restarts, uses scoped connections, exposes failures, and prevents shared/isolated guests from accessing personal devices or connections.

## Release discipline

- Track capability completion by WS IDs in RESEARCH.md.
- Keep every release's documented behavior and unsupported operations explicit.
- Benchmark request-to-usable-shell and request-to-first-HTTP-response, not only VMM load time.
- Tests should verify real persistence, recovery, cancellation, isolation and branching outcomes.
- Defer broad enterprise scaffolding and a hundred-service catalog until the core vertical slice works.
