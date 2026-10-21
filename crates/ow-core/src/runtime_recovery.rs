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
        if request["op"] == "recovery-seal" {
            return self.recovery_seal(id, &recovery, request);
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
                && !self.unsealed.contains_key(&p.snapshot)
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
        // An unsealed capture of a point no longer pending is never committed.
        let stale: Vec<String> = self
            .unsealed
            .iter()
            .filter(|(name, staged)| staged.snapshot.workspace == id && **name != point.snapshot)
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            self.unsealed.remove(&name).unwrap().discard();
        }
        // A new uploader seals the capture itself while sending it (one read).
        let client_seal = request["client_seal"] == true;
        let mut unsealed = None;
        // Chunks still in the last durable point's extents are taken from its seal unread.
        let previous = recovery
            .durable
            .as_ref()
            .map(|p| self.snapshot_dir(&p.snapshot))
            .filter(|p| p.join(crate::snapshot_digest::SIDECAR).is_file());
        let mut stats = Value::Null;
        let result = (|| -> Result<()> {
            let destination = self.snapshot_dir(&point.snapshot);
            if destination.join("manifest.json").is_file() {
                self.verify_snapshot(&point.snapshot, false)?;
            } else if let Some(staged) = self.unsealed.get(&point.snapshot) {
                // A retry resumes the same unsealed capture.
                ensure!(client_seal, "capture is waiting for its uploader's seal");
                unsealed = Some(staged.staging.clone());
            } else if let Some(source) = &point.source
                && client_seal
            {
                let source_dir = self.verify_snapshot(source, false)?;
                let mut snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(source_dir.join("manifest.json"))?)?;
                let staging = self.snapshot_dir(&format!(".pending-{}", vm::nonce()?));
                fs::create_dir(&staging)?;
                let staged = (|| -> Result<Staged> {
                    // Pin the sealed inodes: wake may remove the sleep directory during upload.
                    for file in crate::snapshot_store::DATA_FILES {
                        fs::hard_link(source_dir.join(file), staging.join(file))?;
                    }
                    snapshot.name = point.snapshot.clone();
                    snapshot.managed_recovery = true;
                    snapshot.digest = None;
                    snapshot.files = BTreeMap::new();
                    Ok(Staged {
                        snapshot,
                        staging: staging.clone(),
                        previous: previous.clone(),
                        started: std::time::Instant::now(),
                        paused_ms: 0,
                        index: workspace.index,
                        fork: None,
                    })
                })();
                let staged = match staged {
                    Ok(staged) => staged,
                    Err(error) => {
                        let _ = fs::remove_dir_all(&staging);
                        return Err(error);
                    }
                };
                unsealed = Some(staging);
                self.unsealed.insert(point.snapshot.clone(), staged);
            } else if point.source.is_none() && client_seal {
                match self.capture_stage(id, &point.snapshot, false, true, previous.as_deref())? {
                    Ok(_) => {}
                    Err(staged) => {
                        unsealed = Some(staged.staging.clone());
                        self.unsealed.insert(point.snapshot.clone(), staged);
                    }
                }
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
                    let (files, counted) = crate::snapshot_digest::seal_directory(
                        &staging,
                        &crate::snapshot_store::DATA_FILES,
                        previous.as_deref(),
                        crate::snapshot_digest::CHUNKS_V1,
                    )?;
                    snapshot.files = files;
                    snapshot.digest = Some(crate::snapshot_digest::CHUNKS_V1.into());
                    stats = json!(counted);
                    atomic_json(&staging.join("manifest.json"), &snapshot)?;
                    fs::set_permissions(
                        staging.join("manifest.json"),
                        fs::Permissions::from_mode(0o400),
                    )?;
                    fs::rename(&staging, &destination)?;
                    File::open(self.root.join("snapshots"))?.sync_all()?;
                    self.verified
                        .insert(point.snapshot.clone(), fingerprints(&destination)?);
                    Ok(())
                })();
                if copied.is_err() {
                    let _ = fs::remove_dir_all(staging);
                }
                copied?;
            } else {
                stats = self.capture_inner(
                    id,
                    &point.snapshot,
                    false,
                    true,
                    previous.as_deref(),
                )?["chunks"]
                    .take();
            }
            Ok(())
        })();
        if let Err(error) = result {
            recovery.failures = recovery.failures.saturating_add(1);
            recovery.error = Some("Capture failed; retry scheduled.".into());
            atomic_json(&path, &recovery)?;
            return Err(error);
        }
        let mut result = json!({"snapshot":point.snapshot,"captured_at":point.captured_at});
        if !stats.is_null() {
            result["chunks"] = stats;
        }
        if let Some(staging) = unsealed {
            result["unsealed"] = json!(staging.file_name().unwrap().to_string_lossy());
            result["digest"] = json!(crate::snapshot_digest::CHUNKS_V2);
            result["previous"] = json!(
                previous
                    .as_ref()
                    .and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            );
        }
        Ok(result)
    }

    /// Commits a pending point's unsealed capture under the seals its uploader
    /// computed while sending it. The uploader runs as this worker's owner and
    /// reads the same files; each seal must describe the staged file as it is now
    /// (inode, size, time, a consistent chunk list), and a later verification
    /// rereads every byte against it.
    fn recovery_seal(&mut self, id: &str, recovery: &Recovery, request: &Value) -> Result<Value> {
        let name = request["snapshot"]
            .as_str()
            .context("missing recovery snapshot")?;
        ensure!(
            recovery
                .pending
                .as_ref()
                .is_some_and(|p| p.snapshot == name),
            "stale recovery seal"
        );
        let staged = self
            .unsealed
            .remove(name)
            .context("capture is no longer waiting for a seal; retry")?;
        let result = (|| -> Result<_> {
            ensure!(
                staged.snapshot.workspace == id,
                "capture of another machine"
            );
            let sealed = crate::snapshot_digest::read_seals(
                &staged.staging.join(crate::snapshot_digest::UPLOAD_SEALS),
                crate::snapshot_digest::CHUNK_V2,
            )?;
            ensure!(
                sealed.len() == crate::snapshot_store::DATA_FILES.len()
                    && crate::snapshot_store::DATA_FILES
                        .iter()
                        .all(|f| sealed.contains_key(*f)),
                "seals must cover every file"
            );
            for (file, seal) in &sealed {
                ensure!(
                    seal.describes(&staged.staging.join(file), crate::snapshot_digest::CHUNK_V2)?,
                    "seal does not describe the captured {file}"
                );
            }
            let stats = serde_json::from_value(request["capture"].clone()).ok();
            fs::rename(
                staged.staging.join(crate::snapshot_digest::UPLOAD_SEALS),
                staged.staging.join(crate::snapshot_digest::SIDECAR),
            )?;
            Ok((
                crate::snapshot_digest::roots(&sealed),
                Some(crate::snapshot_digest::CHUNKS_V2.into()),
                stats,
            ))
        })();
        self.capture_commit(staged, result)
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
                digest: None,
                fork: None,
                disk_only: false,
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

#[cfg(test)]
mod client_seal_tests {
    use super::*;

    #[test]
    fn uploader_seals_are_validated_pinned_across_wake_and_committed_last() {
        for valid in [false, true] {
            let mut runtime = super::super::disk_tests::fixture();
            fs::create_dir(runtime.root.join("recovery")).unwrap();
            let sleep = runtime.snapshot_dir("sleep-fixture");
            fs::create_dir(&sleep).unwrap();
            let mut files = BTreeMap::new();
            for file in crate::snapshot_store::DATA_FILES {
                fs::write(sleep.join(file), file).unwrap();
                files.insert(file.to_owned(), hash(&sleep.join(file)).unwrap());
            }
            let snapshot = json!({"name":"sleep-fixture","workspace":"legacy","memory_mib":1024,
                "vcpu_count":2,"image":"alpine","runtime":crate::common::RUNTIME,
                "cpu":"fixture","kernel_sha256":"fixture","files":files,"captured_at":now()-900});
            atomic_json(&sleep.join("manifest.json"), &snapshot).unwrap();
            let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
            workspace.state = "hibernated".into();
            workspace.hibernation = Some("sleep-fixture".into());
            let store = "a".repeat(64);
            let prepare =
                json!({"op":"recovery-prepare","id":"legacy","store":store,"client_seal":true});
            assert!(runtime.request(&prepare).unwrap().is_null());
            let journal = runtime.root.join("recovery/legacy.json");
            let mut saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            saved.sleep_observed_at = now() - 601;
            atomic_json(&journal, &saved).unwrap();
            let point = runtime.request(&prepare).unwrap();
            let name = point["snapshot"].as_str().unwrap();
            let staging = runtime
                .root
                .join("snapshots")
                .join(point["unsealed"].as_str().unwrap());
            assert!(!runtime.snapshot_dir(name).exists());
            let ack = json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":name,"uploaded":true});
            assert!(
                runtime.request(&ack).is_err(),
                "unsealed point acknowledged as durable"
            );
            // Wake removes the sleep directory, but the pending capture owns its inodes.
            fs::remove_dir_all(&sleep).unwrap();
            let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
            workspace.state = "stopped".into();
            workspace.hibernation = None;
            let mut saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            saved.attempted_at = now() - 301;
            atomic_json(&journal, &saved).unwrap();
            let retry = runtime.request(&prepare).unwrap();
            assert_eq!(retry["unsealed"], point["unsealed"]);
            let (mut seals, stats) = crate::snapshot_digest::seal_files(
                &staging,
                &crate::snapshot_store::DATA_FILES,
                None,
                crate::snapshot_digest::CHUNKS_V2,
                2,
                None,
            )
            .unwrap();
            if !valid {
                seals.remove("memory");
            }
            atomic_json(&staging.join(crate::snapshot_digest::UPLOAD_SEALS), &seals).unwrap();
            let seal = json!({"op":"recovery-seal","id":"legacy","store":store,
                "snapshot":name,"capture":stats});
            let mut stale = seal.clone();
            stale["store"] = json!("b".repeat(64));
            assert!(runtime.request(&stale).is_err());
            assert_eq!(runtime.request(&seal).is_ok(), valid);
            assert_eq!(
                runtime.snapshot_dir(name).join("manifest.json").exists(),
                valid
            );
            assert!(!staging.exists());
            if valid {
                let committed: Snapshot = serde_json::from_slice(
                    &fs::read(runtime.snapshot_dir(name).join("manifest.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    committed.digest.as_deref(),
                    Some(crate::snapshot_digest::CHUNKS_V2)
                );
                runtime.request(&ack).unwrap();
                runtime.request(&ack).unwrap();
                runtime.verified.clear();
                runtime.verify_snapshot(name, false).unwrap();
                fs::set_permissions(
                    runtime.snapshot_dir(name).join("memory"),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
                fs::write(runtime.snapshot_dir(name).join("memory"), "tampered").unwrap();
                assert!(runtime.verify_snapshot(name, false).is_err());
            }
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }
}
