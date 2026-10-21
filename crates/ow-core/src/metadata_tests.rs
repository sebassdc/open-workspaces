use super::*;

const ROLE: &str = "arn:aws:iam::111122223333:role/ow-box/reader";

/// A listener that answers as TAP `index`; tests use distinct indexes and box IDs because
/// the state is the worker's process-wide one.
fn listener(index: u32) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    serve(listener, Arc::new(move |_, _| Some(index)));
    port
}

fn credentials(n: u32) -> Credentials {
    Credentials {
        access_key_id: format!("ASIATEST{n}"),
        secret_access_key: Zeroizing::new(format!("secret{n}+/=")),
        session_token: Zeroizing::new(format!("session{n}")),
        expiration: unix() + 3600,
        issued: unix(),
    }
}

fn role(credentials: Option<Credentials>) -> Role {
    Role {
        role_arn: ROLE.into(),
        generation: 1,
        hop_limit: 1,
        region: Some("us-east-1".into()),
        zone: Some("us-east-1a".into()),
        credentials,
    }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

fn exchange(stream: &mut TcpStream, raw: &str) -> Option<Reply> {
    stream.write_all(raw.as_bytes()).ok()?;
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while !buffer.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        buffer.push(byte[0]);
    }
    let head = String::from_utf8(buffer).unwrap();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    Some(Reply {
        status: head[9..12].parse().unwrap(),
        head,
        body: String::from_utf8(body).unwrap(),
    })
}

fn send(port: u16, raw: &str) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    exchange(&mut stream, raw).expect("an answer")
}

fn token(port: u16) -> String {
    let reply = send(
        port,
        "PUT /latest/api/token HTTP/1.1\r\nHost: 169.254.169.254\r\nX-aws-ec2-metadata-token-ttl-seconds: 21600\r\n\r\n",
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply
            .head
            .contains("X-aws-ec2-metadata-token-ttl-seconds: 21600\r\n")
    );
    reply.body
}

fn get(port: u16, path: &str, token: &str) -> Reply {
    send(
        port,
        &format!("GET {path} HTTP/1.1\r\nX-aws-ec2-metadata-token: {token}\r\n\r\n"),
    )
}

#[test]
fn hmac_matches_rfc_4231() {
    assert_eq!(
        hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn timestamps_are_utc_iso_8601() {
    assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
    assert_eq!(timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
}

#[test]
fn only_a_guest_and_its_own_host_address_name_a_box() {
    let at = |peer: &str, local: &str| peer_index(peer.parse().unwrap(), local.parse().unwrap());
    assert_eq!(at("198.18.0.6:5000", "198.18.0.5:9169"), Some(1));
    assert_eq!(at("198.18.3.10:5000", "198.18.3.9:9169"), Some(3 * 64 + 2));
    // Another box's host address, a host address as peer, other ranges, IPv6.
    assert_eq!(at("198.18.0.6:5000", "198.18.0.9:9169"), None);
    assert_eq!(at("198.18.0.5:5000", "198.18.0.4:9169"), None);
    assert_eq!(at("10.0.2.100:5000", "10.0.2.99:9169"), None);
    assert_eq!(at("198.19.0.6:5000", "198.19.0.5:9169"), None);
    assert_eq!(at("[fd00::2]:5000", "[fd00::1]:9169"), None);
}

#[test]
fn imdsv2_only_without_cors_forwarding_or_bodies() {
    let port = listener(9001);
    attach(9001, "m-v2").unwrap();
    set("m-v2", role(Some(credentials(1))));
    // A token needs its TTL header, within EC2's bounds.
    for ttl in [
        "",
        "X-aws-ec2-metadata-token-ttl-seconds: 0\r\n",
        "X-aws-ec2-metadata-token-ttl-seconds: 21601\r\n",
    ] {
        let reply = send(
            port,
            &format!("PUT /latest/api/token HTTP/1.1\r\n{ttl}\r\n"),
        );
        assert_eq!(reply.status, 400, "{ttl}");
    }
    let token = token(port);
    // IMDSv1 (no token) and wrong tokens are refused.
    let v1 = send(
        port,
        "GET /latest/meta-data/iam/security-credentials/ HTTP/1.1\r\n\r\n",
    );
    assert_eq!(v1.status, 401);
    assert_eq!(
        get(port, "/latest/meta-data/iam/security-credentials/", "1.00").status,
        401
    );
    let forged = format!("{}0", &token[..token.len() - 1]);
    let forged = if forged == token {
        format!("{}1", &token[..token.len() - 1])
    } else {
        forged
    };
    assert_eq!(
        get(port, "/latest/meta-data/iam/security-credentials/", &forged).status,
        401
    );
    // An expired token: the MAC is right, the time is past.
    let expired = mint(
        &Peer {
            id: "m-v2".into(),
            nonce: state().peers[&9001].nonce.clone(),
        },
        unix() - 1,
    );
    assert_eq!(
        get(
            port,
            "/latest/meta-data/iam/security-credentials/",
            &expired
        )
        .status,
        401
    );
    // X-Forwarded-For is refused on every method, as real IMDS does.
    let forwarded = send(
        port,
        "PUT /latest/api/token HTTP/1.1\r\nX-Forwarded-For: 10.0.0.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n",
    );
    assert_eq!(forwarded.status, 403);
    let forwarded = send(
        port,
        &format!(
            "GET /latest/meta-data/iam/security-credentials/ HTTP/1.1\r\nX-Forwarded-For: 10.0.0.1\r\nX-aws-ec2-metadata-token: {token}\r\n\r\n"
        ),
    );
    assert_eq!(forwarded.status, 403);
    // No preflight and no CORS headers, ever.
    let options = send(
        port,
        "OPTIONS /latest/api/token HTTP/1.1\r\nOrigin: http://evil.test\r\nAccess-Control-Request-Method: PUT\r\n\r\n",
    );
    assert_eq!(options.status, 405);
    let get_token = send(
        port,
        "GET /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n",
    );
    assert_eq!(get_token.status, 405);
    let ok = get(port, "/latest/meta-data/iam/security-credentials/", &token);
    for reply in [&options, &get_token, &ok, &forwarded] {
        assert!(
            !reply.head.to_ascii_lowercase().contains("access-control"),
            "{}",
            reply.head
        );
    }
    // A request body is never accepted.
    let body = send(
        port,
        "PUT /latest/api/token HTTP/1.1\r\nContent-Length: 4\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\nabcd",
    );
    assert_eq!(body.status, 400);
    clear("m-v2");
    detach(9001);
}

#[test]
fn credentials_have_the_ec2_shape_and_stay_with_their_box() {
    let (a, b) = (listener(9002), listener(9003));
    attach(9002, "m-shape-a").unwrap();
    attach(9003, "m-shape-b").unwrap();
    set("m-shape-a", role(Some(credentials(2))));
    let token_a = token(a);
    assert_eq!(
        get(a, "/latest/meta-data/iam/security-credentials", &token_a).body,
        "reader"
    );
    let listing = get(a, "/latest/meta-data/iam/security-credentials/", &token_a);
    assert_eq!((listing.status, listing.body.as_str()), (200, "reader"));
    let reply = get(
        a,
        "/latest/meta-data/iam/security-credentials/reader",
        &token_a,
    );
    assert_eq!(reply.status, 200);
    let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(json["Code"], "Success");
    assert_eq!(json["Type"], "AWS-HMAC");
    assert_eq!(json["AccessKeyId"], "ASIATEST2");
    assert_eq!(json["SecretAccessKey"], "secret2+/=");
    assert_eq!(json["Token"], "session2");
    let expiration = json["Expiration"].as_str().unwrap();
    assert_eq!(expiration.len(), 20);
    assert!(expiration.ends_with('Z') && json["LastUpdated"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        get(
            a,
            "/latest/meta-data/iam/security-credentials/other",
            &token_a
        )
        .status,
        404
    );
    let info: serde_json::Value =
        serde_json::from_str(&get(a, "/latest/meta-data/iam/info", &token_a).body).unwrap();
    assert_eq!(
        info["InstanceProfileArn"],
        "arn:aws:iam::111122223333:instance-profile/ow-box/reader"
    );
    assert!(
        info["InstanceProfileId"]
            .as_str()
            .unwrap()
            .starts_with("AIPAOW")
    );
    assert_eq!(
        get(a, "/latest/meta-data/placement/region", &token_a).body,
        "us-east-1"
    );
    assert_eq!(
        get(a, "/latest/meta-data/placement/availability-zone", &token_a).body,
        "us-east-1a"
    );
    let document: serde_json::Value =
        serde_json::from_str(&get(a, "/latest/dynamic/instance-identity/document", &token_a).body)
            .unwrap();
    assert_eq!(
        document,
        serde_json::json!({"region":"us-east-1","availabilityZone":"us-east-1a"})
    );
    assert_eq!(
        get(a, "/latest/meta-data/instance-id", &token_a).status,
        404
    );
    assert_eq!(get(a, "/", &token_a).status, 404);
    // Box A's token presented from box B's address is refused; B has no role of its own.
    assert_eq!(
        get(
            b,
            "/latest/meta-data/iam/security-credentials/reader",
            &token_a
        )
        .status,
        401
    );
    let token_b = token(b);
    let started = Instant::now();
    assert_eq!(
        get(b, "/latest/meta-data/iam/security-credentials/", &token_b).status,
        404
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "no role answers at once"
    );
    // A new VMM of the box (a wake, a restart) gets a new nonce: old tokens stop working.
    attach(9002, "m-shape-a").unwrap();
    assert_eq!(
        get(a, "/latest/meta-data/iam/security-credentials/", &token_a).status,
        401
    );
    // Detach: the role is gone at once.
    let token_a = token(a);
    assert!(clear("m-shape-a"));
    assert_eq!(
        get(
            a,
            "/latest/meta-data/iam/security-credentials/reader",
            &token_a
        )
        .status,
        404
    );
    // A TAP without a VMM answers nothing.
    detach(9002);
    assert_eq!(
        get(a, "/latest/meta-data/iam/security-credentials/", &token_a).status,
        404
    );
    detach(9003);
}

#[test]
fn missing_credentials_are_awaited_briefly() {
    let port = listener(9004);
    attach(9004, "m-hold").unwrap();
    set("m-hold", role(None));
    let token = token(port);
    // Never arrive: 404 within the SDKs' one second.
    let started = Instant::now();
    assert_eq!(
        get(port, "/latest/meta-data/iam/security-credentials/", &token).status,
        404
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(700) && waited < Duration::from_millis(1000),
        "{waited:?}"
    );
    // Arrive while the request waits.
    let deliver = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        set("m-hold", role(Some(credentials(4))));
    });
    let started = Instant::now();
    let reply = get(port, "/latest/meta-data/iam/security-credentials/", &token);
    assert_eq!((reply.status, reply.body.as_str()), (200, "reader"));
    assert!(started.elapsed() < Duration::from_millis(700));
    deliver.join().unwrap();
    assert!(present("m-hold", 1) && !present("m-hold", 2));
    // A stop drops the secrets and keeps the wait.
    stopped("m-hold");
    assert!(!present("m-hold", 1));
    assert!(state().roles["m-hold"].credentials.is_none());
    clear("m-hold");
    detach(9004);
}

#[test]
fn connections_are_bounded_per_box() {
    let port = listener(9005);
    attach(9005, "m-limits").unwrap();
    // Keep-alive: several requests on one connection.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    for _ in 0..3 {
        let reply = exchange(
            &mut stream,
            "PUT /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n",
        )
        .unwrap();
        assert_eq!(reply.status, 200);
        assert!(reply.head.contains("Connection: keep-alive"));
    }
    let mut held = vec![stream];
    while held.len() < CONNECTIONS {
        held.push(TcpStream::connect(("127.0.0.1", port)).unwrap());
    }
    // Let the server count them before the one too many.
    for stream in &mut held[1..] {
        exchange(
            stream,
            "PUT /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n",
        )
        .unwrap();
    }
    let mut extra = TcpStream::connect(("127.0.0.1", port)).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert!(
        exchange(
            &mut extra,
            "PUT /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n"
        )
        .is_none()
    );
    drop(held);
    // Oversized headers end the connection.
    let mut big = TcpStream::connect(("127.0.0.1", port)).unwrap();
    big.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let padding = "X-Pad: ".to_owned() + &"a".repeat(HEADER_BYTES) + "\r\n";
    assert!(exchange(&mut big, &format!("GET / HTTP/1.1\r\n{padding}\r\n")).is_none());
    // A client that never finishes its headers is cut off after a second.
    let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let started = Instant::now();
    assert_eq!(slow.read(&mut [0; 16]).unwrap_or(0), 0);
    assert!(started.elapsed() < Duration::from_millis(2500));
    detach(9005);
}

#[test]
fn requests_are_rate_limited_per_box() {
    let port = listener(9006);
    attach(9006, "m-rate").unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut limited = 0;
    for _ in 0..(BURST as usize + 50) {
        let reply = exchange(
            &mut stream,
            "PUT /latest/api/token HTTP/1.1\r\nX-aws-ec2-metadata-token-ttl-seconds: 60\r\n\r\n",
        )
        .unwrap();
        if reply.status == 429 {
            limited += 1;
        }
    }
    assert!(limited > 0, "a flood is throttled");
    detach(9006);
}

#[test]
fn answers_carry_the_box_hop_limit() {
    use std::os::fd::AsRawFd;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    for hops in [1u8, 2] {
        hop_limit(&server, hops);
        let mut ttl: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        unsafe {
            libc::getsockopt(
                server.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_TTL,
                (&mut ttl as *mut libc::c_int).cast(),
                &mut size,
            );
        }
        assert_eq!(ttl, libc::c_int::from(hops));
    }
    drop(client);
}

#[test]
fn controller_values_are_validated() {
    assert!(role(Some(credentials(7))).validate().is_ok());
    let mut bad = role(Some(credentials(7)));
    bad.hop_limit = 3;
    assert!(bad.validate().is_err());
    let mut bad = role(None);
    bad.role_arn = "arn:aws:iam::111122223333:role/x;id".into();
    assert!(bad.validate().is_err());
    let mut bad = role(None);
    bad.region = Some("us-east-1\r\nX: y".into());
    assert!(bad.validate().is_err());
    let mut injected = credentials(7);
    injected.session_token = Zeroizing::new("a\"b".into());
    assert!(role(Some(injected)).validate().is_err());
}

#[test]
fn handled_requests_lose_their_secrets() {
    let mut request = serde_json::json!({"op":"aws-creds","access_key_id":"ASIAX","secret_access_key":"s3cret","session_token":"t0ken"});
    wipe(&mut request);
    assert_eq!(request["access_key_id"], "ASIAX");
    for key in ["secret_access_key", "session_token"] {
        assert!(!request[key].as_str().unwrap().contains(|c: char| c != '\0'));
    }
}
