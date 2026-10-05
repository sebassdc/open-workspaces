#!/usr/bin/env python3
"""Build private ext4 inputs from locally verified Ubuntu release artifacts.
No download, host mounts, sudo, guest networking or live deployment.
"""

import argparse, gzip, hashlib, json, os, pathlib, shutil, subprocess, tarfile, tempfile


def run(*args, **kw):
    subprocess.run(args, check=True, **kw)


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1024 * 1024), b""):
            h.update(b)
    return h.hexdigest()


UEC_FINGERPRINT = "D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify_release(key, signature, document):
    # Isolated GnuPG home: neither user IDs nor ambient/default keyrings are trust.
    with tempfile.TemporaryDirectory(prefix="ow-release-trust-") as home:

        def gpg(*args):
            return subprocess.check_output(
                ["gpg", "--no-options", "--batch", "--homedir", home, *args]
            )

        gpg("--import", str(key))
        records = (
            gpg("--with-colons", "--fingerprint", "--list-keys").decode().splitlines()
        )
        primary = []
        waiting = False
        for record in records:
            fields = record.split(":")
            if fields[0] == "pub":
                waiting = True
            elif fields[0] == "fpr" and waiting:
                primary.append(fields[9])
                waiting = False
        require(UEC_FINGERPRINT in primary, "pinned primary release key absent")
        restricted = pathlib.Path(home) / "release.gpg"
        restricted.write_bytes(gpg("--export", UEC_FINGERPRINT))
        status = (
            subprocess.check_output(
                [
                    "gpgv",
                    "--homedir",
                    home,
                    "--status-fd",
                    "1",
                    "--keyring",
                    str(restricted),
                    str(signature),
                    str(document),
                ]
            )
            .decode()
            .splitlines()
        )
        signers = [
            line.split()[2:] for line in status if line.startswith("[GNUPG:] VALIDSIG ")
        ]
        require(
            bool(signers)
            and all(
                fields[0] == UEC_FINGERPRINT or fields[-1] == UEC_FINGERPRINT
                for fields in signers
            ),
            "unexpected successful release signer",
        )


def authenticated_archive_keyring(root_tar, supplied=None):
    with tarfile.open(root_tar, "r:xz") as archive:
        matches = [
            member
            for member in archive.getmembers()
            if (member.name[2:] if member.name.startswith("./") else member.name)
            == "usr/share/keyrings/ubuntu-archive-keyring.gpg"
        ]
        require(
            len(matches) == 1 and matches[0].isreg() and matches[0].size <= 1048576,
            "authenticated root archive keyring absent/invalid",
        )
        data = archive.extractfile(matches[0]).read()
    if supplied is not None and supplied.exists():
        require(
            supplied.read_bytes() == data,
            "supplied archive keyring differs from authenticated root",
        )
    return data


def verify_archive(src, root_tar):
    data = authenticated_archive_keyring(root_tar, src / "archive-keyring.gpg")
    with tempfile.TemporaryDirectory(prefix="ow-archive-trust-") as home:
        key = pathlib.Path(home) / "archive.gpg"
        key.write_bytes(data)
        run("gpgv", "--homedir", home, "--keyring", str(key), str(src / "InRelease"))


def main():
    a = argparse.ArgumentParser()
    a.add_argument("upstream", type=pathlib.Path)
    a.add_argument("destination", type=pathlib.Path)
    a.add_argument("--guest", required=True, type=pathlib.Path)
    v = a.parse_args()
    src = v.upstream.resolve()
    out = v.destination.resolve()
    require(not out.exists(), "destination already exists")
    key = src / "cloud-image.gpg"
    names = [
        "ubuntu-24.04-server-cloudimg-arm64-root.tar.xz",
        "unpacked/ubuntu-24.04-server-cloudimg-arm64-vmlinuz-generic",
        "unpacked/ubuntu-24.04-server-cloudimg-arm64-initrd-generic",
    ]
    for sub in ["", "unpacked"]:
        p = src / sub
        verify_release(key, p / "SHA256SUMS.gpg", p / "SHA256SUMS")
    for name in names:
        p = src / name
        manifest = (p.parent / "SHA256SUMS").read_text()
        require(
            any(
                line.split() == [sha(p), "*" + p.name]
                or line.split() == [sha(p), p.name]
                for line in manifest.splitlines()
            ),
            "signed input checksum mismatch: " + name,
        )
    # The module package is verified through the archive keyring inside the signed root.
    verify_archive(src, src / names[0])
    import lzma, re

    digest = sha(src / "Packages.xz")
    require(
        re.search(
            r"^ " + digest + r"\s+\d+ main/binary-arm64/Packages.xz$",
            (src / "InRelease").read_text(),
            re.M,
        ),
        "archive index checksum mismatch",
    )
    paragraphs = (
        lzma.decompress((src / "Packages.xz").read_bytes()).decode().split("\n\n")
    )
    entry = next(
        x
        for x in paragraphs
        if x.startswith("Package: linux-modules-6.8.0-142-generic\n")
    )
    expected = next(
        x.split(": ", 1)[1] for x in entry.splitlines() if x.startswith("SHA256: ")
    )
    require(
        sha(src / "linux-modules.deb") == expected, "module package checksum mismatch"
    )
    out.mkdir(mode=0o700, parents=True)
    os.chmod(out, 0o700)
    mk = "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs"
    dbg = "/opt/homebrew/opt/e2fsprogs/sbin/debugfs"
    with tempfile.TemporaryDirectory(prefix="build-", dir=out) as temp:
        root = pathlib.Path(temp) / "root"
        root.mkdir()
        run(
            "bsdtar",
            "--no-same-owner",
            "--exclude",
            "dev/*",
            "-xf",
            str(src / names[0]),
            "-C",
            str(root),
        )
        data = next(
            x
            for x in subprocess.check_output(
                ["bsdtar", "-tf", str(src / "linux-modules.deb")], text=True
            ).splitlines()
            if x.startswith("data.tar")
        )
        archive = pathlib.Path(temp) / "modules.tar"
        archive.write_bytes(
            subprocess.check_output(
                ["bsdtar", "-xOf", str(src / "linux-modules.deb"), data]
            )
        )
        modules = pathlib.Path(temp) / "modules"
        modules.mkdir()
        run("bsdtar", "--no-same-owner", "-xf", str(archive), "-C", str(modules))
        shutil.copytree(
            modules / "lib/modules", root / "usr/lib/modules", dirs_exist_ok=True
        )
        (root / "dev").mkdir(exist_ok=True)
        (root / "usr/local/bin/ow-guest").write_bytes(v.guest.read_bytes())
        os.chmod(root / "usr/local/bin/ow-guest", 0o755)
        (root / "etc/sudoers.d/ow-dev").write_text("dev ALL=(ALL) NOPASSWD: ALL\n")
        os.chmod(root / "etc/sudoers.d/ow-dev", 0o440)
        (root / "etc/ow-developer").write_text("dev\n")
        (root / "home/dev").mkdir(exist_ok=True)
        with open(root / "etc/passwd", "a") as f:
            f.write("dev:x:1000:1000:Developer:/home/dev:/bin/bash\n")
        with open(root / "etc/shadow", "a") as f:
            f.write("dev:!:20000:0:99999:7:::\n")
        (root / "etc/hostname").write_text("ow-node\n")
        with open(root / "etc/hosts", "a") as f:
            f.write("127.0.1.1 ow-node\n")
        with open(root / "etc/group", "a") as f:
            f.write("dev:x:1000:\n")
        init = root / "ow-node-init"
        init.write_text("""#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /run /tmp
mount -t devpts devpts /dev/pts
chmod 1777 /tmp
hostname ow-node
ip link set lo up
for arg in $(cat /proc/cmdline); do
    case "$arg" in ow_epoch=*) date -u -s "@${arg#ow_epoch=}";; esac
done
depmod -a
modprobe vmw_vsock_virtio_transport
printf 'OW_NODE_AGENT_START\\n'
exec /usr/local/bin/ow-guest --vsock
""")
        os.chmod(init, 0o755)
        # Archive directories such as /var/empty have mode 000. Temporarily
        # grant the unprivileged builder read/traverse; restore exact modes in ext4.
        import stat

        modes = {}
        for directory, dirs, files in os.walk(root):
            for name in dirs + files:
                p = pathlib.Path(directory) / name
                info = p.lstat()
                modes["/" + str(p.relative_to(root))] = info.st_mode
                if not stat.S_ISLNK(info.st_mode):
                    os.chmod(
                        p,
                        info.st_mode | 0o700
                        if stat.S_ISDIR(info.st_mode)
                        else info.st_mode | 0o400,
                    )
        run(
            mk,
            "-q",
            "-t",
            "ext4",
            "-b",
            "4096",
            "-F",
            "-d",
            str(root),
            str(out / "root.ext4"),
            "1048576",
        )
        # mke2fs -d copies the builder UID; restore archive UID/GID inside ext4,
        # without changing any ownership on the host.
        owners = {}
        with tarfile.open(src / names[0]) as t:
            for m in t:
                name = m.name.lstrip("./")
                if (root / name).exists() or (root / name).is_symlink():
                    owners["/" + name] = (m.uid, m.gid)
                    kind = (
                        stat.S_IFDIR
                        if m.isdir()
                        else stat.S_IFLNK
                        if m.issym()
                        else stat.S_IFREG
                    )
                    modes["/" + name] = kind | m.mode
        for p in root.rglob("*"):
            owners.setdefault("/" + str(p.relative_to(root)), (0, 0))
        for p in ["/home/dev"]:
            owners[p] = (1000, 1000)
        commands = []
        for p, (uid, gid) in owners.items():
            require(not any(c in p for c in '\n"'), "unsafe debugfs path")
            commands += [
                'set_inode_field "' + p + '" uid ' + str(uid),
                'set_inode_field "' + p + '" gid ' + str(gid),
            ]
        for p, mode in modes.items():
            require(not any(c in p for c in '\n"'), "unsafe debugfs path")
            commands.append('set_inode_field "' + p + '" mode ' + str(mode))
        batch = pathlib.Path(temp) / "owners"
        batch.write_text("\n".join(commands) + "\n")
        with open(out / "build.log", "w") as log:
            run(
                dbg,
                "-w",
                "-f",
                str(batch),
                str(out / "root.ext4"),
                stdout=log,
                stderr=subprocess.STDOUT,
            )
    (out / "Image").write_bytes(gzip.decompress((src / names[1]).read_bytes()))
    require((out / "Image").read_bytes()[56:60] == b"ARMd", "not an ARM64 kernel")
    shutil.copyfile(src / names[2], out / "initrd")
    for name in ["Image", "initrd", "root.ext4"]:
        os.chmod(out / name, 0o600)
    manifest = {
        "version": 1,
        "backend": "apple-virtualization",
        "arch": "aarch64",
        "runtime": "apple-vz-v1",
        "image": "ubuntu-arm64",
        "upstream": "ubuntu-noble-20260926",
        "signing_fingerprint": "D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81",
        "guest_sha256": sha(v.guest),
        "files": {
            name: {"size": (out / name).stat().st_size, "sha256": sha(out / name)}
            for name in ["Image", "initrd", "root.ext4"]
        },
        "inputs": {
            name: sha(src / name)
            for name in names + ["linux-modules.deb", "InRelease", "Packages.xz"]
        },
        "licenses": {
            "ow-guest": "Apache-2.0",
            "ubuntu": "individual package licenses retained under /usr/share/doc; signed Ubuntu upstream packages",
            "linux-kernel": "GPL-2.0-only; Ubuntu linux source package 6.8.0-142.142",
        },
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    os.chmod(out / "manifest.json", 0o600)
    print("Prepared signed Ubuntu ARM64 assets; no VM started.")


if __name__ == "__main__":
    main()
