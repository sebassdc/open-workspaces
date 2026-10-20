use super::*;
use crate::snapshot_store::{
    S3Store,
    http::{Http, RANGE},
};
use serde_json::json;
use std::sync::{Arc, OnceLock};

/// The deployment backup root, not `store/v1` itself. Requests go through the
/// AWS SDK and its credential chain; objects move in parallel parts and ranges.
pub struct S3Objects {
    transport: S3Store,
    http: OnceLock<Arc<Http>>,
    bucket: String,
    prefix: String,
    part_size: u64,
}
impl S3Objects {
    pub fn new(url: &str, endpoint: Option<&str>) -> Result<Self> {
        #[cfg(feature = "test-endpoint")]
        let fixture = std::env::var("OW_TEST_S3_ENDPOINT").ok();
        #[cfg(feature = "test-endpoint")]
        let endpoint = endpoint.or(fixture.as_deref());
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
            part_size: RANGE,
        })
    }
    fn http(&self) -> Result<Arc<Http>> {
        if let Some(http) = self.http.get() {
            return Ok(http.clone());
        }
        let _ = self.http.set(Arc::new(Http::new(&self.transport)?));
        Ok(self.http.get().unwrap().clone())
    }
    /// Full key of a store-relative `key`, checked.
    fn key(&self, key: &str) -> Result<String> {
        key_path(key)?;
        Ok(format!("{}{key}", self.prefix))
    }
    /// Fail closed before publishing into a bucket that could expire live artifacts.
    pub fn configuration_snapshot(&self) -> Result<Value> {
        let http = self.http()?;
        let unverifiable = |what: &str| format!("cannot verify artifact bucket safety ({what})");
        let versioning = http
            .versioning()
            .with_context(|| unverifiable("get-bucket-versioning"))?;
        let lifecycle = http
            .lifecycle()
            .with_context(|| unverifiable("get-bucket-lifecycle-configuration"))?
            .with_context(|| unverifiable("get-bucket-lifecycle-configuration"))?;
        let policy = http
            .policy()
            .with_context(|| unverifiable("get-bucket-policy"))?;
        Ok(
            json!({"versioning":{"Status":versioning},"lifecycle":lifecycle,"policy":{"Policy":policy}}),
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
}
impl gc::ControllerObjects for S3Objects {
    fn configuration(&self) -> Result<()> {
        self.verify_configuration()
    }
    fn marker(&self, run: &str) -> Result<i64> {
        attempt_prefix(run)?;
        let key = format!("{}gc/runs/{run}.json", self.prefix);
        let http = self.http()?;
        ensure!(
            http.put_new(&key, &b"{\"format\":1}"[..])
                .context("S3 run marker failed")?,
            "S3 run marker already exists"
        );
        http.head(&key, false)
            .context("S3 run marker failed")?
            .and_then(|head| head.modified)
            .context("run marker has no server timestamp")
    }
    fn list(&self) -> Result<Vec<gc::ListedObject>> {
        let mut objects = Vec::new();
        for object in self
            .http()?
            .list(&format!("{}u/", self.prefix))
            .context("S3 candidate enumeration failed")?
        {
            let key = object
                .key
                .strip_prefix(&self.prefix)
                .context("listing escaped store")?;
            key_path(key)?;
            objects.push(gc::ListedObject {
                key: key.into(),
                bytes: object.size,
                modified: object.modified,
            });
        }
        Ok(objects)
    }
    fn head(&self, key: &str) -> Result<Option<gc::ObjectVersion>> {
        let key = self.key(key)?;
        let Some(head) = self
            .http()?
            .head(&key, false)
            .context("S3 verification HEAD failed")?
        else {
            return Ok(None);
        };
        let version = head.version.context("versioning proof missing")?;
        ensure!(
            !version.is_empty() && version != "null",
            "unversioned artifact"
        );
        Ok(Some(gc::ObjectVersion {
            bytes: head.length,
            version,
        }))
    }
    fn delete(&self, key: &str) -> Result<String> {
        let key = self.key(key)?;
        let (marker, version) = self.http()?.delete(&key)?;
        ensure!(marker, "delete did not return a versioned marker");
        let version = version.context("delete marker version missing")?;
        ensure!(
            !version.is_empty() && version != "null",
            "unversioned deletion"
        );
        Ok(version)
    }
}
impl Objects for S3Objects {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        let key = self.key(key)?;
        self.http()?.put_stream(&key, source, self.part_size, true)
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        let key = self.key(key)?;
        Ok(Box::new(self.http()?.reader(&key)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_s3::FakeS3;
    use crate::snapshot_store::http::transfers;
    use std::sync::atomic::Ordering;
    pub(super) fn objects(fake: &FakeS3, part_size: u64) -> S3Objects {
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
    fn s3_parts_grow_every_thousand_to_raise_the_object_limit() {
        let fake = FakeS3::start();
        let store = objects(&fake, 8);
        let data: Vec<u8> = (0..1000 * 8 + 3 * 16).map(|i| (i % 249) as u8).collect();
        let key = key();
        store.put_once(&key, &mut &data[..]).unwrap();
        assert_eq!(fake.calls("part"), 1003);
        assert_eq!(
            fake.state.objects.lock().unwrap()[&format!("root/store/v1/{key}")].bytes,
            data
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
        assert!(fake.peak("part") <= transfers());
        let mut read = Vec::new();
        store.get(&key).unwrap().read_to_end(&mut read).unwrap();
        assert!(read == data, "ranged download reassembled wrong bytes");
        assert_eq!(fake.calls("range"), 4);
        assert!(fake.peak("range") > 1 && fake.peak("range") <= transfers());
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
    #[test]
    fn s3_a_range_cut_short_is_fetched_again() {
        let fake = FakeS3::start();
        let store = objects(&fake, 64 << 10);
        let data: Vec<u8> = (0..(RANGE as usize + 4321))
            .map(|i| (i % 241) as u8)
            .collect();
        let key = key();
        store.put_once(&key, &mut &data[..]).unwrap();
        // Each range's first response sends half its body and drops the connection.
        fake.state.cut_ranges.store(2, Ordering::SeqCst);
        let mut read = Vec::new();
        store.get(&key).unwrap().read_to_end(&mut read).unwrap();
        assert!(read == data);
        assert_eq!(
            fake.calls("range"),
            4,
            "two cut ranges, each asked again once"
        );
    }
    #[test]
    fn s3_bucket_safety_is_read_through_the_sdk_and_fails_closed() {
        let fake = FakeS3::start();
        let store = objects(&fake, RANGE);
        // Nothing configured: unverifiable, so refused.
        assert!(store.verify_configuration().is_err());
        *fake.state.versioning.lock().unwrap() = Some("Enabled".into());
        *fake.state.lifecycle.lock().unwrap() = Some("<Rule><ID>undo</ID><Filter><Prefix>root/store/</Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration></Rule>".into());
        *fake.state.policy.lock().unwrap() = Some(json!({"Statement":[{"Effect":"Deny","Principal":"*","Action":"s3:PutObject","Resource":"arn:aws:s3:::bucket/root/store/v1/*","Condition":{"Null":{"s3:if-none-match":"true"},"Bool":{"s3:ObjectCreationOperation":"true"}}}]}).to_string());
        store.verify_configuration().unwrap();
        let evidence = store.configuration_snapshot().unwrap();
        assert_eq!(evidence["versioning"]["Status"], "Enabled");
        assert_eq!(
            evidence["lifecycle"]["Rules"][0]["Filter"]["Prefix"],
            "root/store/"
        );
        // An expiring rule, a tag-scoped undo rule or suspended versioning are refused.
        *fake.state.lifecycle.lock().unwrap() = Some("<Rule><ID>undo</ID><Filter><Prefix>root/store/</Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration></Rule><Rule><ID>age</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>30</Days></Expiration></Rule>".into());
        assert!(store.verify_configuration().is_err());
        *fake.state.lifecycle.lock().unwrap() = Some("<Rule><ID>undo</ID><Filter><And><Prefix>root/store/</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration></Rule>".into());
        assert!(store.verify_configuration().is_err());
        *fake.state.versioning.lock().unwrap() = Some("Suspended".into());
        assert!(store.verify_configuration().is_err());
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
fn controller_gc_uses_server_marker_listing_and_only_current_deletes() {
    use gc::ControllerObjects;
    let fake = crate::fake_s3::FakeS3::start();
    let s3 = tests::objects(&fake, RANGE);
    assert_eq!(
        s3.marker(&token().unwrap()).unwrap(),
        crate::fake_s3::MODIFIED
    );
    let prefix = attempt_prefix(&"a".repeat(32)).unwrap();
    for (file, bytes) in [
        ("manifest.json", &b"0123456789"[..]),
        ("disk.ext4.zst", &[7; 20][..]),
    ] {
        s3.put_once(&format!("{prefix}{file}"), &mut &bytes[..])
            .unwrap();
    }
    let listed = s3.list().unwrap();
    assert_eq!(listed.len(), 2, "run markers are not candidates");
    assert_eq!(listed.iter().map(|o| o.bytes).sum::<u64>(), 30);
    assert!(
        listed
            .iter()
            .all(|o| o.modified == crate::fake_s3::MODIFIED)
    );
    let head = s3.head(&listed[0].key).unwrap().unwrap();
    assert!(head.version.starts_with('v') && head.bytes == listed[0].bytes);
    assert!(s3.head(&format!("{prefix}memory.zst")).unwrap().is_none());
    assert!(s3.delete(&listed[0].key).unwrap().starts_with("marker-"));
    assert!(s3.head(&listed[0].key).unwrap().is_none());
    for bad in [
        "../catalog/saved.json",
        "u/aa/wrong/disk.ext4.zst",
        "gc/runs/run.json",
    ] {
        assert!(s3.delete(bad).is_err());
    }
    assert_eq!(
        fake.calls("delete"),
        1,
        "only the checked current object was deleted"
    );
}
