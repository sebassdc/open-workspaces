# Deployment proposal

A dedicated authenticated guest-app ingress has been deployed. A Rust/Firecracker prototype
runs on the development machine; see [local setup](LOCAL_PROTOTYPE.md) and
[runtime spike](experiments/runtime-spike.md).

The owner subsequently authorized dedicated authenticated Internet access through
Cloudflare. The local single-app JWT gateway, named tunnel, Access policy and
dedicated DNS route are running. Public anonymous/forged browser requests redirect
to login and invalid origin assertions are denied. Positive owner login and
session revocation checks remain pending. See
[remote access](REMOTE_ACCESS.md). This authorization does not include changing
unrelated Hostinger services or shared ingress.

The same project hostname now serves the [browser dashboard](DASHBOARD.md),
replacing the guest counter page. It connects to the local Unix worker through
an explicit data directory. No additional hostname/domain, cloud service, shared
listener or existing workspace state was changed for this UI deployment.

## Development machine

The owner has a separate machine available. Verify Linux/KVM, architecture, permissions, memory, local disk capacity, filesystem reflink behavior and existing services first. Use dedicated workspace/snapshot directories and test network ranges. Cross-platform development can build control-plane code, but the chosen KVM backend needs a compatible Linux worker.

A 1 vCPU / 1 GiB guest can be an initial experimental profile, not a production guarantee. Browser/build workloads will often need more. Choose concurrency from observed CPU, memory, disk I/O and host headroom rather than virtual disk size or theoretical CoW sharing.

## Existing Hostinger server

A read-only check during research found /dev/kvm absent. Published Hostinger policy disables nested virtualization: https://www.hostinger.com/support/10429687-is-nested-virtualization-supported-in-hostinger/

Options:

1. API/dashboard on existing Dokku, separate KVM worker reached through authenticated private transport.
2. Trusted container workspaces on Hostinger, advertising shared-kernel isolation and reduced state features.
3. All components on the new development machine initially, move the control plane after the alpha is usable.

Option 3 is the simplest development start. Option 1 is the preferred eventual reuse of current hosting for a microVM product.

The owner's local dokku-cloudflare.md is the source of truth for that host. Do not copy its credentials, account identifiers or full live configuration into this public repository. Existing Dokku ingress uses HTTP behind Cloudflare; Tailscale Funnel is reserved for other services. OpenClaw configuration is protected.

Proposed workspace endpoints should use one-level names such as ws-123.example.com. The existing Cloudflare certificate arrangement does not cover deeper workspace/port hostnames. Route a dedicated project gateway through Dokku; avoid creating a Dokku application per guest. Tune project-specific streaming/file limits when deployment is authorized.

Raw SSH/TCP/UDP needs a separately planned listener or authenticated tunnel; ordinary Cloudflare HTTP proxying is insufficient. Never repurpose the existing Tailscale HTTPS listener. The current Flexible SSL origin path is a prototype hosting constraint; design authenticated encrypted worker transport and an enterprise TLS ingress independently, without changing the host's global SSL mode.

## AWS

Start with a standalone EC2 worker, control-plane service, PostgreSQL and S3. Add managed services/HA only when required. AWS supports nested virtualization on selected EC2 families; enable it explicitly, check regional support and verify runtime operation.

Reference: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html

Bare metal remains an alternative for performance-sensitive workloads. Avoid assuming an arbitrary EC2 type, Fargate task or serverless platform permits KVM.

Enterprise packaging should include private worker subnets, least-privilege IAM, controlled guest egress and metadata access, encrypted artifact storage, OIDC, audit export, backup retention, and safe worker drain/replacement. Treat worker local storage as recoverable only through tested durable artifacts.

Spot workers are appropriate only where interruptions and restoration semantics are acceptable. Autoscaling requires artifact placement, task leases, capacity limits and a drain procedure; it is not just a CPU alarm.

## Kubernetes

Deploy API/dashboard/gateway as ordinary workloads. Place workers on dedicated KVM-capable Linux nodes. Grant required device/network access through a documented mechanism; use node labels/taints and dedicated scheduling so general application pods do not share privileged worker nodes accidentally.

Reference: https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/device-plugins/

Start with a worker agent managing multiple guests per node. A Kubernetes operator/custom resource can follow if it improves customer operations; it is not required for the first release. Use external PostgreSQL and S3-compatible durable storage. Document local disk affinity, node drains, artifact compatibility and cold recovery fallback.

Installing Helm on nodes without KVM does not enable the microVM backend. EKS support depends on the actual worker instance types and node configuration.

## Cost model

No current provider quote or cost calculation was performed. Avoid a fixed advertised monthly price until region, machine, disk, traffic and workload are known.

Monthly budget = control-plane compute + worker compute + allocated disks + artifact storage/requests + transfer + database + gateway/load balancer + any Kubernetes management fee.

Hibernating a guest saves RAM/CPU capacity, not an always-on worker bill. Meaningful cloud savings require higher packing density or safe worker scale-down. CoW still consumes storage as guests diverge; full RAM snapshots can be large. AWS managed database, networking, NAT and Kubernetes overhead may exceed the initial application's compute cost.

The first economical release should run on the already available development machine with one worker, modest measured guest sizes, limited retention, and optional remote artifact storage. Add enterprise topology after proving the core lifecycle and recovery.
