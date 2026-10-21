//! Dormant worker steps. The authenticated controller activates only after catalog commit.
use super::*;
use crate::common::Placement;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Active,
    Importing,
    Prepared,
    Exporting,
    Captured,
    Exported,
    Cleaned,
    Discarded,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Managed {
    pub authority: Placement,
    pub phase: Phase,
    pub intent: Option<Export>,
    pub import: Option<Import>,
    pub capture_ram: Option<bool>,
    #[serde(default)]
    pub fork_initialized: bool,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Export {
    transfer: String,
    pub(super) capture: String,
    disk_only: bool,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Import {
    snapshot: String,
    manifest_sha256: String,
    cold: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) fork_source: Option<String>,
}

impl Runtime {
    pub(super) fn workspace_count(&self) -> usize {
        self.state
            .workspaces
            .keys()
            .filter(|id| {
                self.state
                    .placements
                    .get(*id)
                    .is_none_or(|m| !matches!(m.phase, Phase::Cleaned | Phase::Discarded))
            })
            .count()
    }

    pub(super) fn lifecycle_capture(&self, name: &str) -> bool {
        self.state.placements.values().any(|m| {
            m.intent.as_ref().is_some_and(|i| i.capture == name)
                || m.import.as_ref().is_some_and(|i| i.snapshot == name)
        })
    }

    pub(super) fn lifecycle_capture_of_another(&self, id: &str, name: &str) -> bool {
        self.state.placements.iter().any(|(other, m)| {
            other != id
                && (m.intent.as_ref().is_some_and(|i| i.capture == name)
                    || m.import.as_ref().is_some_and(|i| i.snapshot == name))
        })
    }

    pub(super) fn recover_state(state: &mut State) -> Result<()> {
        for (id, workspace) in &mut state.workspaces {
            ensure!(
                workspace.managed == state.placements.contains_key(id),
                "managed registry missing placement authority: {id}"
            );
            if let Some(managed) = state.placements.get(id) {
                managed.authority.validate()?;
                workspace.state = match managed.phase {
                    Phase::Active => {
                        if workspace.hibernation.is_some() {
                            "hibernated"
                        } else {
                            "stopped"
                        }
                    }
                    Phase::Importing => "importing",
                    Phase::Prepared => "prepared",
                    Phase::Exporting | Phase::Captured => "exporting",
                    Phase::Exported => "exported",
                    Phase::Cleaned if managed.intent.is_none() => "deleted",
                    Phase::Cleaned => "exported-cleaned",
                    Phase::Discarded => "discarded",
                }
                .into();
            } else if workspace.state != "hibernated" {
                ensure!(
                    !matches!(
                        workspace.state.as_str(),
                        "importing"
                            | "prepared"
                            | "exporting"
                            | "exported"
                            | "exported-cleaned"
                            | "deleted"
                            | "discarded"
                    ),
                    "lifecycle state is missing managed placement: {id}"
                );
                workspace.state = "stopped".into();
            }
        }
        ensure!(
            state
                .placements
                .keys()
                .all(|id| state.workspaces.contains_key(id)),
            "orphan placement fence"
        );
        Ok(())
    }

    /// A managed delete leaves its fence cleaned with no export intent: a tombstone.
    pub(super) fn deleted(&self, id: &str) -> bool {
        self.state
            .placements
            .get(id)
            .is_some_and(|m| m.phase == Phase::Cleaned && m.intent.is_none())
    }
    /// An activated copy that is not running, or a cleaned source left by a move or park.
    /// An export not yet cleaned may still be a transfer's fallback, so it is cleaned first.
    pub(super) fn require_deletable(&self, id: &str) -> Result<()> {
        if let Some(m) = self.state.placements.get(id) {
            ensure!(
                match m.phase {
                    Phase::Active => m.intent.is_none(),
                    Phase::Cleaned => true,
                    _ => false,
                },
                "managed workspace is fenced or not activated"
            );
        }
        Ok(())
    }
    /// The capture a fork child imported names its parent, so the delete loop keeps it. It
    /// goes with the child when nothing else here can still own it: no other copy imports it,
    /// and the parent has no live copy on this runner (whose own delete would remove it).
    pub(super) fn orphaned_import(&self, id: &str) -> Option<String> {
        let import = self.state.placements.get(id)?.import.as_ref()?;
        let parent = import.fork_source.as_deref()?;
        let name = &import.snapshot;
        let manifest = fs::read(self.snapshot_dir(name).join("manifest.json")).ok()?;
        let snapshot: Snapshot = serde_json::from_slice(&manifest).ok()?;
        (snapshot.workspace == parent
            && !self.lifecycle_capture_of_another(id, name)
            && (!self.state.workspaces.contains_key(parent) || self.deleted(parent)))
        .then(|| name.clone())
    }
    pub(super) fn mark_deleted(&mut self, id: &str) {
        let managed = self.state.placements.get_mut(id).unwrap();
        managed.phase = Phase::Cleaned;
        // Its export and import captures are released: nothing may resume this copy.
        managed.intent = None;
        managed.import = None;
        self.state.workspaces.get_mut(id).unwrap().state = "deleted".into();
    }

    pub(super) fn require_unmanaged(&self, id: &str) -> Result<()> {
        ensure!(
            !self.workspace(id)?.managed && !self.state.placements.contains_key(id),
            "missing placement epoch for managed workspace"
        );
        Ok(())
    }
    pub(super) fn require_active(&self, id: &str) -> Result<()> {
        let workspace = self.workspace(id)?;
        if workspace.managed || self.state.placements.contains_key(id) {
            let managed = self
                .state
                .placements
                .get(id)
                .context("managed registry missing placement authority")?;
            ensure!(
                workspace.managed && managed.phase == Phase::Active && managed.intent.is_none(),
                "managed workspace is fenced or not activated"
            );
            managed.authority.validate()?;
        }
        Ok(())
    }
    /// Validate even a running VM: a connection cannot bypass a newly persisted export intent.
    pub fn authorize(&self, request: &Value, active: bool) -> Result<()> {
        let id = request["id"].as_str().context("missing ID")?;
        identifier(id)?;
        let managed = self.state.placements.get(id);
        if managed.is_some() || self.state.workspaces.get(id).is_some_and(|w| w.managed) {
            let actual = managed.context("managed registry missing placement authority")?;
            let requested = Placement::from_request(request)?;
            ensure!(
                requested == actual.authority,
                "stale or wrong placement epoch, node or transfer"
            );
            if active {
                self.require_active(id)?;
            }
        } else {
            ensure!(
                request.get("placement").is_none(),
                "unregistered placement; prepare before activation"
            );
        }
        Ok(())
    }

    pub(super) fn placement_summary(&self, id: &str) -> Result<Value> {
        let mut summary = self.workspace_summary(id)?;
        if let Some(managed) = self.state.placements.get(id) {
            summary["placement"] = json!(managed.authority);
            summary["lifecycle"] = json!(managed);
            summary["vmm_running"] = json!(self.machines.contains_key(id));
        }
        Ok(summary)
    }

    pub(super) fn check_replay(&self, request: &Value) -> Result<()> {
        let original = request.get("request").context("missing replay request")?;
        self.authorize(original, false)?;
        let id = original["id"].as_str().context("missing ID")?;
        let Some(managed) = self.state.placements.get(id) else {
            return Ok(());
        };
        match original["op"].as_str().unwrap_or("") {
            "export" | "seal-disk" | "export-cleanup" => {
                let intent = managed.intent.as_ref().context("missing export intent")?;
                ensure!(
                    original["transfer"] == intent.transfer
                        && original["capture"] == intent.capture
                        && (original["op"] != "seal-disk" || intent.disk_only),
                    "different export intent"
                );
            }
            "import-discard" => ensure!(
                managed.phase == Phase::Discarded,
                "target no longer excluded"
            ),
            "import-prepare" | "fork-prepare" => {
                ensure!(
                    managed.import.as_ref() == Some(&Self::import_intent(original)?),
                    "different import intent"
                );
                ensure!(
                    matches!(
                        managed.phase,
                        Phase::Importing | Phase::Prepared | Phase::Active
                    ),
                    "import has since been exported"
                );
            }
            "activate" | "placement-register" => self.require_active(id)?,
            "delete" => ensure!(self.deleted(id), "managed delete was not completed"),
            // Removing a named snapshot never executes or changes the copy itself.
            "snapshot-delete" => {}
            _ => self.require_active(id)?,
        }
        Ok(())
    }

    pub(super) fn lifecycle_request(&mut self, request: &Value) -> Result<Value> {
        let id = request["id"].as_str().context("missing ID")?;
        identifier(id)?;
        let authority = Placement::from_request(request)?;
        match request["op"].as_str().context("missing op")? {
            "placement-register" => {
                ensure!(
                    request["committed"] == true,
                    "registration requires committed placement"
                );
                if self.state.placements.contains_key(id) {
                    self.authorize(request, true)?;
                    return self.placement_summary(id);
                }
                let workspace = self.workspace(id)?;
                ensure!(
                    !workspace.managed
                        && matches!(
                            workspace.state.as_str(),
                            "stopped" | "hibernated" | "running"
                        ),
                    "only an ordinary unmanaged copy may be registered"
                );
                self.state.workspaces.get_mut(id).unwrap().managed = true;
                self.state.placements.insert(
                    id.into(),
                    Managed {
                        authority,
                        phase: Phase::Active,
                        intent: None,
                        import: None,
                        capture_ram: None,
                        fork_initialized: false,
                    },
                );
                self.save()?;
            }
            "export" | "seal-disk" => return self.export_copy(request),
            "import-prepare" | "fork-prepare" => return self.prepare_import(request, authority),
            "import-discard" => return self.discard_import(request, authority),
            "policy-prepare" => {
                self.authorize(request, false)?;
                ensure!(
                    self.state.placements[id].phase == Phase::Prepared
                        && !self.machines.contains_key(id),
                    "policy install requires exact prepared copy"
                );
                crate::ssh_guest::install_public(&self.root, id, &request["policy"])?;
            }
            "activate" => {
                self.authorize(request, false)?;
                ensure!(
                    request["committed"] == true,
                    "activation requires committed placement"
                );
                let start = request
                    .get("start")
                    .map(|v| v.as_bool().context("invalid activation start flag"))
                    .transpose()?
                    .unwrap_or(false);
                let managed = self
                    .state
                    .placements
                    .get_mut(id)
                    .context("missing prepared copy")?;
                ensure!(
                    matches!(managed.phase, Phase::Prepared | Phase::Active),
                    "copy cannot be activated"
                );
                managed.phase = Phase::Active;
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                if workspace.state == "prepared" {
                    workspace.state = if workspace.hibernation.is_some() {
                        "hibernated"
                    } else {
                        "stopped"
                    }
                    .into();
                }
                self.save()?;
                self.lifecycle_checkpoint("activation")?;
                if start {
                    return self.start(id);
                }
            }
            "export-cleanup" => {
                self.authorize(request, false)?;
                let managed = &self.state.placements[id];
                ensure!(
                    matches!(managed.phase, Phase::Exported | Phase::Cleaned),
                    "source has not completed export"
                );
                self.check_replay(&json!({"request":request}))?;
                ensure!(
                    !self.machines.contains_key(id),
                    "exported source still has a VMM"
                );
                match fs::remove_dir_all(self.machine_dir(id)) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    result => result?,
                }
                File::open(self.root.join("machines"))?.sync_all()?;
                self.lifecycle_checkpoint("cleanup-effect")?;
                self.state.placements.get_mut(id).unwrap().phase = Phase::Cleaned;
                self.state.workspaces.get_mut(id).unwrap().state = "exported-cleaned".into();
                // Retain the fence and capture: late requests must never recreate the old copy.
                self.save()?;
            }
            _ => bail!("unsupported lifecycle operation"),
        }
        self.placement_summary(id)
    }

    fn export_copy(&mut self, request: &Value) -> Result<Value> {
        self.authorize(request, false)?;
        let id = request["id"].as_str().context("missing ID")?;
        let intent = Export {
            transfer: request["transfer"]
                .as_str()
                .context("missing export transfer")?
                .into(),
            capture: request["capture"]
                .as_str()
                .context("missing export capture")?
                .into(),
            disk_only: request["op"] == "seal-disk",
        };
        identifier(&intent.transfer)?;
        identifier(&intent.capture)?;
        let workspace = self.workspace(id)?;
        let managed = self
            .state
            .placements
            .get_mut(id)
            .context("export requires a managed copy")?;
        if let Some(previous) = &managed.intent {
            ensure!(previous == &intent, "different export intent");
        } else {
            ensure!(managed.phase == Phase::Active, "copy is not active");
            ensure!(
                !intent.disk_only
                    || (workspace.state == "stopped" && !self.machines.contains_key(id)),
                "disk sealing requires a stopped copy"
            );
            ensure!(
                !self.snapshot_dir(&intent.capture).exists(),
                "capture name is already occupied"
            );
            ensure!(
                !self.lifecycle_capture(&intent.capture),
                "capture name is already reserved"
            );
            self.state.placements.get_mut(id).unwrap().intent = Some(intent.clone());
            self.state.placements.get_mut(id).unwrap().phase = Phase::Exporting;
            self.state.workspaces.get_mut(id).unwrap().state = "exporting".into();
            self.save()?;
            self.lifecycle_checkpoint("intent")?;
        }
        ensure!(
            self.state.placements[id].phase != Phase::Cleaned,
            "exported copy already cleaned"
        );
        let destination = self.snapshot_dir(&intent.capture);
        let staging = self.snapshot_dir(&format!(".pending-export-{}", intent.capture));
        if !destination.exists() {
            if self.state.placements[id].phase == Phase::Exporting
                && !self.capture_ready.contains_key(id)
            {
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
                fs::create_dir(&staging)?;
                let ram = if self.machines.contains_key(id) {
                    self.storage_headroom(u64::from(workspace.memory_mib) * 1024 * 1024)?;
                    let disk = self.machine_dir(id).join("disk.ext4");
                    self.running(id)?.capture(&staging, &disk, false)?;
                    true
                } else if let Some(name) = &workspace.hibernation {
                    let source = self.verify_snapshot(name, false)?;
                    let snapshot: Snapshot =
                        serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
                    ensure!(
                        snapshot.workspace == id
                            && snapshot.memory_mib == workspace.memory_mib
                            && snapshot.vcpu_count == workspace.vcpu_count
                            && snapshot.image == workspace.image,
                        "hibernation belongs to another copy"
                    );
                    for file in snapshot.files.keys() {
                        vm::reflink(&source.join(file), &staging.join(file))?;
                    }
                    snapshot.files.len() == 3
                } else {
                    vm::reflink(
                        &self.machine_dir(id).join("disk.ext4"),
                        &staging.join("disk.ext4"),
                    )?;
                    false
                };
                // Keep an in-process retry from issuing guest commands to an already paused VM.
                self.capture_ready.insert(id.into(), ram);
                self.lifecycle_checkpoint("capture")?;
            }
            ensure!(
                matches!(
                    self.state.placements[id].phase,
                    Phase::Exporting | Phase::Captured
                ),
                "missing completed export capture"
            );
            let ram = self
                .capture_ready
                .get(id)
                .copied()
                .or(self.state.placements[id].capture_ram)
                .context("captured intent is missing consistency-set kind")?;
            let components = if ram {
                vec!["state", "memory", "disk.ext4"]
            } else {
                vec!["disk.ext4"]
            };
            for file in &components {
                ensure!(
                    fs::symlink_metadata(staging.join(file))?.is_file(),
                    "capture file is not regular"
                );
                File::open(staging.join(file))?.sync_all()?;
            }
            File::open(&staging)?.sync_all()?;
            File::open(self.root.join("snapshots"))?.sync_all()?;
            let managed = self.state.placements.get_mut(id).unwrap();
            managed.phase = Phase::Captured;
            managed.capture_ram = Some(ram);
            self.save()?;
            self.capture_ready.remove(id);
            self.lifecycle_checkpoint("fsync")?;
            // Confirm child exit before releasing its host resources or acknowledging export.
            if let Some(machine) = self.machines.get_mut(id) {
                machine.terminate()?;
            }
            self.machines.remove(id);
            let mut files = BTreeMap::new();
            for file in components {
                ensure!(
                    fs::symlink_metadata(staging.join(file))?.is_file(),
                    "capture file is not regular"
                );
                files.insert(file.into(), hash(&staging.join(file))?);
            }
            let snapshot = Snapshot {
                // Export replay may reuse an earlier seal; its capture start is unknown.
                captured_at: 0,
                managed_recovery: false,
                name: intent.capture.clone(),
                workspace: id.into(),
                managed: true,
                memory_mib: workspace.memory_mib,
                vcpu_count: workspace.vcpu_count,
                image: workspace.image,
                image_digest: workspace.image_digest,
                runtime: crate::common::RUNTIME.into(),
                cpu: self.cpu.clone(),
                kernel_sha256: self.kernel_hash.clone(),
                files,
                digest: None,
                fork: None,
            };
            // RAM copied from hibernation keeps the original compatibility tuple.
            let snapshot = if let Some(name) = &workspace.hibernation {
                let original: Snapshot = serde_json::from_slice(&fs::read(
                    self.snapshot_dir(name).join("manifest.json"),
                )?)?;
                Snapshot {
                    runtime: original.runtime,
                    cpu: original.cpu,
                    kernel_sha256: original.kernel_sha256,
                    ..snapshot
                }
            } else {
                snapshot
            };
            atomic_json(&staging.join("manifest.json"), &snapshot)?;
            for file in snapshot
                .files
                .keys()
                .map(String::as_str)
                .chain(std::iter::once("manifest.json"))
            {
                fs::set_permissions(staging.join(file), fs::Permissions::from_mode(0o400))?;
                File::open(staging.join(file))?.sync_all()?;
            }
            File::open(&staging)?.sync_all()?;
            self.lifecycle_checkpoint("seal")?;
            fs::rename(&staging, &destination)?;
            File::open(self.root.join("snapshots"))?.sync_all()?;
            self.lifecycle_checkpoint("rename")?;
        }
        ensure!(!self.machines.contains_key(id), "source VMM still exists");
        let directory = self.verify_snapshot(&intent.capture, false)?;
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(snapshot.workspace == id, "capture belongs to another copy");
        ensure!(
            snapshot.managed,
            "export capture is missing managed provenance"
        );
        self.state.placements.get_mut(id).unwrap().phase = Phase::Exported;
        self.state.workspaces.get_mut(id).unwrap().state = "exported".into();
        self.save()?;
        self.lifecycle_checkpoint("registry")?;
        let mut result = self.placement_summary(id)?;
        result["capture"] = self.sealed_capture(&intent.capture)?;
        Ok(result)
    }

    /// Native local metadata + sealed uncompressed files, ready for a format-1 adapter.
    fn sealed_capture(&self, name: &str) -> Result<Value> {
        let directory = self.snapshot_dir(name);
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        Ok(
            json!({"snapshot":name,"metadata":snapshot,"manifest_sha256":hash(&directory.join("manifest.json"))?,
            "files":snapshot.files.iter().map(|(file,sha)| (file.clone(),json!({"path":directory.join(file),"sha256":sha}))).collect::<BTreeMap<_,_>>()}),
        )
    }

    fn import_intent(request: &Value) -> Result<Import> {
        let snapshot = request["snapshot"]
            .as_str()
            .context("missing snapshot")?
            .to_owned();
        identifier(&snapshot)?;
        let manifest_sha256 = request["manifest_sha256"]
            .as_str()
            .context("missing native manifest digest")?
            .to_owned();
        ensure!(
            manifest_sha256.len() == 64
                && manifest_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid manifest digest"
        );
        let cold = match request.get("cold") {
            None => false,
            Some(v) => v.as_bool().context("invalid cold consent")?,
        };
        let fork_source = request
            .get("fork_source")
            .map(|v| v.as_str().context("invalid fork source").map(str::to_owned))
            .transpose()?;
        if let Some(source) = &fork_source {
            identifier(source)?;
        }
        ensure!(
            (request["op"] == "fork-prepare") == fork_source.is_some(),
            "fork preparation requires source identity"
        );
        Ok(Import {
            fork_source,
            snapshot,
            manifest_sha256,
            cold,
        })
    }
    fn prepare_import(&mut self, request: &Value, authority: Placement) -> Result<Value> {
        let id = request["id"].as_str().context("missing ID")?;
        let intent = Self::import_intent(request)?;
        let directory = self.verify_snapshot(&intent.snapshot, false)?;
        ensure!(
            hash(&directory.join("manifest.json"))? == intent.manifest_sha256,
            "native manifest digest mismatch"
        );
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(
            snapshot.workspace == intent.fork_source.as_deref().unwrap_or(id)
                && intent.fork_source.as_deref() != Some(id),
            "artifact subject differs from reserved workspace"
        );
        resources(snapshot.memory_mib, snapshot.vcpu_count)?;
        image_profile(&snapshot.image)?;
        let ram = snapshot.files.len() == 3;
        ensure!(
            intent.cold || !ram || snapshot.resumable(&self.cpu, &self.kernel_hash),
            "incompatible RAM requires explicit cold consent"
        );
        let replacing_cleaned = self.state.placements.get(id).is_some_and(|m| {
            matches!(m.phase, Phase::Cleaned | Phase::Discarded)
                && authority.epoch > m.authority.epoch
        }) && !self.deleted(id);
        if let Some(managed) = self.state.placements.get(id).filter(|_| !replacing_cleaned) {
            ensure!(
                managed.authority == authority && managed.import.as_ref() == Some(&intent),
                "different import placement or fingerprint"
            );
            ensure!(
                matches!(
                    managed.phase,
                    Phase::Importing | Phase::Prepared | Phase::Active
                ),
                "import copy has since been exported"
            );
            if managed.phase != Phase::Importing {
                return self.placement_summary(id);
            }
        } else {
            ensure!(
                (!self.state.workspaces.contains_key(id) || replacing_cleaned)
                    && !self.machine_dir(id).exists(),
                "existing copy cannot be overwritten"
            );
            ensure!(
                self.workspace_count() < MAX_WORKSPACES && self.state.next_index < 8192,
                "workspace capacity exhausted"
            );
            self.reserve_disk(directory.join("disk.ext4").metadata()?.len())?;
            let workspace = Workspace {
                id: id.into(),
                index: self.state.next_index,
                state: "importing".into(),
                memory_mib: snapshot.memory_mib,
                vcpu_count: snapshot.vcpu_count,
                image: snapshot.image,
                image_digest: snapshot.image_digest,
                source: intent.fork_source.clone(),
                hibernation: (ram && !intent.cold).then(|| intent.snapshot.clone()),
                managed: true,
            };
            self.state.next_index += 1;
            self.state.workspaces.insert(id.into(), workspace);
            self.state.placements.insert(
                id.into(),
                Managed {
                    authority,
                    phase: Phase::Importing,
                    intent: None,
                    import: Some(intent.clone()),
                    capture_ram: None,
                    fork_initialized: false,
                },
            );
            self.save()?;
            self.lifecycle_checkpoint("import-intent")?;
        }
        let machine = self.machine_dir(id);
        // Only the directory owned by this durable importing intent can be rebuilt.
        if machine.exists() {
            fs::remove_dir_all(&machine)?;
        }
        fs::create_dir(&machine)?;
        self.lifecycle_checkpoint("import-directory")?;
        vm::reflink(&directory.join("disk.ext4"), &machine.join("disk.ext4"))?;
        self.lifecycle_checkpoint("import-copy")?;
        File::open(machine.join("disk.ext4"))?.sync_all()?;
        File::open(&machine)?.sync_all()?;
        File::open(self.root.join("machines"))?.sync_all()?;
        self.state.placements.get_mut(id).unwrap().phase = Phase::Prepared;
        self.state.workspaces.get_mut(id).unwrap().state = "prepared".into();
        self.save()?;
        self.lifecycle_checkpoint("import-registry")?;
        self.placement_summary(id)
    }

    // Persist exclusion before removing bytes: a delayed prepare at this epoch can never start.
    fn discard_import(&mut self, request: &Value, authority: Placement) -> Result<Value> {
        let id = request["id"].as_str().context("missing ID")?;
        if let Some(m) = self.state.placements.get(id) {
            ensure!(
                m.authority == authority
                    && matches!(
                        m.phase,
                        Phase::Importing | Phase::Prepared | Phase::Discarded
                    ),
                "target may be active or belongs to another attempt"
            );
            ensure!(!self.machines.contains_key(id), "target is running");
            self.state.placements.get_mut(id).unwrap().phase = Phase::Discarded;
        } else {
            ensure!(
                !self.state.workspaces.contains_key(id) && !self.machine_dir(id).exists(),
                "unmanaged target cannot be excluded"
            );
            self.state.workspaces.insert(
                id.into(),
                Workspace {
                    id: id.into(),
                    index: 0,
                    state: "discarded".into(),
                    memory_mib: 0,
                    vcpu_count: 0,
                    image: "alpine".into(),
                    image_digest: None,
                    source: None,
                    hibernation: None,
                    managed: true,
                },
            );
            self.state.placements.insert(
                id.into(),
                Managed {
                    authority,
                    phase: Phase::Discarded,
                    intent: None,
                    import: None,
                    capture_ram: None,
                    fork_initialized: false,
                },
            );
        }
        self.state.workspaces.get_mut(id).unwrap().state = "discarded".into();
        self.save()?;
        self.lifecycle_checkpoint("discard-fence")?;
        match fs::remove_dir_all(self.machine_dir(id)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
        File::open(self.root.join("machines"))?.sync_all()?;
        self.placement_summary(id)
    }

    fn lifecycle_checkpoint(&self, name: &str) -> Result<()> {
        #[cfg(test)]
        ensure!(self.crash != Some(name), "injected crash after {name}");
        let _ = name;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Runtime {
        let runtime = super::super::disk_tests::fixture();
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(4096)
            .unwrap();
        runtime
    }
    fn authority(epoch: u64) -> Value {
        json!({"epoch":epoch,"node":"alpha","transfer":"incarnation"})
    }
    fn request(op: &str) -> Value {
        let mut request = json!({"op":op,"id":"legacy","placement":authority(7)});
        if op == "placement-register" {
            request["committed"] = json!(true);
        }
        request
    }
    fn register(runtime: &mut Runtime) {
        runtime.request(&request("placement-register")).unwrap();
    }
    fn export() -> Value {
        let mut r = request("export");
        r["transfer"] = json!("move1");
        r["capture"] = json!("capture1");
        r
    }
    fn restart(runtime: &mut Runtime) {
        runtime.machines.clear();
        runtime.capture_ready.clear();
        runtime.state = State::read(&fs::read(runtime.root.join("state.json")).unwrap()).unwrap();
        Runtime::recover_state(&mut runtime.state).unwrap();
        runtime.crash = None;
        runtime.cleanup_pending_snapshots().unwrap();
        runtime.save().unwrap();
    }
    #[test]
    fn fork_preparation_preserves_parent_disk_and_needs_commit_and_fresh_ssh_enrollment() {
        let mut runtime = fixture();
        register(&mut runtime);
        runtime.request(&export()).unwrap();
        let digest = hash(&runtime.snapshot_dir("capture1/manifest.json")).unwrap();
        let fork = json!({"op":"fork-prepare","id":"child","fork_source":"legacy","placement":authority(8),"snapshot":"capture1","manifest_sha256":digest});
        let before = fs::read(runtime.machine_dir("legacy/disk.ext4")).unwrap();
        let prepared = runtime.request(&fork).unwrap();
        assert_eq!(prepared["state"], "prepared");
        assert_eq!(prepared["source"], "legacy");
        assert!(!crate::ssh_guest::path(&runtime.root, "child").exists());
        assert!(
            runtime
                .request(&json!({"op":"start","id":"child","placement":authority(8)}))
                .is_err()
        );
        use std::os::unix::fs::PermissionsExt;
        let sealed = runtime.snapshot_dir("capture1/disk.ext4");
        let child = runtime.machine_dir("child/disk.ext4");
        assert_eq!(
            fs::metadata(&sealed).unwrap().permissions().mode() & 0o222,
            0
        );
        assert_ne!(
            fs::metadata(&child).unwrap().permissions().mode() & 0o200,
            0
        );
        fs::write(&child, b"independent child").unwrap();
        assert_eq!(fs::read(&sealed).unwrap(), before);
        assert_eq!(
            fs::read(runtime.machine_dir("legacy/disk.ext4")).unwrap(),
            before
        );
        assert_eq!(runtime.state.value()["worker_state_schema"], 3);
        restart(&mut runtime);
        assert_eq!(runtime.request(&fork).unwrap()["state"], "prepared");
        assert!(!runtime.state.placements["child"].fork_initialized);
        let mut invalid = fork.clone();
        invalid["fork_source"] = json!("other");
        assert!(runtime.request(&invalid).is_err());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn discard_before_or_after_import_permanently_excludes_the_old_epoch_across_restart() {
        for imported in [false, true] {
            for checkpoint in [None, Some("discard-fence")] {
                let mut runtime = fixture();
                register(&mut runtime);
                runtime.request(&export()).unwrap();
                let fork = json!({"op":"fork-prepare","id":"child","fork_source":"legacy","placement":authority(8),"snapshot":"capture1","manifest_sha256":hash(&runtime.snapshot_dir("capture1/manifest.json")).unwrap()});
                if imported {
                    runtime.request(&fork).unwrap();
                }
                let discard = json!({"op":"import-discard","id":"child","placement":authority(8)});
                runtime.crash = checkpoint;
                let result = runtime.request(&discard);
                assert_eq!(result.is_err(), checkpoint.is_some());
                restart(&mut runtime);
                assert!(runtime.request(&fork).is_err());
                assert!(runtime.request(&json!({"op":"activate","id":"child","placement":authority(8),"committed":true,"start":true})).is_err());
                assert_eq!(
                    runtime.request(&discard).unwrap()["lifecycle"]["phase"],
                    "discarded"
                );
                assert!(!runtime.machine_dir("child").exists());
                let mut next = fork;
                next["placement"] = authority(9);
                assert_eq!(runtime.request(&next).unwrap()["state"], "prepared");
                assert!(runtime.request(&discard).is_err());
                fs::remove_dir_all(runtime.root).unwrap();
            }
        }
    }

    #[test]
    fn recovery_preserves_managed_authority_and_durability_summary() {
        let mut runtime = fixture();
        fs::create_dir(runtime.root.join("recovery")).unwrap();
        register(&mut runtime);
        let mut prepare = request("recovery-prepare");
        prepare["store"] = json!("a".repeat(64));
        let mut missing = prepare.clone();
        missing.as_object_mut().unwrap().remove("placement");
        assert!(runtime.request(&missing).is_err());
        let mut stale = prepare.clone();
        stale["placement"] = authority(6);
        assert!(runtime.request(&stale).is_err());
        assert!(runtime.request(&prepare).unwrap().is_null());
        let summary = runtime
            .request(&json!({"op":"inspect","id":"legacy"}))
            .unwrap();
        assert_eq!(summary["placement"], authority(7));
        assert!(summary["durability"]["last_durable_at"].is_null());
        assert!(summary["durability"]["overdue"].as_bool().unwrap());
        let mut journal: crate::recovery::Recovery =
            serde_json::from_slice(&fs::read(runtime.root.join("recovery/legacy.json")).unwrap())
                .unwrap();
        journal.pending = Some(crate::recovery::Point {
            snapshot: "sleep-recovery-fixture".into(),
            captured_at: 1000,
            source: None,
        });
        atomic_json(&runtime.root.join("recovery/legacy.json"), &journal).unwrap();
        let mut result = request("recovery-result");
        result["store"] = prepare["store"].clone();
        result["snapshot"] = json!("sleep-recovery-fixture");
        result["uploaded"] = json!(false);
        stale = result.clone();
        stale["placement"] = authority(6);
        assert!(runtime.request(&stale).is_err());
        assert_eq!(runtime.request(&result).unwrap()["failures"], 1);
        fs::remove_dir_all(runtime.root).unwrap();
    }

    fn assert_fenced(runtime: &mut Runtime) {
        for op in [
            "start",
            "exec",
            "shell-exec",
            "get",
            "put",
            "ssh-keys",
            "aws-token",
            "stop",
            "delete",
            "restore",
            "snapshot",
            "fork",
            "recovery-prepare",
            "recovery-result",
            "recovery-seal",
        ] {
            // A cleaned export may still be deleted with its exact authority (a deleted box).
            if op == "delete" && runtime.state.placements["legacy"].phase == Phase::Cleaned {
                continue;
            }
            let e = runtime.request(&request(op)).unwrap_err();
            assert!(e.to_string().contains("fenced"), "{op}: {e:#}");
        }
        for op in ["ssh", "terminal", "tunnel"] {
            let req = request(op);
            let e = match op {
                "ssh" => runtime.ssh_request(&req).map(|_| ()).unwrap_err(),
                "terminal" => runtime.terminal_request(&req).map(|_| ()).unwrap_err(),
                _ => runtime.address_request(&req).map(|_| ()).unwrap_err(),
            };
            assert!(e.to_string().contains("fenced"), "{op}: {e:#}");
        }
        assert!(runtime.ssh("legacy").is_err());
        assert!(runtime.terminal("legacy").is_err());
        assert!(runtime.address("legacy").is_err());
    }
    fn ram_stage(runtime: &Runtime) {
        let directory = runtime.snapshot_dir(".pending-export-capture1");
        fs::create_dir_all(&directory).unwrap();
        for file in ["state", "memory", "disk.ext4"] {
            fs::write(directory.join(file), file.as_bytes()).unwrap();
        }
    }
    fn import_request(runtime: &Runtime) -> Value {
        json!({"op":"import-prepare","id":"legacy","placement":authority(8),"snapshot":"capture1",
            "manifest_sha256":hash(&runtime.snapshot_dir("capture1").join("manifest.json")).unwrap()})
    }
    fn forget_source_for_target_fixture(runtime: &mut Runtime) {
        runtime.state.workspaces.clear();
        runtime.state.placements.clear();
        fs::remove_dir_all(runtime.machine_dir("legacy")).unwrap();
        runtime.save().unwrap();
    }

    #[test]
    fn pinned_image_metadata_survives_managed_disk_and_ram_exports_and_prepared_import() {
        for ram in [false, true] {
            let mut runtime = fixture();
            let digest = "a".repeat(64);
            let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
            workspace.image = "tools@1".into();
            workspace.image_digest = Some(digest.clone());
            register(&mut runtime);
            let server = if ram {
                let (machine, server) = vm::capture_fixture(&runtime.machine_dir("legacy"), false);
                runtime.machines.insert("legacy".into(), machine);
                Some(server)
            } else {
                None
            };
            let exported = runtime.request(&export()).unwrap();
            if let Some(server) = server {
                server.join().unwrap();
            }
            assert_eq!(exported["capture"]["metadata"]["image"], "tools@1");
            assert_eq!(exported["capture"]["metadata"]["image_digest"], digest);
            assert_eq!(exported["capture"]["metadata"]["managed"], true);
            assert_eq!(
                exported["capture"]["files"].as_object().unwrap().len(),
                if ram { 3 } else { 1 }
            );
            let req = import_request(&runtime);
            forget_source_for_target_fixture(&mut runtime);
            let prepared = runtime.request(&req).unwrap();
            assert_eq!(prepared["state"], "prepared");
            assert_eq!(prepared["image"], "tools@1");
            assert_eq!(prepared["image_digest"], digest);
            assert!(runtime.machines.is_empty());
            restart(&mut runtime);
            assert_eq!(runtime.request(&req).unwrap(), prepared);
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }

    #[test]
    fn export_crashes_replay_one_capture_and_preserve_the_fence() {
        for point in ["intent", "capture", "fsync", "seal", "rename", "registry"] {
            let mut runtime = fixture();
            register(&mut runtime);
            runtime.crash = Some(point);
            assert!(
                runtime
                    .request(&export())
                    .unwrap_err()
                    .to_string()
                    .contains("injected crash")
            );
            assert_fenced(&mut runtime);
            restart(&mut runtime);
            assert_fenced(&mut runtime);
            let first = runtime.request(&export()).unwrap();
            assert_eq!(first["capture"]["metadata"]["managed"], true);
            let bytes = fs::read(runtime.snapshot_dir("capture1").join("manifest.json")).unwrap();
            assert_eq!(first["state"], "exported");
            assert_eq!(runtime.request(&export()).unwrap(), first);
            assert_eq!(
                fs::read(runtime.snapshot_dir("capture1").join("manifest.json")).unwrap(),
                bytes
            );
            assert_eq!(
                fs::read_dir(runtime.root.join("snapshots"))
                    .unwrap()
                    .count(),
                1
            );
            assert_fenced(&mut runtime);
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }
    #[test]
    fn export_cases_a_b_c_d_keep_completed_ram_and_discard_only_incomplete_ram() {
        // a/b: manifest sealed with/without final registry write, covered by registry/rename above.
        // c: captured and fsynced staging survives startup and is sealed without recapture.
        // d: capture-in-progress died with worker; only that incomplete RAM is discarded.
        for phase in [Phase::Captured, Phase::Exporting] {
            let mut runtime = fixture();
            register(&mut runtime);
            runtime.crash = Some("intent");
            runtime.request(&export()).unwrap_err();
            ram_stage(&runtime);
            runtime.state.placements.get_mut("legacy").unwrap().phase = phase.clone();
            runtime
                .state
                .placements
                .get_mut("legacy")
                .unwrap()
                .capture_ram = Some(true);
            runtime.save().unwrap();
            restart(&mut runtime);
            assert!(
                runtime
                    .snapshot_dir(".pending-export-capture1/memory")
                    .exists()
            );
            let result = runtime.request(&export()).unwrap();
            let files = result["capture"]["files"].as_object().unwrap();
            assert_eq!(files.len(), if phase == Phase::Captured { 3 } else { 1 });
            if phase == Phase::Captured {
                assert_eq!(
                    fs::read(runtime.snapshot_dir("capture1/memory")).unwrap(),
                    b"memory"
                );
            }
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }
    #[test]
    fn importing_missing_partial_and_complete_directories_registers_once_unstarted() {
        for point in [
            "import-intent",
            "import-directory",
            "import-copy",
            "import-registry",
        ] {
            let mut runtime = fixture();
            register(&mut runtime);
            runtime.request(&export()).unwrap();
            let req = import_request(&runtime);
            forget_source_for_target_fixture(&mut runtime);
            let index = runtime.state.next_index;
            runtime.crash = Some(point);
            runtime.request(&req).unwrap_err();
            if point == "import-directory" {
                fs::write(runtime.machine_dir("legacy/disk.ext4"), b"partial write").unwrap();
            }
            restart(&mut runtime);
            let first = runtime.request(&req).unwrap();
            assert_eq!(first["state"], "prepared");
            assert!(runtime.lifecycle_capture("capture1"));
            assert_eq!(runtime.request(&req).unwrap(), first);
            assert_eq!(runtime.state.next_index, index + 1);
            assert!(runtime.machines.is_empty());
            let mut start = request("start");
            start["placement"] = authority(8);
            assert!(
                runtime
                    .request(&start)
                    .unwrap_err()
                    .to_string()
                    .contains("not activated")
            );
            let mut activate = request("activate");
            activate["placement"] = authority(8);
            assert!(runtime.request(&activate).is_err());
            activate["committed"] = json!(true);
            activate["start"] = json!("invalid");
            assert!(runtime.request(&activate).is_err());
            activate.as_object_mut().unwrap().remove("start");
            runtime.crash = Some("activation");
            assert!(runtime.request(&activate).is_err());
            restart(&mut runtime);
            runtime.request(&activate).unwrap();
            assert_eq!(runtime.request(&activate).unwrap()["state"], "stopped");
            restart(&mut runtime);
            assert!(runtime.authorize(&start, true).is_ok());
            assert!(runtime.machines.is_empty());
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }
    #[test]
    fn every_managed_wake_requires_exact_activated_authority() {
        let mut runtime = fixture();
        register(&mut runtime);
        for op in [
            "start",
            "exec",
            "shell-exec",
            "get",
            "put",
            "ssh-keys",
            "ssh",
            "terminal",
            "tunnel",
        ] {
            for placement in [
                None,
                Some(authority(6)),
                Some(authority(8)),
                Some(json!({"epoch":7,"node":"beta","transfer":"incarnation"})),
                Some(json!({"epoch":7,"node":"alpha","transfer":"other"})),
                Some(json!({"node":"alpha","transfer":"incarnation"})),
            ] {
                let mut req = request(op);
                if let Some(p) = placement {
                    req["placement"] = p;
                } else {
                    req.as_object_mut().unwrap().remove("placement");
                }
                let error = match op {
                    "ssh" => runtime.ssh_request(&req).map(|_| ()).unwrap_err(),
                    "terminal" => runtime.terminal_request(&req).map(|_| ()).unwrap_err(),
                    "tunnel" => runtime.address_request(&req).map(|_| ()).unwrap_err(),
                    _ => runtime.request(&req).map(|_| ()).unwrap_err(),
                };
                assert!(
                    format!("{error:#}").contains("placement")
                        || format!("{error:#}").contains("epoch"),
                    "{op}: {error:#}"
                );
            }
        }
        // Loss of the authority cannot reinterpret the persisted managed marker as legacy.
        runtime.state.placements.clear();
        runtime.save().unwrap();
        assert!(Runtime::recover_state(&mut runtime.state).is_err());
        assert!(
            runtime
                .request(&json!({"op":"start","id":"legacy"}))
                .is_err()
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }
    #[test]
    fn cleanup_is_source_only_keeps_all_captures_and_rejects_stale_results() {
        let mut runtime = fixture();
        register(&mut runtime);
        let result = runtime.request(&export()).unwrap();
        assert!(runtime.lifecycle_capture("capture1"));
        let keep = runtime.snapshot_dir("unrelated");
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("manifest.json"), b"keep").unwrap();
        let mut cleanup = export();
        cleanup["op"] = json!("export-cleanup");
        runtime.crash = Some("cleanup-effect");
        assert!(runtime.request(&cleanup).is_err());
        restart(&mut runtime);
        let first = runtime.request(&cleanup).unwrap();
        assert_eq!(runtime.request(&cleanup).unwrap(), first);
        assert!(!runtime.machine_dir("legacy").exists());
        assert!(runtime.snapshot_dir("capture1/manifest.json").exists());
        assert!(runtime.lifecycle_capture("capture1"));
        assert!(keep.join("manifest.json").exists());
        restart(&mut runtime);
        assert_fenced(&mut runtime);
        runtime
            .state
            .placements
            .get_mut("legacy")
            .unwrap()
            .authority
            .epoch = 8;
        assert!(runtime.check_replay(&json!({"request":cleanup})).is_err());
        assert_eq!(result["placement"]["epoch"], 7);
        fs::remove_dir_all(runtime.root).unwrap();
    }
    #[test]
    fn import_rejects_subject_hash_existing_copy_and_implicit_cold_loss() {
        let mut runtime = fixture();
        register(&mut runtime);
        runtime.crash = Some("intent");
        runtime.request(&export()).unwrap_err();
        ram_stage(&runtime);
        runtime.state.placements.get_mut("legacy").unwrap().phase = Phase::Captured;
        runtime
            .state
            .placements
            .get_mut("legacy")
            .unwrap()
            .capture_ram = Some(true);
        runtime.crash = None;
        runtime.request(&export()).unwrap();
        let req = import_request(&runtime);
        assert!(runtime.request(&req).is_err());
        forget_source_for_target_fixture(&mut runtime);
        runtime.cpu = "different CPU".into();
        assert!(
            runtime
                .request(&req)
                .unwrap_err()
                .to_string()
                .contains("cold consent")
        );
        assert!(runtime.state.workspaces.is_empty());
        let mut bad = req.clone();
        bad["manifest_sha256"] = json!("0".repeat(64));
        assert!(runtime.request(&bad).is_err());
        bad = req.clone();
        bad["id"] = json!("another");
        assert!(runtime.request(&bad).is_err());
        let mut cold = req;
        cold["cold"] = json!(true);
        let prepared = runtime.request(&cold).unwrap();
        assert!(prepared["hibernation"].is_null());
        assert!(runtime.snapshot_dir("capture1/memory").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }
    #[test]
    fn disk_seal_is_stopped_only_and_standalone_requests_stay_legacy() {
        let mut runtime = fixture();
        assert!(
            runtime
                .authorize(&json!({"op":"start","id":"legacy"}), true)
                .is_ok()
        );
        assert!(
            !runtime
                .request(&json!({"op":"inspect","id":"legacy"}))
                .unwrap()["managed"]
                .as_bool()
                .unwrap()
        );
        register(&mut runtime);
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "hibernated".into();
        let mut req = export();
        req["op"] = json!("seal-disk");
        assert!(runtime.request(&req).is_err());
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "stopped".into();
        assert_eq!(
            runtime.request(&req).unwrap()["capture"]["files"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        runtime.state.placements.get_mut("legacy").unwrap().phase = Phase::Active;
        assert!(
            runtime.authorize(&request("start"), true).is_err(),
            "an export intent fences wake independently of the phase label"
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn review_managed_export_cannot_enter_through_legacy_import() {
        for ram in [false, true] {
            let mut source = fixture();
            register(&mut source);
            let server = ram.then(|| {
                let (machine, server) = vm::capture_fixture(&source.machine_dir("legacy"), false);
                source.machines.insert("legacy".into(), machine);
                server
            });
            let exported = source.request(&export()).unwrap();
            if let Some(server) = server {
                server.join().unwrap();
            }
            for cold in [true, false].into_iter().filter(|cold| ram || *cold) {
                let mut target = fixture();
                forget_source_for_target_fixture(&mut target);
                let directory = target.snapshot_dir("capture1");
                fs::create_dir(&directory).unwrap();
                for entry in fs::read_dir(source.snapshot_dir("capture1")).unwrap() {
                    let entry = entry.unwrap();
                    fs::copy(entry.path(), directory.join(entry.file_name())).unwrap();
                }
                let before = fs::read(target.root.join("state.json")).unwrap();
                let index = target.state.next_index;
                target.closing = true;
                let error = target
                    .request(&json!({"op":"import","snapshot":"capture1","cold":cold}))
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("managed capture requires import-prepare"),
                    "legacy import reached registration or launch: {error:#}"
                );
                assert!(!target.machine_dir("legacy").exists());
                assert!(!target.state.workspaces.contains_key("legacy"));
                assert_eq!(target.state.next_index, index);
                assert_eq!(fs::read(target.root.join("state.json")).unwrap(), before);
                restart(&mut target);
                assert!(
                    target
                        .request(&json!({"op":"start","id":"legacy"}))
                        .is_err()
                );
                let mut prepare = import_request(&target);
                prepare["cold"] = json!(cold);
                assert_eq!(target.request(&prepare).unwrap()["state"], "prepared");
                restart(&mut target);
                assert!(
                    target
                        .request(&json!({"op":"start","id":"legacy"}))
                        .is_err()
                );
                let mut start = request("start");
                start["placement"] = authority(8);
                assert!(
                    target
                        .request(&start)
                        .unwrap_err()
                        .to_string()
                        .contains("not activated")
                );
                let mut activate = request("activate");
                activate["placement"] = authority(8);
                activate["committed"] = json!(true);
                target.request(&activate).unwrap();
                assert!(target.authorize(&start, true).is_ok());
                assert!(
                    target
                        .authorize(&json!({"op":"start","id":"legacy"}), true)
                        .is_err()
                );
                assert!(target.machines.is_empty());
                fs::remove_dir_all(target.root).unwrap();
            }
            assert_eq!(exported["capture"]["metadata"]["managed"], true);
            fs::remove_dir_all(source.root).unwrap();
        }
    }

    #[test]
    fn managed_snapshot_and_hibernation_provenance_survives_copy_to_a_fresh_worker() {
        for managed in [false, true] {
            for hibernate in [false, true] {
                let mut source = fixture();
                if managed {
                    register(&mut source);
                }
                let (machine, server) =
                    vm::capture_fixture(&source.machine_dir("legacy"), !hibernate);
                source.machines.insert("legacy".into(), machine);
                let mut capture = if managed {
                    request(if hibernate { "hibernate" } else { "snapshot" })
                } else {
                    json!({"op":if hibernate {"hibernate"} else {"snapshot"},"id":"legacy"})
                };
                capture["name"] = json!("capture1");
                let result = source.request(&capture).unwrap();
                source.machines.clear();
                server.join().unwrap();
                let name = result[if hibernate { "hibernation" } else { "name" }]
                    .as_str()
                    .unwrap();
                let mut target = fixture();
                let directory = target.snapshot_dir(name);
                fs::create_dir(&directory).unwrap();
                for entry in fs::read_dir(source.snapshot_dir(name)).unwrap() {
                    let entry = entry.unwrap();
                    fs::copy(entry.path(), directory.join(entry.file_name())).unwrap();
                }
                let manifest = directory.join("manifest.json");
                let mut metadata: Value =
                    serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
                assert_eq!(metadata["managed"], managed);
                if managed {
                    let before = fs::read(target.machine_dir("legacy/disk.ext4")).unwrap();
                    for req in [
                        json!({"op":"restore","id":"legacy","name":name}),
                        json!({"op":"fork","id":"legacy","child":"child","snapshot":name}),
                    ] {
                        let error = target.request(&req).unwrap_err();
                        assert!(
                            error
                                .to_string()
                                .contains("managed capture requires import-prepare"),
                            "{error:#}"
                        );
                    }
                    assert_eq!(
                        fs::read(target.machine_dir("legacy/disk.ext4")).unwrap(),
                        before
                    );
                    assert!(!target.machine_dir("child").exists());
                    let legacy = target.state.workspaces.get_mut("legacy").unwrap();
                    legacy.hibernation = Some(name.into());
                    legacy.state = "hibernated".into();
                    target.save().unwrap();
                    restart(&mut target);
                    let error = target
                        .request(&json!({"op":"start","id":"legacy"}))
                        .unwrap_err();
                    assert!(
                        error
                            .to_string()
                            .contains("managed capture requires import-prepare"),
                        "{error:#}"
                    );
                } else {
                    // Pre-fencing unmanaged manifests have no provenance field.
                    metadata.as_object_mut().unwrap().remove("managed");
                    atomic_json(&manifest, &metadata).unwrap();
                }
                forget_source_for_target_fixture(&mut target);
                for cold in [false, true] {
                    target.closing = true;
                    let error = target
                        .request(&json!({"op":"import","snapshot":name,"cold":cold}))
                        .unwrap_err();
                    if managed {
                        assert!(
                            error
                                .to_string()
                                .contains("managed capture requires import-prepare"),
                            "{error:#}"
                        );
                        assert!(!target.state.workspaces.contains_key("legacy"));
                        assert!(!target.machine_dir("legacy").exists());
                    } else {
                        assert!(
                            error.to_string().contains("worker is shutting down"),
                            "{error:#}"
                        );
                        assert_eq!(
                            target
                                .request(&json!({"op":"inspect","id":"legacy"}))
                                .unwrap()["managed"],
                            false
                        );
                        restart(&mut target);
                        assert!(
                            target
                                .authorize(&json!({"op":"start","id":"legacy"}), true)
                                .is_ok()
                        );
                        forget_source_for_target_fixture(&mut target);
                    }
                }
                fs::remove_dir_all(target.root).unwrap();
                fs::remove_dir_all(source.root).unwrap();
            }
        }
    }

    #[test]
    fn review_cached_unmanaged_delete_validation_survives_the_missing_record() {
        let mut runtime = fixture();
        let delete = json!({"op":"delete","id":"legacy"});
        assert_eq!(
            runtime.request(&delete).unwrap(),
            json!({"deleted":"legacy","snapshots":[]})
        );
        let check = json!({"op":"placement-check","id":"legacy","request":delete});
        assert!(
            runtime.request(&check).is_ok(),
            "a completed unmanaged delete must remain replayable"
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn review_ordinary_snapshot_respects_an_interrupted_export_reservation() {
        for reserved in [false, true] {
            let mut runtime = fixture();
            let mut other = runtime.workspace("legacy").unwrap();
            other.id = "other".into();
            other.memory_mib = 256;
            other.state = "running".into();
            runtime.state.workspaces.insert("other".into(), other);
            fs::create_dir(runtime.machine_dir("other")).unwrap();
            fs::write(runtime.machine_dir("other/disk.ext4"), b"other disk").unwrap();
            if reserved {
                register(&mut runtime);
                runtime.crash = Some("intent");
                runtime.request(&export()).unwrap_err();
                runtime.crash = None;
            }
            let (machine, server) = vm::capture_fixture(&runtime.machine_dir("other"), true);
            runtime.machines.insert("other".into(), machine);
            let snapshot =
                runtime.request(&json!({"op":"snapshot","id":"other","name":"capture1"}));
            let replay = reserved.then(|| runtime.request(&export()));
            runtime.machines.remove("other");
            server.join().unwrap();
            if reserved {
                assert!(
                    snapshot.is_err(),
                    "ordinary snapshot stole the export reservation: {snapshot:?}"
                );
                assert_eq!(
                    replay.unwrap().unwrap()["capture"]["metadata"]["workspace"],
                    "legacy"
                );
            } else {
                assert_eq!(snapshot.unwrap()["workspace"], "other");
            }
            fs::remove_dir_all(runtime.root).unwrap();
        }
    }

    #[test]
    fn review_cleaned_fences_release_capacity_without_losing_admission_or_epoch_guards() {
        fn incoming(runtime: &Runtime, subject: &str, name: &str, managed: bool) -> Value {
            let mut snapshot: Snapshot = serde_json::from_slice(
                &fs::read(runtime.snapshot_dir("sealed0/manifest.json")).unwrap(),
            )
            .unwrap();
            snapshot.name = name.into();
            snapshot.workspace = subject.into();
            snapshot.managed = managed;
            let directory = runtime.snapshot_dir(name);
            fs::create_dir(&directory).unwrap();
            fs::copy(
                runtime.snapshot_dir("sealed0/disk.ext4"),
                directory.join("disk.ext4"),
            )
            .unwrap();
            atomic_json(&directory.join("manifest.json"), &snapshot).unwrap();
            json!({"op":"import-prepare","id":subject,"placement":authority(8),"snapshot":name,
                "manifest_sha256":hash(&directory.join("manifest.json")).unwrap()})
        }
        let mut runtime = fixture();
        let template = runtime.workspace("legacy").unwrap();
        runtime
            .request(&json!({"op":"delete","id":"legacy"}))
            .unwrap();
        for i in 0..MAX_WORKSPACES {
            let id = format!("source{i}");
            let mut workspace = template.clone();
            workspace.id = id.clone();
            workspace.index = i as u32 + 1;
            fs::create_dir(runtime.machine_dir(&id)).unwrap();
            fs::write(runtime.machine_dir(&id).join("disk.ext4"), b"source disk").unwrap();
            runtime.state.workspaces.insert(id.clone(), workspace);
            let mut register = request("placement-register");
            register["id"] = json!(id);
            runtime.request(&register).unwrap();
            let export = json!({"op":"export","id":id,"placement":authority(7),"transfer":format!("move{i}"),"capture":format!("sealed{i}")});
            runtime.request(&export).unwrap();
            let mut cleanup = export;
            cleanup["op"] = json!("export-cleanup");
            runtime.request(&cleanup).unwrap();
        }
        assert_eq!(
            fs::read_dir(runtime.root.join("machines")).unwrap().count(),
            0
        );
        restart(&mut runtime);
        let count = runtime.request(&json!({"op":"status"})).unwrap()["workspace_count"].clone();
        let import = incoming(&runtime, "target0", "incoming0", true);
        let prepared = runtime.request(&import);
        assert!(
            prepared.is_ok(),
            "empty runner after {MAX_WORKSPACES} cleanups rejected a new subject: {prepared:?}"
        );
        assert_eq!(count, 0);
        assert_eq!(runtime.state.placements.len(), MAX_WORKSPACES + 1);
        let legacy = incoming(&runtime, "legacy-target", "legacy-import", false);
        // Exercise legacy registration while quiesce prevents any guest boot.
        runtime.closing = true;
        let result = runtime
            .request(&json!({"op":"import","snapshot":legacy["snapshot"],"cold":true}))
            .unwrap_err();
        assert!(
            result.to_string().contains("worker is shutting down"),
            "legacy import still counts tombstones: {result:#}"
        );
        assert!(runtime.workspace("legacy-target").is_ok());
        assert!(runtime.machines.is_empty());
        runtime.closing = false;
        runtime
            .request(&json!({"op":"delete","id":"legacy-target"}))
            .unwrap();
        assert!(
            runtime
                .request(&json!({"op":"start","id":"source0"}))
                .is_err()
        );
        assert_eq!(
            runtime
                .request(&json!({"op":"inspect","id":"source0"}))
                .unwrap()["state"],
            "exported-cleaned"
        );
        let create = runtime
            .request(&json!({"op":"create","id":"fresh"}))
            .unwrap_err();
        assert!(
            create.to_string().contains("image profile is not prepared"),
            "ordinary create still counts tombstones: {create:#}"
        );
        for i in 1..MAX_WORKSPACES {
            let import = incoming(
                &runtime,
                &format!("target{i}"),
                &format!("incoming{i}"),
                true,
            );
            runtime.request(&import).unwrap();
        }
        assert_eq!(
            runtime.request(&json!({"op":"status"})).unwrap()["workspace_count"],
            MAX_WORKSPACES
        );
        assert!(
            runtime.request(&import).is_ok(),
            "a prepared retry does not need another slot"
        );
        let overflow = incoming(&runtime, "overflow", "incoming-overflow", false);
        assert!(
            runtime
                .request(&overflow)
                .unwrap_err()
                .to_string()
                .contains("capacity exhausted")
        );
        assert!(
            runtime
                .request(&json!({"op":"create","id":"fresh"}))
                .unwrap_err()
                .to_string()
                .contains("workspace limit")
        );
        assert!(
            runtime
                .request(&json!({"op":"import","snapshot":"incoming-overflow","cold":true}))
                .unwrap_err()
                .to_string()
                .contains("workspace limit")
        );
        let returning = json!({"op":"import-prepare","id":"source0","placement":authority(8),"snapshot":"sealed0",
            "manifest_sha256":hash(&runtime.snapshot_dir("sealed0/manifest.json")).unwrap()});
        assert!(
            runtime
                .request(&returning)
                .unwrap_err()
                .to_string()
                .contains("capacity exhausted"),
            "returning subjects need a live slot too"
        );
        assert!(runtime.machines.is_empty());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn interrupted_export_reserves_its_capture_name_and_missing_markers_fail_closed() {
        let mut runtime = fixture();
        let mut other = runtime.workspace("legacy").unwrap();
        other.id = "other".into();
        fs::create_dir(runtime.machine_dir("other")).unwrap();
        fs::write(runtime.machine_dir("other/disk.ext4"), b"other disk").unwrap();
        runtime.state.workspaces.insert("other".into(), other);
        register(&mut runtime);
        let mut register_other = request("placement-register");
        register_other["id"] = json!("other");
        runtime.request(&register_other).unwrap();
        runtime.crash = Some("intent");
        runtime.request(&export()).unwrap_err();
        runtime.crash = None;
        let mut competing = export();
        competing["id"] = json!("other");
        assert!(
            runtime
                .request(&competing)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        runtime.state.placements.clear();
        for workspace in runtime.state.workspaces.values_mut() {
            workspace.managed = false;
        }
        assert!(Runtime::recover_state(&mut runtime.state).is_err());
        fs::remove_dir_all(runtime.root).unwrap();
    }
}

#[cfg(test)]
mod execution_tests {
    use super::*;
    #[test]
    fn running_export_waits_for_child_exit_and_keeps_one_complete_ram_capture() {
        let mut runtime = super::super::disk_tests::fixture();
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(4096)
            .unwrap();
        let placement = json!({"epoch":1,"node":"alpha","transfer":"initial"});
        runtime.request(&json!({"op":"placement-register","id":"legacy","committed":true,"placement":placement})).unwrap();
        runtime
            .state
            .workspaces
            .get_mut("legacy")
            .unwrap()
            .memory_mib = 256;
        let (machine, fixture) = vm::capture_fixture(&runtime.root, false);
        let pid = machine.child.id();
        runtime.machines.insert("legacy".into(), machine);
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "running".into();
        let start = json!({"op":"start","id":"legacy","placement":placement});
        assert_eq!(
            runtime.request(&start).unwrap(),
            runtime.request(&start).unwrap()
        );
        assert_eq!(runtime.machines["legacy"].child.id(), pid);
        assert_eq!(runtime.machines.len(), 1);
        let request = json!({"op":"export","id":"legacy","placement":placement,"transfer":"move1","capture":"capture1"});
        runtime.crash = Some("capture");
        runtime.request(&request).unwrap_err();
        fixture.join().unwrap();
        let durable = State::read(&fs::read(runtime.root.join("state.json")).unwrap()).unwrap();
        assert!(durable.placements["legacy"].phase == Phase::Exporting);
        let memory = runtime.snapshot_dir(".pending-export-capture1/memory");
        fs::remove_file(&memory).unwrap();
        runtime.crash = None;
        assert!(
            runtime.request(&request).is_err(),
            "completed RAM cannot become disk-only"
        );
        assert!(
            PathBuf::from(format!("/proc/{pid}")).exists(),
            "incomplete capture cannot terminate the source"
        );
        fs::write(&memory, b"VM memory").unwrap();
        // Effect-before-reply: complete registry is present even if acknowledgment is lost.
        runtime.crash = Some("registry");
        assert!(runtime.request(&request).is_err());
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
        assert!(runtime.machines.is_empty());
        runtime.crash = None;
        let result = runtime.request(&request).unwrap();
        assert_eq!(result["capture"]["files"].as_object().unwrap().len(), 3);
        assert_eq!(
            fs::read(runtime.snapshot_dir("capture1/memory")).unwrap(),
            b"VM memory"
        );
        assert_eq!(runtime.request(&request).unwrap(), result);
        fs::remove_dir_all(runtime.root).unwrap();
    }
    #[test]
    fn managed_registry_refuses_legacy_readers_and_missing_reader_guards() {
        let mut runtime = super::super::disk_tests::fixture();
        let original = runtime.state.value();
        assert!(serde_json::from_value::<State>(original).is_ok());
        runtime.request(&json!({"op":"placement-register","id":"legacy","committed":true,"placement":{"epoch":1,"node":"alpha","transfer":"initial"}})).unwrap();
        let value = runtime.state.value();
        assert!(
            value["workspaces"].is_object(),
            "operator inventory keeps its existing shape"
        );
        assert!(
            serde_json::from_value::<State>(value.clone()).is_err(),
            "legacy reader requires an integer next_index"
        );
        assert!(State::read(&serde_json::to_vec(&value).unwrap()).is_ok());
        let mut missing = value;
        missing
            .as_object_mut()
            .unwrap()
            .remove("worker_state_schema");
        missing["next_index"] = json!(1);
        assert!(State::read(&serde_json::to_vec(&missing).unwrap()).is_err());
        fs::remove_dir_all(runtime.root).unwrap();
    }
}

#[cfg(test)]
mod revisit_tests {
    use super::*;
    #[test]
    fn a_cleaned_runner_can_prepare_only_a_newer_incarnation() {
        let mut runtime = super::super::disk_tests::fixture();
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(4096)
            .unwrap();
        let placement = json!({"epoch":7,"node":"alpha","transfer":"initial"});
        runtime.request(&json!({"op":"placement-register","id":"legacy","committed":true,"placement":placement})).unwrap();
        let export = json!({"op":"export","id":"legacy","placement":placement,"transfer":"move","capture":"capture"});
        runtime.request(&export).unwrap();
        let import = json!({"op":"import-prepare","id":"legacy","placement":placement,"snapshot":"capture",
            "manifest_sha256":hash(&runtime.snapshot_dir("capture/manifest.json")).unwrap()});
        assert!(runtime.request(&import).is_err());
        let mut cleanup = export.clone();
        cleanup["op"] = json!("export-cleanup");
        runtime.request(&cleanup).unwrap();
        for epoch in [6, 7] {
            let mut stale = import.clone();
            stale["placement"]["epoch"] = json!(epoch);
            assert!(runtime.request(&stale).is_err());
        }
        let mut next = import;
        next["placement"]["epoch"] = json!(8);
        next["placement"]["transfer"] = json!("return");
        assert_eq!(runtime.request(&next).unwrap()["state"], "prepared");
        assert!(runtime.request(&cleanup).is_err());
        assert!(runtime.check_replay(&json!({"request":export})).is_err());
        assert!(runtime.snapshot_dir("capture/manifest.json").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }
}

#[cfg(test)]
mod managed_lifecycle_tests {
    use super::*;
    fn runtime() -> (Runtime, Value) {
        let mut runtime = super::super::disk_tests::fixture();
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(4096)
            .unwrap();
        let placement = json!({"epoch":7,"node":"alpha","transfer":"move"});
        runtime.request(&json!({"op":"placement-register","id":"legacy","committed":true,"placement":placement})).unwrap();
        (runtime, placement)
    }
    fn named(runtime: &mut Runtime, placement: &Value, name: &str) {
        let _ = fs::remove_file(runtime.machine_dir("legacy").join("fixture-api.sock"));
        let (machine, server) = vm::capture_fixture(&runtime.machine_dir("legacy"), true);
        runtime.machines.insert("legacy".into(), machine);
        let result = runtime
            .request(&json!({"op":"snapshot","id":"legacy","name":name,"placement":placement}))
            .unwrap();
        runtime.machines.clear();
        server.join().unwrap();
        assert_eq!(result["name"], name);
        let manifest: Value = serde_json::from_slice(
            &fs::read(runtime.snapshot_dir(name).join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["managed"], true);
    }
    fn restart(runtime: &mut Runtime) {
        runtime.state = State::read(&fs::read(runtime.root.join("state.json")).unwrap()).unwrap();
        Runtime::recover_state(&mut runtime.state).unwrap();
    }

    #[test]
    fn managed_delete_needs_exact_authority_and_leaves_a_replayable_tombstone() {
        let (mut runtime, placement) = runtime();
        named(&mut runtime, &placement, "kept-by-name");
        named(&mut runtime, &placement, "probe");
        // Kept aside: a valid capture of this box, to try to bring it back after the delete.
        fs::rename(
            runtime.snapshot_dir("probe"),
            runtime.root.join("probe-aside"),
        )
        .unwrap();
        let delete = json!({"op":"delete","id":"legacy","placement":placement});
        for wrong in [
            json!({"op":"delete","id":"legacy"}),
            json!({"op":"delete","id":"legacy","placement":{"epoch":6,"node":"alpha","transfer":"move"}}),
            json!({"op":"delete","id":"legacy","placement":{"epoch":7,"node":"beta","transfer":"move"}}),
        ] {
            assert!(runtime.request(&wrong).is_err(), "{wrong}");
        }
        assert!(runtime.machine_dir("legacy").exists());
        let first = runtime.request(&delete).unwrap();
        assert_eq!(first["deleted"], "legacy");
        assert_eq!(first["snapshots"], json!(["kept-by-name"]));
        assert!(!runtime.machine_dir("legacy").exists());
        assert!(!runtime.snapshot_dir("kept-by-name").exists());
        assert!(runtime.deleted("legacy"));
        assert_eq!(runtime.workspace_count(), 0);
        // A lost reply: the repeat and the agent's cached-reply check both succeed.
        assert_eq!(
            runtime.request(&delete).unwrap(),
            json!({"deleted":"legacy","snapshots":[]})
        );
        runtime
            .request(&json!({"op":"placement-check","id":"legacy","request":delete}))
            .unwrap();
        restart(&mut runtime);
        assert_eq!(runtime.state.workspaces["legacy"].state, "deleted");
        assert!(runtime.deleted("legacy"));
        for op in ["start", "exec", "resize", "snapshot", "ssh-keys"] {
            let mut late = delete.clone();
            late["op"] = json!(op);
            assert!(runtime.request(&late).is_err(), "{op}");
        }
        // The tombstone is permanent: a valid capture of this box cannot bring it back, even
        // at a later epoch, and no export of it can be cleaned again.
        fs::rename(
            runtime.root.join("probe-aside"),
            runtime.snapshot_dir("probe"),
        )
        .unwrap();
        let import = json!({"op":"import-prepare","id":"legacy","snapshot":"probe",
            "manifest_sha256":hash(&runtime.snapshot_dir("probe/manifest.json")).unwrap(),
            "placement":{"epoch":9,"node":"alpha","transfer":"back"},"cold":true});
        let error = runtime.request(&import).unwrap_err();
        // (A cleaned export fence would accept this newer epoch; a tombstone never does.)
        assert!(
            format!("{error:#}").contains("different import placement"),
            "{error:#}"
        );
        let cleanup = json!({"op":"export-cleanup","id":"legacy","placement":placement,"transfer":"move","capture":"probe"});
        let error = runtime.request(&cleanup).unwrap_err();
        assert!(
            format!("{error:#}").contains("missing export intent"),
            "{error:#}"
        );
        assert!(!runtime.machine_dir("legacy").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn an_old_fenced_copy_deletes_with_its_own_authority_and_releases_its_captures() {
        let (mut runtime, placement) = runtime();
        let export = json!({"op":"export","id":"legacy","placement":placement,"transfer":"onward","capture":"onward-capture"});
        runtime.crash = Some("capture");
        assert!(runtime.request(&export).is_err());
        // Mid-export the copy cannot be deleted.
        let delete = json!({"op":"delete","id":"legacy","placement":placement});
        let error = runtime.request(&delete).unwrap_err();
        assert!(format!("{error:#}").contains("fenced"), "{error:#}");
        runtime.crash = None;
        runtime.request(&export).unwrap();
        let digest = hash(&runtime.snapshot_dir("onward-capture/manifest.json")).unwrap();
        // A fork child on this runner imports the export capture and keeps it reserved.
        let child = json!({"epoch":8,"node":"alpha","transfer":"fork"});
        runtime.request(&json!({"op":"fork-prepare","id":"child","fork_source":"legacy","placement":child,"snapshot":"onward-capture","manifest_sha256":digest})).unwrap();
        runtime
            .request(&json!({"op":"activate","id":"child","placement":child,"committed":true}))
            .unwrap();
        // An export not yet cleaned may still be a transfer's fallback: clean it first.
        let error = runtime.request(&delete).unwrap_err();
        assert!(format!("{error:#}").contains("fenced"), "{error:#}");
        let mut cleanup = export.clone();
        cleanup["op"] = json!("export-cleanup");
        runtime.request(&cleanup).unwrap();
        // The old source goes; the capture the child imported stays while it is reserved.
        assert_eq!(runtime.request(&delete).unwrap()["snapshots"], json!([]));
        assert!(runtime.deleted("legacy"));
        assert!(
            runtime
                .snapshot_dir("onward-capture/manifest.json")
                .exists()
        );
        // Deleting the child releases that reservation, and with its parent already deleted
        // here nothing else could ever remove the capture: it goes with the child.
        let deleted = runtime
            .request(&json!({"op":"delete","id":"child","placement":child}))
            .unwrap();
        assert_eq!(deleted["snapshots"], json!(["onward-capture"]));
        assert!(runtime.deleted("child"));
        assert!(!runtime.lifecycle_capture("onward-capture"));
        assert!(!runtime.snapshot_dir("onward-capture").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn managed_named_snapshot_delete_works_active_or_fenced_but_never_on_captures() {
        let (mut runtime, placement) = runtime();
        named(&mut runtime, &placement, "first");
        named(&mut runtime, &placement, "second");
        let delete = |name: &str, placement: &Value| json!({"op":"snapshot-delete","id":"legacy","name":name,"placement":placement});
        assert!(
            runtime
                .request(&json!({"op":"snapshot-delete","id":"legacy","name":"first"}))
                .is_err()
        );
        assert!(
            runtime
                .request(&delete(
                    "first",
                    &json!({"epoch":6,"node":"alpha","transfer":"move"})
                ))
                .is_err()
        );
        assert_eq!(
            runtime.request(&delete("first", &placement)).unwrap()["existed"],
            true
        );
        assert!(!runtime.snapshot_dir("first").exists());
        assert_eq!(
            runtime.request(&delete("first", &placement)).unwrap()["existed"],
            false
        );
        // Moved away: the fenced, cleaned source still removes its named snapshot copy.
        let export = json!({"op":"export","id":"legacy","placement":placement,"transfer":"onward","capture":"onward-capture"});
        runtime.request(&export).unwrap();
        assert!(
            runtime
                .request(&delete("onward-capture", &placement))
                .is_err()
        );
        let mut cleanup = export.clone();
        cleanup["op"] = json!("export-cleanup");
        runtime.request(&cleanup).unwrap();
        assert!(
            runtime
                .request(&delete("onward-capture", &placement))
                .is_err()
        );
        assert!(
            runtime
                .request(&json!({"op":"start","id":"legacy","placement":placement}))
                .is_err()
        );
        assert_eq!(
            runtime.request(&delete("second", &placement)).unwrap()["existed"],
            true
        );
        runtime
            .request(&json!({"op":"placement-check","id":"legacy","request":delete("second", &placement)}))
            .unwrap();
        assert!(
            runtime
                .snapshot_dir("onward-capture/manifest.json")
                .exists()
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }

    /// Exports `legacy` on a source runner and imports that capture as fork child `child` on
    /// a separate target runner, where the parent has never been.
    fn cross_runner_child() -> (Runtime, Value) {
        let (mut source, placement) = runtime();
        source
            .request(&json!({"op":"export","id":"legacy","placement":placement,"transfer":"cross","capture":"fork-import"}))
            .unwrap();
        let mut target = super::super::disk_tests::fixture();
        target
            .request(&json!({"op":"delete","id":"legacy"}))
            .unwrap();
        let to = target.snapshot_dir("fork-import");
        fs::create_dir(&to).unwrap();
        for entry in fs::read_dir(source.snapshot_dir("fork-import")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
        let child = json!({"epoch":8,"node":"beta","transfer":"cross"});
        let digest = hash(&to.join("manifest.json")).unwrap();
        target.request(&json!({"op":"fork-prepare","id":"child","fork_source":"legacy","snapshot":"fork-import","manifest_sha256":digest,"placement":child})).unwrap();
        target
            .request(&json!({"op":"activate","id":"child","placement":child,"committed":true}))
            .unwrap();
        fs::remove_dir_all(source.root).unwrap();
        (target, child)
    }

    #[test]
    fn a_cross_runner_fork_child_takes_its_imported_capture_with_it() {
        let (mut target, child) = cross_runner_child();
        let delete = json!({"op":"delete","id":"child","placement":child});
        assert_eq!(
            target.request(&delete).unwrap()["snapshots"],
            json!(["fork-import"])
        );
        assert!(target.deleted("child"));
        assert!(!target.snapshot_dir("fork-import").exists());
        assert_eq!(
            target.request(&json!({"op":"snapshots"})).unwrap(),
            json!([])
        );
        // A repeat answers from the tombstone.
        assert_eq!(target.request(&delete).unwrap()["deleted"], "child");
        fs::remove_dir_all(target.root).unwrap();
    }

    #[test]
    fn a_fork_childs_capture_stays_while_its_parent_lives_on_that_runner_and_goes_with_it() {
        // Same runner: the child imports the parent's own capture, which the parent still owns.
        let (mut runtime, placement) = runtime();
        named(&mut runtime, &placement, "parents-own");
        let child = json!({"epoch":8,"node":"alpha","transfer":"fork"});
        let digest = hash(&runtime.snapshot_dir("parents-own/manifest.json")).unwrap();
        runtime.request(&json!({"op":"fork-prepare","id":"child","fork_source":"legacy","snapshot":"parents-own","manifest_sha256":digest,"placement":child})).unwrap();
        runtime
            .request(&json!({"op":"activate","id":"child","placement":child,"committed":true}))
            .unwrap();
        assert_eq!(
            runtime
                .request(&json!({"op":"delete","id":"child","placement":child}))
                .unwrap()["snapshots"],
            json!([])
        );
        assert!(runtime.snapshot_dir("parents-own/manifest.json").exists());
        // The parent's own delete removes it.
        let removed = runtime
            .request(&json!({"op":"delete","id":"legacy","placement":placement}))
            .unwrap();
        assert_eq!(removed["snapshots"], json!(["parents-own"]));
        assert!(!runtime.snapshot_dir("parents-own").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn a_capture_sealing_when_its_box_is_deleted_is_never_committed() {
        let (mut runtime, placement) = runtime();
        let _ = fs::remove_file(runtime.machine_dir("legacy").join("fixture-api.sock"));
        let (machine, server) = vm::capture_fixture(&runtime.machine_dir("legacy"), true);
        runtime.machines.insert("legacy".into(), machine);
        let super::super::Step::Seal(staged) = runtime
            .begin(&json!({"op":"snapshot","id":"legacy","name":"late","placement":placement}))
        else {
            panic!("capture not deferred")
        };
        server.join().unwrap();
        runtime
            .request(&json!({"op":"stop","id":"legacy","placement":placement}))
            .unwrap();
        runtime
            .request(&json!({"op":"delete","id":"legacy","placement":placement}))
            .unwrap();
        let sealed = staged.seal();
        let error = runtime.finish(staged, sealed).unwrap_err();
        assert!(
            format!("{error:#}").contains("deleted during the capture"),
            "{error:#}"
        );
        assert!(!runtime.snapshot_dir("late").exists());
        fs::remove_dir_all(runtime.root).unwrap();
    }
}
