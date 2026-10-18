//! Dormant host-side credential brokerage. No transport or real AWS adapter is installed.
//! See docs/CREDENTIAL_BROKER.md for the trusted adapter and lifecycle contracts.
use crate::common::identifier;
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

/// Host-resolved physical box identity, never an owner name or a guest assertion.
/// Generation must change on fork/restore/reassignment and credential-authority changes.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WorkloadIdentity {
    pub box_id: String,
    pub generation: u64,
}

impl WorkloadIdentity {
    fn validate(&self) -> Result<()> {
        identifier(&self.box_id)?;
        ensure!(self.generation > 0, "invalid workload generation");
        Ok(())
    }

    fn source_identity(&self) -> String {
        let digest = Sha256::digest(serde_json::to_vec(self).expect("identity serialization"));
        format!("ow-{}", &format!("{digest:x}")[..32])
    }
}

/// Resolve a host-owned channel handle on EVERY call. Deny stopped/deleted/shared
/// workloads, stale channels and revoked identities. The guest must not select this handle.
/// Serialize resolution and issuance with lifecycle/policy changes in the host adapter.
pub trait WorkloadAuthorizer {
    fn resolve(&self, channel: &str) -> Result<WorkloadIdentity>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub grants: Vec<BoxGrant>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoxGrant {
    pub workload: WorkloadIdentity,
    pub profile: String,
    /// Host Unix seconds; both issuance and the returned session must end by this time.
    pub expires_at: u64,
    pub aws_role: Option<AwsRoleGrant>,
    /// Exact pre-created AWS Secrets Manager ARN; write only, no secret read interface.
    pub secret_arn: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsRoleGrant {
    pub role_arn: String,
    pub max_duration_seconds: u32,
    pub allow: Vec<Allow>,
}

/// Deliberately bounded subset of IAM: explicit actions and exact resource ARNs.
/// No wildcards, NotAction/NotResource, principal, guest policy or policy-ARN override.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    pub actions: Vec<String>,
    pub resources: Vec<String>,
}

fn arn(value: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = value.splitn(6, ':').collect();
    ensure!(
        parts.len() == 6
            && parts[0] == "arn"
            && matches!(parts[1], "aws" | "aws-cn" | "aws-us-gov")
            && !parts[2].is_empty()
            && !parts[5].is_empty()
            && value.len() <= 2048
            && value
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"*?${}".contains(&b)),
        "exact AWS ARN required"
    );
    Ok(parts)
}

fn account(value: &str) -> bool {
    value.len() == 12 && value.bytes().all(|b| b.is_ascii_digit())
}

impl AwsRoleGrant {
    fn session_policy(&self) -> String {
        json!({"Version":"2012-10-17", "Statement":self.allow.iter().map(|s|
            json!({"Effect":"Allow", "Action":s.actions, "Resource":s.resources})
        ).collect::<Vec<_>>()})
        .to_string()
    }

    fn validate(&self) -> Result<()> {
        let parts = arn(&self.role_arn)?;
        ensure!(
            parts[2] == "iam"
                && parts[3].is_empty()
                && account(parts[4])
                && parts[5]
                    .strip_prefix("role/")
                    .is_some_and(|r| !r.is_empty()),
            "exact IAM role ARN required"
        );
        ensure!(
            (900..=3600).contains(&self.max_duration_seconds),
            "role duration must be 900–3600 seconds"
        );
        ensure!(
            !self.allow.is_empty() && self.allow.len() <= 16,
            "session scope required"
        );
        for statement in &self.allow {
            ensure!(
                !statement.actions.is_empty()
                    && statement.actions.len() <= 32
                    && !statement.resources.is_empty()
                    && statement.resources.len() <= 32,
                "session scope required"
            );
            for action in &statement.actions {
                ensure!(
                    action
                        .split_once(':')
                        .is_some_and(|(service, operation)| !service.is_empty()
                            && !operation.is_empty()
                            && service.bytes().all(|b| b.is_ascii_lowercase()
                                || b.is_ascii_digit()
                                || b == b'-')
                            && operation.bytes().all(|b| b.is_ascii_alphanumeric())),
                    "explicit AWS action required"
                );
            }
            for resource in &statement.resources {
                arn(resource)?;
            }
        }
        ensure!(
            self.session_policy().len() <= 2048,
            "session policy exceeds 2048 bytes"
        );
        Ok(())
    }
}

impl Policy {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 65536, "broker policy too large");
        let policy: Self =
            serde_json::from_slice(bytes).map_err(|_| anyhow!("invalid broker policy"))?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.grants.len() <= 256, "too many broker grants");
        let mut seen = BTreeSet::new();
        for grant in &self.grants {
            grant.workload.validate()?;
            identifier(&grant.profile)?;
            ensure!(
                seen.insert((&grant.workload, &grant.profile)),
                "duplicate broker grant"
            );
            ensure!(
                grant.expires_at > 0 && (grant.aws_role.is_some() || grant.secret_arn.is_some()),
                "empty broker grant"
            );
            if let Some(role) = &grant.aws_role {
                role.validate()?;
            }
            if let Some(secret) = &grant.secret_arn {
                let parts = arn(secret)?;
                ensure!(
                    parts[2] == "secretsmanager"
                        && !parts[3].is_empty()
                        && account(parts[4])
                        && parts[5]
                            .strip_prefix("secret:")
                            .is_some_and(|s| !s.is_empty()),
                    "exact Secrets Manager ARN required"
                );
            }
        }
        Ok(())
    }
}

/// Explicit exposure only; Debug never prints the value. Not serializable or cloneable.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    AwsCredentials {
        profile: String,
        duration_seconds: u32,
    },
    PutSecret {
        profile: String,
        write_id: String,
        value: SecretValue,
    },
}

impl Request {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 512 * 1024, "broker request too large");
        serde_json::from_slice(bytes).map_err(|_| anyhow!("invalid broker request"))
    }
}

/// Only host-selected parameters reach STS. Adapters MUST send all fields unchanged,
/// use host credentials and approved HTTPS AWS endpoints, and must not log responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssumeRole {
    pub role_arn: String,
    pub session_policy: String,
    pub source_identity: String,
    pub role_session_name: String,
    pub duration_seconds: u32,
}

pub struct TemporaryCredentials {
    pub access_key_id: SecretValue,
    pub secret_access_key: SecretValue,
    pub session_token: SecretValue,
    pub expires_at: u64,
}

impl fmt::Debug for TemporaryCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemporaryCredentials")
            .field("credentials", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub trait RoleAssumer {
    fn assume_role(&self, request: &AssumeRole) -> Result<TemporaryCredentials>;
}

/// Write ONLY to this exact pre-created secret; forward the deterministic token to
/// PutSecretValue as ClientRequestToken. Same token + changed value must fail.
/// Never create/list/read/delete secrets or include values in errors/logs.
pub trait SecretWriter {
    fn put_secret(
        &self,
        secret_arn: &str,
        client_request_token: &str,
        value: &SecretValue,
    ) -> Result<()>;
}

#[derive(Debug)]
pub enum Response {
    AwsCredentials(TemporaryCredentials),
    SecretWritten,
}

/// Borrowed policy and no credential cache: dropping/replacing grants affects the
/// next request. Already issued AWS credentials remain usable until AWS expires them.
pub struct CredentialBroker<'a> {
    policy: &'a Policy,
    authorizer: &'a dyn WorkloadAuthorizer,
    roles: &'a dyn RoleAssumer,
    secrets: &'a dyn SecretWriter,
}

impl<'a> CredentialBroker<'a> {
    pub fn new(
        policy: &'a Policy,
        authorizer: &'a dyn WorkloadAuthorizer,
        roles: &'a dyn RoleAssumer,
        secrets: &'a dyn SecretWriter,
    ) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            policy,
            authorizer,
            roles,
            secrets,
        })
    }

    /// `channel` and the Unix clock come from the trusted host, never request JSON.
    /// No retry, journal, filesystem write or provider error body crosses this boundary.
    pub fn handle(
        &self,
        channel: &str,
        request: Request,
        clock: impl Fn() -> u64,
    ) -> Result<Response> {
        let workload = self
            .authorizer
            .resolve(channel)
            .map_err(|_| anyhow!("workload not authorized"))?;
        workload.validate()?;
        let profile = match &request {
            Request::AwsCredentials { profile, .. } | Request::PutSecret { profile, .. } => profile,
        };
        identifier(profile)?;
        let grant = self
            .policy
            .grants
            .iter()
            .find(|g| g.workload == workload && &g.profile == profile)
            .ok_or_else(|| anyhow!("broker grant denied"))?;
        let now = clock();
        ensure!(now > 0 && now < grant.expires_at, "broker grant expired");
        match request {
            Request::AwsCredentials {
                duration_seconds, ..
            } => {
                let role = grant
                    .aws_role
                    .as_ref()
                    .ok_or_else(|| anyhow!("role grant denied"))?;
                ensure!(
                    (900..=role.max_duration_seconds).contains(&duration_seconds),
                    "requested duration denied"
                );
                let end = now
                    .checked_add(u64::from(duration_seconds))
                    .ok_or_else(|| anyhow!("invalid host time"))?;
                ensure!(end <= grant.expires_at, "requested session outlives grant");
                let identity = workload.source_identity();
                let credentials = self
                    .roles
                    .assume_role(&AssumeRole {
                        role_arn: role.role_arn.clone(),
                        session_policy: role.session_policy(),
                        source_identity: identity.clone(),
                        role_session_name: identity,
                        duration_seconds,
                    })
                    .map_err(|_| anyhow!("role provider failed; outcome unknown"))?;
                let finished = clock();
                let latest = finished
                    .checked_add(u64::from(duration_seconds))
                    .ok_or_else(|| anyhow!("invalid host time"))?;
                ensure!(
                    finished >= now
                        && credentials.expires_at > finished
                        && credentials.expires_at <= latest
                        && credentials.expires_at <= grant.expires_at,
                    "invalid credential expiration"
                );
                for value in [
                    &credentials.access_key_id,
                    &credentials.secret_access_key,
                    &credentials.session_token,
                ] {
                    ensure!(
                        !value.expose().is_empty()
                            && value.expose().len() <= 16384
                            && value.expose().bytes().all(|b| b.is_ascii_graphic()),
                        "invalid temporary credentials"
                    );
                }
                Ok(Response::AwsCredentials(credentials))
            }
            Request::PutSecret {
                write_id, value, ..
            } => {
                let target = grant
                    .secret_arn
                    .as_ref()
                    .ok_or_else(|| anyhow!("secret write denied"))?;
                ensure!(
                    (32..=64).contains(&write_id.len())
                        && write_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "invalid secret write ID"
                );
                ensure!(
                    !value.expose().is_empty() && value.expose().len() <= 65536,
                    "secret must be 1–65536 bytes"
                );
                // Scope replay tokens to the exact authority and destination, not a guest-global ID.
                let token = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&(
                        &workload,
                        &grant.profile,
                        target,
                        &write_id
                    ))?)
                );
                self.secrets
                    .put_secret(target, &token, &value)
                    .map_err(|_| {
                        anyhow!("secret provider failed; retry only the same write ID and value")
                    })?;
                Ok(Response::SecretWritten)
            }
        }
    }
}
