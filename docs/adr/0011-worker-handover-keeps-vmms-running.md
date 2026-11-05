# ADR 0011: a worker restart hands running VMMs to the next worker

Status: implemented in 0.9.59; sandbox EC2 check (hand-over, fallback and a VMM
change) in [UPGRADES.md](../UPGRADES.md#sandbox-ec2-check-hand-over-2026-11-03). Amends [ADR 0005](0005-change-classes-and-engine-boundary.md),
which left worker re-adoption of running VMMs out.
Pattern: ADR 0005's journaled upgrade state machine (abort before the switch,
rollback after it) and its release-impact manifest; the worker's delegated cgroup
and network namespace from [networking](../NETWORKING.md).

## Context

A class 2 upgrade (new worker binary) hibernates every running box, restarts the
worker and wakes the boxes. Hibernation writes and hashes each box's full RAM and
disk one box at a time; on a host with a 16 GiB and a 4 GiB box this took about
31 minutes, while the binary switch itself takes about 6 s. Most worker releases
change neither Firecracker nor the guest kernel, so the running VMMs could keep
running if they did not die with their worker. Today they die because of five ties:

| Tie | Today | Why the VMM dies or breaks |
| --- | --- | --- |
| Parent-death signal | `PR_SET_PDEATHSIG(SIGTERM)` in the VMM's pre-exec | worker exit sends SIGTERM |
| Unit cgroup | VMMs live in the worker unit's delegated `guests` cgroup, `KillMode=control-group` | `systemctl stop` kills the whole cgroup |
| Serial PTY master | only the worker holds it; the guest management channel is the serial console | master closed: the guest's console hangs up and Firecracker stops reading stdin |
| Diagnostics pipe | VMM stderr is a pipe the worker drains into `vmm.log` | read end closed: the next log write gets `SIGPIPE`/`EPIPE` |
| Namespaces and uplink | `unshare --user --map-root-user --net` per worker start; TAPs, nftables policy and `slirp4netns` live there | the next worker starts in a fresh namespace and cannot see the TAPs |

## Decision

### 1. VMMs are spawned to outlive their worker

- No parent-death signal for VMMs (the supervisor keeps it for the worker and
  `slirp4netns`).
- The worker unit gets `KillMode=process`: a stop signals the supervisor only, and
  VMMs stay in the unit's delegated `guests` cgroup with their CPU weight, memory
  protection and OOM score unchanged. `ExecStop` (hibernate everything) is
  unchanged, so a reboot, instance stop or a plain `systemctl stop` still
  hibernates; anything left after it no longer dies with the unit, and the next
  worker adopts it.
- `guests` is a sibling of the worker's own subgroup (`DelegateSubgroup=control`),
  not its child: `control` never enables controllers for children, so the
  restarted unit can always attach its new supervisor there while VMMs still run
  (cgroup v2 refuses processes in a cgroup that enables controllers for children).
- The VMM itself holds the other end of its own ties: the PTY master and the
  diagnostics pipe's read end are inherited into the Firecracker process as fds 3
  and 4 (not close-on-exec). While the VMM lives the console never hangs up and the
  pipe never loses its reader. Output written while no worker reads waits in the
  kernel buffers (a few seconds of a quiet console).
- As soon as the VMM process exists the worker writes `machines/ID/vmm.json`: the
  boot ID, VMM PID and its start time (`/proc/PID/stat` field 22; with the boot ID
  this defeats PID reuse), the Firecracker binary's and guest kernel's SHA-256,
  network index, TAP, address, the fd numbers, and a record format version. It is
  removed when the VMM is reaped.

### 2. Namespaces persist through the VMMs

The user and network namespaces are kept alive by the VMMs inside them. A new
supervisor that finds a live recorded VMM enters that VMM's namespaces
(`nsenter --target PID --user --net --preserve-credentials`: the unprivileged
worker user is root inside through the first worker's mapping) instead of
creating fresh ones; with no live VMM it unshares fresh ones as today. Inside, the
worker replaces its nftables policy in one `nft -f` transaction, so the forward
chain is never absent; forwarding stays off until the uplink and host addresses
are back, and each adopted box's set elements are added as it is adopted (a guest
not yet adopted is dropped, fail-closed). `slirp4netns` is restarted against the new worker PID and recreates
`tap-egress`: guest TCP connections to the Internet that were open during the
switch are reset; new ones work as soon as the uplink is up. Inbound SSH and
terminal streams go through the worker and reconnect, as on a class 1 restart.

### 3. The next worker adopts each VMM

On start the worker reads every `vmm.json` and adopts a VMM only if all hold:

1. the boot, PID and start time match the record;
2. the hash of `/proc/PID/exe` is the hash of the Firecracker binary this worker
   runs, and the recorded guest kernel hash is this worker's;
3. `pidfd_open` + `pidfd_getfd` retrieve fds 3 and 4 (the PTY master and the log
   pipe); a fresh drain thread resumes `vmm.log`;
4. the VMM API socket answers and reports the VM running (a paused VM, left by a
   capture interrupted mid-way, is resumed; one not started, left by a worker that
   died during a cold start, is ended);
5. a guest command (`true` with a fresh marker) answers on the console within 10 s.

An adopted box is `running` again with the same VMM PID; its network policy
elements and per-box metadata endpoint are re-attached and its idle timer reset
(its SSH policy lives in the guest and on disk, unchanged). The dirty-page base is
unknown, so its next capture is a full one.

An adopted VMM is not this process's child: liveness and kill go through its
pidfd (poll for exit, `pidfd_send_signal`), never a PID read from disk.

### 4. A failed adoption falls back to hibernation or a cold boot, never to a lost disk

No VMM is left running that no worker manages, except where noted below. Per box:

- check 1 fails: the recorded process is gone (a PID that now names another
  process is never signalled). Its TAP and record are cleaned; the box keeps its
  disk and the state recovery gave it (`stopped`, as after a worker crash today);
- check 2 fails on the recorded, live VMM (another binary or guest kernel, its
  binary unreadable): this worker cannot manage it, so it ends it through its
  pidfd and cleans up as above; the box keeps its disk, its RAM state is lost and
  reported. So is a VM its API reports not started: no guest ever ran in it;
- the box is not `stopped` (hibernated, exporting or moved: the worker died after
  the capture and before dropping the VMM): the VMM is ended the same way, so no
  second copy of the box runs;
- check 3 (a security module may refuse `pidfd_getfd` where the start-up probe on
  the worker's own process succeeded) or checks 4-5 (or re-attaching its
  network) fail on an adopted VMM: it stays adopted and running, reported
  `unresponsive`; without its console only its API reaches it, which suffices to
  hibernate it. The worker does not hibernate it
  while starting (that would hold the new worker, and the upgrader's quiesce, for
  as long as its RAM takes to write); after verify the upgrader asks the worker
  (`hibernate-unresponsive`) to hibernate it with its placement authority, going on
  when the guest `sync` fails (a dead console cannot answer it; RAM and disk are
  saved paused together), and its resume step wakes it. Any other capture still
  fails on a failed `sync`, since a later cold boot would lose unflushed writes. If that hibernation fails too, the box
  keeps running under the new worker and is reported.

After a hand-over the upgrader's resume step also boots again, cold, every box
that was running and came back `stopped` (its VMM gone or ended): its RAM state
is lost, its disk and its uptime as a box are not.

Not covered: an unreadable record, a `pidfd_open` failing for lack of fds or
memory, or a kill that fails leave that VMM running with its record and TAP (the
box cannot start a second VMM on its disk, and the next worker tries again); a
worker that dies between spawning a VMM and writing its record leaves that VMM
unrecorded. An adopted box keeps the in-guest setup it was started with (network,
tools bundle): a worker release that changes that setup reaches it at its next
cold boot or wake.

Checks 2-3 are kept out of an upgrade's way before it starts (§5): the upgrader
compares the Firecracker and guest kernel bytes of both releases, and hands over
only when the running worker advertises `worker_handover_v1`, which it does only
where `pidfd_getfd` works on its own process (Linux 5.6+, Yama `ptrace_scope` below
3); a refusal on the VMM itself falls back as above, keeping its RAM.

### 5. The upgrader decides: hand over or hibernate

`ow-host upgrade` and `ow-host restart --worker` hand over only when every one of
these holds, and otherwise hibernate exactly as today:

- no release the worker crosses lists `vmm` or `guest-kernel` in `changes`, and the
  source and target entries' `runtime.vmm` and `runtime.guest_kernel_sha256` are
  equal;
- the target entry's `runtime.handover` window (`{"speaks": N, "accepts": [lo,
  hi]}`) accepts the source entry's `speaks`. A release without the field cannot
  hand over: the upgrade that installs this feature still hibernates;
- the worker's unit files render unchanged and no worker config change is pending
  (either may move the cgroup or namespaces the VMMs live in);
- the controller setting `upgrade.worker_handover` is on (the default) and
  `--hibernate` was not passed.

The plan prints which path it takes and why. Idle hibernation stops while the
worker is quiesced, so nothing new holds the worker in front of the detach. Just before the hand-over the
upgrader checks once more on the host itself: the running worker advertises
`worker_handover_v1`, and the staged release's Firecracker binary and guest
kernels hash the same as the running one's (the manifest's runtime label alone
could hide a different binary). Either failing takes the hibernate path, recorded
as `handover_refused` in the journal. The journal step `hibernate` becomes
`handover` on that path: the old worker is quiesced and drained as today, the
running boxes are recorded, the worker answers a new `detach` request (it
forgets its VMMs without killing them and exits its loop), the unit stops, the
pin switches, the new worker starts and adopts. **verify** is unchanged; after
it the journal's `adoption` says per box whether it was kept (adopted with its
recorded VMM PID) or what happened instead, and the journal's error names every
box not kept. The report is its own journal step (`adoption`), so an upgrader
interrupted there finishes the upgrade on recover instead of rolling it back. A box
not kept never rolls the upgrade back: a rollback would hibernate every other box.
**abort** after the hand-over began restarts the worker unit: its stop hibernates
whatever is still attached (a detach still queued) and its start adopts whatever
was handed over. **rollback** after the switch takes today's path: the
new worker hibernates the boxes on stop and the old one wakes them.

### 6. Switch and advertisement

`upgrade.worker_handover` is a controller setting (catalog `settings` table,
`ow config`), delivered to runners on every heartbeat acknowledgement and stored
by the node agent for the upgrader to read; no per-runner or IaC switch. From
0.9.60 the node agent stores `upgrade.running_boxes` in the same file: when no
hand-over is possible, `stop` has the upgrader stop the running boxes instead of
hibernating them (docs/UPGRADES.md). A worker
that can adopt on its host reports `worker_handover_v1` in its status, the agent
advertises it with every heartbeat, and `ow nodes` shows it per node.

## Consequences

- Box downtime on a worker-only upgrade becomes the worker swap (seconds) instead
  of a RAM write and hash per box. The guest's processes, clock and memory are
  untouched.
- A worker that crashes no longer kills the VMMs: the supervisor exits with an
  error, the stop hook finds no live worker behind a stale control socket (it used
  to retry against it forever), and `Restart=on-failure` starts a worker that
  adopts them.
- Published HTTP ports (`ow publish` gateways) stop with the old worker, as on any
  worker stop; the boxes keep running.
- A reboot or instance stop in the seconds between the old worker's detach and the
  new worker's start finds no worker to hibernate the boxes: their RAM state is
  lost, and the boot's recovery of the open journal boots them again, cold.
- Consoles are close-on-exec, so no VMM inherits another box's console (before,
  every later VMM held the consoles of the boxes already running).
- Firecracker, guest kernel and host-layout changes keep paying the full
  hibernate cost.
- Not decided here: keeping `slirp4netns` across the switch so outbound
  connections survive; keeping the previous Firecracker and kernel side by side
  so a runtime bump can also hand over; carrying the dirty-page base across.
