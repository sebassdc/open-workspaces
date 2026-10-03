# Runtime feasibility experiment

These Python standard-library probes support the runtime spike; the product
runtime remains planned in Rust. No public API or VM service exists yet.

Run from the repository root:

```bash
mkdir -p data/runtime-spike/results
python3 experiments/runtime-spike/preflight.py > data/runtime-spike/results/preflight.json
python3 experiments/runtime-spike/storage_probe.py > data/runtime-spike/results/storage.json
```

Preflight exits 2 when KVM is unavailable. A successful KVM VM creation only
establishes basic device access; guest boot and workload readiness remain separate
acceptance checks. The storage probe uses FICLONE on a temporary 16 MiB file,
verifies writes in both directions, and cleans up its own files. It does not
demonstrate RAM snapshots, VM cloning, or application consistency.

Keep upstream checkouts, toolchains, build logs, guest disks, keys, snapshots and
raw host reports under ignored `data/runtime-spike/`. Never commit them.
Upstream code is evaluated in separate checkouts and is not imported into this
repository's implementation.

Pinned sources and next acceptance checks are in
[runtime-spike.md](../../docs/experiments/runtime-spike.md).

Fetch the pinned official Firecracker release with verified archive hash:

```bash
bash experiments/runtime-spike/fetch-firecracker.sh
```

The isolated GNU source build is useful for build feasibility only. It has no
default syscall filtering; use the official release for boot experiments.

Prepare and exercise a real disposable guest:

```bash
python3 experiments/runtime-spike/prepare-guest.py
unshare --user --map-root-user --net python3 experiments/runtime-spike/guest-smoke.py
```

Preparation refuses to overwrite an existing base image. The shell/network test
keeps its generated disks and snapshots in ignored data and closes its VMMs.

The Rust prototype's real-VM tests and measurement tools are:

```bash
python3 experiments/runtime-spike/prototype-e2e.py
python3 experiments/runtime-spike/measure.py --samples 10
python3 experiments/runtime-spike/memory-profile.py
```

These use fresh dedicated data directories and shut down their own workers and
gateways. The recovery test deliberately kills only its authenticated test worker.
See [the local prototype guide](../../docs/LOCAL_PROTOTYPE.md) for normal usage.
