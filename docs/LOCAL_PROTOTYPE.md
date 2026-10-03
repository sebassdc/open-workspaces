# Local microVM prototype

The prototype runs real Firecracker/KVM Linux guests on this machine. Its Rust
CLI and worker support commands, files, persistent disks, RAM+disk snapshots,
independent forks, hibernation, and local HTTP publishing. This is a single
operator development prototype, not a production or public sandbox service.

## Try it

From the repository root:

```bash
./ow up
./ow create playground
./ow exec playground -- 'uname -r; echo hello; echo persistent > /persist/message'
./ow shell playground
```

The shell preserves its working directory and variables between submitted lines.
Type `exit` or Ctrl-D to leave. This now opens a real guest PTY, streams output,
supports interactive applications, forwards Ctrl-C/job control and window size,
and returns the guest shell's exit status. It requires a local interactive terminal;
Remote CLI login and HTTPS/WSS terminals are implemented; editor SSH integration remains pending. See [remote CLI and egress](NETWORKING.md). See [terminal
transport](TERMINALS.md). `exec` runs
each request in its own shell and returns the actual guest exit code; stdout and
stderr are combined.

Start the supplied HTTP counter workload and get a browser URL:

```bash
./ow exec playground -- '( /http-fixture >/run/http.log 2>&1 </dev/null & )'
./ow publish playground
```

Open the printed `http://127.0.0.1:...` URL. Every request increments a counter
held only in the server process's memory; the response includes `X-Counter`.
The gateway binds loopback on an OS-selected available port. No public ingress,
host firewall, global forwarding, or existing listener is changed.

## Capture and branch

```bash
./ow snapshot playground prepared
./ow fork playground attempt-a --snapshot prepared
./ow fork playground attempt-b --snapshot prepared
./ow publish attempt-a
./ow publish attempt-b
./ow exec playground -- 'echo parent > /persist/branch'
./ow exec attempt-a -- 'echo child > /persist/branch'
./ow exec playground -- 'cat /persist/branch'
./ow exec attempt-a -- 'cat /persist/branch'
```

The restored HTTP servers already run; starting another server is unnecessary.
Each child resumes the counter captured in `prepared`. Writes then diverge.
Without `--snapshot`, `fork` captures the parent's current full state first.

Names identify immutable snapshots: repeating `snapshot` with the same name
returns the existing capture, rather than overwriting it. Repeated creation or
fork requests with the same workspace ID return that workspace when their
parameters agree. A fork child ID cannot silently switch its source parent.

Capture synchronizes the guest, pauses the VM, saves RAM and VM state, clones the
matching disk before resuming, hashes the artifacts, and atomically publishes the
manifest. It does not guarantee application-consistent transactions with external
databases or services. Process state can duplicate external side effects.

Snapshots are made read-only. SHA-256 verification is cached inside a worker
after capture/verification, with inode, size, modification/change timestamps and
mode checks for all components. Changed metadata forces re-verification; a new
worker verifies artifacts again. This assumes trusted local host storage and
operator ownership; it is not an authenticity protocol for untrusted imports.

Before exposing a fork's TAP interface, the worker assigns an independent IP,
MAC, hostname and machine ID, removes any inherited SSH host-key files, and
relies on the guest's VMGenID support for kernel entropy reseeding. The supplied
template contains no account or control-plane credentials. Application-specific
keys, cached identities, userspace random generators and external sessions need
further restore hooks before handling arbitrary credentialed workloads.

## Sleep, restart and rewind

```bash
./ow hibernate attempt-a
./ow stats
./ow start attempt-a
./ow stop playground
./ow start playground
./ow exec playground -- 'cat /persist/message'
./ow restore playground prepared
```

Hibernation retains a paired checkpoint and exits the VMM; `start` resumes that
checkpoint. Host page cache can still retain snapshot data and is reclaimable.
Restore requests advance the guest's KVM clock by the elapsed wall time.
Cold stop/start preserves disk contents and starts fresh processes. `restore`
replaces that workspace's current state with its named snapshot; subsequent guest
changes are intentionally rewound. Backups should be made before rewinding work
that must be retained. HTTP clients reconnect across these transitions; a sleeping
workspace returns 502 through its gateway until explicitly started.

## Files and limits

```bash
./ow put playground ./example.txt /persist/example.txt
./ow get playground /persist/example.txt ./downloaded.txt
./ow create larger --memory 512
./ow list
./ow inspect playground
./ow snapshots
./ow stats
```

Downloads refuse to overwrite an existing local file. Upload destinations are
inside the guest and their parent directory must exist. This prototype limits
transfers to 256 KiB and commands to 2,800 bytes after quoting. Commands time out
after 30 seconds; a stuck serial channel may require stop/start. Guest output is
bounded and serial logs are capped at 4 MiB per workspace.

Each guest has one vCPU, a 128 MiB disk, and 256/512/1,024 MiB RAM. The minimal
Alpine image is suitable for the runtime demo; larger developer toolchains need
larger images. Limits are eight running workspaces, 4 GiB of reserved guest RAM,
32 workspace records and 24 snapshot directories. Excess memory requests are
rejected before creating a disk or VM. These conservative sandbox limits are not
measured maximum host capacity. Filtered rootless IPv4 Internet egress is now implemented; see [network policy](NETWORKING.md).

`stats` reports reservations, RSS, proportional memory (PSS), and private dirty
memory. PSS helps avoid double-counting shared pages but excludes extra host page
cache and kernel overhead. Forks initially share clean memory pages and disk
extents; subsequent writes consume private storage and RAM. Hibernation reduces
running process memory, while checkpoints still consume storage. Snapshot
retention and garbage collection are manual in this version.

An eight-VM experiment observed combined VMM PSS grow from about 253 MiB to
638 MiB when each guest wrote to its 64 MiB application heap. Both conditions
reserved 2 GiB of guest RAM. This is an example of CoW savings disappearing with
writes, not a density guarantee. The [experiment record](experiments/runtime-spike.md)
includes exact conditions and measurement limits.

## Host boundaries and recovery

Workspace data lives under `data/prototype/`, protected by owner-only permissions.
The worker creates a dedicated user/network namespace and separate TAPs; no guest
can change the host's forwarding policy. Control requests use a Unix socket with
filesystem permissions and peer UID checks. Guests receive no management
credentials, host directories, Docker sockets or hypervisor sockets.

The API serializes control operations. Long capture/verification operations can
delay new gateway connections; existing tunnel connections use independent
threads. The official Firecracker binary supplies its default syscall filtering.
The worker does not yet use the jailer, cgroup CPU/I/O limits, public API auth,
encrypted backups, cross-worker recovery or production hardening. Use it for
controlled local development workloads.

The registry and snapshots persist across worker restarts. An abrupt worker exit
terminates its VMM children through parent-death signals; restarting reconciles
previous running records to stopped instead of creating duplicates. Hibernated
records retain their resume checkpoint. Disks survive; unsynchronized guest
writes at a crash are not guaranteed. Corrupt/incompatible snapshots are rejected
before child creation. Incomplete captures remain unpublished for inspection.

Compatibility checks currently require matching CPU model/flags, host kernel,
guest kernel hash and runtime version. Portability beyond this tested profile is
not established; an OS upgrade can require a cold restart instead of RAM restore.

Stop the prototype's worker and its gateways while retaining disks and captures:

```bash
./ow down
```

`--data-dir` or `OW_DATA_DIR` selects another dedicated data directory. The CLI
refuses to adopt a nonempty directory without its marker. This keeps accidental
configuration from changing an unrelated directory. Experiment data and snapshots
are ignored by Git; do not publish them.

## Build and verify

Already built on this machine. To reproduce the isolated setup:

```bash
bash scripts/setup-local.sh
python3 experiments/runtime-spike/prototype-e2e.py
python3 experiments/runtime-spike/measure.py --samples 10
```

Setup installs Rust 1.97.0 under project data, verifies pinned upstream downloads,
builds the guest fixture and locked Rust workspace, and checks KVM access. It
does not install global packages or change the desktop. A changed upstream
installer hash fails rather than executing an unverified installer.

The real-VM regression covers exec exit codes, 48 KiB binary transfer, repeated
operations, HTTP memory state, independent disk writes, guest network separation,
hibernation, cold restart, rewind, artifact corruption, worker-loss recovery,
resource exhaustion and live memory statistics. Results and measurement methods
are in [runtime-spike.md](experiments/runtime-spike.md).

Additional headless Arch and Ubuntu Base images can be chosen with
`./ow create NAME --image arch` or `--image ubuntu`; see [image preparation and
limits](GUEST_IMAGES.md). Existing workspaces keep their original Alpine profile.
