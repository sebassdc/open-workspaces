//! In-process S3 object requests for chunked recovery points. One AWS CLI call
//! exports the credentials of its usual chain (refreshed every few minutes);
//! each request is then signed here, so a chunk costs a request, not a process.
use super::*;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle, actions::ListObjectsV2};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const SIGNED_FOR: Duration = Duration::from_secs(600);
const REFRESH: Duration = Duration::from_secs(300);
const ATTEMPTS: u32 = 4;

pub(super) struct Http {
    bucket: Bucket,
    /// Key prefix of the store, with a trailing slash unless empty.
    pub prefix: String,
    client: reqwest::blocking::Client,
    credentials: Mutex<(Credentials, Instant)>,
    endpoint: Option<String>,
}

impl Http {
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
        let response = self.send("checking stored object", |c| {
            let url = self.bucket.head_object(Some(c), key).sign(SIGNED_FOR);
            self.client.head(url)
        })?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => bail!("checking stored object failed (HTTP {status})"),
        }
    }

    /// Creates `key` unless it exists; false when it already did. The body's
    /// SHA-256 is checked by the store.
    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let checksum = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            Sha256::digest(&body),
        );
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
