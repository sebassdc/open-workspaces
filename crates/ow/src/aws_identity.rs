//! Per-box AWS access: the controller is an OpenID Connect identity provider for its boxes.
//!
//! Off unless the operator writes `aws-identity.json` (issuer URL and role allow-list) next to
//! the catalog. An owner or admin attaches one allowed role to a box; the controller then signs
//! a short-lived ES256 token for that box generation and the worker writes it into the running
//! guest ([`ow_core::web_identity`]), where the AWS SDKs exchange it with
//! `AssumeRoleWithWebIdentity`. The controller holds no AWS credentials. See docs/AWS_ACCESS.md.
use crate::catalog::{Catalog, Refusal};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ow_core::{common, credential_broker::WorkloadIdentity};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) const CONFIG: &str = "aws-identity.json";
pub(crate) const AUDIENCE: &str = "sts.amazonaws.com";
/// Token lifetime. A token copied out of a box (or into a fork's RAM) stays valid this long.
pub(crate) const TTL: u64 = 900;
/// Re-issue once less than this remains, so a running box always holds a usable token.
const REFRESH_BEFORE: u64 = 600;
pub(crate) const DISCOVERY: &str = "/.well-known/openid-configuration";
pub(crate) const JWKS: &str = "/.well-known/jwks.json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Public HTTPS URL that serves the discovery document and JWKS, as AWS fetches them.
    pub issuer: String,
    pub allowed_roles: Vec<AllowedRole>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AllowedRole {
    pub role_arn: String,
    /// Box owners (login emails) who may attach it; empty means every user of this controller.
    #[serde(default)]
    pub owners: Vec<String>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.issuer).context("invalid issuer URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && !self.issuer.ends_with('/')
                && self.issuer.len() <= 255,
            "issuer must be an https:// URL without port, query or trailing slash"
        );
        ensure!(self.allowed_roles.len() <= 100, "at most 100 allowed roles");
        for role in &self.allowed_roles {
            ow_core::credential_broker::role_arn(&role.role_arn)?;
            for owner in &role.owners {
                common::email(owner)?;
            }
        }
        Ok(())
    }

    fn allows(&self, role_arn: &str, owner: &str) -> bool {
        self.allowed_roles.iter().any(|r| {
            r.role_arn == role_arn
                && (r.owners.is_empty() || r.owners.iter().any(|o| o.eq_ignore_ascii_case(owner)))
        })
    }

    /// The roles `owner` may attach to their boxes.
    pub(crate) fn roles_for(&self, owner: &str) -> Vec<&str> {
        self.allowed_roles
            .iter()
            .filter(|r| self.allows(&r.role_arn, owner))
            .map(|r| r.role_arn.as_str())
            .collect()
    }
}

/// The operator's configuration, or `None` while per-box AWS access is off.
pub(crate) fn config(root: &Path) -> Result<Option<Config>> {
    let path = root.join(CONFIG);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return Ok(None);
    };
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0,
        "{CONFIG} must be a regular file owned by the controller user and not group/world writable"
    );
    let config: Config = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("invalid {CONFIG}"))?;
    config.validate()?;
    Ok(Some(config))
}

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    // Additive: an older binary ignores it, so boxes simply stop receiving new tokens.
    // `owner` pins the attachment to who made it: a box with another owner has no role.
    db.execute_batch("CREATE TABLE IF NOT EXISTS aws_roles(physical TEXT PRIMARY KEY,owner INTEGER NOT NULL REFERENCES users(id),role_arn TEXT NOT NULL,generation INTEGER NOT NULL CHECK(generation>0),node TEXT,epoch INTEGER,fork_mark TEXT,expires_at INTEGER,error TEXT,updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);")?;
    Ok(())
}

/// One P-256 signing key per box generation, in an owner-only directory. The JWKS lists
/// exactly the current ones: dropping a generation's key is what makes AWS refuse its
/// tokens, since AWS cannot know a box moved, was forked or changed owner.
pub(crate) struct Signer {
    pkcs8: Vec<u8>,
    public: Vec<u8>,
    kid: String,
}

/// Directory of `<box>.<generation>.p8` keys and the never-signing `anchor.p8`.
const KEYS: &str = "aws-identity-keys";
/// AWS reads at most 100 EC keys from a JWKS; the anchor takes one.
pub(crate) const MAX_ATTACHED: usize = 99;

fn keys(root: &Path) -> Result<std::path::PathBuf> {
    let dir = root.join(KEYS);
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "{KEYS} must be an owner-only directory"
    );
    Ok(dir)
}

impl Signer {
    fn open(path: &Path, create: bool) -> Result<Self> {
        if create && !path.exists() {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                .map_err(|_| anyhow::anyhow!("signing key generation failed"))?;
            let staging = path.with_extension(format!("{}.tmp", common::nonce()?));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&staging)?;
            file.write_all(pkcs8.as_ref())?;
            file.sync_all()?;
            std::fs::rename(&staging, path)?;
        }
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "signing keys must be owner-only regular files"
        );
        let pkcs8 = std::fs::read(path)?;
        let pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &pkcs8,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|_| anyhow::anyhow!("invalid signing key"))?;
        let public = pair.public_key().as_ref().to_vec();
        ensure!(
            public.len() == 65 && public[0] == 4,
            "unexpected public key"
        );
        let kid = URL_SAFE_NO_PAD.encode(&Sha256::digest(&public)[..12]);
        Ok(Self { pkcs8, public, kid })
    }

    /// The key for this box generation, created on first use; every other generation's key
    /// of the box is removed first, so their tokens stop verifying.
    pub(crate) fn current(root: &Path, physical: &str, generation: i64) -> Result<Self> {
        common::identifier(physical)?;
        let name = format!("{physical}.{generation}.p8");
        revoke_except(root, physical, Some(&name))?;
        Self::open(&keys(root)?.join(name), true)
    }

    fn jwk(&self) -> Value {
        json!({"kty":"EC","crv":"P-256","alg":"ES256","use":"sig","kid":self.kid,
            "x":URL_SAFE_NO_PAD.encode(&self.public[1..33]),"y":URL_SAFE_NO_PAD.encode(&self.public[33..])})
    }

    fn sign(&self, claims: &Value) -> Result<String> {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        Ok(jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_ec_der(&self.pkcs8),
        )?)
    }
}

/// Withdraw every signing key of a box (detach, owner change, allow-list removal, delete).
pub(crate) fn revoke(root: &Path, physical: &str) -> Result<()> {
    revoke_except(root, physical, None)
}

fn revoke_except(root: &Path, physical: &str, keep: Option<&str>) -> Result<()> {
    let prefix = format!("{physical}.");
    for entry in std::fs::read_dir(keys(root)?)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && Some(name.as_ref()) != keep {
            std::fs::remove_file(root.join(KEYS).join(name.as_ref()))?;
        }
    }
    Ok(())
}

/// The published key set: every current box generation, plus a key that never signs so
/// the set is not empty before the first attachment (AWS requires one).
pub(crate) fn jwks(root: &Path) -> Result<Value> {
    let dir = keys(root)?;
    let mut keys = vec![Signer::open(&dir.join("anchor.p8"), true)?.jwk()];
    let mut names = std::fs::read_dir(&dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    for name in names {
        if name != "anchor.p8" && name.ends_with(".p8") {
            keys.push(Signer::open(&dir.join(name), false)?.jwk());
        }
    }
    Ok(json!({"keys":keys}))
}

pub(crate) fn discovery(issuer: &str) -> Value {
    json!({"issuer":issuer,"jwks_uri":format!("{issuer}{JWKS}"),
        "response_types_supported":["id_token"],"subject_types_supported":["public"],
        "id_token_signing_alg_values_supported":["ES256"],
        "claims_supported":["iss","sub","aud","iat","nbf","exp","jti","ow_box","ow_owner","ow_generation"]})
}

/// The unauthenticated documents AWS fetches, when per-box AWS access is configured.
pub(crate) fn public_document(root: &Path, path: &str) -> Option<Value> {
    let config = config(root).ok()??;
    match path {
        DISCOVERY => Some(discovery(&config.issuer)),
        JWKS => jwks(root).ok(),
        _ => None,
    }
}

/// `sub`: stable for a box and its owner, so trust policies can match one box or one owner.
pub(crate) fn subject(owner: &str, physical: &str) -> String {
    format!("owner/{}/box/{physical}", owner.to_ascii_lowercase())
}

pub(crate) fn claims(
    issuer: &str,
    owner: &str,
    workload: &WorkloadIdentity,
    now: u64,
) -> Result<Value> {
    Ok(
        json!({"iss":issuer,"sub":subject(owner,&workload.box_id),"aud":AUDIENCE,
        "iat":now,"nbf":now,"exp":now+TTL,"jti":common::nonce()?,
        "ow_box":workload.box_id,"ow_owner":owner.to_ascii_lowercase(),"ow_generation":workload.generation}),
    )
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The attachment shown by `ow inspect` and the dashboard, or null.
pub(crate) fn view(db: &Connection, physical: &str) -> Result<Value> {
    Ok(db
        .query_row(
            "SELECT a.role_arn,a.generation,a.expires_at,a.error,a.owner=r.owner,u.email FROM aws_roles a JOIN resources r ON r.kind='machine' AND r.physical=a.physical JOIN users u ON u.id=r.owner WHERE a.physical=?1",
            [physical],
            |r| {
                let (role, generation, expires, error, current, owner): (String, i64, Option<i64>, Option<String>, bool, String) =
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
                let state = if !current {
                    "inactive"
                } else if error.is_some() {
                    "error"
                } else if expires.is_some_and(|e| e as u64 > now()) {
                    "active"
                } else {
                    // Delivered when the box next runs.
                    "pending"
                };
                Ok(json!({"role_arn":role,"state":state,"token_expires_at":expires,"generation":generation,
                    "subject":subject(&owner, physical),"error":error}))
            },
        )
        .optional()?
        .unwrap_or(Value::Null))
}

/// One unit of refresher work, each done under its own catalog lock.
pub(crate) enum Work {
    Attached(String),
    /// A running box without a role, by incarnation: clear anything its source left behind.
    /// Keyed with its last restore or start too: a local restore keeps node and epoch but
    /// brings back RAM from a capture, which may hold token files.
    Bare((String, String, i64, i64)),
}

impl Catalog {
    /// `{"op":"aws-role","id":NAME,"role_arn":ARN}` attaches; without `role_arn` it detaches.
    pub fn aws_role(&mut self, user: i64, request: &Value) -> Result<Value> {
        let name = request["id"].as_str().context("id")?;
        let physical = self.machine(user, name, "aws-role")?;
        let role = request["role_arn"].as_str();
        if let Some(role) = role {
            let config = config(&self.root)?.ok_or_else(|| Refusal {
                code: "aws_access_off",
                message: "Per-box AWS access is not configured on this server; ask its operator."
                    .into(),
            })?;
            let (owner, email): (i64, String) = self.db.query_row(
                "SELECT r.owner,u.email FROM resources r JOIN users u ON u.id=r.owner WHERE r.kind='machine' AND r.physical=?1",
                [&physical],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if !config.allows(role, &email) {
                return Err(Refusal {
                    code: "aws_role_not_allowed",
                    message: format!(
                        "{role} is not on this server's allow-list for {email}'s boxes; ask the operator to add it."
                    ),
                }
                .into());
            }
            let attached: i64 = self.db.query_row(
                "SELECT COUNT(*) FROM aws_roles WHERE physical<>?1",
                [&physical],
                |r| r.get(0),
            )?;
            if attached as usize >= MAX_ATTACHED {
                return Err(Refusal {
                    code: "aws_role_limit",
                    message: format!(
                        "This server already gives {MAX_ATTACHED} boxes an AWS role, the most one issuer can publish keys for; detach one first."
                    ),
                }
                .into());
            }
            // A new attachment is a new credential authority: a new generation, key and token.
            // Forks made before it are not this attachment's concern.
            let mark = self.fork_mark(&physical)?;
            revoke(&self.root, &physical)?;
            self.db.execute(
                "INSERT INTO aws_roles(physical,owner,role_arn,generation,fork_mark) VALUES(?1,?2,?3,1,?4)
                 ON CONFLICT(physical) DO UPDATE SET owner=excluded.owner,role_arn=excluded.role_arn,generation=generation+1,node=NULL,epoch=NULL,fork_mark=excluded.fork_mark,expires_at=NULL,error=NULL,updated_at=CURRENT_TIMESTAMP",
                params![physical, owner, role, mark],
            )?;
        } else {
            self.db
                .execute("DELETE FROM aws_roles WHERE physical=?1", [&physical])?;
            revoke(&self.root, &physical)?;
        }
        // Deliver (or remove) now; a stopped box gets its token when it next runs.
        let _ = self.sync_aws(&physical, true);
        Ok(json!({"id":name,"aws_role":view(&self.db, &physical)?}))
    }

    /// Changes whenever a fork of this box is started or made (here or on another runner):
    /// a fork's RAM holds the parent's token, so the parent then moves to a new generation.
    fn fork_mark(&self, physical: &str) -> Result<String> {
        Ok(self.db.query_row(
            "SELECT (SELECT COUNT(*)||'/'||COALESCE(MAX(rowid),0) FROM resources WHERE kind='machine' AND json_extract(metadata,'$.source')=?1)||':'||(SELECT COUNT(*)||'/'||COALESCE(MAX(id),0) FROM transfers WHERE kind='fork' AND source=?1)",
            [physical],
            |r| r.get(0),
        )?)
    }

    /// Bring one box's guest in line with its attachment. `force` re-issues a valid token.
    /// Returns whether the worker wrote to a running guest.
    pub(crate) fn sync_aws(&mut self, physical: &str, force: bool) -> Result<bool> {
        #[allow(clippy::type_complexity)]
        let row: Option<(String, i64, i64, Option<String>, Option<i64>, Option<i64>, i64, String, i64, String, Option<String>, Option<String>)> = self.db.query_row(
            "SELECT a.role_arn,a.owner,a.generation,a.node,a.epoch,a.expires_at,r.owner,r.node,r.placement_epoch,u.email,a.fork_mark,a.error FROM aws_roles a JOIN resources r ON r.kind='machine' AND r.physical=a.physical JOIN users u ON u.id=r.owner WHERE a.physical=?1",
            [physical],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)),
        ).optional()?;
        let moving: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1)",
            [physical],
            |r| r.get(0),
        )?;
        let node: Option<String> = self
            .db
            .query_row(
                "SELECT node FROM resources WHERE kind='machine' AND physical=?1",
                [physical],
                |r| r.get(0),
            )
            .optional()?;
        // Revoke even without a worker: copied tokens can outlive their placement.
        let Some(node) = node.filter(|n| !n.is_empty() && !moving) else {
            revoke(&self.root, physical)?;
            self.db.execute(
                "UPDATE aws_roles SET expires_at=NULL WHERE physical=?1",
                [physical],
            )?;
            return Ok(false);
        };
        let Some((
            role,
            owner,
            mut generation,
            seen_node,
            seen_epoch,
            expires,
            current_owner,
            _,
            epoch,
            email,
            seen_forks,
            seen_error,
        )) = row
        else {
            return self.deliver(&node, physical, None);
        };
        let config = config(&self.root)?;
        let allowed = config.as_ref().is_some_and(|c| c.allows(&role, &email));
        if owner != current_owner || !allowed {
            let error = if owner != current_owner {
                "the box changed owner; its new owner must attach a role"
            } else {
                "role removed from the operator allow-list"
            };
            // Its tokens stop verifying once AWS re-reads the JWKS, whatever copies exist.
            revoke(&self.root, physical)?;
            if seen_error.as_deref() == Some(error) {
                return Ok(false);
            }
            self.db.execute(
                "UPDATE aws_roles SET expires_at=NULL,error=?2 WHERE physical=?1",
                params![physical, error],
            )?;
            return self.deliver(&node, physical, None);
        }
        let config = config.unwrap();
        // A move, a wake or a fork of the box is a new incarnation: a new generation, whose
        // new key replaces the old one in the JWKS, so tokens of the old one stop verifying.
        let forks = self.fork_mark(physical)?;
        let moved = seen_node.is_some()
            && (seen_node.as_deref() != Some(node.as_str()) || seen_epoch != Some(epoch));
        let forked = seen_forks.is_some() && seen_forks.as_deref() != Some(forks.as_str());
        if moved || forked || seen_node.is_none() || seen_forks.is_none() {
            if moved || forked {
                generation += 1;
            }
            self.db.execute(
                "UPDATE aws_roles SET generation=?2,node=?3,epoch=?4,fork_mark=?5,expires_at=NULL WHERE physical=?1",
                params![physical, generation, node, epoch, forks],
            )?;
        } else if !force
            && expires.is_some_and(|e| e as u64 > now() + REFRESH_BEFORE)
            // A cold restart emptied the guest's /run: the worker knows its VMM changed.
            && self.aws_call(&node, physical, json!({"check":true}))?["present"] == true
        {
            return Ok(false);
        }
        let workload = WorkloadIdentity {
            box_id: physical.to_owned(),
            generation: generation as u64,
        };
        let issued = now();
        let token = Signer::current(&self.root, physical, generation)?.sign(&claims(
            &config.issuer,
            &email,
            &workload,
            issued,
        )?)?;
        let session = workload.source_identity();
        let delivery = ow_core::web_identity::Delivery {
            token: &token,
            role_arn: &role,
            session_name: &session,
        };
        match self.deliver(&node, physical, Some(&delivery)) {
            Ok(written) => {
                self.db.execute(
                    "UPDATE aws_roles SET expires_at=?2,error=NULL WHERE physical=?1 AND generation=?3",
                    params![physical, written.then_some((issued + TTL) as i64), generation],
                )?;
                Ok(written)
            }
            Err(error) => {
                // Worker errors name no token: the guest command prints nothing.
                let text: String = format!("{error:#}").chars().take(300).collect();
                self.db.execute(
                    "UPDATE aws_roles SET expires_at=NULL,error=?2 WHERE physical=?1",
                    params![physical, text],
                )?;
                Err(error)
            }
        }
    }

    fn deliver(
        &self,
        node: &str,
        physical: &str,
        delivery: Option<&ow_core::web_identity::Delivery>,
    ) -> Result<bool> {
        let mut fields = json!({});
        if let Some(d) = delivery {
            d.validate()?;
            fields = json!({"token":d.token,"role_arn":d.role_arn,"session_name":d.session_name});
        }
        Ok(self.aws_call(node, physical, fields)?["written"] == true)
    }

    fn aws_call(&self, node: &str, physical: &str, fields: Value) -> Result<Value> {
        crate::nodes::operation_capable(&self.root, node, "aws-token")?;
        let mut request = fields;
        request["op"] = json!("aws-token");
        request["id"] = json!(physical);
        if let Some(placement) = self.managed_placement(physical)? {
            request["placement"] = json!(placement);
        }
        crate::wire::request(&crate::nodes::route(&self.root, node)?, request)
    }

    pub(crate) fn aws_work(&self) -> Result<Vec<Work>> {
        let mut work = self
            .db
            .prepare("SELECT physical FROM aws_roles")?
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|p| p.map(Work::Attached))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let bare = self.db.prepare("SELECT physical,node,placement_epoch,(SELECT COALESCE(MAX(o.id),0) FROM operations o WHERE (json_extract(o.request,'$.request.id')=r.physical OR (o.owner=r.owner AND o.resource=r.name)) AND o.op IN ('restore','start')) FROM resources r WHERE kind='machine' AND node<>'' AND json_extract(metadata,'$.state')='running' AND NOT EXISTS(SELECT 1 FROM aws_roles a WHERE a.physical=r.physical) AND NOT EXISTS(SELECT 1 FROM transfer_locks t WHERE t.machine=r.physical)")?
            .query_map([], |r| Ok(Work::Bare((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        work.extend(bare);
        Ok(work)
    }

    /// `cleared` remembers role-less incarnations already checked (a fork copies its parent's RAM).
    pub(crate) fn aws_step(
        &mut self,
        work: Work,
        cleared: &mut HashSet<(String, String, i64, i64)>,
    ) {
        match work {
            Work::Attached(physical) => {
                if let Err(error) = self.sync_aws(&physical, false) {
                    eprintln!("aws identity: {physical}: {error:#}");
                }
            }
            Work::Bare(key) => {
                if !cleared.contains(&key) && self.deliver(&key.1, &key.0, None).unwrap_or(false) {
                    cleared.insert(key);
                }
            }
        }
    }
}

/// Keep tokens fresh while `aws-identity.json` exists. Each box takes the catalog lock on its
/// own, so lifecycle and API calls interleave with a long pass.
pub(crate) fn start(shared: &Arc<Mutex<Catalog>>) {
    let root = shared.lock().unwrap().root.clone();
    if !root.join(CONFIG).exists() {
        return;
    }
    let weak = Arc::downgrade(shared);
    std::thread::spawn(move || {
        let mut cleared = HashSet::new();
        loop {
            let Some(shared) = weak.upgrade() else { break };
            let work = shared.lock().unwrap().aws_work();
            match work {
                Ok(work) => {
                    for item in work {
                        shared.lock().unwrap().aws_step(item, &mut cleared);
                    }
                }
                Err(error) => eprintln!("aws identity refresh failed: {error:#}"),
            }
            drop(shared);
            if crate::nodes::STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
#[path = "aws_identity_tests.rs"]
mod tests;
