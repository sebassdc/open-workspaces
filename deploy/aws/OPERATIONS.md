# Operating a host: `ow-aws`

`bin/ow-aws` is the operator helper for a host made by the `Host` component.
It runs on your laptop or in CI and reaches the host through SSM only (the host
has no inbound ports). The host side is `bin/ow-aws-remote.sh`, sent with every
command; nothing is installed on the host.

Needs bash, python3, the AWS CLI with the
[Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html),
and AWS credentials and region for the host's account in the environment. For
`ssh-config`, also EC2 Instance Connect (`ec2-instance-connect:SendSSHPublicKey`).

```bash
export AWS_PROFILE=... AWS_REGION=us-east-1
deploy/aws/bin/ow-aws --host my-host ready
```

`--host NAME` (or `OW_HOST`) is the component name: the instance is found by
its tag `Name`. `OW_INSTANCE` skips the lookup; in CI the instance comes from
the stack's `instanceId` output. `OW_SSH_KEY` picks the private key (default
`~/.ssh/id_ed25519`; its `.pub` is what gets pushed).

## Commands

| Command | Does |
|---|---|
| `shell` | SSM shell on the host (then `sudo ow list`, `sudo ow shell BOX`) |
| `run 'COMMANDS'` | `COMMANDS` as root in bash on the host; prints the output, exits non-zero on failure |
| `ready [--wait S] [--images]` | waits (default 600 s) for cloud-init, the bootstrap's `ok` status, the `ow` user and the controller unit, then for the worker to answer, the runner to be online and the controller to listen on the tailnet address (once the host is on the tailnet). `--images` also waits for the developer image (`developerImages`) |
| `authorize BOX` | replaces `BOX`'s SSH key list with your public key |
| `ssh-config [BOX..] [-o FILE]` | writes an OpenSSH config (default `~/.ssh/NAME.conf`) with the host (`NAME`) and each box (`ow-BOX`); the host and box keys, read over SSM, are pinned in `FILE.known_hosts` with `StrictHostKeyChecking yes`, so a changed key is refused. Use `ssh -F FILE ow-BOX`. Authorize the box first |
| `restart [--worker]` | restarts the service: running boxes are hibernated, the unit restarts, the host waits until ready, the boxes resume. On hosts with `ow-host`, `ow-host restart --control-plane` (boxes keep running) or `--worker` |
| `stop` | hibernates the running boxes, remembering them, then stops the instance. Refuses unless the instance is running or stopped |
| `start` | starts the instance, waits for SSM and readiness, resumes the remembered boxes. A box that fails to resume stays remembered for the next start or restart |
| `join-tailnet --secret ARN` | joins the tailnet with the auth key in that Secrets Manager secret (the host role must be allowed to read it: `tailscaleAuthKeySecretArn`), restarts as above, starts the dashboard. The key goes to a root-only file under `/run` and is deleted; it is never printed |
| `image RECIPE` | `ready --images`, then `ow image build RECIPE` on the host's runner. The file must be named `NAME.sh` |
| `backups [--url S3URL] [OWNER/NAME \| ID]` | backed-up captures, oldest first: `uploaded  owner/machine  box  capture`. Default URL: the host's `backupUrl`. With a box, its newest capture is the last line. Restore one with `sudo ow backup restore` on a runner |
| `apply`, `upgrade [ARGS]` | `ow-host apply` or `ow-host upgrade` on hosts that ship `ow-host`; refused on others |
| `ci-preflight --stack S` | before `pulumi up`: if `pulumi preview` replaces this host's instance, hibernates its boxes (`ow down --hibernate`, which keeps them running if one cannot be hibernated) and stops the service. Refuses when the instance is neither running nor stopped |
| `ci-post-up --stack S [--image RECIPE].. [--max-class N]` | after `pulumi up`: starts a stopped host, waits until ready (up to 50 minutes: a new host builds from source), resumes remembered boxes, runs `ow-host apply` and `ow-host upgrade` where the host has them, then builds each image (an unchanged recipe is a no-op) |

Each SSM command has a local deadline, so an unreachable host ends the command
instead of hanging. SSM keeps at most 24,000 characters of a command's output.

## In CI

Run the helper from the release tag your stack pins (clone the repository at
that tag and check the commit):

```bash
ow=open-workspaces/deploy/aws/bin/ow-aws
"$ow" --host my-host ci-preflight --stack prod
pulumi up --stack prod --yes
"$ow" --host my-host ci-post-up --stack prod --image images/team.sh
```

The CI role needs `ec2:DescribeInstances`, `ec2:StartInstances`,
`ssm:SendCommand` (scope it to the host's tags), `ssm:GetCommandInvocation`
and `ssm:DescribeInstanceInformation`.

## Backup bucket: `ArtifactStore`

`ArtifactStore` creates a private bucket for `backupUrl`: public access
blocked, versioning on, a policy that denies requests without TLS, and
lifecycle rules. Objects under `PREFIX/catalog/` and `PREFIX/snapshots/`
expire after `expireDays` (default 30) and overwritten copies after
`noncurrentDays` (default 7); unfinished multipart uploads are dropped after a
day. No age rule covers anything else in the bucket, so a content store kept
under `PREFIX/store/` is never aged out.

```typescript
import * as ow from "@sebassdc/open-workspaces-aws";

const store = new ow.ArtifactStore("my-host-backups", { bucketName: "my-host-backups-123456789012", prefix: "my-host" });
const host = new ow.Host("my-host", { backupUrl: store.url /* s3://bucket/my-host */ });
```

`ow-backup` (installed with `backupUrl`) still needs a scheduler: run it
nightly, for example with a systemd timer installed through `ow-aws run`. The
root disk does not survive a host replacement, so reinstall the timer after
one.
