//! `ow upgrade`: replace this client with a published release, checked as install.sh checks
//! it, then renamed over the running binary so a failed check leaves the old one intact.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const RELEASES: &str = "https://github.com/sebassdc/open-workspaces/releases";
/// How long a release that failed after its `.sha256` was found (download, checks or
/// `status`) is not downloaded again: a timer runs `ow upgrade` every 30 s.
const REFUSAL: Duration = Duration::from_secs(600);
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

/// The release `RELEASES/latest` redirects to. GitHub sends there only for a published
/// release that is not a draft or prerelease.
fn tag(location: &str) -> Result<Version> {
    let tag = location
        .strip_prefix(RELEASES)
        .and_then(|rest| rest.strip_prefix("/tag/"))
        .with_context(|| format!("{RELEASES}/latest redirected to {location}, not a release"))?;
    parse(tag)
}

/// The candidate's `status` against the controller: connected and nothing to do first.
fn compatible(status: &str) -> Result<()> {
    let status: Value = serde_json::from_str(status).context("status did not print JSON")?;
    ensure!(status["connected"] == true, "status is not connected");
    match status["compatibility"].get("next_action") {
        Some(Value::Null) => Ok(()),
        Some(action) => bail!("status asks first: {action}"),
        None => bail!("status has no compatibility.next_action"),
    }
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

/// Total time for a small request (the latest lookup, `.sha256`, `SHA256SUMS`) and for
/// the client itself.
const SMALL: Duration = Duration::from_secs(10);
const DOWNLOAD: Duration = Duration::from_secs(300);

fn fetch(client: &reqwest::blocking::Client, url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(timeout)
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
    writable(exe.parent().context("the client has no directory")?)?;
    Ok(exe)
}

/// The new client is staged beside the old one and renamed over it: both need this.
fn writable(dir: &Path) -> Result<()> {
    let probe = Staged(dir.join(format!(".ow-upgrade-{}", crate::common::nonce()?)));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe.0)
        .with_context(|| format!("cannot write to {}; reinstall where you can", dir.display()))?;
    Ok(())
}

/// Standard output of `path args...`, or an error unless it exits 0 within 30 s.
fn output_of(path: &Path, args: &[OsString]) -> Result<String> {
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().context("no output")?;
    // Read while it runs: a full pipe would block a large status before it exits.
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        (&mut stdout)
            .take(LIMIT)
            .read_to_string(&mut out)
            .map(|_| out)
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the downloaded client did not finish {args:?} within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader
        .join()
        .map_err(|_| anyhow!("reading the downloaded client's output failed"))??;
    ensure!(
        status.success(),
        "the downloaded client's {args:?} exited with {status}"
    );
    Ok(out.trim().to_owned())
}

/// Removes the staged file unless it was renamed into place.
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Downloads and verifies the client whose `.sha256` is `published`, runs it, lets
/// `accept` judge it, then renames it over `exe`.
fn install(
    client: &reqwest::blocking::Client,
    base: &str,
    exe: &Path,
    artifact: &str,
    published: &str,
    target: Version,
    accept: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let sums = String::from_utf8(fetch(client, &format!("{base}/SHA256SUMS"), SMALL)?)?;
    let bytes = fetch(client, &format!("{base}/{artifact}"), DOWNLOAD)?;
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
        output_of(&staged.0, &["--version".into()])? == format!("ow {}", show(target)),
        "download version mismatch: expected ow {}",
        show(target)
    );
    accept(&staged.0)?;
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

/// Where the target release came from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    /// GitHub's latest release; installed only once its own `status` passed against the controller.
    Latest,
    /// The controller's `GET /api/version` server release.
    Controller,
    /// `--version`.
    Version,
}
impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Latest => "latest",
            Source::Controller => "controller",
            Source::Version => "version",
        }
    }
}

#[derive(Debug, PartialEq)]
enum Step {
    Current,
    Available,
    Upgraded,
}

#[derive(Debug)]
struct Outcome {
    to: Version,
    source: Source,
    reason: Option<String>,
    step: Step,
}

/// What `ow upgrade` reads and changes outside this process; tests answer it offline.
trait Releases {
    /// The `Location` that `RELEASES/latest` redirects to.
    fn latest(&mut self) -> Result<String>;
    /// The release the configured controller serves `GET /api/version` from.
    fn controller(&mut self) -> Result<String>;
    /// Fails unless this client's file may be replaced; checked before any download.
    fn replaceable(&mut self) -> Result<()>;
    /// This platform's `.sha256` in `target`'s release.
    fn published(&mut self, target: Version) -> Result<String>;
    /// Installs `target`; with `against_controller`, only once its own `status` is
    /// compatible.
    fn install(&mut self, target: Version, published: &str, against_controller: bool)
    -> Result<()>;
    /// Why installing `target` failed within `REFUSAL`, if it did.
    fn refused(&self, target: Version) -> Option<String>;
    fn refuse(&mut self, target: Version, why: &str);
    /// The controller is an HTTPS server, not this host's socket: only then can a
    /// candidate's `status` reach it.
    fn https(&mut self) -> Result<bool>;
}

/// Picks the target and installs it: the latest release when it is newer than this client
/// and compatible with the controller, else the controller's release, or exactly
/// `--version`. The latest release is only tried against an HTTPS server: its `status`
/// cannot reach a controller over the local socket.
fn select(
    releases: &mut impl Releases,
    current: Version,
    version: Option<&str>,
    check: bool,
) -> Result<Outcome> {
    if let Some(version) = version {
        return attempt(releases, current, parse(version)?, Source::Version, check);
    }
    let mut controller = None;
    let reason = if releases.https()? {
        match latest(releases, current, check, &mut controller)? {
            Ok(outcome) => return Ok(outcome),
            Err(reason) => reason,
        }
    } else {
        "the latest release needs an HTTPS server to check its status against".into()
    };
    let controller = match controller {
        Some(version) => version,
        None => releases.controller()?,
    };
    let target = parse(&controller).context("the controller's /api/version")?;
    let mut outcome = attempt(releases, current, target, Source::Controller, check)?;
    outcome.reason = Some(match outcome.reason {
        Some(also) => format!("{reason}; {also}"),
        None => reason,
    });
    Ok(outcome)
}

/// The latest release's outcome, or why the controller's release is the target instead.
/// Asks the controller first (into `controller`) when it will install: no controller, no
/// compatibility check, so no download.
fn latest(
    releases: &mut impl Releases,
    current: Version,
    check: bool,
    controller: &mut Option<String>,
) -> Result<Result<Outcome, String>> {
    let latest = match releases.latest().and_then(|location| tag(&location)) {
        Ok(latest) => latest,
        Err(error) => return Ok(Err(format!("the latest release is unknown: {error:#}"))),
    };
    if !wanted(current, latest, false) {
        return attempt(releases, current, latest, Source::Latest, check).map(Ok);
    }
    let v = show(latest);
    if let Some(why) = releases.refused(latest) {
        return Ok(Err(format!("v{v} was refused recently: {why}")));
    }
    if !check {
        releases.replaceable()?;
        *controller = Some(releases.controller()?);
    }
    // Not published for this platform yet: nothing was downloaded, so the next run retries.
    let published = match releases.published(latest) {
        Ok(published) => published,
        Err(error) => return Ok(Err(format!("v{v}: {error:#}"))),
    };
    let mut outcome = Outcome {
        to: latest,
        source: Source::Latest,
        reason: None,
        step: Step::Available,
    };
    if check {
        outcome.reason = Some("compatibility with the controller is checked on install".into());
        return Ok(Ok(outcome));
    }
    match releases.install(latest, &published, true) {
        Ok(()) => {
            outcome.step = Step::Upgraded;
            Ok(Ok(outcome))
        }
        // Any failure after the download began: a timer would download it every run.
        Err(error) => {
            let why = format!("{error:#}");
            releases.refuse(latest, &why);
            Ok(Err(format!("v{v}: {why}")))
        }
    }
}

/// The controller's release or `--version`: no compatibility check. A controller release
/// that failed after its `.sha256` was found is remembered, like a latest one; `--version`
/// always tries.
fn attempt(
    releases: &mut impl Releases,
    current: Version,
    target: Version,
    source: Source,
    check: bool,
) -> Result<Outcome> {
    let mut outcome = Outcome {
        to: target,
        source,
        reason: None,
        step: Step::Current,
    };
    if !wanted(current, target, source == Source::Version) {
        return Ok(outcome);
    }
    let remember = source == Source::Controller;
    if let Some(why) = releases.refused(target).filter(|_| remember) {
        outcome.reason = Some(format!("v{} was refused recently: {why}", show(target)));
        return Ok(outcome);
    }
    if !check {
        releases.replaceable()?;
    }
    let published = releases.published(target)?;
    if check {
        outcome.step = Step::Available;
        return Ok(outcome);
    }
    if let Err(error) = releases.install(target, &published, false) {
        if remember {
            releases.refuse(target, &format!("{error:#}"));
        }
        return Err(error);
    }
    outcome.step = Step::Upgraded;
    Ok(outcome)
}

/// The real network, files and processes.
struct Github<F, H> {
    client: reqwest::blocking::Client,
    no_redirect: reqwest::blocking::Client,
    controller: Option<F>,
    /// Global options this process got (`--data-dir`, `--server`, `--local`), for the
    /// candidate's `status`.
    options: Vec<OsString>,
    exe: Option<PathBuf>,
    memo: Option<PathBuf>,
    https: Option<H>,
}

impl<F: FnOnce() -> Result<String>, H: FnOnce() -> Result<bool>> Releases for Github<F, H> {
    fn latest(&mut self) -> Result<String> {
        let url = format!("{RELEASES}/latest");
        let response = self
            .no_redirect
            .get(&url)
            .timeout(SMALL)
            .send()
            .with_context(|| format!("look up {url}"))?;
        ensure!(
            response.status().is_redirection(),
            "{url} answered HTTP {}",
            response.status()
        );
        Ok(response
            .headers()
            .get(reqwest::header::LOCATION)
            .with_context(|| format!("{url} redirected without a Location"))?
            .to_str()?
            .to_owned())
    }

    fn controller(&mut self) -> Result<String> {
        (self
            .controller
            .take()
            .context("the controller was already asked")?)()
    }

    fn replaceable(&mut self) -> Result<()> {
        if self.exe.is_none() {
            self.exe = Some(replaceable()?);
        }
        Ok(())
    }

    fn published(&mut self, target: Version) -> Result<String> {
        let artifact = ARTIFACT.context("no published client for this platform")?;
        let url = format!("{RELEASES}/download/v{}/{artifact}.sha256", show(target));
        Ok(String::from_utf8(fetch(&self.client, &url, SMALL)?)?)
    }

    fn install(
        &mut self,
        target: Version,
        published: &str,
        against_controller: bool,
    ) -> Result<()> {
        let artifact = ARTIFACT.context("no published client for this platform")?;
        let exe = self.exe.clone().context("not checked as replaceable")?;
        let base = format!("{RELEASES}/download/v{}", show(target));
        let mut status = self.options.clone();
        status.push("status".into());
        install(
            &self.client,
            &base,
            &exe,
            artifact,
            published,
            target,
            |staged| {
                if !against_controller {
                    return Ok(());
                }
                output_of(staged, &status)
                    .and_then(|out| compatible(&out))
                    .context("not compatible with the controller")
            },
        )
    }

    fn refused(&self, target: Version) -> Option<String> {
        let path = self.memo.as_ref()?;
        let age = fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
        let text = fs::read_to_string(path).ok()?;
        let (version, why) = text.split_once('\n')?;
        (age < REFUSAL && version == show(target)).then(|| why.to_owned())
    }

    fn refuse(&mut self, target: Version, why: &str) {
        let v = show(target);
        let saved = match &self.memo {
            None => Err(anyhow!("HOME or XDG_CONFIG_HOME required")),
            Some(path) => path
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(path, format!("{v}\n{why}")))
                .with_context(|| format!("cannot write {}", path.display())),
        };
        // `--json` stays quiet on the fallback's no-op; this is the timer log's one trace.
        match saved {
            Ok(()) => eprintln!(
                "ow upgrade: v{v} refused for {} minutes: {why}",
                REFUSAL.as_secs() / 60
            ),
            Err(error) => eprintln!(
                "ow upgrade: v{v} failed and could not be remembered ({error:#}), so the next run downloads it again: {why}"
            ),
        }
    }

    fn https(&mut self) -> Result<bool> {
        (self.https.take().context("already asked")?)()
    }
}

/// Runs `ow upgrade`; `controller` asks the configured controller which release it runs,
/// `options` are the global options that configured it and `https` says whether that
/// controller is an HTTPS server (not the local socket); it is asked only without `--version`.
pub(crate) fn run(
    version: Option<String>,
    check: bool,
    json: bool,
    controller: impl FnOnce() -> Result<String>,
    options: Vec<OsString>,
    https: impl FnOnce() -> Result<bool>,
) -> Result<i32> {
    let started = Instant::now();
    let current = parse(env!("CARGO_PKG_VERSION"))?;
    let build = |policy| {
        reqwest::blocking::Client::builder()
            .https_only(true)
            .connect_timeout(SMALL)
            .timeout(DOWNLOAD)
            .redirect(policy)
            .build()
    };
    let mut github = Github {
        client: build(reqwest::redirect::Policy::default())?,
        no_redirect: build(reqwest::redirect::Policy::none())?,
        controller: Some(controller),
        options,
        exe: None,
        memo: crate::client::config()
            .ok()
            .map(|config| config.with_file_name("upgrade-refused")),
        https: Some(https),
    };
    let outcome = select(&mut github, current, version.as_deref(), check)?;
    // A timer runs `--json` often: a no-op stays silent there.
    if outcome.step == Step::Current && json && !check {
        return Ok(0);
    }
    let (from, to) = (show(current), show(outcome.to));
    let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    if json {
        let mut line = serde_json::json!({"from":from,"to":to,"seconds":seconds,
            "upgraded":outcome.step == Step::Upgraded,"source":outcome.source.name()});
        if let Some(reason) = &outcome.reason {
            line["reason"] = reason.as_str().into();
        }
        println!("{line}");
    } else {
        let source = outcome.source.name();
        match outcome.step {
            Step::Upgraded => println!("ow {from} -> {to} in {seconds} s (target from {source})"),
            Step::Current => println!("ow {from} is current (target {to} from {source})"),
            Step::Available => println!("ow {from}, target {to} from {source}: upgrade available"),
        }
        if let Some(reason) = &outcome.reason {
            println!("  {reason}");
        }
    }
    Ok(if outcome.step == Step::Available {
        AVAILABLE
    } else {
        0
    })
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

    /// Answers `Releases` from fields and records what was installed.
    struct Fake {
        latest: Result<&'static str, &'static str>,
        controller: &'static str,
        missing: Option<Version>,
        https: bool,
        /// Installing one of these fails with its message.
        broken: Vec<(Version, &'static str)>,
        installs: usize,
        refused: Option<(Version, String)>,
        asked_controller: bool,
        installed: Vec<(Version, bool)>,
    }

    fn fake(latest: &'static str, controller: &'static str) -> Fake {
        Fake {
            latest: Ok(latest),
            controller,
            missing: None,
            https: true,
            broken: Vec::new(),
            installs: 0,
            refused: None,
            asked_controller: false,
            installed: Vec::new(),
        }
    }

    impl Releases for Fake {
        fn latest(&mut self) -> Result<String> {
            self.latest
                .map(|tag| format!("{RELEASES}/tag/{tag}"))
                .map_err(|e| anyhow!(e))
        }
        fn controller(&mut self) -> Result<String> {
            assert!(!self.asked_controller, "the controller is asked once");
            self.asked_controller = true;
            Ok(self.controller.into())
        }
        fn replaceable(&mut self) -> Result<()> {
            Ok(())
        }
        fn published(&mut self, target: Version) -> Result<String> {
            ensure!(self.missing != Some(target), "404 for v{}", show(target));
            Ok("sha".into())
        }
        fn install(&mut self, target: Version, _: &str, against_controller: bool) -> Result<()> {
            self.installs += 1;
            if let Some((_, error)) = self.broken.iter().find(|(v, _)| *v == target) {
                bail!("{error}");
            }
            self.installed.push((target, against_controller));
            Ok(())
        }
        fn refused(&self, target: Version) -> Option<String> {
            self.refused
                .as_ref()
                .filter(|(v, _)| *v == target)
                .map(|(_, why)| why.clone())
        }
        fn refuse(&mut self, target: Version, why: &str) {
            self.refused = Some((target, why.into()));
        }
        fn https(&mut self) -> Result<bool> {
            Ok(self.https)
        }
    }

    const CURRENT: Version = (0, 9, 62);

    #[test]
    fn a_newer_compatible_latest_release_is_installed() {
        let mut r = fake("v0.9.64", "0.9.62");
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 64), Source::Latest, Step::Upgraded)
        );
        assert_eq!(o.reason, None);
        assert_eq!(
            r.installed,
            [((0, 9, 64), true)],
            "checked against the controller"
        );
    }

    #[test]
    fn an_incompatible_latest_release_falls_back_to_the_controller() {
        let mut r = fake("v0.9.64", "0.9.63");
        r.broken = vec![(
            (0, 9, 64),
            "not compatible with the controller: status is not connected",
        )];
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 63), Source::Controller, Step::Upgraded)
        );
        assert!(
            o.reason
                .unwrap()
                .contains("v0.9.64: not compatible with the controller")
        );
        assert_eq!(r.installed, [((0, 9, 63), false)]);
        assert!(
            r.refused.is_some(),
            "remembered so a timer does not download it again"
        );

        let mut again = fake("v0.9.64", "0.9.62");
        again.refused = r.refused.take();
        let o = select(&mut again, CURRENT, None, false).unwrap();
        assert_eq!((o.source, o.step), (Source::Controller, Step::Current));
        assert!(o.reason.unwrap().contains("refused recently"));
        assert!(again.installed.is_empty());
    }

    #[test]
    fn any_failure_after_the_download_began_is_remembered() {
        for error in [
            "download checksum mismatch for ow-darwin-arm64",
            "the downloaded client did not finish [\"--version\"] within 30 s",
        ] {
            let mut r = fake("v0.9.64", "0.9.62");
            r.broken = vec![((0, 9, 64), error)];
            let o = select(&mut r, CURRENT, None, false).unwrap();
            assert_eq!((o.source, o.step), (Source::Controller, Step::Current));
            assert_eq!(r.refused, Some(((0, 9, 64), error.to_owned())));
        }
    }

    #[test]
    fn a_broken_release_the_controller_also_runs_is_downloaded_once() {
        let mut r = fake("v0.9.64", "0.9.64");
        r.broken = vec![((0, 9, 64), "download checksum mismatch for ow-darwin-arm64")];
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 64), Source::Controller, Step::Current)
        );
        assert!(o.reason.unwrap().contains("refused recently"));
        assert_eq!(r.installs, 1, "the fallback does not download it again");
    }

    #[test]
    fn a_failed_controller_release_is_remembered_but_version_always_tries() {
        let mut r = fake("", "0.9.63");
        r.latest = Err("offline");
        r.broken = vec![((0, 9, 63), "download checksum mismatch for ow-darwin-arm64")];
        assert!(
            select(&mut r, CURRENT, None, false).is_err(),
            "the failure is reported"
        );
        assert_eq!(r.refused.as_ref().map(|(v, _)| *v), Some((0, 9, 63)));

        let mut again = fake("", "0.9.63");
        again.latest = Err("offline");
        again.broken = r.broken.clone();
        again.refused = r.refused.clone();
        let o = select(&mut again, CURRENT, None, false).unwrap();
        assert_eq!((o.source, o.step), (Source::Controller, Step::Current));
        assert!(o.reason.unwrap().contains("v0.9.63 was refused recently"));
        assert_eq!(again.installs, 0);

        assert!(select(&mut again, CURRENT, Some("v0.9.63"), false).is_err());
        assert_eq!(again.installs, 1, "--version is not held back by the memo");
    }

    #[test]
    fn a_local_controller_never_tries_the_latest_release() {
        let mut r = fake("v0.9.64", "0.9.63");
        r.latest = Err("not asked");
        r.https = false;
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 63), Source::Controller, Step::Upgraded)
        );
        assert!(o.reason.unwrap().contains("HTTPS server"));
        assert_eq!(r.installed, [((0, 9, 63), false)]);
    }

    #[test]
    fn a_directory_the_user_cannot_write_is_refused_before_downloading() {
        let dir =
            std::env::temp_dir().join(format!("ow-upgrade-{}", crate::common::nonce().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        writable(&dir).unwrap();
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "the probe is removed"
        );
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        // Root writes anywhere.
        if unsafe { libc::geteuid() } != 0 {
            assert!(
                writable(&dir)
                    .unwrap_err()
                    .to_string()
                    .contains("cannot write")
            );
        }
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_latest_release_without_its_assets_keeps_the_client() {
        let mut r = fake("v0.9.64", "0.9.62");
        r.missing = Some((0, 9, 64));
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!((o.source, o.step), (Source::Controller, Step::Current));
        assert!(o.reason.unwrap().contains("404"));
        assert!(
            r.installed.is_empty() && r.refused.is_none(),
            "retried next run"
        );
    }

    #[test]
    fn an_unknown_latest_release_falls_back_to_the_controller() {
        let mut r = fake("", "0.9.63");
        r.latest = Err("offline");
        let o = select(&mut r, CURRENT, None, false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 63), Source::Controller, Step::Upgraded)
        );
        assert!(o.reason.unwrap().contains("offline"));
    }

    #[test]
    fn a_latest_release_not_newer_is_a_no_op() {
        for latest in ["v0.9.62", "v0.9.61"] {
            let mut r = fake(latest, "0.9.70");
            let o = select(&mut r, CURRENT, None, false).unwrap();
            assert_eq!((o.source, o.step), (Source::Latest, Step::Current));
            assert!(!r.asked_controller && r.installed.is_empty());
        }
    }

    #[test]
    fn check_reports_the_latest_release_without_downloading_it() {
        let mut r = fake("v0.9.64", "0.9.62");
        let o = select(&mut r, CURRENT, None, true).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 64), Source::Latest, Step::Available)
        );
        assert!(o.reason.unwrap().contains("checked on install"));
        assert!(!r.asked_controller && r.installed.is_empty());
    }

    #[test]
    fn an_explicit_version_never_looks_up_the_latest_release() {
        let mut r = fake("v0.9.64", "0.9.64");
        r.latest = Err("not asked");
        let o = select(&mut r, CURRENT, Some("v0.9.57"), false).unwrap();
        assert_eq!(
            (o.to, o.source, o.step),
            ((0, 9, 57), Source::Version, Step::Upgraded)
        );
        assert_eq!(r.installed, [((0, 9, 57), false)]);
        let o = select(&mut r, CURRENT, Some("0.9.62"), false).unwrap();
        assert_eq!(o.step, Step::Current);
        assert!(select(&mut r, CURRENT, Some("garbage"), false).is_err());
        assert!(!r.asked_controller);
    }

    #[test]
    fn the_latest_tag_comes_from_the_release_redirect() {
        assert_eq!(tag(&format!("{RELEASES}/tag/v0.9.64")).unwrap(), (0, 9, 64));
        for bad in [
            format!("{RELEASES}"),
            format!("{RELEASES}/tag/"),
            format!("{RELEASES}/tag/v0.9"),
            format!("{RELEASES}/tag/v0.9.64-rc1"),
            format!("{RELEASES}/download/v0.9.64"),
            "https://github.com/someone/else/releases/tag/v0.9.64".into(),
            "/sebassdc/open-workspaces/releases/tag/v0.9.64".into(),
        ] {
            assert!(tag(&bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn only_a_connected_status_with_nothing_to_do_is_compatible() {
        compatible(r#"{"connected":true,"compatibility":{"next_action":null}}"#).unwrap();
        for bad in [
            r#"{"connected":false,"compatibility":{"next_action":null}}"#,
            r#"{"connected":true,"compatibility":{"next_action":"unsupported_capability: upgrade"}}"#,
            r#"{"connected":true,"compatibility":{}}"#,
            r#"{"connected":true}"#,
            "not json",
        ] {
            assert!(compatible(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn the_candidate_must_exit_zero_and_its_output_is_read() {
        let sh = Path::new("/bin/sh");
        let run = |script: &str| output_of(sh, &["-c".into(), script.into()]);
        let status = r#"{"connected":true,"compatibility":{"next_action":null}}"#;
        compatible(&run(&format!("printf '%s\\n' '{status}'")).unwrap()).unwrap();
        // Larger than a pipe's buffer: read while it runs, not after.
        assert_eq!(
            run("head -c 200000 /dev/zero | tr '\\0' x").unwrap().len(),
            200_000
        );
        assert!(
            run("echo '{}'; exit 3")
                .unwrap_err()
                .to_string()
                .contains("exited")
        );
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
