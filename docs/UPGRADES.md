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
| 2 runner | `open-workspaces-worker.service` | hibernated, worker swapped, resumed; processes continue when every capture still matches | dropped |
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
    src/target -> /srv/ow/build/target     shared, incremental cargo builds
    assets/                   this release's own asset set (a reflink copy, then updated)
    bin/ow                    copied from the build
    STAGED                    version, commit, ref, sha256 of bin/ow, build seconds
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

Staging uses `scripts/setup-local.sh --controller-only`: it reuses the pinned
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
fetch → plan → stage → gate → [catalog copy] → [drain → hibernate] → stop → [sync images]
      → switch → start → verify → [resume] → [undrain] → done
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
  written last. A complete release is reused.
- **gate**: again, with the staged kernel's hash and the boxes as they are now.
  With `--accept-cold` and backups configured, `ow-backup` runs first.
- **hibernate**: the worker is quiesced first (it then refuses starts and wakes),
  so the running boxes read next are exactly the ones the old worker's own
  `down --hibernate` captures; they are recorded in the journal before it acts.
  The capture gate runs a last time here, on every capture now present, so a
  box started after planning cannot lose its RAM state without `--accept-cold`;
  a refusal aborts (quiesce lifted, drain removed).
- **verify** (each unit against its own pin): the unit is active and its
  `/proc/PID/exe` is its pin's `bin/ow`; the worker answers with its pin's
  version; the runner is online with the agent's and worker's versions; the
  controller listens on the tailnet address; the dashboard answers. A class 2
  upgrade with no box to resume runs a canary box.
- **abort** (any failure before the switch): start the old worker if it is down
  (else lift its quiesce), resume every recorded box, start stopped units,
  undrain. **rollback** (switch, start or verify): pins back, units restarted and
  verified on the old releases, then as abort. A box that does not resume stays
  hibernated (it wakes on its next connection) and is reported.
- **recover**: every upgrade and every boot (`open-workspaces-recover.service`)
  reads the journal first and aborts, rolls back or finishes.

Old releases are pruned: every pinned release plus the two newest others stay.

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
| Planned replacement v0.4.1 (layout v1) → 0.5.0 | `pulumi up` 80 s; stage 0 with build 4-5 min (cargo target moved from the old layout, incremental). Same data volume, tailnet device and address (`100.76.57.76`), dashboard back. The box hibernated on the old instance resumed warm: counter PID 293 kept counting. |
| `pulumi preview` after a `ref` change | only the config parameter updates; the instance is unchanged |
| Class 1 (controller, agent, dashboard) | worker PID and Firecracker PID unchanged, counter PID 293 kept counting; control plane and dashboard on the new release. Journal: stop 1.0 s, switch 0.1 s, start 0.1 s, verify 3.1 s. Whole command 266 s, of which staging (build on the host) about 250 s. |
| Dashboard-only release | only the dashboard restarted; the agent stayed on the previous release (same PID) and verify passed against the per-unit pins |
| Class 2 (worker) | drain, hibernate 2.1 s, switch/start/verify 3.2 s, resume 0.4 s; new worker and Firecracker PIDs, counter PID 293 continued; the agent kept its PID (it waits for the worker) |
| Injected rollback (worker exits at start) | verify failed after its 180 s deadline, pins went back, the old worker started, the box resumed with its process |
| Upgrader killed (`SIGKILL`) at verify | `ow-host recover` rolled back (6.7 s) and resumed the box with its process |
| Step back to 0.5.0 (`ow-host upgrade v0.5.0`) | class 2, all three units back to 0.5.0, box process kept |
| `ow-aws restart` / `restart --worker` | 5 s / 9 s end to end, box process kept |
| Job in flight during `systemctl restart open-workspaces` | an `exec` of `sleep 8` sent through the controller finished and replied (8.1 s); the restart waited for it (6.2 s) |
| Tailnet | `tailscale ping ow-wrk13` answered from `100.76.57.76` after every step; the dashboard answered 401 over the tailnet to a login it does not list |
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
Host `ow-wrk14` (m8i-flex.xlarge, 4 vCPUs, 16 GiB) and a Runner `ow-wrk14r`
(m8i-flex.2xlarge, 8 vCPUs, 32 GiB, Intel Xeon 6975P-C), both on AMI
`ami-02a9cdd4e98bc3c78` (host kernel 7.0.0-1014-aws), `developerImages:
["ubuntu"]`, `idleMinutes: 0`, an `ArtifactStore` for backups. The releases were
this branch plus test tags (version bumps and manifest entries). A box `keep1`
on the Host's own runner ran a counter (`echo $n > /tmp/t` every second) for the
whole run; its PID and guest uptime were read after every step. Each
measurement is a single run (n = 1), so no p50/p95.

| Step | Result |
| --- | --- |
| `pulumi preview` adding the Runner to a running Host | 15 creates (the Runner's resources and the controller role's `ow-wrk14r-invite` policy); nothing of the Host changes |
| `ow-aws --host ow-wrk14 runner-add ow-wrk14r` | 35 s: join minted on the controller into the runner's secret, redeemed, secret overwritten with `{"used": …}`, runner online (23664 MiB, 8 slots, 8 vCPUs) |
| Placement | four automatic creates alternated between the two runners; `--node` put a box on each |
| Runner 2 box lifecycle | create, exec, terminal (`ow shell` through a pty), SSH as `dev` (Ubuntu box, `ow ssh-config` with a pinned host key), snapshot, fork (the fork stayed on runner 2 with the source's files), hibernate (4.8 s) and wake on exec (same PID, counter carried on) |
| Golden image | the same recipe built on each runner (`ow-aws --host … image`); both advertised it without a restart and booted a box from it |
| Runner 2 control-plane restart | 10 s; runner 2 box process kept |
| Runner 2 worker restart | 103 s, six boxes hibernated and resumed with their processes |
| Runner 2 in place: v0.6.0 (claims protocol 2) with the controller on 0.5.0 | refused at plan: "the controller (0.5.0) accepts protocol 1..1 … upgrade the controller first" |
| Runner 2 in place: class 1, then class 2 | 268 s and 365 s (both mostly the build); the runner ran agent 0.5.1 and worker 0.5.2 against a 0.5.0 controller |
| Controller class 1, then the runner | controller 264 s, runner 2's Firecracker PIDs and box process unchanged; then the runner's agent followed (`Stopped: no jobs in flight.`, then `agent.json` online with 0.5.1) |
| Runner 2 offline (`ow-aws stop`) | 110 s; its boxes listed `offline`, an automatic create went to runner 1, `--node ow-wrk14r` and exec on its boxes were refused; `ow-aws start` (96 s) resumed all six with their processes |
| Runner 2 replacement (`pulumi up --replace` its instance) | 73 s; only its instance and volume attachment replaced, 32 resources unchanged; it came back as node `ow-wrk14r` (credential on the data volume), `ci-post-up` 44 s, a box resumed warm on exec |
| Admission | filling with 2048 MiB boxes: runner 1 stopped at 4 of 4 vCPUs, runner 2 at 8 of 8 slots; the next create and a 256 MiB one were refused |
| Backups | `ow-backup` on each: the catalog and runner 1's captures under `PREFIX/`, runner 2's captures under `PREFIX/runners/ow-wrk14r/snapshots` only; `ow-aws --host ow-wrk14r backups` lists them with owners (run on the controller) |
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
