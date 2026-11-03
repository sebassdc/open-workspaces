//! Workload tokens: scoped API credentials for a program, not a person. A token is bound to
//! one owner and one pool, and reaches only that owner's own boxes through create, put, exec,
//! stop and delete. Only its SHA-256 is stored; the token itself is shown once, at issue.
use crate::catalog::Identity;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

/// Every token starts with this, so the gateway never mistakes another bearer scheme for one.
pub(crate) const PREFIX: &str = "owt_";
const OPERATIONS: [&str; 5] = ["create", "put", "exec", "stop", "delete"];

/// Additive, like grants: an older binary ignores the table, so tokens stop working.
pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS workload_tokens(name TEXT PRIMARY KEY, owner INTEGER NOT NULL REFERENCES users(id), pool TEXT NOT NULL, hash TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);")?;
    Ok(())
}

/// The identity a presented token acts as, and its pool. Revoked or unknown: None.
pub(crate) fn verify(db: &Connection, presented: &str) -> Result<Option<(Identity, String)>> {
    if !presented.starts_with(PREFIX) || presented.len() > 256 {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT u.issuer,u.subject,u.email,t.pool FROM workload_tokens t JOIN users u ON u.id=t.owner WHERE t.hash=?1 AND u.subject IS NOT NULL",
            [crate::nodes::digest(presented)],
            |r| {
                Ok((
                    Identity {
                        issuer: r.get(0)?,
                        subject: r.get(1)?,
                        email: r.get(2)?,
                    },
                    r.get(3)?,
                ))
            },
        )
        .optional()?)
}

/// Narrow a validated dashboard operation to what a token bound to `pool` may do.
pub(crate) fn scoped(mut operation: Value, pool: &str) -> Result<Value> {
    let op = operation["op"].as_str().unwrap_or_default().to_owned();
    ensure!(
        OPERATIONS.contains(&op.as_str()),
        "a workload token may only create, put, exec, stop and delete; not {op}"
    );
    ensure!(
        !operation["id"].as_str().unwrap_or_default().contains('/'),
        "a workload token reaches only its owner's own boxes"
    );
    ensure!(
        operation.get("owner").is_none(),
        "a workload token creates boxes only for its owner"
    );
    if op == "create" {
        let asked = operation["pool"].as_str().unwrap_or(pool);
        ensure!(
            asked == pool,
            "this workload token creates only in pool '{pool}'"
        );
        operation["pool"] = json!(pool);
        operation["pool_grant"] = json!(pool);
    }
    Ok(operation)
}

/// The catalog user a token acts as. An email nobody has signed in with becomes a
/// program-only owner: its subject is set, so no later login can claim it.
fn owner(db: &Connection, email: &str) -> Result<i64> {
    let email = email.to_ascii_lowercase();
    crate::common::email(&email)?;
    let subject = format!("workload:{email}");
    let rows: Vec<i64> = db
        .prepare("SELECT id FROM users WHERE email=?1")?
        .query_map([&email], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        rows.len() <= 1,
        "{email} belongs to more than one identity provider"
    );
    if let Some(id) = rows.first() {
        db.execute(
            "UPDATE users SET subject=?1 WHERE id=?2 AND subject IS NULL",
            params![subject, id],
        )?;
        return Ok(*id);
    }
    let issuers: Vec<String> = db
        .prepare("SELECT DISTINCT issuer FROM users")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        issuers.len() == 1,
        "the controller's identity provider is not known yet; sign in to the dashboard once first"
    );
    db.execute(
        "INSERT INTO users(issuer,subject,email) VALUES(?1,?2,?3)",
        params![issuers[0], subject, email],
    )?;
    Ok(db.last_insert_rowid())
}

fn open(root: &Path) -> Result<Connection> {
    let db = crate::nodes::db(root)?;
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='users')",
            [],
            |r| r.get::<_, bool>(0)
        )?,
        "no workspace catalog here; run this on the controller with its data directory"
    );
    initialize(&db)?;
    Ok(db)
}

pub fn command(root: &Path, action: crate::TokenAction) -> Result<i32> {
    println!("{:#}", run(&open(root)?, action)?);
    Ok(0)
}

pub(crate) fn run(db: &Connection, action: crate::TokenAction) -> Result<Value> {
    Ok(match action {
        crate::TokenAction::Issue { name, owner, pool } => {
            crate::common::identifier(&name)?;
            crate::common::identifier(&pool)?;
            let tx = db.unchecked_transaction()?;
            let id = self::owner(&tx, &owner)?;
            let token = format!("{PREFIX}{}", crate::nodes::random_secret()?);
            tx.execute(
                "INSERT INTO workload_tokens(name,owner,pool,hash) VALUES(?1,?2,?3,?4)",
                params![name, id, pool, crate::nodes::digest(&token)],
            )
            .with_context(|| format!("token '{name}' already exists; revoke it first"))?;
            tx.commit()?;
            json!({"name":name,"owner":owner.to_ascii_lowercase(),"owner_id":id,"pool":pool,"token":token,"note":"Shown once; store it now."})
        }
        crate::TokenAction::Revoke { name } => {
            ensure!(
                db.execute("DELETE FROM workload_tokens WHERE name=?1", [&name])? == 1,
                "unknown token '{name}'"
            );
            json!({"revoked":name})
        }
        crate::TokenAction::List => {
            let rows: Vec<Value> = db
                .prepare("SELECT t.name,u.email,t.owner,t.pool,t.created_at FROM workload_tokens t JOIN users u ON u.id=t.owner ORDER BY t.name")?
                .query_map([], |r| {
                    Ok(json!({"name":r.get::<_,String>(0)?,"owner":r.get::<_,String>(1)?,"owner_id":r.get::<_,i64>(2)?,"pool":r.get::<_,String>(3)?,"created_at":r.get::<_,String>(4)?}))
                })?
                .collect::<rusqlite::Result<_>>()?;
            json!(rows)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> (std::path::PathBuf, crate::catalog::Catalog) {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("ow-tokens-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = crate::catalog::Catalog::open(&root, "issuer", None).unwrap();
        (root, catalog)
    }
    fn issue(db: &Connection, name: &str, owner: &str, pool: &str) -> Result<Value> {
        run(
            db,
            crate::TokenAction::Issue {
                name: name.into(),
                owner: owner.into(),
                pool: pool.into(),
            },
        )
    }

    #[test]
    fn scope_allows_only_own_boxes_and_its_pool() {
        for op in OPERATIONS {
            assert!(scoped(json!({"op":op,"id":"job"}), "jobs").is_ok(), "{op}");
        }
        for op in [
            "start",
            "get",
            "resize",
            "snapshot",
            "fork",
            "restore",
            "share",
            "unshare",
            "rename",
            "transfer",
            "hibernate",
            "move",
            "park",
            "wake",
            "publish",
            "aws-role",
            "ssh-keys",
            "ssh-info",
            "snapshot-delete",
            "transfer-control",
        ] {
            assert!(scoped(json!({"op":op,"id":"job"}), "jobs").is_err(), "{op}");
        }
        assert!(scoped(json!({"op":"exec","id":"alice@example.test/job"}), "jobs").is_err());
        assert!(
            scoped(
                json!({"op":"create","id":"job","owner":"alice@example.test"}),
                "jobs"
            )
            .is_err()
        );
        assert!(scoped(json!({"op":"create","id":"job","pool":"default"}), "jobs").is_err());
        for create in [
            json!({"op":"create","id":"job"}),
            json!({"op":"create","id":"job","pool":"jobs"}),
        ] {
            let scoped = scoped(create, "jobs").unwrap();
            assert_eq!(
                (&scoped["pool"], &scoped["pool_grant"]),
                (&json!("jobs"), &json!("jobs"))
            );
        }
        // Only create carries pool authority.
        assert!(scoped(json!({"op":"exec","id":"job"}), "jobs").unwrap()["pool_grant"].is_null());
    }

    #[test]
    fn issue_stores_only_a_hash_and_revoke_is_immediate() {
        let (root, mut c) = catalog();
        // No signed-in user yet: the identity provider is unknown.
        assert!(issue(&c.db, "runner", "jobs@example.test", "jobs").is_err());
        let human = c
            .user(&Identity {
                issuer: "issuer".into(),
                subject: "s-1".into(),
                email: "alice@example.test".into(),
            })
            .unwrap();
        let issued = issue(&c.db, "runner", "Jobs@Example.test", "jobs").unwrap();
        let token = issued["token"].as_str().unwrap().to_owned();
        assert!(token.starts_with(PREFIX) && token.len() == PREFIX.len() + 64);
        assert!(issue(&c.db, "runner", "jobs@example.test", "jobs").is_err());
        let stored: String =
            c.db.query_row(
                "SELECT group_concat(name||pool||hash) FROM workload_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!stored.contains(&token[PREFIX.len()..]));
        let (identity, pool) = verify(&c.db, &token).unwrap().unwrap();
        assert_eq!(
            (
                identity.email.as_str(),
                identity.subject.as_str(),
                pool.as_str()
            ),
            ("jobs@example.test", "workload:jobs@example.test", "jobs")
        );
        // The program-only owner is a catalog user whom no login can claim.
        let owner = c.user(&identity).unwrap();
        assert_eq!(owner, issued["owner_id"].as_i64().unwrap());
        assert!(
            c.user(&Identity {
                issuer: "issuer".into(),
                subject: "attacker".into(),
                email: "jobs@example.test".into(),
            })
            .is_err()
        );
        // A token for a person who signed in acts as that person.
        let second = issue(&c.db, "alice-runner", "alice@example.test", "default").unwrap();
        assert_eq!(second["owner_id"], human);
        assert_eq!(
            verify(&c.db, second["token"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .0
                .subject,
            "s-1"
        );
        let listed = run(&c.db, crate::TokenAction::List).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert!(!listed.to_string().contains(PREFIX));
        for wrong in [
            "",
            "owt_",
            "secret",
            &format!("{token}x"),
            &token[..token.len() - 1],
        ] {
            assert!(verify(&c.db, wrong).unwrap().is_none(), "{wrong}");
        }
        run(
            &c.db,
            crate::TokenAction::Revoke {
                name: "runner".into(),
            },
        )
        .unwrap();
        assert!(verify(&c.db, &token).unwrap().is_none());
        assert!(
            run(
                &c.db,
                crate::TokenAction::Revoke {
                    name: "runner".into()
                }
            )
            .is_err()
        );
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
}
