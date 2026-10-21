//! The controller's STS client for the metadata mode of per-box AWS access
//! ([`crate::metadata`]): `AssumeRole` into box roles as the controller host's EC2 instance
//! role. Its credentials come from instance metadata only, never the SDK's default chain, so
//! no `AWS_*` variable or `~/.aws` file on the controller host changes who assumes box roles.
//! No AWS CLI process runs. Errors are fixed text: they reach box owners and logs, and must
//! never carry a request field or a credential.
use anyhow::{Context, Result, anyhow, ensure};
use aws_config::provider_config::ProviderConfig;
use aws_sdk_sts::{
    Client,
    config::{BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig},
    error::ProvideErrorMetadata,
    types::Tag,
};
use std::time::Duration;
use zeroize::Zeroizing;

/// Role session length; the controller refreshes with half of it left.
pub const DURATION_SECONDS: u64 = 3600;
/// Session tag keys, marked transitive: trust policies pin them, role chains keep them.
pub const BOX_TAG: &str = "ow-box";
pub const OWNER_TAG: &str = "ow-owner";

/// What the controller's instance identity document says about it.
#[derive(Clone, PartialEq, Eq)]
pub struct Host {
    pub account: String,
    pub region: String,
    pub zone: Option<String>,
}

/// One box's role session.
pub struct Request<'a> {
    pub role_arn: &'a str,
    /// `ow-` plus a hash of box and generation: role session name and source identity.
    pub session: &'a str,
    pub box_id: &'a str,
    /// The box owner's login email; tagged in lower case, as trust policies compare exactly.
    pub owner: &'a str,
}

/// Temporary credentials of a role session. Deliberately not `Debug`.
pub struct Session {
    pub access_key_id: String,
    pub secret_access_key: Zeroizing<String>,
    pub session_token: Zeroizing<String>,
    /// Unix seconds.
    pub expiration: u64,
}

pub struct Sts {
    client: Client,
    host: Host,
    runtime: tokio::runtime::Runtime,
}

/// The account of an IAM role ARN (validated by `credential_broker::role_arn`).
pub fn account(role_arn: &str) -> Option<&str> {
    role_arn.split(':').nth(4)
}

fn refusal(code: Option<&str>) -> anyhow::Error {
    match code.filter(|c| {
        (1..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
    }) {
        Some(code) => anyhow!("AWS STS refused the role session ({code})"),
        None => anyhow!("AWS STS could not be reached"),
    }
}

impl Sts {
    /// As this EC2 instance: its identity document and instance role credentials.
    pub fn from_instance() -> Result<Self> {
        Self::build(None, None)
    }

    /// `imds` and `sts` replace the AWS endpoints (tests only).
    #[doc(hidden)]
    pub fn build(imds: Option<&str>, sts: Option<&str>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
            ))
            .build_https();
        let config = ProviderConfig::without_region().with_http_client(http.clone());
        let mut metadata = aws_config::imds::Client::builder().configure(&config);
        if let Some(endpoint) = imds {
            metadata = metadata
                .endpoint(endpoint)
                .map_err(|_| anyhow!("invalid metadata endpoint"))?;
        }
        let metadata = metadata.build();
        let document = runtime
            .block_on(metadata.get("/latest/dynamic/instance-identity/document"))
            .map_err(|_| {
                anyhow!("instance metadata is unavailable: metadata mode needs a controller on EC2 with an instance role")
            })?;
        let document: serde_json::Value = serde_json::from_str(document.as_ref())
            .context("invalid instance identity document")?;
        let text = |key: &str| document[key].as_str().map(str::to_owned);
        let host = Host {
            account: text("accountId").context("instance identity has no account")?,
            region: text("region").context("instance identity has no region")?,
            zone: text("availabilityZone"),
        };
        let credentials = aws_config::imds::credentials::ImdsCredentialsProvider::builder()
            .configure(&config)
            .imds_client(metadata)
            .build();
        let mut sts_config = aws_sdk_sts::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .http_client(http)
            .region(Region::new(host.region.clone()))
            .credentials_provider(credentials)
            .retry_config(RetryConfig::standard().with_max_attempts(3))
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .operation_attempt_timeout(Duration::from_secs(10))
                    .operation_timeout(Duration::from_secs(30))
                    .build(),
            );
        if let Some(endpoint) = sts {
            sts_config = sts_config.endpoint_url(endpoint);
        }
        Ok(Self {
            client: Client::from_conf(sts_config.build()),
            host,
            runtime,
        })
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// `AssumeRole` with the box and owner as transitive session tags and the session name as
    /// source identity, for [`DURATION_SECONDS`]. Only roles in the controller's own account.
    pub fn assume(&self, request: &Request) -> Result<Session> {
        ensure!(
            account(request.role_arn) == Some(self.host.account.as_str()),
            "the role is outside the controller's AWS account"
        );
        let tag = |key: &str, value: &str| {
            Tag::builder()
                .key(key)
                .value(value)
                .build()
                .map_err(|_| anyhow!("invalid session tag"))
        };
        let call = self
            .client
            .assume_role()
            .role_arn(request.role_arn)
            .role_session_name(request.session)
            .source_identity(request.session)
            .duration_seconds(DURATION_SECONDS as i32)
            .tags(tag(BOX_TAG, request.box_id)?)
            .tags(tag(OWNER_TAG, &request.owner.to_ascii_lowercase())?)
            .transitive_tag_keys(BOX_TAG)
            .transitive_tag_keys(OWNER_TAG);
        let output = self
            .runtime
            .block_on(call.send())
            .map_err(|error| refusal(error.code()))?;
        let credentials = output
            .credentials
            .ok_or_else(|| anyhow!("AWS STS returned no credentials"))?;
        Ok(Session {
            expiration: u64::try_from(credentials.expiration.secs())
                .map_err(|_| anyhow!("AWS STS returned an invalid expiry"))?,
            access_key_id: credentials.access_key_id,
            secret_access_key: Zeroizing::new(credentials.secret_access_key),
            session_token: Zeroizing::new(credentials.session_token),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    const ROLE: &str = "arn:aws:iam::111122223333:role/ow-box/reader";

    /// A loopback HTTP/1.1 server: `answer(method and path, headers, body)` gives status and
    /// body; every request is recorded.
    fn server(
        answer: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 8192];
                let (head, body) = loop {
                    let n = stream.read(&mut buffer).unwrap();
                    if n == 0 {
                        break (String::new(), String::new());
                    }
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break (head.to_owned(), body.to_owned());
                        }
                    }
                };
                if head.is_empty() {
                    continue;
                }
                let line = head.lines().next().unwrap().to_owned();
                let (status, reply) = answer(&line, &head, &body);
                record.lock().unwrap().push((line, head, body));
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nx-aws-ec2-metadata-token-ttl-seconds: 21600\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        (url, seen)
    }

    /// Enough of EC2's IMDSv2 for the SDK's instance role provider.
    fn fake_imds() -> String {
        server(|line, head, _| {
            let token = head.to_ascii_lowercase().contains("x-aws-ec2-metadata-token: t0k3n");
            match line.split(' ').take(2).collect::<Vec<_>>()[..] {
                ["PUT", "/latest/api/token"] => (200, "t0k3n".into()),
                _ if !token => (401, String::new()),
                ["GET", "/latest/dynamic/instance-identity/document"] => (
                    200,
                    r#"{"accountId":"111122223333","region":"eu-west-1","availabilityZone":"eu-west-1b"}"#.into(),
                ),
                ["GET", "/latest/meta-data/iam/security-credentials/"] => (200, "ow-host".into()),
                ["GET", "/latest/meta-data/iam/security-credentials/ow-host"] => (
                    200,
                    r#"{"Code":"Success","LastUpdated":"2026-10-21T00:00:00Z","Type":"AWS-HMAC","AccessKeyId":"ASIAFROMIMDS","SecretAccessKey":"imds-secret","Token":"imds-token","Expiration":"2099-01-01T00:00:00Z"}"#.into(),
                ),
                _ => (404, String::new()),
            }
        })
        .0
    }

    fn form(body: &str) -> std::collections::BTreeMap<String, String> {
        body.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| {
                let mut out = Vec::new();
                let bytes = v.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'%' {
                        out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                        i += 3;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                (k.to_owned(), String::from_utf8(out).unwrap())
            })
            .collect()
    }

    #[test]
    fn assumes_with_instance_role_tags_and_source_identity() {
        let (sts, seen) = server(|_, _, body| {
            if body.contains("denied") {
                // AWS names the caller and the role in the message: never shown.
                return (403, "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>User: arn:aws:sts::111122223333:assumed-role/ow-host/i-0 is not authorized to perform: sts:AssumeRole on resource: arn:aws:iam::111122223333:role/ow-box/denied</Message></Error><RequestId>1</RequestId></ErrorResponse>".into());
            }
            (200, "<AssumeRoleResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleResult><Credentials><AccessKeyId>ASIABOX</AccessKeyId><SecretAccessKey>box-secret</SecretAccessKey><SessionToken>box-session</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::111122223333:assumed-role/reader/ow-1</Arn><AssumedRoleId>AROA:ow-1</AssumedRoleId></AssumedRoleUser><SourceIdentity>ow-1</SourceIdentity></AssumeRoleResult><ResponseMetadata><RequestId>1</RequestId></ResponseMetadata></AssumeRoleResponse>".into())
        });
        let client = Sts::build(Some(&fake_imds()), Some(&sts)).unwrap();
        assert!(
            client.host()
                == &Host {
                    account: "111122223333".into(),
                    region: "eu-west-1".into(),
                    zone: Some("eu-west-1b".into()),
                }
        );
        let session = client
            .assume(&Request {
                role_arn: ROLE,
                session: "ow-0123456789abcdef0123456789abcdef",
                box_id: "m-0123",
                owner: "Alice@Example.test",
            })
            .unwrap();
        assert_eq!(session.access_key_id, "ASIABOX");
        assert_eq!(session.secret_access_key.as_str(), "box-secret");
        assert_eq!(session.session_token.as_str(), "box-session");
        assert_eq!(session.expiration, 4070908800);
        let (_, head, body) = seen.lock().unwrap().remove(0);
        let form = form(&body);
        assert_eq!(form["Action"], "AssumeRole");
        assert_eq!(form["RoleArn"], ROLE);
        assert_eq!(
            form["RoleSessionName"],
            "ow-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            form["SourceIdentity"],
            "ow-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(form["DurationSeconds"], "3600");
        assert_eq!(form["Tags.member.1.Key"], "ow-box");
        assert_eq!(form["Tags.member.1.Value"], "m-0123");
        assert_eq!(form["Tags.member.2.Key"], "ow-owner");
        assert_eq!(form["Tags.member.2.Value"], "alice@example.test");
        assert_eq!(form["TransitiveTagKeys.member.1"], "ow-box");
        assert_eq!(form["TransitiveTagKeys.member.2"], "ow-owner");
        // Signed as the instance role from instance metadata, in its region.
        let head = head.to_ascii_lowercase();
        assert!(head.contains("credential=asiafromimds/"), "{head}");
        assert!(head.contains("/eu-west-1/sts/aws4_request"), "{head}");
        assert!(head.contains("x-amz-security-token: imds-token"), "{head}");
        // Refusals name the AWS error code only.
        let denied = client
            .assume(&Request {
                role_arn: "arn:aws:iam::111122223333:role/ow-box/denied",
                session: "ow-1",
                box_id: "m-0123",
                owner: "alice@example.test",
            })
            .err()
            .unwrap();
        assert_eq!(
            denied.to_string(),
            "AWS STS refused the role session (AccessDenied)"
        );
        // Another account is refused before any AWS call.
        let before = seen.lock().unwrap().len();
        let foreign = client
            .assume(&Request {
                role_arn: "arn:aws:iam::444455556666:role/ow-box/reader",
                session: "ow-1",
                box_id: "m-0123",
                owner: "alice@example.test",
            })
            .err()
            .unwrap();
        assert_eq!(
            foreign.to_string(),
            "the role is outside the controller's AWS account"
        );
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    #[test]
    fn without_instance_metadata_it_says_so() {
        let error = Sts::build(Some("http://127.0.0.1:9"), Some("http://127.0.0.1:9"))
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("metadata mode needs a controller on EC2")
        );
    }
}
