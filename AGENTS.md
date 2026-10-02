# Instructions for implementation agents

Read README.md and docs/HANDOFF.md before changing code. The repository is currently a documentation handoff, not an implemented service.

## Scope and engineering

- Start with the runtime feasibility spike in docs/ROADMAP.md. Build a vertical slice before a broad framework.
- Rust is preferred for systems components. Reuse a microVM engine; do not write a new hypervisor.
- Treat Firecracker as provisional until evidence and an architecture decision record confirm it. Evaluate Ignition without silently importing AGPL code.
- Distinguish observed behavior, upstream documentation, proposed behavior, and unverified claims.
- Maintain docs and acceptance criteria when scope changes. Never label a disk clone as a live fork or a container as hardware-isolated.
- Do not invent benchmark results. Record versions, host configuration, method, sample count, and p50/p95 latency.
- Use focused tests for isolation, persistence, operation idempotency, recovery, and snapshot correctness. A passing API call alone does not establish a working VM.
- Keep secrets and infrastructure identifiers out of commits. Never publish credentials, user filesystem contents, snapshots, or live host configuration.
- Guest root must not be able to weaken host policy. Keep worker credentials and hypervisor sockets inaccessible to guests.
- Do not expose host Docker sockets to the public API or guest workloads.
- Workload identity must grant only intended workspace capabilities, not implicit account-wide control.

## Deployment boundaries

The user authorized creating this research repository. Deploying infrastructure or changing existing services is outside this initial handoff task. Future instructions may authorize those actions.

On the owner's existing Hostinger host, obey parent AGENTS.md and read the local dokku-cloudflare.md before changing apps or domains. OpenClaw configuration is protected. Tailscale Funnel is reserved for existing OpenClaw/webhook ingress; never disable its shared HTTPS listener. Existing Cloudflare Flexible SSL must not be changed globally as part of this project.

On a separate development machine, inspect host capabilities and use dedicated data directories and networking. Avoid assuming this machine has the same ingress configuration as Hostinger.
