use super::*;
use crate::recovery::{Point, Recovery, now};

impl Runtime {
    pub(super) fn recovery_operation(&mut self, request: &Value) -> Result<Value> {
        let id = request["id"].as_str().context("missing ID")?;
        let workspace = self.workspace(id)?;
        let path = self.root.join("recovery").join(format!("{id}.json"));
        let mut recovery: Recovery = if path.exists() {
            serde_json::from_slice(&fs::read(&path)?)?
        } else {
            Recovery::default()
        };
        let at = now();
        let store = request["store"]
            .as_str()
            .context("missing recovery store identity")?;
        ensure!(
            store.len() == 64 && store.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid store identity"
        );
        if recovery.store != store {
            ensure!(
                request["op"] == "recovery-prepare",
                "stale recovery store result"
            );
            recovery.store = store.into();
            recovery.durable = None;
            recovery.uploaded_at = None;
            recovery.attempted_at = 0;
        }
        if request["op"] == "recovery-result" {
            let name = request["snapshot"]
                .as_str()
                .context("missing recovery snapshot")?;
            if recovery.pending.is_none()
                && recovery
                    .durable
                    .as_ref()
                    .is_some_and(|p| p.snapshot == name)
            {
                return Ok(recovery.summary(at));
            }
            ensure!(
                recovery
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.snapshot == name),
                "stale recovery result"
            );
            if request["uploaded"] == true {
                self.verify_snapshot(name, false)?;
                recovery.durable = recovery.pending.take();
                recovery.uploaded_at = Some(at);
                recovery.failures = 0;
                recovery.error = None;
                recovery.attempted_at = 0;
            } else {
                recovery.failures = recovery.failures.saturating_add(1);
                recovery.error = Some("Upload failed; retry scheduled.".into());
            }
            atomic_json(&path, &recovery)?;
            self.prune_recovery(id, &recovery)?;
            return Ok(recovery.summary(at));
        }
        if workspace.state == "hibernated" {
            if recovery.sleep != workspace.hibernation {
                recovery.sleep = workspace.hibernation.clone();
                recovery.sleep_observed_at = at;
            }
        } else {
            recovery.sleep = None;
            recovery.sleep_observed_at = 0;
        }
        let sleep = workspace
            .hibernation
            .as_deref()
            .map(|name| -> Result<_> {
                let snapshot: Snapshot = serde_json::from_slice(&fs::read(
                    self.snapshot_dir(name).join("manifest.json"),
                )?)?;
                Ok((
                    name.to_owned(),
                    recovery.sleep_observed_at,
                    snapshot.captured_at,
                ))
            })
            .transpose()?;
        if recovery.pending.as_ref().is_some_and(|p| {
            !self
                .snapshot_dir(&p.snapshot)
                .join("manifest.json")
                .exists()
                && p.source
                    .as_ref()
                    .map_or(workspace.state != "running", |source| {
                        !self.snapshot_dir(source).join("manifest.json").exists()
                    })
        }) {
            // An unsealed intent has lost its source; never recycle a possibly uploaded name.
            recovery.pending = None;
            recovery.attempted_at = 0;
            recovery.error = Some(
                "Pending capture unavailable; waiting for a new running or hibernated point."
                    .into(),
            );
        }
        if !recovery.due(
            &workspace.state,
            sleep.as_ref().map(|(n, t, _)| (n.as_str(), *t)),
            at,
        ) {
            atomic_json(&path, &recovery)?;
            return Ok(Value::Null);
        }
        if recovery.pending.is_none() {
            recovery.pending = Some(Point {
                snapshot: format!("{RECOVERY}{}", &vm::nonce()?[..17]),
                captured_at: sleep
                    .as_ref()
                    .filter(|_| workspace.state == "hibernated")
                    .map_or(at, |(_, _, captured)| *captured),
                source: sleep
                    .as_ref()
                    .filter(|_| workspace.state == "hibernated")
                    .map(|(n, _, _)| n.clone()),
            });
        }
        recovery.attempted_at = at;
        atomic_json(&path, &recovery)?;
        let point = recovery.pending.as_ref().unwrap();
        let result = (|| -> Result<()> {
            let destination = self.snapshot_dir(&point.snapshot);
            if destination.join("manifest.json").is_file() {
                self.verify_snapshot(&point.snapshot, false)?;
            } else if let Some(source) = &point.source {
                let source = self.verify_snapshot(source, false)?;
                let mut snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
                let staging = self.snapshot_dir(&format!(".pending-{}", vm::nonce()?));
                fs::create_dir(&staging)?;
                let copied = (|| -> Result<()> {
                    // Pin the sealed inodes: wake may remove the sleep directory during upload.
                    for file in crate::snapshot_store::DATA_FILES {
                        fs::hard_link(source.join(file), staging.join(file))?;
                    }
                    snapshot.name = point.snapshot.clone();
                    snapshot.managed_recovery = true;
                    atomic_json(&staging.join("manifest.json"), &snapshot)?;
                    fs::set_permissions(
                        staging.join("manifest.json"),
                        fs::Permissions::from_mode(0o400),
                    )?;
                    fs::rename(&staging, destination)?;
                    File::open(self.root.join("snapshots"))?.sync_all()?;
                    Ok(())
                })();
                if copied.is_err() {
                    let _ = fs::remove_dir_all(staging);
                }
                copied?;
            } else {
                self.capture_inner(id, &point.snapshot, false, true)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            recovery.failures = recovery.failures.saturating_add(1);
            recovery.error = Some("Capture failed; retry scheduled.".into());
            atomic_json(&path, &recovery)?;
            return Err(error);
        }
        Ok(json!({"snapshot":point.snapshot,"captured_at":point.captured_at}))
    }

    fn prune_recovery(&self, id: &str, recovery: &Recovery) -> Result<()> {
        for entry in fs::read_dir(self.root.join("snapshots"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(RECOVERY)
                || recovery
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.snapshot == name)
                || recovery
                    .durable
                    .as_ref()
                    .is_some_and(|p| p.snapshot == name)
                || self
                    .state
                    .workspaces
                    .values()
                    .any(|w| w.hibernation.as_deref() == Some(&name))
            {
                continue;
            }
            let manifest = entry.path().join("manifest.json");
            if manifest.is_file() {
                let snapshot: Snapshot = serde_json::from_slice(&fs::read(manifest)?)?;
                if snapshot.managed_recovery && snapshot.workspace == id {
                    fs::remove_dir_all(entry.path())?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_running_intent_yields_to_sleep_but_sealed_uploads_survive() {
        for (sealed, sleep_age, recent_failure, state) in [
            (false, 0, false, "hibernated"),
            (false, 601, true, "hibernated"),
            (true, 601, false, "hibernated"),
            (false, 601, false, "stopped"),
            (false, 601, false, "running"),
        ] {
            let mut runtime = super::super::disk_tests::fixture();
            let at = now();
            let sleep_name = "sleep-fixture";
            let old_name = "sleep-recovery-obsolete";
            let sleep = runtime.snapshot_dir(sleep_name);
            fs::create_dir(&sleep).unwrap();
            fs::create_dir(runtime.root.join("recovery")).unwrap();
            let mut files = BTreeMap::new();
            for file in crate::snapshot_store::DATA_FILES {
                fs::write(sleep.join(file), file).unwrap();
                files.insert(file.to_owned(), hash(&sleep.join(file)).unwrap());
            }
            let snapshot = Snapshot {
                captured_at: at - 900,
                managed_recovery: false,
                name: sleep_name.into(),
                workspace: "legacy".into(),
                managed: false,
                memory_mib: 1024,
                vcpu_count: 2,
                image: "alpine".into(),
                image_digest: None,
                runtime: crate::common::RUNTIME.into(),
                cpu: "fixture".into(),
                kernel_sha256: "fixture".into(),
                files,
            };
            atomic_json(&sleep.join("manifest.json"), &snapshot).unwrap();
            if sealed {
                let old = runtime.snapshot_dir(old_name);
                fs::create_dir(&old).unwrap();
                for file in crate::snapshot_store::DATA_FILES {
                    fs::hard_link(sleep.join(file), old.join(file)).unwrap();
                }
                let mut manifest = serde_json::to_value(&snapshot).unwrap();
                manifest["name"] = json!(old_name);
                manifest["managed_recovery"] = json!(true);
                atomic_json(&old.join("manifest.json"), &manifest).unwrap();
            }
            let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
            workspace.state = state.into();
            workspace.hibernation = (state == "hibernated").then(|| sleep_name.into());
            let store = "a".repeat(64);
            let journal = runtime.root.join("recovery/legacy.json");
            let saved = Recovery {
                store: store.clone(),
                sleep: Some(sleep_name.into()),
                sleep_observed_at: at - sleep_age,
                pending: Some(Point {
                    snapshot: old_name.into(),
                    captured_at: snapshot.captured_at,
                    source: None,
                }),
                attempted_at: if recent_failure { at } else { at - 301 },
                failures: 4,
                error: Some("Capture failed; retry scheduled.".into()),
                ..Recovery::default()
            };
            atomic_json(&journal, &saved).unwrap();
            let prepare = json!({"op":"recovery-prepare","id":"legacy","store":store});
            if state == "running" {
                assert!(runtime.request(&prepare).is_err());
                let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
                assert_eq!(saved.pending.unwrap().snapshot, old_name);
                fs::remove_dir_all(runtime.root).unwrap();
                continue;
            }
            let mut point = runtime.request(&prepare).unwrap();
            if state == "stopped" {
                assert!(point.is_null());
                let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
                assert!(saved.pending.is_none() && saved.durable.is_none());
                fs::remove_dir_all(runtime.root).unwrap();
                continue;
            }
            if sleep_age == 0 {
                assert!(
                    point.is_null(),
                    "an obsolete running intent bypassed the sleep delay"
                );
                let mut saved: Recovery =
                    serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
                assert!(saved.pending.is_none());
                saved.sleep_observed_at = at - 601;
                atomic_json(&journal, &saved).unwrap();
                point = runtime.request(&prepare).unwrap();
            }
            let name = point["snapshot"]
                .as_str()
                .expect("due sleep was blocked by obsolete intent");
            assert_eq!(
                name == old_name,
                sealed,
                "sealed uploads must retain their name; unsealed intents must not"
            );
            assert_eq!(point["captured_at"], snapshot.captured_at);
            let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            assert_eq!(
                saved.pending.as_ref().unwrap().source.as_deref(),
                (!sealed).then_some(sleep_name)
            );
            assert!(saved.durable.is_none());
            if !sealed {
                assert!(runtime.request(&json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":old_name,"uploaded":true})).is_err());
            }
            runtime.request(&json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":name,"uploaded":true})).unwrap();
            let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            assert_eq!(saved.durable.unwrap().snapshot, name);
            assert_eq!(saved.failures, 0);
            assert!(saved.pending.is_none() && saved.error.is_none());
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }
}
