//! Metadata mode: the controller assumes roles outside the catalog lock and the runner's
//! credentials are withdrawn before any change that ends or re-owns an attachment commits.
use super::*;
use crate::aws_metadata::Assume;
use ow_core::sts;
use std::sync::Weak;

pub(super) const READER: &str = "arn:aws:iam::111122223333:role/ow-box/reader";
const ADMIN_ONLY: &str = "arn:aws:iam::111122223333:role/ow-box/a-only";

pub(super) fn metadata_config() -> Value {
    json!({"mode":"metadata","allowed_roles":[
        {"role_arn":READER,"owners":["a@example.test","B@example.test"]},
        {"role_arn":ADMIN_ONLY,"owners":["a@example.test"]}]})
}

type During = Box<dyn FnMut(&Arc<Mutex<Catalog>>)>;

/// STS stand-in: checks it is called with the catalog unlocked and records each request.
pub(super) struct FakeSts {
    catalog: Weak<Mutex<Catalog>>,
    calls: Vec<(String, String, String, String)>,
    fail: bool,
    /// Runs inside the STS call, as another request would while STS is slow.
    during: Option<During>,
}

impl FakeSts {
    pub(super) fn new(catalog: &Arc<Mutex<Catalog>>) -> Self {
        Self {
            catalog: Arc::downgrade(catalog),
            calls: Vec::new(),
            fail: false,
            during: None,
        }
    }
}

impl Assume for FakeSts {
    fn host(&mut self) -> Result<sts::Host> {
        Ok(sts::Host {
            account: "111122223333".into(),
            region: "us-east-1".into(),
            zone: Some("us-east-1a".into()),
        })
    }

    fn assume(&mut self, request: &sts::Request) -> Result<sts::Session> {
        let catalog = self.catalog.upgrade().unwrap();
        assert!(
            catalog.try_lock().is_ok(),
            "STS must be called with the catalog lock released"
        );
        if let Some(during) = &mut self.during {
            during(&catalog);
        }
        self.calls.push((
            request.role_arn.into(),
            request.session.into(),
            request.box_id.into(),
            request.owner.into(),
        ));
        anyhow::ensure!(
            !self.fail,
            "AWS STS refused the role session (AccessDenied)"
        );
        Ok(sts::Session {
            access_key_id: format!("ASIAFAKE{}", self.calls.len()),
            secret_access_key: "fakeSecret+/=".to_owned().into(),
            session_token: "fakeSession+/=".to_owned().into(),
            expiration: now() + 3600,
        })
    }
}

fn session(physical: &str, generation: u64) -> String {
    WorkloadIdentity {
        box_id: physical.into(),
        generation,
    }
    .source_identity()
}

fn set_state(c: &Catalog, physical: &str, state: &str) {
    c.db.execute(
        "UPDATE resources SET metadata=json_set(metadata,'$.state',?2) WHERE physical=?1",
        params![physical, state],
    )
    .unwrap();
}

#[test]
fn metadata_config_is_validated() {
    let role = |arn: &str, owners: &[&str]| AllowedRole {
        role_arn: arn.into(),
        owners: owners.iter().map(|o| o.to_string()).collect(),
    };
    let config = |issuer: Option<&str>, roles: Vec<AllowedRole>| Config {
        mode: Mode::Metadata,
        issuer: issuer.map(Into::into),
        allowed_roles: roles,
    };
    config(None, vec![role(READER, &["a@example.test"])])
        .validate()
        .unwrap();
    for (bad, why) in [
        (
            config(Some(ISSUER), vec![role(READER, &["a@example.test"])]),
            "issuer",
        ),
        (
            config(None, vec![role(ROLE, &["a@example.test"])]),
            "outside /ow-box/",
        ),
        (config(None, vec![role(READER, &[])]), "every user"),
        (
            config(
                None,
                vec![
                    role(READER, &["a@example.test"]),
                    role(
                        "arn:aws:iam::444455556666:role/ow-box/x",
                        &["a@example.test"],
                    ),
                ],
            ),
            "two accounts",
        ),
    ] {
        assert!(bad.validate().is_err(), "{why}");
    }
    // Web-identity mode still needs its issuer.
    let missing = Config {
        mode: Mode::WebIdentity,
        issuer: None,
        allowed_roles: vec![],
    };
    assert!(missing.validate().is_err());
}

#[test]
fn metadata_mode_assumes_outside_the_lock_and_follows_the_box() {
    let f = Fixture::new();
    f.lists.store(true, Ordering::SeqCst);
    f.configure(metadata_config());
    let shared = Arc::new(Mutex::new(Catalog::open(&f.root, "test", None).unwrap()));
    let mut sts = FakeSts::new(&shared);
    let mut cleared = HashSet::new();
    const BOX: &str = "m-meta-follow";
    let a = {
        let mut c = shared.lock().unwrap();
        let a = c.user(&identity("a", "A@Example.test")).unwrap();
        running(&c, a, "box", BOX);
        // An operator-allowed role outside /ow-box/ never reaches metadata mode, nor a role
        // of another owner; the issuer is not published.
        let refused = c.operation(a, json!({"op":"aws-role","id":"box","role_arn":ROLE}));
        assert_eq!(refusal(refused.unwrap_err()), "aws_role_not_allowed");
        assert!(public_document(&f.root, DISCOVERY).is_none());
        assert_eq!(c.state(a).unwrap()["aws_identity"]["mode"], "metadata");
        let attached = c
            .operation(a, json!({"op":"aws-role","id":"box","role_arn":READER}))
            .unwrap();
        assert_eq!(attached["aws_role"]["state"], "pending");
        assert_eq!(attached["aws_role"]["hop_limit"], 1);
        a
    };
    // The runner was told to wait for this generation before the attachment was recorded.
    let creds = f.creds();
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0].0["role_arn"], READER);
    assert_eq!(creds[0].0["generation"], 1);
    assert!(creds[0].0.get("access_key_id").is_none());
    assert_eq!(creds[0].1, Some((a, false)));
    assert!(!ow_core::metadata::present(BOX, 1));

    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(
        sts.calls,
        [(
            READER.into(),
            session(BOX, 1),
            BOX.into(),
            "a@example.test".into()
        )]
    );
    assert!(ow_core::metadata::present(BOX, 1));
    let delivered = f.creds();
    let pushed = delivered
        .iter()
        .find(|(r, _)| r.get("access_key_id").is_some())
        .unwrap();
    assert_eq!(pushed.0["access_key_id"], "ASIAFAKE1");
    assert_eq!(pushed.0["region"], "us-east-1");
    assert_eq!(pushed.0["availability_zone"], "us-east-1a");
    let view = |c: &Catalog| crate::aws_identity::view(&c.db, BOX).unwrap();
    {
        let c = shared.lock().unwrap();
        assert_eq!(view(&c)["state"], "active");
        assert!(view(&c)["token_expires_at"].as_u64().unwrap() > now() + 3000);
    }
    // Fresh: left alone. A restarted worker forgot it: assumed again at once.
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), 1);
    ow_core::metadata::clear(BOX);
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), 2);
    assert!(ow_core::metadata::present(BOX, 1));
    // Less than half an hour left: a new session, same generation.
    let soon = |c: &Catalog| {
        c.db.execute("UPDATE aws_roles SET expires_at=?1", [(now() + 600) as i64])
            .unwrap();
    };
    soon(&shared.lock().unwrap());
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), 3);
    // STS refuses: the error shows, what the runner holds stays until it expires.
    sts.fail = true;
    soon(&shared.lock().unwrap());
    pass(&shared, &mut sts, &mut cleared);
    sts.fail = false;
    assert_eq!(
        view(&shared.lock().unwrap())["error"],
        "AWS STS refused the role session (AccessDenied)"
    );
    assert!(ow_core::metadata::present(BOX, 1));

    // A move is a new incarnation: a new generation and session.
    shared
        .lock()
        .unwrap()
        .db
        .execute(
            "UPDATE resources SET placement_epoch=9 WHERE physical=?1",
            [BOX],
        )
        .unwrap();
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.last().unwrap().1, session(BOX, 2));
    assert!(ow_core::metadata::present(BOX, 2));
    assert!(view(&shared.lock().unwrap())["error"].is_null());

    // A fork child gets nothing; the parent moves to a new generation.
    {
        let c = shared.lock().unwrap();
        forked(&c, a, "child", "m-meta-follow-child", BOX);
    }
    let before = sts.calls.len();
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), before + 1);
    assert!(sts.calls.iter().all(|call| call.2 == BOX));
    assert_eq!(sts.calls.last().unwrap().1, session(BOX, 3));
    assert!(!ow_core::metadata::present("m-meta-follow-child", 1));

    // A stopped box gets no session (its runner keeps only a wait); starting it does.
    set_state(&shared.lock().unwrap(), BOX, "stopped");
    ow_core::metadata::stopped(BOX);
    soon(&shared.lock().unwrap());
    let before = sts.calls.len();
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), before);
    assert_eq!(view(&shared.lock().unwrap())["state"], "pending");
    set_state(&shared.lock().unwrap(), BOX, "hibernated");
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(
        sts.calls.len(),
        before + 1,
        "hibernated boxes are kept fresh"
    );
    assert!(ow_core::metadata::present(BOX, 3));

    // Allow-list removal: the runner forgets the role first, then the error is recorded.
    f.creds();
    f.configure(json!({"mode":"metadata","allowed_roles":[{"role_arn":ADMIN_ONLY,"owners":["a@example.test"]}]}));
    pass(&shared, &mut sts, &mut cleared);
    let withdrawn = f.creds();
    assert!(
        withdrawn
            .iter()
            .any(|(r, _)| r.get("role_arn").is_none() && r["id"] == BOX)
    );
    assert!(!ow_core::metadata::present(BOX, 3));
    assert_eq!(
        view(&shared.lock().unwrap())["error"],
        "role removed from the operator allow-list"
    );
    ow_core::metadata::clear(BOX);
}

#[test]
fn credentials_assumed_for_an_old_attachment_are_never_delivered() {
    let f = Fixture::new();
    f.lists.store(true, Ordering::SeqCst);
    f.configure(metadata_config());
    let shared = Arc::new(Mutex::new(Catalog::open(&f.root, "test", None).unwrap()));
    let mut sts = FakeSts::new(&shared);
    let mut cleared = HashSet::new();
    const BOX: &str = "m-meta-race";
    let a = {
        let mut c = shared.lock().unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        c.user(&identity("b", "b@example.test")).unwrap();
        running(&c, a, "box", BOX);
        c.operation(a, json!({"op":"aws-role","id":"box","role_arn":READER}))
            .unwrap();
        a
    };
    // While STS answers, the owner gives the box away (and with it, the role goes).
    sts.during = Some(Box::new(move |catalog| {
        catalog
            .lock()
            .unwrap()
            .operation(a, json!({"op":"transfer","id":"box","to":"b@example.test"}))
            .unwrap();
    }));
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), 1);
    assert!(!ow_core::metadata::present(BOX, 1));
    assert!(
        f.creds()
            .iter()
            .all(|(r, _)| r.get("access_key_id").is_none()),
        "the old owner's session never reached the runner"
    );
    // Re-attached while STS answers: the older generation's session is dropped too.
    {
        let mut c = shared.lock().unwrap();
        let b = c.user(&identity("b", "b@example.test")).unwrap();
        c.operation(b, json!({"op":"aws-role","id":"box","role_arn":READER}))
            .unwrap();
        sts.during = Some(Box::new(move |catalog| {
            catalog
                .lock()
                .unwrap()
                .operation(b, json!({"op":"aws-role","id":"box","role_arn":READER}))
                .unwrap();
        }));
    }
    let generation: i64 = shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT generation FROM aws_roles WHERE physical=?1",
            [BOX],
            |r| r.get(0),
        )
        .unwrap();
    pass(&shared, &mut sts, &mut cleared);
    assert!(!ow_core::metadata::present(BOX, generation as u64));
    sts.during = None;
    pass(&shared, &mut sts, &mut cleared);
    assert!(ow_core::metadata::present(BOX, generation as u64 + 1));
    // The operator removes the role while STS answers: what was assumed is not delivered.
    ow_core::metadata::clear(BOX);
    shared
        .lock()
        .unwrap()
        .db
        .execute("UPDATE aws_roles SET expires_at=NULL", [])
        .unwrap();
    let root = f.root.clone();
    sts.during = Some(Box::new(move |_| {
        let narrowed = json!({"mode":"metadata","allowed_roles":[{"role_arn":ADMIN_ONLY,"owners":["a@example.test"]}]});
        std::fs::write(root.join(CONFIG), narrowed.to_string()).unwrap();
    }));
    f.creds();
    pass(&shared, &mut sts, &mut cleared);
    assert!(!ow_core::metadata::present(BOX, generation as u64 + 1));
    assert!(
        f.creds()
            .iter()
            .all(|(r, _)| r.get("access_key_id").is_none())
    );
    ow_core::metadata::clear(BOX);
}

#[test]
fn ending_or_reowning_a_role_withdraws_it_before_commit() {
    let f = Fixture::new();
    f.lists.store(true, Ordering::SeqCst);
    f.configure(metadata_config());
    let shared = Arc::new(Mutex::new(Catalog::open(&f.root, "test", None).unwrap()));
    let mut sts = FakeSts::new(&shared);
    let mut cleared = HashSet::new();
    const BOX: &str = "m-meta-commit";
    const STOPPED: &str = "m-meta-commit-stopped";
    let (a, b) = {
        let mut c = shared.lock().unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        let b = c.user(&identity("b", "b@example.test")).unwrap();
        running(&c, a, "box", BOX);
        running(&c, a, "cold", STOPPED);
        for name in ["box", "cold"] {
            c.operation(a, json!({"op":"aws-role","id":name,"role_arn":READER}))
                .unwrap();
        }
        (a, b)
    };
    pass(&shared, &mut sts, &mut cleared);
    assert!(ow_core::metadata::present(BOX, 1));
    f.creds();
    // Detach: withdrawn while the attachment was still committed.
    shared
        .lock()
        .unwrap()
        .operation(a, json!({"op":"aws-role","id":"box"}))
        .unwrap();
    let detach = f.creds();
    assert_eq!(detach.len(), 1);
    assert!(detach[0].0.get("role_arn").is_none());
    assert_eq!(detach[0].1, Some((a, true)));
    assert!(!ow_core::metadata::present(BOX, 1));
    // Re-attach a different role: the runner swaps to a wait on generation 2 first.
    shared
        .lock()
        .unwrap()
        .operation(
            a,
            json!({"op":"aws-role","id":"box","role_arn":ADMIN_ONLY,"hop_limit":2}),
        )
        .unwrap();
    let reattach = f.creds();
    assert_eq!(reattach[0].0["role_arn"], ADMIN_ONLY);
    assert_eq!(reattach[0].0["hop_limit"], 2);
    assert_eq!(reattach[0].1, Some((a, false)));
    pass(&shared, &mut sts, &mut cleared);
    assert!(ow_core::metadata::present(BOX, 1));
    f.creds();

    // An unreachable runner refuses a transfer and a detach of the running box; nothing changes.
    f.down.store(true, Ordering::SeqCst);
    {
        let mut c = shared.lock().unwrap();
        let transfer = c.operation(a, json!({"op":"transfer","id":"box","to":"b@example.test"}));
        assert_eq!(refusal(transfer.unwrap_err()), "aws_runner_unreachable");
        let detach = c.operation(a, json!({"op":"aws-role","id":"box"}));
        assert_eq!(refusal(detach.unwrap_err()), "aws_runner_unreachable");
        let owner: i64 =
            c.db.query_row(
                "SELECT owner FROM resources WHERE physical=?1",
                [BOX],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, a);
        assert!(!crate::aws_identity::view(&c.db, BOX).unwrap().is_null());
        // A stopped box's runner holds no credentials: its transfer goes ahead.
        set_state(&c, STOPPED, "stopped");
        c.operation(
            a,
            json!({"op":"transfer","id":"cold","to":"b@example.test"}),
        )
        .unwrap();
        assert!(crate::aws_identity::view(&c.db, STOPPED).unwrap().is_null());
    }
    // Also when the operator just removed the file or switched modes: the runner was given
    // credentials for this attachment, so it must be reached first.
    for config in [
        None,
        Some(json!({"issuer":ISSUER,"allowed_roles":[{"role_arn":READER}]})),
    ] {
        match &config {
            None => std::fs::remove_file(f.root.join(CONFIG)).unwrap(),
            Some(config) => f.configure(config.clone()),
        }
        let mut c = shared.lock().unwrap();
        let transfer = c.operation(a, json!({"op":"transfer","id":"box","to":"b@example.test"}));
        assert_eq!(
            refusal(transfer.unwrap_err()),
            "aws_runner_unreachable",
            "{config:?}"
        );
    }
    f.configure(metadata_config());
    f.down.store(false, Ordering::SeqCst);
    assert!(ow_core::metadata::present(BOX, 1));

    // Transfer: withdrawn while the old owner and the role were still committed.
    shared
        .lock()
        .unwrap()
        .operation(a, json!({"op":"transfer","id":"box","to":"b@example.test"}))
        .unwrap();
    let transfer = f.creds();
    assert_eq!(transfer.len(), 1);
    assert!(transfer[0].0.get("role_arn").is_none());
    assert_eq!(transfer[0].1, Some((a, true)));
    assert!(!ow_core::metadata::present(BOX, 1));
    let c = shared.lock().unwrap();
    assert!(crate::aws_identity::view(&c.db, BOX).unwrap().is_null());
    let owner: i64 =
        c.db.query_row(
            "SELECT owner FROM resources WHERE physical=?1",
            [BOX],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner, b);
    drop(c);
    // The new owner's box has no role: the refresher assumes nothing for it.
    let before = sts.calls.len();
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(sts.calls.len(), before);
    ow_core::metadata::clear(STOPPED);
}

#[test]
fn switching_back_to_web_identity_withdraws_served_credentials() {
    let f = Fixture::new();
    f.lists.store(true, Ordering::SeqCst);
    f.configure(metadata_config());
    let shared = Arc::new(Mutex::new(Catalog::open(&f.root, "test", None).unwrap()));
    let mut sts = FakeSts::new(&shared);
    let mut cleared = HashSet::new();
    const BOX: &str = "m-meta-switch";
    {
        let mut c = shared.lock().unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        running(&c, a, "box", BOX);
        c.operation(a, json!({"op":"aws-role","id":"box","role_arn":READER}))
            .unwrap();
    }
    pass(&shared, &mut sts, &mut cleared);
    assert!(ow_core::metadata::present(BOX, 1));
    // The same role, now through web identity: the runner's metadata copy goes first.
    f.configure(json!({"issuer":ISSUER,"allowed_roles":[{"role_arn":READER}]}));
    pass(&shared, &mut sts, &mut cleared);
    assert!(!ow_core::metadata::present(BOX, 1));
    assert!(f.requests().iter().any(|r| r.get("token").is_some()));
    let served: bool = shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT served FROM aws_roles WHERE physical=?1",
            [BOX],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!served);
}
