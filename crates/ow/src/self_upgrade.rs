//! Upgrades when an operator chooses (docs/UPGRADES.md#choosing-the-release): a published
//! release is only available until the controller's `upgrade.version` (the fleet) or
//! `upgrade.runner_versions` (one runner) names it. Each host's agent then sees it in its
//! acknowledgements and asks its own host's upgrader for it through a request file that a
//! root path unit watches (`ow-host self-upgrade`): download, checksum check, hand-over and
//! rollback are the upgrader's. Nothing here installs anything itself.
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The hidden setting that dates the last choice; it also lets a re-choice retry a release
/// a host refused or rolled back.
pub(crate) const CHOSEN_AT: &str = "upgrade.chosen_at";

/// The host layout root (`OW_HOST_LAYOUT`, set by the AWS host units). Elsewhere (a laptop,
/// a Mac host) it is unset and nothing is ever requested.
pub(crate) fn layout() -> Option<PathBuf> {
    std::env::var_os("OW_HOST_LAYOUT")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn request_path(layout: &Path) -> PathBuf {
    layout.join(".ow/self-upgrade/request.json")
}

fn result_path(layout: &Path) -> PathBuf {
    layout.join(".ow/self-upgrade/result.json")
}

fn read(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The release the host runs: the version its `current` pin was staged as.
pub(crate) fn host_release(layout: &Path) -> Option<String> {
    read(&layout.join("current/STAGED"))?["version"]
        .as_str()
        .map(str::to_owned)
}

/// What the host's upgrader last reported for a request (`ow-host self-upgrade`).
pub(crate) fn result(layout: &Path) -> Value {
    read(&result_path(layout)).unwrap_or(Value::Null)
}

/// A release version, `X.Y.Z` or `vX.Y.Z` (decimal, no leading zeros), as `X.Y.Z`.
pub(crate) fn release(text: &str) -> Result<String> {
    let bare = text.strip_prefix('v').unwrap_or(text);
    let parts: Vec<&str> = bare.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|p| {
                !p.is_empty()
                    && p.len() <= 6
                    && p.bytes().all(|b| b.is_ascii_digit())
                    && (*p == "0" || !p.starts_with('0'))
            }),
        "{text:?} is not a release version (X.Y.Z)"
    );
    Ok(bare.to_owned())
}

/// `upgrade.version` and `upgrade.client_version`: empty (nothing chosen) or a release.
pub(crate) fn typed_version(value: &str) -> Result<Value> {
    if value.is_empty() {
        return Ok(json!(""));
    }
    Ok(json!(release(value)?))
}

/// `upgrade.runner_versions`: a JSON object, runner name to release.
pub(crate) fn typed_runner_versions(value: &str) -> Result<Value> {
    let Ok(Value::Object(map)) =
        serde_json::from_str::<Value>(if value.is_empty() { "{}" } else { value })
    else {
        bail!("upgrade.runner_versions is a JSON object of runner name to release");
    };
    let mut out = serde_json::Map::new();
    for (runner, version) in map {
        crate::common::identifier(&runner)?;
        let Some(version) = version.as_str() else {
            bail!("upgrade.runner_versions: {runner}'s release is not a string");
        };
        out.insert(runner, json!(release(version)?));
    }
    Ok(Value::Object(out))
}

/// The release chosen for this runner (`node`; None for the controller's own host) from what
/// an acknowledgement or the settings carry: its own entry, else the fleet's.
pub(crate) fn chosen(offer: &Value, node: Option<&str>) -> Option<String> {
    node.and_then(|n| offer["runner_versions"][n].as_str())
        .or(offer["upgrade_version"].as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Asks the host's upgrader for `want` unless the host already runs it or this exact request
/// (release, controller release, decision time) was already made; `last` remembers it across
/// calls (read from the file on the first one, so a restarted agent does not ask again after
/// a refusal or rollback). True when a request was written.
pub(crate) fn follow(
    layout: &Path,
    want: Option<String>,
    controller: Option<&str>,
    chosen_at: i64,
    last: &mut Option<Value>,
) -> Result<bool> {
    let path = request_path(layout);
    let Some(want) = want else {
        // Nothing chosen (cleared, or an older controller): the host's upgrades follow its
        // config's release again.
        if last.as_ref() != Some(&Value::Null) {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => *last = Some(Value::Null),
            }
        }
        return Ok(false);
    };
    if host_release(layout).as_deref() == Some(want.as_str()) {
        return Ok(false);
    }
    let request = json!({"version":want,"controller":controller,"chosen_at":chosen_at});
    if last.is_none() {
        *last = read(&path).map(|mut old| {
            old.as_object_mut().map(|o| o.remove("requested_at"));
            old
        });
    }
    if last.as_ref() == Some(&request) {
        return Ok(false);
    }
    let mut written = request.clone();
    written["requested_at"] = json!(now());
    let dir = path.parent().expect("request path has a parent");
    let temporary = dir.join(format!(".request-{}.tmp", crate::common::nonce()?));
    // Mode 0644: the root upgrader reads it; it holds no secret.
    fs::write(&temporary, format!("{written}\n"))?;
    fs::rename(&temporary, &path)?;
    *last = Some(request);
    Ok(true)
}

/// The fields a runner adds to its heartbeat on a host layout: the release the host runs and
/// what its upgrader last reported.
pub(crate) fn beat_fields(beat: &mut Value) {
    if let Some(layout) = layout() {
        beat["host_release"] = json!(host_release(&layout));
        beat["self_upgrade"] = result(&layout);
    }
}

/// A controller-only host follows the fleet release (runners can be chosen one by one; the
/// controller cannot). Called when the controller starts and when the choice changes. A host
/// with its own runner leaves it to that runner's agent, which asks within one heartbeat.
pub(crate) fn follow_controller(root: &Path) {
    let Some(layout) = layout() else {
        return;
    };
    if layout.join(".ow/runner/node.json").exists() {
        return;
    }
    let outcome = crate::nodes::db(root)
        .and_then(|db| Ok((crate::telemetry::settings(&db)?, chosen_at(&db)?)))
        .and_then(|(settings, at)| {
            let offer = json!({"upgrade_version":settings["upgrade.version"]});
            follow(&layout, chosen(&offer, None), None, at, &mut None)
        });
    match outcome {
        Ok(true) => eprintln!("self-upgrade: asked this host's upgrader for the chosen release"),
        Ok(false) => {}
        Err(error) => eprintln!("self-upgrade: {error:#}"),
    }
}

/// When the release choice last changed (unix seconds; 0 when never).
pub(crate) fn chosen_at(db: &rusqlite::Connection) -> Result<i64> {
    use rusqlite::OptionalExtension;
    Ok(db
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM settings WHERE key=?1",
            [CHOSEN_AT],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Dates a choice inside the caller's transaction (no configuration version of its own:
/// the setting it accompanies has one).
pub(crate) fn mark_chosen(db: &rusqlite::Connection, at: i64) -> Result<()> {
    db.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![CHOSEN_AT, at.to_string()],
    )?;
    Ok(())
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `upgrade.status`: what is chosen, what every host runs, and, once every chosen host runs
/// its release, how long that took from the decision (`healthy_s`).
pub(crate) fn status(settings: &Value, at: i64, inventory: &Value) -> Value {
    let fleet = settings["upgrade.version"].as_str().unwrap_or("");
    let offer =
        json!({"upgrade_version":fleet,"runner_versions":settings["upgrade.runner_versions"]});
    let mut pending = vec![];
    let mut reached = vec![];
    let controller = layout().map(|layout| {
        let release = host_release(&layout);
        let result = result(&layout);
        let want = chosen(&offer, None);
        if want.is_some() && release != want {
            pending.push("controller".to_owned());
        } else if want.is_some() && result["version"].as_str() == want.as_deref() {
            reached.push(result["finished"].as_i64().unwrap_or(0));
        }
        json!({"release":release,"chosen":want,"self_upgrade":result})
    });
    let nodes: Vec<Value> = inventory
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["revoked"] != true)
        .map(|n| {
            let id = n["id"].as_str().unwrap_or_default();
            let cap = &n["capabilities"];
            let release = cap["host_release"].as_str().or(cap["version"].as_str());
            let want = chosen(&offer, Some(id));
            let current = want.is_none() || (n["online"] == true && release == want.as_deref());
            if !current {
                pending.push(id.to_owned());
            } else if want.is_some() {
                reached.push(cap["host_release_since"].as_i64().unwrap_or(0));
            }
            json!({"id":id,"online":n["online"],"chosen":want,"release":release,
                "agent":cap["version"],"worker":cap["worker"]["version"],
                "since":cap["host_release_since"],"current":current,"self_upgrade":cap["self_upgrade"]})
        })
        .collect();
    let healthy_s = (pending.is_empty() && at > 0)
        .then(|| reached.iter().map(|t| (t - at).max(0)).max().unwrap_or(0));
    json!({"chosen":{"version":fleet,"runner_versions":settings["upgrade.runner_versions"],
        "client_version":settings["upgrade.client_version"],"chosen_at":at},
        "controller":controller,"nodes":nodes,"pending":pending,"healthy_s":healthy_s})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_parse_strictly() {
        assert_eq!(release("v0.9.74").unwrap(), "0.9.74");
        assert_eq!(release("1.0.0").unwrap(), "1.0.0");
        for bad in [
            "",
            "0.9",
            "0.09.1",
            "0.9.1-rc1",
            "latest",
            "0.9.x",
            "../0.9.1",
        ] {
            assert!(release(bad).is_err(), "{bad}");
        }
        assert_eq!(typed_version("").unwrap(), json!(""));
        assert_eq!(
            typed_runner_versions(r#"{"r2":"v0.9.74"}"#).unwrap(),
            json!({"r2":"0.9.74"})
        );
        assert_eq!(typed_runner_versions("").unwrap(), json!({}));
        assert!(typed_runner_versions(r#"{"r 2":"0.9.74"}"#).is_err());
        assert!(typed_runner_versions(r#"{"r2":"next"}"#).is_err());
        assert!(typed_runner_versions("[]").is_err());
    }

    #[test]
    fn a_runner_entry_wins_over_the_fleet() {
        let offer = json!({"upgrade_version":"0.9.74","runner_versions":{"r2":"0.9.75"}});
        assert_eq!(chosen(&offer, Some("r2")).as_deref(), Some("0.9.75"));
        assert_eq!(chosen(&offer, Some("r3")).as_deref(), Some("0.9.74"));
        assert_eq!(chosen(&offer, None).as_deref(), Some("0.9.74"));
        assert_eq!(chosen(&json!({"upgrade_version":""}), Some("r3")), None);
        // An older controller sends neither: nothing is chosen.
        assert_eq!(chosen(&json!({"enabled":true}), Some("r3")), None);
    }

    struct Host(PathBuf);
    impl Host {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Host {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn host(release: &str) -> Host {
        let dir = std::env::temp_dir().join(format!(
            "ow-self-upgrade-{}",
            crate::common::nonce().unwrap()
        ));
        fs::create_dir_all(dir.join("current")).unwrap();
        fs::create_dir_all(dir.join(".ow/self-upgrade")).unwrap();
        fs::write(
            dir.join("current/STAGED"),
            json!({"version":release}).to_string(),
        )
        .unwrap();
        Host(dir)
    }

    #[test]
    fn a_request_is_written_once_per_choice() {
        let dir = host("0.9.73");
        let mut last = None;
        let want = || Some("0.9.74".to_owned());
        // Nothing chosen, or the release the host runs: no request.
        assert!(!follow(dir.path(), None, None, 5, &mut last).unwrap());
        assert!(!follow(dir.path(), Some("0.9.73".into()), None, 5, &mut last).unwrap());
        assert!(follow(dir.path(), want(), Some("0.9.73"), 5, &mut last).unwrap());
        let written = read(&request_path(dir.path())).unwrap();
        assert_eq!(written["version"], "0.9.74");
        assert_eq!(written["controller"], "0.9.73");
        assert!(written["requested_at"].as_i64().unwrap() > 0);
        // Every later acknowledgement: the same request is not made again.
        assert!(!follow(dir.path(), want(), Some("0.9.73"), 5, &mut last).unwrap());
        // A restarted agent (after a refusal or rollback) reads it back and does not ask again.
        assert!(!follow(dir.path(), want(), Some("0.9.73"), 5, &mut None).unwrap());
        // The controller moved (a runner refused for its protocol tries again), or the
        // operator chose again: a new request.
        assert!(follow(dir.path(), want(), Some("0.9.74"), 5, &mut last).unwrap());
        assert!(follow(dir.path(), want(), Some("0.9.74"), 9, &mut last).unwrap());
        // Cleared: the request goes, so upgrades follow the config's release again.
        assert!(!follow(dir.path(), None, None, 0, &mut last).unwrap());
        assert!(!request_path(dir.path()).exists());
        assert!(follow(dir.path(), want(), Some("0.9.74"), 9, &mut last).unwrap());
        // Done: the host runs it.
        fs::write(
            dir.path().join("current/STAGED"),
            json!({"version":"0.9.74"}).to_string(),
        )
        .unwrap();
        assert!(!follow(dir.path(), want(), Some("0.9.74"), 11, &mut last).unwrap());
    }

    #[test]
    fn status_times_the_decision_once_every_chosen_host_runs_it() {
        let settings = json!({"upgrade.version":"0.9.74","upgrade.runner_versions":{"r3":"0.9.73"},"upgrade.client_version":""});
        let node = |id: &str, release: &str, since: i64, online: bool| json!({"id":id,"online":online,"revoked":false,"capabilities":{"version":release,"host_release":release,"host_release_since":since,"worker":{"version":release}}});
        let inventory = json!([
            node("r2", "0.9.73", 50, true),
            node("r3", "0.9.73", 50, true)
        ]);
        let s = status(&settings, 100, &inventory);
        assert_eq!(s["pending"], json!(["r2"]));
        assert_eq!(s["healthy_s"], Value::Null);
        assert_eq!(s["nodes"][1]["current"], true);
        let inventory = json!([
            node("r2", "0.9.74", 131, true),
            node("r3", "0.9.73", 50, true)
        ]);
        let s = status(&settings, 100, &inventory);
        assert_eq!(s["pending"], json!([]));
        assert_eq!(s["healthy_s"], 31);
        // Offline at the chosen release is not healthy.
        let inventory = json!([node("r2", "0.9.74", 131, false)]);
        assert_eq!(status(&settings, 100, &inventory)["pending"], json!(["r2"]));
    }
}
