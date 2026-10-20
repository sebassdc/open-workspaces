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
   `jti`, and `ow_box`, `ow_owner`, `ow_generation` claims. The key is a P-256
   key created on first use in the controller data directory (owner-only file).
2. **Issuer.** The gateway serves `/.well-known/openid-configuration` and
   `/.well-known/jwks.json` without sign-in when per-box access is configured. The
   operator registers that issuer as an IAM OIDC provider with audience
   `sts.amazonaws.com`. AWS accepts ES256 tokens from OIDC providers.
3. **Delivery.** The worker gains one operation, `aws-token`, which writes the token
   to `/run/ow/aws/token` and `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` and
   `AWS_ROLE_SESSION_NAME` to `/run/ow/aws/env` in a running guest, over the existing
   serial command channel (echo off, the command prints nothing), and a static
   `/etc/profile.d/ow-aws.sh` hook. `/run` is tmpfs: no secret is written to the
   box's disk. The operation never wakes a hibernated box and does not count as
   activity for idle hibernation. The SDKs and CLI call
   `AssumeRoleWithWebIdentity` themselves and re-read the file when they refresh.
4. **Generation.** A box generation is the CredentialBroker `WorkloadIdentity`:
   box ID plus a counter in the new `aws_roles` table. Attaching a role, a move or
   wake to another placement epoch, and re-attaching advance it; the role session
   name is that identity's hashed `SourceIdentity` (`ow-<32 hex>`), so CloudTrail
   separates incarnations. Tokens are issued only for the current placement.
5. **Authority.** Off unless the operator writes `aws-identity.json` (issuer and
   role allow-list, optionally per owner email). The owner or an `admin` grantee
   attaches one allowed role; `use` grantees cannot. An attachment is pinned to the
   owner who made it: a box whose owner changes has no role until its new owner
   attaches one. Forks get a new box ID and no row, and the refresher removes the
   token files a full-state fork copied from its parent. Removing a role from the
   allow-list stops issuance and clears the guest.
6. **Refresh.** A controller thread checks attached boxes every minute and
   re-issues when less than 10 minutes remain or the incarnation changed. Each box
   is handled under the catalog lock on its own, serialized with lifecycle and API
   calls.

## Consequences

- No AWS credential exists on the controller or runner hosts for this feature; AWS
  verifies each token against the published JWKS and the role's trust policy.
- A token is a bearer credential until it expires. Guest root, a copied snapshot
  or a fork's RAM can hold one for up to 15 minutes, and STS sessions already
  issued from it last for the role's session duration (1 hour by default). Detach,
  owner change and allow-list removal stop new tokens and remove the files; they do
  not revoke sessions already issued. Trust policies should pin `sub` to one box or
  one owner, and roles should use short maximum session durations.
- The trust boundary in AWS is the trust policy, not the allow-list: the allow-list
  stops users choosing arbitrary roles, and the `sub` condition stops a box using a
  role meant for another box or owner even if the allow-list is too broad.
- The issuer documents must be reachable by AWS without Cloudflare Access; the
  operator adds an Access bypass for the two paths or publishes them elsewhere.
- Runners older than this release lack `aws_identity_v1`; the role shows "not
  delivered" until the runner is upgraded (class 2, worker).
- Key rotation is not automated and the JWKS holds one key. Replacing the key file
  makes tokens already in boxes fail verification until the refresher replaces
  them (within about 6 minutes); how fast AWS re-reads a changed JWKS is not verified here.
  Session tags, several roles per box and Mac runners are not supported.

## Rejected

- **Host-side AssumeRole (the dormant broker as is).** Needs host AWS credentials
  able to assume every box role and a new guest channel; a compromised host could
  act as any box.
- **Container-credentials or metadata endpoint inside the guest network.** Needs
  per-box listeners in the filtered egress path and host AWS credentials; the
  token-file chain is supported by every current AWS SDK and the CLI.
- **Long TTL with revocation lists.** AWS cannot consult a revocation list for OIDC
  tokens; a short TTL is the only bound.
