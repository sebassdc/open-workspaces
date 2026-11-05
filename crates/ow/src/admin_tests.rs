//! Keeps the admin API, the CLI and docs/ADMIN_API.md in step, and checks the handlers.
use super::*;
use crate::{Cli, admin_client};
use clap::{CommandFactory, Parser};
use std::os::unix::fs::PermissionsExt;

/// Workspace commands: `POST /api/operation` or the local worker, not admin ops.
const WORKSPACE: &[&str] = &[
    "status",
    "stats",
    "list",
    "inspect",
    "create",
    "resize",
    "start",
    "park",
    "move",
    "wake",
    "rename",
    "transfer",
    "audit",
    "stop",
    "exec",
    "shell",
    "ssh",
    "ssh-proxy",
    "ssh-config",
    "ssh-authorize",
    "ssh-revoke",
    "ssh-info",
    "put",
    "get",
    "snapshot",
    "snapshots",
    "delete-snapshot",
    "fork",
    "hibernate",
    "share",
    "aws-role set",
    "aws-role unset",
    "unshare",
    "delete",
    "restore",
    "publish",
    "unpublish",
    "nodes",
    "image list",
];

/// Stays local by nature, with the reason (docs/ADMIN_API.md#commands-that-are-not-ops).
const LOCAL: &[(&str, &str)] = &[
    ("host up", "installs or starts the server itself"),
    ("host doctor", "checks this machine"),
    ("host join", "joins this machine as a runner"),
    ("host configure", "this machine's runner budget"),
    ("host update-assets", "this machine's runtime assets"),
    ("host rollback-assets", "this machine's runtime assets"),
    ("host start", "starts this machine's services"),
    ("host status", "this machine's services"),
    ("host stop", "stops this machine's services"),
    ("host run", "the host service process"),
    ("up", "starts the local worker"),
    ("down", "stops the local worker"),
    ("node-controller", "server process"),
    ("node-agent", "server process"),
    ("dashboard", "server process"),
    ("serve", "server process"),
    ("worker", "server process"),
    ("supervisor", "server process"),
    ("gateway", "server process"),
    ("backup snapshots", "this runner's data"),
    ("backup recover", "this runner's data"),
    ("backup restore", "this runner's data"),
    ("backup list", "reads a store directly"),
    ("image build", "builds on this machine's runner"),
    ("image pull", "pulls onto this machine's runner"),
    ("mac-host macos-probe", "this Mac"),
    ("mac-host capabilities", "this Mac"),
    ("mac-host boot", "this Mac"),
    ("mac-host restore", "this Mac"),
    ("backup catalog", "disaster recovery with the gateway down"),
    (
        "backup catalog-check",
        "disaster recovery with the gateway down",
    ),
    ("login", "client-side sign-in"),
];

/// Admin commands not yet ops; each gets its contract in its own change.
const PLANNED: &[&str] = &[
    "scaler",
    "dashboard-ui list",
    "dashboard-ui install",
    "node list",
    "node add",
    "image publish",
    "node drain",
    "store gc",
    "store verify",
    "store undo-plan",
];

fn leaves(command: &clap::Command, prefix: &[String], out: &mut Vec<String>) {
    let subcommands: Vec<_> = command
        .get_subcommands()
        .filter(|c| c.get_name() != "help")
        .collect();
    if subcommands.is_empty() {
        out.push(prefix.join(" "));
    }
    for sub in subcommands {
        let mut path = prefix.to_vec();
        path.push(sub.get_name().to_owned());
        leaves(sub, &path, out);
    }
}

#[test]
fn every_command_is_an_op_a_workspace_command_or_stays_local() {
    let mut all = vec![];
    leaves(&Cli::command(), &[], &mut all);
    let ops: Vec<String> = OPS
        .iter()
        .flat_map(|s| s.cli.iter().map(|path| path.join(" ")))
        .collect();
    for leaf in &all {
        let homes = usize::from(ops.contains(leaf))
            + usize::from(WORKSPACE.contains(&leaf.as_str()))
            + usize::from(LOCAL.iter().any(|(c, _)| c == leaf))
            + usize::from(PLANNED.contains(&leaf.as_str()));
        assert_eq!(
            homes, 1,
            "`ow {leaf}` must be exactly one of: an admin op (admin::OPS), a workspace command, \
             a local command with a reason, or a planned op"
        );
    }
    let listed = ops
        .iter()
        .map(String::as_str)
        .chain(WORKSPACE.iter().copied())
        .chain(LOCAL.iter().map(|(c, _)| *c))
        .chain(PLANNED.iter().copied());
    for entry in listed {
        assert!(
            all.iter().any(|l| l == entry)
                || (entry.starts_with("mac-host ") && !cfg!(target_os = "macos")),
            "`ow {entry}` is listed but is not a command"
        );
    }
    for spec in OPS {
        assert!(!spec.cli.is_empty(), "{} has no command", spec.name);
    }
}

/// (op, request, response) for each `<!-- admin:example op=... -->` in docs/ADMIN_API.md.
fn examples() -> Vec<(String, Value, Value)> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/ADMIN_API.md"
    ))
    .unwrap();
    let mut out = vec![];
    for chunk in text.split("<!-- admin:example op=").skip(1) {
        let op = chunk[..chunk.find(" -->").unwrap()].to_owned();
        let mut blocks = chunk.split("```json\n").skip(1).take(2).map(|b| {
            let body = &b[..b.find("```").unwrap()];
            serde_json::from_str::<Value>(body)
                .unwrap_or_else(|e| panic!("example for {op} is not JSON: {e}\n{body}"))
        });
        out.push((op.clone(), blocks.next().unwrap(), blocks.next().unwrap()));
    }
    out
}

#[test]
fn documented_examples_parse_and_cover_every_op() {
    let examples = examples();
    for spec in OPS {
        assert!(
            examples.iter().any(|(op, ..)| op == spec.name),
            "docs/ADMIN_API.md has no example for {}",
            spec.name
        );
    }
    for (op, request, response) in &examples {
        assert_eq!(request["op"], *op);
        assert!(spec(op).is_some(), "documented op {op} is not registered");
        parse(request).unwrap_or_else(|f| panic!("{op} example request: {}", f.message));
        assert_eq!(response["op"], *op);
        assert!(response["ok"].is_boolean());
        if response["ok"] == true {
            assert!(response["result"].is_object(), "{op}");
        } else {
            assert!(
                response["error_code"].is_string() && response["details"].is_object(),
                "{op}"
            );
        }
    }
}

#[test]
fn the_cli_builds_the_documented_request() {
    let first = |op: &str| examples().into_iter().find(|(o, ..)| o == op).unwrap().1;
    let cases: &[(&str, &[&str])] = &[
        ("config.list", &["config", "list"]),
        ("config.get", &["config", "get", "telemetry.interval_s"]),
        (
            "config.set",
            &[
                "config",
                "set",
                "telemetry.interval_s",
                "30",
                "--expect-version",
                "41",
            ],
        ),
        (
            "config.history",
            &["config", "history", "telemetry.interval_s", "--limit", "2"],
        ),
        ("health.get", &["health"]),
        ("node.pool.set", &["node-pool", "n1", "ci"]),
        (
            "token.issue",
            &[
                "--operation-key",
                "k7f2",
                "token",
                "issue",
                "ci-broker",
                "--owner",
                "ci@example.com",
                "--pool",
                "ci",
            ],
        ),
        ("token.revoke", &["token", "revoke", "ci-broker"]),
        ("token.list", &["token", "list"]),
        (
            "host.invite",
            &[
                "--operation-key",
                "k9a1",
                "host",
                "invite",
                "n3",
                "--memory",
                "8192",
            ],
        ),
        (
            "host.invite",
            &[
                "--operation-key",
                "k9a1",
                "node-join",
                "n3",
                "--output",
                "x",
                "--memory",
                "8192",
                "--cpus",
                "2",
                "--controller",
                "https://ignored.example.com",
            ],
        ),
        (
            "host.budget",
            &[
                "host", "budget", "n3", "--memory", "16384", "--slots", "4", "--cpus", "4",
            ],
        ),
        ("host.revoke", &["host", "revoke", "n3"]),
        ("host.revoke", &["node-revoke", "n3"]),
    ];
    for (op, args) in cases {
        let cli = Cli::try_parse_from([&["ow"], *args].concat()).unwrap();
        let key = cli.operation_key.clone().unwrap_or_else(|| "unused".into());
        let built = admin_client::request(&cli.command, &key).unwrap();
        assert_eq!(built, first(op), "ow {}", args.join(" "));
    }
    for spec in OPS {
        assert!(
            cases.iter().any(|(op, _)| *op == spec.name),
            "no CLI case builds the documented {} request",
            spec.name
        );
    }
    for args in [
        &["nodes"][..],
        &["list"],
        &["dashboard-ui", "list"],
        &["scaler"],
    ] {
        let cli = Cli::try_parse_from([&["ow"], args].concat()).unwrap();
        assert!(
            admin_client::request(&cli.command, "k").is_none(),
            "{args:?}"
        );
    }
}

#[test]
fn requests_refuse_unknown_ops_fields_and_misplaced_keys() {
    assert_eq!(
        parse(&json!({"op":"config.drop"})).unwrap_err().code,
        "unknown_op"
    );
    assert_eq!(
        parse(&json!({"key":"x"})).unwrap_err().code,
        "invalid_request"
    );
    assert_eq!(parse(&json!([1])).unwrap_err().code, "invalid_request");
    for bad in [
        json!({"op":"config.list","extra":1}),
        json!({"op":"config.get"}),
        json!({"op":"config.get","key":5}),
        json!({"op":"config.set","key":"telemetry.enabled","value":"false","operation_key":"k"}),
        json!({"op":"token.issue","name":"a","owner":"a@example.com","pool":"p"}),
        json!({"op":"host.budget","node":"n","memory":-1,"slots":1,"cpus":1}),
    ] {
        assert_eq!(parse(&bad).unwrap_err().code, "invalid_request", "{bad}");
    }
    assert_eq!(
        parse(&json!({"op":"config.history"})).unwrap(),
        AdminRequest::ConfigHistory {
            key: None,
            limit: None
        }
    );
}

pub(super) fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("ow-admin-{}", crate::common::nonce().unwrap()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}
fn host() -> Actor {
    Actor::Host {
        uid: 0,
        claimed_user: Some("alice".into()),
    }
}
fn call(root: &Path, request: Value) -> (u16, Value) {
    let ctx = Context {
        root,
        actor: host(),
        controller: Some("https://ctl.example.test/_nodes".into()),
    };
    handle(&ctx, request.to_string().as_bytes())
}
fn audit_rows(root: &Path) -> Vec<(String, String, String)> {
    crate::nodes::db(root)
        .unwrap()
        .prepare("SELECT op,result,via FROM admin_audit ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn config_set_is_typed_idempotent_versioned_and_audited() {
    let root = temp_root();
    let (status, list) = call(&root, json!({"op":"config.list"}));
    assert_eq!(status, 200);
    assert_eq!(list["result"]["version"], 0);
    assert_eq!(list["result"]["settings"]["telemetry.interval_s"], 15);
    assert!(list["result"]["settings"].get("version").is_none());
    let (status, bad) = call(&root, json!({"op":"config.get","key":"no.such"}));
    assert_eq!(
        (status, bad["error_code"].as_str()),
        (400, Some("unknown_setting"))
    );

    let (status, set) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.interval_s","value":"30"}),
    );
    assert_eq!(status, 200, "{set}");
    let r = &set["result"];
    assert_eq!(
        (
            r["value"].clone(),
            r["previous"].clone(),
            r["changed"].clone(),
            r["version"].clone()
        ),
        (json!(30), json!(15), json!(true), json!(1))
    );
    assert_eq!(r["effective"], "next_ack");
    // Typed JSON stores the same as the string form; an equal value changes nothing.
    let (_, same) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.interval_s","value":30}),
    );
    assert_eq!(
        (
            same["result"]["changed"].clone(),
            same["result"]["version"].clone()
        ),
        (json!(false), json!(1))
    );
    let (status, wrong) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.interval_s","value":true}),
    );
    assert_eq!(
        (status, wrong["error_code"].as_str()),
        (422, Some("invalid_value"))
    );
    let (status, conflict) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.interval_s","value":"30","expected_version":0}),
    );
    assert_eq!(
        (status, conflict["error_code"].as_str()),
        (409, Some("version_conflict"))
    );
    assert_eq!(conflict["details"], json!({"expected":0,"current":1}));
    let (status, _) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.enabled","value":false,"expected_version":1}),
    );
    assert_eq!(status, 200);
    // Empty is the default.
    let (_, reset) = call(
        &root,
        json!({"op":"config.set","key":"telemetry.enabled","value":""}),
    );
    assert_eq!(
        (
            reset["result"]["value"].clone(),
            reset["result"]["version"].clone()
        ),
        (json!(true), json!(3))
    );
    let (_, got) = call(
        &root,
        json!({"op":"config.get","key":"telemetry.interval_s"}),
    );
    assert_eq!(
        got["result"],
        json!({"key":"telemetry.interval_s","value":30,"default":15,"version":3})
    );

    let changes: i64 = crate::nodes::db(&root)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM config_changes WHERE actor='host:alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        changes, 3,
        "unchanged and refused sets add no config_changes row"
    );
    let rows = audit_rows(&root);
    let results: Vec<&str> = rows.iter().map(|(_, r, _)| r.as_str()).collect();
    assert_eq!(
        results,
        [
            "ok",
            "unchanged",
            "invalid_value",
            "version_conflict",
            "ok",
            "ok"
        ]
    );
    assert!(
        rows.iter()
            .all(|(op, _, via)| op == "config.set" && via == "local")
    );

    let (_, history) = call(
        &root,
        json!({"op":"config.history","key":"telemetry.interval_s"}),
    );
    let changes = history["result"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["value"], 30);
    assert_eq!(changes[0]["previous"], 15);
    assert_eq!(
        changes[0]["actor"],
        json!({"kind":"host","uid":0,"claimed_user":"alice"})
    );
    let (status, _) = call(&root, json!({"op":"config.history","limit":0}));
    assert_eq!(status, 400);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn host_invite_keys_and_revoke_are_idempotent() {
    let root = temp_root();
    crate::limits::free_pool(&root);
    let invite = json!({"op":"host.invite","node":"n3","memory":1024,"operation_key":"k1"});
    let (status, first) = call(&root, invite.clone());
    assert_eq!(status, 200, "{first}");
    assert_eq!(
        first["result"]["controller"],
        "https://ctl.example.test/_nodes"
    );
    assert_eq!(first["result"]["secret"].as_str().unwrap().len(), 64);
    let (status, again) = call(&root, invite);
    assert_eq!(
        (status, again["error_code"].as_str()),
        (409, Some("already_exists"))
    );
    assert!(again["details"]["issued_unix_s"].is_i64());
    assert!(again.to_string().find("secret").is_none());
    let (status, reused) = call(
        &root,
        json!({"op":"host.invite","node":"n4","operation_key":"k1"}),
    );
    assert_eq!(
        (status, reused["error_code"].as_str()),
        (409, Some("operation_key_reused"))
    );
    let audit = crate::nodes::db(&root)
        .unwrap()
        .query_row(
            "SELECT request FROM admin_audit WHERE op='host.invite' AND result='ok'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert!(!audit.contains(first["result"]["secret"].as_str().unwrap()));

    let (status, missing) = call(&root, json!({"op":"host.revoke","node":"nope"}));
    assert_eq!(
        (status, missing["error_code"].as_str()),
        (404, Some("not_found"))
    );
    let (_, revoked) = call(&root, json!({"op":"host.revoke","node":"n3"}));
    assert_eq!(revoked["result"], json!({"revoked":"n3","changed":true}));
    let (_, again) = call(&root, json!({"op":"host.revoke","node":"n3"}));
    assert_eq!(again["result"], json!({"revoked":"n3","changed":false}));
    std::fs::remove_dir_all(root).unwrap();
}

/// Runs each documented example against a real controller database and checks that the
/// answer has the documented shape (and, for the documented errors, the same error).
#[cfg(target_os = "linux")]
#[test]
fn documented_examples_match_real_answers() {
    let root = temp_root();
    crate::limits::free_pool(&root);
    let mut catalog = crate::catalog::Catalog::open(&root, "issuer", None).unwrap();
    catalog
        .user(&crate::catalog::Identity {
            issuer: "issuer".into(),
            subject: "s".into(),
            email: "someone@example.com".into(),
        })
        .unwrap();
    drop(catalog);
    let (status, _) = call(
        &root,
        json!({"op":"host.invite","node":"n1","operation_key":"setup"}),
    );
    assert_eq!(status, 200);
    let mut seen_issue = false;
    for (op, mut request, documented) in examples() {
        if let Some(r) = request.as_object_mut() {
            r.remove("expected_version");
        }
        if op == "host.budget" {
            crate::nodes::db(&root)
                .unwrap()
                .execute(
                    "UPDATE nodes SET credential_hash='synthetic' WHERE id='n3'",
                    [],
                )
                .unwrap();
        }
        let (status, answer) = call(&root, request.clone());
        if op == "token.issue" && seen_issue {
            // The documented retry.
            assert_eq!(answer["error_code"], documented["error_code"]);
            continue;
        }
        seen_issue |= op == "token.issue";
        if documented["ok"] == true {
            assert_eq!(status, 200, "{op}: {answer}");
            let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
            assert_eq!(
                keys(&answer["result"]),
                keys(&documented["result"]),
                "{op} result fields"
            );
        } else {
            assert_eq!(answer, documented, "{op}");
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn node_pool_and_tokens() {
    let root = temp_root();
    crate::limits::free_pool(&root);
    let mut catalog = crate::catalog::Catalog::open(&root, "issuer", None).unwrap();
    catalog
        .user(&crate::catalog::Identity {
            issuer: "issuer".into(),
            subject: "s".into(),
            email: "someone@example.com".into(),
        })
        .unwrap();
    drop(catalog);
    call(
        &root,
        json!({"op":"host.invite","node":"n1","operation_key":"setup"}),
    );
    let (status, missing) = call(
        &root,
        json!({"op":"node.pool.set","node":"nope","pool":"ci"}),
    );
    assert_eq!(
        (status, missing["error_code"].as_str()),
        (404, Some("not_found"))
    );
    let (status, bad) = call(
        &root,
        json!({"op":"node.pool.set","node":"n1","pool":"bad pool"}),
    );
    assert_eq!(
        (status, bad["error_code"].as_str()),
        (422, Some("invalid_value"))
    );
    let (_, set) = call(&root, json!({"op":"node.pool.set","node":"n1","pool":"ci"}));
    assert_eq!(
        set["result"],
        json!({"node":"n1","pool":"ci","previous":"default","changed":true})
    );
    let (_, again) = call(&root, json!({"op":"node.pool.set","node":"n1","pool":"ci"}));
    assert_eq!(again["result"]["changed"], false);
    assert_eq!(
        crate::nodes::inventory(&root).unwrap()[0]["capabilities"]["pool"],
        "ci"
    );

    let issue = json!({"op":"token.issue","name":"bot","owner":"Bot@Example.com","pool":"ci","operation_key":"t1"});
    let (status, issued) = call(&root, issue.clone());
    assert_eq!(status, 200, "{issued}");
    let token = issued["result"]["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("owt_"));
    assert_eq!(issued["result"]["owner"], "bot@example.com");
    let (status, retried) = call(&root, issue);
    assert_eq!(
        (status, retried["error_code"].as_str()),
        (409, Some("already_exists"))
    );
    assert!(!retried.to_string().contains(&token));
    let (status, taken) = call(
        &root,
        json!({"op":"token.issue","name":"bot","owner":"x@example.com","pool":"ci","operation_key":"t2"}),
    );
    assert_eq!(
        (status, taken["error_code"].as_str()),
        (409, Some("already_exists"))
    );
    let (_, list) = call(&root, json!({"op":"token.list"}));
    assert_eq!(list["result"]["tokens"][0]["name"], "bot");
    assert!(!list.to_string().contains(&token));
    let dump: String = crate::nodes::db(&root)
        .unwrap()
        .prepare("SELECT request||result FROM admin_audit")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!dump.contains(&token), "the audit log never holds a token");
    let (_, revoked) = call(&root, json!({"op":"token.revoke","name":"bot"}));
    assert_eq!(revoked["result"], json!({"revoked":"bot","changed":true}));
    let (_, again) = call(&root, json!({"op":"token.revoke","name":"bot"}));
    assert_eq!(again["result"], json!({"revoked":"bot","changed":false}));
    let (status, never) = call(&root, json!({"op":"token.revoke","name":"never"}));
    assert_eq!(
        (status, never["error_code"].as_str()),
        (404, Some("not_found"))
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// The socket serves the same answers, to root or the controller's own user only, and
/// requires the request marker.
#[test]
fn admin_socket_answers_like_the_handler() {
    let root = temp_root();
    let served = root.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(serve_socket(served, None))
            .unwrap();
    });
    let socket = root.join(SOCKET);
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let request = json!({"op":"config.set","key":"alerts.disk_low_gib","value":"30"});
    let (status, body) =
        admin_client::socket_call(&socket, "POST", "/api/admin", Some(&request)).unwrap();
    assert_eq!(status, 200);
    let over_socket: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(over_socket["result"]["changed"], true);
    let (_, direct) = call(
        &root,
        json!({"op":"config.get","key":"alerts.disk_low_gib"}),
    );
    let (_, body) = admin_client::socket_call(
        &socket,
        "POST",
        "/api/admin",
        Some(&json!({"op":"config.get","key":"alerts.disk_low_gib"})),
    )
    .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), direct);
    let (_, body) = admin_client::socket_call(&socket, "GET", "/api/version", None).unwrap();
    let version: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        version["result"]["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!(CAPABILITY))
    );
    assert_eq!(
        version["result"]["admin_ops"].as_array().unwrap().len(),
        OPS.len()
    );
    let actor: String = crate::nodes::db(&root)
        .unwrap()
        .query_row("SELECT actor FROM admin_audit WHERE via='local'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&actor).unwrap()["uid"],
        unsafe { libc::geteuid() }
    );
    assert!(peer_allowed(0) && peer_allowed(unsafe { libc::geteuid() }));
    assert!(
        !peer_allowed(unsafe { libc::geteuid() } + 1) || unsafe { libc::geteuid() } == u32::MAX
    );
    // Without the request marker: refused.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "content-type",
        "application/json; charset=utf-8".parse().unwrap(),
    );
    assert_eq!(
        shape(&headers, false, 10).unwrap().code,
        "same_origin_required"
    );
    headers.insert("x-ow-request", "dashboard".parse().unwrap());
    assert!(shape(&headers, false, 10).is_none());
    assert_eq!(shape(&headers, true, 10).unwrap().code, "invalid_request");
    assert_eq!(
        shape(&headers, false, BODY_LIMIT + 1).unwrap().code,
        "too_large"
    );
    std::fs::remove_dir_all(root).unwrap();
}
