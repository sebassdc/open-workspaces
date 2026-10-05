# Open Workspaces

An open-source platform for persistent Linux workspaces for developers and AI agents, designed for self-hosting on a single machine and eventual deployment to AWS or Kubernetes.

**Status: runnable local Rust/Firecracker prototype. Production hardening is pending.**

KVM now works on the development machine. Real-VM tests pass for exec, files,
HTTP access, persistence, paired RAM/disk snapshots, independent forks,
hibernation, corruption rejection and worker restart recovery.

```bash
./ow up
./ow create my-box
./ow exec my-box -- 'uname -r; echo hello'
./ow snapshot my-box prepared
./ow fork my-box branch --snapshot prepared
./ow hibernate branch
./ow stats
```

See the [local prototype guide](docs/LOCAL_PROTOTYPE.md),
[runtime measurements](docs/experiments/runtime-spike.md), and
[runtime/storage decision](docs/adr/0001-local-runtime-and-storage.md).

The browser dashboard is running behind Cloudflare Access on the existing
project hostname: create machines, run commands, capture snapshots, fork,
hibernate/resume and view memory use. See the [dashboard guide](docs/DASHBOARD.md)
and [Internet access setup](docs/REMOTE_ACCESS.md). **Open Terminal** now provides
a real guest PTY over authenticated WebSocket; `./ow shell` uses the same guest
backend locally. See [terminal details](docs/TERMINALS.md). Owner browser
verification of the updated UI remains pending.

Headless Arch and Ubuntu Base guest profiles are also available from the New
machine dialog or `./ow create NAME --image arch` or `--image ubuntu`. See [image sources,
Omarchy findings and compatibility tests](docs/GUEST_IMAGES.md).

A bounded native Apple Silicon node now runs persistent Ubuntu ARM64 with
exec, files and PTY over the existing authenticated node transport. Local VM
and TLS acceptance passed; live gateway/dashboard pool acceptance awaits
planner review and a dedicated invitation. macOS guests remain pending.
See the [Mac operator guide](docs/MAC_HOST.md) and [integration evidence](docs/plans/mac-node-report.md).

## Read first

1. [Agent handoff](docs/HANDOFF.md): context, first task, and acceptance criteria.
2. [Research and feature inventory](docs/RESEARCH.md): Boxd capabilities, user stories, sources, and limitations.
3. [Architecture](docs/ARCHITECTURE.md): proposed Rust components and runtime decision.
4. [Roadmap](docs/ROADMAP.md): phased backlog and delivery gates.
5. [Deployment](docs/DEPLOYMENT.md): development machine, Hostinger, AWS, and Kubernetes.
6. [Agent instructions](AGENTS.md): engineering and host boundaries.

## Proposed direction

- Rust API, scheduler, worker, guest agent, and CLI.
- Firecracker v1.17.0 for the local prototype under ADR 0001; broader deployment suitability remains under evaluation.
- SQLite for the current single-host ownership/catalog; PostgreSQL proposed for a broader control plane; local copy-on-write storage for active disks; S3-compatible storage for durable artifacts.
- TypeScript dashboard and SDK, followed by a Python SDK.
- First local pilot: persistent remote workspaces, full-state snapshots/forks, hibernation and resource measurements. Broader recovery hardening, teams, desktop and integrations follow.
- A container backend may support trusted workloads on hosts without KVM, with its reduced capabilities explicitly advertised.

This is an independent implementation inspired by public product behavior. There is no affiliation with Boxd and no claim of API compatibility or performance parity. The name is provisional.

## Licensing

Original material in this repository is licensed under Apache-2.0. Upstream components keep their own licenses. Ignition is AGPL-3.0: evaluating it does not authorize incorporating its code into an Apache-only implementation. Record a licensing decision before reusing upstream code.

Guest Internet access and the remote CLI curl installer are implemented. See [networking and remote connection](docs/NETWORKING.md).

New Ubuntu and Arch machines include Git, curl, Neovim, GCC and mise-managed
Node/Python/Rust, with guest-only passwordless sudo for the `dev` account. See
[developer image details](docs/GUEST_IMAGES.md#developer-tools-and-permissions).

Dashboard and remote CLI machines now belong to the authenticated user. Names,
snapshots, terminal access and usage are scoped through a durable SQLite catalog.
See [ownership, migration and database limits](docs/USERS.md).

Guest OpenSSH source now provides `ow ssh`, standard ProxyCommand/config and
SFTP for explicitly enrolled Ubuntu/Arch guests, locally and through outbound
Linux nodes. See [SSH setup, identity and verification](docs/SSH.md).
Image publication and shared-service rollout remain planner-owned.
