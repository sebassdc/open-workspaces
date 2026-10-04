# Invited Linux hosts

This implementation admits trusted operator hosts to an owner-managed shared
pool. Invited friends do not need a human Access login. Admitted humans can use
pool workspaces; host operators can inspect or stop workloads on their hardware.
Participation gives no human workspace-management authority. Physical-host/NAT
acceptance remains pending until the owner runs the commands on the second host.

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
Only verified manifest-selected images are advertised. Ubuntu delivery is optional
and raw: 8,589,934,592 image bytes plus the helper/common files, despite sparse
physical storage. The verified v3 complete bundle transfers 8,778,379,912 asset
bytes; Alpine selection transfers 186,049,272. Admission requires full selected
logical bytes plus the saved disk reserve and 512 MiB staging overhead, retaining
prior assets. Each TLS file download has a 1,800-second deadline. Interrupted
transfers are restarted; no compressed/resumable delivery claim.

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
