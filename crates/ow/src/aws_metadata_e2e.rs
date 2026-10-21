//! End to end, as root in a disposable Linux container (CI runs it privileged in Ubuntu):
//! the worker's real network policy and metadata server in a fresh network namespace, two
//! guest namespaces on `ow1`/`ow2` standing in for Firecracker TAPs, a stand-in for the
//! runner's real instance metadata behind `tap-egress`, and the catalog with the worker's
//! own `aws-creds` handler. A box reads its attached role's credentials from
//! 169.254.169.254 through curl and botocore, and loses them the moment its role is
//! detached or the box is transferred.
use super::metadata::{ADMIN_ONLY, FakeSts, READER, metadata_config};
use super::*;
use std::{
    io::Write as _,
    net::TcpListener,
    os::fd::AsRawFd,
    process::{Command, Output},
    time::Instant,
};

const NS: &str = "owe2e";

fn sh(command: &str) -> Output {
    Command::new("sh").arg("-c").arg(command).output().unwrap()
}

fn ok(command: &str) -> String {
    let out = sh(command);
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A command inside guest `i`'s network namespace, with nothing of ours in its environment.
fn guest(i: u32, command: &str) -> Output {
    sh(&format!(
        "ip netns exec {NS}-g{i} env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin sh -c {}",
        common::shell_quote(command)
    ))
}

/// curl's exit code: 0 answered, 7 refused, 28 timed out (dropped).
fn curl(i: u32, arguments: &str) -> (i32, String) {
    let out = guest(i, &format!("curl -s --max-time 2 {arguments}"));
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A metadata GET from guest `i` with a fresh IMDSv2 token: (HTTP status, body).
fn metadata(i: u32, path: &str) -> (u16, String) {
    let out = guest(
        i,
        &format!(
            "t=$(curl -sf --max-time 2 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token) || exit 9; \
             curl -s --max-time 2 -w '\\n%{{http_code}}' -H \"X-aws-ec2-metadata-token: $t\" http://169.254.169.254{path}"
        ),
    );
    assert!(
        out.status.success(),
        "metadata from guest {i}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let (body, status) = text.rsplit_once('\n').unwrap();
    (status.parse().unwrap(), body.to_owned())
}

/// Answer one HTTP request with `text`, after reading it (closing unread would reset).
fn reply(mut stream: std::net::TcpStream, text: &str) {
    use std::io::Read;
    let mut request = Vec::new();
    let mut byte = [0u8];
    while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
        request.push(byte[0]);
    }
    let _ = write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{text}",
        text.len()
    );
}

/// Serve `answer` on `port` from inside network namespace `name`.
fn listen_in(name: &'static str, port: u16, answer: &'static str) {
    std::thread::spawn(move || {
        let ns = std::fs::File::open(format!("/run/netns/{name}")).unwrap();
        assert_eq!(
            unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) },
            0
        );
        let listener = TcpListener::bind(("0.0.0.0", port)).unwrap();
        for stream in listener.incoming() {
            reply(stream.unwrap(), answer);
        }
    });
}

/// What botocore (the AWS CLI's credential chain) resolves in guest 1: (method, access key).
fn botocore(home: &Path, profile: Option<&str>) -> (String, String) {
    let profile = profile.map_or(String::new(), |p| format!("AWS_PROFILE={p} "));
    let out = guest(
        1,
        &format!(
            "HOME={} AWS_EC2_METADATA_SERVICE_NUM_ATTEMPTS=1 {profile}python3 -c \"import botocore.session as b; c=b.get_session().get_credentials(); print(c.method if c else 'none', c.access_key if c else '-')\"",
            home.display()
        ),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let (method, key) = text.trim().split_once(' ').unwrap();
    (method.into(), key.into())
}

#[test]
#[ignore = "root in a disposable Linux container with nft, iproute2, curl and python3-botocore"]
fn a_box_reads_its_role_at_the_metadata_address_and_loses_it_on_detach_and_transfer() {
    // This thread becomes the worker's fresh network namespace; threads and processes it
    // starts stay in it, the rest of the test binary does not.
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWNET) },
        0,
        "needs root"
    );
    ok("ip link set lo up");
    ow_core::network::initialize().unwrap();
    // The runner's uplink: slirp's side holds the real metadata address and a public host.
    let _ = sh(&format!(
        "ip netns del {NS}-egress; ip netns del {NS}-g1; ip netns del {NS}-g2"
    ));
    ok(&format!(
        "ip netns add {NS}-egress && ip link add tap-egress type veth peer name eth0 netns {NS}-egress \
         && ip addr add 10.0.2.100/24 dev tap-egress && ip link set tap-egress up \
         && ip netns exec {NS}-egress sh -c 'ip link set lo up && ip link set eth0 up && ip addr add 10.0.2.2/24 dev eth0 \
            && ip addr add 169.254.169.254/32 dev lo && ip addr add 203.0.113.10/32 dev lo'"
    ));
    ow_core::network::enable(&json!([])).unwrap();
    listen_in("owe2e-egress", 80, "REAL-IMDS");
    listen_in("owe2e-egress", 8080, "REAL-IMDS");
    listen_in("owe2e-egress", 8081, "INTERNET");
    // Two boxes, as `Vm::spawn` wires them: ow{i} with the host's .1 and the guest's .2.
    const BOXES: [&str; 2] = ["m-e2e-meta-1", "m-e2e-meta-2"];
    for i in 1..=2u32 {
        ok(&format!(
            "ip netns add {NS}-g{i} && ip link add ow{i} type veth peer name eth0 netns {NS}-g{i} \
             && ip addr add 198.18.0.{}/30 dev ow{i} && ip link set ow{i} up \
             && ip netns exec {NS}-g{i} sh -c 'ip link set lo up && ip link set eth0 up \
                && ip addr add 198.18.0.{}/30 dev eth0 && ip route add default via 198.18.0.{}'",
            4 * i + 1,
            4 * i + 2,
            4 * i + 1
        ));
        ow_core::network::register(i).unwrap();
        ow_core::metadata::attach(i, BOXES[i as usize - 1]).unwrap();
    }
    ow_core::metadata::start().unwrap();

    let f = Fixture::new();
    f.lists.store(true, Ordering::SeqCst);
    f.configure(metadata_config());
    let shared = Arc::new(Mutex::new(Catalog::open(&f.root, "test", None).unwrap()));
    let mut sts = FakeSts::new(&shared);
    let mut cleared = HashSet::new();
    let (a, b) = {
        let mut c = shared.lock().unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        let b = c.user(&identity("b", "b@example.test")).unwrap();
        running(&c, a, "one", BOXES[0]);
        running(&c, a, "two", BOXES[1]);
        (a, b)
    };
    const CREDENTIALS: &str = "/latest/meta-data/iam/security-credentials/";

    // No role: an immediate 404, so SDKs move on instead of waiting for a timeout.
    let started = Instant::now();
    assert_eq!(metadata(1, CREDENTIALS).0, 404);
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "{:?}",
        started.elapsed()
    );

    let python = sh("python3 -c 'import botocore'").status.success();
    if std::env::var("OW_E2E_REQUIRE_BOTOCORE").as_deref() == Ok("1") {
        assert!(python, "python3-botocore is required");
    }
    let home = f.root.join("home");
    std::fs::create_dir_all(home.join(".aws")).unwrap();
    // Attach through the CLI/dashboard handler with STS slower than the 800 ms IMDS hold.
    // No refresher pass or guest retry may be needed after the successful response.
    sts.during = Some(Box::new(|_| std::thread::sleep(Duration::from_secs(2))));
    let started = Instant::now();
    let attached = crate::aws_metadata::role(
        &shared,
        a,
        &json!({"op":"aws-role","id":"one","role_arn":READER}),
        false,
        &mut sts,
    )
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(attached["aws_role"]["state"], "active");
    sts.during = None;
    if python {
        assert_eq!(
            botocore(&home, None),
            ("iam-role".into(), "ASIAFAKE1".into())
        );
    }
    assert_eq!(metadata(1, CREDENTIALS), (200, "reader".into()));
    let (status, body) = metadata(1, &format!("{CREDENTIALS}reader"));
    assert_eq!(status, 200);
    let credentials: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(credentials["AccessKeyId"], "ASIAFAKE1");
    assert_eq!(credentials["Type"], "AWS-HMAC");
    assert_eq!(
        metadata(1, "/latest/meta-data/placement/region").1,
        "us-east-1"
    );
    // Box 2 has none, and box 1's token is refused from box 2.
    assert_eq!(metadata(2, CREDENTIALS).0, 404);
    let token = guest(
        1,
        "curl -sf --max-time 2 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token",
    );
    let token = String::from_utf8(token.stdout).unwrap();
    let (_, code) = curl(
        2,
        &format!(
            "-o /dev/null -w '%{{http_code}}' -H 'X-aws-ec2-metadata-token: {token}' http://169.254.169.254{CREDENTIALS}reader"
        ),
    );
    assert_eq!(code, "401");
    // IMDSv1 is refused.
    assert_eq!(
        curl(
            1,
            &format!("-o /dev/null -w '%{{http_code}}' http://169.254.169.254{CREDENTIALS}")
        )
        .1,
        "401"
    );

    // Box 1 posing as box 2 (box 2's address) is dropped before it reaches anything.
    ok(&format!(
        "ip netns exec {NS}-g1 ip addr add 198.18.0.10/32 dev eth0"
    ));
    let (spoofed, _) = curl(
        1,
        "--interface 198.18.0.10 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token",
    );
    assert_eq!(spoofed, 28, "a spoofed source is dropped");
    ok(&format!(
        "ip netns exec {NS}-g1 ip addr del 198.18.0.10/32 dev eth0"
    ));

    // The guest agent installer's ephemeral port on the box's own host address still works;
    // another box cannot reach it.
    let installer = TcpListener::bind("198.18.0.5:0").unwrap();
    let port = installer.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in installer.incoming() {
            reply(stream.unwrap(), "AGENT");
        }
    });
    assert_eq!(
        curl(1, &format!("http://198.18.0.5:{port}/")),
        (0, "AGENT".into())
    );
    assert_eq!(curl(2, &format!("http://198.18.0.5:{port}/")).0, 28);

    // The runner's real metadata service stays out of reach, while the Internet does not;
    // port 80 at the metadata address is answered by the worker, never forwarded.
    assert_eq!(curl(1, "http://203.0.113.10:8081/"), (0, "INTERNET".into()));
    assert_eq!(curl(1, "http://169.254.169.254:8080/").0, 28);
    assert!(
        !curl(1, "http://169.254.169.254/latest/api/token")
            .1
            .contains("REAL-IMDS")
    );
    // The listener is closed to the uplink side.
    let uplink = sh(&format!(
        "ip netns exec {NS}-egress curl -s --max-time 2 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://10.0.2.100:{}/latest/api/token",
        ow_core::metadata::PORT
    ));
    assert_eq!(uplink.status.code(), Some(28));
    // IPv6 from a guest (EC2's fd00:ec2::254 included) is dropped by the address binding.
    ok(&format!(
        "ip -6 addr add fd00::1/64 dev ow1 nodad && ip -6 addr add fd00:ec2::254/128 dev lo \
         && ip netns exec {NS}-g1 sh -c 'ip -6 addr add fd00::2/64 dev eth0 nodad && ip -6 route add default via fd00::1'"
    ));
    let v6 = TcpListener::bind("[::]:8169").unwrap();
    let reached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = reached.clone();
    std::thread::spawn(move || {
        for stream in v6.incoming() {
            count.fetch_add(1, Ordering::SeqCst);
            reply(stream.unwrap(), "V6");
        }
    });
    // Neighbour discovery is dropped too: no connection (7) or a timeout (28), never an answer.
    for target in [
        "[fd00:ec2::254]:8169",
        "[fd00::1]:8169",
        "[fd00:ec2::254]:80",
    ] {
        let (code, _) = curl(1, &format!("-g -6 http://{target}/"));
        assert!(matches!(code, 7 | 28), "{target}: curl exit {code}");
    }
    assert_eq!(reached.load(Ordering::SeqCst), 0);
    // Control: the same listener answers over IPv6 inside the worker's namespace.
    assert!(ok("curl -s --max-time 2 -g -6 http://[fd00:ec2::254]:8169/").contains("V6"));

    // The SDK credential chain (botocore, which the AWS CLI uses), one IMDS attempt as the
    // CLI makes: the role when the user has nothing, the user's own keys and profiles first.
    if python {
        assert_eq!(
            botocore(&home, None),
            ("iam-role".into(), "ASIAFAKE1".into())
        );
        std::fs::write(
            home.join(".aws/credentials"),
            "[default]\naws_access_key_id = AKIAUSERDEFAULT\naws_secret_access_key = user\n[x]\naws_access_key_id = AKIAUSERPROFILEX\naws_secret_access_key = user\n",
        )
        .unwrap();
        assert_eq!(
            botocore(&home, None),
            ("shared-credentials-file".into(), "AKIAUSERDEFAULT".into())
        );
        assert_eq!(
            botocore(&home, Some("x")),
            ("shared-credentials-file".into(), "AKIAUSERPROFILEX".into())
        );
        std::fs::remove_file(home.join(".aws/credentials")).unwrap();
        // A wake starts a new VMM (new token nonce) and keeps the credentials: the first
        // single-attempt lookup after it succeeds.
        ow_core::metadata::detach(1);
        ow_core::metadata::attach(1, BOXES[0]).unwrap();
        assert_eq!(
            botocore(&home, None),
            ("iam-role".into(), "ASIAFAKE1".into())
        );
    }

    // Retain a token from this VMM incarnation across a real detach/re-attach.
    let token = guest(
        1,
        "curl -sf --max-time 2 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token",
    );
    assert!(token.status.success());
    let token = String::from_utf8(token.stdout).unwrap();
    let replay = format!(
        "-o /dev/null -w '%{{http_code}}' -H 'X-aws-ec2-metadata-token: {token}' http://169.254.169.254{CREDENTIALS}"
    );
    assert_eq!(curl(1, &replay).1, "200");
    // Detach: gone at once, without waiting for a refresher pass.
    crate::aws_metadata::role(
        &shared,
        a,
        &json!({"op":"aws-role","id":"one"}),
        false,
        &mut sts,
    )
    .unwrap();
    let started = Instant::now();
    assert_eq!(metadata(1, CREDENTIALS).0, 404);
    assert!(started.elapsed() < Duration::from_millis(700));
    assert_eq!(curl(1, &replay).1, "401");
    if python {
        assert_eq!(botocore(&f.root.join("home"), None).0, "none");
    }

    // Re-attach, then transfer to another owner: gone the moment the transfer returns.
    crate::aws_metadata::role(
        &shared,
        a,
        &json!({"op":"aws-role","id":"one","role_arn":ADMIN_ONLY}),
        false,
        &mut sts,
    )
    .unwrap();
    assert_eq!(
        curl(1, &replay).1,
        "401",
        "withdrawn tokens must stay revoked"
    );
    assert_eq!(metadata(1, CREDENTIALS), (200, "a-only".into()));
    let (status, body) = metadata(1, &format!("{CREDENTIALS}a-only"));
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["AccessKeyId"],
        "ASIAFAKE2"
    );
    shared
        .lock()
        .unwrap()
        .operation(a, json!({"op":"transfer","id":"one","to":"b@example.test"}))
        .unwrap();
    assert_eq!(metadata(1, CREDENTIALS).0, 404);
    // The new owner's next pass assumes nothing for it.
    pass(&shared, &mut sts, &mut cleared);
    assert_eq!(metadata(1, CREDENTIALS).0, 404);
    let _ = b;

    for i in 1..=2u32 {
        ow_core::metadata::detach(i);
        ow_core::metadata::clear(BOXES[i as usize - 1]);
    }
    let _ = sh(&format!(
        "ip netns del {NS}-egress; ip netns del {NS}-g1; ip netns del {NS}-g2"
    ));
}
