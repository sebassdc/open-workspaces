# ADR 0011: a worker restart hands running VMMs to the next worker

Status: implemented in 0.9.51 (sandbox check in [UPGRADES.md](../UPGRADES.md)). Amends [ADR 0005](0005-change-classes-and-engine-boundary.md),
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
- The VMM itself holds the other end of its own ties: the PTY master and the
  diagnostics pipe's read end are inherited into the Firecracker process as fds 3
  and 4 (not close-on-exec). While the VMM lives the console never hangs up and the
  pipe never loses its reader. Output written while no worker reads waits in the
  kernel buffers (a few seconds of a quiet console).
- At spawn the worker writes `machines/ID/vmm.json`: VMM PID, its start time
  (`/proc/PID/stat` field 22, which defeats PID reuse), the binary's path and
  SHA-256, the guest kernel's SHA-256, network index, TAP, address, the fd
  numbers, and a record format version. It is removed when the VMM is reaped.

### 2. Namespaces persist through the VMMs

The user and network namespaces are kept alive by the VMMs inside them. A new
supervisor that finds a live recorded VMM enters that VMM's namespaces
(`nsenter --target PID --user --net`) instead of creating fresh ones; with no live
VMM it unshares fresh ones as today. Inside, the worker re-applies its nftables
policy as one atomic `nft -f` transaction (delete and recreate the table, re-add
the adopted boxes' set elements), so there is no instant without the forward
policy. `slirp4netns` is restarted against the new worker PID and recreates
`tap-egress`: guest TCP connections to the Internet that were open during the
switch are reset; new ones work as soon as the uplink is up. Inbound SSH and
terminal streams go through the worker and reconnect, as on a class 1 restart.

### 3. The next worker adopts each VMM

On start the worker reads every `vmm.json` and adopts a VMM only if all hold:

1. the PID is alive and its start time matches the record;
2. `/proc/PID/exe` is the recorded Firecracker binary and its hash is the one
   this worker would run, and the recorded guest kernel hash is this worker's;
3. `pidfd_open` + `pidfd_getfd` retrieve fds 3 and 4 (the PTY master and the log
   pipe); a fresh drain thread resumes `vmm.log`;
4. the VMM API socket answers and reports the VM running (a paused VM, left by a
   capture interrupted mid-way, is resumed);
5. a guest command (`true` with a fresh marker) answers on the console within 10 s.

An adopted box is `running` again with the same VMM PID; its network policy
elements and per-box metadata endpoint are re-attached and its idle timer reset
(its SSH policy lives in the guest and on disk, unchanged). The dirty-page base is
unknown, so its next capture is a full one.

An adopted VMM is not this process's child: liveness and kill go through its
pidfd (poll for exit, `pidfd_send_signal`), never a PID read from disk.

### 4. A failed adoption falls back to hibernation, never to a lost box

Per box, in order:

- checks 1-3 fail and the VMM is dead: its TAP and record are cleaned and the box
  is `stopped` with its disk (what a worker crash does today);
- check 1 fails (the PID now names another process): it is never signalled; the
  box is `stopped` with its disk;
- check 2 fails on the recorded, live VMM (another binary or guest kernel): it is
  never signalled and keeps running; the box is `failed` and reported. Its TAP
  stays, so no second VMM can start on its disk;
- checks 4-5 (or re-attaching its network) fail on an adopted VMM: the worker
  hibernates it with its normal hibernation; if that fails too, the VMM stays
  adopted and running, and the box is reported.

### 5. The upgrader decides: hand over or hibernate

`ow-host upgrade` and `ow-host restart --worker` hand over only when every one of
these holds, and otherwise hibernate exactly as today:

- no release the worker crosses lists `vmm` or `guest-kernel` in `changes`, and the
  source and target entries' `runtime.vmm` and `runtime.guest_kernel_sha256` are
  equal;
- the target entry's `runtime.handover` window (`{"speaks": N, "accepts": [lo,
  hi]}`) accepts the source entry's `speaks`. A release without the field cannot
  hand over: the upgrade that installs this feature still hibernates;
- the worker unit file renders unchanged (a unit change may move the cgroup);
- the controller setting `upgrade.worker_handover` is on (the default) and
  `--hibernate` was not passed.

The plan prints which path it takes and why. The journal step `hibernate` becomes
`handover` on that path: the old worker is quiesced and drained as today, the
running boxes are recorded, the worker answers a new `detach` request (it
forgets its VMMs without killing them and exits its loop), the unit stops, the
pin switches, the new worker starts and adopts. **verify** additionally requires
every recorded box to be `running` with its recorded VMM PID. **abort** before the
switch: the old worker restarts and adopts its own VMMs. **rollback** after the
switch takes today's path: the new worker hibernates the boxes on stop and the old
one wakes them. A box that is neither adopted nor hibernated is reported in the
journal's error, as a box that does not resume is today.

### 6. Switch and advertisement

`upgrade.worker_handover` is a controller setting (catalog `settings` table,
`ow config`), delivered to runners on every heartbeat acknowledgement and stored
by the node agent for the upgrader to read; no per-runner or IaC switch. A worker
that can adopt reports `worker_handover_v1` in its status, the agent advertises
it, and `ow nodes` shows it per node.

## Consequences

- Box downtime on a worker-only upgrade becomes the worker swap (seconds) instead
  of a RAM write and hash per box. The guest's processes, clock and memory are
  untouched.
- A worker crash no longer kills the VMMs; the restarted worker adopts them.
- Firecracker, guest kernel and host-layout changes keep paying the full
  hibernate cost.
- Not decided here: keeping `slirp4netns` across the switch so outbound
  connections survive; keeping the previous Firecracker and kernel side by side
  so a runtime bump can also hand over; carrying the dirty-page base across.
