# Interactive terminals

The dashboard's **Open Terminal** button creates a separate Linux PTY inside the
selected running Firecracker VM. Both the guest agent and WebSocket gateway are
Rust. The agent uses libc's `openpty`, `setsid`, `TIOCSCTTY` and `TIOCSWINSZ`;
it does not implement a hypervisor or require an SSH daemon. The browser uses
MIT-licensed xterm.js 5.5.0 and addon-fit 0.10.0, vendored locally with their
licenses and verified npm tarball integrity. No external CDN is required.

## Use

Build with `bash scripts/build-local.sh`. This also builds the static musl guest
agent into ignored runtime assets. Start the worker and select a running machine
in the authenticated dashboard, then choose **Open Terminal**. Exit or Disconnect
ends that terminal session. Reopening creates a fresh shell. Multiple sessions
have separate PTYs and shell environments.

`./ow shell MACHINE` uses the same guest PTY over the local owner-only Unix
control socket. It requires an interactive local terminal, forwards raw input
and window size, restores terminal attributes on normal/error return, and returns
the guest shell's exit status. This command is still local to the worker machine.
Remote CLI login/WebSocket authentication, SSH and editor Remote-SSH remain pending.

## Transport and policy

```text
Browser/xterm.js <-> authenticated WSS /api/terminal/MACHINE
  <-> Rust gateway <-> owner-only Unix socket <-> isolated worker
  <-> dedicated guest TCP connection <-> Rust guest agent <-> guest PTY/shell
```

JWT signature, issuer, audience, subject, email allowlist, expiry and Host checks
precede upgrade. The exact configured HTTPS Origin is required even for GET;
cross-origin and missing-Origin upgrades are denied. URLs cannot carry tokens,
host paths or client-selected ports. The existing Cloudflare hostname, Access
application and tunnel are reused. Admitted emails remain operators of the whole
sandbox; per-workspace multi-user roles are not implemented.

Each agent accepts one connection from its workspace's worker-side TAP address,
opens a PTY and runs `/bin/sh -i` as guest root. The worker installs the hash-checked
static binary into guest `/run` on demand through a temporary, address-restricted
transfer listener in its isolated network namespace. The agent carries no host
credential, Access JWT, account token or management socket. Guest root controls
its own agent and shell; authentication and quotas remain host-side. Workspaces
now have filtered IPv4 Internet egress; peer VM routing remains blocked.
See [networking and remote CLI login](NETWORKING.md).

Management exec retains its independent serial console. Terminal output/input
does not enter that command protocol or its serial log. Gateway limits and guest
queues bound buffering; processing acknowledgments from xterm.js provide output
backpressure. Disconnect kills processes still belonging to the terminal's Linux
session using guest pidfds, then reaps the shell. Processes that intentionally
create a separate session, such as a detached tmux server, are not covered by that
cleanup. Alpine does not include tmux; the developer Ubuntu/Arch profiles include it.
Those profiles open interactive shells as UID 1000 `dev`, with guest-only
passwordless sudo, while the internal terminal agent stays privileged inside
the guest to manage its PTY and session cleanup.

## Wire contract and bounds

- Browser binary messages: raw terminal input, at most 4,096 bytes per message.
- Browser text messages: strict JSON `{"type":"resize","cols":80,"rows":24}`
  or `{"type":"ack","bytes":N}`. Sizes are 1–500 columns and 1–300 rows;
  acknowledgments cannot exceed outstanding output bytes. No arbitrary operations.
- Server binary messages: terminal output; server text: `{"type":"exit","code":N}`.
- Internal agent frames: one type byte, big-endian u32 payload length, payload.
  Type 0 carries bytes; type 1 carries big-endian u16 columns/rows; type 2 carries
  a big-endian i32 exit status. Internal frames are bounded to 4,096 bytes.
- At most 16 active terminals in the gateway and worker. Sessions close at JWT
  expiration or one hour, whichever comes first. The agent has its own one-hour
  lifetime. Ping/Pong checks close stale clients after 90 seconds; writes time out.
- The gateway pauses output reads around a 64 KiB unacknowledged window, with a
  bounded 16-frame upstream channel. Guest input/output queues are bounded around
  64 KiB, and the browser rejects input that would exceed its 64 KiB send buffer.
- Disconnect/reconnect does not promise session persistence. Hibernate, restore,
  cold stop and forks require terminal reconnection; saved RAM can contain PTY
  processes and stale sockets. External WebSocket connections are not cloned.
- Access policy changes are not pushed into already upgraded sockets; expiration
  bounds an existing session. Immediate session revocation remains pending.

## Observed validation

The real-VM Playwright regression runs Chromium over HTTPS/WSS through a local
TLS terminator and checks actual guest TTY descriptors, persistent cwd/environment,
resize, Ctrl-C, Ctrl-Z/fg, vi editing, heavy-output interruption, exit status 42,
disconnect cleanup and management exec while a PTY is attached. It also runs the
existing RAM/disk snapshot, fork, hibernation and restore regression. Synthetic
RSA assertions are limited to that test router; there is no production auth bypass.
JWT/Origin denial tests cover the terminal endpoint. An actual authenticated owner
WebSocket through Cloudflare remains an owner browser acceptance check.

```bash
bash scripts/build-local.sh
python3 experiments/runtime-spike/terminal-cli-test.py
export RUSTUP_HOME="$PWD/data/runtime-spike/rustup"
export CARGO_HOME="$PWD/data/runtime-spike/cargo"
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked dashboard_browser_real_vm -- --ignored --nocapture
```

Build/runtime verification remains a local prototype claim. Jailer/cgroups,
multi-user roles and production hardening are separate gates.

Upstream references: [Linux openpty](https://man7.org/linux/man-pages/man3/openpty.3.html),
[Axum WebSocket support](https://docs.rs/axum/0.8.9/axum/extract/ws/index.html),
[xterm.js flow control](https://xtermjs.org/docs/guides/flowcontrol/).

Remote `ow login`, API operations and `ow shell` over verified HTTPS/WSS are now implemented. The dashboard includes a copyable curl installer for Linux x86-64; see [setup and limits](NETWORKING.md).

WSS terminal names are resolved through the authenticated user's SQLite
ownership mapping before opening the worker connection. See [user isolation](USERS.md).
