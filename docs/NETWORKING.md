# Guest Internet and remote CLI

Implemented and observed on the dedicated development host on 2026-10-02.
Guests now have outbound IPv4 TCP/UDP access, DNS, and a default route, including
following RAM/disk fork and hibernation restore. Browser and CLI terminals remain
Access-authenticated HTTPS/WebSocket connections; no guest SSH daemon is required.

## Rootless egress policy

`ow up` starts a dedicated supervisor, user/network namespace worker, and upstream
slirp4netns 1.3.6 (libslirp 4.9.5). The pinned executable is fetched and SHA256
verified by `scripts/prepare-network.py`. It is a separate GPL-2.0-or-later upstream
executable, not imported source in the Apache Rust crates. Its sandbox, seccomp,
and disable-host-loopback options are enabled. There are no inbound port mappings.
All nftables rules and forwarding sysctls live in the disposable worker network
namespace. No host routes, host firewall rules or global sysctls are changed.

Per-TAP source-IP validation prevents guest root from using another VM's source
address. Forwarding defaults to drop. Outbound public IPv4 TCP/UDP is allowed;
private, loopback, link-local/metadata, multicast, benchmark/guest ranges and the
host's actual interface addresses at worker startup are blocked. The DNS alias
10.0.2.3 allows only TCP/UDP port 53. DNS resolving a private destination does not
bypass the packet filter. Guests cannot route to peer VMs or reach worker addresses
assigned to another TAP. Management can still connect to the guest. A temporary
asset listener binds only the requesting guest's host TAP address and verifies
its peer address; it serves a fixed hash-verified project asset.

The supervisor owns both child processes and cleans them up together. An uplink
exit causes worker shutdown; disks and existing checkpoints remain recoverable.
Restart the worker to refresh the host-interface deny list after address changes.

Arch and Ubuntu additionally receive a small verified Alpine HTTPS helper bundle
under `/ow`, with private libraries and CA roots. These do not replace their native
OpenSSL libraries. An unconfigured Arch mirror list gets the official geo mirror.
Arch package installation may require `pacman-key --init` and `pacman-key --populate
archlinux` inside the guest. Package-manager update checks are tracked separately
from DNS and HTTP reachability.

This is the development pilot policy, not production tenancy certification.
IPv6, ICMP/ping forwarding, per-workspace bandwidth/rate quotas, per-workspace
network policies and egress throughput/density benchmarks remain pending. Snapshot
and fork do not preserve external TCP/WebSocket connections; reconnect clients.

## Connect from macOS or Linux

The dashboard shows a copyable command using its existing hostname:

```bash
curl -fsSL https://YOUR_EXISTING_HOST/cli/install.sh | sh
ow login https://YOUR_EXISTING_HOST
ow list
ow shell MACHINE
ow exec MACHINE -- 'uname -a'
```

The installer selects a native macOS Apple Silicon/Intel client (macOS 13+) or
a static musl Linux x86-64 CLI, verifies its SHA256, and
installs it in `~/.local/bin`. If cloudflared is unavailable it installs the pinned,
verified official helper under `~/.local/lib/open-workspaces`. It uses curl and sha256sum or macOS shasum; no sudo. `OW_INSTALL_DIR` selects another CLI directory. Add the printed
PATH setting if needed. Linux ARM and Windows installers are not yet shipped.

`ow login` uses cloudflared's browser Access login and caches only the selected
HTTPS origin in a mode-600 client config. cloudflared manages the operator session
credential. No Cloudflare account admin token or tunnel credential belongs on a
client or in a guest. HTTPS/WSS validate TLS certificates. Redirects are disabled
for authenticated API calls. The terminal sends the exact origin, handles raw
input, resize, Ctrl-C/job control, bounded output acknowledgments and guest exit
status. Sign in again after expiry. `--server https://HOST` selects a server without
changing the saved one; `--local` selects the local worker. Remote commands cover
list/status/stats/inspect, exec/shell, create/start/stop, snapshot/fork/hibernate/restore.
Local worker lifecycle, file transfer and HTTP publishing commands remain local.

Only `/cli/install.sh`, `ow-linux-amd64`, `ow-darwin-arm64`, `ow-darwin-amd64`
and their `.sha256` files under `/cli/` are
public GET/HEAD resources. A dedicated Access bypass application covers `/cli/*`,
but the Tunnel authentication exception matches only those seven exact files.
The Rust gateway also bypasses JWT only for those exact fixed resources, rejects
queries and non-read methods, and never maps a request to an arbitrary host file.
The dashboard, API and terminal retain their existing Access allowlist/JWT checks.
`scripts/publish-cli.py` owns only this project's path-specific Access application
and ingress entry, keeps infrastructure identifiers private, and refuses to adopt
unrelated applications. Root domain and global SSL settings are unchanged.

## Acceptance evidence

`experiments/runtime-spike/network-test.py` boots real Alpine, Arch and Ubuntu VMs,
checks DNS/public HTTP/HTTPS, peer/private/metadata/host-address denial, guest-root
source spoofing, fork/hibernation egress and failed-uplink recovery. The dashboard
browser test also drives the remote native CLI through a local TLS edge with an
ephemeral Access assertion: list/exec, rejection of an untrusted certificate, real
WSS PTY, resize, Ctrl-C, exit 42 and restored local terminal attributes. The edge
models header injection, not a real Cloudflare browser login. Production public
installer reachability and protected-route denial are checked independently;
authenticated remote CLI login through Cloudflare still requires the owner to sign
in. No production login bypass or fabricated network benchmark is used.

Live deployment acceptance: the existing hostname served the installer and checksum
with HTTP 200 to curl without login. A fresh isolated user-directory installation
downloaded both verified binaries and executed the static CLI. Anonymous requests
to the dashboard, API and terminal returned Access redirects; `/cli/api/state`
and extended artifact paths were denied, and a file-selection query returned 400.
All four previously running machines were restored from paired RAM/disk checkpoints
and successfully fetched a public HTTPS page; the previously hibernated machine
remained hibernated. Unit/auth tests, Clippy, the 20-check real-VM lifecycle regression,
11-check network regression and 21-check browser/remote-CLI regression passed.

The macOS client is compiled separately from Linux-only runtime modules. Both
Mach-O targets were built on Linux using pinned cargo-zigbuild 0.23.4, Zig 0.15.2
and a build-only MacOSX 14.5 SDK (joseluisq/macosx-sdks mirror), all SHA256 verified
by `scripts/prepare-macos-build.py`. `scripts/build-macos.sh` publishes only the
client binaries/checksums, not SDK or build tools. Both resulting Mach-O executables declare macOS 13 as their minimum; the Apple
Silicon binary has an ad-hoc Mach-O signature, not an Apple developer notarization.
Native macOS execution, Keychain trust loading and browser login require checking
on a Mac; cross-compilation alone does not establish those behaviors.
