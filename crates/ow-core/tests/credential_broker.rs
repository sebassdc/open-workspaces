//! Exercises the exported host request path with credential-free AWS stand-ins.
use anyhow::{Result, bail, ensure};
use ow_core::credential_broker::*;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};

const NOW: u64 = 1_900_000_000;
const CANARY: &str = "synthetic-secret-never-print";
const WRITE_ID: &str = "0123456789abcdef0123456789abcdef";
const FIXTURE: &[u8] = include_bytes!("../../../docs/examples/credential-policy.json");

struct Host(RefCell<Option<WorkloadIdentity>>);
impl Host {
    fn live() -> Self {
        Self(RefCell::new(Some(WorkloadIdentity {
            box_id: "example-box".into(),
            generation: 1,
        })))
    }
}
impl WorkloadAuthorizer for Host {
    fn resolve(&self, channel: &str) -> Result<WorkloadIdentity> {
        ensure!(channel == "host-channel-a", "{CANARY}");
        self.0
            .borrow()
            .clone()
            .ok_or_else(|| anyhow::anyhow!(CANARY))
    }
}

struct Aws {
    calls: RefCell<Vec<AssumeRole>>,
    expiry: Cell<u64>,
    fail: Cell<bool>,
    lose_write_response: Cell<bool>,
    empty_token: Cell<bool>,
    writes: RefCell<Vec<(String, String)>>,
    versions: RefCell<BTreeMap<(String, String), String>>,
}
impl Default for Aws {
    fn default() -> Self {
        Self {
            calls: RefCell::new(vec![]),
            expiry: Cell::new(NOW + 900),
            fail: Cell::new(false),
            lose_write_response: Cell::new(false),
            empty_token: Cell::new(false),
            writes: RefCell::new(vec![]),
            versions: RefCell::new(BTreeMap::new()),
        }
    }
}
impl RoleAssumer for Aws {
    fn assume_role(&self, request: &AssumeRole) -> Result<TemporaryCredentials> {
        self.calls.borrow_mut().push(request.clone());
        ensure!(!self.fail.get(), "{CANARY}");
        Ok(TemporaryCredentials {
            access_key_id: SecretValue::new("synthetic-access-key".into()),
            secret_access_key: SecretValue::new(CANARY.into()),
            session_token: SecretValue::new(
                if self.empty_token.get() {
                    ""
                } else {
                    "synthetic-session-token"
                }
                .into(),
            ),
            expires_at: self.expiry.get(),
        })
    }
}
impl SecretWriter for Aws {
    fn put_secret(&self, target: &str, token: &str, value: &SecretValue) -> Result<()> {
        self.writes.borrow_mut().push((target.into(), token.into()));
        ensure!(!self.fail.get(), "{CANARY}");
        assert_eq!(token.len(), 64);
        let key = (target.to_owned(), token.to_owned());
        let mut versions = self.versions.borrow_mut();
        if let Some(old) = versions.get(&key) {
            ensure!(old == value.expose(), "{CANARY}");
        } else {
            versions.insert(key, value.expose().to_owned());
        }
        ensure!(!self.lose_write_response.get(), "{CANARY}");
        Ok(())
    }
}

fn policy() -> Policy {
    Policy::from_json(FIXTURE).unwrap()
}
fn request() -> Request {
    Request::from_json(br#"{"op":"aws_credentials","profile":"project","duration_seconds":900}"#)
        .unwrap()
}
fn write(value: &str) -> Request {
    Request::PutSecret {
        profile: "project".into(),
        write_id: WRITE_ID.into(),
        value: SecretValue::new(value.into()),
    }
}
fn parse(value: &Value) -> Result<Policy> {
    Policy::from_json(&serde_json::to_vec(value).unwrap())
}

#[test]
fn host_request_passes_only_the_granted_role_scope_and_bounded_session() {
    let (policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    let response = broker.handle("host-channel-a", request(), || NOW).unwrap();
    let rendered = format!("{response:?}");
    assert!(!rendered.contains(CANARY));
    assert!(!rendered.contains("synthetic-access-key"));
    assert!(!rendered.contains("synthetic-session-token"));
    let Response::AwsCredentials(credentials) = response else {
        panic!("credentials required")
    };
    assert_eq!(credentials.secret_access_key.expose(), CANARY);
    assert_eq!(credentials.expires_at, NOW + 900);
    let call = aws.calls.borrow()[0].clone();
    assert_eq!(
        call.role_arn,
        policy.grants[0].aws_role.as_ref().unwrap().role_arn
    );
    assert_eq!(call.duration_seconds, 900);
    assert!(call.source_identity.starts_with("ow-"));
    assert_eq!(call.role_session_name, call.source_identity);
    assert_eq!(
        serde_json::from_str::<Value>(&call.session_policy).unwrap(),
        json!({
            "Version":"2012-10-17", "Statement":[{"Effect":"Allow", "Action":["s3:GetObject"],
            "Resource":["arn:aws:s3:::example-bucket/project/input.json"]}]
        })
    );
    broker.handle("host-channel-a", request(), || NOW).unwrap();
    assert_eq!(
        aws.calls.borrow().len(),
        2,
        "sessions are not cached or journalled"
    );
    assert!(aws.writes.borrow().is_empty());
}

#[test]
fn foreign_stale_stopped_or_ungranted_boxes_never_reach_providers() {
    let (policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    assert!(
        broker
            .handle("guest-selected-channel", request(), || NOW)
            .is_err()
    );
    for identity in [
        None,
        Some(WorkloadIdentity {
            box_id: "another-box".into(),
            generation: 1,
        }),
        Some(WorkloadIdentity {
            box_id: "example-box".into(),
            generation: 2,
        }),
        Some(WorkloadIdentity {
            box_id: "../escape".into(),
            generation: 1,
        }),
        Some(WorkloadIdentity {
            box_id: "example-box".into(),
            generation: 0,
        }),
    ] {
        *host.0.borrow_mut() = identity;
        assert!(broker.handle("host-channel-a", request(), || NOW).is_err());
        assert!(
            broker
                .handle("host-channel-a", write(CANARY), || NOW)
                .is_err()
        );
    }
    *host.0.borrow_mut() = Host::live().0.into_inner();
    assert!(
        broker
            .handle(
                "host-channel-a",
                Request::AwsCredentials {
                    profile: "other".into(),
                    duration_seconds: 900
                },
                || NOW
            )
            .is_err()
    );
    for now in [
        0,
        policy.grants[0].expires_at,
        policy.grants[0].expires_at + 1,
    ] {
        assert!(broker.handle("host-channel-a", request(), || now).is_err());
        assert!(
            broker
                .handle("host-channel-a", write(CANARY), || now)
                .is_err()
        );
    }
    assert!(aws.calls.borrow().is_empty());
    assert!(aws.writes.borrow().is_empty());
}

#[test]
fn removal_expiration_and_separate_operation_grants_fail_closed() {
    let (mut policy, host, aws) = (policy(), Host::live(), Aws::default());
    CredentialBroker::new(&policy, &host, &aws, &aws)
        .unwrap()
        .handle("host-channel-a", request(), || NOW)
        .unwrap();
    let role = policy.grants[0].aws_role.take().unwrap();
    assert!(
        CredentialBroker::new(&policy, &host, &aws, &aws)
            .unwrap()
            .handle("host-channel-a", request(), || NOW)
            .is_err()
    );
    policy.grants[0].aws_role = Some(role);
    policy.grants[0].secret_arn = None;
    policy.grants[0].secret_name = None;
    assert!(
        CredentialBroker::new(&policy, &host, &aws, &aws)
            .unwrap()
            .handle("host-channel-a", write(CANARY), || NOW)
            .is_err()
    );
    policy.grants.clear();
    assert!(
        CredentialBroker::new(&policy, &host, &aws, &aws)
            .unwrap()
            .handle("host-channel-a", request(), || NOW)
            .is_err()
    );
    assert_eq!(aws.calls.borrow().len(), 1);
    assert!(aws.writes.borrow().is_empty());
}

#[test]
fn durations_and_provider_expiration_are_checked_without_silent_clamping() {
    let (mut policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    for duration_seconds in [0, 899, 901, 3601, u32::MAX] {
        assert!(
            broker
                .handle(
                    "host-channel-a",
                    Request::AwsCredentials {
                        profile: "project".into(),
                        duration_seconds
                    },
                    || NOW
                )
                .is_err()
        );
    }
    assert!(aws.calls.borrow().is_empty());
    for expires in [0, NOW, NOW + 901, u64::MAX] {
        aws.expiry.set(expires);
        assert!(broker.handle("host-channel-a", request(), || NOW).is_err());
    }
    aws.expiry.set(NOW + 900);
    aws.empty_token.set(true);
    assert!(broker.handle("host-channel-a", request(), || NOW).is_err());
    aws.empty_token.set(false);
    // Actual provider time advances: a valid 900 s session must still pass.
    aws.expiry.set(NOW + 905);
    let time = Cell::new(NOW);
    broker
        .handle("host-channel-a", request(), || {
            let t = time.get();
            time.set(t + 5);
            t
        })
        .unwrap();
    let time = Cell::new(NOW);
    assert!(
        broker
            .handle("host-channel-a", request(), || {
                let t = time.get();
                time.set(t - 1);
                t
            })
            .is_err()
    );
    policy.grants[0].expires_at = NOW + 899;
    let count = aws.calls.borrow().len();
    assert!(
        CredentialBroker::new(&policy, &host, &aws, &aws)
            .unwrap()
            .handle("host-channel-a", request(), || NOW)
            .is_err()
    );
    assert_eq!(aws.calls.borrow().len(), count);
}

#[test]
fn write_only_secret_uses_exact_target_and_scoped_replay_token() {
    let (mut policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    let response = broker
        .handle("host-channel-a", write(CANARY), || NOW)
        .unwrap();
    assert_eq!(format!("{response:?}"), "SecretWritten");
    broker
        .handle("host-channel-a", write(CANARY), || NOW)
        .unwrap();
    assert_eq!(
        aws.versions.borrow().len(),
        1,
        "same write reuses one version"
    );
    assert_eq!(aws.writes.borrow()[0], aws.writes.borrow()[1]);
    assert_eq!(
        &aws.writes.borrow()[0].0,
        policy.grants[0].secret_arn.as_ref().unwrap()
    );
    let error = broker
        .handle("host-channel-a", write("different-value"), || NOW)
        .unwrap_err();
    assert!(!format!("{error:#}").contains(CANARY));
    assert_eq!(aws.versions.borrow().len(), 1);
    assert!(aws.calls.borrow().is_empty());
    let original_token = aws.writes.borrow()[0].1.clone();
    // A new generation needs an explicit new grant and gets a distinct audit identity/token.
    policy.grants[0].workload.generation = 2;
    host.0.borrow_mut().as_mut().unwrap().generation = 2;
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    broker
        .handle("host-channel-a", write(CANARY), || NOW)
        .unwrap();
    assert_ne!(aws.writes.borrow().last().unwrap().1, original_token);
    assert_eq!(aws.versions.borrow().len(), 2);
    broker.handle("host-channel-a", request(), || NOW).unwrap();
    let new_identity = aws.calls.borrow()[0].source_identity.clone();
    policy.grants[0].workload.generation = 1;
    host.0.borrow_mut().as_mut().unwrap().generation = 1;
    CredentialBroker::new(&policy, &host, &aws, &aws)
        .unwrap()
        .handle("host-channel-a", request(), || NOW)
        .unwrap();
    assert_ne!(aws.calls.borrow()[1].source_identity, new_identity);
}

#[test]
fn invalid_secret_inputs_are_rejected_before_any_write() {
    let (policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    for write_id in [
        "".to_owned(),
        "a".repeat(31),
        "a".repeat(65),
        "/".repeat(32),
    ] {
        assert!(
            broker
                .handle(
                    "host-channel-a",
                    Request::PutSecret {
                        profile: "project".into(),
                        write_id,
                        value: SecretValue::new(CANARY.into())
                    },
                    || NOW
                )
                .is_err()
        );
    }
    for value in [String::new(), "a".repeat(65537), "é".repeat(32769)] {
        assert!(
            broker
                .handle("host-channel-a", write(&value), || NOW)
                .is_err()
        );
    }
    assert!(aws.writes.borrow().is_empty());
    broker
        .handle("host-channel-a", write(&"a".repeat(65536)), || NOW)
        .unwrap();
}

#[test]
fn lost_write_response_reuses_the_original_version_without_automatic_retry() {
    let (policy, host, aws) = (policy(), Host::live(), Aws::default());
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    aws.lose_write_response.set(true);
    let error = broker
        .handle("host-channel-a", write(CANARY), || NOW)
        .unwrap_err();
    assert!(!format!("{error:#}").contains(CANARY));
    assert_eq!(aws.writes.borrow().len(), 1);
    assert_eq!(
        aws.versions.borrow().len(),
        1,
        "write succeeded before response loss"
    );
    aws.lose_write_response.set(false);
    broker
        .handle("host-channel-a", write(CANARY), || NOW)
        .unwrap();
    assert_eq!(aws.writes.borrow().len(), 2);
    assert_eq!(aws.versions.borrow().len(), 1);
    assert_eq!(aws.writes.borrow()[0], aws.writes.borrow()[1]);
}

#[test]
fn provider_latency_cannot_return_credentials_past_grant_expiry() {
    let (mut policy, host, aws) = (policy(), Host::live(), Aws::default());
    policy.grants[0].expires_at = NOW + 900;
    aws.expiry.set(NOW + 905);
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    let time = Cell::new(NOW);
    assert!(
        broker
            .handle("host-channel-a", request(), || {
                let t = time.get();
                time.set(t + 5);
                t
            })
            .is_err()
    );
    assert_eq!(
        aws.calls.borrow().len(),
        1,
        "already-issued credentials are withheld, not retried"
    );
}

#[test]
fn failures_and_malformed_json_never_echo_values_or_auto_retry() {
    let (policy, host, aws) = (policy(), Host::live(), Aws::default());
    aws.fail.set(true);
    let broker = CredentialBroker::new(&policy, &host, &aws, &aws).unwrap();
    for request in [request(), write(CANARY)] {
        assert!(!format!("{request:?}").contains(CANARY));
        let error = broker
            .handle("host-channel-a", request, || NOW)
            .unwrap_err();
        assert!(!format!("{error:#}").contains(CANARY));
    }
    let error = broker.handle("untrusted", request(), || NOW).unwrap_err();
    assert!(!format!("{error:#}").contains(CANARY));
    assert_eq!(aws.calls.borrow().len(), 1);
    assert_eq!(aws.writes.borrow().len(), 1);
    for bytes in [
        format!(r#"{{"op":"{CANARY}"}}"#),
        format!(r#"{{"{CANARY}":false}}"#),
    ] {
        assert!(
            !format!("{:#}", Request::from_json(bytes.as_bytes()).unwrap_err()).contains(CANARY)
        );
        assert!(
            !format!("{:#}", Policy::from_json(bytes.as_bytes()).unwrap_err()).contains(CANARY)
        );
    }
}

#[test]
fn request_schema_has_no_guest_identity_role_policy_endpoint_or_read_overrides() {
    for field in [
        "box_id",
        "generation",
        "role_arn",
        "session_policy",
        "secret_arn",
        "secret_name",
        "endpoint",
        "now",
    ] {
        let mut input =
            json!({"op":"aws_credentials", "profile":"project", "duration_seconds":900});
        input[field] = json!("attacker");
        assert!(
            Request::from_json(&serde_json::to_vec(&input).unwrap()).is_err(),
            "{field}"
        );
    }
    for op in [
        "get_secret",
        "list_secrets",
        "delete_secret",
        "create_secret",
        "assume_role",
    ] {
        assert!(
            Request::from_json(&serde_json::to_vec(&json!({"op":op,"profile":"project"})).unwrap())
                .is_err()
        );
    }
    assert!(Request::from_json(&vec![b' '; 512 * 1024 + 1]).is_err());
}

#[test]
fn partial_secret_arns_are_rejected_before_write_for_json_and_rust_policies() {
    let (host, aws) = (Host::live(), Aws::default());
    for resource in [
        "example-box-output".to_owned(),
        "example-box-output-".into(),
        "example-box-output-AbCdE".into(),
        "example-box-output-AbCdEfG".into(),
        "example-box-output-AbCdE_".into(),
        "-AbCdEf".into(),
        "invalid:name-AbCdEf".into(),
        "invalid!name-AbCdEf".into(),
        format!("{}-AbCdEf", "a".repeat(513)),
    ] {
        let destination =
            format!("arn:aws:secretsmanager:us-east-1:000000000000:secret:{resource}");
        let mut input: Value = serde_json::from_slice(FIXTURE).unwrap();
        input["grants"][0]["secret_arn"] = json!(destination);
        assert!(
            parse(&input).is_err(),
            "accepted partial or invalid ARN: {destination}"
        );
        let mut direct = policy();
        direct.grants[0].secret_arn = Some(destination.clone());
        let result = CredentialBroker::new(&direct, &host, &aws, &aws);
        if let Ok(broker) = &result {
            let _ = broker.handle("host-channel-a", write(CANARY), || NOW);
        }
        assert!(result.is_err(), "direct policy accepted {destination}");
    }
    assert!(
        aws.writes.borrow().is_empty(),
        "invalid targets reached writer"
    );
}

#[test]
fn complete_secret_arns_preserve_exact_destinations_including_hyphenated_names() {
    let (host, aws) = (Host::live(), Aws::default());
    for resource in [
        "example-box-output-AbCdEf".to_owned(),
        "name-AbCdEf-GhIj09".into(),
        "path/to_name+=.@-AbCd09".into(),
        format!("{}-aB0123", "a".repeat(512)),
    ] {
        let destination =
            format!("arn:aws:secretsmanager:us-east-1:000000000000:secret:{resource}");
        let mut input: Value = serde_json::from_slice(FIXTURE).unwrap();
        input["grants"][0]["secret_arn"] = json!(destination);
        input["grants"][0]["secret_name"] = json!(resource.rsplit_once('-').unwrap().0);
        let policy = parse(&input).unwrap();
        CredentialBroker::new(&policy, &host, &aws, &aws)
            .unwrap()
            .handle("host-channel-a", write(CANARY), || NOW)
            .unwrap();
        assert_eq!(aws.writes.borrow().last().unwrap().0, destination);
    }
}

#[test]
fn policy_rejects_ambiguous_broad_or_malformed_authority() {
    let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
    let mut bad = vec![];
    for (pointer, value) in [
        ("/grants/0/workload/box_id", json!("../escape")),
        ("/grants/0/workload/generation", json!(0)),
        ("/grants/0/expires_at", json!(0)),
        ("/grants/0/profile", json!("../project")),
        (
            "/grants/0/aws_role/role_arn",
            json!("arn:aws:iam::000000000000:role/*"),
        ),
        (
            "/grants/0/aws_role/role_arn",
            json!("arn:aws:iam::000000000000:user/example"),
        ),
        (
            "/grants/0/aws_role/role_arn",
            json!("arn:aws:iam::123:role/example"),
        ),
        ("/grants/0/aws_role/max_duration_seconds", json!(899)),
        ("/grants/0/aws_role/max_duration_seconds", json!(3601)),
        ("/grants/0/aws_role/allow", json!([])),
        ("/grants/0/aws_role/allow/0/actions", json!(["s3:*"])),
        ("/grants/0/aws_role/allow/0/actions", json!([])),
        ("/grants/0/aws_role/allow/0/resources", json!(["*"])),
        (
            "/grants/0/aws_role/allow/0/resources",
            json!(["arn:aws:s3:::bucket/*"]),
        ),
        (
            "/grants/0/aws_role/allow/0/resources",
            json!(["arn:aws:s3:::bucket/${variable}"]),
        ),
        (
            "/grants/0/aws_role/allow/0/resources",
            json!(["arn:aws:s3:::bucket/".to_owned() + &"a".repeat(2048)]),
        ),
        (
            "/grants/0/secret_arn",
            json!("arn:aws:secretsmanager:us-east-1:000000000000:secret:*"),
        ),
        (
            "/grants/0/secret_arn",
            json!("arn:aws:ssm:us-east-1:000000000000:parameter/not-a-secret"),
        ),
        ("/grants/0/secret_name", Value::Null),
        ("/grants/0/secret_name", json!("different-name")),
        ("/grants/0/secret_name", json!("example-box")),
        ("/grants/0/secret_arn", Value::Null),
    ] {
        let mut value_copy = fixture.clone();
        *value_copy.pointer_mut(pointer).unwrap() = value;
        bad.push(value_copy);
    }
    let mut duplicate = fixture.clone();
    duplicate["grants"]
        .as_array_mut()
        .unwrap()
        .push(fixture["grants"][0].clone());
    bad.push(duplicate);
    let mut empty_grant = fixture.clone();
    empty_grant["grants"][0]["aws_role"] = Value::Null;
    empty_grant["grants"][0]["secret_arn"] = Value::Null;
    bad.push(empty_grant);
    let mut override_policy = fixture.clone();
    override_policy["grants"][0]["aws_role"]["allow"][0]["NotAction"] = json!(["s3:DeleteObject"]);
    bad.push(override_policy);
    for value in bad {
        assert!(parse(&value).is_err(), "accepted {value}");
    }
    assert!(Policy::from_json(&vec![b' '; 65537]).is_err());
    // Direct Rust construction cannot bypass policy validation at broker creation.
    let (mut policy, host, aws) = (policy(), Host::live(), Aws::default());
    policy.grants[0].aws_role.as_mut().unwrap().allow[0].resources = vec!["*".into()];
    assert!(CredentialBroker::new(&policy, &host, &aws, &aws).is_err());
}

#[test]
fn provider_contract_has_no_secret_read_and_no_default_fallback() -> Result<()> {
    let empty = Policy::from_json(br#"{"grants":[]}"#)?;
    let (host, aws) = (Host::live(), Aws::default());
    let broker = CredentialBroker::new(&empty, &host, &aws, &aws)?;
    if broker.handle("host-channel-a", request(), || NOW).is_ok() {
        bail!("default grant")
    }
    assert!(aws.calls.borrow().is_empty());
    Ok(())
}
