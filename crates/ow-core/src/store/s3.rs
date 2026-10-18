use super::*;
use crate::snapshot_store::S3Store;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::process::{Child, ChildStdout, Stdio};

/// The deployment backup root, not `store/v1` itself. Uses the normal AWS CLI role chain.
pub struct S3Objects {
    transport: S3Store,
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
    pub fn verify_configuration(&self) -> Result<()> {
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
        let versioning = read("get-bucket-versioning")?;
        let lifecycle = read("get-bucket-lifecycle-configuration")?;
        check_configuration(&versioning, &lifecycle, &self.prefix)?;
        let policy = read("get-bucket-policy")?;
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
    fn same(&self, key: &str, checksum: &str, length: u64) -> Result<()> {
        let head = self.call("head-object", key, &["--checksum-mode", "ENABLED"])?;
        ensure!(
            head["ChecksumSHA256"] == checksum && head["ContentLength"] == length,
            "write-once object mismatch"
        );
        Ok(())
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
fn part(source: &mut dyn Read, path: &Path, size: u64) -> Result<(u64, Vec<u8>)> {
    let mut output = private_file(path)?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0; 1024 * 1024];
    let mut length = 0;
    let mut input = source.take(size);
    loop {
        let n = input.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        output.write_all(&bytes[..n])?;
        hash.update(&bytes[..n]);
        length += n as u64;
    }
    Ok((length, hash.finalize().to_vec()))
}
impl Objects for S3Objects {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        key_path(key)?;
        let scratch = Scratch::new()?;
        let path = scratch.0.join("part");
        let body = path.to_str().context("non UTF-8 staging path")?;
        let (mut length, mut hash) = part(source, &path, self.part_size)?;
        if length < self.part_size {
            let checksum = STANDARD.encode(&hash);
            if self
                .call(
                    "put-object",
                    key,
                    &[
                        "--body",
                        body,
                        "--if-none-match",
                        "*",
                        "--checksum-algorithm",
                        "SHA256",
                        "--checksum-sha256",
                        &checksum,
                    ],
                )
                .is_err()
            {
                self.same(key, &checksum, length)?;
            }
            return Ok(());
        }
        let created = self.call(
            "create-multipart-upload",
            key,
            &[
                "--checksum-algorithm",
                "SHA256",
                "--checksum-type",
                "COMPOSITE",
            ],
        )?;
        let upload = created["UploadId"].as_str().context("missing upload ID")?;
        let result = (|| {
            let mut parts = Vec::new();
            let mut total = 0;
            let mut composite = Sha256::new();
            loop {
                let number = parts.len() + 1;
                // shortcut: 64 MiB parts cap compressed objects at 625 GiB; raise part size before supporting larger ones.
                ensure!(number <= 10000, "multipart limit exceeded");
                let checksum = STANDARD.encode(&hash);
                let uploaded = self.call(
                    "upload-part",
                    key,
                    &[
                        "--upload-id",
                        upload,
                        "--part-number",
                        &number.to_string(),
                        "--body",
                        body,
                        "--checksum-sha256",
                        &checksum,
                    ],
                )?;
                ensure!(
                    uploaded["ChecksumSHA256"] == checksum,
                    "part checksum mismatch"
                );
                let etag = uploaded["ETag"].as_str().context("missing part ETag")?;
                parts.push(json!({"PartNumber":number,"ETag":etag,"ChecksumSHA256":checksum}));
                total += length;
                composite.update(&hash);
                fs::remove_file(&path)?;
                (length, hash) = part(source, &path, self.part_size)?;
                if length == 0 {
                    break;
                }
            }
            let checksum = format!("{}-{}", STANDARD.encode(composite.finalize()), parts.len());
            let manifest = json!({"Parts":parts}).to_string();
            if self
                .call(
                    "complete-multipart-upload",
                    key,
                    &[
                        "--upload-id",
                        upload,
                        "--multipart-upload",
                        &manifest,
                        "--if-none-match",
                        "*",
                        "--checksum-type",
                        "COMPOSITE",
                    ],
                )
                .is_err()
            {
                self.same(key, &checksum, total)?;
            }
            Ok(())
        })();
        // Also harmless after a lost completion response or successful completion.
        let _ = self.call("abort-multipart-upload", key, &["--upload-id", upload]);
        result
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        key_path(key)?;
        // High-level cp is read-only here; its stdout streams without staging a full disk.
        let mut child = self
            .aws()
            .args([
                "s3",
                "cp",
                "--only-show-errors",
                &format!("s3://{}/{}{key}", self.bucket, self.prefix),
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().context("download stdout")?;
        Ok(Box::new(Download {
            child,
            stdout,
            finished: false,
        }))
    }
}
struct Download {
    child: Child,
    stdout: ChildStdout,
    finished: bool,
}
impl Read for Download {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let n = self.stdout.read(bytes)?;
        if n == 0 && !self.finished {
            self.finished = true;
            if !self.child.wait()?.success() {
                return Err(std::io::Error::other("S3 download failed"));
            }
        }
        Ok(n)
    }
}
impl Drop for Download {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s3_conditional_single_and_multipart_retries_verify_checksums() {
        let scratch = Scratch::new().unwrap();
        let script = scratch.0.join("aws");
        fs::write(&script, r##"#!/usr/bin/env python3
import sys,json,hashlib,base64,pathlib
root=pathlib.Path(__file__).parent
args=sys.argv[1:]
def arg(k): return args[args.index(k)+1]
def digest(b): return base64.b64encode(hashlib.sha256(b).digest()).decode()
op=args[1]
key=arg('--key'); dest=root/'object'; state=root/'state'
with (root/'calls').open('a') as f: f.write(json.dumps(args)+'\n')
if op=='put-object':
 assert arg('--if-none-match')=='*'
 data=pathlib.Path(arg('--body')).read_bytes(); assert digest(data)==arg('--checksum-sha256')
 if dest.exists():sys.exit(1)
 dest.write_bytes(data);state.write_text(json.dumps({'ChecksumSHA256':digest(data),'ContentLength':len(data)}));print('{}')
elif op=='head-object':
 if not dest.exists():sys.exit(1)
 print(state.read_text())
elif op=='create-multipart-upload':print(json.dumps({'UploadId':'test-upload'}))
elif op=='upload-part':
 data=pathlib.Path(arg('--body')).read_bytes();h=digest(data);assert h==arg('--checksum-sha256')
 (root/('part'+arg('--part-number'))).write_bytes(data);print(json.dumps({'ETag':'etag','ChecksumSHA256':h}))
elif op=='complete-multipart-upload':
 assert arg('--if-none-match')=='*'
 parts=json.loads(arg('--multipart-upload'))['Parts'];data=b''.join((root/('part'+str(p['PartNumber']))).read_bytes() for p in parts)
 if dest.exists():sys.exit(1)
 composite=digest(b''.join(base64.b64decode(p['ChecksumSHA256']) for p in parts))+'-'+str(len(parts))
 dest.write_bytes(data);state.write_text(json.dumps({'ChecksumSHA256':composite,'ContentLength':len(data)}))
 # Deliberately lose the completion response after the effect.
 sys.exit(1)
elif op=='abort-multipart-upload': print('{}')
else:raise Exception(op)
"##).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = S3Objects::new("s3://bucket/root", None).unwrap();
        store.program = Some(script);
        store.part_size = 8; // Exercise the protocol with tiny parts, not S3's real size floor.
        let key = format!(
            "{}disk.ext4.zst",
            attempt_prefix(&token().unwrap()).unwrap()
        );
        for bytes in [&b"short"[..], &b"multipart-content"[..]] {
            let _ = fs::remove_file(scratch.0.join("object"));
            store.put_once(&key, &mut &bytes[..]).unwrap();
            store.put_once(&key, &mut &bytes[..]).unwrap();
            assert!(store.put_once(&key, &mut &b"wrong"[..]).is_err());
            assert_eq!(fs::read(scratch.0.join("object")).unwrap(), bytes);
        }
        let calls = fs::read_to_string(scratch.0.join("calls")).unwrap();
        assert!(calls.contains("head-object") && calls.contains("abort-multipart-upload"));
        assert!(calls.contains("root/store/v1/u/"));
        assert!(!calls.contains("delete-object") && !calls.contains("list-objects"));
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
