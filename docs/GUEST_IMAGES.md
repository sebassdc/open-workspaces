# Headless guest images

The prototype now has an explicit image profile on create, with that profile
preserved in workspace/snapshot metadata and inherited by forks. Existing
workspace records and snapshots default to Alpine, preserving compatibility.

| Profile | Userspace | Provisioned root disk | Intended comparison |
| --- | --- | ---: | --- |
| `alpine` | Alpine 3.24.2, BusyBox ash | 128 MiB | Existing smallest baseline |
| `ubuntu` | Ubuntu Base 24.04.5 amd64, Bash/apt + developer tools | 8 GiB sparse | Ubuntu development environment |
| `arch` | Arch bootstrap 2026.10.01, Bash/pacman + developer tools | 8 GiB sparse | Headless Arch development environment |

These are real Firecracker VMs using the same pinned Linux 6.1.186 kernel and our
minimal init. They are not containers and do not boot the distributions' stock
kernel/systemd stack. No desktop or display server runs. Package managers and developer tools are
present. Guests now have filtered IPv4 Internet access, DNS and HTTPS helpers;
see [network policy and remote CLI](NETWORKING.md). The Arch bootstrap includes more base
packages and is considerably larger on disk than Ubuntu Base.

## Omarchy findings (2026-10-02)

[Omaterm](https://github.com/omacom/omaterm) described itself as headless Omarchy:
Bash, Starship, fzf, eza, zoxide, tmux, Neovim, developer tools and agents. That
repository was archived on 2026-08-30 and points to
[Omarchy Server](https://omarchy.org/server/), whose current official page says
“Coming 2026.” It is not an available verified replacement image for this pilot.
Omaterm's documented installation runs a Docker environment and can expose the
host Docker engine; that deployment model is not used here.

Our `arch` profile is **headless Arch**, not official Omarchy/Omaterm, and does not
claim their complete CLI tools, agent integrations or configuration. It provides
an independent Bash configuration and developer tool layer described below. No host dotfiles, accounts, tokens, desktop configuration or Docker
sockets are copied into any guest.

## Prepare and use

```bash
python3 experiments/runtime-spike/prepare-profiles.py ubuntu
python3 experiments/runtime-spike/prepare-profiles.py arch
bash scripts/build-local.sh
python3 experiments/guest-dev/build.py ubuntu
python3 experiments/guest-dev/build.py arch
./ow up
./ow create ubuntu-test --image ubuntu
./ow create arch-test --image arch
./ow shell ubuntu-test
./ow shell arch-test
```

The dashboard's **New machine** dialog also has an **Operating system** selector.
Open Terminal uses Bash in images that include it and falls back to ash for
Alpine. Stop/start keeps the same userspace and disk; snapshot/fork/restore keeps
the paired RAM/disk state and image profile. There is no in-place OS conversion.

The builder downloads versioned upstream rootfs archives, verifies pinned SHA256
checksums, safely extracts into fresh ignored staging storage, adds only a small
management helper/init and Bash configuration, and creates ext4 using a dedicated
unprivileged user namespace. It requires no sudo, Docker or host mounts and
refuses to overwrite an existing image. Ubuntu's checksum is from Canonical's
[release checksums](https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/SHA256SUMS);
Arch's pinned artifact is available from the
[official archive](https://archive.archlinux.org/iso/2026.10.01/).
Checksums were obtained over HTTPS; detached release-signature verification was
not performed. Licenses for upstream binary packages remain with those packages;
image artifacts are private/ignored, not distributed as repository source.

## Developer tools and permissions

New Ubuntu and Arch workspaces use `dev` (UID/GID 1000) in `/home/dev` for
interactive terminals and `ow exec`. `sudo` requires no password **inside the
guest**. It grants guest root; host credentials, the host filesystem and the
worker/hypervisor sockets are not exposed. This single-operator prototype does
not provide separate capabilities for different users of the same workspace.
Existing minimal workspaces retain their existing disks and root shell; create a
new workspace to get the developer template. There is no destructive migration.

Both templates include Git, curl/wget, Neovim, GCC/make, pkg-config, CMake/Ninja,
common native build libraries, OpenSSH client, jq, ripgrep, fd, fzf, tmux, zip/unzip,
man pages, Bash completion, certificates and guest sudo. `mise` 2026.10.0 activates
Node 24.21.0 (npm 11.19), Python 3.13.16 and Rust/Cargo 1.99.0, with rustfmt and
Clippy. Python virtual environments work. Rust documentation is omitted to save
space. Tool installation and activation belong to the guest developer account.

Ubuntu retains signed official noble, updates, security and backports packages,
including main/universe/restricted/multiverse; Arch retains signed core/extra.
Install additional tools or change language versions normally:

```bash
# Ubuntu
sudo apt-get update
sudo apt-get install tree
# Arch: update the system together with package installation
sudo pacman -Syu tree
# Either profile
mise use --global node@lts python@3.13 rust@stable
```

The developer builder installs packages in a dedicated microVM, records package
inventory/toolchain versions, validates sudo, cleans package caches and builder
logs, and promotes a stopped ext4 filesystem only after a filesystem check.
Sparse extraction uses `e2image -ra` (all allocated filesystem data); the 8 GiB
logical size does not reserve 8 GiB on the host. Observed template allocated
blocks are 1,732.1 MiB for Ubuntu and 2,581.6 MiB for Arch. These are individual
file allocations, not unique clone disk usage. Btrfs reflinks share unchanged
blocks; workloads, package updates and retained snapshots add storage. Default
VM memory is still 256 MiB; use 1,024 MiB for a practical initial developer
workspace, then measure the actual workload. Admission accounts for configured
memory even when idle resident memory is lower. No new developer-image latency
or maximum-density benchmark is claimed.

## Validation and consumption

The developer regression uses fresh 1,024 MiB VMs, installs `tree` from signed
repositories, checks UID/permissions, compiles C and Rust, creates a Python venv,
runs Node, validates Git/Neovim and exercises paired RAM/disk snapshots, forks,
hibernation, restore and cold restart:

```bash
python3 experiments/guest-dev/test.py
```

Observed developer acceptance run (2026-10-03): both profiles passed the above
checks, including live Python process counters across paired RAM/disk operations.
Private artifacts: `data/dev-test-houcfxhp/result.json`. Native PTY checks passed
for both profiles (`data/tty-gbah8qwu` and `data/tty-bju3sni_`); the real VM/browser
and remote HTTPS/WSS regression passed 21 checks (`data/ui-20b84531`). Mac
installer branches and downloaded Mach-O architecture/checksums were verified
under a platform simulation; native execution on macOS remains unverified.

Native terminal tests pass on both Ubuntu and Arch: guest TTY, raw input/Ctrl-C,
resize, shell exit 42 and local terminal restoration. The real HTTPS/WSS browser
regression also creates both image profiles through the UI, checks their actual
`/etc/os-release`, and opens interactive guest PTYs. The profile regression
exercises package-manager execution, persistent files, process-memory HTTP
counters, snapshot/fork independence, hibernation and checkpoint restore.

```bash
python3 experiments/runtime-spike/terminal-cli-test.py --image ubuntu
python3 experiments/runtime-spike/terminal-cli-test.py --image arch
# Build the test workload, independently of the guest base images:
export RUSTUP_HOME="$PWD/data/runtime-spike/rustup"
export CARGO_HOME="$PWD/data/runtime-spike/cargo"
mkdir -p data/runtime-spike/profiles
"$CARGO_HOME/bin/rustc" +1.97.0 --edition=2024 --target x86_64-unknown-linux-musl \
  -C opt-level=2 experiments/runtime-spike/http-fixture.rs \
  -o data/runtime-spike/profiles/http-fixture
strip data/runtime-spike/profiles/http-fixture
python3 experiments/runtime-spike/profile-test.py
```

The profile pilot records five serial create-to-management-ready samples and
idle process PSS samples per image, with versions, host CPU/kernel/RAM, method,
p50/p95 and raw samples in ignored artifacts. It uses 256 MiB/one vCPU, waits
250 ms before idle PSS, keeps host caches warm, and runs on the shared desktop
with other VMs. Load is not controlled; this is not a maximum-density benchmark.
PSS excludes additional host filesystem cache, controller and kernel overhead.
A larger disk image does not imply the whole userspace is resident in RAM.

Historical **minimal images before the developer layer**: observed warm pilot on Ryzen 7 3700X (8 cores/16 threads), about 31.2 GiB RAM,
Btrfs, host Linux 7.2.5-3-omarchy, Firecracker 1.17.0 and guest Linux 6.1.186:

| Profile | Create p50 / p95 | Idle VM PSS p50 / p95 | Base file allocated blocks |
| --- | ---: | ---: | ---: |
| `alpine` | 830.96 / 859.63 ms | 59.32 / 59.32 MiB | 17.0 MiB |
| `arch` | 840.80 / 844.63 ms | 59.32 / 59.33 MiB | 630.3 MiB |
| `ubuntu` | 833.50 / 838.51 ms | 59.32 / 59.32 MiB | 102.0 MiB |

Sample count is **5 per image**, nearest-rank p95 is the maximum sample, and
shared-host load was not controlled. Readiness is the management serial shell and
network configuration, before opening Bash or running a workload. All three boot
the same minimal management init, explaining their similar idle memory; this
does not establish that full Omaterm tooling or real workloads consume the same
resources. Each running VM still reserves 256 MiB in the admission controller.
Allocated blocks are for each base file, not unique CoW clone consumption.
