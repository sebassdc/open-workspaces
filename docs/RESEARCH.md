# Boxd research and feature inventory

Research date: 2026-10-01. Based on public documentation and repository inspection through web reads; the hosted product has not been tested. Subsequent local upstream build results are recorded in the [runtime spike](experiments/runtime-spike.md). Documentation can change. The docs index endpoint failed during research; the combined docs and individual pages were accessible. This inventory groups the documented product capabilities rather than claiming an exhaustive list of every API flag.

## Sources

- [Boxd overview](https://docs.boxd.sh/)
- [Combined documentation](https://docs.boxd.sh/llms-full.txt)
- [CLI commands](https://docs.boxd.sh/cli/commands)
- [TypeScript SDK](https://docs.boxd.sh/reference/typescript-sdk)
- [Python SDK](https://docs.boxd.sh/reference/python-sdk)
- [Forks](https://docs.boxd.sh/guides/fork)
- [Snapshots](https://docs.boxd.sh/guides/snapshots)
- [Lifecycle](https://docs.boxd.sh/guides/suspend-resume)
- [Disaster recovery](https://docs.boxd.sh/guides/disaster-recovery)
- [Resource limits](https://docs.boxd.sh/guides/resources)
- [Sharing](https://docs.boxd.sh/guides/share-a-vm)
- [Egress control](https://docs.boxd.sh/guides/egress)
- [Desktop](https://docs.boxd.sh/guides/desktop)
- [Browser automation](https://docs.boxd.sh/guides/automations/browser)
- [Local client utilities](https://docs.boxd.sh/guides/client-utilities)
- [Connections](https://docs.boxd.sh/guides/integrations/connections)
- [Scripts](https://docs.boxd.sh/guides/automations/scripts)
- [Jobs](https://docs.boxd.sh/guides/automations/jobs)
- [Ignition](https://github.com/lttle-cloud/ignition)
- [Ignition Rust manifest](https://github.com/lttle-cloud/ignition/blob/master/Cargo.toml)
- [Firecracker](https://github.com/firecracker-microvm/firecracker)
- [Firecracker snapshot behavior](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)
- [Hostinger virtualization policy](https://www.hostinger.com/support/10429687-is-nested-virtualization-supported-in-hostinger/)
- [AWS nested virtualization](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html)
- [Kubernetes device plugins](https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/device-plugins/)

## Feature map and proposed user stories

Phase labels refer to ROADMAP.md. These are proposed priorities, not implemented capabilities.

| ID | Boxd capability | User story for our project | Target |
| --- | --- | --- | --- |
| WS-01 | Full Linux workspace, root/toolchain, Docker inside guest | Developer installs arbitrary tools and retains the environment | Alpha; Docker validated separately |
| WS-02 | Machine creation, inspection, rename, start/stop/reboot/delete | Operator manages the complete workspace lifecycle | Alpha |
| WS-03 | Dedicated microVM isolation | Platform owner runs agent code behind a hardware VM boundary | Spike/alpha |
| WS-04 | Remote exec, streamed output, PTY, shell, SSH/editor access | Developer works from terminal or editor and receives actual exit status | Alpha; editor polish beta |
| WS-05 | File upload/download and directory operations | Agent exchanges source and output with callers | Alpha |
| WS-06 | Persistent copy-on-write root disks | User restarts a workspace without losing files; clones diverge | Alpha/beta |
| WS-07 | Additional disks with per-attachment mount modes | User keeps a dataset separate from disposable workspaces | Beta |
| WS-08 | Live fork including RAM and running processes | Agent branches several attempts from one initialized state | Initial local pilot; hardening beta |
| WS-09 | Named/versioned RAM+disk snapshots | Team reuses a prepared environment without reinstallation | Initial local pilot; hardening beta |
| WS-10 | In-place RAM+disk checkpoints | Developer rewinds an experiment while keeping workspace endpoints | Stateful beta |
| WS-11 | Pause/resume, hibernate/wake, configurable automatic idle transitions | User returns to a sleeping workspace with process state retained | Stateful beta |
| WS-12 | Golden environment refreshed from code changes | Team starts new workspaces from a current known-good base | Stateful beta |
| WS-13 | Off-worker disk backup, schedules, retention, restore | Operator recovers data after corruption or worker loss | Alpha; schedules beta |
| WS-14 | Default HTTPS, detected/pinned ports, extra proxies | Developer publishes a running service at a stable URL | Alpha |
| WS-15 | Custom domains and organization wildcard domains | Organization publishes workspaces under its own domain | Team release |
| WS-16 | Raw TCP/UDP public port forwarding | User exposes non-HTTP applications with explicit policy | Beta |
| WS-17 | Private network labels, DNS, Tailscale connectivity | Team connects related services and restricts other peers | Beta/team release |
| WS-18 | Isolated workspace mode that removes account bridges | Operator creates a sandbox with no implicit account access | Alpha |
| WS-19 | Host/IP egress allowlists enforced outside guest | Administrator restricts agent destinations even with guest root | Basic alpha; domain policy beta |
| WS-20 | Scoped variables and encrypted write-only secrets | User supplies configuration without secret readback in management API | Alpha/team release |
| WS-21 | Host-bound secrets inserted outside guest | Agent calls an approved API without possessing the actual token | Extended release |
| WS-22 | Organizations, teams, invitations, roles, scoped API keys | Administrator manages people and service access | Team release |
| WS-23 | Private/shared workspaces and personal credential handoff | Owner shares a workspace with controlled access and credentials | Team release |
| WS-24 | CLI/JSON/REPL, SDKs, gRPC, agent skill support | Developer and agent automate the same lifecycle as the UI | CLI/API alpha; SDKs beta; compatibility later |
| WS-25 | Quotas, usage, billing | Owner limits resource usage and tracks consumption | Quotas alpha; metering enterprise; billing later |
| WS-26 | Live desktop, human control, launcher, clipboard/upload | Human observes an agent and intervenes in its environment | Extended release |
| WS-27 | Playwright and agent browser; visible/headless modes; persistent profile; stealth options | Agent uses a browser while a human can watch and help | Extended release; stealth optional |
| WS-28 | Opt-in local clipboard/files/browser bridge, device selection | User grants a private workspace selected local capabilities | Extended release |
| WS-29 | Broad connector catalog; personal/shared accounts; credential brokerage | User connects a service once and uses it across environments | Extended release, begin with GitHub |
| WS-30 | Typed TypeScript automations, triggers, schedules, durable state | Developer writes an event-driven automation as code | Extended release |
| WS-31 | Job lifecycle, logs, retries, blocked connection state, wake delivery | Operator monitors and recovers automation execution | Extended release |
| WS-32 | Bot checks before waking on HTTPS | Owner prevents unwanted traffic repeatedly waking machines | Beta; API/webhook behavior explicit |

## Important semantic distinctions

- A fork branches the current machine into a new identity. A snapshot is a named reusable capture. A checkpoint rewinds the existing identity. A backup is a durable disk recovery artifact. Implement these as distinct operations.
- Boxd's documented snapshots and checkpoints contain RAM and disk; backups contain disk only. A disk-only clone cannot resume processes.
- Additional disks are documented as single-attachment rather than a shared concurrent filesystem. Define whether our snapshots include them; do not imply coverage before it exists.
- Sharing shell access and publishing an application are separate permissions. Removing known credential files cannot retroactively remove copies from old snapshots or process memory.
- Boxd's documented idle detection uses inbound network traffic, which can classify CPU-only work as idle. Our design should include leases or explicit keep-running policies.
- A wake challenge can break API clients/webhooks. Define route-specific wake authentication and bounded buffering.
- Connector calls brokered by the platform and transparent host-bound secret injection are different mechanisms. The latter requires deliberate TLS/protocol handling and is deferred.

## Performance and capacity

Boxd advertises sub-10ms startup, sub-200ms forks, and very fast warm resume. These claims were not independently verified; they are not acceptance thresholds for this repository.

Its resource page lists default machines at 2 vCPU / 8 GiB RAM / 100 GB disk. Those defaults are not appropriate requirements for a small initial host. Choose sizes from actual workload measurements and reserve capacity for the host and platform.

Copy-on-write delays allocation; it does not remove storage growth. Memory capture, dirty pages, browser workloads, snapshot retention, and backups can dominate capacity. A sleeping guest still has storage costs and an always-on worker still has a hosting bill.

## Reuse assessment

Ignition is a promising Rust foundation, not confirmed current Boxd source. Its public README warns against production use and lacks a complete self-hosting guide. Its AGPL license requires a deliberate choice before incorporation.

Firecracker supplies a KVM microVM engine and snapshot primitives, but not the finished platform. Its snapshot documentation identifies clone entropy/identity risks, trusted snapshot inputs, and connection loss across restores. We must implement artifact authentication, consistency, identity refresh, policy enforcement, scheduling, and recovery.

No technical or commercial claim in this research establishes a legal opinion, production security assessment, or upstream build result.
