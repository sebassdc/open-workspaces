# Roadmap and acceptance gates

The runtime spike produced a runnable local Rust/Firecracker prototype; see
[usage](LOCAL_PROTOTYPE.md) and [evidence](experiments/runtime-spike.md).
The broader acceptance gates remain open. Estimates are planning ranges for
2–3 experienced engineers, not commitments; runtime, security, staffing, and
product decisions can materially change them.

## Completed local pilot checks

- Real guest boot, exec/exit status, binary file transfer and loopback HTTP access.
- Persistent disk, immutable paired RAM/disk snapshots, independent full-state
  forks and hibernation/resume with HTTP process memory retained.
- Rust CLI/worker/gateway with owner-only control access, basic namespace
  networking, conservative resource admission and live memory statistics.
- Repeated operations, tampered artifacts, guest-root network attempts, cold
  restart and abrupt worker-loss recovery exercised by real-VM regression.

Still needed: cache-cold and memory-intensive capacity measurements, comprehensive
isolation/recovery tests, jailer/cgroup hardening, larger images, remote CLI authentication,
internet egress policy, off-worker backups, retention/GC and authenticated public
access. Do not treat the entire alpha or feasibility gate as complete.

Authenticated Internet access continuation: the single guest-app JWT gateway
and local negative/forwarding tests are implemented. Cloudflare tunnel login and
dedicated tunnel/Access policy creation and DNS routing succeeded. External
anonymous/forged browser requests redirect to login; positive owner login and
session revocation checks remain pending;
see [the deployment acceptance criteria](REMOTE_ACCESS.md). A browser management
dashboard is now implemented on that existing hostname, including create,
guest exec, paired snapshots/forks, hibernate/resume, checkpoint restore and
memory use. Real browser/VM tests pass, including native guest PTYs over authenticated
WebSocket. Remote CLI authentication and per-workspace roles
remain unfinished. See [dashboard evidence](DASHBOARD.md).

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
- Initial local pilot includes named paired RAM+disk snapshots and independent full-state forks, with identity/entropy refresh and measured fan-out resource use. Validate the primitives in Phase 0 before productizing them.
- Resource limits, isolated guest networks, scoped credentials and basic host-enforced network policy.
- Persistent disk, initial immutable base images, protected HTTP publishing and explicit public routes.
- Off-worker disk backup and restore onto a replacement worker; basic logs and operational metrics.

Gate: create -> usable shell -> run HTTP app -> publish URL -> write file -> restart -> verify file -> backup -> replace worker -> restore data. Verify unauthorized operations are rejected and API/worker restart does not duplicate machines.

Initial pilot gate also includes snapshot -> fork multiple children -> verify restored process state and independent disk writes -> hibernate/restore, with bounded capacity and measured resource growth. Public exposure and broader recovery hardening follow the local experiment.

## Phase 2 — Stateful beta (6–10 additional weeks)

- Harden and extend the pilot's named RAM+disk snapshots and full-state forks; add in-place checkpoints.
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

The current pilot also implements filtered rootless IPv4 egress and remote CLI login/WSS terminals with a public, fixed-file Linux x86-64 curl installer. See [acceptance evidence and remaining networking limits](NETWORKING.md). Production network quotas, immediate session revocation and authenticated owner CLI acceptance remain open.
