//! `ow upgrade`: replace this client with a published release, checked as install.sh checks
//! it, then renamed over the running binary so a failed check leaves the old one intact.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const RELEASES: &str = "https://github.com/sebassdc/open-workspaces/releases/download";
/// `--check` exit status when the target is a different release than this client.
pub(crate) const AVAILABLE: i32 = 10;

/// The release asset for this platform.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ARTIFACT: Option<&str> = Some("ow-linux-amd64");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ARTIFACT: Option<&str> = Some("ow-darwin-arm64");
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ARTIFACT: Option<&str> = Some("ow-darwin-amd64");
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    )
)))]
const ARTIFACT: Option<&str> = None;

type Version = (u64, u64, u64);

/// `X.Y.Z` or `vX.Y.Z`, decimal without leading zeros, as install.sh accepts.
fn parse(text: &str) -> Result<Version> {
    let text = text.trim();
    let parts: Vec<u64> = text
        .strip_prefix('v')
        .unwrap_or(text)
        .split('.')
        .map(|p| {
            ensure!(
                !p.is_empty()
                    && p.bytes().all(|b| b.is_ascii_digit())
                    && (p == "0" || !p.starts_with('0')),
                "not a release version: {text}"
            );
            Ok(p.parse()?)
        })
        .collect::<Result<_>>()?;
    match parts[..] {
        [a, b, c] => Ok((a, b, c)),
        _ => bail!("not a release version: {text}"),
    }
}

fn show((a, b, c): Version) -> String {
    format!("{a}.{b}.{c}")
}

/// Install `target`? Only a newer one, unless the user named it (the only way to downgrade).
fn wanted(current: Version, target: Version, explicit: bool) -> bool {
    target != current && (explicit || target > current)
}

/// The download is the published artifact: its `.sha256` names it and `SHA256SUMS` lists that
/// hash for this artifact, and its header is this platform's executable.
fn verify(artifact: &str, bytes: &[u8], published: &str, sums: &str) -> Result<()> {
    let published = published.trim();
    ensure!(
        published.len() == 64
            && published
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid published checksum for {artifact}"
    );
    let actual: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ensure!(
        actual == published,
        "download checksum mismatch for {artifact}"
    );
    ensure!(
        sums.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some(published)
                && fields.next().map(|f| f.trim_start_matches('*')) == Some(artifact)
                && fields.next().is_none()
        }),
        "SHA256SUMS does not list this {artifact} checksum"
    );
    let header: &[u8] = match artifact {
        "ow-linux-amd64" => &[0x7f, b'E', b'L', b'F', 2, 1, 1],
        "ow-darwin-arm64" => &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1],
        _ => &[0xcf, 0xfa, 0xed, 0xfe, 0x07, 0, 0, 1],
    };
    let x86_64 = artifact != "ow-linux-amd64" || bytes.get(18..20) == Some(&[0x3e, 0][..]);
    ensure!(
        bytes.starts_with(header) && x86_64,
        "download platform mismatch for {artifact}"
    );
    Ok(())
}

/// Largest download accepted; the published clients are far smaller.
const LIMIT: u64 = 512 << 20;

fn fetch(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("download {url}"))?;
    let mut bytes = Vec::new();
    response.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "{url} is larger than 512 MiB");
    Ok(bytes)
}

/// A host's own release (`<release>/bin/ow` beside its STAGED record, or under a host
/// layout's `releases/` with `run/` pins): its services run this file, so only the host
/// upgrader may replace it.
fn host_managed(exe: &Path) -> bool {
    let Some(release) = exe.parent().and_then(Path::parent) else {
        return false;
    };
    release.join("STAGED").exists()
        || release.parent().is_some_and(|releases| {
            releases.file_name() == Some("releases".as_ref())
                && releases
                    .parent()
                    .is_some_and(|srv| srv.join("run").is_dir())
        })
}

/// The path this client was started as: argv[0], looked up in PATH when it is a bare name.
fn invoked() -> Option<PathBuf> {
    let argv0 = PathBuf::from(std::env::args_os().next()?);
    if argv0.components().count() > 1 {
        return Some(argv0);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&argv0))
        .find(|path| path.is_file())
}

/// The file to replace: refused when reached through a symlink or owned by a host release.
fn replaceable() -> Result<PathBuf> {
    let raw = std::env::current_exe()?;
    for path in [Some(raw.clone()), invoked()].into_iter().flatten() {
        ensure!(
            !fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()),
            "{} is a symlink; reinstall with install.sh or upgrade the file it names",
            path.display()
        );
    }
    let exe = raw.canonicalize()?;
    ensure!(
        !host_managed(&exe),
        "{} belongs to this host's release; upgrade the host with ow-host upgrade",
        exe.display()
    );
    Ok(exe)
}

/// `ow --version` of the staged file, or an error if it does not answer within 30 s.
fn version_of(path: &Path) -> Result<String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the downloaded client did not answer --version within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .context("no --version output")?
        .read_to_string(&mut out)?;
    Ok(out.trim().to_owned())
}

/// Removes the staged file unless it was renamed into place.
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Downloads and verifies the client whose `.sha256` is `published`, runs it, then renames
/// it over `exe`.
fn install(
    client: &reqwest::blocking::Client,
    base: &str,
    exe: &Path,
    artifact: &str,
    published: &str,
    target: Version,
) -> Result<()> {
    let sums = String::from_utf8(fetch(client, &format!("{base}/SHA256SUMS"))?)?;
    let bytes = fetch(client, &format!("{base}/{artifact}"))?;
    verify(artifact, &bytes, published, &sums)?;
    let dir = exe.parent().context("the client has no directory")?;
    let staged = Staged(dir.join(format!(".ow-upgrade-{}", crate::common::nonce()?)));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&staged.0)
        .with_context(|| format!("cannot write next to {}", exe.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(&staged.0, fs::metadata(exe)?.permissions())?;
    ensure!(
        version_of(&staged.0)? == format!("ow {}", show(target)),
        "download version mismatch: expected ow {}",
        show(target)
    );
    fs::rename(&staged.0, exe)?;
    // The new client is in place; a failed directory sync only weakens crash durability.
    if let Err(error) = fs::File::open(dir).and_then(|d| d.sync_all()) {
        eprintln!(
            "warning: upgraded, but syncing {} failed: {error}",
            dir.display()
        );
    }
    Ok(())
}

/// Runs `ow upgrade`; `controller` asks the configured controller which release it runs.
pub(crate) fn run(
    version: Option<String>,
    check: bool,
    json: bool,
    controller: impl FnOnce() -> Result<String>,
) -> Result<i32> {
    let started = Instant::now();
    let current = parse(env!("CARGO_PKG_VERSION"))?;
    let explicit = version.is_some();
    let target = match version {
        Some(v) => parse(&v)?,
        None => parse(&controller()?).context("the controller's /api/version")?,
    };
    let upgrade = wanted(current, target, explicit);
    let (from, to) = (show(current), show(target));
    let report = |upgraded: bool| {
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        if json {
            println!(
                "{}",
                serde_json::json!({"from":from,"to":to,"seconds":seconds,"upgraded":upgraded})
            );
        } else if upgraded {
            println!("ow {from} -> {to} in {seconds} s");
        } else if !upgrade {
            println!("ow {from} is current (target {to})");
        } else {
            println!("ow {from}, target {to}: upgrade available");
        }
    };
    if !upgrade {
        // A timer runs `--json` often: a no-op stays silent there.
        if check || !json {
            report(false);
        }
        return Ok(0);
    }
    let artifact = ARTIFACT.context("no published client for this platform")?;
    let exe = if check { None } else { Some(replaceable()?) };
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(300))
        .build()?;
    let base = format!("{RELEASES}/v{to}");
    let published = String::from_utf8(fetch(&client, &format!("{base}/{artifact}.sha256"))?)?;
    let Some(exe) = exe else {
        report(false);
        return Ok(AVAILABLE);
    };
    install(&client, &base, &exe, artifact, &published, target)?;
    report(true);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_like_the_installer() {
        assert_eq!(parse("v0.9.57").unwrap(), (0, 9, 57));
        assert_eq!(parse("1.10.0\n").unwrap(), (1, 10, 0));
        for bad in ["", "v1.2", "1.2.3.4", "v01.2.3", "1.2.x", "1..3"] {
            assert!(parse(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn only_an_explicit_version_downgrades() {
        let v = (0, 9, 58);
        assert!(!wanted(v, v, false));
        assert!(!wanted(v, v, true));
        assert!(wanted(v, (0, 9, 59), false));
        assert!(wanted(v, (0, 10, 0), false));
        assert!(!wanted(v, (0, 9, 57), false), "the controller is older");
        assert!(wanted(v, (0, 9, 57), true));
    }

    #[test]
    fn equal_and_older_targets_never_download() {
        let ours = env!("CARGO_PKG_VERSION");
        let (a, b, c) = parse(ours).unwrap();
        assert_eq!(run(None, false, true, || Ok(ours.into())).unwrap(), 0);
        assert_eq!(run(None, true, false, || Ok(ours.into())).unwrap(), 0);
        let older = show((a, b.saturating_sub(1), c));
        assert_eq!(run(None, false, false, || Ok(older)).unwrap(), 0);
        assert!(run(None, true, false, || Ok("garbage".into())).is_err());
    }

    #[test]
    fn a_host_release_is_never_replaced() {
        let root =
            std::env::temp_dir().join(format!("ow-upgrade-{}", crate::common::nonce().unwrap()));
        let staged = root.join("a/releases/0.9.58-abc/bin/ow");
        let pinned = root.join("b/releases/0.9.58-abc/bin/ow");
        let user = root.join("c/.local/bin/ow");
        for exe in [&staged, &pinned, &user] {
            fs::create_dir_all(exe.parent().unwrap()).unwrap();
        }
        fs::write(root.join("a/releases/0.9.58-abc/STAGED"), "{}").unwrap();
        fs::create_dir_all(root.join("b/run")).unwrap();
        assert!(host_managed(&staged), "beside a STAGED record");
        assert!(host_managed(&pinned), "under releases/ with run/ pins");
        assert!(!host_managed(&user));
        assert!(!host_managed(Path::new("ow")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_download_that_does_not_match_is_refused() {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
        bytes.resize(20, 0);
        bytes[18] = 0x3e;
        let sha: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!("{sha}  ow-linux-amd64\n{}  install.sh\n", "0".repeat(64));
        verify("ow-linux-amd64", &bytes, &format!("{sha}\n"), &sums).unwrap();
        let other = "1".repeat(64);
        let refused = |artifact: &str, bytes: &[u8], published: &str, sums: &str| {
            verify(artifact, bytes, published, sums)
                .unwrap_err()
                .to_string()
        };
        assert!(refused("ow-linux-amd64", &bytes, &other, &sums).contains("checksum mismatch"));
        assert!(refused("ow-linux-amd64", &bytes, "abc", &sums).contains("invalid"));
        let unlisted = format!("{other}  ow-linux-amd64\n{sha}  ow-darwin-arm64\n");
        assert!(refused("ow-linux-amd64", &bytes, &sha, &unlisted).contains("SHA256SUMS"));
        let wrong = format!("{sha}  ow-darwin-arm64\n");
        assert!(refused("ow-darwin-arm64", &bytes, &sha, &wrong).contains("platform"));
    }
}
