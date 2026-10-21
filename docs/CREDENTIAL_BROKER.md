# CredentialBroker

**Web-identity mode is live (0.9.37, opt-in):** [per-box AWS access](AWS_ACCESS.md)
reuses this module's `WorkloadIdentity` (physical box ID plus generation, hashed
`SourceIdentity` as the role session name) and its exact role-ARN validation. The
controller signs OIDC tokens and the guest's AWS SDK calls
`AssumeRoleWithWebIdentity`, so no host AWS credential or guest-to-host channel is
needed ([ADR 0008](adr/0008-per-box-aws-web-identity.md)). **Metadata mode is live
too (0.9.40, opt-in):** an AWS-hosted controller calls `AssumeRole` with its
instance role and runners serve the box's credentials at the instance metadata
address ([ADR 0009](adr/0009-per-box-aws-metadata-endpoint.md)). The host-side
`AssumeRole` and secret-write broker described below remains dormant.

**Implemented:** a dormant host-side library in
`ow_core::credential_broker`, with fake-provider integration tests. It is not
installed in the worker, controller, CLI, guest images or network listeners.
There is no live AWS adapter, guest credential endpoint or secret-read API.

The library follows the existing `ow-core` trait pattern. A trusted host resolves
a request's channel to an exact **physical box ID and generation**, then the
broker selects an explicit, unexpired profile grant. Guest JSON cannot supply
identity, time, role ARN, policy, secret ARN or AWS endpoint. Empty policy denies
all requests. There is no fallback to a host role or owner-wide grant.

## Contract

`WorkloadAuthorizer` resolves host-owned channel handles on every request.
`RoleAssumer` receives all parameters for AWS STS `AssumeRole`.
`SecretWriter` receives only a permitted, pre-created AWS Secrets Manager ARN,
a deterministic version token and the supplied value. `CredentialBroker::handle`
connects these interfaces; see `crates/ow-core/tests/credential_broker.rs` for an
executable example using all three fakes and the public request parser.

The request shapes are:

```json
{"op":"aws_credentials","profile":"project","duration_seconds":900}
```

```json
{"op":"put_secret","profile":"project","write_id":"0123456789abcdef0123456789abcdef","value":"synthetic-example-only"}
```

`put_secret` means **upload only**: a workload explicitly granted write authority
may replace a value at its fixed secret destination. The response is only
`Response::SecretWritten`. This is not a general secret injection, retrieval,
listing or account-linking service. New secret resources, rotation schedules,
provider consent and injection into workloads are outside this slice. An owner
upload API would need its own authenticated owner authorization; this workload
endpoint must not be reused as one implicitly.

`aws_credentials` returns access key ID, secret access key, session token and
Unix expiration. The secret-bearing types intentionally do not implement
`Serialize` or `Clone`; `Debug` redacts values, and transport adapters must call
`SecretValue::expose` deliberately. Provider and authorizer error chains are
replaced by fixed errors before crossing the broker boundary. Parsing errors
also discard offending field names/values. The broker does not log, journal,
cache, retry or persist credentials or submitted secrets. Rust strings and
incoming JSON buffers are **not zeroized** by this implementation.

## Policy

[The synthetic policy fixture](examples/credential-policy.json) is loaded by the
integration tests. It contains no real account or deployment configuration.
`Policy::from_json` validates a maximum 64 KiB policy with at most 256 grants.
`CredentialBroker::new` validates direct Rust construction too. Unknown fields
and duplicate `(box ID, generation, profile)` grants are refused.

Each grant has a mandatory expiry and either or both of:

- An exact IAM role ARN, a maximum requested duration of 900–3600 seconds, and an
  inline session scope. The scope accepts explicit actions and exact resource
  ARNs only, with at most 16 statements, 32 actions/resources per statement and
  2048 bytes of generated JSON. Wildcards and policy variables are refused.
- A complete, pre-created Secrets Manager ARN for writes (`secret_arn`), paired
  with its expected `secret_name`. Both fields are required together. The resource
  must equal `secret:<expected name>-<six alphanumeric characters>`. The name must
  be 1–512 characters from AWS's allowed alphabet (`A–Z`, `a–z`, `0–9`, `/_+=.@-`).
  Pinning the name matters: a partial ARN ending in `-output` already looks like
  it has a six-character suffix. Name-only destinations, partial ARNs relative to
  the expected name, wildcard destinations and create-secret behavior are refused
  before provider dispatch. Populate the pair from the approved provisioner's
  actual Name/ARN response; syntax validation alone cannot establish existence or
  provenance. Policies with only `secret_arn` are now refused. See AWS's
  [secret ARN format](https://docs.aws.amazon.com/secretsmanager/latest/userguide/whats-in-a-secret.html).

This initial policy language deliberately omits conditions, managed policy ARNs,
resource patterns and resource-less AWS actions. Extend it only for a concrete,
reviewed capability. Resource-based grants can also affect effective AWS permissions. Syntactic
validation is not an IAM policy simulator:
administrators must review the action/resource pair, target role permissions,
trust, resource policies, boundaries, organization restrictions and KMS access.
Do not grant IAM administration, role chaining or secret readback implicitly.
Actual deployment values/configuration belong in the consuming private repo,
not this public mechanism or fixture.

The broker sends the configured inline scope unchanged along with a stable,
hashed box-generation `SourceIdentity` and role session name. AWS sessions are
bounded by the request and the grant's expiry. Requests below 900 seconds or
above the profile cap are rejected, not clamped. The host clock is sampled again
after the provider returns; expired, overlong, malformed or empty sessions and
backwards clock movement are refused. Provider latency cannot extend a session
past the grant expiry. A provider call may have succeeded even if its response
was refused or lost; the broker does not silently repeat it.

For secret writes, the supplied write ID must be 32–64 ASCII alphanumeric or
hyphen characters; values must be 1–65536 UTF-8 bytes. The provider receives a
64-character SHA-256 `ClientRequestToken` scoped to box generation, profile,
target and write ID. An uncertain write may be retried with **the same ID and
value**. The adapter must preserve AWS's same-token/same-value replay and
same-token/different-value rejection. The fake proves that plumbing; live AWS
behavior has not been exercised. New IDs create new versions, so the future host
service needs per-box rate/concurrency limits before enabling writes.

AWS's [AssumeRole contract](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRole.html)
defines the duration floor, inline policy limit and source identity. Session
permissions intersect the role permissions; this library uses a conservative
one-hour cap suitable for role chaining. The
[PutSecretValue contract](https://docs.aws.amazon.com/secretsmanager/latest/apireference/API_PutSecretValue.html)
defines idempotent version tokens and warns about rapid version creation.

## Required host integration before activation

These are requirements for the next adapter slice, not claims about today's
runtime:

1. Use a dedicated authenticated guest-to-host channel with a host-owned mapping
   to the catalog's globally unique physical box ID and a durable, fenced
   generation. Do not use guest-supplied box names, source IP alone, a copied
   bearer file or host instance credentials as workload identity. Guests must
   remain unable to reach host metadata, management sockets and AWS host roles.
2. Resolve the current mapping for every request and deny stopped, deleted,
   revoked or shared workloads. Advance the generation and invalidate channels
   on fork, restore, reassignment, sharing/unsharing, ownership change and
   credential-authority changes. Forks get no inherited broker grants. This
   generation store and these lifecycle hooks do not exist in this slice.
3. Serialize the whole authorization/provider/response turn with lifecycle and
   policy changes, using the existing host's exclusive operation boundary or a
   dedicated lease. A resolve-then-issue race must not issue for a stale owner or
   generation. The library borrows an immutable policy for one broker lifetime;
   adapters must replace it atomically and release obsolete broker instances.
4. Use host time, a bounded request body (512 KiB before JSON decoding), private
   policy storage and an approved fixed HTTPS AWS endpoint. `from_json` limits
   already-buffered input; the transport must bound allocation before buffering.
   Never place secrets in argv, environment dumps, operation journals, traces,
   access logs, response caches or the controller's durable result table. Return
   credentials only to the exact requesting authenticated channel. Record only
   box generation, profile, operation type, outcome and expiry for audit.
5. Implement `RoleAssumer` and `SecretWriter` using a reviewed host credential
   source and transport. Forward role, inline policy, duration, source identity,
   session name, secret ARN and replay token exactly. Do not inherit arbitrary
   endpoint/profile overrides. Disable automatic AssumeRole retries; reconcile
   uncertain secret writes using their original token. Apply request/time/rate
   limits and test denial and lost-response behavior before enabling the service.

Removing grants or rejecting stale channels blocks future issuance. It does not
revoke AWS sessions already handed to a box. Guest root can copy temporary
credentials into disk, logs, memory and snapshots until they expire. A full-state
fork may copy still-valid sessions even though it receives no new broker grant.
Neither this broker nor deleting known credential files sanitizes a snapshot.
Sharing/restore admission and IAM revocation/incident response must account for
that residual exposure before credentialed boxes are admitted.

## Activation gates and exact operator steps

No step below has been run. This library is complete as a credential-free slice;
activation is a separate release/deployment with its own acceptance.

1. **Integration/code gate:** implement and review the channel, fenced generation,
   lifecycle hooks, bounded response encoding, host providers and rate limits
   above. Run host/guest negative tests with fake AWS first. Keep disabled by
   default. Approve the upgrade impact before touching a host.
2. **IAM/spend gate (human):** approve the AWS account/region, exact resources,
   requested actions, session TTL and spend limits. In the consuming deployment
   repo, prepare a reviewed IaC diff creating a dedicated target role and its
   least-privilege permissions. Permit only the intended host broker principal
   to call `sts:AssumeRole` and `sts:SetSourceIdentity`, constrain source identity
   to the intended workload identities, and deny arbitrary role selection. Never
   widen the existing host's account-wide rights as a shortcut. Apply only after
   the named approval.
3. **Secrets/consent gate (human):** approve secret ownership and overwrite
   semantics, create dedicated Secrets Manager resources with approved KMS and
   retention, and grant the broker only required writes to their exact ARNs.
   Keep any unavoidable KMS/rotation rights narrowly scoped and reviewed. Enter
   real secret values only through the approved secure path after consent;
   never commit them or copy long-lived access keys into a box. No OAuth/admin
   consent or third-party messages are implied by this task.
4. **Deployment gate:** store actual role/secret ARNs and per-box policy in the
   consuming deployment repo's approved private configuration. Plan the rollout
   using [per-unit upgrade classes](UPGRADES.md). This dormant library alone
   activates no service and needs no restart. A future separate control-plane
   adapter may be class 1 only if its real diff leaves worker interfaces alone;
   a worker/channel change requires class 2 and the normal targeted
   hibernate/resume gate. A host restart/replacement must be justified explicitly,
   never bundled merely to install policy. Never reuse an existing release tag.
5. **Live acceptance gate:** use a disposable explicitly granted box. Verify its
   caller identity and allowed action; denied role/action/resource, another box,
   stale generation and expired/revoked grant; secret replay/conflict and no read
   route; safe error/log output; and session expiry after grant removal. Capture
   the actual role/session audit evidence without values. Check unrelated boxes
   and their running workloads stayed up. Do not claim snapshot sanitization or
   instantaneous credential revocation.

## Validation

```sh
cargo test --locked -p ow-core --test credential_broker
cargo test --locked -p ow-core
```

The tests use synthetic in-memory providers and the checked-in example policy.
They exercise authorization-to-provider dispatch, scope/duration, clock/provider
expiry, grant removal, cross-box/generation denial, redaction and write replay.
They do not prove live IAM, a running VM/channel, concurrent host lifecycle
fencing, AWS credential delivery or deployment safety.
