# Guest SSH implementation report
Updated: 2026-10-04 | Size: L | State: source complete; RELEASE review gate
Branch: `work/ssh-guest` | Baseline: `b1703f1`
The scoped commit containing this report is identified by `git log -1`.

## Result and scope

Real distribution OpenSSH access is implemented for explicitly enrolled
Ubuntu/Arch Linux guests, through both local worker stdio and authenticated
HTTPS/WSS outbound-node routing. `ow ssh` invokes ordinary `ssh`; `ssh-config`
prints a standard ProxyCommand stanza usable by OpenSSH/SFTP/editors without
editing user config. This is SSH protocol to guest sshd, not the PTY transport.

Pattern: ADR0002 owner-first SQLite lookup, fixed placement, one-use
node/generation-bound jobs. New `/api/ssh/NAME` and private `{op:"ssh",id:...}`
accept no address/port; the worker connects only to the selected guest port 22.
The optional `guest_ssh_v1` capability is additive and backend-independent;
missing capability denies remote SSH. No protocol/schema migration or Mac
backend implementation is included. Planner accepted this wire direction.

Public-key registration/revocation is per machine and owner-bound. Only plain
Ed25519 public keys are accepted, <=16; private keys never leave the client.
Private sidecar policy remains outside RAM/disk snapshots. Key updates persist
an unready generation fence before guest mutation, then enable it only after
configuration succeeds. Restart/restore reapplies current policy; revoked access
cannot return from an old snapshot. Fork stops inherited sshd processes before
TAP exposure, removes managed authorization/host keys and requires separate child
enrollment. Same-machine host keys are pinned; mismatches fail closed. No
automatic changed-key acceptance or host-key rotation API is added.

The guest recipe adds normal distribution OpenSSH/procps, dev-only key auth,
internal SFTP and local TCP forwarding for editors. Password/root/agent/X11/
remote/TUN/Unix forwarding are disabled by configuration. `developer-v2-ssh`
uses a version marker and fresh output directory; existing image/bundle pins are
retained. Existing developer guests need explicit `ssh-authorize --upgrade`.
No image bundle was rebuilt or published, nor any existing owner guest upgraded.

A required narrow runtime fix routes Firecracker logging through its supported
log destination into a private 4 MiB diagnostic drain instead of guest command
serial. Interleaved VMM diagnostics had split a completion marker during a cold
start/restore. After the limit, bytes are discarded while draining; diagnostic
write failures also discard instead of closing the VMM pipe. Lifecycle cleanup
kills/waits the VMM and joins its drain. See upstream
[Firecracker logger documentation](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/logger.md).
No broad runtime refactor was undertaken.

## Frozen attribution

All private paths below are relative to this isolated lane. They remain ignored
and must not be uploaded: fixtures include credentials, private keys and snapshots.

- Native Linux GNU debug CLI: `data/ssh-evidence/frozen/ow` (same as tested
  `target/debug/ow`). SHA256:
  `38a6303328ebdb718efa45963cfb8f83241f306880b294ff79ecf0cc19d3a479`.
- Final compiled-source/fixture input map digest:
  `cecd172d42c11892125eb7be04b26b91632330ecdb5cdd498a375b8d742b39b3`.
  Method: SHA256 of sorted compact JSON of `inputs.json.source_files`; contains
  Rust/Cargo/UI/embedded provisioning inputs and acceptance script, not prose.
- Final acceptance script SHA256:
  `0ae7c50417837584216996210c48e385299a01ee32220074f0f2a98e13d7b0f6`.
- Per-file pins, pristine image manifest/hash, headroom and exact binary hash:
  `v/v/inputs.json`, `v/w/inputs.json`. Both report stable source throughout.
- Toolchain: Rust 1.97.0 (2d8144b78, 2026-07-07), locked existing crates,
  Firecracker 1.17.0, guest kernel 6.1.186. No dependency/lockfile change.
- Actual client: OpenSSH 10.5p1/OpenSSL 3.6.4. Guest provisioning logs record
  Ubuntu `1:9.6p1-3ubuntu13.19`, Arch `openssh-10.5p1-1`.
  The artifacts are native development binaries, not static distribution or Mac
  releases; no portability/production benchmark claim is made.

## Executed verification

**49 offline workspace tests passed, 5 real-VM tests intentionally ignored**, in
12.09 seconds with `--test-threads=1`. New checks cover strict public keys and
16/17 bounds, foreign name/physical-ID/info/key denials before any worker contact,
and actual Unix/TCP bytes in both directions with policy replacement requiring
EOF/reset and no stale output. Existing node transport tests cover both-direction
generation/revocation/job binding. Terminal and SSH Origin tests are retained.
These are transport/authority tests, not guest evidence.

**Ubuntu: 23 real acceptance checks passed**, final `v/v/result.json`, 92.81s.
**Arch: 23 real acceptance checks passed**, final `v/w/result.json`, 81.22s. Both use the exact frozen binary/source.
Full case names are in receipts; the executed behaviors include:

1. One real isolated guest created through a private-CA authenticated outbound
   node. Standard SSH over local and node routes returns `dev`, exact stdout and
   exit 42; easy `ow ssh` returns actual guest exit 37.
2. SSH PTY checks both guest TTY descriptors and an explicit `SSH_PTY_OK` marker,
   runs `stty`, and returns 23. Ordinary SFTP roundtrips all 32 KiB binary bytes.
3. Local TCP forwarding reaches guest loopback sshd; remote forwarding is denied.
   Unenrolled key and changed known_hosts entry are rejected.
4. Signed foreign owner, unknown and malformed/repeated-prefix names are denied.
   Wrong TLS CA/hostname produce failure with empty proxy stdout. A separate TLS
   redirect fixture is rejected with a zero-follow counter. Silent HTTP handshake
   fails within bounds (observed socket timeout; not a slow-drip watchdog test).
5. Actual authorized guest streams close on text and >16 KiB WebSocket frames.
   Active key revocation closes an SSH process and denies reconnect. A stopped
   guest is unavailable.
6. Restore retains host identity and current revoked-key policy; explicit
   reenrollment restores files. A sequential RAM/disk fork starts with no managed
   authorization or running sshd, denies SSH until enrolled, and receives a new
   host public key and alias. Parent/child are never running concurrently.
7. Cold boot, hibernate/resume and worker cold restart retain identity/policy and
   persistent guest files. Deliberately invalid sshd config leaves policy unready,
   denying old/new keys; an explicit matching restore applies the current policy.
8. Deliberately changed guest host key prevents cold exposure; matching snapshot
   restore recovers without replacing client trust. An authenticated replacement
   node control session and node revocation each cause positive SSH exit/EOF and
   deny stale/offline access. A timeout is not counted as fencing success.

**Legacy Alpine: 5 real checks passed**, final `v/x/result.json`: no SSH marker
or authorization, normal management exec/exit 19, SSH denial, sequential fork,
restore and cold boot. No OpenSSH/package provisioning occurs. This covers the
always-called version-guarded lifecycle hook on legacy guests.

`cargo fmt --all --check`, Python compilation, shell syntax and `git diff --check`
passed. Final guest serial logs contain no VMM diagnostic records; small private
`vmm.log` files retain the diagnostics separately. Log-cap/sink-error behavior is
code-reviewed rather than disk-full stress-tested. No latency benchmark is
inferred from suite durations.

## Exact commands and bounds

Executed from this lane, with explicit trusted source templates:

```sh
export CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo
export RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked --workspace -j 2 -- --test-threads=1
"$CARGO_HOME/bin/cargo" +1.97.0 build --locked -p ow -j 2
export OW_SSH_TEST_BINARY="$PWD/target/debug/ow"
export OW_SSH_TEST_ASSETS=/home/sebassdc/dev/open-workspaces/data/runtime-spike
OW_SSH_TEST_ROOT="$PWD/v/v" "$CARGO_HOME/bin/cargo" +1.97.0 test --locked -p ow openssh_guest_real_vm -j 2 -- --ignored --nocapture
OW_SSH_TEST_PROFILE=arch OW_SSH_TEST_ROOT="$PWD/v/w" "$CARGO_HOME/bin/cargo" +1.97.0 test --locked -p ow openssh_guest_real_vm -j 2 -- --ignored --nocapture
python experiments/ssh-guest/legacy.py "$PWD/v/x"
```

Reproduction must choose fresh short roots, not reuse these retained directories,
and obtain a planner host turn first. Rust tests have test-only signed identities
and private-CA TLS, with a header-injection fixture emulating Cloudflare. Production
signature/issuer/subject/owner checks still execute. Guest and node binaries are
the actual CLI; no mock SSH server or PTY is used for these acceptance claims.

Planner granted exclusive host turns before boot. Caps: ONE new guest at a time,
<=1024 MiB/2 CPU; worker 1024 MiB/one slot/two CPU, 8 GiB minimum free threshold.
Headroom checks require >=8 GiB MemAvailable and >=16 GiB free disk. Pristine
Ubuntu/Arch/Alpine templates are reflinked into fresh ignored storage; existing
owner disks are never copied or mounted. Kernel/engine/helpers are read-only
references. Listeners/CA/node/catalog roots are dedicated, never live listeners.

Cleanup is `finally`-guarded: down the exact private worker, terminate/wait only
spawned processes, verify no owned VMM/worker/controller/agent remains and compare
shared PID/start identities plus owner disk inode. Each final receipt confirms
cleanup, unchanged shared identities/owner inode and no remaining owned processes.
The final post-cleanup helper scan and pins are `data/ssh-evidence/final-checks.json`.
No shared service restart, remote host access, cloud write, paid resource, push,
main merge or publication occurred. Exclusive host turn was released to planner.

## Retained failures and practical limits

Earlier isolated runs are retained (`v/a` through `v/u`, omitting unused letters),
with cleanup receipts and logs under `data/ssh-evidence/`. They are not counted as
full acceptance: marker initialization; missing operation allowlist; stdio line
buffering; incomplete fixture CA extensions; plain-GET versus WebSocket assertion;
VMM diagnostic marker interleaving; config-file reuse; same-root agent lock;
silent-peer lower-bound assertion; and a shadowed expected payload. Product defects
were fixed; fixture defects were corrected. Intermediate `m/n/r/s` successes are
precursor evidence, not substitutes for final `v/w/x` attribution. The first
legacy fixture expected serial CLI output without its framing newline; corrected
`u/x` verifies marker absence, exit status and normalized serial output.

One final parallel offline run had 48 pass/one existing lost-reply recovery test
fail with `another gateway owns this catalog`. The complete serialized suite
passed; the related node-core test was not refactored. A tooling invocation with
incorrect toolchain environment was terminated before any VM; corrected explicit
roots produced the recorded builds. Do not count failed attempts as passing tests.

Limits: same physical Linux host only; no remote/NAT host, public Cloudflare login,
Mac hardware, actual editor UI, new prepared image bundle, static/cross build or
production deployment evidence. Byte inactivity closes idle sessions after 90s;
use SSH keepalives. Key registration supports Ed25519 only. Slow package upgrades
can hit existing serial/gateway deadlines and require operation-state inspection.
Immediate human JWT revocation remains the existing expiry boundary. Guest root
can change its own server; host ownership/network/fencing remains authoritative.
Arbitrary cloned application secrets/RAM are not sanitized. Detached guest work
may survive disconnect; accepted effects cannot be undone. Strict host-key trust
conflicts have no automatic rotation workaround. Whole-pool fair scheduling,
per-owner stream quotas, long-duration load, exhaustive private-destination forward
probes and disk-full diagnostic fault injection remain outside this acceptance.

RELEASE: source and evidence are stable for planner review. Keep existing bundles,
owner guests and services unchanged. Planner owns integration, publishing new
versioned assets and any later explicitly authorized deployment.
