use super::*;
use crate::snapshot_store::{
    S3Store,
    http::{Http, checksum, transfers},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

/// Bytes per ranged download request.
const RANGE: u64 = 16 << 20;

/// The deployment backup root, not `store/v1` itself. Uses the normal AWS CLI
/// role chain; objects move in process, in parallel parts and ranges.
pub struct S3Objects {
    transport: S3Store,
    http: OnceLock<Arc<Http>>,
    bucket: String,
    prefix: String,
    part_size: u64,
    #[cfg(test)]
    program: Option<PathBuf>,
}
impl S3Objects {
    pub fn new(url: &str, endpoint: Option<&str>) -> Result<Self> {
        let transport = S3Store::new(url, endpoint)?;
        let root = url
            .strip_prefix("s3://")
            .context("S3 URL required")?
            .trim_end_matches('/');
        let (bucket, prefix) = root.split_once('/').unwrap_or((root, ""));
        ensure!(
            !bucket.is_empty()
                && bucket.len() <= 63
                && bucket
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
            "invalid S3 bucket"
        );
        ensure!(
            prefix.len() <= 512
                && prefix.split('/').all(|p| p.is_empty() && prefix.is_empty()
                    || !p.is_empty()
                        && p != "."
                        && p != ".."
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "invalid S3 prefix"
        );
        Ok(Self {
            transport,
            http: OnceLock::new(),
            bucket: bucket.into(),
            prefix: if prefix.is_empty() {
                "store/v1/".into()
            } else {
                format!("{prefix}/store/v1/")
            },
            part_size: 64 * 1024 * 1024,
            #[cfg(test)]
            program: None,
        })
    }
    fn aws(&self) -> std::process::Command {
        let command = self.transport.aws();
        #[cfg(test)]
        if let Some(program) = &self.program {
            let mut fake = std::process::Command::new(program);
            for (name, value) in command.get_envs() {
                if let Some(value) = value {
                    fake.env(name, value);
                } else {
                    fake.env_remove(name);
                }
            }
            return fake;
        }
        command
    }
    /// Fail closed before publishing into a bucket that could expire live artifacts.
    pub fn configuration_snapshot(&self) -> Result<Value> {
        let read = |operation: &str| -> Result<Value> {
            let output = self
                .aws()
                .args([
                    "s3api",
                    operation,
                    "--bucket",
                    &self.bucket,
                    "--output",
                    "json",
                ])
                .output()?;
            ensure!(
                output.status.success(),
                "cannot verify artifact bucket safety ({operation})"
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        Ok(
            json!({"versioning":read("get-bucket-versioning")?,"lifecycle":read("get-bucket-lifecycle-configuration")?,"policy":read("get-bucket-policy")?}),
        )
    }
    pub fn verify_configuration(&self) -> Result<()> {
        let evidence = self.configuration_snapshot()?;
        check_configuration(
            &evidence["versioning"],
            &evidence["lifecycle"],
            &self.prefix,
        )?;
        let policy = &evidence["policy"];
        let policy: Value = serde_json::from_str(
            policy["Policy"]
                .as_str()
                .context("artifact bucket policy missing")?,
        )?;
        ensure!(
            ["aws", "aws-us-gov", "aws-cn"]
                .iter()
                .any(|partition| check_bucket_policy(
                    &policy,
                    &format!("arn:{partition}:s3:::{}/{}*", self.bucket, self.prefix)
                )
                .is_ok()),
            "artifact bucket lacks the write-once creation policy"
        );
        Ok(())
    }
    fn command(&self, operation: &str, key: &str) -> Result<std::process::Command> {
        key_path(key)?;
        let mut command = self.aws();
        command
            .env("AWS_PAGER", "")
            .env("AWS_CLI_AUTO_PROMPT", "off");
        command.args([
            "s3api",
            operation,
            "--bucket",
            &self.bucket,
            "--key",
            &format!("{}{key}", self.prefix),
            "--output",
            "json",
        ]);
        Ok(command)
    }
    fn call(&self, operation: &str, key: &str, args: &[&str]) -> Result<Value> {
        let output = self
            .command(operation, key)?
            .args(args)
            .output()
            .context("AWS CLI required for artifact store")?;
        // CLI stderr can contain deployment details; keep it out of public responses.
        ensure!(
            output.status.success(),
            "S3 {operation} failed ({})",
            output.status
        );
        let value = if output.stdout.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&output.stdout)?
        };
        ensure!(
            value.get("Error").is_none(),
            "S3 {operation} returned an error"
        );
        Ok(value)
    }
    fn http(&self) -> Result<Arc<Http>> {
        if let Some(http) = self.http.get() {
            return Ok(http.clone());
        }
        let _ = self.http.set(Arc::new(Http::new(&self.transport)?));
        Ok(self.http.get().unwrap().clone())
    }
    /// The existing object at full key `key` must hold exactly these bytes.
    fn same(http: &Http, key: &str, checksum: &str, length: u64) -> Result<()> {
        let head = http
            .head(key, true)?
            .context("write-once object mismatch")?;
        let header = |name: &str| head.get(name).and_then(|v| v.to_str().ok());
        ensure!(
            header("x-amz-checksum-sha256") == Some(checksum)
                && header("content-length") == Some(&length.to_string()),
            "write-once object mismatch"
        );
        Ok(())
    }
    /// Uploads the parts of `source` after `first`, at most [`parts_in_flight`]
    /// at a time, then commits them unless `key` exists.
    fn multipart(
        &self,
        http: &Http,
        key: &str,
        upload: &str,
        first: Vec<u8>,
        source: &mut dyn Read,
    ) -> Result<()> {
        let done = Mutex::new(Vec::new());
        let failed = AtomicBool::new(false);
        // A rendezvous channel: a part is read only when a sender is free.
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(u16, bytes::Bytes)>(0);
        let receiver = Mutex::new(receiver);
        let mut total = 0u64;
        let mut count = 0u16;
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..parts_in_flight())
                .map(|_| {
                    scope.spawn(|| -> Result<()> {
                        let mut result = Ok(());
                        // After a failure keep draining so the reader never blocks.
                        loop {
                            // Not in the loop condition: the lock must drop before the upload.
                            let next = receiver.lock().unwrap().recv();
                            let Ok((number, body)) = next else { break };
                            if result.is_err() || failed.load(Ordering::Relaxed) {
                                continue;
                            }
                            match http.upload_part(key, upload, number, body) {
                                Ok((etag, checksum)) => {
                                    done.lock().unwrap().push((number, etag, checksum))
                                }
                                Err(error) => {
                                    failed.store(true, Ordering::Relaxed);
                                    result = Err(error);
                                }
                            }
                        }
                        result
                    })
                })
                .collect();
            let read = (|| -> Result<()> {
                let mut body = first;
                while !body.is_empty() && !failed.load(Ordering::Relaxed) {
                    // shortcut: 64 MiB parts cap compressed objects at 625 GiB; raise part size before supporting larger ones.
                    ensure!(count < 10000, "multipart limit exceeded");
                    count += 1;
                    total += body.len() as u64;
                    sender
                        .send((count, body.into()))
                        .map_err(|_| anyhow::anyhow!("part upload stopped"))?;
                    body = part(source, self.part_size)?;
                }
                Ok(())
            })();
            drop(sender);
            for worker in workers {
                worker.join().expect("part upload thread panicked")?;
            }
            read
        })?;
        let mut parts = done.into_inner().unwrap();
        parts.sort();
        ensure!(
            parts.len() == count as usize
                && parts.iter().enumerate().all(|(i, p)| p.0 as usize == i + 1),
            "multipart upload lost a part"
        );
        let mut composite = Sha256::new();
        for (_, _, checksum) in &parts {
            composite.update(STANDARD.decode(checksum)?);
        }
        let checksum = format!("{}-{}", STANDARD.encode(composite.finalize()), parts.len());
        let parts: Vec<_> = parts.into_iter().map(|(_, e, c)| (e, c)).collect();
        if let Err(error) = http.complete_multipart(key, upload, &parts) {
            Self::same(http, key, &checksum, total).map_err(|_| error)?;
        }
        Ok(())
    }
}
/// Parts uploaded at once: each holds one part, so memory is about this x 64 MiB.
fn parts_in_flight() -> usize {
    (transfers() / 4).max(1)
}
impl gc::ControllerObjects for S3Objects {
    fn configuration(&self) -> Result<()> {
        self.verify_configuration()
    }
    fn marker(&self, run: &str) -> Result<i64> {
        attempt_prefix(run)?;
        let key = format!("gc/runs/{run}.json");
        let scratch = Scratch::new()?;
        let body = scratch.0.join("marker");
        private_file(&body)?.write_all(b"{\"format\":1}")?;
        let call = |op: &str, args: &[&str]| -> Result<Value> {
            let output = self
                .aws()
                .args([
                    "s3api",
                    op,
                    "--bucket",
                    &self.bucket,
                    "--key",
                    &format!("{}{key}", self.prefix),
                    "--output",
                    "json",
                ])
                .args(args)
                .output()?;
            ensure!(output.status.success(), "S3 run marker failed");
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        call(
            "put-object",
            &[
                "--body",
                body.to_str().context("marker path")?,
                "--if-none-match",
                "*",
            ],
        )?;
        let head = call("head-object", &[])?;
        gc::timestamp(
            head["LastModified"]
                .as_str()
                .context("run marker has no server timestamp")?,
        )
    }
    fn list(&self) -> Result<Vec<gc::ListedObject>> {
        let prefix = format!("{}u/", self.prefix);
        let mut continuation: Option<String> = None;
        let mut objects = Vec::new();
        loop {
            let mut command = self.aws();
            command.args([
                "s3api",
                "list-objects-v2",
                "--bucket",
                &self.bucket,
                "--prefix",
                &prefix,
                "--no-paginate",
                "--output",
                "json",
            ]);
            if let Some(token) = &continuation {
                command.args(["--continuation-token", token]);
            }
            let output = command.output()?;
            ensure!(output.status.success(), "S3 candidate enumeration failed");
            let value: Value = serde_json::from_slice(&output.stdout)?;
            if let Some(contents) = value.get("Contents") {
                for object in contents.as_array().context("invalid listing")? {
                    let key = object["Key"]
                        .as_str()
                        .context("listing key missing")?
                        .strip_prefix(&self.prefix)
                        .context("listing escaped store")?;
                    key_path(key)?;
                    objects.push(gc::ListedObject {
                        key: key.into(),
                        bytes: object["Size"].as_u64().context("listing size missing")?,
                        modified: gc::timestamp(
                            object["LastModified"]
                                .as_str()
                                .context("listing timestamp missing")?,
                        )?,
                    });
                }
            }
            if value["IsTruncated"] == false {
                break;
            }
            let next = value["NextContinuationToken"]
                .as_str()
                .context("incomplete listing")?
                .to_owned();
            ensure!(
                continuation.as_ref() != Some(&next),
                "listing did not advance"
            );
            continuation = Some(next);
        }
        Ok(objects)
    }
    fn head(&self, key: &str) -> Result<Option<gc::ObjectVersion>> {
        let output = self.command("head-object", key)?.output()?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("(404)") || error.contains("(NoSuchKey)") {
                return Ok(None);
            }
            anyhow::bail!("S3 verification HEAD failed");
        }
        let head: Value = serde_json::from_slice(&output.stdout)?;
        let version = head["VersionId"]
            .as_str()
            .context("versioning proof missing")?;
        ensure!(
            !version.is_empty() && version != "null",
            "unversioned artifact"
        );
        Ok(Some(gc::ObjectVersion {
            bytes: head["ContentLength"]
                .as_u64()
                .context("HEAD size missing")?,
            version: version.into(),
        }))
    }
    fn delete(&self, key: &str) -> Result<String> {
        let value = self.call("delete-object", key, &[])?;
        ensure!(
            value["DeleteMarker"] == true,
            "delete did not return a versioned marker"
        );
        let version = value["VersionId"]
            .as_str()
            .context("delete marker version missing")?;
        ensure!(
            !version.is_empty() && version != "null",
            "unversioned deletion"
        );
        Ok(version.into())
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("ow-upload-{}", token()?));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Up to `size` bytes of `source`; fewer only at its end.
fn part(source: &mut dyn Read, size: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    source.take(size).read_to_end(&mut bytes)?;
    Ok(bytes)
}
impl Objects for S3Objects {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        key_path(key)?;
        let http = self.http()?;
        let key = format!("{}{key}", self.prefix);
        let first = part(source, self.part_size)?;
        if (first.len() as u64) < self.part_size {
            let (checksum, length) = (checksum(&first), first.len() as u64);
            match http.put_new(&key, first) {
                Ok(true) => {}
                Ok(false) => Self::same(&http, &key, &checksum, length)?,
                Err(error) => Self::same(&http, &key, &checksum, length).map_err(|_| error)?,
            }
            return Ok(());
        }
        let upload = http.create_multipart(&key)?;
        let result = self.multipart(&http, &key, &upload, first, source);
        // Also harmless after a lost completion response or successful completion.
        let _ = http.abort_multipart(&key, &upload);
        result
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        key_path(key)?;
        let http = self.http()?;
        let key = format!("{}{key}", self.prefix);
        let head = http.head(&key, false)?.context("stored object missing")?;
        let header = |name: &str| head.get(name).and_then(|v| v.to_str().ok());
        let size = header("content-length")
            .and_then(|v| v.parse().ok())
            .context("stored object size missing")?;
        // Every range must come from this one version of the object.
        let etag = header("etag").context("stored object ETag missing")?.into();
        Ok(Box::new(Download {
            http,
            key,
            etag,
            size,
            next: 0,
            pending: VecDeque::new(),
            current: Cursor::new(Vec::new()),
        }))
    }
}
/// An object read in order from parallel ranged requests, a bounded number ahead.
struct Download {
    http: Arc<Http>,
    key: String,
    etag: String,
    size: u64,
    next: u64,
    pending: VecDeque<JoinHandle<Result<Vec<u8>>>>,
    current: Cursor<Vec<u8>>,
}
impl Read for Download {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.current.read(bytes)?;
            if n > 0 || bytes.is_empty() {
                return Ok(n);
            }
            while self.pending.len() < (transfers() / 2).max(1) && self.next < self.size {
                let (start, end) = (self.next, (self.next + RANGE).min(self.size));
                let (http, key, etag) = (self.http.clone(), self.key.clone(), self.etag.clone());
                self.pending.push_back(std::thread::spawn(move || {
                    http.get_range(&key, start, end, &etag)
                }));
                self.next = end;
            }
            let Some(range) = self.pending.pop_front() else {
                return Ok(0);
            };
            let range = range
                .join()
                .map_err(|_| std::io::Error::other("download thread panicked"))?;
            self.current = Cursor::new(range.map_err(std::io::Error::other)?);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_s3::FakeS3;
    fn objects(fake: &FakeS3, part_size: u64) -> S3Objects {
        let mut objects = S3Objects::new("s3://bucket/root", None).unwrap();
        let _ = objects
            .http
            .set(Arc::new(Http::fixed(&fake.endpoint, "bucket", "root/")));
        objects.part_size = part_size;
        objects
    }
    fn key() -> String {
        format!(
            "{}disk.ext4.zst",
            attempt_prefix(&token().unwrap()).unwrap()
        )
    }
    #[test]
    fn s3_conditional_single_and_multipart_writes_verify_checksums() {
        let fake = FakeS3::start();
        // Tiny parts exercise the protocol, not S3's real size floor.
        let store = objects(&fake, 8);
        for bytes in [&b"short"[..], &b"multipart-content-in-several-parts"[..]] {
            let key = key();
            store.put_once(&key, &mut &bytes[..]).unwrap();
            // Retrying the same bytes succeeds; different bytes never replace them.
            store.put_once(&key, &mut &bytes[..]).unwrap();
            assert!(store.put_once(&key, &mut &b"wrong"[..]).is_err());
            assert!(
                store
                    .put_once(&key, &mut &b"wrong-and-long-enough"[..])
                    .is_err()
            );
            let stored = fake.state.objects.lock().unwrap()[&format!("root/store/v1/{key}")]
                .bytes
                .clone();
            assert_eq!(stored, bytes);
            let mut read = Vec::new();
            store.get(&key).unwrap().read_to_end(&mut read).unwrap();
            assert_eq!(read, bytes);
        }
        assert!(fake.calls("part") >= 5 && fake.calls("abort") >= 1);
        assert!(store.get(&key()).is_err(), "missing object read as empty");
    }
    #[test]
    fn s3_lost_completion_is_checked_not_repeated_and_failed_parts_fail_closed() {
        let fake = FakeS3::start();
        let store = objects(&fake, 1 << 10);
        let data: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let key = key();
        // The completion takes effect but every response is lost.
        fake.state.lose_completions.store(4, Ordering::SeqCst);
        store.put_once(&key, &mut &data[..]).unwrap();
        assert_eq!(
            fake.state.objects.lock().unwrap()[&format!("root/store/v1/{key}")].bytes,
            data
        );
        // A refused part fails the upload without committing anything.
        fake.state.lose_completions.store(0, Ordering::SeqCst);
        fake.state.refuse_puts.store(1, Ordering::SeqCst);
        let other = self::key();
        assert!(store.put_once(&other, &mut &data[..]).is_err());
        assert!(
            !fake
                .state
                .objects
                .lock()
                .unwrap()
                .contains_key(&format!("root/store/v1/{other}"))
        );
    }
    #[test]
    fn s3_parts_and_ranges_run_in_parallel_within_their_limits() {
        let fake = FakeS3::start();
        let store = objects(&fake, 64 << 10);
        let data: Vec<u8> = (0..(RANGE as usize * 3 + 12_345))
            .map(|i| (i % 253) as u8)
            .collect();
        let key = key();
        store.put_once(&key, &mut &data[..]).unwrap();
        let parts = fake.calls("part");
        assert_eq!(parts, data.len().div_ceil(64 << 10));
        assert!(fake.peak("part") > 1, "parts were sent one at a time");
        assert!(fake.peak("part") <= parts_in_flight());
        let mut read = Vec::new();
        store.get(&key).unwrap().read_to_end(&mut read).unwrap();
        assert!(read == data, "ranged download reassembled wrong bytes");
        assert_eq!(fake.calls("range"), 4);
        assert!(fake.peak("range") > 1 && fake.peak("range") <= (transfers() / 2).max(1));
        // A replaced object is never stitched from two versions.
        let mut download = store.get(&key).unwrap();
        fake.state
            .objects
            .lock()
            .unwrap()
            .get_mut(&format!("root/store/v1/{key}"))
            .unwrap()
            .etag = "\"other\"".into();
        assert!(download.read_to_end(&mut Vec::new()).is_err());
    }
}

fn check_configuration(versioning: &Value, lifecycle: &Value, prefix: &str) -> Result<()> {
    ensure!(
        versioning["Status"] == "Enabled",
        "artifact store requires bucket versioning"
    );
    let rules = lifecycle["Rules"]
        .as_array()
        .context("artifact lifecycle rules missing")?;
    let mut undo = false;
    for rule in rules.iter().filter(|r| r["Status"] == "Enabled") {
        let scope = rule["Filter"]["Prefix"]
            .as_str()
            .or(rule["Filter"]["And"]["Prefix"].as_str())
            .or(rule["Prefix"].as_str())
            .unwrap_or("");
        if !(prefix.starts_with(scope) || scope.starts_with(prefix)) {
            continue;
        }
        ensure!(
            rule["Expiration"].get("Days").is_none() && rule["Expiration"].get("Date").is_none(),
            "artifact lifecycle would expire live objects"
        );
        if let Some(days) = rule["NoncurrentVersionExpiration"]["NoncurrentDays"].as_u64() {
            ensure!(days >= 7, "artifact undo window is shorter than seven days");
            // An unconditional covering rule is needed, not a tag/size-filtered promise.
            if prefix.starts_with(scope)
                && rule["Filter"].get("Tag").is_none()
                && rule["Filter"].get("And").is_none()
                && rule["Filter"].get("ObjectSizeGreaterThan").is_none()
                && rule["Filter"].get("ObjectSizeLessThan").is_none()
            {
                undo = true;
            }
        }
    }
    ensure!(
        undo,
        "artifact store needs a covering seven-day noncurrent undo rule"
    );
    Ok(())
}
#[cfg(test)]
#[test]
fn s3_store_configuration_rejects_expiring_live_artifacts() {
    let versioning = json!({"Status":"Enabled"});
    let rule = json!({"Status":"Enabled","Filter":{"Prefix":"root/store/"},"NoncurrentVersionExpiration":{"NoncurrentDays":7}});
    let good = json!({"Rules":[rule.clone()]});
    assert!(check_configuration(&versioning, &good, "root/store/v1/").is_ok());
    for bad in [
        json!({"Rules":[rule.clone(),{"Status":"Enabled","Filter":{},"Expiration":{"Days":30}}]}),
        json!({"Rules":[{"Status":"Enabled","Filter":{"Prefix":"root/store/"},"NoncurrentVersionExpiration":{"NoncurrentDays":1}}]}),
        json!({"Rules":[]}),
    ] {
        assert!(check_configuration(&versioning, &bad, "root/store/v1/").is_err());
    }
    assert!(check_configuration(&json!({"Status":"Suspended"}), &good, "root/store/v1/").is_err());
}

fn check_bucket_policy(policy: &Value, resource: &str) -> Result<()> {
    let statements = policy["Statement"]
        .as_array()
        .context("bucket policy statements missing")?;
    let covered = |value: &Value, expected: &str| {
        value == expected
            || value
                .as_array()
                .is_some_and(|values| values.iter().any(|v| v == expected))
    };
    ensure!(
        statements.iter().any(|s| s["Effect"] == "Deny"
            && s["Principal"] == "*"
            && covered(&s["Action"], "s3:PutObject")
            && covered(&s["Resource"], resource)
            && s["Condition"].as_object().is_some_and(|o| o.len() == 2)
            && s["Condition"]["Null"]
                .as_object()
                .is_some_and(|o| o.len() == 1)
            && s["Condition"]["Bool"]
                .as_object()
                .is_some_and(|o| o.len() == 1)
            && s["Condition"]["Null"]["s3:if-none-match"] == "true"
            && s["Condition"]["Bool"]["s3:ObjectCreationOperation"] == "true"),
        "artifact bucket lacks the write-once creation policy"
    );
    Ok(())
}
#[cfg(test)]
#[test]
fn s3_store_requires_write_once_policy_for_every_principal() {
    let resource = "arn:aws:s3:::bucket/root/store/v1/*";
    let mut policy = json!({"Statement":[{"Effect":"Deny","Principal":"*","Action":"s3:PutObject","Resource":resource,"Condition":{"Null":{"s3:if-none-match":"true"},"Bool":{"s3:ObjectCreationOperation":"true"}}}]});
    assert!(check_bucket_policy(&policy, resource).is_ok());
    policy["Statement"][0]["Principal"] = json!({"AWS":"one-role"});
    assert!(check_bucket_policy(&policy, resource).is_err());
    assert!(check_bucket_policy(&json!({"Statement":[]}), resource).is_err());
}

#[cfg(test)]
#[test]
fn controller_gc_uses_server_marker_paged_listing_and_only_current_deletes() {
    use gc::ControllerObjects;
    let scratch = Scratch::new().unwrap();
    let script = scratch.0.join("aws");
    fs::write(&script,r#"#!/usr/bin/env python3
import sys,json,pathlib
root=pathlib.Path(__file__).parent
a=sys.argv[1:]
def arg(k):return a[a.index(k)+1]
op=a[1]
with (root/'gc-calls').open('a') as f:f.write(json.dumps(a)+'\n')
assert '--version-id' not in a
assert arg('--bucket')=='bucket'
if op=='put-object':
 assert arg('--if-none-match')=='*'
 assert arg('--key').startswith('root/store/v1/gc/runs/')
 assert pathlib.Path(arg('--body')).read_text()=='{"format":1}'
 print('{}')
elif op=='head-object':
 key=arg('--key')
 if key.endswith('memory.zst'):
  print('An error occurred (404) when calling the HeadObject operation',file=sys.stderr);sys.exit(1)
 if key.endswith('state.zst'):
  print('An error occurred (403) when calling the HeadObject operation',file=sys.stderr);sys.exit(1)
 print(json.dumps({'LastModified':'2026-10-19T12:34:56+00:00','ContentLength':9,'VersionId':'original'}))
elif op=='list-objects-v2':
 assert arg('--prefix')=='root/store/v1/u/' and '--no-paginate' in a
 page=1 if '--continuation-token' not in a else 2
 if page==2:assert arg('--continuation-token')=='page2'
 print(json.dumps({'Contents':[{'Key':'root/store/v1/u/aa/'+'a'*32+('/manifest.json' if page==1 else '/disk.ext4.zst'),'Size':page*10,'LastModified':'2026-10-19T10:00:00Z'}], 'IsTruncated':page==1,**({'NextContinuationToken':'page2'} if page==1 else {})}))
elif op=='delete-object':print(json.dumps({'DeleteMarker':True,'VersionId':'marker'}))
else:raise Exception(op)
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut s3 = S3Objects::new("s3://bucket/root", None).unwrap();
    s3.program = Some(script);
    assert_eq!(s3.marker(&token().unwrap()).unwrap(), 1792413296);
    let listed = s3.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed.iter().map(|o| o.bytes).sum::<u64>(), 30);
    assert_eq!(
        s3.head(&listed[0].key).unwrap().unwrap().version,
        "original"
    );
    let prefix = attempt_prefix(&"a".repeat(32)).unwrap();
    assert!(s3.head(&format!("{prefix}memory.zst")).unwrap().is_none());
    assert!(s3.head(&format!("{prefix}state.zst")).is_err());
    assert_eq!(s3.delete(&listed[0].key).unwrap(), "marker");
    for bad in [
        "../catalog/saved.json",
        "u/aa/wrong/disk.ext4.zst",
        "gc/runs/run.json",
    ] {
        assert!(s3.delete(bad).is_err());
    }
    let calls = fs::read_to_string(scratch.0.join("gc-calls")).unwrap();
    assert!(!calls.contains("--version-id") && !calls.contains("delete-object-version"));
}
