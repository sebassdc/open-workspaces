# ADR 0008: per-box AWS access through controller-issued web identity

Status: implemented (release 0.9.37), off by default. Live AWS acceptance with a real
IAM OIDC provider and role is pending; see [per-box AWS access](../AWS_ACCESS.md).
Pattern: the [CredentialBroker](../CREDENTIAL_BROKER.md) workload identity (host-resolved
box ID plus generation, hashed session identity, exact role ARNs, no fallback grant),
ADR 0005's per-unit upgrade classes and ADR 0007's placement epochs.

## Context

Owners want a box to reach AWS as a role they choose, without copying long-lived
keys into it. The dormant CredentialBroker answers this with host-held AWS
credentials calling `sts:AssumeRole` per request over a guest-to-host channel that
does not exist yet. That would make every controller or runner host an AWS
principal able to assume every box's role, and it needs a new authenticated guest
channel before anything works.

## Decision

The controller is an OpenID Connect identity provider for its boxes:

1. **Token.** For a box with an attached role, the controller signs an ES256 JWT:
   `iss` = the operator's issuer URL, `sub` = `owner/<owner email>/box/<box ID>`,
   `aud` = `sts.amazonaws.com`, `iat`/`nbf`, `exp` 15 minutes later, a random
   `jti`, and `ow_box`, `ow_owner`, `ow_generation` claims. Each box generation
   has its own P-256 key, created when its first token is signed, in an
   owner-only `aws-identity-keys/` directory of the controller data directory.
2. **Issuer.** The gateway serves `/.well-known/openid-configuration` and
   `/.well-known/jwks.json` without sign-in when per-box access is configured. The
   operator registers that issuer as an IAM OIDC provider with audience
   `sts.amazonaws.com`. AWS accepts ES256 tokens from OIDC providers. The JWKS lists
   exactly the keys of current generations plus one anchor key that never signs
   (AWS needs at least one key before anything is attached).
3. **Delivery.** The worker gains one operation, `aws-token`, which writes the token
   to `/run/ow/aws/token` and `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` and
   `AWS_ROLE_SESSION_NAME` to `/run/ow/aws/env` in a running guest, over the existing
   serial command channel (echo off, the command prints nothing), and a static
   `/etc/profile.d/ow-aws.sh` hook. `ow exec` sources the env file itself for images
   without a `dev` user (Alpine runs a plain `sh -c`), and the terminal agent passes
   the variables to its shell. `/run` is tmpfs: no secret is written to the
   box's disk. The operation never wakes a hibernated box and does not count as
   activity for idle hibernation. The SDKs and CLI call
   `AssumeRoleWithWebIdentity` themselves and re-read the file when they refresh.
4. **Generation and invalidation.** A box generation is the CredentialBroker
   `WorkloadIdentity`: box ID plus a counter in the new `aws_roles` table.
   Attaching or re-attaching a role, a move or wake to another placement epoch, and
   a fork of the box (a child naming it as source, here or through a cross-runner
   transfer) advance it. Re-attachment withdraws the old key immediately, even
   while parked. Refresh also withdraws keys for parked, transferring or missing
   placements without contacting a worker. Otherwise, the old key is deleted when
   the new one is created, so **its tokens stop verifying in AWS**, including copies
   in a fork's RAM, a snapshot or anywhere else, as soon as AWS reads the new JWKS. Detach,
   owner change, allow-list removal and deleting the box delete the box's key. AWS
   cannot ask the controller which incarnation is current; removing the key from
   the JWKS is the only revocation an OIDC issuer has, so `sub` stays stable and
   trust policies need no update on a move. The role session name is the hashed
   `SourceIdentity` (`ow-<32 hex>`), so CloudTrail separates generations.
5. **Authority.** Off unless the operator writes `aws-identity.json` (issuer and
   role allow-list, optionally per owner email). The owner or an `admin` grantee
   attaches one allowed role; `use` grantees cannot. An attachment is pinned to the
   owner who made it: a box whose owner changes has no role until its new owner
   attaches one. Forks get a new box ID and no row, and the refresher removes the
   token files a full-state fork copied from its parent. Cleanup is repeated after
   restore or start operations, including those dispatched by an admin grantee,
   using the journal's physical box ID. Removing a role from the
   allow-list stops issuance, withdraws the key and clears the guest.
6. **Refresh.** A controller thread checks attached boxes every minute and
   re-issues when less than 10 minutes remain or the incarnation changed. Each box
   is handled under the catalog lock on its own, serialized with lifecycle and API
   calls.

## Consequences

- No AWS credential exists on the controller or runner hosts for this feature; AWS
  verifies each token against the published JWKS and the role's trust policy.
- A token of the current generation is a bearer credential: anyone with guest root
  can use it. A token of an old generation verifies only until AWS re-reads the
  JWKS (we serve it with `max-age=60`; how long AWS actually caches it is not
  documented and not verified here), and never past its 15-minute expiry. STS
  sessions already issued last for the role's session duration (1 hour by
  default); nothing here revokes them. Trust policies should pin `sub` to one box
  or one owner, and roles should use short maximum session durations.
- At most 99 boxes per controller can have a role at once: AWS reads at most 100 EC
  keys from a JWKS, one of which is the anchor. Attaching a hundredth is refused.
- The trust boundary in AWS is the trust policy, not the allow-list: the allow-list
  stops users choosing arbitrary roles, and the `sub` condition stops a box using a
  role meant for another box or owner even if the allow-list is too broad.
- The issuer documents must be reachable by AWS without Cloudflare Access; the
  operator adds an Access bypass for the two paths or publishes them elsewhere.
- Runners older than this release lack `aws_identity_v1`; the role shows "not
  delivered" until the runner is upgraded (class 2, worker).
- Keys rotate with every generation; deleting a key file by hand has the same effect
  as a rotation for that box until its next token. Session tags, several roles per
  box and Mac runners are not supported.

## Rejected

- **Host-side AssumeRole (the dormant broker as is).** Needs host AWS credentials
  able to assume every box role and a new guest channel; a compromised host could
  act as any box.
- **Container-credentials or metadata endpoint inside the guest network.** Needs
  per-box listeners in the filtered egress path and host AWS credentials; the
  token-file chain is supported by every current AWS SDK and the CLI.
- **Long TTL with revocation lists.** AWS cannot consult a revocation list for OIDC
  tokens; a short TTL is the only bound.
