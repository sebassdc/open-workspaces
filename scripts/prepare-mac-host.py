#!/usr/bin/env python3
"""Prepare a pinned offline ARM64 boot fixture. No user disk or host mount."""
import argparse
import gzip
import fcntl
import stat
import hashlib
import json
import os
from pathlib import Path
import subprocess
import struct
import urllib.request

BASE = "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/netboot-3.24.2/"
PINS = {
    "vmlinuz-virt": "e45e1f6083d1ed45db6647b422e32b6ae6dc54de7b8190b7b97744fb293412e3",
    "initramfs-virt": "ffe65ec5a0c0bf470042ad28f7ce7aa5f842ce8090e4230fb2703a7a34e1bebe",
}

def archive_file(name, content, mode, inode):
    name = name.encode() + b"\0"
    fields = [inode, mode, 0, 0, 1, 0, len(content), 0, 0, 0, 0, len(name), 0]
    header = b"070701" + b"".join(f"{v:08x}".encode() for v in fields)
    entry = header + name
    entry += b"\0" * (-len(entry) % 4)
    entry += content
    return entry + b"\0" * (-len(content) % 4)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-dir", type=Path, required=True)
    opts = parser.parse_args()
    root = opts.data_dir.absolute()
    # Refuse unsafe roots rather than changing their ownership/permissions.
    if root.is_symlink(): raise ValueError("symlink root denied")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = root.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("test root must be owned and mode 0700")
    descriptor = os.open(root / ".runtime.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try: fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError: raise ValueError("fixture is running; stop its owned helper first")
    # Keep the lock until process exit; never reprepare a running VM's inputs.
    for name, digest in PINS.items():
        path = root / name
        if path.is_symlink(): raise ValueError("symlink artifact denied")
        if not path.exists():
            with urllib.request.urlopen(BASE + name, timeout=60) as src:
                data = src.read(32 * 1024 * 1024 + 1)
            if len(data) > 32 * 1024 * 1024: raise ValueError("artifact too large")
            if hashlib.sha256(data).hexdigest() != digest: raise ValueError("checksum mismatch")
            with path.open("xb") as dest: dest.write(data)
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"{name}: checksum mismatch")
    kernel = (root / "vmlinuz-virt").read_bytes()
    # Alpine uses the Linux EFI zboot wrapper; Apple direct boot takes Image.
    if kernel[4:8] == b"zimg" and kernel[24:28] == b"gzip":
        offset, size = struct.unpack_from("<II", kernel, 8)
        if offset + size > len(kernel): raise ValueError("invalid zboot bounds")
        import io
        with gzip.GzipFile(fileobj=io.BytesIO(kernel[offset:offset+size])) as compressed:
            kernel = compressed.read(128 * 1024 * 1024 + 1)
    if len(kernel) > 128 * 1024 * 1024 or kernel[56:60] != b"ARM\x64":
        raise ValueError("wrong kernel architecture or size")
    image = root / "Image"
    if image.is_symlink(): raise ValueError("symlink kernel output denied")
    image.write_bytes(kernel)
    init = (Path(__file__).resolve().parent.parent / "native/macos/ow-init").read_bytes()
    overlay = archive_file("ow-init", init, 0o100755, 1)
    overlay += archive_file("TRAILER!!!", b"", 0, 2)
    output = root / "initramfs-ow.gz"
    if output.is_symlink(): raise ValueError("symlink output denied")
    output.write_bytes((root / "initramfs-virt").read_bytes() + gzip.compress(overlay, mtime=0))
    # FAT is a small persistence fixture, not a production Linux root disk. Its
    # permissions are imposed by guest mount options. No preexisting disk altered.
    disk = root / "persist.img"
    if not disk.exists() and not disk.is_symlink():
        if (root / "persist.dmg").exists() or (root / "persist.cdr").exists():
            raise ValueError("incomplete disk staging retained; use a fresh test root")
        subprocess.run(["/usr/bin/hdiutil", "create", "-size", "256m", "-fs", "MS-DOS",
                        "-layout", "NONE", "-type", "UDIF", str(root / "persist.dmg")], check=True)
        subprocess.run(["/usr/bin/hdiutil", "convert", str(root / "persist.dmg"), "-format", "UDTO",
                        "-o", str(root / "persist")], check=True)
        (root / "persist.cdr").rename(disk)
        (root / "persist.dmg").unlink()
    disk_info = disk.lstat()
    if not stat.S_ISREG(disk_info.st_mode) or disk_info.st_uid != os.getuid() or disk_info.st_size != 256*1024*1024:
        raise ValueError("existing disk is not an owned regular 256 MiB fixture")
    manifest = {"schema": 1, "architecture": "aarch64", "alpine": "3.24.2", "source": BASE,
                "inputs": PINS, "kernel_sha256": hashlib.sha256(kernel).hexdigest(), "initramfs_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
                "disk": "persist.img", "network": False, "shared_folders": False}
    manifest_path = root / "fixture.json"
    if manifest_path.is_symlink(): raise ValueError("symlink manifest denied")
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print("Prepared offline ARM64 fixture (one 512 MiB guest):", root)

if __name__ == "__main__": main()
