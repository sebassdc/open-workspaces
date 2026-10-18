# Remote CLI discovery for an existing coding agent

Use the installed `ow` client to operate your own persistent remote workspace.
The agent running on your computer is the client; installing another agent inside
the workspace is optional. The operator supplies the server URL and admits your
identity. This guide does not grant admission or change tenant policy.

```sh
ow login https://workspaces.example.com
# For an operator-provided Tailscale server instead:
# ow login https://HOST.TAILNET.ts.net --tailnet
ow status
ow image list
ow nodes
```

Login may require human sign-in. After login the discovery and workspace commands
work with stdin closed. Selection order is `--server URL`, `OW_SERVER`, saved
login. `--local` ignores those remote defaults. Use `ow --server URL status` to
confirm a target explicitly. A supplied remote target never falls through to
local node inventory. Remote host-only commands fail rather than inspect an
unrelated local worker.

`status`, `nodes` and `image list` use the same authenticated state API as the
dashboard. Status preserves `server`, `connected`, and owned `machines` count and
adds `client_version`, `server_version`, your verified `identity.email`, discovery
compatibility and the discovery object. Server/client release strings are
informational; `compatibility.discovery_supported` specifically checks discovery
schema 1, not every possible operation or runtime feature. An older server may
still support list/exec while status reports discovery unsupported. Upgrade the
server/client before relying on remote discovery. No token, identity-provider
subject, host filesystem path, credential or another owner's machine list is
part of discovery. `owner_limits` is passed through when the server supplies it;
null means that this server has not reported owner limits. When reported, the
client also checks remaining owner CPU, memory, disk and box quotas before
offering an image or development choice; it does not invent quota defaults.

## Choose an image and resources

`ow image list` returns a JSON array with `name`, `available`, and per-node
`nodes`. The built-in names also appear when missing. Golden names come from
node advertisements. A node entry distinguishes `missing_image`, `offline`,
`draining`, `unsupported_capability`, `insufficient_capacity` and `available`.
Availability describes current advertisements and admission headroom; it does not
reserve capacity. Offline image advertisements remain visible but never available.
Revoked nodes are omitted. Images absent from all advertisements cannot be
assumed prepared. Remote listing does not build, upload or download an image. Discovery covers
connected-node advertisements; a legacy controller-local worker is not silently
substituted for that registry. An empty node array means no connected nodes.

For built-in Ubuntu/Arch developer images, a non-null `development_choice`
suggests 2048 MiB and two virtual CPUs only when they fit that node's observed
compute limits. Linux choices specify the existing 32 GiB disk default. Mac
choices omit disk sizing because that backend does not support it. A small
Alpine prototype is not suggested as a development environment. If no development
choice fits, wait or inspect the node's available resources and allowed memory
sizes; do not silently substitute a minimal image for a requested development box.

Choose an available image with a non-null development choice, then use those
resources, with a new workspace name and **without `--node`**:

```sh
# Example only: run this if image list currently offers this choice.
ow create project --image ubuntu --memory 2048 --cpus 2 --disk 32GiB
ow inspect project
ow exec project -- 'git --version; uname -m'
```

Automatic placement rechecks capacity. Discovery is an observation, not a promise
that a subsequent create will succeed: concurrent requests, host writes, image
changes, owner policy and worker admission can still refuse it. An unknown disk
observation (`disk_capacity_known: false`) requires worker admission to establish
disk capacity; it is never inferred from physical host totals.

`ow nodes` returns the sanitized placement nodes used by discovery:

- `configured`: administrator-configured memory, CPU and running-slot budgets.
  `effective`: those budgets clamped by the node advertisement.
- `reserved`: controller reservations across the pool's owners plus observed
  untracked running work; it includes uncertain effects. It contains aggregate
  numbers, not owner identities. `available`: effective minus reserved, saturating
  at zero. Offline/draining nodes have null availability, never a fresh estimate.
- `per_guest.memory_mib`: allowed memory sizes within the worker ceiling;
  `max_vcpus`: per-guest ceiling. CPU and RAM headroom must both fit; a free
  running slot alone is insufficient. Mac shape restrictions remain in force.
- `disk.configured_gib`, `reserved_bytes`, and `host_reserve_gib` reflect the
  worker's logical disk cap, all active disk reservations (including stopped and
  hibernated boxes), and its host reserve. `available_bytes` is the lesser of
  remaining logical cap and filesystem free space after host reserve and existing
  full logical reservations, matching [worker disk admission](DISKS.md).
- `workspaces` reports the worker box limit and observed count (including stopped
  boxes). Available box count is null without a fresh worker observation.
- `last_seen_unix_s` is the controller heartbeat time. `disk.observed_unix_s` is
  the worker observation time. Disk availability becomes null after 30 seconds,
  when offline/draining, or when the worker does not report it. Cached disk
  reservations and configured limits may remain visible with `fresh: false`.
- `capabilities` lists backend, architecture, runtime, node transport protocol,
  SSH/disk support and operation restrictions where advertised. An absent
  `operations` key preserves legacy Linux admission behavior. Explicit null and
  other non-array operation values project as an empty array and grant no
  operations. Other missing fields remain null; no Linux-only capability is
  inferred for a Mac runner.

Memory uses MiB (2^20 bytes), disks GiB (2^30 bytes), CPU values are counts of
virtual CPUs, slots are concurrent running reservations, and timestamps are Unix
seconds. Disk use inside one guest remains the separate `disk_used_bytes` field
on list/inspect. It is not admission headroom. These are configured budgets and
observations, not guarantees based on host RAM, CPU totals or old heartbeats.

## Everyday command route and output contracts

Use `ow COMMAND --help` for arguments. List/inspect/stats/snapshots and mutation
metadata keep their existing JSON output. Status/nodes/remote image list also
print one JSON value on stdout, without an interactive prompt or progress banner.

- `list`, `inspect NAME`: discover owned or explicitly shared workspaces.
- `exec NAME -- COMMAND`: noninteractive command; raw guest output on stdout,
  preserving the guest exit status. Guest stdout/stderr are currently combined.
  CLI diagnostics and operation retry keys go to local stderr.
- `put NAME LOCAL GUEST`, `get NAME GUEST LOCAL`: bounded file transfer (256 KiB
  remotely); get refuses to replace an existing local file. SSH/SFTP handles
  larger transfers. `ssh-authorize`, `ssh-info`, `ssh-config`, `ssh` provide the
  explicit guest public-key enrollment and OpenSSH route; private keys stay local.
- `publish NAME --port PORT`, `unpublish NAME --port PORT`: guest HTTP previews
  when supported by the selected server. Publish prints its URL on stdout;
  unpublish prints JSON metadata. Follow that server's preview access
  policy; discovery does not enable public access.
- `start`, `stop`, `hibernate`, `snapshot`, `fork`, `restore`, `delete`: lifecycle.
  Stop preserves disk but ends processes; hibernate preserves paired state;
  restore replaces later changes. Delete is deliberate and owner-scoped.

Host participation, enrollment, local image build, backup, `node` (singular),
`up`/`down`, `dashboard`/`serve` and `node-*` are host/operator commands, not steps
needed by a remote coworker agent. `host invite/budget/revoke` additionally require
owner administration. Local image list keeps its existing golden-image JSON
schema and `--runner` option; remote listing refuses `--runner`.

## Failures and safe retries

CLI/API failures exit nonzero (normally 1); command syntax errors use Clap's
nonzero status. A guest command returns its actual exit status, which may also
be 1. Discovery failures leave stdout empty and explain the cause on stderr.
Recognizable failure codes are:

- `authentication_or_admission`: sign in as yourself; ask the administrator to
  admit that identity when access is denied. No automatic policy changes.
- `server_unreachable`: check the intended URL, tailnet/network and TLS trust.
  Never disable certificate verification to make discovery pass.
- `unsupported_capability`: update compatible client/server/runner components,
  or choose an advertised capability/shape. Status on an older server reports
  discovery unsupported without claiming that list or exec are incompatible.
- `unavailable_image`: run image list and choose a prepared online image.
- `insufficient_capacity`: inspect nodes and image list; choose fitting resources
  or wait. Full disk reservations count even when disks are sparse.
- `node_unavailable`: wait for the intended runner; existing placement stays fixed.

Admission codes are additive API `error_code` fields (or an operation's stored
`error_code`) and included in CLI diagnostics; arbitrary worker errors retain the
existing safe generic message and operation metadata. Unknown or uncertain
failures are not proof that a mutation had no effect.

Before sending a remote operation the client saves its request privately under
its existing config `operations/KEY.json` and prints `--operation-key KEY` on
stderr. **Retain that key after a timeout, disconnect or uncertain response.**
Inspect `ow list`, `ow inspect NAME` and status operation metadata, then retry the
same request with `ow --operation-key KEY create ...` (or its original command).
Do not create a new box or invent another key merely because the response was
lost. Reusing a key with a different request is refused. A deliberately changed
intent after a confirmed failed operation uses a new key. This discovery change
does not alter the existing operation replay behavior or introduce an async job
service.
