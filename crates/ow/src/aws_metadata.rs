//! Controller side of the metadata mode of per-box AWS access (ADR 0009): the controller
//! assumes each attached role with its own EC2 instance role and pushes the temporary
//! credentials to the box's runner, which serves them at `169.254.169.254`
//! ([`ow_core::metadata`]). STS runs with the catalog lock released. Every change that ends
//! or re-owns an attachment (detach, re-attach, transfer, allow-list removal) withdraws the
//! runner's copy before it commits; the runner never asks for credentials, it only receives.
use crate::aws_identity::{Config, Mode, Work, config, notify, view};
use crate::catalog::{Catalog, Refusal};
use anyhow::{Result, ensure};
use ow_core::{credential_broker::WorkloadIdentity, sts};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock},
};

/// Re-assume once less than this remains of a one-hour session; SDKs refresh instance
/// credentials 10-15 minutes before they expire, so they never see old ones.
const REFRESH_BEFORE: u64 = 1800;

/// The controller's AWS account once known: attachments outside it are refused at once.
static ACCOUNT: OnceLock<String> = OnceLock::new();

pub(crate) fn validate(config: &Config) -> Result<()> {
    ensure!(
        config.issuer.is_none(),
        "metadata mode has no issuer; remove it"
    );
    let mut accounts = HashSet::new();
    for role in &config.allowed_roles {
        ensure!(
            role.role_arn
                .split(':')
                .nth(5)
                .is_some_and(|r| r.starts_with("role/ow-box/")),
            "metadata mode allows only roles under the IAM path /ow-box/: {}",
            role.role_arn
        );
        // `owners: []` would mean every user of the controller.
        ensure!(
            !role.owners.is_empty(),
            "metadata mode needs the owners of every role: {}",
            role.role_arn
        );
        accounts.insert(sts::account(&role.role_arn));
    }
    ensure!(
        accounts.len() <= 1,
        "metadata mode roles must all be in the controller's own AWS account"
    );
    Ok(())
}

/// Who assumes box roles: the controller's instance role, or a stand-in in tests.
pub(crate) trait Assume {
    fn host(&mut self) -> Result<sts::Host>;
    fn assume(&mut self, request: &sts::Request) -> Result<sts::Session>;
}

/// The controller host's EC2 instance role, connected on first use and after a failure.
#[derive(Default)]
pub(crate) struct Instance(Option<sts::Sts>);

impl Instance {
    fn client(&mut self) -> Result<&sts::Sts> {
        if self.0.is_none() {
            let client = sts::Sts::from_instance()?;
            let _ = ACCOUNT.set(client.host().account.clone());
            // Role credentials pass through this process: no core dumps, no same-user ptrace.
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0);
            }
            self.0 = Some(client);
        }
        Ok(self.0.as_ref().expect("connected"))
    }
}

impl Assume for Instance {
    fn host(&mut self) -> Result<sts::Host> {
        Ok(self.client()?.host().clone())
    }

    fn assume(&mut self, request: &sts::Request) -> Result<sts::Session> {
        self.client()?.assume(request)
    }
}

/// A role session to request for one box generation, collected under the catalog lock.
pub(crate) struct Pending {
    physical: String,
    node: String,
    role: String,
    generation: i64,
    owner: i64,
    email: String,
    hops: i64,
    session: String,
}

/// One refresher step in metadata mode.
pub(crate) fn step(
    shared: &Arc<Mutex<Catalog>>,
    work: Work,
    sts: &mut dyn Assume,
    cleared: &mut HashSet<(String, String, i64, i64)>,
) {
    match work {
        Work::Attached(physical) => {
            let plan = shared.lock().unwrap().aws_plan(&physical);
            let result = match plan {
                Ok(Some(pending)) => {
                    // The catalog lock is released: an STS slowdown stalls nothing else.
                    let session = sts.assume(&sts::Request {
                        role_arn: &pending.role,
                        session: &pending.session,
                        box_id: &pending.physical,
                        owner: &pending.email,
                    });
                    let host = sts.host().ok();
                    shared.lock().unwrap().aws_finish(&pending, session, host)
                }
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                eprintln!("aws identity: {physical}: {error:#}");
            }
        }
        Work::Bare(key) => {
            if !cleared.contains(&key) && shared.lock().unwrap().aws_bare(&key) {
                cleared.insert(key);
            }
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Catalog {
    pub(crate) fn creds_call(&self, node: &str, physical: &str, fields: Value) -> Result<Value> {
        crate::nodes::operation_capable(&self.root, node, "aws-creds")?;
        let mut request = fields;
        request["op"] = json!("aws-creds");
        request["id"] = json!(physical);
        if let Some(placement) = self.managed_placement(physical)? {
            request["placement"] = json!(placement);
        }
        crate::wire::request(&crate::nodes::route(&self.root, node)?, request)
    }

    /// Withdraw the box's credentials on its runner now, before the catalog change that ends
    /// or re-owns its attachment commits. `next` (role, generation, hop limit) swaps them for
    /// a wait on a new attachment instead. In metadata mode this is refused while the runner
    /// may hold credentials (the box is not stopped) and cannot be reached.
    pub(crate) fn withdraw_aws(
        &self,
        physical: &str,
        next: Option<(&str, i64, i64)>,
    ) -> Result<()> {
        let row: Option<(String, Option<String>, bool)> = self
            .db
            .query_row(
                "SELECT node,json_extract(metadata,'$.state'),COALESCE((SELECT served FROM aws_roles WHERE physical=?1),0) FROM resources WHERE kind='machine' AND physical=?1",
                [physical],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((node, state, served)) = row.filter(|(node, _, _)| !node.is_empty()) else {
            return Ok(());
        };
        // A runner without the capability never held any.
        if crate::nodes::operation_capable(&self.root, &node, "aws-creds").is_err() {
            return Ok(());
        }
        let fields = match next {
            Some((role, generation, hops)) => {
                json!({"role_arn":role,"generation":generation,"hop_limit":hops})
            }
            None => json!({}),
        };
        // Not the current mode alone: the operator may have switched modes, or broken or
        // removed the file, while the runner still holds credentials it was given.
        let held = served
            || config(&self.root)
                .ok()
                .flatten()
                .is_some_and(|c| c.mode == Mode::Metadata);
        match self.creds_call(&node, physical, fields) {
            Ok(_) => {
                self.db.execute(
                    "UPDATE aws_roles SET served=0 WHERE physical=?1",
                    [physical],
                )?;
                Ok(())
            }
            Err(_) if !held || state.as_deref() == Some("stopped") => Ok(()),
            Err(_) => Err(Refusal {
                code: "aws_runner_unreachable",
                message: format!(
                    "Runner {node} cannot be reached to withdraw this box's AWS credentials; try again when it is back."
                ),
            }
            .into()),
        }
    }

    /// Attach in metadata mode: the runner drops the old role's credentials and waits for
    /// the new ones, then the attachment is recorded and the refresher assumes the role.
    pub(crate) fn attach_metadata(
        &mut self,
        name: &str,
        physical: &str,
        owner: i64,
        role: &str,
        request: &Value,
    ) -> Result<Value> {
        let hops = request["hop_limit"].as_u64().unwrap_or(1);
        ensure!(matches!(hops, 1 | 2), "hop_limit must be 1 or 2");
        if let Some(account) = ACCOUNT.get()
            && sts::account(role) != Some(account.as_str())
        {
            return Err(Refusal {
                code: "aws_role_not_allowed",
                message: format!("{role} is not in this server's AWS account."),
            }
            .into());
        }
        let generation = self.next_aws_generation(physical)?;
        let mark = self.fork_mark(physical)?;
        self.withdraw_aws(physical, Some((role, generation, hops as i64)))?;
        self.db.execute(
            "INSERT INTO aws_roles(physical,owner,role_arn,generation,fork_mark,hop_limit) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(physical) DO UPDATE SET owner=excluded.owner,role_arn=excluded.role_arn,generation=excluded.generation,node=NULL,epoch=NULL,fork_mark=excluded.fork_mark,expires_at=NULL,error=NULL,hop_limit=excluded.hop_limit,served=0,updated_at=CURRENT_TIMESTAMP",
            params![physical, owner, role, generation, mark, hops as i64],
        )?;
        notify();
        Ok(json!({"id":name,"aws_role":view(&self.db, physical)?}))
    }

    /// Everything about one attached box except the STS call: withdraw what must go, follow
    /// its incarnation, and say whether it needs a new role session.
    pub(crate) fn aws_plan(&mut self, physical: &str) -> Result<Option<Pending>> {
        #[allow(clippy::type_complexity)]
        let row: Option<(String, i64, i64, Option<String>, Option<i64>, Option<i64>, i64, i64, String, Option<String>, i64, Option<String>)> = self.db.query_row(
            "SELECT a.role_arn,a.owner,a.generation,a.node,a.epoch,a.expires_at,r.owner,r.placement_epoch,u.email,a.fork_mark,a.hop_limit,json_extract(r.metadata,'$.state') FROM aws_roles a JOIN resources r ON r.kind='machine' AND r.physical=a.physical JOIN users u ON u.id=r.owner WHERE a.physical=?1",
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
        // Parked or moving: the runner it leaves cleared them when it gave the box up.
        let Some(node) = node.filter(|n| !n.is_empty() && !moving) else {
            self.db.execute(
                "UPDATE aws_roles SET expires_at=NULL WHERE physical=?1",
                [physical],
            )?;
            return Ok(None);
        };
        let Some((
            role,
            owner,
            mut generation,
            seen_node,
            seen_epoch,
            expires,
            current_owner,
            epoch,
            email,
            seen_forks,
            hops,
            state,
        )) = row
        else {
            self.creds_call(&node, physical, json!({}))?;
            return Ok(None);
        };
        let allowed = config(&self.root)?
            .is_some_and(|c| c.mode == Mode::Metadata && c.allows(&role, &email));
        if owner != current_owner || !allowed {
            let error = if owner != current_owner {
                "the box changed owner; its new owner must attach a role"
            } else {
                "role removed from the operator allow-list"
            };
            let withdrawn = self.creds_call(&node, physical, json!({}));
            self.db.execute(
                "UPDATE aws_roles SET expires_at=NULL,error=?2,served=served AND ?3 WHERE physical=?1",
                params![physical, error, withdrawn.is_err()],
            )?;
            withdrawn?;
            return Ok(None);
        }
        // A move, a wake on another runner or a fork of the box is a new incarnation: a new
        // generation and role session, so CloudTrail tells them apart.
        let forks = self.fork_mark(physical)?;
        let moved = seen_node.is_some()
            && (seen_node.as_deref() != Some(node.as_str()) || seen_epoch != Some(epoch));
        let forked = seen_forks.is_some() && seen_forks.as_deref() != Some(forks.as_str());
        let incarnation = moved || forked || seen_node.is_none() || seen_forks.is_none();
        if incarnation {
            if moved || forked {
                generation += 1;
            }
            self.db.execute(
                "UPDATE aws_roles SET generation=?2,node=?3,epoch=?4,fork_mark=?5,expires_at=NULL WHERE physical=?1",
                params![physical, generation, node, epoch, forks],
            )?;
        }
        if !matches!(state.as_deref(), Some("running" | "hibernated")) {
            // Stopped: its runner keeps only a wait; it gets credentials when it runs.
            self.db.execute(
                "UPDATE aws_roles SET expires_at=NULL WHERE physical=?1",
                [physical],
            )?;
            return Ok(None);
        }
        if !incarnation
            && expires.is_some_and(|e| e as u64 > now() + REFRESH_BEFORE)
            // A restarted worker or a cold start emptied the runner's memory.
            && self.creds_call(&node, physical, json!({"check":true,"generation":generation}))?["present"] == true
        {
            return Ok(None);
        }
        Ok(Some(Pending {
            physical: physical.to_owned(),
            node,
            role,
            generation,
            owner,
            email,
            hops,
            session: WorkloadIdentity {
                box_id: physical.to_owned(),
                generation: generation as u64,
            }
            .source_identity(),
        }))
    }

    /// Deliver a role session, unless the attachment changed while STS was asked: then the
    /// credentials are dropped unseen.
    pub(crate) fn aws_finish(
        &mut self,
        pending: &Pending,
        session: Result<sts::Session>,
        host: Option<sts::Host>,
    ) -> Result<()> {
        let current: Option<(String, i64, i64, i64, i64, String)> = self.db.query_row(
            "SELECT a.role_arn,a.owner,a.generation,a.hop_limit,r.owner,r.node FROM aws_roles a JOIN resources r ON r.kind='machine' AND r.physical=a.physical WHERE a.physical=?1 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1)",
            [&pending.physical],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        ).optional()?;
        if current
            != Some((
                pending.role.clone(),
                pending.owner,
                pending.generation,
                pending.hops,
                pending.owner,
                pending.node.clone(),
            ))
            // Removed from the allow-list (or access turned off) while STS answered.
            || !config(&self.root)?.is_some_and(|c| {
                c.mode == Mode::Metadata && c.allows(&pending.role, &pending.email)
            })
        {
            return Ok(());
        }
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                // Fixed text from `ow_core::sts`; the runner keeps what it has until it expires.
                self.db.execute(
                    "UPDATE aws_roles SET error=?2 WHERE physical=?1 AND generation=?3",
                    params![pending.physical, format!("{error:#}"), pending.generation],
                )?;
                return Err(error);
            }
        };
        let mut fields = json!({"role_arn":pending.role,"generation":pending.generation,"hop_limit":pending.hops,
            "access_key_id":session.access_key_id,"secret_access_key":session.secret_access_key.as_str(),
            "session_token":session.session_token.as_str(),"expiration":session.expiration});
        if let Some(host) = host {
            fields["region"] = json!(host.region);
            if let Some(zone) = host.zone {
                fields["availability_zone"] = json!(zone);
            }
        }
        match self.creds_call(&pending.node, &pending.physical, fields) {
            Ok(answer) => {
                self.db.execute(
                    "UPDATE aws_roles SET expires_at=?2,error=NULL,served=served OR ?4 WHERE physical=?1 AND generation=?3",
                    params![
                        pending.physical,
                        (answer["stored"] == true).then_some(session.expiration as i64),
                        pending.generation,
                        answer["stored"] == true
                    ],
                )?;
                Ok(())
            }
            Err(error) => {
                // Worker errors are fixed text and never echo the request.
                let text: String = format!("{error:#}").chars().take(300).collect();
                self.db.execute(
                    "UPDATE aws_roles SET expires_at=NULL,error=?2 WHERE physical=?1",
                    params![pending.physical, text],
                )?;
                Err(error)
            }
        }
    }

    /// A running box, once per incarnation: role-less ones lose any credentials on the runner,
    /// and every one loses identity files an earlier web-identity mode wrote into it, which
    /// most SDKs would otherwise try before instance metadata.
    fn aws_bare(&self, (physical, node, _, _): &(String, String, i64, i64)) -> bool {
        let attached = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM aws_roles WHERE physical=?1)",
                [physical],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(true);
        (attached || self.creds_call(node, physical, json!({})).is_ok())
            && self
                .aws_call(node, physical, json!({}))
                .is_ok_and(|v| v["written"] == true)
    }
}

/// The account of the controller once known (tests set it for a stand-in).
#[cfg(test)]
pub(crate) fn assume_account(account: &str) {
    let _ = ACCOUNT.set(account.to_owned());
}
