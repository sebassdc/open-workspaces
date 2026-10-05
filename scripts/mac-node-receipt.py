"""Private native attribution. Call before a bounded run and complete after cleanup."""

import hashlib, json, os, pathlib, shutil, subprocess, time


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1048576), b""):
            h.update(b)
    return h.hexdigest()


def command(*args):
    p = subprocess.run(args, capture_output=True, text=True, timeout=15)
    return {
        "exit_code": p.returncode,
        "stdout": p.stdout.strip(),
        "stderr": p.stderr.strip(),
    }


def sources():
    files = [pathlib.Path("Cargo.toml"), pathlib.Path("Cargo.lock")]
    files += (
        list(pathlib.Path("crates").rglob("*.rs"))
        + list(pathlib.Path("crates").rglob("Cargo.toml"))
        + list(pathlib.Path("native/macos").glob("*.swift"))
    )
    files += [
        pathlib.Path("scripts") / p
        for p in [
            "build-mac-host.sh",
            "prepare-mac-node.py",
            "test-mac-node.py",
            "test-mac-node-transport.py",
            "mac-node-receipt.py",
            "test-mac-node-trust.py",
        ]
    ]
    files += [
        pathlib.Path("crates/ow/ui/app.js"),
        pathlib.Path("native/macos/virtualization.entitlements"),
    ]
    return {str(p): sha(p) for p in sorted(files)}


def begin(root, cli, assets):
    disk = shutil.disk_usage(root)
    return {
        "started_ns": time.time_ns(),
        "base_revision": command("git", "rev-parse", "HEAD")["stdout"],
        "source_files": sources(),
        "artifacts": {
            "cli_sha256": sha(cli),
            "signed_helper_sha256": sha(cli.parent / "ow-vz"),
            "prepared_manifest_sha256": sha(assets / "manifest.json"),
            "manifest": json.loads((assets / "manifest.json").read_text()),
        },
        "signature": command(
            "codesign",
            "-d",
            "--entitlements",
            "-",
            "--verbose=2",
            str(cli.parent / "ow-vz"),
        ),
        "helper_minimum": command(
            "/usr/bin/vtool", "-show-build", str(cli.parent / "ow-vz")
        ),
        "host": {
            "os": command("sw_vers"),
            "chip": command("sysctl", "-n", "machdep.cpu.brand_string"),
            "memory": command("sysctl", "-n", "hw.memsize"),
            "memory_pressure": command("/usr/bin/memory_pressure"),
            "free_disk_bytes": disk.free,
            "swift": command("swift", "--version"),
            "cargo": command("cargo", "--version"),
        },
        "argv": __import__("sys").argv,
    }


def end(root, receipt, results):
    receipt["finished_ns"] = time.time_ns()
    receipt["source_unchanged"] = receipt["source_files"] == sources()
    receipt["results"] = results
    receipt["owned_native_identities"] = {
        str(p.relative_to(root)): json.loads(p.read_text())
        for p in root.rglob("process-*.json")
    }
    receipt["final_disks"] = {
        str(p.relative_to(root)): sha(p) for p in root.rglob("root.ext4")
    }
    path = root / ("receipt-" + str(receipt["started_ns"]) + ".json")
    path.write_text(json.dumps(receipt, indent=2) + "\n")
    os.chmod(path, 0o600)
    return path
