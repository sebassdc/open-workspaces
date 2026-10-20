//! In-process S3 object requests for recovery points and the artifact store. One
//! AWS CLI call exports the credentials of its usual chain (refreshed every few
//! minutes); each request is then signed here on one shared connection pool, so
//! an object or part costs a request, not a process.
use super::*;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use rusty_s3::{
    Bucket, Credentials, S3Action, UrlStyle,
    actions::{CreateMultipartUpload, ListObjectsV2},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const SIGNED_FOR: Duration = Duration::from_secs(600);
const REFRESH: Duration = Duration::from_secs(300);
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

pub(crate) struct Http {
    bucket: Bucket,
    /// Key prefix of the store, with a trailing slash unless empty.
    pub prefix: String,
    client: reqwest::blocking::Client,
    credentials: Mutex<(Credentials, Instant)>,
    endpoint: Option<String>,
}

impl Http {
    /// A client for a fake store on loopback, with fixed credentials.
    #[cfg(test)]
    pub fn fixed(endpoint: &str, bucket: &str, prefix: &str) -> Self {
        Self {
            bucket: Bucket::new(
                url::Url::parse(endpoint).unwrap(),
                UrlStyle::Path,
                bucket.to_owned(),
                "us-east-1".to_owned(),
            )
            .unwrap(),
            prefix: prefix.into(),
            client: reqwest::blocking::Client::new(),
            credentials: Mutex::new((Credentials::new("key", "secret"), Instant::now())),
            endpoint: None,
        }
    }

    pub fn new(store: &S3Store) -> Result<Self> {
        let path = store.url.strip_prefix("s3://").context("invalid S3 URL")?;
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let (endpoint, style, region) = match &store.endpoint {
            Some(endpoint) => (
                url::Url::parse(endpoint)?,
                UrlStyle::Path,
                configured_region(store).unwrap_or_else(|| "us-east-1".into()),
            ),
            None => {
                let configured = configured_region(store).unwrap_or_else(|| "us-east-1".into());
                let region = bucket_region(&client, bucket, &configured)?;
                let endpoint = url::Url::parse(&format!("https://s3.{region}.amazonaws.com"))?;
                // Dotted names do not match the wildcard certificate as virtual hosts.
                let style = if bucket.contains('.') {
                    UrlStyle::Path
                } else {
                    UrlStyle::VirtualHost
                };
                (endpoint, style, region)
            }
        };
        let bucket = Bucket::new(endpoint, style, bucket.to_owned(), region)
            .map_err(|e| anyhow!("invalid S3 bucket: {e}"))?;
        let credentials = Mutex::new((
            export_credentials(store.endpoint.as_deref())?,
            Instant::now(),
        ));
        Ok(Self {
            bucket,
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            client,
            credentials,
            endpoint: store.endpoint.clone(),
        })
    }

    fn credentials(&self) -> Result<Credentials> {
        let mut held = self.credentials.lock().unwrap();
        if held.1.elapsed() > REFRESH {
            *held = (
                export_credentials(self.endpoint.as_deref())?,
                Instant::now(),
            );
        }
        Ok(held.0.clone())
    }

    /// Sends one signed request, retrying throttling, server errors and lost connections.
    fn send(
        &self,
        what: &str,
        build: impl Fn(&Credentials) -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let credentials = self.credentials()?;
            let retry = match build(&credentials).send() {
                Ok(response)
                    if response.status().is_server_error()
                        || response.status() == reqwest::StatusCode::CONFLICT
                        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS =>
                {
                    format!("{what}: HTTP {}", response.status())
                }
                Ok(response) => return Ok(response),
                // A presigned URL carries the signature and session token; never print it.
                Err(error) => format!("{what}: {}", error.without_url()),
            };
            ensure!(attempt < ATTEMPTS, "{retry}");
            std::thread::sleep(Duration::from_millis(250 << attempt));
        }
    }

    /// Whether an object exists. Any answer other than found/not found fails.
    pub fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.head(key, false)?.is_some())
    }

    /// An object's headers, or None when it does not exist; `checksum` asks for
    /// its stored checksum too.
    pub fn head(&self, key: &str, checksum: bool) -> Result<Option<HeaderMap>> {
        let response = self.send("checking stored object", |c| {
            let mut action = self.bucket.head_object(Some(c), key);
            if checksum {
                action
                    .headers_mut()
                    .insert("x-amz-checksum-mode", "ENABLED");
            }
            let mut request = self.client.head(action.sign(SIGNED_FOR));
            if checksum {
                request = request.header("x-amz-checksum-mode", "ENABLED");
            }
            request
        })?;
        match response.status().as_u16() {
            200 => Ok(Some(response.headers().clone())),
            404 => Ok(None),
            status => bail!("checking stored object failed (HTTP {status})"),
        }
    }

    /// Creates `key` unless it exists; false when it already did. The body's
    /// SHA-256 is checked by the store.
    pub fn put_new(&self, key: &str, body: impl Into<Bytes>) -> Result<bool> {
        let body = body.into();
        let checksum = checksum(&body);
        let response = self.send("uploading object", |c| {
            let mut action = self.bucket.put_object(Some(c), key);
            action.headers_mut().insert("if-none-match", "*");
            action
                .headers_mut()
                .insert("x-amz-checksum-sha256", checksum.clone());
            self.client
                .put(action.sign(SIGNED_FOR))
                .header("if-none-match", "*")
                .header("x-amz-checksum-sha256", &checksum)
                .body(body.clone())
        })?;
        match response.status().as_u16() {
            200 => Ok(true),
            // Another writer committed this key first.
            412 => Ok(false),
            status => bail!(
                "conditional upload failed (HTTP {status}){}",
                code(response)
            ),
        }
    }

    /// An object's bytes, refusing more than `limit`.
    pub fn get(&self, key: &str, limit: u64) -> Result<Vec<u8>> {
        let response = self.send("downloading object", |c| {
            let url = self.bucket.get_object(Some(c), key).sign(SIGNED_FOR);
            self.client.get(url)
        })?;
        let status = response.status().as_u16();
        ensure!(
            status == 200,
            "downloading object failed (HTTP {status}){}",
            code(response)
        );
        let mut bytes = Vec::new();
        response.take(limit + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= limit, "stored object too large");
        Ok(bytes)
    }

    /// Bytes `start..end` of the object version tagged `etag`.
    pub fn get_range(&self, key: &str, start: u64, end: u64, etag: &str) -> Result<Vec<u8>> {
        let range = format!("bytes={start}-{}", end - 1);
        let response = self.send("downloading object", |c| {
            let mut action = self.bucket.get_object(Some(c), key);
            action.headers_mut().insert("range", range.clone());
            action.headers_mut().insert("if-match", etag.to_owned());
            self.client
                .get(action.sign(SIGNED_FOR))
                .header("range", &range)
                .header("if-match", etag)
        })?;
        let status = response.status().as_u16();
        ensure!(
            status == 206,
            "downloading object failed (HTTP {status}){}",
            code(response)
        );
        let mut bytes = Vec::with_capacity((end - start) as usize);
        response.take(end - start + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == end - start,
            "downloaded range has the wrong length"
        );
        Ok(bytes)
    }

    /// Starts a multipart upload whose parts carry SHA-256 checksums.
    pub fn create_multipart(&self, key: &str) -> Result<String> {
        let response = self.send("starting upload", |c| {
            let mut action = self.bucket.create_multipart_upload(Some(c), key);
            action
                .headers_mut()
                .insert("x-amz-checksum-algorithm", "SHA256");
            action
                .headers_mut()
                .insert("x-amz-checksum-type", "COMPOSITE");
            self.client
                .post(action.sign(SIGNED_FOR))
                .header("x-amz-checksum-algorithm", "SHA256")
                .header("x-amz-checksum-type", "COMPOSITE")
        })?;
        let status = response.status().as_u16();
        ensure!(
            status == 200,
            "starting upload failed (HTTP {status}){}",
            code(response)
        );
        let created = CreateMultipartUpload::parse_response(&response.text()?)
            .map_err(|e| anyhow!("invalid upload response: {e}"))?;
        Ok(created.upload_id().to_owned())
    }

    /// Sends one part; returns its ETag and the base64 SHA-256 the store checked.
    pub fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u16,
        body: Bytes,
    ) -> Result<(String, String)> {
        let checksum = checksum(&body);
        let response = self.send("uploading part", |c| {
            let mut action = self.bucket.upload_part(Some(c), key, number, upload);
            action
                .headers_mut()
                .insert("x-amz-checksum-sha256", checksum.clone());
            self.client
                .put(action.sign(SIGNED_FOR))
                .header("x-amz-checksum-sha256", &checksum)
                .body(body.clone())
        })?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (etag, stored) = (header("etag"), header("x-amz-checksum-sha256"));
        ensure!(
            status == 200,
            "uploading part failed (HTTP {status}){}",
            code(response)
        );
        ensure!(stored == Some(checksum.clone()), "part checksum mismatch");
        Ok((etag.context("missing part ETag")?, checksum))
    }

    /// Commits `parts` (ETag, checksum) in order unless `key` already exists.
    /// Any failure, including a lost response after success, is an error.
    pub fn complete_multipart(
        &self,
        key: &str,
        upload: &str,
        parts: &[(String, String)],
    ) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, (etag, checksum)) in parts.iter().enumerate() {
            body.push_str(&format!(
                "<Part><ETag>{}</ETag><ChecksumSHA256>{checksum}</ChecksumSHA256><PartNumber>{}</PartNumber></Part>",
                etag.replace('&', "&amp;").replace('<', "&lt;"),
                i + 1
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let response = self.send("completing upload", |c| {
            let mut action =
                self.bucket
                    .complete_multipart_upload(Some(c), key, upload, std::iter::empty());
            action.headers_mut().insert("if-none-match", "*");
            action
                .headers_mut()
                .insert("x-amz-checksum-type", "COMPOSITE");
            self.client
                .post(action.sign(SIGNED_FOR))
                .header("if-none-match", "*")
                .header("x-amz-checksum-type", "COMPOSITE")
                .body(body.clone())
        })?;
        let status = response.status().as_u16();
        ensure!(
            status == 200,
            "completing upload failed (HTTP {status}){}",
            code(response)
        );
        // S3 can report a failed completion inside a 200 response.
        let text = response.text()?;
        ensure!(
            text.contains("<CompleteMultipartUploadResult") && !text.contains("<Error>"),
            "completing upload failed"
        );
        Ok(())
    }

    /// Discards an upload's parts; harmless when it already completed.
    pub fn abort_multipart(&self, key: &str, upload: &str) -> Result<()> {
        self.send("aborting upload", |c| {
            let action = self.bucket.abort_multipart_upload(Some(c), key, upload);
            self.client.delete(action.sign(SIGNED_FOR))
        })?;
        Ok(())
    }

    /// Every key under `prefix` (a full key prefix).
    pub fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let response = self.send("listing objects", |c| {
                let mut action = ListObjectsV2::new(&self.bucket, Some(c));
                action.with_prefix(prefix);
                if let Some(token) = &token {
                    action.with_continuation_token(token.as_str());
                }
                self.client.get(action.sign(SIGNED_FOR))
            })?;
            let status = response.status().as_u16();
            ensure!(
                status == 200,
                "listing objects failed (HTTP {status}){}",
                code(response)
            );
            let page = ListObjectsV2::parse_response(&response.text()?)
                .map_err(|e| anyhow!("invalid object listing: {e}"))?;
            keys.extend(page.contents.into_iter().map(|c| c.key));
            match page.next_continuation_token {
                Some(next) => token = Some(next),
                None => return Ok(keys),
            }
        }
    }
}

/// The base64 SHA-256 S3 checks a body against.
pub(crate) fn checksum(body: &[u8]) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        Sha256::digest(body),
    )
}

/// The S3 error code of a failed response, never its message (it can name resources).
fn code(response: reqwest::blocking::Response) -> String {
    let body = response.text().unwrap_or_default();
    body.split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| format!(": {code}"))
        .unwrap_or_default()
}

fn export_credentials(endpoint: Option<&str>) -> Result<Credentials> {
    let output = aws(endpoint)
        .args(["configure", "export-credentials", "--format", "process"])
        .output()
        .context("the AWS CLI (aws) is required for snapshot stores")?;
    ensure!(
        output.status.success(),
        "exporting AWS credentials failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let exported: Value = serde_json::from_slice(&output.stdout)
        .context("the AWS CLI exported unreadable credentials")?;
    let field = |name: &str| exported[name].as_str().map(str::to_owned);
    let key = field("AccessKeyId").context("exported credentials lack a key")?;
    let secret = field("SecretAccessKey").context("exported credentials lack a secret")?;
    Ok(match field("SessionToken") {
        Some(token) => Credentials::new_with_token(key, secret, token),
        None => Credentials::new(key, secret),
    })
}

fn configured_region(store: &S3Store) -> Option<String> {
    for name in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(region) = std::env::var(name).ok().filter(|r| !r.is_empty()) {
            return Some(region);
        }
    }
    let output = store
        .aws()
        .args(["configure", "get", "region"])
        .output()
        .ok()?;
    let region = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!region.is_empty()).then_some(region)
}

/// AWS answers any bucket request, even an unauthenticated one to another
/// region's endpoint, with the bucket's region.
fn bucket_region(client: &reqwest::blocking::Client, bucket: &str, probe: &str) -> Result<String> {
    ensure!(
        !probe.is_empty()
            && probe
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid AWS region"
    );
    let response = client
        .head(format!("https://s3.{probe}.amazonaws.com/{bucket}"))
        .send()
        .context("finding the snapshot bucket's region")?;
    let region = response
        .headers()
        .get("x-amz-bucket-region")
        .and_then(|v| v.to_str().ok())
        .filter(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .context("cannot find the snapshot bucket's region")?;
    Ok(region.to_owned())
}
