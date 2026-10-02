# Open Workspaces

An open-source platform for persistent Linux workspaces for developers and AI agents, designed for self-hosting on a single machine and eventual deployment to AWS or Kubernetes.

**Status: research and implementation planning. No runnable platform exists yet.**

The owner has another machine available for development. Its hardware, OS, KVM support, storage, and capacity have not been inspected. Start there with a runtime feasibility spike.

## Read first

1. [Agent handoff](docs/HANDOFF.md): context, first task, and acceptance criteria.
2. [Research and feature inventory](docs/RESEARCH.md): Boxd capabilities, user stories, sources, and limitations.
3. [Architecture](docs/ARCHITECTURE.md): proposed Rust components and runtime decision.
4. [Roadmap](docs/ROADMAP.md): phased backlog and delivery gates.
5. [Deployment](docs/DEPLOYMENT.md): development machine, Hostinger, AWS, and Kubernetes.
6. [Agent instructions](AGENTS.md): engineering and host boundaries.

## Proposed direction

- Rust API, scheduler, worker, guest agent, and CLI.
- Firecracker as the provisional microVM engine; evaluate Ignition before confirming.
- PostgreSQL for control-plane state; local copy-on-write storage for active disks; S3-compatible storage for durable artifacts.
- TypeScript dashboard and SDK, followed by a Python SDK.
- Persistent workspaces and reliable recovery first; live forks, hibernation, teams, desktop, and integrations follow.
- A container backend may support trusted workloads on hosts without KVM, with its reduced capabilities explicitly advertised.

This is an independent implementation inspired by public product behavior. There is no affiliation with Boxd and no claim of API compatibility or performance parity. The name is provisional.

## Licensing

Original material in this repository is licensed under Apache-2.0. Upstream components keep their own licenses. Ignition is AGPL-3.0: evaluating it does not authorize incorporating its code into an Apache-only implementation. Record a licensing decision before reusing upstream code.
