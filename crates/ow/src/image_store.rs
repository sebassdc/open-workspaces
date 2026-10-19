//! Agent-side image transfers and build jobs, outside the serialized worker.
use crate::{
    common,
    image_catalog::{self, Image},
    wire,
};
use anyhow::{Context, Result, ensure};
use ow_core::store::{self, Objects, Reference, Source};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn local_name(image: &Image) -> String {
    format!("{}@{}", image.name, image.version)
}
fn check(image: &Image) -> Result<()> {
    common::identifier(&image.name)?;
    ensure!(image.version > 0, "invalid image version");
    store::attempt_prefix(&image.reference.attempt)?;
    store::valid_digest(&image.base_digest)?;
    store::valid_digest(&image.recipe_sha256)?;
    Ok(())
}
pub fn install(objects: &dyn Objects, image: &Image, root: &Path) -> Result<Value> {
    check(image)?;
    let manifest = store::read_manifest(objects, &image.reference)?;
    ensure!(
        matches!(manifest.kind.as_str(), "base" | "image")
            && manifest.metadata["name"] == image.name
            && manifest.metadata["version"] == image.version,
        "image identity mismatch"
    );
    let images = root.join("images");
    fs::create_dir_all(&images)?;
    let name = local_name(image);
    let target = images.join(&name);
    let local = json!({"name":name,"base":manifest.metadata["base"],"recipe_sha256":manifest.metadata["recipe_sha256"],"disk_sha256":manifest.files["disk.ext4"].sha256,"store":image,"image_digest":image.reference.digest});
    if target.exists() {
        let previous: Value = serde_json::from_slice(&fs::read(target.join("manifest.json"))?)?;
        ensure!(
            previous["image_digest"] == image.reference.digest
                && store::hash_file(&target.join("disk.ext4"))?
                    == manifest.files["disk.ext4"].sha256,
            "installed image version conflicts or is corrupt"
        );
        return Ok(previous);
    }
    let count = || -> Result<usize> {
        let mut count = 0;
        for entry in fs::read_dir(&images)? {
            if !entry?.file_name().to_string_lossy().starts_with('.') {
                count += 1;
            }
        }
        Ok(count)
    };
    ensure!(count()? < 32, "golden image limit (32) reached");
    let stage = images.join(format!(".pull-{}", store::token()?));
    store::pull(objects, &image.reference, &stage)?;
    let result = (|| {
        fs::remove_file(stage.join("manifest.json"))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage.join("manifest.json"))?;
        use std::io::Write;
        output.write_all(&serde_json::to_vec(&local)?)?;
        output.sync_all()?;
        File::open(&stage)?.sync_all()?;
        // Serialize installs, including concurrent creates, without replacing any version.
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(images.join(".install.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "image install lock"
        );
        if target.exists() {
            let previous: Value = serde_json::from_slice(&fs::read(target.join("manifest.json"))?)?;
            ensure!(
                previous["image_digest"] == image.reference.digest,
                "concurrent image conflict"
            );
            fs::remove_dir_all(&stage)?;
        } else {
            ensure!(count()? < 32, "golden image limit (32) reached");
            fs::rename(&stage, &target)?;
            File::open(&images)?.sync_all()?;
        }
        Ok(local)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}
/// Only an operator or the authenticated controller calls this job; guests cannot.
pub fn job(root: &Path, request: &Value) -> Result<Value> {
    let image: Image = serde_json::from_value(request["image"].clone())?;
    check(&image)?;
    let configured = configured_url(root)
        .context("runner has no configured store-url; enable the store explicitly")?;
    ensure!(
        configured == image.store_url,
        "controller store differs from runner configuration"
    );
    let objects = store::S3Objects::new(&image.store_url, None)?;
    if request["op"] == "image-pull" {
        return install(&objects, &image, root);
    }
    objects.verify_configuration()?;
    ensure!(
        request["op"] == "image-build" && image.expires > now(),
        "invalid or expired image build"
    );
    let recipe = request["recipe"].as_str().context("recipe required")?;
    ensure!(
        recipe.len() <= 192 * 1024 && store::sha256(recipe.as_bytes()) == image.recipe_sha256,
        "recipe hash mismatch"
    );
    let base: Image = serde_json::from_value(request["base_image"].clone())?;
    ensure!(
        base.reference.digest == image.base_digest
            && base.name == image.base
            && base.store_url == image.store_url,
        "base image mismatch"
    );
    install(&objects, &base, root)?;
    let jobs = root.join("image-jobs");
    if !jobs.exists() {
        fs::DirBuilder::new().mode(0o700).create(&jobs)?;
    }
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(jobs.join(format!("{}.lock", image.reference.attempt)))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "image build lock"
    );
    let name = format!("b-{}", &image.reference.attempt[..24]);
    let worker_image = root.join("images").join(&name);
    let sealed = jobs.join(format!("{}.sealed", image.reference.attempt));
    // Recover a crash between worker sealing and moving its private build result.
    if !sealed.exists() && worker_image.join("manifest.json").is_file() {
        fs::rename(&worker_image, &sealed)?;
        File::open(&jobs)?.sync_all()?;
    }
    let marker = jobs.join(&image.reference.attempt);
    let local = if sealed.join("manifest.json").is_file() {
        let local: Value = serde_json::from_slice(&fs::read(sealed.join("manifest.json"))?)?;
        ensure!(
            local["recipe_sha256"] == image.recipe_sha256 && local["base"] == local_name(&base),
            "sealed build conflict"
        );
        local
    } else {
        ensure!(
            !marker.exists(),
            "interrupted image recipe; inspect the builder before starting a fresh version"
        );
        let mut started = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)?;
        use std::io::Write;
        started.write_all(&serde_json::to_vec(&image)?)?;
        started.sync_all()?;
        File::open(&jobs)?.sync_all()?;
        let memory = request["memory_mib"].as_u64().context("memory required")?;
        let cpus = request["vcpu_count"].as_u64().context("CPUs required")?;
        common::resources(u32::try_from(memory)?, u32::try_from(cpus)?)?;
        let seconds = request["timeout_seconds"]
            .as_u64()
            .context("timeout required")?;
        ensure!(
            (1..=3600).contains(&seconds),
            "controller image builds allow at most one hour"
        );
        let local = crate::image::build_text(
            root,
            &name,
            &local_name(&base),
            recipe,
            memory as u32,
            cpus as u32,
            Duration::from_secs(seconds),
            Some(&format!("img-{}", &image.reference.attempt[..16])),
        )?;
        fs::rename(&worker_image, &sealed)?;
        File::open(&jobs)?.sync_all()?;
        local
    };
    let reference = publish(&objects, &image, &sealed.join("disk.ext4"), local)?;
    Ok(json!({"reference":reference}))
}
pub fn publish(
    objects: &dyn Objects,
    image: &Image,
    disk: &Path,
    local: Value,
) -> Result<Reference> {
    let digest = local["disk_sha256"]
        .as_str()
        .context("sealed disk digest required")?;
    store::push(
        objects,
        &image.reference.attempt,
        if ["alpine", "arch", "ubuntu"].contains(&image.name.as_str()) {
            "base"
        } else {
            "image"
        },
        image_catalog::metadata(image, local.clone()),
        &BTreeMap::from([(
            "disk.ext4".into(),
            Source {
                path: disk.into(),
                sha256: digest.into(),
            },
        )]),
        // Stable across retries of an attempt, so its manifest is also write-once.
        u64::try_from(image.expires - 7200)?,
    )
}
/// The catalog/leases belong to the controller; local job state belongs to its worker.
pub fn dispatch(
    controller: &Path,
    local_runner: &Path,
    node: &str,
    request: Value,
) -> Result<Value> {
    let lease = if request["op"] == "image-pull" {
        let image: Image = serde_json::from_value(request["image"].clone())?;
        let db = image_catalog::open(controller)?;
        ensure!(db.execute("INSERT INTO leases(artifact,holder,purpose,expires) SELECT id,?2,'pull',?3 FROM artifacts WHERE id=?1 AND state='stored'",rusqlite::params![image.artifact,node,now()+7200])?==1,"image is not stored");
        Some((db.last_insert_rowid(), db))
    } else {
        None
    };
    let result = (|| {
        if node == "local" {
            return job(local_runner, &request);
        }
        let root = crate::nodes::route(controller, node)?;
        let value = wire::connect_waiting(&root, &request, Duration::from_secs(7200))?.1;
        if request["op"] == "image-pull" {
            let image: Image = serde_json::from_value(request["image"].clone())?;
            ensure!(
                value["name"] == local_name(&image)
                    && value["image_digest"] == image.reference.digest,
                "runner did not confirm the pinned image digest"
            );
        }
        Ok(value)
    })();
    // An uncertain remote pull may still be running: retain its lease until expiry.
    if result.is_ok()
        && let Some((id, db)) = lease
    {
        db.execute("DELETE FROM leases WHERE id=?1", [id])?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_base_pull_changes_only_the_template_and_checks_kernel() {
        let root = std::env::temp_dir().join(format!("ow-base-pull-{}", store::token().unwrap()));
        fs::create_dir(&root).unwrap();
        let assets = root.join("assets");
        fs::create_dir_all(assets.join("downloads")).unwrap();
        fs::create_dir_all(assets.join("guest")).unwrap();
        fs::write(assets.join("downloads/vmlinux-6.1.186"), b"kernel").unwrap();
        fs::write(assets.join("guest/ubuntu.ext4"), b"old template").unwrap();
        let box_disk = root.join("existing-box.ext4");
        fs::copy(assets.join("guest/ubuntu.ext4"), &box_disk).unwrap();
        let disk = root.join("sealed.ext4");
        fs::write(&disk, b"new template").unwrap();
        let objects = store::DirectoryObjects(root.join("objects"));
        let mut image = Image {
            name: "ubuntu".into(),
            version: 1,
            artifact: "a-base".into(),
            status: "building".into(),
            store_url: "s3://bucket/root".into(),
            reference: Reference {
                attempt: store::token().unwrap(),
                digest: String::new(),
            },
            base: "ubuntu".into(),
            base_digest: "a".repeat(64),
            recipe_sha256: "b".repeat(64),
            node: "node".into(),
            expires: 7201,
            stale: false,
        };
        image.reference=publish(&objects,&image,&disk,json!({"disk_sha256":store::hash_file(&disk).unwrap(),"kernel_sha256":store::sha256(b"kernel"),"profile":{"developer_user":true}})).unwrap();
        let runner = root.join("runner");
        fs::create_dir(&runner).unwrap();
        install(&objects, &image, &runner).unwrap();
        install_base(&objects, &image, &runner, &assets).unwrap();
        assert_eq!(
            fs::read(assets.join("guest/ubuntu.ext4")).unwrap(),
            b"new template"
        );
        assert_eq!(fs::read(&box_disk).unwrap(), b"old template");
        let profile: Value =
            serde_json::from_slice(&fs::read(assets.join("guest/ubuntu.json")).unwrap()).unwrap();
        assert_eq!(profile["image_digest"], image.reference.digest);
        fs::write(assets.join("downloads/vmlinux-6.1.186"), b"other").unwrap();
        assert!(install_base(&objects, &image, &runner, &assets).is_err());
        assert_eq!(
            fs::read(assets.join("guest/ubuntu.ext4")).unwrap(),
            b"new template"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn image_distribution_installs_pinned_bytes_and_preserves_other_versions() {
        let root = std::env::temp_dir().join(format!("ow-image-store-{}", store::token().unwrap()));
        fs::create_dir(&root).unwrap();
        let objects = store::DirectoryObjects(root.join("objects"));
        let disk = root.join("disk");
        fs::write(&disk, b"sealed disk").unwrap();
        let mut image = Image {
            name: "tools".into(),
            version: 1,
            artifact: "a-test".into(),
            status: "building".into(),
            store_url: "s3://bucket/root".into(),
            reference: Reference {
                attempt: store::token().unwrap(),
                digest: String::new(),
            },
            base: "ubuntu".into(),
            base_digest: "a".repeat(64),
            recipe_sha256: "b".repeat(64),
            node: "node".into(),
            expires: 7201,
            stale: false,
        };
        image.reference = publish(
            &objects,
            &image,
            &disk,
            json!({"disk_sha256":store::hash_file(&disk).unwrap()}),
        )
        .unwrap();
        let runner = root.join("runner");
        fs::create_dir(&runner).unwrap();
        install(&objects, &image, &runner).unwrap();
        install(&objects, &image, &runner).unwrap();
        assert_eq!(
            fs::read(runner.join("images/tools@1/disk.ext4")).unwrap(),
            b"sealed disk"
        );
        let mut wrong = image.clone();
        wrong.reference.digest = "f".repeat(64);
        assert!(install(&objects, &wrong, &runner).is_err());
        image.version = 2;
        image.reference.attempt = store::token().unwrap();
        image.reference = publish(
            &objects,
            &image,
            &disk,
            json!({"disk_sha256":store::hash_file(&disk).unwrap()}),
        )
        .unwrap();
        install(&objects, &image, &runner).unwrap();
        assert!(runner.join("images/tools@1").exists() && runner.join("images/tools@2").exists());
        assert!(fs::read_dir(runner.join("images")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pull-")
        }));
        fs::remove_dir_all(root).unwrap();
    }
}

pub fn from_reference(url: &str, selector: &str, reference: Reference) -> Result<Image> {
    let (name, version) = store::image_name(selector)?;
    let version = version.context("standalone pulls need NAME@N")?;
    let objects = store::S3Objects::new(url, None)?;
    let manifest = store::read_manifest(&objects, &reference)?;
    ensure!(
        manifest.metadata["name"] == name && manifest.metadata["version"] == version,
        "image selector differs from pinned manifest"
    );
    Ok(Image {
        name: name.into(),
        version,
        artifact: String::new(),
        status: "stored".into(),
        store_url: url.into(),
        reference,
        base: manifest.metadata["base"]
            .as_str()
            .context("image base")?
            .into(),
        base_digest: manifest.metadata["base_digest"]
            .as_str()
            .context("base digest")?
            .into(),
        recipe_sha256: manifest.metadata["recipe_sha256"]
            .as_str()
            .context("recipe digest")?
            .into(),
        node: String::new(),
        expires: 0,
        stale: false,
    })
}
/// New creates see the new template; existing boxes already own independent disks.
pub fn install_base(
    objects: &dyn Objects,
    image: &Image,
    runner: &Path,
    assets: &Path,
) -> Result<()> {
    ensure!(
        ["ubuntu", "arch", "alpine"].contains(&image.name.as_str()),
        "only built-in base profiles can replace assets"
    );
    let manifest = store::read_manifest(objects, &image.reference)?;
    ensure!(manifest.kind == "base", "base artifact required");
    let kernel = store::hash_file(&assets.join("downloads/vmlinux-6.1.186"))?;
    ensure!(
        manifest.metadata["local"]["kernel_sha256"] == kernel,
        "base image kernel differs from runtime assets"
    );
    let profile = &manifest.metadata["local"]["profile"];
    ensure!(profile.is_object(), "base image profile metadata missing");
    let guest = assets.join("guest");
    ensure!(guest.is_dir(), "runtime guest assets missing");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(guest.join(".store.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "base image lock"
    );
    let name = if image.name == "alpine" {
        "base"
    } else {
        &image.name
    };
    let stage = guest.join(format!(".store-{}", store::token()?));
    let result = (|| {
        let disk = runner
            .join("images")
            .join(local_name(image))
            .join("disk.ext4");
        if crate::vm::reflink(&disk, &stage).is_err() {
            let _ = fs::remove_file(&stage);
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&stage)?;
            ensure!(
                ow_core::snapshot_store::write_sparse(File::open(&disk)?, &mut output)?
                    == manifest.files["disk.ext4"].sha256,
                "base template copy digest mismatch"
            );
        }
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o400))?;
        File::open(&stage)?.sync_all()?;
        fs::rename(&stage, guest.join(format!("{name}.ext4")))?;
        // Profile metadata is informational; worker create consumes the complete disk above.
        let meta = guest.join(format!(".profile-{}", store::token()?));
        let mut value = profile.clone();
        value["image_digest"] = json!(image.reference.digest);
        use std::io::Write;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&meta)?;
        file.write_all(&serde_json::to_vec(&value)?)?;
        file.sync_all()?;
        fs::rename(meta, guest.join(format!("{}.json", image.name)))?;
        File::open(&guest)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

pub fn configured_url(root: &Path) -> Result<String> {
    if let Ok(url) = fs::read_to_string(root.join("store-url")) {
        return Ok(url.trim().into());
    }
    let value: Value = serde_json::from_slice(&fs::read("/etc/open-workspaces/store.json")?)?;
    Ok(value["url"].as_str().context("store URL missing")?.into())
}

/// A joined capable runner prefetches current images; at most two pulls run together.
pub fn prefetch(controller: &Path, node: &str, advertised: &Value) -> Result<()> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(
            controller
                .join("nodes")
                .join(node)
                .join("image-prefetch.lock"),
        )?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    let db = image_catalog::open(controller)?;
    let images: Vec<_> = image_catalog::list(&db)?
        .into_iter()
        .filter(|i| {
            i.status == "current"
                && !advertised
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v["digest"] == i.reference.digest))
        })
        .collect();
    for batch in images.chunks(2) {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = batch
                .iter()
                .map(|image| {
                    scope.spawn(move || {
                        dispatch(
                            controller,
                            controller,
                            node,
                            json!({"op":"image-pull","image":image}),
                        )
                    })
                })
                .collect();
            for (image, job) in batch.iter().zip(jobs) {
                match job.join() {
                    Ok(Ok(_)) => {
                        db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES(?1,?2,'present',?3) ON CONFLICT(artifact,node) DO UPDATE SET state='present',last_used=excluded.last_used",rusqlite::params![image.artifact,node,now()])?;
                    }
                    _ => eprintln!("image prefetch failed; on-demand create can retry"),
                }
            }
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(())
}
