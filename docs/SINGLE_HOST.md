# Single-host mode and SSH runners

Two commands make a self-hosted pool without hand-written join files or
certificates:

- `ow host up` runs the controller and a local runner on one Linux machine.
  This is the default single-host mode.
- `ow node add ssh://user@host` turns any SSH-reachable Linux machine into
  another runner of that controller. `ssm://` and `tailscale://` targets reach
  it through AWS Systems Manager or a tailnet instead.

They compose the existing pieces from [the multi-node prototype](MULTINODE.md):
`node-controller`, the private rootless worker, single-use join files and the
outbound `node-agent`. Nothing new is exposed to guests.

## Requirements

Every runner, including the controller machine, needs Linux x86-64, read/write
`/dev/kvm` for the SSH user, a Btrfs data directory and the host tools that
`ow host doctor` checks (`ip`, `nft`, `unshare`, `cp`, `curl`, `sha256sum` and
unprivileged user namespaces). SSH runners also need `flock`. The controller machine also needs `openssl` and
the runtime assets (`scripts/setup-local.sh`, or `OW_ASSET_DIR`).

## Start the controller

```sh
ow host up                      # foreground; Ctrl-C stops controller and agent
ow --data-dir /data/ow host up  # base directory on Btrfs, shorter than 52 bytes
```

`host up` checks the machine with `ow host doctor`, starts the local worker, and
creates a private CA in `~/.ow/tls` on first use. It then starts the controller
and enrolls the local runner, named after the hostname (`--node` to change it).
Budgets default to 2048 MiB, 2 running guests and 2 vCPUs per runner
(`--memory`, `--slots`, `--cpus`). Boxes idle for 30 minutes hibernate and wake
on the next connection (`--idle-minutes`, 0 to turn off; `ow node add` takes the
same flag). See [wake on connect](DAILY_USE.md).

The controller listens on this machine's Tailscale IPv4 address at port 8790.
Without a running Tailscale it listens on loopback only, so it is never on a
public port by default. Use `--listen ADDR:PORT` to choose another address.
Runners dial the tailnet DNS name (`https://NAME.TAILNET.ts.net:8790`) and trust
only the saved CA.

Stopping `host up` stops the controller and local agent. The worker and its
guests keep running, as with `ow up`, and the next `host up` reuses them.
`ow --data-dir ~/.ow/runner down` stops the local worker. For a service, run
`ow host up` under systemd with `Restart=on-failure`.

Layout under the base directory (default `~/.ow`, mode 0700):

| Path | Content |
| --- | --- |
| `controller/` | catalog, node sockets, controller lock |
| `runner/` | local worker, guests, local node credential |
| `tls/` | private CA and controller certificate (keys mode 0600) |
| `host.json` | controller URL that `node add` hands to runners |

## Add a runner over SSH

With `ow host up` running, on the same machine:

```sh
ow node add ssh://dev@build-box
ow node add ssh://dev@10.0.0.7:2222 --name gpu-1 --memory 8192 --slots 4 --cpus 4
ow node add dev@build-box --remote-dir /mnt/btrfs/ow --ubuntu
ow node list
```

`node add` uses the system OpenSSH client, so `~/.ssh/config` aliases, keys and
agents apply. It:

1. checks the remote is Linux x86-64 with read/write `/dev/kvm` and that the
   remote directory (default `~/.ow-node`) is on Btrfs;
2. optionally joins a tailnet (below);
3. installs `ow`, Firecracker 1.17.0, the kernel, the Alpine image, the guest
   agent and slirp4netns into the remote directory. Each file is checked by
   SHA-256 after transfer; identical files are skipped. `--ubuntu` also sends
   the Ubuntu developer image (about 8 GiB);
4. runs `ow host doctor` there;
5. mints a single-use, ten-minute join file in the controller catalog and
   writes it to the remote with mode 0600, together with the controller CA;
6. writes `start.sh` and runs it: it starts the worker with the chosen budgets
   and launches `ow node-agent` in the background. The agent redeems the join
   file, deletes it and keeps only the node credential;
7. waits until the controller sees the runner online, then reports it.

Nothing runs as root except the optional Tailscale step.

Re-running `node add` for an enrolled runner never enrolls it twice:

- agent running: nothing is restarted; it reports the runner online;
- agent stopped (for example after a reboot): `start.sh` starts the worker and
  agent, and success requires a fresh heartbeat from the new agent;
- other `--memory`, `--slots` or `--cpus` than the enrolled ones: refused.
  `node add` does not change an enrolled runner's capacity. Revoke it and add
  the machine under a new `--name` and `--remote-dir` instead.

`start.sh` itself starts no second agent when one is running. After a reboot of
the runner, run `~/.ow-node/start.sh` there, or `ow node add` again.

The installed binary is `bin/ow-linux-amd64` from the asset directory when it
exists (static musl build from `scripts/build-local.sh`), otherwise the running
`ow`. Use `--ow-binary` when the runner's C library differs.

### Connect over AWS SSM or Tailscale

The target's scheme picks the [connect provider](CORE.md#connect-providers).
All three drive the system OpenSSH client; SSM and Tailscale only carry the
connection, so every step above is the same:

```sh
ow node add ssm://ubuntu@i-0123456789abcdef0      # no inbound port on the instance
ow node add tailscale://dev@build-box.tail1234.ts.net
```

- `ssm://` tunnels through AWS Systems Manager (`AWS-StartSSHSession`). It needs
  the AWS CLI with the Session Manager plugin on this machine, IAM permission
  for `ssm:StartSession` on the instance, and the instance's SSM agent online.
  Region and profile come from `AWS_REGION`/`AWS_PROFILE`. SSH login still needs
  a key the instance accepts; for an EC2 instance without one, push yours for 60
  seconds first:
  `aws ec2-instance-connect send-ssh-public-key --instance-id i-… --instance-os-user ubuntu --ssh-public-key file://$HOME/.ssh/id_ed25519.pub`.
  The default runner name is the instance ID.
- `tailscale://` tunnels through `tailscale nc` over the tailnet, so this machine
  must be on the same tailnet. With Tailscale SSH enabled on the runner
  (`tailscale up --ssh`) the tailnet identity is the login; otherwise its sshd
  checks your key as usual.

### Join a tailnet

```sh
ow node add ssh://dev@build-box --tailscale-auth-key file:/home/me/.config/ow/ts-key
TS_KEY=$(aws secretsmanager get-secret-value --secret-id SECRET_ID \
  --query SecretString --output text) \
  ow node add ssh://dev@build-box --tailscale-auth-key env:TS_KEY
```

The key comes only from a mode-0600 file or an environment variable and never
appears in arguments, logs or saved files. On the runner it is written to a
private temporary file that is removed afterwards. If `tailscale` is missing it
is installed with the official script; `tailscale up` then runs with
`--hostname` set to the runner ID. Both steps use `sudo -n`, so the SSH user
needs passwordless sudo for them. A runner already connected to a tailnet is
left as it is. Use a tagged, pre-authorized key; tags and ACLs are managed in
the tailnet admin console.

## Health

`ow node list` prints every runner with `online` true when its agent sent a
heartbeat in the last 15 seconds. Revoke a runner with
`ow --data-dir ~/.ow/controller node-revoke ID`; revoked IDs cannot be reused.

## Backups

`ow backup catalog --output FILE` copies the live controller catalog;
`ow backup snapshots --to s3://bucket/prefix` uploads the runner's sealed
snapshots and `ow backup restore` brings one up on another runner. See
[backups](CORE.md#backups) and [snapshot store](DAILY_USE.md#snapshot-store).

## Limits

- The controller is a single process with a SQLite catalog. The human dashboard
  still needs its Cloudflare Access config (`ow dashboard`); `host up` does not
  start it.
- One credential per runner; a stolen credential can act as that runner until
  revoked. Enrollment trust is the controller's private CA.
- The remote must accept a POSIX `sh -c` command string over SSH.
- Evidence so far: unit tests, and an end-to-end run in a privileged x86-64
  container over real OpenSSH: private-CA TLS, real worker and agents, both
  runners online, re-run restarts, join file consumed, Tailscale step with a
  stand-in `tailscale`. That container had a stand-in `/dev/kvm` and booted no
  guest. A run on real KVM hardware and a real tailnet is pending. The ignored
  test `selfhost::tests::host_up_then_node_add` repeats the run on a KVM host.

```sh
OW_NODE_TEST_BINARY=$PWD/target/debug/ow OW_ASSET_DIR=/path/to/assets \
  OW_SELFHOST_TEST_DIR=/short/btrfs/dir cargo test -p ow -- --ignored selfhost
```
