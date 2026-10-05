# Guest OpenSSH

This source adds real OpenSSH to Linux guests. `ow shell` remains the separate
PTY feature. SSH travels as binary stdio through the authenticated HTTPS/WSS
human gateway, the machine's fixed placement and its fenced outbound node job,
then guest TCP port 22. Local operator access uses the private worker socket.
No public guest listener, inbound node port or arbitrary tunnel destination is
added. Review/publication of this source and new image assets is a planner step.

## Connect

Install an ordinary OpenSSH client and sign in with `ow login https://PROJECT_HOST`.
Choose an existing **public** Ed25519 key, or explicitly create a dedicated one
with `ssh-keygen -t ed25519 -f ~/.ssh/ow_ed25519`. Keep the private file local.
For a new SSH-capable Ubuntu/Arch developer image:

```sh
ow ssh-authorize MACHINE --key ~/.ssh/ow_ed25519.pub
ow ssh-info MACHINE
ow ssh MACHINE -- -i ~/.ssh/ow_ed25519
ow ssh MACHINE -- -i ~/.ssh/ow_ed25519 'uname -a; exit 42'
```

`ow ssh` invokes installed `ssh` as guest `dev`, prints the expected guest public
host key on stderr and returns OpenSSH's exit status. OpenSSH's normal
`StrictHostKeyChecking=ask` and known_hosts verification remain enabled. Compare
the initial fingerprint with `ssh-info` (public key piped to `ssh-keygen -lf -`)
before accepting. Changed keys are rejected; never remove a known_hosts entry
merely to silence a warning. There is no private-key upload or implicit enrollment.

For an existing Ubuntu/Arch **developer** guest, explicit opt-in installs the
distribution server and config inside that one guest:

```sh
ow ssh-authorize MACHINE --key ~/.ssh/ow_ed25519.pub --upgrade
```

This mutates the selected guest, requires guest package-network access and a
`dev` account, and may fail if packages are unavailable. Never apply it to a
running owner guest without the owner's explicit request. Minimal Alpine is
unsupported. Package installation uses the existing bounded serial operation;
a slow install can exceed its 30-second command or 45-second gateway deadline.
An uncertain result requires inspecting operation/guest state, then retrying the
same operation key; if the version marker was successfully installed, omit
`--upgrade` when issuing a new enrollment. No automated disk upgrade occurs on
worker startup or the first SSH connection.

## Standard OpenSSH, SFTP and editors

Generate a reviewable config without changing `~/.ssh/config`:

```sh
ow ssh-config MACHINE > ow-ssh.conf
ssh -F ow-ssh.conf -i ~/.ssh/ow_ed25519 ow-MACHINE 'id -un'
sftp -F ow-ssh.conf -i ~/.ssh/ow_ed25519 ow-MACHINE
```

The generated stanza names the exact machine, `dev`, a machine-specific
`HostKeyAlias`, strict host checking and a standard `ProxyCommand` using the
current absolute CLI executable and explicit server. It contains no token or
private key. Select that config in an editor's SSH config setting, or review and
copy the stanza yourself. The proxy's contract is raw bytes on stdin/stdout;
all CLI diagnostics stay on stderr. A manual equivalent is:

```sshconfig
Host ow-example
    HostName example
    User dev
    HostKeyAlias ow-IDENTITY_FROM_SSH_INFO
    StrictHostKeyChecking ask
    ProxyCommand /absolute/path/ow --server https://PROJECT_HOST ssh-proxy example
    IdentityFile ~/.ssh/ow_ed25519
```

For trusted local maintenance, use `ow --local --data-dir PRIVATE_WORKER_ROOT
ssh-config PHYSICAL_ID` or `ssh PHYSICAL_ID`. Local access is host-operator
maintenance authority, not a restricted tenant identity. Generated aliases stay
unique across machine names, accounts, recreated machines and fork children.
A selected remote config always uses that authenticated owner's logical name.
Cloudflare token acquisition and TLS validation occur on every proxy connection;
redirects are refused. DNS and TCP each have a five-second budget; the TLS/HTTP
handshake has a ten-second absolute watchdog. Test-private CA support uses explicit client trust setup,
never insecure TLS flags.

## Keys, capture, fork and restore

`ssh-authorize` **replaces** the selected machine's full key list. Repeat `--key`
to register up to 16 plain Ed25519 public keys. Options, certificates, private
material, multiline input and malformed blobs are rejected; comments are stripped.
`ow ssh-revoke MACHINE` removes every managed key and closes current SSH streams.
Enrollment/revocation is owner-authorized before worker contact and uses existing
owner operation keys and node result journaling. Worker credentials and sockets
never enter the guest.

The private worker `machines/PHYSICAL_ID/ssh.json` contains public host identity,
registered public keys, a random stream generation and readiness. It lives
**outside** disk/RAM captures. A failed key application stays unready and denies
new streams. Restore reapplies this current policy, so an older snapshot cannot
revive a revoked managed key. Keep this sidecar with worker backups; deleting it
is not a supported identity reset. Guest root retains authority over its guest
filesystem and server; host transport ownership/fencing remains authoritative.
This feature does not sanitize arbitrary application secrets copied in snapshots.

Guest host private keys are generated in the guest, remain in its disk and are
never uploaded by the client. Same-machine cold boot, hibernation and a snapshot
with the same host key retain its identity. Fork kills inherited sshd/session
processes before host TAP exposure, deletes host keys and managed authorization,
and creates no child policy. Child access requires its owner's explicit
enrollment and receives a fresh public host key and client alias. Source and
child disks remain independent.

Restore also stops inherited sshd/session processes before exposure. Restoring a
missing/different host key from another point in lineage fails closed against
the sidecar pin; no automatic trust replacement or host-key rotation API is
provided. Restore a known matching checkpoint or involve the operator to inspect
lineage. Existing TCP connections are not preserved across fork/restore/restart.
Detached guest application processes may persist; closing SSH is not a promise
to kill every descendant or undo already accepted work.

## Policy and bounds

Guest-only OpenSSH configuration permits `dev` public-key authentication,
exec, PTY and internal SFTP. Password, keyboard-interactive, root, agent, X11,
TUN and remote/Unix-socket forwarding are disabled. Local TCP forwarding is
allowed for editors; destinations are reached **from the guest** under the
existing namespace packet filter. This does not bypass peer/private/metadata/
LAN isolation or grant host management access. These denials govern OpenSSH's
built-in forwarding; a guest user with a shell can run their own guest programs.

Human SSH frames are binary only, <=16 KiB; text/oversized frames close the stream.
Gateway: 16 SSH slots, <=64 KiB write buffer, 5-second writes, 90-second **byte
inactivity** and JWT expiration/one-hour maximum. Ping/Pong does not extend byte
inactivity; use OpenSSH ServerAliveInterval for idle interactive sessions.
Worker: shared SSH/terminal cap of 16, fixed endpoint, 16 KiB buffer,
5-second socket writes, 90-second byte inactivity and one-hour maximum.
Stdout backpressure has a five-second deadline. Disconnect drops both upstream
sides and releases slots; no per-connection unbounded queues or copy threads.

Outbound node jobs retain existing 64 KiB transport frames, one-use job binding,
controller capacity, 10-second dispatch and 5-second writes. Revocation/session
replacement checks precede reads/writes in both directions and are polled every
three seconds; already accepted effects/in-flight writes cannot be undone.
A replacement/revoked/offline node invalidates existing streams and denies new
connections, without moving the machine. Optional protocol-v1
`guest_ssh_v1:true` advertises worker support for fixed SSH/key/info operations and
external snapshot-safe policy. Missing means unsupported; it does not assert
that every image contains a server. This contract is backend-independent; Mac
hardware/server capability has not been tested by this lane.

## Provisioning and evidence

New developer image recipe version is `developer-v2-ssh`, with a versioned
`/etc/ow-ssh-v1` marker, distribution OpenSSH/procps and no host keys or enrolled
keys in the template. `experiments/guest-dev/build.py` now requires a fresh
`--output-assets` directory and retains existing published images/manifests.
No image rebuild, served bundle change or shared service restart was performed.
Legacy pinned bundles keep their existing bytes and require explicit upgrades.

See [implementation report](plans/ssh-guest-report.md) for executed commands,
source/binary attribution, failures, cleanup and limitations. The ignored
`openssh_guest_real_vm` test and `experiments/ssh-guest/acceptance.py` require an
explicit fresh short root, binary, pristine template assets and a planner-granted
exclusive host turn. They never adopt owner disks or use production credentials.

Configuration and trust behavior follow the official
[OpenSSH server manual](https://man.openbsd.org/sshd_config),
[client configuration manual](https://man.openbsd.org/ssh_config) and
[key tool manual](https://man.openbsd.org/ssh-keygen). OpenSSH is a separate
upstream distribution executable; no new crypto implementation or crate dependency
is introduced.
