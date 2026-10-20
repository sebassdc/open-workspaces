//! S3 requests through the AWS SDK for Rust, for recovery points, snapshot
//! backups and the artifact store. One client per store: the SDK's credential
//! chain (environment, profile, SSO, process, web identity, container and
//! instance roles) resolved once and refreshed by the SDK, one connection pool,
//! retries with backoff. No AWS CLI process runs.
//!
//! The callers are synchronous; requests run on one shared Tokio runtime and the
//! calling thread waits for each, so any number of threads may issue them.
use super::*;
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Region, RequestChecksumCalculation, ResponseChecksumValidation,
        http::HttpResponse, retry::RetryConfig,
    },
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::{ChecksumAlgorithm, ChecksumType, CompletedMultipartUpload, CompletedPart},
};
use bytes::Bytes;
use std::{future::Future, sync::OnceLock, time::Duration};

const ATTEMPTS: u32 = 4;

/// Parallel requests per transfer: `OW_S3_TRANSFERS` (1-128), default 16. Each
/// holds at most about 16 MiB, so memory stays near transfers x 16 MiB.
pub(crate) fn transfers() -> usize {
    std::env::var("OW_S3_TRANSFERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| (1..=128).contains(n))
        .unwrap_or(16)
}

/// Runs `future` on the shared runtime and waits for it on this thread. Callers
/// are never runtime workers, so waiting cannot starve the runtime.
fn wait<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("ow-s3")
            .enable_all()
            .build()
            .expect("starting the S3 runtime")
    });
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver.recv().expect("S3 runtime stopped")
}

/// What a HEAD request reports about an object.
pub(crate) struct Head {
    pub length: u64,
    pub etag: String,
    /// Base64 SHA-256 (with `-N` for a composite multipart checksum), when asked.
    pub checksum: Option<String>,
    pub version: Option<String>,
    pub modified: Option<i64>,
}

/// One listed object.
pub(crate) struct Listed {
    pub key: String,
    pub size: u64,
    pub modified: i64,
}

pub(crate) struct Http {
    client: Client,
    bucket: String,
    /// Key prefix of the store, with a trailing slash unless empty.
    pub prefix: String,
}

fn http_client() -> aws_sdk_s3::config::SharedHttpClient {
    aws_smithy_http_client::Builder::new()
        .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
            aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https()
}

fn s3_config(
    shared: &aws_config::SdkConfig,
    region: String,
    endpoint: Option<&str>,
    path_style: bool,
) -> aws_sdk_s3::Config {
    let mut builder = aws_sdk_s3::config::Builder::from(shared);
    // Only the checked endpoint: never AWS_ENDPOINT_URL* or a profile's endpoint_url.
    builder.set_endpoint_url(endpoint.map(str::to_owned));
    builder
        .region(Region::new(region))
        .force_path_style(path_style)
        .retry_config(RetryConfig::standard().with_max_attempts(ATTEMPTS))
        // Bodies carry the SHA-256 we computed; no extra CRC pass or aws-chunked framing.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build()
}

impl Http {
    /// A client for a fake store on loopback, with fixed credentials.
    #[cfg(test)]
    pub fn fixed(endpoint: &str, bucket: &str, prefix: &str) -> Self {
        let shared = aws_config::SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .http_client(http_client())
            .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
                aws_sdk_s3::config::Credentials::new("key", "secret", None, None, "fixture"),
            ))
            .build();
        Self {
            client: Client::from_conf(s3_config(&shared, "us-east-1".into(), Some(endpoint), true)),
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }

    pub fn new(store: &S3Store) -> Result<Self> {
        let path = store.url.strip_prefix("s3://").context("invalid S3 URL")?;
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        let bucket = bucket.to_owned();
        let endpoint = store.endpoint.clone();
        let probe = bucket.clone();
        let (shared, region) = wait(async move {
            let shared = aws_config::defaults(BehaviorVersion::latest())
                .http_client(http_client())
                .load()
                .await;
            let configured = shared
                .region()
                .map(|r| r.as_ref().to_owned())
                .unwrap_or_else(|| "us-east-1".into());
            let region = match endpoint {
                Some(_) => Ok(configured),
                None => bucket_region(&probe, configured).await,
            };
            (shared, region)
        });
        let region = region?;
        // Dotted names do not match the wildcard certificate as virtual hosts.
        let path_style = store.endpoint.is_some() || bucket.contains('.');
        Ok(Self {
            client: Client::from_conf(s3_config(
                &shared,
                region,
                store.endpoint.as_deref(),
                path_style,
            )),
            bucket,
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
        })
    }

    /// Whether an object exists. Any answer other than found/not found fails.
    pub fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.head(key, false)?.is_some())
    }

    /// An object's length, ETag, version and time, or None when it does not
    /// exist; `checksum` asks for its stored SHA-256 too.
    pub fn head(&self, key: &str, checksum: bool) -> Result<Option<Head>> {
        let mut request = self.client.head_object().bucket(&self.bucket).key(key);
        if checksum {
            request = request.checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        }
        match wait(request.send()) {
            Ok(head) => Ok(Some(Head {
                length: head.content_length.unwrap_or(0).max(0) as u64,
                etag: head.e_tag.context("stored object has no ETag")?,
                checksum: head.checksum_sha256,
                version: head.version_id,
                modified: head.last_modified.map(|t| t.secs()),
            })),
            Err(error) if status(&error) == Some(404) => Ok(None),
            Err(error) => Err(failed("checking stored object", error)),
        }
    }

    /// Creates `key` unless it exists; false when it already did. The body's
    /// SHA-256 is checked by the store.
    pub fn put_new(&self, key: &str, body: impl Into<Bytes>) -> Result<bool> {
        self.put_object(key, body.into(), true)
    }

    /// One PutObject; with `once` only when the key is absent (false when it was not).
    fn put_object(&self, key: &str, body: Bytes, once: bool) -> Result<bool> {
        let checksum = checksum(&body);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .set_if_none_match(once.then(|| "*".to_owned()))
                .checksum_sha256(&checksum)
                .body(ByteStream::from(body.clone()))
                .customize()
                .disable_payload_signing();
            match wait(request.send()) {
                Ok(_) => return Ok(true),
                // Another writer committed this key first.
                Err(error) if once && status(&error) == Some(412) => return Ok(false),
                // A concurrent conditional write is still in flight: ask again.
                Err(error) if status(&error) == Some(409) && attempt < ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(250 << attempt))
                }
                Err(error) => return Err(failed("uploading object", error)),
            }
        }
    }

    /// An object's bytes, refusing more than `limit`.
    pub fn get(&self, key: &str, limit: u64) -> Result<Vec<u8>> {
        self.read(key, None, limit)
    }

    /// Bytes `start..end` of the object version tagged `etag`.
    pub fn get_range(&self, key: &str, start: u64, end: u64, etag: &str) -> Result<Vec<u8>> {
        let bytes = self.read(key, Some((start, end, etag.to_owned())), end - start)?;
        ensure!(
            bytes.len() as u64 == end - start,
            "downloaded range has the wrong length"
        );
        Ok(bytes)
    }

    /// A GET whose response body is read whole; a body cut short is requested
    /// again (the SDK retries only up to the response headers).
    fn read(&self, key: &str, range: Option<(u64, u64, String)>, limit: u64) -> Result<Vec<u8>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self.client.get_object().bucket(&self.bucket).key(key);
            if let Some((start, end, etag)) = &range {
                request = request
                    .range(format!("bytes={start}-{}", end - 1))
                    .if_match(etag);
            }
            let read = wait(async move {
                let output = match request.send().await {
                    Ok(output) => output,
                    Err(error) => return Err(failed("downloading object", error)),
                };
                let mut body = output.body;
                let mut bytes = Vec::new();
                loop {
                    match body.try_next().await {
                        Ok(Some(chunk)) => {
                            bytes.extend_from_slice(&chunk);
                            if bytes.len() as u64 > limit {
                                return Err(anyhow!("stored object too large"));
                            }
                        }
                        Ok(None) => return Ok(Ok(bytes)),
                        Err(error) => return Ok(Err(DisplayErrorContext(&error).to_string())),
                    }
                }
            })?;
            match read {
                Ok(bytes) => return Ok(bytes),
                Err(cause) if attempt < ATTEMPTS => {
                    eprintln!("download of {key} interrupted ({cause}); retrying");
                    std::thread::sleep(Duration::from_millis(250 << attempt))
                }
                Err(cause) => bail!("download interrupted: {cause}"),
            }
        }
    }

    /// Starts a multipart upload whose parts carry SHA-256 checksums.
    fn create_multipart(&self, key: &str) -> Result<String> {
        let request = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .checksum_algorithm(ChecksumAlgorithm::Sha256)
            .checksum_type(ChecksumType::Composite);
        let created = wait(request.send()).map_err(|e| failed("starting upload", e))?;
        created.upload_id.context("missing upload ID")
    }

    /// Sends one part; returns its ETag and the base64 SHA-256 the store checked.
    fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u16,
        body: Bytes,
    ) -> Result<(String, String)> {
        let checksum = checksum(&body);
        let request = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload)
            .part_number(number.into())
            .checksum_sha256(&checksum)
            .body(ByteStream::from(body))
            .customize()
            .disable_payload_signing();
        let uploaded = wait(request.send()).map_err(|e| failed("uploading part", e))?;
        ensure!(
            uploaded.checksum_sha256.as_deref() == Some(checksum.as_str()),
            "part checksum mismatch"
        );
        Ok((uploaded.e_tag.context("missing part ETag")?, checksum))
    }

    /// Commits `parts` (ETag, checksum) in order; with `once` unless `key` already
    /// exists. Any failure, including a lost response after success, is an error.
    fn complete_multipart(
        &self,
        key: &str,
        upload: &str,
        parts: &[(String, String)],
        once: bool,
    ) -> Result<()> {
        let parts = parts
            .iter()
            .enumerate()
            .map(|(i, (etag, checksum))| {
                CompletedPart::builder()
                    .e_tag(etag)
                    .checksum_sha256(checksum)
                    .part_number(i as i32 + 1)
                    .build()
            })
            .collect();
        let request = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload)
            .set_if_none_match(once.then(|| "*".to_owned()))
            .checksum_type(ChecksumType::Composite)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            );
        wait(request.send()).map_err(|e| failed("completing upload", e))?;
        Ok(())
    }

    /// Discards an upload's parts; harmless when it already completed.
    fn abort_multipart(&self, key: &str, upload: &str) -> Result<()> {
        let request = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload);
        wait(request.send()).map_err(|e| failed("aborting upload", e))?;
        Ok(())
    }

    /// Every object under `prefix` (a full key prefix).
    pub fn list(&self, prefix: &str) -> Result<Vec<Listed>> {
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token.clone());
            let page = wait(request.send()).map_err(|e| failed("listing objects", e))?;
            for object in page.contents.unwrap_or_default() {
                objects.push(Listed {
                    key: object.key.context("listing key missing")?,
                    size: object.size.context("listing size missing")?.max(0) as u64,
                    modified: object
                        .last_modified
                        .context("listing timestamp missing")?
                        .secs(),
                });
            }
            match page.next_continuation_token {
                Some(next) if page.is_truncated == Some(true) => {
                    ensure!(token.as_ref() != Some(&next), "listing did not advance");
                    token = Some(next);
                }
                _ => return Ok(objects),
            }
        }
    }

    /// Writes a delete marker over the current version; returns (is a marker, its version).
    pub fn delete(&self, key: &str) -> Result<(bool, Option<String>)> {
        let request = self.client.delete_object().bucket(&self.bucket).key(key);
        let deleted = wait(request.send()).map_err(|e| failed("deleting object", e))?;
        Ok((deleted.delete_marker == Some(true), deleted.version_id))
    }

    /// The bucket's versioning status (`Enabled`, `Suspended`), if ever set.
    pub fn versioning(&self) -> Result<Option<String>> {
        let request = self.client.get_bucket_versioning().bucket(&self.bucket);
        let versioning =
            wait(request.send()).map_err(|e| failed("reading bucket versioning", e))?;
        Ok(versioning.status.map(|s| s.as_str().to_owned()))
    }

    /// The bucket's lifecycle rules in the S3 API's JSON shape, or None when it has none.
    pub fn lifecycle(&self) -> Result<Option<Value>> {
        let request = self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket);
        let rules = match wait(request.send()) {
            Ok(output) => output.rules.unwrap_or_default(),
            Err(error) if error.code() == Some("NoSuchLifecycleConfiguration") => {
                return Ok(None);
            }
            Err(error) => return Err(failed("reading bucket lifecycle", error)),
        };
        Ok(Some(
            json!({"Rules": rules.iter().map(rule_json).collect::<Vec<_>>()}),
        ))
    }

    /// The bucket policy document.
    pub fn policy(&self) -> Result<String> {
        let request = self.client.get_bucket_policy().bucket(&self.bucket);
        let policy = wait(request.send()).map_err(|e| failed("reading bucket policy", e))?;
        policy.policy.context("bucket policy missing")
    }
}

/// A lifecycle rule as the S3 API's JSON names it, keeping every filter key
/// present so safety checks can refuse conditional rules.
// The legacy top-level `Prefix` still scopes old rules; the safety checks must see it.
#[allow(deprecated)]
fn rule_json(rule: &aws_sdk_s3::types::LifecycleRule) -> Value {
    let mut out = json!({"Status": rule.status.as_str()});
    if let Some(prefix) = &rule.prefix {
        out["Prefix"] = json!(prefix);
    }
    if let Some(filter) = &rule.filter {
        let mut f = json!({});
        if let Some(prefix) = &filter.prefix {
            f["Prefix"] = json!(prefix);
        }
        if filter.tag.is_some() {
            f["Tag"] = json!({});
        }
        if let Some(size) = filter.object_size_greater_than {
            f["ObjectSizeGreaterThan"] = json!(size);
        }
        if let Some(size) = filter.object_size_less_than {
            f["ObjectSizeLessThan"] = json!(size);
        }
        if let Some(and) = &filter.and {
            f["And"] = json!({});
            if let Some(prefix) = &and.prefix {
                f["And"]["Prefix"] = json!(prefix);
            }
        }
        out["Filter"] = f;
    }
    if let Some(expiration) = &rule.expiration {
        let mut e = json!({});
        if let Some(days) = expiration.days {
            e["Days"] = json!(days);
        }
        if let Some(date) = &expiration.date {
            e["Date"] = json!(date.secs());
        }
        if let Some(marker) = expiration.expired_object_delete_marker {
            e["ExpiredObjectDeleteMarker"] = json!(marker);
        }
        out["Expiration"] = e;
    }
    if let Some(noncurrent) = &rule.noncurrent_version_expiration {
        let mut n = json!({});
        if let Some(days) = noncurrent.noncurrent_days {
            n["NoncurrentDays"] = json!(days);
        }
        out["NoncurrentVersionExpiration"] = n;
    }
    out
}

fn status<E>(error: &SdkError<E, HttpResponse>) -> Option<u16> {
    error.raw_response().map(|r| r.status().as_u16())
}

/// A request failure: HTTP status and S3 error code, never the message (it can
/// name resources); transport failures keep their cause.
fn failed<E>(what: &str, error: SdkError<E, HttpResponse>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    match (status(&error), error.code()) {
        (Some(status), Some(code)) => anyhow!("{what} failed (HTTP {status}: {code})"),
        (Some(status), None) => anyhow!("{what} failed (HTTP {status})"),
        _ => anyhow!("{what} failed: {}", DisplayErrorContext(&error)),
    }
}

/// The base64 SHA-256 S3 checks a body against.
pub(crate) fn checksum(body: &[u8]) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        Sha256::digest(body),
    )
}

/// AWS answers any bucket request, even an anonymous one to another region's
/// endpoint, with the bucket's region.
async fn bucket_region(bucket: &str, probe: String) -> Result<String> {
    let client = Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .http_client(http_client())
            .allow_no_auth()
            .region(Region::new(probe))
            .force_path_style(true)
            .retry_config(RetryConfig::standard().with_max_attempts(ATTEMPTS))
            .build(),
    );
    let region = match client.head_bucket().bucket(bucket).send().await {
        Ok(head) => head.bucket_region,
        Err(error) => error
            .raw_response()
            .and_then(|r| r.headers().get("x-amz-bucket-region"))
            .map(str::to_owned),
    };
    region
        .filter(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .context("cannot find the snapshot bucket's region")
}

/// Bytes per ranged download request and per upload part at first.
pub(crate) const RANGE: u64 = 16 << 20;

/// Up to `size` bytes of `source`; fewer only at its end.
fn part(source: &mut dyn Read, size: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    source.take(size).read_to_end(&mut bytes)?;
    Ok(bytes)
}

impl Http {
    /// Uploads `source` as `key`; the store checks every part's SHA-256. Parts of
    /// `part_size` (doubling every 1000 parts, at most 8x) go out on [`transfers`]
    /// threads with at most `transfers x 16 MiB` in flight. With `once` the object
    /// is created only if absent, and an existing one is accepted only when it
    /// holds exactly these bytes (which also covers a lost completion response).
    pub fn put_stream(
        &self,
        key: &str,
        source: &mut dyn Read,
        part_size: u64,
        once: bool,
    ) -> Result<()> {
        let first = part(source, part_size)?;
        if (first.len() as u64) < part_size {
            let (checksum, length) = (checksum(&first), first.len() as u64);
            return match self.put_object(key, first.into(), once) {
                Ok(true) => Ok(()),
                Ok(false) => self.same(key, &checksum, length),
                Err(error) if once => self.same(key, &checksum, length).map_err(|_| error),
                Err(error) => Err(error),
            };
        }
        let upload = self.create_multipart(key)?;
        let result = self.multipart(key, &upload, first, source, part_size, once);
        // Also harmless after a lost completion response or successful completion.
        let _ = self.abort_multipart(key, &upload);
        result
    }

    /// The existing object at `key` must hold exactly these bytes.
    fn same(&self, key: &str, checksum: &str, length: u64) -> Result<()> {
        let head = self
            .head(key, true)?
            .context("write-once object mismatch")?;
        ensure!(
            head.checksum.as_deref() == Some(checksum) && head.length == length,
            "write-once object mismatch"
        );
        Ok(())
    }

    fn multipart(
        &self,
        key: &str,
        upload: &str,
        first: Vec<u8>,
        source: &mut dyn Read,
        part_size: u64,
        once: bool,
    ) -> Result<()> {
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let done = Mutex::new(Vec::new());
        let failed = AtomicBool::new(false);
        let budget = transfers() as u64 * RANGE;
        let (in_flight, freed) = (Mutex::new(0u64), Condvar::new());
        // A rendezvous channel: a part is read only when a sender is free.
        let (sender, receiver) = std::sync::mpsc::sync_channel::<(u16, Bytes)>(0);
        let receiver = Mutex::new(receiver);
        let mut total = 0u64;
        let mut count = 0u16;
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..transfers())
                .map(|_| {
                    scope.spawn(|| -> Result<()> {
                        let mut result = Ok(());
                        // After a failure keep draining so the reader never blocks.
                        loop {
                            // Not in the loop condition: the lock must drop before the upload.
                            let next = receiver.lock().unwrap().recv();
                            let Ok((number, body)) = next else { break };
                            let length = body.len() as u64;
                            if result.is_ok() && !failed.load(Ordering::Relaxed) {
                                match self.upload_part(key, upload, number, body) {
                                    Ok((etag, checksum)) => {
                                        done.lock().unwrap().push((number, etag, checksum))
                                    }
                                    Err(error) => {
                                        failed.store(true, Ordering::Relaxed);
                                        result = Err(error);
                                    }
                                }
                            }
                            *in_flight.lock().unwrap() -= length;
                            freed.notify_one();
                        }
                        result
                    })
                })
                .collect();
            let read = (|| -> Result<()> {
                let mut body = first;
                while !body.is_empty() && !failed.load(Ordering::Relaxed) {
                    // shortcut: parts of 16, 32, 64, then 128 MiB cap compressed objects at about 987 GiB; grow them further before supporting larger ones.
                    ensure!(count < 10000, "multipart limit exceeded");
                    count += 1;
                    let length = body.len() as u64;
                    total += length;
                    {
                        let mut held = in_flight.lock().unwrap();
                        while *held > 0
                            && *held + length > budget
                            && !failed.load(Ordering::Relaxed)
                        {
                            held = freed.wait(held).unwrap();
                        }
                        *held += length;
                    }
                    sender
                        .send((count, body.into()))
                        .map_err(|_| anyhow!("part upload stopped"))?;
                    body = part(source, part_size << (count / 1000).min(3))?;
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
            composite.update(base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                checksum,
            )?);
        }
        let checksum = format!(
            "{}-{}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                composite.finalize()
            ),
            parts.len()
        );
        let parts: Vec<_> = parts.into_iter().map(|(_, e, c)| (e, c)).collect();
        match self.complete_multipart(key, upload, &parts, once) {
            Err(error) if once => self.same(key, &checksum, total).map_err(|_| error),
            other => other,
        }
    }

    /// An object read in order from parallel ranged requests, all pinned to the
    /// version first seen; [`transfers`] ranges of 16 MiB at most ahead.
    pub fn reader(self: &std::sync::Arc<Self>, key: &str) -> Result<Download> {
        let head = self.head(key, false)?.context("stored object missing")?;
        Ok(Download {
            http: self.clone(),
            key: key.into(),
            etag: head.etag,
            size: head.length,
            next: 0,
            pending: std::collections::VecDeque::new(),
            current: std::io::Cursor::new(Vec::new()),
        })
    }
}

pub(crate) struct Download {
    http: std::sync::Arc<Http>,
    key: String,
    etag: String,
    size: u64,
    next: u64,
    pending: std::collections::VecDeque<std::thread::JoinHandle<Result<Vec<u8>>>>,
    current: std::io::Cursor<Vec<u8>>,
}

impl Read for Download {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.current.read(bytes)?;
            if n > 0 || bytes.is_empty() {
                return Ok(n);
            }
            while self.pending.len() < transfers() && self.next < self.size {
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
            self.current = std::io::Cursor::new(range.map_err(std::io::Error::other)?);
        }
    }
}
