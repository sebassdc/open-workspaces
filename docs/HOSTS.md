# Invited hosts

For the native Apple Silicon implementation, use the [Mac operator guide](MAC_HOST.md).
The Linux participant procedure below and existing Darwin CLI downloads do not
install the signed native helper or verified ARM64 guest assets. Mac local/TLS
acceptance is recorded; live private-pool acceptance and distribution remain gated.

## Linux hosts

This implementation admits trusted operator hosts to an owner-managed shared
pool. Invited friends do not need a human Access login. Admitted humans can use
pool workspaces; host operators can inspect or stop workloads on their hardware.
Participation gives no human workspace-management authority. Physical-host/NAT
acceptance remains pending until the owner runs the commands on the second host.

The controller operator configures [owner quotas and pool cost admission](LIMITS.md)
before minting invitations; participants cannot override those limits.

## Participant

Install the project CLI with the existing domain's `/cli/install.sh`, then run:

```sh
ow host doctor
ow host join
ow host status
```

Join asks for a hidden invitation paste, consent, a disjoint guest RAM budget,
running slots, guest vCPU budget and minimum free disk GiB. The invitation carries
a validated HTTPS controller origin. Default acceptance is 512 MiB / 2 slots /
2 CPU; an initial guest can use 256 MiB / 1 CPU. Default downloaded hosts advertise Alpine only. The capacity release candidate
adds opt-in clean Ubuntu dev with `host join --ubuntu-dev` or stopped-host
`host update-assets --ubuntu-dev`; Arch remains limited to previously prepared
unmanaged nodes. The server clamps claims to durable owner RAM/slot/CPU caps. Other workers and desktop reserve must
be accounted for by the operator. `--storage-gib` is a free-space admission
threshold, not a disk quota or reserved allocation.

Linux x86-64 requires accessible `/dev/kvm`, Btrfs/reflink storage, unprivileged
user/network namespaces and system `ip`, `nft`, `unshare`, `cp`, `curl`,
`sha256sum`. Doctor performs a cleaned small reflink probe and reports failures;
it does not install tools or change privileged configuration. Omarchy 4.0.4
owner preflight reports readable/writable KVM, Btrfs HOME, 41 GiB available RAM
and 818 GiB free disk; namespace/tool/reflink checks and real outbound HTTPS
remain to be run there. Tailscale is not required.

`ow host start` restarts saved participation. Success is either a fresh
controller acknowledgement with matching live worker/agent identity, or explicit
PENDING; enrollment and a held PID lock alone do not mean dispatchable.
`ow host stop` shuts down this managed worker and its guests, retains disks and
requires positive owned-process exit evidence. UNKNOWN status refuses unsafe
fallback signals. The default private host root is `$HOME/.ow-host`; advanced
`--data-dir` selects another short, owned private root. Never point these commands
at existing legacy worker or workspace roots.

Advanced unattended join accepts `--invite-file PRIVATE_FILE`, capacity flags,
`--accept-shared-pool` and `--no-start`. A valid invitation file has a private
parent and mode 0600. The default wizard requires no manual file/chmod work.
For a self-hosted private CA only, `ow host join --ca-cert PRIVATE_CA` validates
and copies the CA privately into the host root. Asset/enrollment/agent TLS and
later restarts use that saved CA. Public system trust is the default; no TLS
bypass or inherited arbitrary trust environment is used.

If a download fails, repeat join with the still-valid invitation. Owned exact
staging remnants are recovered under the lifecycle lock. If credentials were
saved but local completion failed, repeat join/start: saved enrollment completes
without redemption. If the enrollment response or credential publication was
lost after server consumption, ask the owner to revoke that node and invite a
fresh ID. Preserve the old root for diagnosis and select a new dedicated short
root with `--data-dir`; do not delete arbitrary directories or overwrite node
credentials. Revoked IDs cannot be reissued.

## Owner

Configure gateway `node_owner_subject` and `node_owner_user_id` together, binding
an existing catalog user to the verified configured Access-team issuer/subject.
Missing configuration fails closed; email allowlisting alone is insufficient.
The owner dashboard provides invitation, per-node budget and revoke controls. CLI equivalents:

```sh
ow --server https://PROJECT_DOMAIN host invite friend-one --memory 512 --slots 2 --cpus 2 --output PRIVATE_FILE
ow --server https://PROJECT_DOMAIN host budget friend-one --memory 8192 --slots 2 --cpus 4
ow --server https://PROJECT_DOMAIN host revoke friend-one
```

Invitation output contains a secret: share privately and avoid logs/shell history.
Use the wizard's hidden paste on the invited host. Replacing an unused invite
invalidates older secrets; editing an enrolled budget preserves its ID/credentials; revoked IDs require fresh IDs.

## Adjusting saved participation

Ceilings: worker 65,536 MiB / 64 vCPU / 8 running slots. Workspace shapes:
256/512/1024/2048/4096/8192/16384 MiB and 1–16 vCPU. Existing conservative
defaults remain. Owner caps and local operator consent both bound admission;
raising one never raises the other. Doctor reports logical CPU availability and
optional `lscpu` topology; it does not select or promise a physical capacity budget.

For an increase, owner saves the enrolled budget first. For a decrease, the
operator first stops that host's guests while keeping participation online, and
the owner lowers only with fresh idle inventory and zero registered/unregistered
reservations or pending/uncertain effects. Offline lowering deliberately fails.
Then the operator explicitly stops participation and updates saved consent:

```sh
ow --data-dir HOST_ROOT host stop
# Install the planner-published CLI with the existing /cli/install.sh.
# Installer replaces the CLI/helper only; enrollment/config/assets/disks remain.
ow --data-dir HOST_ROOT host update-assets --ubuntu-dev
ow --data-dir HOST_ROOT host configure --memory 8192 --slots 2 --cpus 4 --accept-shared-pool
ow --data-dir HOST_ROOT host start
ow --data-dir HOST_ROOT host status
```

Numbers above are examples requiring an explicit disjoint host budget. Configure
and asset updates require positively stopped owned worker/agent, no sockets or
guest processes and a resolved stopped journal; they never signal processes.
Do not point them at legacy owner roots. A failed update before activation
recovers old assets; after verified activation it may roll forward. A corrupt new
selection is retained and the verified old selection restored. Config, credentials,
trust and guest disks are preserved; exact prior consent and asset revisions are
retained privately. Supported asset rollback, while stopped:

```sh
ow --data-dir HOST_ROOT host rollback-assets --revision assets-rollback-NONCE
```

V1 Alpine and opt-out v2 use five files; complete Ubuntu v2 requires seven files,
including the pinned guest network-tools archive. It is not extracted on the host.
Only verified manifest-selected images are advertised. Existing deployed clients
and `/cli/host-manifest.json` retain raw delivery: 8,589,934,592 Ubuntu bytes plus
helper/common files (8,778,379,912 total for the accepted complete bundle).

The prepared compressed-delivery candidate requests
`/cli/host-compressed-manifest.json`, falling back to the old raw endpoint only
on HTTP 404. The gateway returns 404 when compressed capability is genuinely
absent; malformed/unsafe metadata remains an error. The separate transport-v1
wrapper embeds the original canonical v2 manifest and fixed `ubuntu.ext4.zst`
encoded length/SHA-256. Only Ubuntu is compressed. The reviewed bundle transfers
631,333,954 total bytes, including 442,888,634 encoded Ubuntu bytes. A built-in
pinned zstd decoder verifies encoded bytes before sparse decoding, bounds its
window to 64 MiB and output to the expanded manifest length, verifies the original
SHA-256, and refuses dictionaries, skippable/concatenated frames or trailing bytes.
The locally published canonical manifest contains expanded names, hashes and
lengths; enrollment and existing disks are preserved. Admission reserves expanded
logical bytes plus encoded staging, saved disk reserve and 512 MiB overhead.
Transfer progress counts encoded bytes and is followed by explicit decode/verify
and publication phases. Each TLS file download has a 1,800-second deadline.
Interrupted transfers restart. Parent review/publication remains pending; see
[measurements, compatibility and release pins](plans/compressed-assets-report.md).

Host asset downloads show aggregate progress, the current filename and average
transfer rate on stderr. Terminals get a single-line bar refreshed at most five
times per second during streaming, with received/total MiB or GiB; redirected
stderr gets concise start, per-file and phase lines without terminal escapes.
Download completion is followed by verification and publication: 100% transferred
does not mean the update succeeded. Existing stdout messages are unchanged.

Cold resize remains explicit stop → `ow resize NAME --memory 4096 --cpus 2` →
start. New guests can select `ow create NAME --image ubuntu --memory 4096 --cpus 2
--node NODE`; their fixed placement and ownership remain. This release candidate
has local 4 GiB/2 CPU Ubuntu persistence evidence, not physical remote upgrade or
deployment acceptance; see [capacity report](plans/host-capacity-report.md).

## Release preparation and scoped rollout

The bounded trusted-pool release is merged and deployed on the existing project
domain. Independent focused V1–V6 acceptance and actual public default-TLS node
enrollment/channel/revocation checks passed; physical second-host/NAT acceptance
remains pending. See [verification](plans/host-verification-results.md) and
[planner handoff](plans/host-onboarding-handoff.md). The preparation scripts below
produce local outputs only; scoped deployment is an operator action.

Use a dedicated owned 0700 ignored build root with `.ow-host-build` containing
`open-workspaces dedicated host build v1` plus newline. Select separate target,
fresh release output and fresh public bundle output inside it. The build script
requires `CARGO_HOME`, `RUSTUP_HOME`, `OW_HOST_BUILD_ROOT`, `CARGO_TARGET_DIR`,
`OW_CROSS_BUILD_ROOT` and `OW_HOST_BUILD_OUTPUT`, checks pinned Zig, records every
compiled source input and refuses changes during build:

```sh
bash scripts/build-host-cli.sh
python3 scripts/prepare-host-bundle.py --help
python3 scripts/prepare-compressed-host-bundle.py --help
```

Prepare pinned runtime bytes only under dedicated source storage: official
Firecracker 1.17.0 executable from its pinned release archive, pinned kernel
6.1.186 and slirp4netns, and a freshly prepared minimal Alpine 3.24.2 base using
`experiments/runtime-spike/prepare-guest.py --data-dir DEDICATED_SOURCE
--toolchain-root DEDICATED_TOOLS`. Never use preparers' legacy shared defaults
for this release. Preserve pinned downloads, the clean-image recipe/digest and
matching release `ow-guest`. Bundle publication requires the build manifest,
matching CLI/guest, fixed pins, clean-image digest and copied CLI help/hash; it
never overwrites an existing bundle. No owner disk/snapshot enters publication.

Planner captures complete paginated Access apps/policies and dedicated tunnel
config privately, normalizing to schema 1 as used by
`experiments/host-onboarding/source-checks.py`. Required state includes recorded
account/zone/tunnel/hostname, dashboard/CLI IDs, audience/team, exact app identities
and policy arrays, ownership, native trust and inner tunnel ID/ingress. Set
`app_inventory_complete:true` only after all pages were captured. Retain original
API capture separately. Policy normalization ignores only `created_at`,
`updated_at`, `app_id`, `reusable`; unknown policy fields require review. An
independent approved record contains those ownership IDs, dashboard/CLI app
identities and policies, and native CA/server certificate paths, SHA256 hashes
and server name. Do not manufacture approval from the current capture.

```sh
python3 scripts/prepare-host-ingress.py --state PRIVATE_CAPTURE --approved PRIVATE_APPROVED --output PRIVATE_PLAN
python3 scripts/prepare-host-ingress.py --state FRESH_PRIVATE_CAPTURE --approved PRIVATE_APPROVED --check-plan PRIVATE_PLAN
```

Before any eventual write, planner verifies the fresh-state precondition,
backs up exact binaries/config/catalog and records the created node-app ID.
Apply only the plan's dedicated node app intent and ordered node/expanded CLI
routes. Existing native TLS `127.0.0.1:8790` remains the sole controller with one
root/session map; a/b endpoints, CA and credentials remain unchanged. Node origin
TLS uses the verified private CA/server name, explicit `noTLSVerify:false`.
Human fallback remains Access-protected. Reject collisions, approved allowlist
drift, unsafe inherited TLS defaults or incomplete captures.

Restart only scoped gateway/controller channels after checking owner/argv/start
identity; preserve existing workers and Ubuntu. Never invoke wholesale
`data/nl/manage.py stop` or `rollback`. Verify the selected gateway public Linux
CLI SHA256 equals the accepted bundle's CLI and installed checksum. Preserve
current macOS assets and prior bundle/binaries for rollback.

```sh
python3 scripts/prepare-host-ingress.py --state PRIVATE_CURRENT --approved PRIVATE_APPROVED --rollback-plan PRIVATE_PLAN --created-app-id RECORDED_CREATED_ID --output PRIVATE_ROLLBACK
```

The rollback generator compensates app-created/route-failed state, supports
repetition and preserves unrelated later routes. Review and apply only its owned
delta. Restore prior scoped gateway/controller binaries/config and reconnect
channels; no routine catalog restore or worker/Ubuntu stop. New participant
reservations remain conservatively registered after rollback. Actual complete
live capture, scoped cloud apply, a/b native reconnect and
public default-TLS enrollment/channel checks have passed. Rollback compensation
and repeat behavior were exercised offline; a real rollback and physical
second-host/NAT remain untested. See the verification report for scope.

The gateway's host manifest/blob handler selects `OW_HOST_BUNDLE_DIR`, defaulting
to `OW_ASSET_DIR/host-public`. Its existing Linux/macOS CLI handler separately
selects `OW_ASSET_DIR/bin/ow-linux-amd64` and checksum. A staged bundle is flat:
planner must select the accepted manifest/blob directory and place the matching
reviewed Linux CLI/checksum into that existing bin selection during authorized
rollout. Merely setting the bundle directory does not update the served CLI.
Compare both served and installed Linux hashes to the accepted build receipt;
retain existing macOS files and prior Linux artifacts. Core has prepared these
bytes in ignored storage and has not changed any live selection.

Public bundle downloads verify the SHA-256 and length of each opened raw or
compressed asset before returning GET or HEAD headers. File metadata is checked
around hashing but does not substitute for checking bytes: some filesystems can
retain identical timestamps after a same-length rewrite. Verification is serialized
to bound concurrent hashing. Repeat requests therefore also pay the full hashing
cost; large-bundle header latency on the deployed host needs separate measurement.

Mixed worker versions use protocol-v1 optional `max_guest_memory_mib` and
`max_guest_vcpus` capabilities. Missing fields mean the legacy per-guest ceiling
of 2048 MiB / 4 CPU, even when the enrolled host advertises a larger total budget.
New workers report 16384 MiB / 16 CPU. Placement and owning-route create, resize,
start, fork and restore admission enforce both the guest ceiling and host budget
before durable intent. Upgrade the worker before requesting larger shapes.

For a managed physical host upgrade, stop guests intentionally and run
**the old installed CLI's `ow --data-dir HOST_ROOT host stop` before replacing
that CLI**. Then install the reviewed new binary, configure/update assets while
stopped, and start. The installer preserves saved host/node configuration,
assets and workspace disks. Do not rely on a newly installed executable to stop
a worker still executing the deleted old binary inode.

## Linux vCPU overcommit

Guided Linux enrollment accepts `host join --vcpu-overcommit 1|2` (default **1**).
This raises the selectable **total running guest vCPU budget** to the smaller of
`logical host CPUs × ratio`, the invitation's CPU cap, and the 64-vCPU protocol
ceiling. `--cpus` is the effective guest budget, so it is never multiplied again.
For example, an eight-CPU host can consent to `--cpus 16 --vcpu-overcommit 2` if
the owner invitation allows 16. CPU-heavy jobs should use ratio 1; ratio 2 is an
operator choice for development workloads whose CPU bursts usually overlap little.
Neither ratio reserves exclusive host cores or guarantees latency.

Saved consent retains the ratio; older consent files default to 1. Changes use
`host configure --vcpu-overcommit 2` while participation is fully stopped, with
all existing resource/consent flags. Owner-side `host budget` is independent:
raise its CPU cap before increasing local consent. Lower only through the idle
inventory procedure above. RAM, eight running slots, disk admission, per-guest
shapes, and controller owner quotas remain independent limits. Multiple workers
sharing a machine still need operator-assigned budgets whose **combined** CPU
reservation fits the intended ratio; each must not claim the whole machine.
Native Mac participation currently supports ratio 1 only. Older CLIs reject the new
saved field; rollback requires the prior saved consent while stopped and a matching
owner cap, rather than deleting the field from an overcommitted configuration.

Low-level `ow host up --cpus N`, SSH `ow node add --cpus N`, and
`OW_MAX_VCPUS=N ow supervisor` already take an **effective** guest budget. Set N
explicitly to the intended ratio's budget; these operator interfaces do not
multiply it or discover disjoint capacity. Since 0.9.24 the worker caps it at the
[host reserve](#host-reserve-capacity-boxes-never-claim) limit, so an overcommitted N
also needs `OW_VCPU_OVERCOMMIT=2`. Worker status `max_vcpus` and the node heartbeat
advertise the capped budget for scheduler admission.

## Host reserve: capacity boxes never claim

A host keeps CPU and RAM for its controller and node agent, so boxes can neither
starve nor out-of-memory-kill the control path, overcommit included. Box admission
budgets are computed **after** the reserve:

- RAM budget = min(¾ of host RAM, host RAM − reserved RAM, 64 GiB).
- vCPU budget = min((logical CPUs − reserved vCPUs) × overcommit ratio, 64), rounded down.

| Host role | Reserved vCPU | Reserved RAM |
| --- | --- | --- |
| Controller host (controller with local runner, or controller only) | 1 | 2 GiB |
| Runner | 0.5 | 1 GiB |

Example: an 8-vCPU, 32 GiB runner with ratio 2 admits (8 − 0.5) × 2 = 15 guest
vCPUs and min(24, 31) = 24 GiB. A 2-vCPU controller host admits one vCPU at ratio
1.

Every Linux worker enforces this, whatever budget it was given (`ow host join`,
`ow host up`, `ow node add`, `ow supervisor`, AWS): it caps `OW_MAX_MEMORY_MIB` at
host RAM − reserved RAM and `OW_MAX_VCPUS` at (logical CPUs − reserved vCPUs) ×
`OW_VCPU_OVERCOMMIT`, and reports the capped budgets to placement. The reserve is
`OW_HOST_RESERVE_CPU_MILLIS` and `OW_HOST_RESERVE_MEMORY_MIB`; unset, a worker
keeps a runner's, and the worker `ow host up` starts keeps a controller host's.
`ow host join` passes its saved `--vcpu-overcommit`; low-level interfaces (`ow host
up --cpus`, `ow node add --cpus`, `ow supervisor`) need `OW_VCPU_OVERCOMMIT=2` in
their environment to keep an overcommitted budget, else it is capped at ratio 1.
A worker whose reserve leaves less than 256 MiB or one vCPU refuses to start and
says which settings to lower; on AWS, `apply` refuses such a reserve before
changing anything, and the controller keeps running.

The disk reserve (`OW_MIN_FREE_GIB`, default 5 GiB) is unchanged. `ow nodes` shows
all three as `host_reserve` (`cpu_millis`, `memory_mib`, `disk_gib`); workers
older than 0.9.24 report the CPU/RAM fields as null.

The AWS components set the reserve (`hostReserveCpuMillis`, `hostReserveMemoryMib`;
see [AWS](../deploy/aws/README.md#host-reserve)) and also make the kernel enforce it:

- The controller/node agent unit runs in its own top-level slice `owcontrol.slice`
  with `CPUWeight=1000` (boxes have the default 100), `MemoryLow` = reserved RAM
  and `MemoryMin` = half of it, and `OOMScoreAdjust=-900`.
- The worker and its VMs run in `owguests.slice`, capped at `MemoryMax` = host RAM
  − reserved RAM. Within it the worker is protected from its own VMs: its unit has
  `OOMScoreAdjust=-900` and a delegated cgroup, and the worker puts itself in a
  `control` child (`cpu.weight` 1000, `memory.min` 64 MiB, `memory.low` 256 MiB)
  and every VMM in a `guests` child. Each VMM raises its own OOM score to 500 at
  start, so under memory pressure the kernel kills a VM before the worker,
  controller or agent. Losing the worker would also stop its VMs (each VMM exits
  with its worker), so its protection matters as much as the controller's.

On every Linux host the VMMs raise their OOM score to 500 at start; the cgroup
split needs the delegated unit the AWS components install.

The node agent also sends heartbeats independently of the worker: worker status
and the beat built from it (which may hash a newly cached image) refresh on their
own thread, and its fsynced status files are written on another. A long capture
(hibernate, snapshot, fork) or a slow disk no longer makes the runner look
offline. A beat not refreshed for 1500 s closes the channel; worker requests time
out after 1200 s.

