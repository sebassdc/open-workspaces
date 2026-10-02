# Agent handoff

## User intent

The owner wants an open-source alternative covering Boxd's publicly documented feature set, preferably in Rust. It should eventually support inexpensive self-hosting, deployment into an enterprise AWS account, and Kubernetes. The owner now has another machine where development can begin and requested this repository to preserve the research and plan for another agent.

This handoff creates documentation only. No runtime was installed, upstream repository compiled, cloud resource provisioned, or existing application changed.

## Current findings

- Boxd combines persistent Linux microVM workspaces, full-state branching, remote human access, and an integration/automation platform.
- Its docs identify the open-source Rust project Ignition as a foundation. The public repository is not established as current Boxd source or full feature parity.
- Ignition is AGPL-3.0 and its README warns against production use; evaluate build reproducibility, security, maintainability, and licensing.
- Firecracker is the default candidate because it supplies the virtualization engine and snapshot primitives. Its orchestration and product layer still need building.
- A read-only check of the existing Hostinger server found no /dev/kvm. Hostinger's published policy disables nested virtualization. This limitation does not establish anything about the owner's separate development machine.
- AWS documents nested virtualization on selected EC2 families. Validate support, region, configuration, and runtime behavior rather than assuming all EC2 instances work.
- Boxd latency claims have not been independently benchmarked. Restoring RAM does not guarantee external network sessions survive or that the application is immediately usable.

## First task: runtime feasibility spike

1. Inspect the development machine: Linux distribution/kernel, CPU architecture and virtualization flags, /dev/kvm availability and permissions, RAM, disk free space, filesystem and reflink support, network capabilities, and existing services.
2. Record environment and tool versions in a new docs/experiments/runtime-spike.md. If host access is unavailable, ask for the machine connection details while preparing the build locally.
3. Pin and build Firecracker; boot a minimal Linux guest on KVM using dedicated test storage and isolated networking. Use host-installed binaries or a clearly documented build environment.
4. Execute a command, transfer a file, and run a small HTTP service. Capture exit status and distinguish shell readiness from service readiness.
5. Demonstrate disk persistence across cold restart, then memory-state restore with a process counter. Explicitly capture and pair the disk state with the memory snapshot.
6. Demonstrate an independent clone: parent and child diverge after capture; identity and entropy are refreshed before exposing the clone; external sessions reconnect as needed.
7. Evaluate Ignition in a separate checkout: pinned revision, build outcome, dependencies, kernel requirements, state semantics, missing self-hosting pieces, and license implications. Do not assume its public tree builds without adjustment.
8. Measure request-to-shell and request-to-HTTP readiness, capture/restore latency, disk growth, and memory usage. Include warm/cache-cold conditions and small repeated sample sets; report p50/p95 and failures.
9. Write an architecture decision record selecting the runtime and storage approach, with evidence and unresolved risks. Update the roadmap.

## Completion criteria for the spike

- A reproducible guest boot procedure on the actual development machine.
- Evidence of execution, file transfer, persistent disk, state restore, and clone write independence, or a precise report of each blocker.
- A documented identity/entropy strategy and network reconnection semantics.
- A runtime recommendation supported by observed results and license constraints.
- No claims of production readiness, security certification, or Boxd performance parity.

## Next implementation slice

After the runtime decision, introduce a Rust workspace with shared types, API, worker, CLI, and guest agent. Implement create -> wait-ready -> exec -> write file -> stop/start -> verify file -> publish HTTP -> backup -> replace worker -> restore. Start with one operator and one worker, but authenticate all externally reachable control operations.

The proposed component names and API in ARCHITECTURE.md are design sketches, not existing code. Do not spend the first iteration scaffolding enterprise services, Kubernetes operators, billing, or a connector catalog.

## Decisions still open

- Development machine access, host capacity, filesystem, and KVM support.
- Firecracker versus an AGPL-compatible Ignition-based implementation.
- Storage clone and snapshot consistency mechanism.
- Initial workspace sizes, quotas, concurrency, and idle policy.
- Authentication provider and whether the first release needs multi-user support.
- Exact AWS region/resources and cost budget.
- Public release scope and enterprise service/support model.

The owner has not supplied these answers. Continue independent development where possible; do not treat an estimate as a requirement.
