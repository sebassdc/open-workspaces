use super::*;
use crate::{catalog::Identity, wire};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use std::{
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

const ROLE: &str = "arn:aws:iam::111122223333:role/open-workspaces/box-reader";
const OTHER: &str = "arn:aws:iam::111122223333:role/open-workspaces/b-only";
const OWNED: &str = "arn:aws:iam::111122223333:role/open-workspaces/a-only";
const ISSUER: &str = "https://ow.example.test";

struct Fixture {
    root: PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    written: Arc<AtomicBool>,
    /// The worker's view: the running VMM holds a token (false after a cold restart).
    present: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    /// A catalog with a fake local worker that records `aws-token` requests.
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ow-aws-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (seen, written, present, stop) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (record, answer, holds, done) =
            (seen.clone(), written.clone(), present.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let request = wire::line(&mut stream).unwrap();
                        let value = if request["op"] == "aws-token" && request["check"] == true {
                            // Like the worker: present while this VMM process holds a token.
                            json!({"written":false,"present":holds.load(Ordering::SeqCst)})
                        } else if request["op"] == "aws-token" {
                            let running = answer.load(Ordering::SeqCst);
                            holds
                                .store(running && request.get("token").is_some(), Ordering::SeqCst);
                            record.lock().unwrap().push(request);
                            json!({"written":running})
                        } else {
                            json!([])
                        };
                        wire::send(&mut stream, &wire::response(Ok(value))).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        Self {
            root,
            seen,
            written,
            present,
            stop,
            worker: Some(worker),
        }
    }

    fn configure(&self, config: Value) {
        let path = self.root.join(CONFIG);
        let _ = std::fs::remove_file(&path);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(config.to_string().as_bytes()).unwrap();
    }

    fn requests(&self) -> Vec<Value> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn identity(subject: &str, email: &str) -> Identity {
    Identity {
        issuer: "https://example.cloudflareaccess.com".into(),
        subject: subject.into(),
        email: email.into(),
    }
}

fn running(c: &Catalog, owner: i64, name: &str, physical: &str) {
    c.db.execute(
        "INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'machine',?2,?3,?4,'local')",
        params![owner, name, physical, json!({"id":physical,"state":"running"}).to_string()],
    )
    .unwrap();
}

/// A fork child: the worker records its parent as `source`.
fn forked(c: &Catalog, owner: i64, name: &str, physical: &str, parent: &str) {
    c.db.execute(
        "INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'machine',?2,?3,?4,'local')",
        params![owner, name, physical, json!({"id":physical,"state":"running","source":parent}).to_string()],
    )
    .unwrap();
}

/// What AWS STS concludes from the JWKS published now: the token no longer verifies.
fn rejected(root: &Path, request: &Value) -> bool {
    let token = request["token"].as_str().unwrap();
    let jwks: JwkSet = serde_json::from_value(public_document(root, JWKS).unwrap()).unwrap();
    let Some(key) = jwks.find(decode_header(token).unwrap().kid.as_deref().unwrap()) else {
        return true;
    };
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[AUDIENCE]);
    validation.set_issuer(&[ISSUER]);
    decode::<Value>(token, &DecodingKey::from_jwk(key).unwrap(), &validation).is_err()
}

fn refusal(error: anyhow::Error) -> &'static str {
    error.downcast_ref::<Refusal>().expect("a refusal").code
}

/// Verify a delivered token as AWS STS would: signature against the published JWKS, then claims.
fn verified(root: &Path, request: &Value) -> Value {
    let token = request["token"].as_str().unwrap();
    let jwks: JwkSet =
        serde_json::from_value(public_document(root, JWKS).expect("published JWKS")).unwrap();
    let header = decode_header(token).unwrap();
    assert_eq!(header.alg, Algorithm::ES256);
    let key = jwks
        .find(header.kid.as_deref().unwrap())
        .expect("kid in JWKS");
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[AUDIENCE]);
    validation.set_issuer(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "nbf"]);
    decode::<Value>(token, &DecodingKey::from_jwk(key).unwrap(), &validation)
        .unwrap()
        .claims
}

#[test]
fn roles_need_the_allow_list_and_owner_or_admin() {
    let f = Fixture::new();
    let mut c = Catalog::open(&f.root, "test", None).unwrap();
    let a = c.user(&identity("a", "a@example.test")).unwrap();
    let use_grantee = c.user(&identity("u", "u@example.test")).unwrap();
    let admin = c.user(&identity("d", "d@example.test")).unwrap();
    running(&c, a, "box", "m-box");
    let set = |role: &str| json!({"op":"aws-role","id":"box","role_arn":role});
    // Off by default: nothing to attach to and nothing published.
    assert_eq!(
        refusal(c.operation(a, set(ROLE)).unwrap_err()),
        "aws_access_off"
    );
    assert!(public_document(&f.root, DISCOVERY).is_none());
    assert_eq!(c.state(a).unwrap()["aws_identity"]["enabled"], false);
    f.configure(json!({"issuer":ISSUER,"allowed_roles":[{"role_arn":ROLE},{"role_arn":OTHER,"owners":["b@example.test"]},{"role_arn":OWNED,"owners":["a@example.test"]}]}));
    let unlisted = "arn:aws:iam::111122223333:role/admin";
    assert_eq!(
        refusal(c.operation(a, set(unlisted)).unwrap_err()),
        "aws_role_not_allowed"
    );
    // Restricted to another owner's boxes.
    assert_eq!(
        refusal(c.operation(a, set(OTHER)).unwrap_err()),
        "aws_role_not_allowed"
    );
    assert!(f.requests().is_empty());
    assert_eq!(
        c.state(a).unwrap()["aws_identity"],
        json!({"enabled":true,"issuer":ISSUER,"roles":[ROLE,OWNED]})
    );
    for (grantee, role) in [(use_grantee, "use"), (admin, "admin")] {
        let email = if role == "use" {
            "u@example.test"
        } else {
            "d@example.test"
        };
        c.operation(a, json!({"op":"share","id":"box","with":email,"role":role}))
            .unwrap();
        let shared = json!({"op":"aws-role","id":"a@example.test/box","role_arn":ROLE});
        let result = c.operation(grantee, shared);
        assert_eq!(result.is_ok(), role == "admin", "{role}: {result:?}");
    }
    let attached = c.state(a).unwrap()["workspaces"][0]["aws_role"].clone();
    assert_eq!(attached["role_arn"], ROLE);
    assert_eq!(attached["state"], "active");
    assert_eq!(attached["subject"], "owner/a@example.test/box/m-box");
    // A grantee sees it too. An admin is offered the owner's roles, not their own: an
    // owner-only role is attachable by the admin and listed for them.
    let shared = c.state(admin).unwrap();
    assert_eq!(shared["shared"][0]["aws_role"]["role_arn"], ROLE);
    assert_eq!(shared["aws_identity"]["roles"], json!([ROLE]));
    assert_eq!(shared["shared"][0]["aws_roles"], json!([ROLE, OWNED]));
    c.operation(
        admin,
        json!({"op":"aws-role","id":"a@example.test/box","role_arn":OWNED}),
    )
    .unwrap();
    assert!(
        c.state(use_grantee).unwrap()["shared"][0]
            .get("aws_roles")
            .is_none()
    );

    // One issuer publishes at most 100 EC keys: the anchor plus 99 attachments.
    for i in 1..MAX_ATTACHED {
        c.db.execute(
            "INSERT INTO aws_roles(physical,owner,role_arn,generation) VALUES(?1,?2,?3,1)",
            params![format!("m-filler-{i}"), a, ROLE],
        )
        .unwrap();
    }
    c.operation(a, set(ROLE)).unwrap(); // the box itself is already counted
    running(&c, a, "more", "m-more");
    let more = json!({"op":"aws-role","id":"more","role_arn":ROLE});
    assert_eq!(refusal(c.operation(a, more).unwrap_err()), "aws_role_limit");
    // Bad config fails closed.
    f.configure(json!({"issuer":"http://ow.example.test","allowed_roles":[]}));
    assert!(c.operation(a, set(ROLE)).is_err());
}

#[test]
fn tokens_are_short_lived_rotate_and_follow_the_incarnation() {
    let f = Fixture::new();
    f.configure(json!({"issuer":ISSUER,"allowed_roles":[{"role_arn":ROLE}]}));
    let mut c = Catalog::open(&f.root, "test", None).unwrap();
    let a = c.user(&identity("a", "A@Example.test")).unwrap();
    let b = c.user(&identity("b", "b@example.test")).unwrap();
    running(&c, a, "box", "m-box");
    c.operation(a, json!({"op":"aws-role","id":"box","role_arn":ROLE}))
        .unwrap();
    let first = f.requests();
    assert_eq!(first.len(), 1);
    let claims = verified(&f.root, &first[0]);
    assert_eq!(claims["sub"], "owner/a@example.test/box/m-box");
    assert_eq!(claims["aud"], AUDIENCE);
    assert_eq!(claims["ow_generation"], 1);
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        TTL
    );
    assert!(TTL <= 900, "a short TTL bounds a copied token's life");
    assert_eq!(first[0]["role_arn"], ROLE);
    let session = |generation| {
        WorkloadIdentity {
            box_id: "m-box".into(),
            generation,
        }
        .source_identity()
    };
    assert_eq!(first[0]["session_name"], session(1));
    // Discovery points AWS at the keys that verified it.
    let discovery = public_document(&f.root, DISCOVERY).unwrap();
    assert_eq!(discovery["issuer"], ISSUER);
    assert_eq!(discovery["jwks_uri"], format!("{ISSUER}{JWKS}"));
    assert_eq!(
        discovery["id_token_signing_alg_values_supported"],
        json!(["ES256"])
    );

    // Fresh: the refresher leaves it. Near expiry: a new token, same generation.
    let mut cleared = HashSet::new();
    let pass = |c: &mut Catalog, cleared: &mut HashSet<_>| {
        for work in c.aws_work().unwrap() {
            c.aws_step(work, cleared);
        }
    };
    pass(&mut c, &mut cleared);
    assert!(f.requests().is_empty());
    // A cold stop and start empties the guest's /run: delivered again without waiting.
    f.present.store(false, Ordering::SeqCst);
    pass(&mut c, &mut cleared);
    let restarted = f.requests();
    assert_eq!(restarted.len(), 1);
    assert_eq!(verified(&f.root, &restarted[0])["ow_generation"], 1);
    c.db.execute("UPDATE aws_roles SET expires_at=?1", [(now() + 60) as i64])
        .unwrap();
    pass(&mut c, &mut cleared);
    let rotated = f.requests();
    assert_eq!(rotated.len(), 1);
    let again = verified(&f.root, &rotated[0]);
    assert_ne!(again["jti"], claims["jti"]);
    assert_eq!(again["ow_generation"], 1);

    // A move (new placement epoch) is a new incarnation: generation and session change now.
    c.db.execute(
        "UPDATE resources SET placement_epoch=7 WHERE physical='m-box'",
        [],
    )
    .unwrap();
    pass(&mut c, &mut cleared);
    let moved = f.requests();
    assert_eq!(verified(&f.root, &moved[0])["ow_generation"], 2);
    assert_eq!(moved[0]["session_name"], session(2));
    // The old incarnation's key left the JWKS: its tokens, wherever copied, stop verifying.
    for old in [&first[0], &restarted[0], &rotated[0]] {
        assert!(rejected(&f.root, old));
    }

    // Re-attaching is a new credential authority too.
    c.operation(a, json!({"op":"aws-role","id":"box","role_arn":ROLE}))
        .unwrap();
    let reattached = f.requests();
    assert_eq!(verified(&f.root, &reattached[0])["ow_generation"], 3);
    assert!(rejected(&f.root, &moved[0]));

    // A stopped box gets nothing written and shows pending; it is retried once it runs.
    f.written.store(false, Ordering::SeqCst);
    c.db.execute("UPDATE aws_roles SET expires_at=NULL", [])
        .unwrap();
    pass(&mut c, &mut cleared);
    assert_eq!(f.requests().len(), 1);
    assert_eq!(
        c.state(a).unwrap()["workspaces"][0]["aws_role"]["state"],
        "pending"
    );
    f.written.store(true, Ordering::SeqCst);

    // A fork copies RAM but not the role: its guest files are cleared once per incarnation,
    // and the parent moves to a new generation, so the copied token stops verifying.
    let before_fork = reattached[0].clone();
    forked(&c, a, "child", "m-child", "m-box");
    pass(&mut c, &mut cleared);
    let after_fork = f.requests();
    let cleared_child: Vec<_> = after_fork.iter().filter(|r| r["id"] == "m-child").collect();
    assert_eq!(cleared_child.len(), 1);
    assert!(cleared_child[0].get("token").is_none());
    let parent: Vec<_> = after_fork.iter().filter(|r| r["id"] == "m-box").collect();
    assert_eq!(verified(&f.root, parent[0])["ow_generation"], 4);
    assert!(rejected(&f.root, &before_fork));
    assert!(c.state(a).unwrap()["workspaces"][1]["aws_role"].is_null());
    pass(&mut c, &mut cleared);
    assert!(f.requests().iter().all(|r| r["id"] != "m-child"));
    // A restore keeps node and epoch but brings back RAM: checked again.
    c.db.execute(
        "INSERT INTO operations(owner,op,resource,state) VALUES(?1,'restore','child','succeeded')",
        [a],
    )
    .unwrap();
    // (The fake runner lists no machines, so the state() above marked the child missing.)
    c.db.execute(
        "UPDATE resources SET metadata=json_set(metadata,'$.state','running') WHERE physical='m-child'",
        [],
    )
    .unwrap();
    pass(&mut c, &mut cleared);
    assert_eq!(
        f.requests().iter().filter(|r| r["id"] == "m-child").count(),
        1
    );
    let latest = parent[0].clone();

    // An operator removing the role stops issuance and clears the guest.
    f.configure(json!({"issuer":ISSUER,"allowed_roles":[]}));
    c.db.execute("UPDATE aws_roles SET expires_at=NULL", [])
        .unwrap();
    pass(&mut c, &mut cleared);
    let revoked = f.requests();
    assert_eq!(revoked.len(), 1);
    assert!(revoked[0].get("token").is_none());
    assert!(rejected(&f.root, &latest));
    // Cleared once, not on every pass.
    pass(&mut c, &mut cleared);
    assert!(f.requests().is_empty());
    assert_eq!(
        c.state(a).unwrap()["workspaces"][0]["aws_role"]["state"],
        "error"
    );

    // A new owner does not inherit the attachment.
    f.configure(json!({"issuer":ISSUER,"allowed_roles":[{"role_arn":ROLE}]}));
    c.db.execute("UPDATE resources SET owner=?1 WHERE physical='m-box'", [b])
        .unwrap();
    pass(&mut c, &mut cleared);
    let transferred = f.requests();
    assert!(transferred.iter().all(|r| r.get("token").is_none()));
    assert_eq!(
        c.state(b).unwrap()["workspaces"][0]["aws_role"]["state"],
        "inactive"
    );
    c.db.execute("UPDATE resources SET owner=?1 WHERE physical='m-box'", [a])
        .unwrap();

    // Detach removes the row and the guest files.
    c.operation(a, json!({"op":"aws-role","id":"box"})).unwrap();
    let detached = f.requests();
    assert_eq!(detached.len(), 1);
    assert!(detached[0].get("token").is_none());
    assert!(c.state(a).unwrap()["workspaces"][0]["aws_role"].is_null());

    // Only the never-signing anchor key is left; keys are private files.
    assert_eq!(jwks(&f.root).unwrap()["keys"].as_array().unwrap().len(), 1);
    assert_eq!(jwks(&f.root).unwrap(), jwks(&f.root).unwrap());
    let dir = f.root.join(KEYS);
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    for entry in std::fs::read_dir(dir).unwrap() {
        assert_eq!(entry.unwrap().metadata().unwrap().mode() & 0o777, 0o600);
    }
}

#[test]
fn issuer_and_roles_are_validated() {
    for (issuer, role) in [
        ("http://ow.example.test", ROLE),
        ("https://ow.example.test/", ROLE),
        ("https://ow.example.test:8443", ROLE),
        ("https://ow.example.test?x=1", ROLE),
        (ISSUER, "arn:aws:iam::111122223333:role/*"),
        (ISSUER, "arn:aws:iam::111122223333:user/someone"),
    ] {
        let config = Config {
            issuer: issuer.into(),
            allowed_roles: vec![AllowedRole {
                role_arn: role.into(),
                owners: vec![],
            }],
        };
        assert!(config.validate().is_err(), "{issuer} {role}");
    }
    let config = Config {
        issuer: format!("{ISSUER}/oidc"),
        allowed_roles: vec![AllowedRole {
            role_arn: ROLE.into(),
            owners: vec!["Owner@Example.test".into()],
        }],
    };
    config.validate().unwrap();
    assert!(config.allows(ROLE, "owner@example.test"));
    assert!(!config.allows(ROLE, "other@example.test"));
}
