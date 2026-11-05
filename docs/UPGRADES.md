# Upgrading a host in place

From `deploy/aws` 0.5.0 a release is applied to a running AWS host, touching
only what the release changed. The decision record is
[ADR 0005](adr/0005-change-classes-and-engine-boundary.md); the operator view
is in [deploy/aws/README.md](../deploy/aws/README.md#upgrades-in-place).

## Change classes

| Class | What restarts | Running boxes | Open SSH and terminal streams |
| --- | --- | --- | --- |
| 0 config | nothing, or a file is rewritten | untouched | untouched |
| 1 control plane | `open-workspaces.service` (controller and node agent) or `open-workspaces-dashboard.service` | untouched: processes, RAM, idle timers | dropped once; clients reconnect |
| 2 runner | `open-workspaces-worker.service` | kept running and adopted by the new worker when the VMM and guest kernel stay ([hand-over](#worker-hand-over-keeps-boxes-running)); otherwise hibernated, worker swapped, resumed, and processes continue when every capture still matches | dropped |
| 3 host | the instance | hibernated before, resumed after; a new host kernel boots RAM states cold | dropped |

A release's class is the highest class of the units it moves on that host; a
unit whose code did not change since the release it runs is not touched.

| `changes` value | Class | Unit (pin) it moves |
| --- | --- | --- |
| `component`, `config`, `guest-agent` | 0 | none (`guest-agent` reaches boxes with the next worker move) |
| `dashboard` | 1 | dashboard |
| `controller`, `agent` | 1 | agent (`open-workspaces.service`) |
| `worker`, `vmm`, `guest-kernel` | 2 | worker, behind the capture gate |
| `host-layout` | 0-2 | the units whose rendered files differ (`ow-host render` of the target) |
| `stage0` | 3 | refused in place: hibernate, then `pulumi up` |

## Host layout v2

```text
/srv/ow/                      data volume (Btrfs), survives replacement
  src/                        git clone; releases are worktrees of it
  releases/VERSION-COMMIT12/  immutable once STAGED is written
    src/                      worktree at the release commit
    src/data/runtime-spike -> ../../assets
    src/target -> /srv/ow/build/target     shared, incremental cargo builds (source builds only)
    assets/                   this release's own asset set (a reflink copy, then updated)
    bin/ow                    the released binary, or copied from the build
    STAGED                    version, commit, ref, sha256 of bin/ow, staging seconds,
                              source ("release" or "build")
  run/worker -> ../releases/…    pin of open-workspaces-worker.service (its assets are the live set)
  run/agent  -> ../releases/…    pin of open-workspaces.service
  run/dashboard -> ../releases/… pin of open-workspaces-dashboard.service
  current -> releases/…          the CLI wrapper and ow-host
  .ow/host-config.json           last applied config
  .ow/upgrade.json               upgrade journal
  .ow/pending.json               units a config change left to restart
```

Staging never writes a file a running process uses: it builds inside its own
release directory. The worker unit runs `ow supervisor` (hibernate on stop); the
control plane runs `ow host up --external-worker`, which never starts a worker.
They are tied by `Wants=`/`After=` only.

### Controller-only hosts

`role: "controller"` installs only `open-workspaces.service` (the existing
listener via `ow host up --controller-only`), the optional dashboard and the
configured catalog backup timer. It creates agent/dashboard pins, no worker pin.
Boot, readiness, restart and upgrades use only installed control-plane units;
readiness requires the listener on the expected tailnet address and an answering
node inventory, even before any node enrolls. Reported local CPU/RAM/slots are zero.
`restart --worker` is refused. Applying a different role to an existing host is
refused before rendering or stopping units; use a separately planned cutover.

Staging installs only the released `ow` (no setup script), or, when it builds,
uses `scripts/setup-local.sh --controller-only`: it reuses the pinned
Rust toolchain but builds only `ow`, without Firecracker, guest builds, network
assets or KVM preflight. Upgrades neither probe a local worker nor inspect
captures, quiesce, drain or canary local boxes. Worker-only release changes do
not move any controller host pin; catalog/protocol compatibility still applies.
The optional dashboard pin moves with the controller for a catalog schema change,
even while its unit is disabled. Pin-only moves do not restart absent units;
enabling the dashboard later uses the migrated catalog's compatible release.
Controller/dashboard restarts interrupt streams; remote worker pins are untouched.

This role adds no catalog migration or protocol/worker interface change.
Packaging is class 1 for the new role and class 0 component/config on unaffected
existing hosts. The role uses the existing stage 0 byte-for-byte: adding the
option does not replace existing hosts or require a stage 0 upgrade.

## Config: one SSM parameter

User data holds stage 0 values only (data volume, parameter name, region, stage-0
version), so it changes only when `bootstrap.sh` does. Everything else is
`/open-workspaces/NAME/config` (String, JSON, no secrets: the Tailscale key
stays in Secrets Manager):

```json
{"schema": 1, "role": "host",
 "release": {"repository": "https://github.com/…/open-workspaces.git", "ref": "v0.5.0"},
 "tailscale": {"secretArn": "arn:aws:secretsmanager:…", "hostname": "NAME", "ssh": false},
 "dashboard": {"logins": ["you@example.com"]},
 "developerImages": ["ubuntu"],
 "backup": {"url": "s3://bucket/prefix", "schedule": "*-*-* 01:00:00 UTC"},
 "idleMinutes": 30}
```

`ow-host apply` fetches and validates it, records the restarts the new files
need as pending before writing any (so a retry after a failure, in setup or
later, keeps them), re-renders the files it owns (a systemd or AppArmor reload
they need stays due across a failure), acts, and stores the config last. It acts by class: logins restart the dashboard alone, images and the backup
timer start, Tailscale SSH and hostname are set, a grown volume is resized. A
change that needs the control plane (hostname) or the worker (idle minutes) is
recorded as pending; `ow-host upgrade` carries it out.

## Release impact manifest

`release-impact.json` at the repository root, newest first, one entry per
release. The release PR adds its entry.

```json
{"version": "0.5.1", "class": 1, "changes": ["dashboard"],
 "protocol": {"speaks": 1, "accepts": [1, 1]},
 "worker_protocol": {"speaks": 1, "accepts": [1, 1]},
 "catalog": {"schema": 3, "min_reader": 3},
 "runtime": {"vmm": "firecracker-v1.17.0", "guest_kernel_sha256": "…", "resumes": ["firecracker-v1.17.0"]},
 "host_layout": 2, "in_place_from": "0.5.0", "notes": "…"}
```

`deploy/aws/test/release.test.mjs` (in `npm run check`) checks that versions are
ordered and this package version has an entry, that `class` is the highest of
its `changes`, that the protocol, worker protocol, catalog schema, runtime and
guest kernel pin match the code, and that the diff since the previous tag is
declared: `bootstrap.sh` needs `stage0`, `deploy/aws/host/` needs `host-layout`,
worker files need `worker`, dashboard files `dashboard`, `nodes.rs`,
`catalog.rs` or `selfhost.rs` need `controller` or `agent`, the Firecracker pin
`vmm`, the kernel pin `guest-kernel`. An entry may carry
`"waivers": {"worker": "reason"}`.

## Interface rules

- **Host-runner protocol.** A controller accepts `PROTOCOL_MIN..=PROTOCOL`
  (`crates/ow-core/src/common.rs`) and the runtimes in `RUNTIMES`; a runner speaks
  its own `PROTOCOL`. Controllers upgrade first. A refused heartbeat closes the
  session with code 4001 and the reason, which the agent logs. No bump for
  optional heartbeat fields or capability flags; bump for anything a peer one
  version older would reject or misread. Raise `PROTOCOL_MIN` only in a release
  after one that already speaks the new protocol, so a bridge exists.
- **Agent-worker.** Worker status reports `version`, `worker_protocol` and
  `compat` (runtime, CPU string, guest kernel hash). An agent drives only a
  worker inside `WORKER_PROTOCOL_MIN..=WORKER_PROTOCOL` (a worker without the
  field speaks 1); otherwise it refuses at start, or stops heartbeats, and logs
  why. The planner moves the agent and worker together when the pair a
  selective upgrade would leave falls outside the agent's window. New worker
  operations are additive.
- **Guest interface.** Additive forever: disks keep the guest tools of the
  release that created them.
- **Catalog.** Migrations are forward-only. Each sets `settings.min_reader_schema`
  to the oldest schema whose binaries may still use the catalog; a binary opens a
  newer catalog only under that floor, and never lowers `user_version`. The
  upgrader copies the catalog before a release that changes the schema, and an
  automatic rollback is allowed only when the old binaries can read it.

## `ow-host upgrade`

```text
fetch → plan → stage → gate → [catalog copy] → [drain → [stop-boxes →] hibernate | handover] → stop → [sync images]
      → switch → start → verify → [adoption] → [resume] → [undrain] → done
```

- **fetch**: `git fetch` the ref (default: the config's `release.ref`), read its
  manifest and workspace version. Going back to an older release plans from the
  newest running release's manifest.
- **plan** (pure, `upgrade.plan`): per unit, the releases it has not run decide
  whether its pin moves; plus units whose rendered files the target changes and
  pending config restarts. Refused: an entry with `stage0` or an
  `in_place_from` newer than the host; another runner below the target's
  protocol floor (names the bridge release); a capture the target cannot resume
  (full tuple: runtime in `resumes`, this host's CPU string, the target's guest
  kernel hash), unless `--accept-cold`; a class above `--max-class` (default 2).
  `--plan` prints the plan and stops.
- **stage**: a worktree, a reflink copy of the live asset set, the release's own
  `scripts/setup-local.sh`, which fetches the release's pinned guest kernel even
  when the copied set has a base image (niced, `CPUWeight=10`, log in
  `/var/log/open-workspaces-stage.log`), `bin/ow --version` must match, `STAGED`
  written last. A complete release is reused. See [Released binaries](#released-binaries)
  for where `ow` and the guest agent come from.
- **gate**: again, with the staged kernel's hash and the boxes as they are now.
  Disk-only recovery points hold no RAM state and are not gated.
  With `--accept-cold` and backups configured, `ow-backup` runs first.
- **timers**: when the journal opens, the upgrader records which of
  `ow-recovery.timer` and `ow-backup.timer` are active, stops them (stopped, not
  disabled) and stops a recovery run in flight; a recovery run's capture or hash
  would otherwise hold the one-request worker during drain, start or verify, and a
  killed upload is retried on its next run. A backup run in flight finishes: it
  asks the worker for nothing. Exactly the recorded timers start again when the
  journal reaches done, aborted or rolled-back. `ow-host apply` enables but does
  not start them while a journal is open.
- **hibernate**: the worker is quiesced first (it then refuses starts and wakes),
  so the running boxes read next are exactly the ones the old worker's own
  `down --hibernate` captures; they are recorded in the journal before it acts.
  The capture gate runs a last time here, on every capture now present, so a
  box started after planning cannot lose its RAM state without `--accept-cold`;
  a refusal aborts (quiesce lifted, drain removed). Each box has its own budget:
  120 s, plus 20 s per GiB of RAM, plus 4 s per GiB of allocated disk. The first
  box that fails or overruns it ends `down --hibernate` at once (the worker keeps
  running, starts are allowed again) and the upgrade aborts; the upgrader's own
  cap is the sum of the budgets plus 600 s. A hibernation is chunk-sealed and
  reuses the chunks it shares with the box's last durable recovery point, so it
  rereads only what changed since. Ephemeral boxes (a CI job's, see
  [the CLI](AGENT_CLI.md)) are stopped instead of hibernated and are not resumed.
  When the plan hands the worker over, this step is `handover` instead
  ([below](#worker-hand-over-keeps-boxes-running)).
- **stop-boxes**: with the controller setting `upgrade.running_boxes` set to
  `stop` (`ow config set upgrade.running_boxes stop`; default `hibernate`), a
  worker restart without hand-over (`worker_path: stop` in `--plan`) first stops
  each recorded running box (`ow stop`: seconds; RAM state lost, disk kept),
  then the hibernate step hibernates what is left: managed boxes, which only
  their controller may start again, and ephemeral boxes are stopped as before.
  The resume step starts the stopped boxes again after verify (a cold boot);
  abort, rollback and recover do the same. `--hibernate` forces hibernation.
  Hand-over, when the plan can take it, still wins. The capture gate still counts
  the running boxes, so a release whose worker cannot resume them asks for
  `--accept-cold` even though they are stopped.
- **start**: the new worker is quiesced as soon as it answers, before the agent
  and the dashboard start, so no wake on connection reaches it until verify passes
  (a large box's resume hashes its hibernation snapshot first and would hold the
  one-request worker past verify's deadline). Rollback starts the old worker the
  same way. The resume step lifts the quiesce; a lift that fails stops the upgrade
  at resume, where the next recover lifts it again (on rollback, the next recover
  restarts the old worker, which starts unquiesced).
- **verify** (each unit against its own pin): the unit is active and its
  `/proc/PID/exe` is its pin's `bin/ow`; the worker answers with its pin's
  version (from a release with this note on, the worker answers `status` and
  `egress-enable` on its own thread: while a long request such as a snapshot hash
  holds it, `status` is the last answer marked `"busy": true`, with live
  `internet`); the runner is online with the agent's and worker's versions; the
  controller listens on the tailnet address; the dashboard answers. A class 2
  upgrade with no box to resume runs a canary box.
- **abort** (any failure before the switch): start the old worker if it is down
  (else lift its quiesce), resume every recorded box, start stopped units,
  undrain. A worker whose unit is active but that does not answer (a
  `down --hibernate` whose client gave up while the worker finishes shutting down)
  is waited for, up to 30 min, until it answers or exits; one that exited starts
  again. Listing or resuming the boxes is tried twice; a failure then is
  recorded in the journal error, not raised, so the drain, units and timers are
  restored and the journal reaches `aborted`. **rollback** (switch, start or verify): pins back, units restarted and
  verified on the old releases, then as abort. A box that does not resume stays
  hibernated (it wakes on its next connection) and is reported.
- **events**: each step that ends while the host's agent (or, on a controller-only
  host, the controller) listens is handed to its telemetry intake at once as an
  `ow.host.upgrade.step` event, so the drain and the hibernation show up while
  the upgrade runs (each when it ends, with its duration); when the journal
  reaches done, aborted or rolled-back, the steps it missed follow, then one
  `ow.host.upgrade` event (see [telemetry](TELEMETRY.md)). A rollback that itself
  fails leaves the journal open for the next recover and reports nothing yet.
  They land in the runner's event log (`ow health`, the admin API's `health.get`;
  a controller-only host's under `controller`) and are forwarded with the other
  events, so each upgrade's history outlives `upgrade.json`, which keeps only the
  last journal.
- **first upgrade to 0.9.56**: it still runs the previous release's upgrader and
  hibernation (840 s for all boxes, whole-disk sealing). On a host with a box whose
  hibernation takes longer (about 4 s per GiB of allocated disk), hibernate that
  box by hand first (`ow hibernate BOX`); it wakes on its next connection.
- **recover**: every upgrade and every boot (`open-workspaces-recover.service`)
  reads the journal first and aborts, rolls back or finishes. It first stops the
  recorded timers and any timer a reboot started again (adding those to the
  record), so a recovered rollback meets no recovery run either.

Old releases are pruned: every pinned release plus the two newest others stay.

### Released binaries

Staging (first boot and `ow-host upgrade`) installs the release's published
binaries instead of compiling them when all of these hold:

- the pinned commit is the release tag `vVERSION` (VERSION from its
  `Cargo.toml`); a branch or commit that is not the tag builds;
- `repository` is a GitHub `https://github.com/OWNER/REPO` URL, whose release
  `vVERSION` is downloaded by exact tag, never `latest`;
- the host is x86-64 and the release's `SHA256SUMS` lists `ow-linux-amd64`, plus
  `ow-guest-linux-amd64` on a host with a worker (0.9.19 is the first release
  that publishes the guest agent).

Then `source-commit.txt` and each binary must match `SHA256SUMS`, and
`source-commit.txt` must name the pinned commit. Any mismatch is refused before
anything is written to the release directory, so a retry starts clean. The
static `ow` becomes `bin/ow` and the dashboard's Linux CLI download; a worker host
still runs the release's `scripts/setup-local.sh` for Firecracker, the guest
kernel and network assets, but with the verified guest agent and no Rust
toolchain or cargo build (a fresh volume without a base image still installs the
toolchain for the image's test fixture). A controller host runs no setup script.
The dashboard is part of `ow`.

Otherwise, or with `ow-host upgrade --from-source` (`ow-aws upgrade --from-source`),
the release builds on the host as before. The output names the path:
`installing … from the released binaries of vX.Y.Z` or `building … from source:
REASON`, and `STAGED` records `"source"`. The upgrader that runs is the host's
current `ow-host`, so a host on 0.9.17 or older builds when it stages 0.9.19;
later upgrades install.

## Runners

A [`Runner`](../deploy/aws/README.md#runners) host has the same layout, pins and
journal, with `role: runner` in its config: its `open-workspaces.service` runs
`ow node-agent` against the controller recorded at enrolment (no controller, no
dashboard). `ow-host upgrade` works the same, with these differences:

- **plan**: there is no catalog on a runner, so no catalog copy. When the agent
  pin moves, the target's protocol must be inside the window of the release the
  controller runs (`--controller-version`, which `ow-aws upgrade` reads from the
  controller host); otherwise the upgrade is refused (upgrade the controller
  first). A controller release missing from the target's manifest is refused too.
- **verify**: the agent's own report, `/srv/ow/.ow/runner/agent.json`, written
  after each heartbeat the controller kept the channel open for: online,
  updated within 15 s, with the agent's and the worker's releases. An agent
  that stops (SIGTERM) takes no new jobs, advertises `drain`, lets accepted jobs
  finish and writes itself offline.
- An unenrolled runner (no credential yet) has no agent to restart or check.

Fleet order: the controller host first, then each runner, one at a time; a
controller accepts the protocol it speaks and the one before, so a runner may
run one protocol version behind its controller but never ahead.

## Sandbox EC2 check (2026-10-17)

Observed on one m8i-flex.xlarge host (Ubuntu 24.04 AMI `ami-02a9cdd4e98bc3c78`,
host kernel 7.0.0-1014-aws, 4 vCPUs), throwaway stack, destroyed afterwards. One
Alpine box (256 MiB) ran a counter (`echo $i > /tmp/t` every second) whose PID
was read before and after each step. The releases were test tags built from this
branch (version bumps plus manifest entries; one with a worker that exits at
start). Each measurement is a single run (n = 1), so no p50/p95.

| Step | Result |
| --- | --- |
| Planned replacement v0.4.1 (layout v1) → 0.5.0 | `pulumi up` 80 s; stage 0 with build 4-5 min (cargo target moved from the old layout, incremental). Same data volume, tailnet device and address (address withheld), dashboard back. The box hibernated on the old instance resumed warm: counter PID 293 kept counting. |
| `pulumi preview` after a `ref` change | only the config parameter updates; the instance is unchanged |
| Class 1 (controller, agent, dashboard) | worker PID and Firecracker PID unchanged, counter PID 293 kept counting; control plane and dashboard on the new release. Journal: stop 1.0 s, switch 0.1 s, start 0.1 s, verify 3.1 s. Whole command 266 s, of which staging (build on the host) about 250 s. |
| Dashboard-only release | only the dashboard restarted; the agent stayed on the previous release (same PID) and verify passed against the per-unit pins |
| Class 2 (worker) | drain, hibernate 2.1 s, switch/start/verify 3.2 s, resume 0.4 s; new worker and Firecracker PIDs, counter PID 293 continued; the agent kept its PID (it waits for the worker) |
| Injected rollback (worker exits at start) | verify failed after its deadline (180 s then; 1800 s since 0.9.48), pins went back, the old worker started, the box resumed with its process |
| Upgrader killed (`SIGKILL`) at verify | `ow-host recover` rolled back (6.7 s) and resumed the box with its process |
| Step back to 0.5.0 (`ow-host upgrade v0.5.0`) | class 2, all three units back to 0.5.0, box process kept |
| `ow-aws restart` / `restart --worker` | 5 s / 9 s end to end, box process kept |
| Job in flight during `systemctl restart open-workspaces` | an `exec` of `sleep 8` sent through the controller finished and replied (8.1 s); the restart waited for it (6.2 s) |
| Tailnet | `tailscale ping example-host` answered after every step (address withheld); the dashboard answered 401 over the tailnet to a login it does not list |
| Disk | each release directory: 1.51 GiB, 453 MiB exclusive (`btrfs filesystem du`) |

Re-run after the review fixes (2026-10-18, a fresh layout v2 host from this
branch, same instance type and AMI, destroyed afterwards): an `idleMinutes`
change applied by `ow-aws apply` was left pending and carried out by
`ow-host upgrade` as a worker restart (the new worker runs
`OW_IDLE_HIBERNATE_SECS=600`; box process kept, agent untouched); a class 2
release then quiesced the worker over its control socket, passed the final
capture gate (no cold capture), hibernated (1.8 s), swapped and resumed
(0.4 s) the box with its process; verify 6.1 s; the dashboard answered over
the tailnet.

Found and fixed during the run: a replaced host keeps its runner's enrolled
node ID but gets a new hostname, so readiness now reads the ID from the runner
credential; the build log flooded SSM's output limit and now goes to a file;
going back to an older tag was refused because its manifest predates what runs.

Not run on the host (covered by `test_upgrade.py`): abort after a partial
hibernate, the guest-kernel-only refusal, the protocol bridge refusal, and a
catalog schema change (the catalog stays at schema 3 through 0.5.x).

## Sandbox EC2 check: a Runner (2026-10-18)

Observed on a throwaway stack in the sandbox account, destroyed afterwards: a
Host `example-host` (m8i-flex.xlarge, 4 vCPUs, 16 GiB) and a Runner `example-runner`
(m8i-flex.2xlarge, 8 vCPUs, 32 GiB, Intel Xeon 6975P-C), both on AMI
`ami-02a9cdd4e98bc3c78` (host kernel 7.0.0-1014-aws), `developerImages:
["ubuntu"]`, `idleMinutes: 0`, an `ArtifactStore` for backups. The releases were
this branch plus test tags (version bumps and manifest entries). A box `keep1`
on the Host's own runner ran a counter (`echo $n > /tmp/t` every second) for the
whole run; its PID and guest uptime were read after every step. Each
measurement is a single run (n = 1), so no p50/p95.

| Step | Result |
| --- | --- |
| `pulumi preview` adding the Runner to a running Host | 15 creates (the Runner's resources and the controller role's `example-runner-invite` policy); nothing of the Host changes |
| `ow-aws --host example-host runner-add example-runner` | 35 s: join minted on the controller into the runner's secret, redeemed, secret overwritten with `{"used": …}`, runner online (23664 MiB, 8 slots, 8 vCPUs) |
| Placement | four automatic creates alternated between the two runners; `--node` put a box on each |
| Runner 2 box lifecycle | create, exec, terminal (`ow shell` through a pty), SSH as `dev` (Ubuntu box, `ow ssh-config` with a pinned host key), snapshot, fork (the fork stayed on runner 2 with the source's files), hibernate (4.8 s) and wake on exec (same PID, counter carried on) |
| Golden image | the same recipe built on each runner (`ow-aws --host … image`); both advertised it without a restart and booted a box from it |
| Runner 2 control-plane restart | 10 s; runner 2 box process kept |
| Runner 2 worker restart | 103 s, six boxes hibernated and resumed with their processes |
| Runner 2 in place: v0.6.0 (claims protocol 2) with the controller on 0.5.0 | refused at plan: "the controller (0.5.0) accepts protocol 1..1 … upgrade the controller first" |
| Runner 2 in place: class 1, then class 2 | 268 s and 365 s (both mostly the build); the runner ran agent 0.5.1 and worker 0.5.2 against a 0.5.0 controller |
| Controller class 1, then the runner | controller 264 s, runner 2's Firecracker PIDs and box process unchanged; then the runner's agent followed (`Stopped: no jobs in flight.`, then `agent.json` online with 0.5.1) |
| Runner 2 offline (`ow-aws stop`) | 110 s; its boxes listed `offline`, an automatic create went to runner 1, `--node example-runner` and exec on its boxes were refused; `ow-aws start` (96 s) resumed all six with their processes |
| Runner 2 replacement (`pulumi up --replace` its instance) | 73 s; only its instance and volume attachment replaced, 32 resources unchanged; it came back as node `example-runner` (credential on the data volume), `ci-post-up` 44 s, a box resumed warm on exec |
| Admission | filling with 2048 MiB boxes: runner 1 stopped at 4 of 4 vCPUs, runner 2 at 8 of 8 slots; the next create and a 256 MiB one were refused |
| Backups | `ow-backup` on each: the catalog and runner 1's captures under `PREFIX/`, runner 2's captures under `PREFIX/runners/example-runner/snapshots` only; `ow-aws --host example-runner backups` lists them with owners (run on the controller) |
| Dashboard | `/api/state` over the tailnet listed both nodes and every box with its node |
| Runner removal | `runner-remove` refused while online; after the Runner was destroyed (15 deletes, 19 unchanged), it revoked the node |

`keep1` kept the same PID through every step above (its counter matched its
uptime), except where it was hibernated on purpose.

Found and fixed during the run: developer images never built on a layout v2
host (the build VM's socket path under `/srv/ow/releases/…` is longer than
Linux allows); the component did not compile for a consumer without
`strictNullChecks`.

## Tailnet previews (v0.8.0)

Preview routing alone is class 1: update the controller/dashboard and every
runner agent, including the agent on the controller host. The combined v0.8.0
release also fixes snapshot capture admission, so it is class 2 and requires
one warm worker hibernate/swap/resume cycle. Keep capture compatibility and
cold-memory-loss gates; protocol, guest images and host instances are unchanged. The host
layout installs the restricted `preview-serve` helper and its sudo rule.
The helper manages only dedicated HTTPS ports 11000–65535, with a fixed
loopback dashboard upstream; unrelated Serve listeners are never reset.

Publication records are an additive catalog table; the schema/read floor stays
at 3 and older binaries ignore it. Older gateways never route preview hosts.
No new dashboard JSON config keys are written, preserving older gateway config
readability during rollback. Upgrade runner agents before accepting previews
on them: older agents reject the additive `tunnel` stream operation.


## Shared artifact images (v0.9.0)

The opt-in [artifact store and shared images](ARTIFACT_STORE.md) add catalog
schema 4 with reader floor 3 and versioned worker image selectors. This is a
class 2 release: copy the catalog, move controller/dashboard together around the
schema change, then use the normal worker hibernate/swap/resume path. Compatible
schema 3 binaries retain catalog SQL read/write access for rollback; they do not
implement the new worker selectors or image administration.

This release retains v0.8.6's worker interface 2 (agents accept workers 1–2)
and its dormant managed-copy fences. Runtime, guest kernel and host-runner
transport pins are unchanged. The catalog reader floor does not permit a legacy
worker to read managed worker state; preserve that state's separate reader guard.
Keep capture compatibility checks and explicit cold-memory-loss consent. Store
activation requires separate opt-in bucket/IAM/configuration; publishing this
release alone does not activate it or replace a host.


## Recovery points (v0.9.2)

The opt-in [recovery timer and incremental snapshots](RECOVERY.md) require the
normal class-2 worker hibernate/swap/resume cycle and a dashboard update for
last-durable/error/retry status. Applying the timer configuration never restarts
the worker; recovery remains off until explicitly enabled against a safe store.
Data volumes are retained for salvage when their resources are removed.

Catalog schema4/reader floor3, worker interface2/readers1–2, host-runner protocol,
runtime, guest kernel, host layout and stage0 are unchanged from v0.9.0. Keep
capture compatibility and explicit cold-memory-loss gates. A published release
alone neither activates recovery nor upgrades a running host.


## Placement catalog (v0.9.3)

The dormant [placement and transfer contract](LIFECYCLE.md) raises the catalog to
schema 5 with minimum reader 5. From v0.9.2 this is class 1: copy the catalog and
move controller, dashboard and agent catalog readers together, retaining the
worker pin. Worker interface 2, host protocol 1, runtime, guest kernel and host
layout are unchanged. Existing recovery configuration stays as configured;
installing this release does not activate managed movement.

Schema-3/4 writers are refused. The upgrade plan requires a catalog backup and
disables automatic rollback across this reader-floor boundary. Freeze mutations
before upgrading, retain the private consistent backup, and follow the controlled
recovery procedure if rollback is needed after new writes. A stale backup cannot
authorize older placement epochs or discard new transfer/artifact roots.

## Negotiated cache operations (v0.9.7)

[Cache inventory and placement](CACHE.md) are class 1; worker eviction and staging
enforcement are class 2 and require the normal worker upgrade action. The optional
capability and controller acknowledgment gate the new shape and operations.
The additive catalog guards retain schema/read floor 5. The v0.9.7 entry moves
controller, dashboard, agent and worker pins; its class-2 worker action retains
capture compatibility and explicit cold-memory-loss gates. Protocol windows,
runtime, guest kernel, host layout and stage 0 are unchanged from v0.9.4.

### Move/fork worker interface 3

Version 0.9.12 is class 2. Move/fork needs interface-3 worker enforcement of
unstarted child preparation, durable target exclusion and serialized SSH-policy
installation. An agent upgrade alone cannot advertise this capability. Agents
accept workers 1–3; older workers remain fixed-placement. New child/exclusion
registries use worker state schema 3 and refuse older readers. Catalog schema and
reader floor remain 5. Use the existing dedicated-runner quiesce/swap/resume
procedure, full RAM compatibility tuple and explicit cold-loss consent. Existing
worker pins and boxes are not changed merely by installing a controller release.

### Chunk-sealed recovery points

Version 0.9.20 is class 2. The worker seals new managed recovery points under
their chunk lists ([RECOVERY.md](RECOVERY.md#chunk-seals-reading-only-what-changed))
and reports `snapshot_delete_v1`. Upgrade the worker with the normal
quiesce/swap/resume procedure; the uploader timer needs no change. Points
already uploaded keep their whole-file seals and stay restorable; the first point
after the upgrade reads its files in full, later ones only what changed. A worker
rolled back to an older release cannot verify chunk-sealed points: it fails
closed, and a pending chunk-sealed point keeps failing until the worker is
upgraded again. Restoring a chunk-sealed point needs a 0.9.20 or newer CLI. Protocol windows, worker
interface 3, catalog schema 7, runtime, guest kernel and host layout 2 are
unchanged.

### Host reserve

Version 0.9.24 is class 2 on runners and controller hosts with a local worker, and
class 1 on a controller-only host. The worker reports the
[host reserve](HOSTS.md#host-reserve-capacity-boxes-never-claim) and AWS hosts
lower its CPU (and, on small hosts, RAM) budget by it; every Linux worker also
caps its configured budgets at host capacity less the reserve (a runner's by
default). The worker and control units move into the `owguests`/`owcontrol` slices
with their protections, and VMMs start with OOM score 500. Use the
normal quiesce/swap/resume procedure; a running box beyond the lowered budget
stays hibernated until capacity frees. The node agent's heartbeats no longer wait
on worker status, cache inventory or status-file writes. Protocol windows, worker
interface 3, catalog schema 7, runtime, guest kernel and host layout 2 are
unchanged.

## Worker hand-over keeps boxes running

From 0.9.59 ([ADR 0011](adr/0011-worker-handover-keeps-vmms-running.md)) a
worker restart that keeps Firecracker and the guest kernel does not hibernate
anything: the old worker detaches from its VMMs (`ow down --detach`) and exits,
the VMMs keep running in the unit's `guests` cgroup (`KillMode=process`), and
the new worker enters their namespaces and adopts each one (PID and start time,
Firecracker binary hash, guest kernel hash, then its console and log pipe
through `pidfd_getfd`, its API and a guest command within 10 s). Guest
processes, RAM and clock are untouched; only the worker swap is downtime.

- **When**: `ow-host upgrade` plans `worker_path: handover` when no release the
  worker crosses lists `vmm` or `guest-kernel`, both releases' `runtime.vmm`
  and guest kernel hash are equal, the target's `runtime.handover.accepts`
  covers the running release's `runtime.handover.speaks`, the worker's unit
  files render unchanged and no worker config change is pending, the controller setting `upgrade.worker_handover` is
  on (default; the node agent stores it in `runner/controller-settings.json`)
  and `--hibernate` was not passed. `--plan` prints the path and why. Otherwise
  the worker restarts through hibernation as before, or by stopping the boxes
  when `upgrade.running_boxes` is `stop` (`worker_path: stop`; `running_boxes`
  in the plan is the path a hand-over refused just before it acts falls back to). `ow-host restart --worker`
  hands over when the running worker advertises `worker_handover_v1`.
- **Journal**: the `hibernate` step becomes `handover`; it records each running
  box's VMM PID, and after verify a step `adoption` records per box `kept`
  (adopted, same PID) or what happened instead. Recovering at `adoption`
  finishes the upgrade. Aborting after `handover` began restarts the worker unit
  (its stop hibernates what is still attached, its start adopts the rest). Idle
  hibernation pauses while the worker is quiesced.
- **Per box fallback**: a VMM that is gone leaves its box stopped with its disk
  (as a worker crash did). One this worker cannot keep (another binary or guest
  kernel, a VM never started, or a box that is not stopped) is ended: the box
  keeps its disk and loses its RAM state; the upgrader prevents the first two
  by comparing the staged Firecracker and kernel bytes before it hands over. An
  adopted VMM whose API or guest does not answer, or whose console or log could
  not be taken (`pidfd_getfd` refused), stays running, reported `unresponsive`;
  after verify the upgrader has the worker hibernate it (with its placement
  authority, going on without a guest sync only there) and its resume step wakes
  it. A box that was running and came back stopped boots again, cold. The
  journal error names every box not kept running; none of these rolls the
  upgrade back. An adopted box keeps its in-guest setup (network, tools) until
  its next cold boot or wake. Ephemeral boxes are handed over and adopted like
  the others, but the upgrader neither records nor resumes them (as on the
  hibernate path, where they are stopped): one adopted `unresponsive` is
  stopped, not hibernated, and one that came back stopped stays stopped.
- **Abort and rollback**: before the switch, the old worker starts again and
  adopts its own VMMs. After it, rollback stops the new worker, which
  hibernates every box (the unit's `ExecStop`), and the old one wakes them.
- **Network**: the nftables policy is replaced in one transaction and
  forwarding stays off until the uplink is back. `slirp4netns` restarts with
  the worker, so guest connections to the Internet open during the swap are
  reset; new ones work as soon as the worker is up. SSH and terminal streams
  reconnect, as on a class 1 restart. Published HTTP ports (`ow publish`)
  stop with the old worker, as on any worker stop.
- **First upgrade**: 0.9.59 itself installs through hibernation (the worker
  it replaces cannot detach, and the worker unit gains `KillMode=process`). From
  0.9.59 on, `ow-host restart --worker` and worker-only upgrades hand over.

A worker that crashes no longer takes its VMMs with it: its supervisor exits with
an error, systemd restarts the unit, and the new worker adopts them the same way.
A reboot in the seconds between detach and the new worker's start loses those
boxes' RAM state; the boot's journal recovery boots them again, cold. The host
check below records timings.

### Sandbox EC2 check: hand-over (2026-11-03)

Observed on one throwaway m8i-flex.xlarge host in the sandbox account (Ubuntu
24.04 AMI `ami-0d6dd9e8265d48906`, host kernel 7.0.0-1014-aws, systemd 255,
4 vCPUs, 16 GiB, Btrfs data volume), destroyed afterwards. The host installed
the hand-over branch, then built on 0.9.50 and numbered 0.9.51; 0.9.52 (worker
only) and 0.9.53 (declaring `vmm`) were test releases built from it (version
bumps and manifest entries). Those numbers name the test builds in the table,
not the published releases of the same numbers. Two
Alpine boxes ran throughout: `big` (4096 MiB, 2 GiB of it filled with random
data in a tmpfs) and `small` (1024 MiB). Each ran a counter (`echo $i > /tmp/t`
every second) and a heartbeat that appends the guest's `/proc/uptime` every
0.1 s; the counter's PID, the VMM PID (`vmm.json`) and the largest heartbeat
gap were read before and after each step. A probe on the host ran
`ow exec big -- true` once a second. Each measurement is a single run (n = 1),
so no p50/p95.

| Step | Path | Result |
| --- | --- | --- |
| `ow-host restart --worker` on 0.9.51 | hand-over | both kept: same VMM PIDs, same counter PIDs, guest uptime continuous. Command 1.3 s end to end (journal: start 1.1 s); one `ow exec` probe failed (unreachable at most 1 s) |
| `ow-host upgrade` 0.9.51 → 0.9.52 (worker only) | hand-over (`same VMM and guest kernel`) | both kept with the same VMM and counter PIDs; largest guest heartbeat gap 0.14 s (`big`) and 0.12 s (`small`): the guests never paused. 1 of 530 probes failed (at most 1 s). Journal: handover 0.0 s, start 1.1 s, verify 6.1 s; the command's 556 s were the release build on the host |
| Forced adoption failure: `small`'s VMM stopped (`SIGSTOP`) across `ow-host restart --worker`, continued 25 s later | hand-over, then fallback | `big` adopted. `small`: its API timed out after 10 s, the new worker hibernated it, and the upgrade's resume step woke it on a new VMM with the same counter PID (RAM state kept). Journal start 52.9 s, resume 0.5 s; error names `small` |
| `ow-host upgrade` 0.9.52 → 0.9.53 (declares `vmm`) | hibernate (`vmm change`) | both hibernated and woken on new VMMs with the same counter PIDs; journal hibernate 81.7 s, resume 19.6 s; the longest probe outage 81.9 s |

So a worker-only change costs about a second of control-plane outage and no
guest pause, against 80-100 s of hibernation and wake for 5 GiB of boxes here
(the ADR cites about 31 minutes for a 16 GiB and a 4 GiB box on another host). Systemd logged both VMMs as "left-over process ... in
control group while starting unit. Ignoring.", as expected with
`KillMode=process`. The VMM held its console (`/dev/ptmx`) as fd 3 and its log
pipe's read end as fd 4 throughout.

In the forced-failure run the new worker hibernated the unresponsive box itself
while starting (the 52.9 s start). After review that hibernation moved to the
upgrader, after verify, so a large unresponsive box cannot hold the new worker's
start past the upgrader's quiesce limit and turn into a rollback; that version
was checked with the upgrader's state-machine tests, not on a host.

Found and fixed during the run: the upgrader read the adoption report from a
busy worker's last answer, which predated the adoption, and reported every box
as not kept; it now waits for a fresh answer.

## Order for 0.9.48 and later

Runner budgets may exceed 65,536 MiB, 64 vCPUs or 8 slots from 0.9.48. A controller older than
0.9.48 refuses such a heartbeat, so upgrade the controller before any runner whose host is that
large (the fleet order already does). Snapshot verification is remembered across worker restarts
(`verified.json`), so a restarted worker answers without re-hashing its snapshots.

The release after 0.9.48 (no version bump in the change itself) adds three things. The upgrader
pauses the backup and recovery timers while its journal is open (above). The worker answers
`status` while busy. Box disks are attached with discard and the guest mounts its root with
`discard`, so blocks a guest frees become holes again and snapshot hashing, copies and uploads skip
them; each cold boot also runs one `fstrim /` in the background to return space freed before. A box
gets discard at its next cold boot; a box resumed from a hibernation keeps the device it was
captured with. The timer pause applies from the upgrade **after** the one that installs this
release, because an upgrade runs the upgrader of the release it starts from. For the upgrade that
installs it, stop `ow-recovery.timer` by hand first (`systemctl stop ow-recovery.timer`) and start
it again after. The runtime string, guest kernel, worker interface and catalog schema are unchanged,
so no capture is cold.
