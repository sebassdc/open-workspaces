//! Local consent and participation. Reuses the verified outbound node transport.
use crate::{HostAction, common, mac_worker, nodes, wire};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct OwnedChild(std::process::Child);
impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn config(root: &Path) -> Result<Value> {
    nodes::private(&root.join("host.json"))?;
    let c: Value = serde_json::from_slice(&fs::read(root.join("host.json"))?)?;
    ensure!(
        c["version"] == 1 && c["policy"] == nodes::POOL_POLICY,
        "invalid saved consent"
    );
    nodes::origin(c["controller"].as_str().context("controller")?, false)?;
    Ok(c)
}
pub fn verified_images(root: &Path) -> Result<Vec<&'static str>> {
    mac_worker::verified_assets(root)?;
    Ok(vec!["ubuntu-arm64"])
}
fn active(root: &Path) -> Result<bool> {
    let f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("host-running.lock"))?;
    Ok(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0)
}
fn lifecycle(root: &Path) -> Result<fs::File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("host-lifecycle.lock"))?;
    ensure!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "host lifecycle busy"
    );
    Ok(f)
}
fn start(root: &Path) -> Result<()> {
    config(root)?;
    verified_images(root)?;
    nodes::private(&root.join("node.json"))?;
    if active(root)? {
        println!(
            "Mac participation already running; check host status for controller acknowledgement."
        );
        return Ok(());
    }
    let marker = root.join("host-stop");
    if marker.exists() {
        nodes::private(&marker)?;
        fs::remove_file(marker)?;
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("host.log"))?;
    Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(root)
        .args(["host", "run"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if active(root)? && wire::request(root, json!({"op":"status"})).is_ok() {
            println!(
                "Mac worker started; enrollment channel acknowledgement remains visible in host status."
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("participation start not observed; inspect private host.log")
}
fn supervise(root: &Path) -> Result<()> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("host-running.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "participation already active"
    );
    let mut writer = &lock;
    writer.set_len(0)?;
    use std::io::Write;
    writer.write_all(
        json!({"pid":std::process::id(),"start":mac_worker::process_start(std::process::id())?})
            .to_string()
            .as_bytes(),
    )?;
    writer.sync_all()?;
    let c = config(root)?;
    let mut worker = OwnedChild(
        Command::new(std::env::current_exe()?)
            .arg("--data-dir")
            .arg(root)
            .arg("worker")
            .stdin(Stdio::piped())
            .spawn()?,
    );
    let ready = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if wire::request(root, json!({"op":"status"})).is_ok() {
                return Ok(());
            }
            if worker.try_wait()?.is_some() {
                bail!("native worker exited")
            };
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("worker start timeout")
    })();
    if let Err(e) = ready {
        let _ = worker.kill();
        let _ = worker.wait();
        return Err(e);
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("--data-dir")
        .arg(root)
        .arg("node-agent")
        .arg("--controller")
        .arg(c["controller"].as_str().unwrap())
        .arg("--credential")
        .arg(root.join("node.json"))
        .stdin(Stdio::null());
    if root.join("controller-ca.pem").exists() {
        cmd.arg("--ca-cert").arg(root.join("controller-ca.pem"));
    }
    let mut agent = match cmd.spawn() {
        Ok(c) => OwnedChild(c),
        Err(e) => {
            let _ = wire::request(root, json!({"op":"shutdown"}));
            let _ = worker.wait();
            return Err(e.into());
        }
    };
    let outcome = loop {
        if root.join("host-stop").exists() {
            break Ok(());
        }
        if worker.try_wait()?.is_some() {
            break Err(anyhow::anyhow!(
                "worker exited; native helpers lose parent pipes"
            ));
        }
        if agent.try_wait()?.is_some() {
            break Err(anyhow::anyhow!("agent exited; participation stopped"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = wire::request(root, json!({"op":"shutdown"}));
    // Only signal owned, unreaped children. try_wait reaps exited children, so
    // check again and never signal a completed/reused PID.
    if agent.try_wait()?.is_none() {
        let _ = agent.kill();
    }
    let _ = agent.wait();
    if worker.try_wait()?.is_none() {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && worker.try_wait()?.is_none() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if worker.try_wait()?.is_none() {
            let _ = worker.kill();
        }
    }
    let _ = worker.wait();
    outcome
}
pub fn run(explicit: Option<PathBuf>, action: HostAction) -> Result<i32> {
    let root = mac_worker::root(explicit)?;
    if matches!(action, HostAction::Run) {
        supervise(&root)?;
        return Ok(0);
    }
    let _lock = lifecycle(&root)?;
    match action {
        HostAction::Doctor => {
            let output = Command::new(mac_worker::helper()?)
                .arg("capabilities")
                .output()?;
            ensure!(output.status.success(), "signed native helper unavailable");
            println!(
                "{}",
                json!({"supported":std::env::consts::ARCH=="aarch64" && serde_json::from_slice::<Value>(&output.stdout)?["hardware_isolation"]==true,"backend":"apple-virtualization","helper":serde_json::from_slice::<Value>(&output.stdout).unwrap_or(json!({})),"limits":{"memory_mib":2048,"slots":1,"vcpus":2,"retained_storage_gib":20},"networking":false})
            );
        }
        HostAction::Join {
            controller,
            invite_file,
            ca_cert,
            memory,
            slots,
            cpus,
            storage_gib,
            accept_shared_pool,
            no_start,
            ..
        } => {
            if root.join("node.json").exists() {
                config(&root)?;
                if !no_start {
                    start(&root)?;
                }
                return Ok(0);
            }
            ensure!(
                accept_shared_pool,
                "explicit --accept-shared-pool local consent required"
            );
            ensure!(
                !root.join("host.json").exists(),
                "enrollment intent already exists without credentials; do not redeem another invitation blindly"
            );
            let p = invite_file
                .context("Mac join requires --invite-file with a dedicated private invitation")?;
            nodes::private(&p)?;
            ensure!(p.metadata()?.len() <= 4096, "invitation too large");
            let v: Value = serde_json::from_slice(&fs::read(p)?)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            common::identifier(v["node"].as_str().context("node")?)?;
            ensure!(
                v["node"] != "local"
                    && v["policy"] == nodes::POOL_POLICY
                    && v["expires"]
                        .as_u64()
                        .is_some_and(|e| e > now && e <= now + 3600)
                    && v["secret"]
                        .as_str()
                        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
                "invalid invitation policy/expiry/secret"
            );
            let origin = nodes::origin(
                v["controller"]
                    .as_str()
                    .context("invitation controller required")?,
                false,
            )?;
            if let Some(c) = controller {
                ensure!(
                    nodes::origin(&c, false)? == origin,
                    "controller override differs from invitation"
                );
            }
            let (ram, cpu) = (memory.unwrap_or(512), cpus.unwrap_or(1));
            ensure!(
                [512, 1024, 2048].contains(&ram)
                    && (1..=2).contains(&cpu)
                    && slots.unwrap_or(1) == 1
                    && (20..=1024).contains(&storage_gib.unwrap_or(20))
                    && ram as u64 <= v["memory"].as_u64().unwrap_or(0)
                    && cpu as u64 <= v["cpus"].as_u64().unwrap_or(0)
                    && v["slots"].as_u64().unwrap_or(0) >= 1,
                "local/owner Mac budget violation"
            );
            // New dedicated root only; don't adopt another worker or guest disk.
            for e in fs::read_dir(&root)? {
                ensure!(
                    e?.file_name() == "host-lifecycle.lock" || false,
                    "join needs a fresh dedicated private root"
                );
            }
            let assets = PathBuf::from(
                std::env::var_os("OW_MAC_ASSETS")
                    .context("set OW_MAC_ASSETS to verified prepared ARM64 assets")?,
            )
            .canonicalize()?;
            let cfg = json!({"version":1,"policy":nodes::POOL_POLICY,"controller":origin,"node":v["node"],"memory":ram,"cpus":cpu,"slots":1,"storage_gib":storage_gib.unwrap_or(20),"assets":assets});
            // Persist intent before the one-use network effect. Lost replies are
            // explicit recovery, never a second silent redemption.
            nodes::write_private(&root.join("host.json"), &cfg)?;
            verified_images(&root)?;
            if let Some(ca) = ca_cert {
                nodes::private(&ca)?;
                let bytes = fs::read(ca)?;
                ensure!(bytes.len() <= 65536, "CA too large");
                let mut f = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(root.join("controller-ca.pem"))?;
                use std::io::Write;
                f.write_all(&bytes)?;
                f.sync_all()?;
            }
            let ca = root.join("controller-ca.pem");
            let creds = nodes::redeem(&origin, &v, ca.exists().then_some(ca.as_path()))?;
            nodes::write_private(&root.join("node.json"), &creds)?;
            println!("Mac enrollment saved with explicit shared-pool consent.");
            if !no_start {
                start(&root)?;
            }
        }
        HostAction::Start => start(&root)?,
        HostAction::Stop => {
            if active(&root)? {
                if root.join("host-stop").exists() {
                    nodes::private(&root.join("host-stop"))?;
                } else {
                    nodes::write_private(&root.join("host-stop"), &json!({"stop":true}))?;
                }
                let deadline = Instant::now() + Duration::from_secs(40);
                while active(&root)? && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
                ensure!(
                    !active(&root)?,
                    "participation stop unresolved; disks/demand retained"
                );
            } else if wire::request(&root, json!({"op":"status"})).is_ok() {
                wire::request(&root, json!({"op":"shutdown"}))?;
            }
            println!("Participation stopped; native disks and credentials retained.");
        }
        HostAction::Status => {
            let worker = wire::request(&root, json!({"op":"status"})).ok();
            let channel = fs::read(root.join("node-channel.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let running = active(&root)?;
            let agent = fs::read(root.join("node-agent.lock"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let agent_matches = agent.as_ref().is_some_and(|a| {
                a["pid"]
                    .as_u64()
                    .and_then(|p| mac_worker::process_start(p as u32).ok())
                    .is_some_and(|s| a["start"] == s)
            });
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let dispatchable = running
                && worker.is_some()
                && agent_matches
                && channel.as_ref().is_some_and(|c| {
                    c["state"] == "dispatchable"
                        && c["generation"].is_string()
                        && c["instance"] == agent.as_ref().unwrap()["instance"]
                        && c["updated"]
                            .as_u64()
                            .is_some_and(|t| t <= now && now - t < 15)
                });
            println!(
                "{}",
                json!({"host_running":running,"controller_dispatchable":dispatchable,"worker":worker,"node_channel":channel,"controller_status":"Inspect channel generation/ack and owner inventory; local PID alone is not pool acceptance."})
            );
        }
        HostAction::Configure {
            memory,
            slots,
            cpus,
            storage_gib,
            accept_shared_pool,
        } => {
            ensure!(
                accept_shared_pool
                    && !active(&root)?
                    && wire::request(&root, json!({"op":"status"})).is_err(),
                "configure requires explicit consent and fully stopped participation"
            );
            ensure!(
                [512, 1024, 2048].contains(&memory)
                    && slots == 1
                    && (1..=2).contains(&cpus)
                    && (20..=1024).contains(&storage_gib.unwrap_or(20)),
                "Mac budget violation"
            );
            mac_worker::stopped_data(&root)?;
            let mut cfg = config(&root)?;
            cfg["memory"] = json!(memory);
            cfg["cpus"] = json!(cpus);
            cfg["slots"] = json!(slots);
            if let Some(s) = storage_gib {
                cfg["storage_gib"] = json!(s);
            }
            let temp = root.join(format!("host-{}.json", common::nonce()?));
            nodes::write_private(&temp, &cfg)?;
            fs::rename(temp, root.join("host.json"))?;
            fs::File::open(&root)?.sync_all()?;
        }
        _ => bail!(
            "unsupported Mac host action; assets are prepared locally, not Linux installer bundles"
        ),
    };
    Ok(0)
}
