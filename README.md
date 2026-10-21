# Open Workspaces

An open-source platform for persistent Linux workspaces for developers and AI agents, designed for self-hosting on one machine or a private pool of hosts.

## Feature matrix

**Core** means available in the supported Linux setup. **Opt-in** requires explicit
operator configuration. **Experimental** has bounded acceptance and platform
limits. **Not yet live** means code is merged, but activation or deployment
acceptance remains outstanding. Versions below are the first tagged release
containing that feature, not a claim that your host has installed or enabled it.
Production hardening remains ongoing. Each dashboard action, its status and the
browser test that guards it are listed in the [UI feature matrix](docs/UI_FEATURES.md).

### Boxes and lifecycle

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Persistent Linux boxes | Create Alpine, Ubuntu or Arch workspaces on a prepared KVM runner; execute commands and transfer files. | Core | v0.1.0 | [Local use](docs/LOCAL_PROTOTYPE.md) |
| Start, stop and delete | Keep disks across cold restarts; explicitly delete a stopped box and its snapshots, named and confirmed first; a create or fork that never made its box fails visibly and deletes cleanly. | Core | v0.1.0; confirm and clean failed forks v0.9.22 | [CLI](docs/AGENT_CLI.md) |
| Rename in place | Change a box's name without forking it; disk, snapshots, shares, previews and running state stay. Owner only. | Core | v0.9.21 | [CLI](docs/AGENT_CLI.md), [daily use](docs/DAILY_USE.md#rename-a-box) |
| CPU, RAM and disk sizing | Select resource sizes, grow sparse root disks, and resize stopped boxes. | Core | v0.1.0; disk sizing v0.7.0 | [Disks](docs/DISKS.md) |

### Access

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Remote CLI | Install compiled macOS/Linux clients; discover images, identity, targets and capacity. | Core | v0.8.0; discovery v0.8.5 | [Install](docs/INSTALL.md), [agent CLI](docs/AGENT_CLI.md) |
| SSH and SFTP | Connect editors and standard SSH tools to explicitly enrolled Linux guests. | Opt-in | v0.1.0 | [SSH](docs/SSH.md) |
| Interactive terminals | Use a guest PTY from `ow shell` or the dashboard, with resize and job control. | Core | v0.1.0 | [Terminals](docs/TERMINALS.md) |
| Dashboard and tailnet login | Manage boxes in the browser using Tailscale identity and an operator-configured access grant; Access JWT remains an alternative. | Opt-in | v0.1.0; tailnet v0.2.0; grants v0.8.3 | [Dashboard](docs/DASHBOARD.md), [UI feature matrix](docs/UI_FEATURES.md) |
| Private previews | Publish a guest HTTP port as a separate authenticated tailnet HTTPS URL. | Opt-in | v0.8.0 | [Previews](docs/DAILY_USE.md#tailnet-https-previews-v080) |

### Snapshots, forks and recovery

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Local snapshots and forks | Capture paired RAM/disk state, restore it, and fork independent Linux disks on the same runner; the runner keeps answering while a capture is hashed. | Core | v0.1.0; hashing off the worker thread v0.9.22 | [Snapshots](docs/LOCAL_PROTOTYPE.md) |
| Hibernate and wake | Release guest CPU/RAM while preserving state; resume on connection, with optional idle hibernation. | Core | v0.2.0 | [Daily use](docs/DAILY_USE.md) |
| Off-host backups | Copy catalogs and sealed snapshots to S3-compatible storage; verify captures before restoring. | Opt-in | v0.2.0 | [Backups](docs/CORE.md), [snapshot store](docs/DAILY_USE.md) |
| Recovery points | Upload periodic disk-only points of running boxes (a brief pause while the disk is reflinked, no RAM write; restore is a cold boot) and hibernation state; report last durable capture and failures. Runners advertise `recovery_disk_only_v1`; controller health writes have independent per-runner WAL locks, and failed sessions reconnect by themselves. Real-runner pause duration remains unmeasured. | Opt-in | v0.9.2; parallel SDK transfers v0.9.35; disk-only points v0.9.40 | [Recovery](docs/RECOVERY.md), [staying up](docs/MULTINODE.md#staying-up-while-the-disk-is-slow) |
| Managed restore | Adopt an existing catalog-authorized capture through durable transfer and worker fencing. | Not yet live | v0.9.5 | [Transfer lifecycle](docs/LIFECYCLE.md) |
| Park, wake and move across runners | Store a box without placement, wake on an eligible runner, or explicitly move/fork to a compatible target. | Not yet live | v0.9.11; move/fork v0.9.12; parallel SDK transfers v0.9.35 | [Transfer lifecycle](docs/LIFECYCLE.md), [artifact store](docs/ARTIFACT_STORE.md) |
| Lifecycle of moved boxes | Delete, resize, snapshot and change SSH keys of a moved, forked or woken box under its placement epoch; its named snapshots go to the artifact store and fork on any runner. | Opt-in | v0.9.39 | [Transfer lifecycle](docs/LIFECYCLE.md#lifecycle-of-a-moved-box) |

### Storage and images

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Developer images | Use prepared Ubuntu/Arch with Git, curl, Neovim, native build tools and language toolchains. | Core | v0.1.0 | [Guest images](docs/GUEST_IMAGES.md) |
| Golden images | Build a reusable local image from a trusted recipe. | Opt-in | v0.2.0 | [Image recipes](docs/DAILY_USE.md) |
| Write-once shared images | Publish versioned images to S3 and pull digest-verified copies onto runners. | Opt-in | v0.9.0 | [Artifact store](docs/ARTIFACT_STORE.md) |
| Cache-aware placement | Advertise verified image copies, reserve staging space and evict eligible sealed cache copies. | Opt-in | v0.9.7 | [Cache](docs/CACHE.md) |
| Artifact garbage collection | Inspect dry runs and, only after activation checks, collect unreferenced format-1 objects with undo evidence. | Not yet live | v0.9.10 | [Collection gates](docs/STORE_GC.md) |

### Hosts, runners and placement

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Self-hosted Linux and invited runners | Run one host, add an SSH/SSM/tailnet runner, or enroll a trusted outbound host. | Core | v0.1.0; transports v0.2.0 | [Single host](docs/SINGLE_HOST.md), [invitations](docs/HOSTS.md) |
| AWS host and runner components | Provision dedicated nested-virtualization EC2 capacity with SSM and optional Tailscale. | Opt-in | v0.1.0; runner v0.6.0 | [AWS](deploy/aws/README.md) |
| Controller without local compute | Run the controller/dashboard while placing boxes only on enrolled remote runners. | Opt-in | v0.9.8 | [Controller-only mode](docs/SINGLE_HOST.md#controller-without-local-compute) |
| Placement authority | Persist transfer intent and enforce exact placement epochs at capable workers; installation alone moves nothing. | Not yet live | fences v0.8.6; catalog v0.9.3 | [Fencing](docs/PLACEMENT_FENCING.md), [catalog](docs/LIFECYCLE.md) |
| Explicit runner retirement | Drain eligible runners, park sleeping boxes and produce an empty certificate; provider deletion is separate. | Not yet live | v0.9.13 | [Drain](docs/DRAIN.md) |
| Apple Silicon host | Run bounded Ubuntu ARM64 via a native helper; Linux-only capabilities and general host distribution remain limited. | Experimental | v0.1.0 | [Mac host](docs/MAC_HOST.md) |

### Limits, security and identity

| Feature | What it does | Status | Shipped in | Guide |
| --- | --- | --- | --- | --- |
| Owner quotas and pool admission | Reserve CPU, RAM, logical disk, box count and configured pool cost estimates before admission. | Core | v0.8.1 | [Limits](docs/LIMITS.md) |
| vCPU overcommit | Allow a Linux host operator to select 1× or 2× vCPU admission; RAM is not overcommitted. | Opt-in | v0.8.5 | [Overcommit](docs/HOSTS.md#linux-vcpu-overcommit) |
| Host reserve | Keep CPU and RAM per host that boxes never claim; on AWS hosts, systemd protects the controller/agent and kills a VM first under memory pressure. | Core | v0.9.24 | [Host reserve](docs/HOSTS.md#host-reserve-capacity-boxes-never-claim) |
| User ownership and sharing | Scope boxes, snapshots and access to authenticated owners with explicit use/admin grants. | Core | v0.2.0 | [Users](docs/USERS.md) |
| Pool admins and box transfer | Transfer a box to another owner in place, or create and fork boxes for others; configured pool admins manage any box; every change is audited for both owners. | Core | v0.9.36 | [Users](docs/USERS.md#pool-admins-and-transfers), [daily use](docs/DAILY_USE.md#give-a-box-to-someone-else) |
| Guest isolation and egress | Run Linux guests under Firecracker/KVM with private host control and filtered guest networking. | Core | v0.1.0 | [Networking](docs/NETWORKING.md) |
| Per-box AWS access | Attach one operator-allowed IAM role to a box (`ow aws-role set`, dashboard); the controller issues short-lived OIDC identity tokens bound to the box and its generation, and the guest's AWS SDK chain assumes the role. No AWS keys in the box. | Opt-in, not yet verified against real AWS | v0.9.37 | [Per-box AWS access](docs/AWS_ACCESS.md) |
| CredentialBroker | Library-only per-box authorization for temporary AWS roles and secret writes; no live provider adapter or guest endpoint. | Not yet live | v0.8.0 (library only) | [Broker boundaries](docs/CREDENTIAL_BROKER.md) |

Snapshots depend on runtime/CPU/kernel compatibility. External connections may
need to reconnect after restore or hibernation. Host operators are trusted with
the workloads on their hardware. No automatic failover, provider scale-in,
production certification, Kubernetes operator or public SDK is claimed.

## Get started

Install a [versioned remote client](docs/INSTALL.md) to connect to an existing
host, or follow [single-host setup](docs/SINGLE_HOST.md) for your own Linux/KVM
machine. Tailnet clients need no Rust, root or cloudflared.

```sh
ow login --tailnet https://NAME.TAILNET.ts.net
ow create my-box
ow exec my-box -- 'uname -r; echo hello'
ow shell my-box
ow snapshot my-box prepared
ow fork my-box branch --snapshot prepared
ow hibernate branch
```

For local development, use `./ow` and the [local setup guide](docs/LOCAL_PROTOTYPE.md).
Deployment-specific endpoints, account configuration and credentials stay private.

## Design and development

- [Architecture](docs/ARCHITECTURE.md) and [runtime/storage decision](docs/adr/0001-local-runtime-and-storage.md).
- [Roadmap](docs/ROADMAP.md), [handoff](docs/HANDOFF.md) and [engineering instructions](AGENTS.md).
- [Runtime evidence](docs/experiments/runtime-spike.md) and [deployment options](docs/DEPLOYMENT.md).
- [In-place upgrades and release impact](docs/UPGRADES.md).
- Configuration rule: the controller is the single source of configuration and
  state; runners get theirs from it and infrastructure code carries no feature
  switches. See [contributing](CONTRIBUTING.md#configuration-the-controller-is-the-single-source).

This is an independent implementation inspired by public product behavior.
There is no affiliation with Boxd or claim of API compatibility or performance
parity. The name is provisional.

## Licensing

Original material in this repository is licensed under [MIT](LICENSE).
Vendored [xterm.js](crates/ow/ui/vendor/XTERM-LICENSE) and
[addon-fit](crates/ow/ui/vendor/FIT-LICENSE) retain their MIT licenses and
copyright notices. Dependencies, downloaded runtimes and guest distribution
packages keep their own licenses; the project license does not relicense them.
Ignition is AGPL-3.0: evaluating it does not authorize incorporating its code
into this MIT-licensed implementation. Record a licensing decision before reusing
upstream code.
