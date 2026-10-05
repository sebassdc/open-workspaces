# Native Apple Silicon host

## Managed Ubuntu ARM64 node

The bounded Mac node implementation passed local hardware and isolated TLS
transport acceptance. It has not yet passed the live human gateway/dashboard
pool acceptance. Read [report](plans/mac-node-report.md),
[handoff](plans/mac-node-handoff.md) and [ADR 0004](adr/0004-mac-node-private-transport.md).

Build on Apple Silicon with Swift, Rust and Command Line Tools:

```sh
./scripts/build-mac-host.sh
./target/mac-host/ow host doctor
```

Keep the release CLI beside its ad hoc signed `ow-vz` helper. The linked helper
minimum is macOS 14.0; this run does not establish hardware acceptance on macOS
14. Public installers are not updated by this task. The operator consents to a
trusted shared private pool: admitted users can select this host, whose operator
can inspect data and stop participation. Node credentials grant participation,
not human workspace administration.

Prepare a NEW private asset directory from signed official Ubuntu Noble ARM64
inputs. `scripts/prepare-mac-node.py` does not download them. Its input directory
requires the release root tar, unpacked kernel/initrd and both signed SHA256SUMS
files; pinned UEC key `cloud-image.gpg`; signed noble-updates `InRelease`,
`Packages.xz` and `linux-modules.deb`. Archive trust is derived directly from
the authenticated root tar; an optional supplied `archive-keyring.gpg` must
match those exact bytes. Successful release signers must match the pinned
primary fingerprint in an isolated restricted keyring; UID text is not trust. Versions and hashes are in [pins](plans/mac-node-pins.json).
The prepared root preserves package ownership/setuid modes, adds the `dev`
account with sudo and installs the static ARM64 guest agent. Homebrew e2fsprogs,
GnuPG, Zig, cargo-zigbuild and the Rust ARM64-musl target are build prerequisites.
Never reuse x86 images or expose a host directory to the guest.

```sh
cargo zigbuild -p ow-guest --release --target aarch64-unknown-linux-musl
python3 scripts/prepare-mac-node.py PRIVATE_UPSTREAM PRIVATE_NEW_ASSETS \
  --guest target/aarch64-unknown-linux-musl/release/ow-guest
```

After planner Linux review and a dedicated invitation for the exact coordinated
controller revision, use a fresh private root and mode-0600 invitation file in a
private directory. Keep the prepared assets outside the node root:

```sh
OW_MAC_ASSETS=PRIVATE_NEW_ASSETS ./target/mac-host/ow --data-dir PRIVATE_NEW_NODE \
  host join --invite-file PRIVATE_INVITE --accept-shared-pool \
  --memory 1024 --cpus 2 --slots 1 --storage-gib 20 --no-start
./target/mac-host/ow --data-dir PRIVATE_NEW_NODE host start
./target/mac-host/ow --data-dir PRIVATE_NEW_NODE host status
./target/mac-host/ow --data-dir PRIVATE_NEW_NODE host stop
```

For a private CA, pass `--ca-cert PRIVATE_CA` at join; verified saved trust is
used on restart. No redirect, TLS bypass or arbitrary proxy is accepted. A lost
enrollment reply leaves intent: preserve the root, revoke/reinvite a fresh ID
rather than blindly redeeming another invitation. The default root is
`$HOME/.ow-mac`. Stop retains disks, machine identity and credentials. Configure
requires stopped participation and positive disk/process checks. Status reports
controller dispatchability only for matching live identities and a fresh
generation acknowledgement; enrollment or a PID alone is insufficient.

| Contract | Mac node |
| --- | --- |
| Backend / architecture / runtime | apple-virtualization / aarch64 / apple-vz-v1 |
| Image | ubuntu-arm64 |
| Guest RAM / CPUs / concurrency | 512, 1024 or 2048 MiB / 1–2 / one |
| Persistence | independent 4 GiB cold disk copies; at most two retained |
| Operations | create, start, stop, inspect, list, status, stats, exec, put, get, shell |
| Explicitly unavailable | snapshot, restore, hibernate, fork, publish, SSH, tunnels, resize |
| Network / host integration | guest loopback only; no NIC, NAT, shares or host sockets |

Owner caps and local budgets both constrain scheduling. `--storage-gib` is a
minimum free-space reserve (20–1024 GiB), not the retained disk quota. Unknown,
unregistered or unresolved runtime demand consumes capacity. A private user-wide
admission lock, independent of `--data-dir` and environment HOME overrides,
allows one native worker; its helper inherits the reservation until exit even
if the worker crashes. Incomplete create replay stays uncertain, and failed
guest sync cannot bypass the owned host shutdown pipe; stop returns the
sync diagnostic after positive exit. Guest root cannot
change host policy because no external networking or host mounts are attached.
Outbound networking requires a separately reviewed host-enforced policy.
No live fork or APFS clone is claimed.

Hardware acceptance requires a fresh dedicated root and sufficient headroom:

```sh
python3 scripts/test-mac-node.py PRIVATE_TEST_ROOT PRIVATE_NEW_ASSETS
python3 scripts/test-mac-node-transport.py PRIVATE_TLS_ROOT PRIVATE_NEW_ASSETS
```

These suites start at most one guest concurrently and keep private receipts and
failed attempts under ignored storage. The TLS suite runs an isolated real
controller and native node agent; it does not prove deployed human gateway or
browser behavior. Do not run suites concurrently or exceed the 20 GiB dedicated
storage budget. No latency benchmark is reported.

## Earlier offline feasibility fixture

The first native Apple Silicon runtime slice is implemented and hardware-tested.
It is an offline local feasibility fixture, not a node in the existing Linux pool.
See [ADR 0003](adr/0003-native-mac-runtime-spike.md) and [report](plans/mac-host-report.md).

## Build and test
On Apple Silicon macOS with Swift, Rust and Command Line Tools:

```sh
python3 scripts/test-mac-host-harness.py
./scripts/build-mac-host.sh
./target/mac-host/ow mac-host capabilities
mkdir -m 700 data/my-mac-fixture
python3 scripts/prepare-mac-host.py --data-dir data/my-mac-fixture
python3 scripts/test-mac-host.py --data-dir data/my-mac-fixture
```

The preparer fetches two HTTPS artifacts pinned by SHA256, unwraps the ARM64
kernel and creates a fresh dedicated 256 MiB disk. No administrator rights,
network changes or existing disk are needed. Never select user/production disk
roots. The test requires a fresh root and retains captures rather than replacing
them. It starts only one 512 MiB/1-vCPU guest at a time and confirms owned process
exit. Private receipts/artifacts stay under ignored data storage. Marker waits cap
unmatched output at 256 KiB and retain a 64 KiB diagnostic tail; excessive output
fails and stops the owned helper. Closed or full stdin cannot bypass bounded
terminate/kill/wait cleanup. The subprocess harness regressions run without a VM.

To open the serial console manually:

```sh
./target/mac-host/ow --data-dir data/my-mac-fixture mac-host boot
```

Inside the guest, use `uname -m`, `echo hello > /persist/message`, `sync` and
`poweroff -f`. Only /persist survives cold boot. This shell uses a controlling
serial tty, not the product guest-agent PTY. The helper replaces the Rust CLI
process so its PID and signals remain direct. Host SIGTERM/SIGINT force-stop the
VM; prefer guest sync/poweroff for disk consistency. Do not launch other fixture
roots concurrently: the initial authorization is one guest across this lane.
The lock enforces one guest per root, not an aggregate host admission policy.

Experimental capture uses SIGUSR1 sent only to the verified owned helper PID
after guest sync/quiescence. It publishes one immutable `checkpoint` directory
with paired disk/state hashes and then resumes. No product snapshot/fork command
is exposed. A failed capture retains incomplete files for diagnosis.

A stopped capture can be restored using a writable independent copy:

```sh
cp -c data/my-mac-fixture/checkpoint/disk.img data/my-mac-fixture/recovery.img
./target/mac-host/ow --data-dir data/my-mac-fixture mac-host restore --disk data/my-mac-fixture/recovery.img
```

Preserve the original private machine-id. The helper requires exact same-host
configuration and paired bytes. A working disk modified since capture is denied;
recovery does not overwrite the newer disk. This is same-identity recovery, not a
concurrent live fork. No guest network or host directories are exposed.

## macOS guest feasibility

```sh
./target/mac-host/ow mac-host macos-probe
```

This asks Apple's framework for supported restore metadata only. It downloads no
IPSW, starts no guest and accepts no license terms. Actual Tahoe installation and
guest access remain pending. The observed 4 GiB minimum exceeds the original
2 GiB authorization and requires a separate owner allocation.

## Remaining work
Persistent ARM64 root/developer image, guest-agent exec/files/real PTY integration,
managed runtime journal/recovery, host admission, identity-refreshed forks,
filtered networking, coordinated backend/node capability extensions, private
invitation, TLS/enrollment/fencing/owner-scope pool acceptance and Tahoe guest
installation. Existing Firecracker snapshots cannot be routed here.
