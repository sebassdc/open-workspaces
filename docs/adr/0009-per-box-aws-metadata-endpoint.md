# ADR 0009: per-box AWS roles through a box metadata endpoint

Status: implemented (release 0.9.41), opt-in with `"mode": "metadata"` in
`aws-identity.json`. Supersedes [ADR 0008](0008-per-box-aws-web-identity.md) for
controllers that run on EC2 with an instance role; web identity stays the default and
the choice for controllers outside AWS. Live acceptance against a real AWS account is
pending; see [per-box AWS access](../AWS_ACCESS.md#real-aws-acceptance-pending).
Pattern: the [CredentialBroker](../CREDENTIAL_BROKER.md) workload identity (box ID
plus generation, hashed source identity, exact role ARNs), ADR 0007's placement
epochs, and the worker network policy of [networking](../NETWORKING.md).

## Context

ADR 0008 makes the controller an OIDC issuer: AWS must fetch its discovery
document and JWKS from a public HTTPS URL (an Access bypass or a separate bucket),
the account needs an IAM OIDC provider, a JWKS holds at most 99 box keys, and the
environment variables it exports in every login shell come before the shared
credentials file in botocore, the AWS CLI, Go v2 (and so Terraform) and Java v2: a
user who keeps their own keys in `~/.aws/credentials` silently gets the box role
instead. ADR 0008 rejected host-side `AssumeRole` because it needed host AWS
credentials and a new guest channel. An AWS-hosted controller already has an
instance role, and the worker already owns each box's network path.

## Decision

1. **Mode.** `aws-identity.json` gains `"mode": "metadata"` (default
   `"web-identity"`). It has no issuer. Every allowed role must be under the IAM
   path `/ow-box/`, all in one account (the controller's own: the controller also
   checks against its instance identity), and name its `owners` (an empty list
   would mean every user). The file is re-read on every request and refresher
   pass, and an edit wakes the refresher, so switching modes takes effect within
   seconds without a restart.
2. **Who calls STS.** The controller calls `sts:AssumeRole` with its EC2 instance
   role, from an STS client whose credentials provider is the instance metadata
   provider alone (no default chain, no CLI): no `AWS_*` variable or `~/.aws` file
   on the controller host changes the principal. Each session has the box's hashed
   generation identity (`ow-<32 hex>`) as `RoleSessionName` and `SourceIdentity`,
   transitive session tags `ow-box` (box ID) and `ow-owner` (owner email, lower
   case), and lasts one hour. Runners hold no AWS credentials for this.
3. **Refresher.** Every minute (and at once after an attach, a start or restore, a
   new worker process or a config edit) the controller collects, under the
   catalog lock and one box at a time, what each attached box needs; it calls STS
   with the lock released; it re-locks and delivers only if the box's role, owner,
   generation, hop limit and runner are unchanged, otherwise the credentials are
   dropped unseen. The CLI/dashboard attachment handler also performs this unlocked
   STS/delivery step synchronously for a running or hibernated box: success requires
   the same attachment to be active and its worker to confirm credentials present.
   STS/delivery failure or concurrent replacement returns an error. Stopped and
   parked attachments remain pending without a wake. Sessions are renewed once less than 30 minutes remain, for
   running and hibernated boxes, so a wake never waits for STS. Stopped boxes get
   none until they run.
4. **Delivery.** The worker operation `aws-creds` (capability `aws_metadata_v1`)
   receives `{role, generation, hop limit, keys, expiry, region}`, a pending marker
   without keys, or a withdrawal. Delivery is push only: no runner can ask for a
   box's credentials, and a worker refuses a box it does not host or whose
   placement epoch is stale. The worker keeps them in memory only (no `Debug`,
   wiped when dropped; the process turns non-dumpable once it holds any), drops the
   keys when the box stops (keeping a pending marker), and forgets them when the box
   is deleted or exported to another runner.
5. **Serving.** One HTTP/1.1 listener per worker, on its own threads and its own
   read-write state, never the worker's runtime lock (SDKs give instance metadata one
   second, botocore and the CLI one attempt). In the worker's network namespace nft
   adds `prerouting: iifname "ow*" ip daddr 169.254.169.254 tcp dport 80 redirect`
   to the listener, which therefore receives connections on the TAP's own host
   address; the `input` chain keeps accepting any port on a box's own host address
   (the guest agent installer uses an ephemeral one) and drops the listener's port
   on every other interface, `tap-egress` included. The box is the connection's
   peer address, which the existing anti-spoofing rule binds to its TAP; the peer
   must be a guest `.2` and the local end the host `.1` of the same /30. IMDSv2
   only: tokens are an HMAC over box, a nonce new with every VMM start of it, the
   role generation (0 without a role) and the expiry, so a detach, re-attach, move
   or fork ends the tokens issued before it, with a key random per worker process.
   Tokens need no memory and are compared in constant time. The catalog retains a
   per-box generation high-water mark when an attachment is deleted; both modes
   allocate above the active and deleted generations, including after transfer or
   controller restart. Requests carrying `X-Forwarded-For`, a body or
   `OPTIONS` are refused and no CORS header is ever sent. The real instance
   metadata service is never contacted. Paths: the token, the role name and
   credentials, `iam/info` with a synthetic `instance-profile/ow-box/...` ARN, and
   the region and zone; everything else is 404. A box without a role gets 404 at
   once; a box whose role is attached but whose keys have not arrived waits up to
   800 ms. Per box: 8 connections, 100 requests per second (burst 200), 4 KiB of
   headers within one second, five idle seconds, and answers with an IP TTL of
   its hop limit (1 by default, so containers inside the box cannot reach it; 2 on
   request).
6. **Withdrawal before commit.** Detach, re-attach, ownership transfer and
   allow-list removal withdraw (or replace) the runner's copy synchronously before
   the catalog change commits. While the runner may hold keys (the box is not
   stopped) and cannot be reached, transfer and detach are refused. Deleting a box
   goes through the worker's delete, which forgets them. A fork child is a new box
   ID: it gets nothing.
7. **Guests.** Nothing is written into the box. In metadata mode the refresher
   removes identity files an earlier web-identity mode left in running guests,
   which most SDKs would otherwise try before instance metadata.

## Consequences

- Every SDK, the AWS CLI and Terraform find the role exactly as on EC2, and only
  after everything the user configured: instance metadata is last in every
  credential chain (environment, profiles, SSO, `credential_process`, the shared
  credentials file and container credentials come first).
  `AWS_EC2_METADATA_DISABLED=true` turns it off in a shell. The region the endpoint
  reports becomes the default for users who set none.
- No public issuer, OIDC provider or 99-box limit. Trust lives in AWS: each role
  trusts only the controller's instance role, with its `ow-owner` (and, for a
  single box, `ow-box`) session tag pinned. The tags are set by the controller, so
  they guard against controller bugs and misconfigured allow-lists, not against a
  compromised controller, which ADR 0008 could not either (it held every signing key).
- Root on the controller host, or any local process that reads its instance
  metadata, can assume every `ow-box` role while the instance role allows it. The
  instance role should be allowed `role/ow-box/*` only (the AWS component's
  `boxRoles`), and IMDS on the controller host should be limited to the controller.
- Root on a runner sees the credentials of the boxes on it, as it sees their RAM.
- Credentials an SDK cached in guest RAM, and so in hibernation snapshots,
  full-state forks and recovery points, stay valid until they expire (at most one
  hour). Only IAM's "revoke active sessions" ends them earlier.
- Anyone who can run commands in the box (owner, `use` and `admin` grantees) can use
  its role.

## Rejected

- **Narrowing `input` to the listener's port.** Breaks the guest agent installer,
  which serves on an ephemeral port of the box's host address.
- **A listener per box, or one behind the worker's runtime lock.** More sockets in
  the egress path, or seconds of stall during forks and restores against a one-second
  SDK timeout.
- **Delivery on wake.** Puts STS on every resume path; keeping hibernated boxes'
  credentials fresh costs one STS call per box every half hour.
- **Shelling out to `aws sts assume-role`.** Reads the host's default chain, takes
  about half a second, and moves secrets through stdout.
- **Runners pulling credentials.** A compromised runner could then ask for boxes the
  controller never placed on it.
