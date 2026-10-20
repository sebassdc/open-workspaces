//! The files a worker writes into a guest drive the standard AWS SDK credential chain to
//! `AssumeRoleWithWebIdentity`, here against a loopback mock STS. With
//! `OW_WEB_IDENTITY_GUEST=1` (CI runs it as root in an Ubuntu container with a `dev` user and
//! `/etc/ow-developer`) the files go to their real guest paths and the variables are read back
//! through dev's login shell, as `ow exec` and SSH sessions see them.
use aws_config::{BehaviorVersion, Region, provider_config::ProviderConfig};
use aws_credential_types::provider::ProvideCredentials;
use ow_core::web_identity::{Delivery, install_command};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{Arc, Mutex},
};

const ROLE: &str = "arn:aws:iam::111122223333:role/open-workspaces/box-reader";
const SESSION: &str = "ow-0123456789abcdef0123456789abcdef";
const FIRST: &str = "eyJhbGciOiJFUzI1NiIsImtpZCI6ImEifQ.eyJzdWIiOiJmaXJzdCJ9.c2lnLWZpcnN0";
const SECOND: &str = "eyJhbGciOiJFUzI1NiIsImtpZCI6ImEifQ.eyJzdWIiOiJzZWNvbmQifQ.c2lnLXNlY29uZA";

/// Answers every request with synthetic credentials and records the form bodies it got.
fn mock_sts() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 8192];
            let body = loop {
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0, "connection closed mid-request");
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
                        break body.to_owned();
                    }
                }
            };
            let n = {
                let mut bodies = seen.lock().unwrap();
                bodies.push(body);
                bodies.len()
            };
            let xml = format!(
                "<AssumeRoleWithWebIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleWithWebIdentityResult>\
                 <Credentials><AccessKeyId>ASIASYNTHETIC{n}</AccessKeyId><SecretAccessKey>synthetic-secret</SecretAccessKey>\
                 <SessionToken>synthetic-session</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials>\
                 <SubjectFromWebIdentityToken>owner/a@example.test/box/b</SubjectFromWebIdentityToken>\
                 <AssumedRoleUser><Arn>arn:aws:sts::111122223333:assumed-role/box-reader/{SESSION}</Arn><AssumedRoleId>AROASYNTHETIC:{SESSION}</AssumedRoleId></AssumedRoleUser>\
                 </AssumeRoleWithWebIdentityResult><ResponseMetadata><RequestId>{n}</RequestId></ResponseMetadata></AssumeRoleWithWebIdentityResponse>"
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{xml}",
                xml.len()
            )
            .unwrap();
        }
    });
    (url, bodies)
}

fn form(body: &str) -> BTreeMap<String, String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| {
            let v = v.replace('+', " ");
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

fn sh(command: &str) -> String {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn guest_files_drive_the_sdk_chain_to_assume_role_with_web_identity() {
    let guest = std::env::var("OW_WEB_IDENTITY_GUEST").as_deref() == Ok("1");
    let root = if guest {
        String::new()
    } else {
        let dir =
            std::env::temp_dir().join(format!("ow-chain-{}", ow_core::common::nonce().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_str().unwrap().to_owned()
    };
    let deliver = |token| {
        let delivery = Delivery {
            token,
            role_arn: ROLE,
            session_name: SESSION,
        };
        sh(&install_command(&root, Some(&delivery)).unwrap());
    };
    deliver(FIRST);
    // What a shell in the box sees: dev's login shell in a guest, the env file otherwise.
    let shown = if guest {
        let out = Command::new("su")
            .args(["-", "dev", "-c", "env"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    } else {
        sh(&format!("set -a; . {root}/run/ow/aws/env; set +a; env"))
    };
    let mut vars: BTreeMap<String, String> = shown
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.starts_with("AWS_"))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    assert_eq!(vars.len(), 3, "{vars:?}");
    assert_eq!(vars["AWS_ROLE_ARN"], ROLE);
    assert_eq!(vars["AWS_ROLE_SESSION_NAME"], SESSION);
    assert_eq!(vars["AWS_WEB_IDENTITY_TOKEN_FILE"], "/run/ow/aws/token");
    if guest {
        // dev reads the token it is given; nobody else on the guest can.
        let read = Command::new("su")
            .args(["-", "dev", "-c", "cat /run/ow/aws/token"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(read.stdout).unwrap(), FIRST);
        // Images without a dev user (Alpine): the worker's exact `ow exec` wrapper, as root.
        #[cfg(target_os = "linux")]
        {
            std::fs::remove_file("/etc/ow-developer").unwrap();
            let root_env = sh(&ow_core::runtime::exec_command("env"));
            std::fs::write("/etc/ow-developer", "").unwrap();
            for (key, value) in &vars {
                assert!(
                    root_env.lines().any(|l| l == format!("{key}={value}")),
                    "{key} missing from a root ow exec: {root_env}"
                );
            }
        }
    } else {
        vars.insert(
            "AWS_WEB_IDENTITY_TOKEN_FILE".into(),
            format!("{root}/run/ow/aws/token"),
        );
    }
    let (sts, bodies) = mock_sts();
    // This test binary is its own process and runs only this test: the environment is ours.
    unsafe {
        for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("AWS_")) {
            std::env::remove_var(key);
        }
        for (key, value) in &vars {
            std::env::set_var(key, value);
        }
        std::env::set_var("AWS_ENDPOINT_URL_STS", &sts);
        std::env::set_var("AWS_REGION", "us-east-1");
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
        std::env::set_var("AWS_CONFIG_FILE", "/nonexistent/ow-config");
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", "/nonexistent/ow-credentials");
    }
    // Plain HTTP to the loopback mock: no CA roots needed in a bare guest container.
    let http = aws_smithy_http_client::Builder::new().build_http();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // The SDK's default chain, as any program in the box would load it.
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .http_client(http.clone())
            .load()
            .await;
        let credentials = shared
            .credentials_provider()
            .unwrap()
            .provide_credentials()
            .await
            .unwrap();
        assert_eq!(credentials.access_key_id(), "ASIASYNTHETIC1");
        // A refreshed token file is what the next STS call presents.
        deliver(SECOND);
        let provider =
            aws_config::web_identity_token::WebIdentityTokenCredentialsProvider::builder()
                .configure(
                    &ProviderConfig::without_region()
                        .with_http_client(http)
                        .with_region(Some(Region::new("us-east-1"))),
                )
                .build();
        let credentials = provider.provide_credentials().await.unwrap();
        assert_eq!(credentials.access_key_id(), "ASIASYNTHETIC2");
    });
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    for (body, token) in bodies.iter().zip([FIRST, SECOND]) {
        let form = form(body);
        assert_eq!(form["Action"], "AssumeRoleWithWebIdentity");
        assert_eq!(form["RoleArn"], ROLE);
        assert_eq!(form["RoleSessionName"], SESSION);
        assert_eq!(form["WebIdentityToken"], token);
    }
    // Detaching the role removes what the chain found.
    sh(&install_command(&root, None).unwrap());
    assert!(!std::path::Path::new(&format!("{root}/run/ow/aws")).exists());
    if !guest {
        std::fs::remove_dir_all(root).unwrap();
    }
}
