#!/usr/bin/env python3
"""Real loopback TLS controller + Mac enrollment/worker/guest. Not live gateway acceptance."""

import argparse, base64, hashlib, json, os, pathlib, socket, ssl, subprocess, time, urllib.request, urllib.error, shutil, sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from importlib.machinery import SourceFileLoader

worker_api = SourceFileLoader(
    "hardware", str(pathlib.Path(__file__).with_name("test-mac-node.py"))
).load_module()
request = worker_api.request
POLICY = "Trusted shared private pool: all admitted workspace users may select this host. The host operator can inspect guest data and stop participation. This invitation grants node participation only, not human workspace management."


def private_json(p, v):
    p.write_text(json.dumps(v))
    os.chmod(p, 0o600)


def mkdir(p):
    p.mkdir(mode=0o700)
    os.chmod(p, 0o700)


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


from importlib.machinery import SourceFileLoader

receipt_api = SourceFileLoader(
    "receipt", str(pathlib.Path(__file__).with_name("mac-node-receipt.py"))
).load_module()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("assets", type=pathlib.Path)
    ap.add_argument(
        "--ow", type=pathlib.Path, default=pathlib.Path("target/mac-host/ow")
    )
    ap.add_argument("--linux-tests", type=pathlib.Path)
    a = ap.parse_args()
    root = a.root.resolve()
    assets = a.assets.resolve()
    ow = a.ow.resolve()
    assert not root.exists()
    mkdir(root)
    c = root / "c"
    n = root / "n"
    mkdir(c)
    mkdir(n)
    receipt = receipt_api.begin(root, ow, assets)
    checks = []
    children = []
    log = open(root / "transport.log", "ab")
    env = os.environ.copy()
    env["OW_MAC_ASSETS"] = str(assets)

    def run(data, *args, success=True):
        result = subprocess.run(
            [str(ow), "--data-dir", str(data), *args],
            capture_output=True,
            env=env,
            timeout=90,
        )
        log.write(result.stdout + result.stderr)
        log.flush()
        assert (result.returncode == 0) == success, (args, result.stderr.decode())
        return result

    def check(name, value):
        assert value, name
        checks.append(name)
        print("PASS", name, flush=True)

    def spawn(data, *args):
        p = subprocess.Popen(
            [str(ow), "--data-dir", str(data), *args],
            stdout=log,
            stderr=log,
            stdin=subprocess.DEVNULL,
            env=env,
        )
        children.append(p)
        return p

    def cert(prefix):
        ca_key = root / (prefix + "-ca.key")
        ca = root / (prefix + "-ca.pem")
        key = root / (prefix + ".key")
        pem = root / (prefix + ".pem")
        csr = root / (prefix + ".csr")
        ext = root / (prefix + ".cnf")
        ext.write_text(
            "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\nCN=Local Test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n[server]\nsubjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
        )

        def ssl_cmd(*args):
            subprocess.run(["openssl", *args], check=True, stdout=log, stderr=log)

        ssl_cmd(
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-keyout",
            str(ca_key),
            "-out",
            str(ca),
            "-config",
            str(ext),
        )
        ssl_cmd(
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(key),
            "-out",
            str(csr),
            "-subj",
            "/CN=localhost",
        )
        ssl_cmd(
            "x509",
            "-req",
            "-in",
            str(csr),
            "-CA",
            str(ca),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-out",
            str(pem),
            "-days",
            "1",
            "-extfile",
            str(ext),
            "-extensions",
            "server",
        )
        for path in [ca_key, ca, key, pem]:
            os.chmod(path, 0o600)
        return ca, key, pem

    pem, key, leaf = cert("server")
    wrong, _, _ = cert("wrong")
    pnum = port()
    origin = "https://localhost:" + str(pnum)
    controller = spawn(
        c,
        "node-controller",
        "--listen",
        "127.0.0.1:" + str(pnum),
        "--tls-cert",
        str(leaf),
        "--tls-key",
        str(key),
    )
    time.sleep(0.5)
    invite = root / "invite.json"
    run(
        c,
        "node-join",
        "native",
        "--controller",
        origin,
        "--output",
        str(invite),
        "--memory",
        "1024",
        "--slots",
        "1",
        "--cpus",
        "1",
        "--ttl",
        "600",
    )
    v = json.loads(invite.read_text())
    try:
        bad = root / "badca"
        mkdir(bad)
        run(
            bad,
            "host",
            "join",
            "--invite-file",
            str(invite),
            "--ca-cert",
            str(wrong),
            "--accept-shared-pool",
            "--no-start",
            success=False,
        )
        check("wrong CA rejected without credentials", not (bad / "node.json").exists())
        bad = root / "badname"
        mkdir(bad)
        nv = dict(v)
        nv["controller"] = "https://127.0.0.1:" + str(pnum)
        ni = root / "wrong-hostname.json"
        private_json(ni, nv)
        run(
            bad,
            "host",
            "join",
            "--invite-file",
            str(ni),
            "--ca-cert",
            str(pem),
            "--accept-shared-pool",
            "--no-start",
            success=False,
        )
        check(
            "wrong hostname rejected without credentials",
            not (bad / "node.json").exists(),
        )
        # A correctly authenticated TLS redirect still must not carry the invite onward.
        import http.server, threading

        class Redirect(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(307)
                self.send_header("Location", origin + "/enroll")
                self.end_headers()

            def log_message(self, *args):
                pass

        redir = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Redirect)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(leaf, key)
        redir.socket = ctx.wrap_socket(redir.socket, server_side=True)
        thread = threading.Thread(target=redir.serve_forever, daemon=True)
        thread.start()
        nv = dict(v)
        nv["controller"] = "https://localhost:" + str(redir.server_port)
        ri = root / "redirect.json"
        private_json(ri, nv)
        bad = root / "badredirect"
        mkdir(bad)
        run(
            bad,
            "host",
            "join",
            "--invite-file",
            str(ri),
            "--ca-cert",
            str(pem),
            "--accept-shared-pool",
            "--no-start",
            success=False,
        )
        redir.shutdown()
        redir.server_close()
        check(
            "TLS redirect rejected without redeeming", not (bad / "node.json").exists()
        )
        run(
            n,
            "host",
            "join",
            "--invite-file",
            str(invite),
            "--ca-cert",
            str(pem),
            "--memory",
            "1024",
            "--cpus",
            "1",
            "--slots",
            "1",
            "--accept-shared-pool",
            "--no-start",
        )
        check(
            "native TLS one-use enrollment",
            json.loads((n / "node.json").read_text())["node"] == "native",
        )
        repeated = root / "reuse"
        mkdir(repeated)
        run(
            repeated,
            "host",
            "join",
            "--invite-file",
            str(invite),
            "--ca-cert",
            str(pem),
            "--accept-shared-pool",
            "--no-start",
            success=False,
        )
        check("consumed invitation rejected", not (repeated / "node.json").exists())
        ei = root / "expired.json"
        run(
            c,
            "node-join",
            "expired",
            "--controller",
            origin,
            "--output",
            str(ei),
            "--memory",
            "1024",
            "--slots",
            "1",
            "--cpus",
            "1",
            "--ttl",
            "1",
        )
        time.sleep(2)
        bad = root / "expired"
        mkdir(bad)
        run(
            bad,
            "host",
            "join",
            "--invite-file",
            str(ei),
            "--ca-cert",
            str(pem),
            "--accept-shared-pool",
            "--no-start",
            success=False,
        )
        check("expired invitation rejected", not (bad / "node.json").exists())
        run(n, "host", "start")
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            inventory = json.loads(run(c, "nodes").stdout)
            entry = next(x for x in inventory if x["id"] == "native")
            if entry["online"]:
                break
            time.sleep(0.2)
        check(
            "Mac dispatchable with exact tuple/owner clamp",
            entry["online"]
            and entry["capabilities"]["arch"] == "aarch64"
            and entry["capabilities"]["backend"] == "apple-virtualization"
            and entry["capabilities"]["images"] == ["ubuntu-arm64"]
            and entry["capabilities"]["memory_mib"] == 1024
            and entry["capabilities"]["vcpus"] == 1,
        )
        route = c / "nodes/native"
        created = request(
            route,
            {
                "op": "create",
                "id": "pool",
                "image": "ubuntu-arm64",
                "memory_mib": 1024,
                "vcpu_count": 1,
                "operation_key": "real-create",
            },
        )
        check(
            "TLS outbound create reaches actual Mac VM", created["state"] == "running"
        )
        executed = request(
            route,
            {
                "op": "exec",
                "id": "pool",
                "command": "printf POOL_EXEC; uname -m; exit 9",
                "operation_key": "real-exec",
            },
        )
        check(
            "TLS outbound exec with real exit",
            executed["exit_code"] == 9
            and "POOL_EXEC" in executed["output"]
            and "aarch64" in executed["output"],
        )
        replay = request(
            route,
            {
                "op": "exec",
                "id": "pool",
                "command": "printf POOL_EXEC; uname -m; exit 9",
                "operation_key": "real-exec",
            },
        )
        check("durable operation-key replay returns receipt", replay == executed)
        raw = bytes(range(256)) * 50
        request(
            route,
            {
                "op": "put",
                "id": "pool",
                "path": "/home/dev/pool-binary",
                "data": base64.b64encode(raw).decode(),
                "operation_key": "real-put",
            },
        )
        check(
            "TLS outbound binary files",
            base64.b64decode(
                request(
                    route, {"op": "get", "id": "pool", "path": "/home/dev/pool-binary"}
                )["data"]
            )
            == raw,
        )
        rootcheck = request(
            route,
            {
                "op": "exec",
                "id": "pool",
                "command": "sudo -n id -u 2>/dev/null; ip -brief link; test ! -S /var/run/docker.sock && printf NO_HOST_DOCKER",
            },
        )
        private_json(root / "root-policy.json", rootcheck)
        check(
            "guest root remains offline without host Docker",
            rootcheck["exit_code"] == 0
            and "0\n" in rootcheck["output"]
            and "NO_HOST_DOCKER" in rootcheck["output"]
            and "eth0" not in rootcheck["output"],
        )
        if a.linux_tests:
            binary = a.linux_tests.read_bytes()
            digest = hashlib.sha256(binary).hexdigest()
            request(
                n,
                {"op": "exec", "id": "pool", "command": "rm -f /home/dev/linux-tests"},
            )
            for offset in range(0, len(binary), 200000):
                request(
                    n,
                    {
                        "op": "put",
                        "id": "pool",
                        "path": "/home/dev/test-chunk",
                        "data": base64.b64encode(
                            binary[offset : offset + 200000]
                        ).decode(),
                    },
                )
                request(
                    n,
                    {
                        "op": "exec",
                        "id": "pool",
                        "command": "cat /home/dev/test-chunk >> /home/dev/linux-tests",
                    },
                )
            verified = request(
                n,
                {
                    "op": "exec",
                    "id": "pool",
                    "command": "chmod 700 /home/dev/linux-tests; sha256sum /home/dev/linux-tests",
                },
            )
            check("Linux test binary hash in ARM64 guest", digest in verified["output"])
            suite = request(
                n,
                {
                    "op": "exec",
                    "id": "pool",
                    "command": '/home/dev/linux-tests --test-threads=2 > /home/dev/linux-test-results 2>&1; code=$?; tail -12 /home/dev/linux-test-results; exit "$code"',
                },
            )
            private_json(root / "linux-test-summary.json", suite)
            print("LINUX_SUITE", suite, flush=True)
            report = request(
                n, {"op": "get", "id": "pool", "path": "/home/dev/linux-test-results"}
            )
            (root / "linux-test-results.log").write_bytes(
                base64.b64decode(report["data"])
            )
        # Replacement is tested with a second native agent sharing the same
        # private worker under this operator. No second guest/worker is launched.
        stream, _ = request(route, {"op": "terminal", "id": "pool"}, True)
        stream.settimeout(10)
        replacement = root / "replacement"
        mkdir(replacement)
        for name in ["host.json", "node.json"]:
            private_json(replacement / name, json.loads((n / name).read_text()))
        os.symlink(n / "control.sock", replacement / "control.sock")
        agent = spawn(
            replacement,
            "node-agent",
            "--controller",
            origin,
            "--credential",
            str(replacement / "node.json"),
            "--ca-cert",
            str(pem),
        )

        def eof(s):
            end = time.monotonic() + 12
            while time.monotonic() < end:
                b = s.recv(65536)
                if not b:
                    return True
            return False

        check("active PTY replacement closes by EOF", eof(stream))
        stream.close()
        agent.terminate()
        agent.wait(timeout=10)
        time.sleep(4)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            try:
                stream, _ = request(route, {"op": "terminal", "id": "pool"}, True)
                break
            except (OSError, RuntimeError):
                time.sleep(0.2)
        stream.settimeout(10)
        run(c, "node-revoke", "native")
        check("active PTY revoke closes by EOF", eof(stream))
        stream.close()
        run(n, "host", "stop")
        check(
            "host stop preserves disk and identity",
            (n / "m/pool/root.ext4").exists()
            and (n / "m/pool/machine-id").exists()
            and json.loads(run(n, "host", "status").stdout)["worker"] is None,
        )
    finally:
        try:
            run(n, "host", "stop")
        except Exception:
            pass
        for p in children:
            if p.poll() is None:
                p.terminate()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        private_json(
            root / "results.json",
            {
                "checks": checks,
                "count": len(checks),
                "scope": "real local TLS transport + actual Mac guest; not live human gateway/dashboard pool acceptance",
                "simultaneous_guests": 1,
            },
        )
        receipt_api.end(
            root,
            receipt,
            {
                "checks": checks,
                "child_exit_codes": [p.returncode for p in children],
                "simultaneous_guests": 1,
                "scope": "local TLS node protocol and actual VM; not live gateway acceptance",
            },
        )
        log.close()


if __name__ == "__main__":
    main()
