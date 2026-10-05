# Experimental native Mac host runtime

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
