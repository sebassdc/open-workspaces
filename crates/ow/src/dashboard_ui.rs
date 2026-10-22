//! Dashboard UI bundles (docs/DASHBOARD.md#ui-bundles). The controller serves the browser
//! UI from a separately versioned bundle chosen by the `dashboard.ui_bundle` setting, or
//! from the copy built into this binary. A bundle replaces only the fixed files below;
//! routes, content types, CSP and authentication stay with the server.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The dashboard API version this server speaks; bundles declare the range they support.
pub const API_VERSION: u64 = 1;
/// Advertised by `GET /api/version` so deployments can check for this feature.
pub const CAPABILITY: &str = "dashboard_ui_bundle_v1";
pub const SETTING: &str = "dashboard.ui_bundle";
const NAME: &str = "ow-dashboard-ui";
const FILE_LIMIT: u64 = 8 * 1024 * 1024;
const TARBALL_LIMIT: u64 = 32 * 1024 * 1024;

/// URL path, bundle file, content type and built-in copy of every UI file.
pub const ASSETS: [(&str, &str, &str, &str); 6] = [
    (
        "/",
        "index.html",
        "text/html; charset=utf-8",
        include_str!("../ui/index.html"),
    ),
    (
        "/app.js",
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/app.js"),
    ),
    (
        "/style.css",
        "style.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style.css"),
    ),
    (
        "/vendor/xterm.js",
        "vendor/xterm.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/vendor/xterm.js"),
    ),
    (
        "/vendor/addon-fit.js",
        "vendor/addon-fit.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/vendor/addon-fit.js"),
    ),
    (
        "/vendor/xterm.css",
        "vendor/xterm.css",
        "text/css; charset=utf-8",
        include_str!("../ui/vendor/xterm.css"),
    ),
];
const BUILTIN: &str = include_str!("../ui/ui.json");

/// The UI the dashboard serves right now.
pub struct Ui {
    /// Bundle bytes in `ASSETS` order; `None` serves the built-in copy.
    files: Option<Arc<Vec<Vec<u8>>>>,
    pub info: Value,
}

impl Ui {
    pub fn asset(&self, path: &str) -> Option<(&'static str, Vec<u8>)> {
        let index = ASSETS.iter().position(|(p, ..)| *p == path)?;
        let body = match &self.files {
            Some(files) => files[index].clone(),
            None => ASSETS[index].3.as_bytes().to_vec(),
        };
        Some((ASSETS[index].2, body))
    }
}

fn builtin(fallback_reason: Option<String>) -> Ui {
    let ui: Value = serde_json::from_str(BUILTIN).expect("built-in ui.json");
    Ui {
        files: None,
        info: json!({"source":"builtin","version":ui["version"],"api":ui["api"],"fallback_reason":fallback_reason}),
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Bundle versions are path segments: letters, digits, `.`, `_` and `-`.
pub fn valid_version(version: &str) -> Result<()> {
    ensure!(
        (1..=64).contains(&version.len())
            && !version.starts_with('.')
            && version != "builtin"
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "a UI bundle version is 1-64 letters, digits, '.', '_' or '-' and does not start with '.'"
    );
    Ok(())
}

fn bundles(root: &Path) -> PathBuf {
    root.join("dashboard-ui")
}

/// A regular file (not a link) within the size limit.
fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("{} missing", path.display()))?;
    ensure!(
        meta.is_file() && meta.len() <= limit,
        "{} is not a regular file within {limit} bytes",
        path.display()
    );
    Ok(fs::read(path)?)
}

/// Reads a bundle directory and checks its manifest and every file's hash.
fn load(dir: &Path) -> Result<(Value, Vec<Vec<u8>>)> {
    let manifest: Value =
        serde_json::from_slice(&read_file(&dir.join("manifest.json"), 64 * 1024)?)
            .context("manifest.json is not JSON")?;
    ensure!(
        manifest["format"] == 1 && manifest["name"] == NAME,
        "manifest is not a format 1 {NAME} manifest"
    );
    valid_version(manifest["version"].as_str().unwrap_or_default())?;
    let (min, max) = (
        manifest["api"]["min"].as_u64(),
        manifest["api"]["max"].as_u64(),
    );
    ensure!(
        min.zip(max).is_some_and(|(min, max)| min <= max),
        "manifest api range is missing or empty"
    );
    let listed = manifest["files"]
        .as_object()
        .context("manifest lists no files")?;
    ensure!(
        listed.len() == ASSETS.len() && ASSETS.iter().all(|(_, f, ..)| listed.contains_key(*f)),
        "manifest must list exactly: {}",
        ASSETS.map(|(_, f, ..)| f).join(", ")
    );
    let mut files = Vec::new();
    for (_, file, ..) in ASSETS {
        let bytes = read_file(&dir.join(file), FILE_LIMIT)?;
        ensure!(
            listed[file].as_str() == Some(sha256(&bytes).as_str()),
            "{file} does not match its manifest hash"
        );
        files.push(bytes);
    }
    Ok((manifest, files))
}

/// Why an installed bundle cannot be served by this server, if it cannot.
fn check(root: &Path, version: &str) -> Result<(Value, Vec<Vec<u8>>)> {
    valid_version(version)?;
    let dir = bundles(root).join(version);
    let (manifest, files) = load(&dir).with_context(|| format!("UI bundle {version}"))?;
    ensure!(
        manifest["version"] == version,
        "UI bundle {version}: manifest names version {}",
        manifest["version"]
    );
    let (min, max) = (
        manifest["api"]["min"].as_u64().unwrap(),
        manifest["api"]["max"].as_u64().unwrap(),
    );
    ensure!(
        (min..=max).contains(&API_VERSION),
        "UI bundle {version} supports dashboard API {min}-{max}; this server speaks {API_VERSION}"
    );
    Ok((manifest, files))
}

/// `ow config set dashboard.ui_bundle`: refuse a bundle this server would not serve.
pub fn selectable(root: &Path, value: &str) -> Result<()> {
    if value != "builtin" {
        check(root, value)?;
    }
    Ok(())
}

/// Inode, size, mtime and ctime of every bundle file: a write to any of them changes it.
type Stamp = Vec<Option<(u64, u64, i64, i64, i64, i64)>>;

fn stamp(dir: &Path) -> Stamp {
    std::iter::once("manifest.json")
        .chain(ASSETS.map(|(_, f, ..)| f))
        .map(|f| {
            let m = fs::symlink_metadata(dir.join(f)).ok()?;
            Some((
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            ))
        })
        .collect()
}

type Verified = (PathBuf, Stamp, Arc<Vec<Vec<u8>>>, Value);

/// The UI to serve for this request. Any problem with the selected bundle serves the
/// built-in copy and says why, so the dashboard is never empty. A verified bundle is
/// hashed again only when one of its files changes.
pub fn selected(root: Option<&Path>) -> Ui {
    static VERIFIED: Mutex<Option<Verified>> = Mutex::new(None);
    let Some(root) = root else {
        return builtin(None);
    };
    let setting = rusqlite::Connection::open_with_flags(
        root.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|db| {
        db.busy_timeout(std::time::Duration::from_secs(2))
            .map(|_| db)
    })
    .map_err(anyhow::Error::from)
    .and_then(|db| crate::telemetry::settings(&db));
    let version = match setting {
        Ok(settings) => settings[SETTING].as_str().unwrap_or("builtin").to_owned(),
        Err(error) => return fallback(format!("controller settings unreadable: {error:#}")),
    };
    if version == "builtin" {
        return builtin(None);
    }
    let dir = bundles(root).join(&version);
    let stamp = stamp(&dir);
    let mut verified = VERIFIED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached, cached_stamp, files, info)) = verified.as_ref()
        && *cached == dir
        && *cached_stamp == stamp
    {
        return Ui {
            files: Some(files.clone()),
            info: info.clone(),
        };
    }
    match check(root, &version) {
        Ok((manifest, files)) => {
            let files = Arc::new(files);
            let info = json!({"source":"bundle","version":version,"api":manifest["api"],"fallback_reason":null});
            *verified = Some((dir, stamp, files.clone(), info.clone()));
            Ui {
                files: Some(files),
                info,
            }
        }
        Err(error) => {
            *verified = None;
            fallback(format!("{error:#}"))
        }
    }
}

fn fallback(reason: String) -> Ui {
    static LAST: Mutex<String> = Mutex::new(String::new());
    if let Ok(mut last) = LAST.lock()
        && *last != reason
    {
        eprintln!("dashboard: serving the built-in UI: {reason}");
        last.clone_from(&reason);
    }
    builtin(Some(reason))
}

/// `ow dashboard-ui install <tarball>`: extract only the known members, verify them and
/// keep them as `<data>/dashboard-ui/<version>/`. Selecting it is a separate setting.
pub fn install(root: &Path, tarball: &Path) -> Result<Value> {
    ensure!(
        fs::metadata(tarball)?.len() <= TARBALL_LIMIT,
        "UI bundle tarball larger than {TARBALL_LIMIT} bytes"
    );
    let parent = bundles(root);
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let staging = parent.join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir(&staging)?;
    let result = (|| {
        let mut members = vec!["manifest.json"];
        members.extend(ASSETS.map(|(_, f, ..)| f));
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(tarball)
            .arg("-C")
            .arg(&staging)
            .args(&members)
            .status()
            .context("running tar")?;
        ensure!(
            status.success(),
            "tar could not extract the UI bundle members"
        );
        let (manifest, files) = load(&staging)?;
        let version = manifest["version"].as_str().unwrap().to_owned();
        let target = parent.join(&version);
        if target.exists() {
            let (existing, _) = load(&target).with_context(|| {
                format!("installed UI bundle {version} is damaged; remove it first")
            })?;
            ensure!(
                existing == manifest,
                "a different UI bundle {version} is already installed"
            );
            return Ok(manifest);
        }
        // Write fresh regular files: nothing tar created (links, modes) survives.
        let fresh = parent.join(format!(".new-{version}"));
        let _ = fs::remove_dir_all(&fresh);
        fs::create_dir_all(fresh.join("vendor"))?;
        fs::write(
            fresh.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        for ((_, file, ..), bytes) in ASSETS.iter().zip(&files) {
            fs::write(fresh.join(file), bytes)?;
        }
        fs::rename(&fresh, &target)?;
        Ok(manifest)
    })();
    let _ = fs::remove_dir_all(&staging);
    let manifest = result?;
    Ok(json!({"installed":manifest["version"],"api":manifest["api"],"api_version":API_VERSION}))
}

/// `ow dashboard-ui list`: installed bundles and whether this server can serve each.
pub fn list(root: &Path) -> Result<Value> {
    let mut out = vec![];
    if let Ok(entries) = fs::read_dir(bundles(root)) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        for name in names {
            out.push(match check(root, &name) {
                Ok((manifest, _)) => json!({"version":name,"api":manifest["api"],"usable":true}),
                Err(error) => json!({"version":name,"usable":false,"error":format!("{error:#}")}),
            });
        }
    }
    let selected = crate::telemetry::settings(&crate::nodes::db(root)?)?[SETTING].clone();
    Ok(
        json!({"selected":selected,"api_version":API_VERSION,"builtin":builtin(None).info["version"],"bundles":out}),
    )
}

pub fn command(root: &Path, action: crate::DashboardUiAction) -> Result<i32> {
    let value = match action {
        crate::DashboardUiAction::Install { tarball } => install(root, &tarball)?,
        crate::DashboardUiAction::List => list(root)?,
    };
    println!("{value:#}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ow-ui-{}", crate::common::nonce().unwrap()));
        fs::create_dir_all(&root).unwrap();
        crate::nodes::db(&root).unwrap();
        root
    }

    /// A bundle directory like scripts/build-dashboard-ui.py makes, with `marker` in index.html.
    fn bundle(dir: &Path, version: &str, marker: &str, api: (u64, u64)) {
        fs::create_dir_all(dir.join("vendor")).unwrap();
        let mut files = serde_json::Map::new();
        for (_, file, _, builtin) in ASSETS {
            let body = if file == "index.html" {
                builtin.replace("<title>", &format!("<title>{marker}"))
            } else {
                builtin.to_owned()
            };
            fs::write(dir.join(file), &body).unwrap();
            files.insert(file.into(), json!(sha256(body.as_bytes())));
        }
        let manifest = json!({"format":1,"name":NAME,"version":version,"api":{"min":api.0,"max":api.1},"files":files});
        fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    fn tarball(root: &Path, version: &str, api: (u64, u64)) -> PathBuf {
        let src = root.join(format!("src-{version}"));
        bundle(&src, version, version, api);
        let out = root.join(format!("{version}.tar.gz"));
        let mut members = vec!["manifest.json"];
        members.extend(ASSETS.map(|(_, f, ..)| f));
        assert!(
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&out)
                .arg("-C")
                .arg(&src)
                .args(&members)
                .status()
                .unwrap()
                .success()
        );
        out
    }

    fn select(root: &Path, version: &str) {
        crate::telemetry::set(root, SETTING, version, "test").unwrap();
    }

    fn index(ui: &Ui) -> String {
        String::from_utf8(ui.asset("/").unwrap().1).unwrap()
    }

    #[test]
    fn install_switch_and_roll_back_without_restart() {
        let root = temp();
        assert_eq!(selected(Some(&root)).info["source"], "builtin");
        for v in ["2.0.0", "2.0.1"] {
            assert_eq!(
                install(&root, &tarball(&root, v, (1, 1))).unwrap()["installed"],
                v
            );
        }
        // Installing the same bundle again is a no-op.
        install(&root, &root.join("2.0.0.tar.gz")).unwrap();
        select(&root, "2.0.1");
        let ui = selected(Some(&root));
        assert_eq!(ui.info["source"], "bundle");
        assert!(index(&ui).contains("<title>2.0.1"));
        assert_eq!(
            ui.asset("/app.js").unwrap().0,
            "text/javascript; charset=utf-8"
        );
        select(&root, "2.0.0");
        assert!(index(&selected(Some(&root))).contains("<title>2.0.0"));
        select(&root, "builtin");
        let ui = selected(Some(&root));
        assert_eq!(ui.info["source"], "builtin");
        assert_eq!(index(&ui), ASSETS[0].3);
        assert!(ui.asset("/other.js").is_none());
        let listed = list(&root).unwrap();
        assert_eq!(listed["bundles"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn broken_or_incompatible_bundles_fall_back_visibly() {
        let root = temp();
        install(&root, &tarball(&root, "3.0.0", (1, 1))).unwrap();
        select(&root, "3.0.0");
        // Served (and cached) first, so the damage below must be noticed past the cache.
        assert_eq!(selected(Some(&root)).info["source"], "bundle");
        fs::write(bundles(&root).join("3.0.0/app.js"), "alert(1)").unwrap();
        let ui = selected(Some(&root));
        assert_eq!(ui.info["source"], "builtin");
        assert!(
            ui.info["fallback_reason"]
                .as_str()
                .unwrap()
                .contains("app.js does not match")
        );
        assert_eq!(index(&ui), ASSETS[0].3);
        // Refused when selected...
        assert!(selectable(&root, "3.0.0").is_err());
        assert!(selectable(&root, "9.9.9").is_err());
        selectable(&root, "builtin").unwrap();
        // ...and at load time when the server moved past its API range.
        bundle(&bundles(&root).join("4.0.0"), "4.0.0", "x", (2, 3));
        assert!(
            format!("{:#}", selectable(&root, "4.0.0").unwrap_err())
                .contains("supports dashboard API 2-3")
        );
        assert!(
            format!(
                "{:#}",
                install(&root, &tarball(&root, "5.0.0", (2, 2)))
                    .map(|_| ())
                    .and_then(|_| selectable(&root, "5.0.0"))
                    .unwrap_err()
            )
            .contains("this server speaks 1")
        );
        // A link instead of a file is refused.
        let dir = bundles(&root).join("6.0.0");
        bundle(&dir, "6.0.0", "x", (1, 1));
        fs::remove_file(dir.join("style.css")).unwrap();
        std::os::unix::fs::symlink(dir.join("app.js"), dir.join("style.css")).unwrap();
        assert!(selectable(&root, "6.0.0").is_err());
        // A version in the manifest that differs from the directory is refused.
        bundle(&bundles(&root).join("7.0.0"), "7.0.1", "x", (1, 1));
        assert!(selectable(&root, "7.0.0").is_err());
    }

    #[test]
    fn install_refuses_bad_bundles_and_keeps_existing_ones() {
        let root = temp();
        let good = tarball(&root, "1.2.3", (1, 1));
        install(&root, &good).unwrap();
        // Same version, different content.
        let src = root.join("other");
        bundle(&src, "1.2.3", "different", (1, 1));
        let other = root.join("other.tar.gz");
        let mut members = vec!["manifest.json"];
        members.extend(ASSETS.map(|(_, f, ..)| f));
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&other)
            .arg("-C")
            .arg(&src)
            .args(&members)
            .status()
            .unwrap();
        assert!(install(&root, &other).is_err());
        // Missing member and tampered member.
        fs::remove_file(src.join("app.js")).unwrap();
        let missing = root.join("missing.tar.gz");
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&missing)
            .arg("-C")
            .arg(&src)
            .args(members.iter().filter(|m| **m != "app.js"))
            .status()
            .unwrap();
        assert!(install(&root, &missing).is_err());
        assert!(
            !fs::read_dir(bundles(&root)).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.'))
        );
        for bad in ["", ".hidden", "builtin", "a/b", "../x", &"x".repeat(65)] {
            assert!(valid_version(bad).is_err(), "{bad}");
        }
        valid_version("1.0.0-rc.1").unwrap();
        assert!(selected(None).info["fallback_reason"].is_null());
    }
}
