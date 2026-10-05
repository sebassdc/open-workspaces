#!/usr/bin/env python3
"""Native hardware worker acceptance. Requires a new private root and prepared assets.
Starts at most one <=1 GiB/2 vCPU Ubuntu guest. Keeps private failure receipts.
"""

import argparse, base64, hashlib, json, os, pathlib, socket, struct, subprocess, time


def request(root, value, stream=False):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(120)
    s.connect(str(root / "control.sock"))
    s.sendall(json.dumps(value).encode() + b"\n")
    b = bytearray()
    while not b.endswith(b"\n"):
        part = s.recv(1)
        if not part:
            raise RuntimeError("worker EOF")
        b.extend(part)
        if len(b) > 1048576:
            raise RuntimeError("worker response limit")
    v = json.loads(b)
    if not v.get("ok"):
        s.close()
        raise RuntimeError(v.get("error", "worker error"))
    if stream:
        return s, v["result"]
    s.close()
    return v["result"]


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
    ap.add_argument("--resume", action="store_true")
    a = ap.parse_args()
    root = a.root.resolve()
    assets = a.assets.resolve()
    ow = a.ow.resolve()
    assert a.resume or not root.exists(), "new dedicated root required"
    root.mkdir(mode=0o700, exist_ok=a.resume)
    os.chmod(root, 0o700)
    cfg = {
        "version": 1,
        "policy": "Trusted shared private pool: all admitted workspace users may select this host. The host operator can inspect guest data and stop participation. This invitation grants node participation only, not human workspace management.",
        "controller": "https://localhost",
        "assets": str(assets),
        "memory": 1024,
        "cpus": 2,
        "slots": 1,
        "storage_gib": 20,
    }
    (root / "host.json").write_text(json.dumps(cfg))
    os.chmod(root / "host.json", 0o600)
    receipt = receipt_api.begin(root, ow, assets)
    checks = []
    workers = []
    external = []
    log = open(root / "test.log", "ab")
    p = None

    def check(name, condition):
        assert condition, name
        checks.append(name)
        print("PASS", name, flush=True)

    def launch():
        p = subprocess.Popen(
            [str(ow), "--data-dir", str(root), "worker"],
            stdout=log,
            stderr=log,
            stdin=subprocess.DEVNULL,
        )
        workers.append(p)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            assert p.poll() is None, "worker exited"
            try:
                request(root, {"op": "status"})
                return p
            except (OSError, RuntimeError):
                time.sleep(0.1)
        raise RuntimeError("worker readiness timeout")

    def deny(name, value):
        try:
            request(root, value)
        except RuntimeError:
            check(name, True)
            return
        raise AssertionError("accepted " + name)

    try:
        p = launch()
        check(
            "truthful Mac capabilities",
            request(root, {"op": "status"})["backend"] == "apple-virtualization",
        )
        deny(
            "x86 image rejected",
            {
                "op": "create",
                "id": "bad",
                "image": "ubuntu",
                "memory_mib": 1024,
                "vcpu_count": 1,
            },
        )
        deny(
            "unsupported shape rejected",
            {
                "op": "create",
                "id": "bad",
                "image": "ubuntu-arm64",
                "memory_mib": 256,
                "vcpu_count": 1,
            },
        )
        v = request(
            root,
            {
                "op": "create",
                "id": "real",
                "image": "ubuntu-arm64",
                "memory_mib": 1024,
                "vcpu_count": 1,
            },
        )
        if v["state"] == "stopped":
            v = request(root, {"op": "start", "id": "real"})
        check(
            "Ubuntu ARM64 running", v["state"] == "running" and v["arch"] == "aarch64"
        )
        v = request(
            root,
            {
                "op": "exec",
                "id": "real",
                "command": "printf NATIVE_EXEC; uname -m; id -u; exit 7",
            },
        )
        check(
            "real exec output/exit",
            v["exit_code"] == 7
            and "NATIVE_EXEC" in v["output"]
            and "aarch64" in v["output"]
            and "1000" in v["output"],
        )
        large = request(
            root,
            {
                "op": "exec",
                "id": "real",
                "command": "head -c 65536 /dev/zero | tr '\\000' X; exit 3",
            },
        )
        check(
            "exec drains complete bounded output",
            large["exit_code"] == 3 and large["output"] == "X" * 65536,
        )
        policy = request(
            root,
            {
                "op": "exec",
                "id": "real",
                "command": "sudo -n id -u; command -v apt-get; test -s /etc/ssl/certs/ca-certificates.crt && printf TRUSTED_CA",
            },
        )
        check(
            "developer sudo and trusted CA/package tools",
            policy["exit_code"] == 0
            and "0\n" in policy["output"]
            and "/usr/bin/apt-get" in policy["output"]
            and "TRUSTED_CA" in policy["output"],
        )
        deny(
            "escaped exec response budget is a clear error",
            {"op": "exec", "id": "real", "command": "head -c 200000 /dev/zero"},
        )
        check(
            "failed exec does not release live demand",
            request(root, {"op": "inspect", "id": "real"})["state"] == "running",
        )
        binary = bytes(range(256)) * 400
        request(
            root,
            {
                "op": "put",
                "id": "real",
                "path": "/home/dev/binary",
                "data": base64.b64encode(binary).decode(),
            },
        )
        v = request(root, {"op": "get", "id": "real", "path": "/home/dev/binary"})
        check("binary file roundtrip", base64.b64decode(v["data"]) == binary)
        deny(
            "file traversal rejected",
            {"op": "put", "id": "real", "path": "/home/dev/../root/x", "data": "eA=="},
        )
        request(
            root,
            {
                "op": "exec",
                "id": "real",
                "command": "ln -s /etc /home/dev/outside; ln -s /etc/shadow /home/dev/secret; mkfifo /home/dev/fifo",
            },
        )
        for name, path in [
            ("parent symlink rejected", "/home/dev/outside/shadow"),
            ("final symlink rejected", "/home/dev/secret"),
            ("FIFO rejected without blocking", "/home/dev/fifo"),
        ]:
            deny(name, {"op": "get", "id": "real", "path": path})
        deny(
            "running local capacity rejected",
            {
                "op": "create",
                "id": "other",
                "image": "ubuntu-arm64",
                "memory_mib": 512,
                "vcpu_count": 1,
            },
        )
        for op in [
            "snapshot",
            "fork",
            "hibernate",
            "restore",
            "ssh",
            "ssh-info",
            "ssh-keys",
            "tunnel",
        ]:
            deny("unsupported " + op, {"op": op, "id": "real"})
        s, _ = request(root, {"op": "terminal", "id": "real"}, True)
        s.settimeout(10)

        def frame(kind, data):
            s.sendall(bytes([kind]) + struct.pack("!I", len(data)) + data)

        def read_until(marker):
            output = bytearray()
            end = time.monotonic() + 15

            def exact(n):
                b = bytearray()
                while len(b) < n:
                    part = s.recv(n - len(b))
                    if not part:
                        raise RuntimeError("PTY EOF before marker")
                    b.extend(part)
                return bytes(b)

            while time.monotonic() < end:
                h = exact(5)
                n = struct.unpack("!I", h[1:])[0]
                assert n <= 8192
                data = exact(n)
                if h[0] == 0:
                    output.extend(data)
                if marker in output:
                    return bytes(output)
            raise RuntimeError("PTY marker absent")

        frame(1, struct.pack("!HH", 99, 37))
        frame(0, b"stty size; printf 'PTY_%s\\n' OK\n")
        out = read_until(b"PTY_OK")
        check("real PTY and resize", b"37 99" in out)
        frame(0, b"sleep 30\n")
        time.sleep(0.3)
        frame(0, b"\x03")
        frame(0, b"printf 'INT_%s\\n' OK\n")
        check("PTY Ctrl-C", b"INT_OK" in read_until(b"INT_OK"))
        frame(0, b"exit 13\n")
        ended = False
        while not ended:
            header = b""
            while len(header) < 5:
                part = s.recv(5 - len(header))
                if not part:
                    raise RuntimeError("PTY EOF before exit status")
                header += part
            length = struct.unpack("!I", header[1:])[0]
            assert length <= 8192
            payload = b""
            while len(payload) < length:
                part = s.recv(length - len(payload))
                if not part:
                    raise RuntimeError("PTY EOF before exit payload")
                payload += part
            if header[0] == 2:
                check(
                    "PTY exit status",
                    length == 4 and struct.unpack("!i", payload)[0] == 13,
                )
                ended = True
        check("PTY exits by EOF", s.recv(1) == b"")
        s.close()
        identity = (root / "m/real/machine-id").read_bytes()
        # Inject loss of the owned guest RPC endpoint without disrupting the VM.
        (root / "m/real/rpc.sock").unlink()
        v = request(root, {"op": "stop", "id": "real"})
        check("unavailable guest sync remains diagnostic", bool(v["sync_error"]))
        check("positive cold stop", v["workspace"]["state"] == "stopped")
        import fcntl

        with open(root / "m/real/vm.lock", "rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            check(
                "RPC failure still exits helper and retains disk",
                (root / "m/real/root.ext4").stat().st_size == 4294967296,
            )
        request(root, {"op": "start", "id": "real"})
        check(
            "cold persistent binary",
            base64.b64decode(
                request(root, {"op": "get", "id": "real", "path": "/home/dev/binary"})[
                    "data"
                ]
            )
            == binary,
        )
        check(
            "machine identity stable",
            (root / "m/real/machine-id").read_bytes() == identity,
        )
        p.kill()
        p.wait()
        time.sleep(2)
        p = launch()
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            v = request(root, {"op": "inspect", "id": "real"})
            if v["state"] == "stopped":
                break
            time.sleep(0.2)
        check("worker crash demand reconciled positively", v["state"] == "stopped")
        request(root, {"op": "start", "id": "real"})
        check(
            "worker recovery preserves files",
            base64.b64decode(
                request(root, {"op": "get", "id": "real", "path": "/home/dev/binary"})[
                    "data"
                ]
            )
            == binary,
        )
        request(root, {"op": "stop", "id": "real"})
        # One real unregistered native helper, using the stopped disk. Never
        # run it concurrently with the managed guest or signal foreign PIDs.
        import stat

        for name in ["rpc.sock", "pty.sock"]:
            endpoint = root / "m/real" / name
            info = endpoint.lstat()
            assert stat.S_ISSOCK(info.st_mode) and info.st_uid == os.getuid()
            endpoint.unlink()
        guest = subprocess.Popen(
            [str(ow.parent / "ow-vz"), "node", str(root / "m/real/vm.json")],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
        external.append(guest)
        time.sleep(0.3)
        deny(
            "live unregistered helper consumes capacity",
            {
                "op": "create",
                "id": "other",
                "image": "ubuntu-arm64",
                "memory_mib": 512,
                "vcpu_count": 1,
            },
        )
        guest.stdin.close()
        guest.wait(timeout=15)
        check("owned unregistered helper cleaned", guest.returncode == 0)
        check(
            "positive reconciliation after external helper",
            request(root, {"op": "inspect", "id": "real"})["state"] == "stopped",
        )
        (root / "m/unregistered").mkdir(mode=0o700)
        deny("unregistered demand reserved", {"op": "start", "id": "real"})
        (root / "m/unregistered").rmdir()
        small = request(
            root,
            {
                "op": "create",
                "id": "small",
                "image": "ubuntu-arm64",
                "memory_mib": 512,
                "vcpu_count": 2,
            },
        )
        if small["state"] == "stopped":
            small = request(root, {"op": "start", "id": "small"})
        check("512 MiB / 2 vCPU Ubuntu starts", small["state"] == "running")
        cpus = request(
            root, {"op": "exec", "id": "small", "command": "nproc; uname -m"}
        )
        check(
            "guest observes two actual CPUs",
            cpus["exit_code"] == 0
            and cpus["output"].startswith("2\n")
            and "aarch64" in cpus["output"],
        )
        request(root, {"op": "stop", "id": "small"})
        check(
            "synthetic reservations removed",
            not any(
                v.get("synthetic_reservation") for v in request(root, {"op": "list"})
            ),
        )
        check(
            "base disk independent of guest writes",
            receipt_api.sha(assets / "root.ext4")
            == json.loads((assets / "manifest.json").read_text())["files"]["root.ext4"][
                "sha256"
            ],
        )
        request(root, {"op": "shutdown"})
        p.wait(timeout=15)
        check("owned worker shutdown", p.returncode == 0)
    finally:
        for guest in external:
            if guest.poll() is None:
                if guest.stdin and not guest.stdin.closed:
                    guest.stdin.close()
                try:
                    guest.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    guest.kill()
                    guest.wait()
        for w in workers:
            if w.poll() is None:
                try:
                    request(root, {"op": "shutdown"})
                    w.wait(timeout=15)
                except Exception:
                    w.kill()
                    w.wait()
        (root / ("results-" + str(time.time_ns()) + ".json")).write_text(
            json.dumps(
                {
                    "checks": checks,
                    "count": len(checks),
                    "worker_exit_codes": [w.returncode for w in workers],
                    "vm_budget": {"memory_mib": 1024, "vcpus": 2, "simultaneous": 1},
                },
                indent=2,
            )
            + "\n"
        )
        receipt_api.end(
            root,
            receipt,
            {
                "checks": checks,
                "worker_exit_codes": [w.returncode for w in workers],
                "simultaneous_guests": 1,
            },
        )
        log.close()


if __name__ == "__main__":
    main()
