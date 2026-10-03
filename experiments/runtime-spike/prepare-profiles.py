#!/usr/bin/env python3
"""Build additional headless userspaces without sudo, Docker, or host mounts."""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / 'data/runtime-spike'
SOURCES = {
    'ubuntu': ('ubuntu-base-24.04.5-base-amd64.tar.gz',
               'https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/',
               'e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86', '', 512),
    'arch': ('archlinux-bootstrap-2026.10.01-x86_64.tar.zst',
             'https://archive.archlinux.org/iso/2026.10.01/',
             '0c9c97b7a58a8a023c9e07f84cb307a9e8006e65d8fc4c17b83ddc8010b45c10', 'root.x86_64/', 1024),
}


def extract(archive, destination, prefix=''):
    """Normalize rootfs absolute symlinks to equivalent in-tree relative links."""
    members = []
    for original in archive.getmembers():
        name = original.name.removeprefix('./')
        if prefix:
            if name in ('pkglist.x86_64.txt', 'version'):
                continue
            if name == prefix.rstrip('/'):
                continue
            if not name.startswith(prefix):
                raise ValueError('unexpected bootstrap archive prefix')
            name = name[len(prefix):]
        member = copy.copy(original)
        member.name = name
        if member.issym() and member.linkname.startswith('/'):
            member.linkname = os.path.relpath(destination / member.linkname.lstrip('/'), (destination / name).parent)
        if member.islnk():
            member.linkname = member.linkname.removeprefix('./').removeprefix(prefix)
        if member.isdev() or member.isfifo():
            continue  # /dev is provided by the guest kernel.
        members.append(member)
    archive.extractall(destination, members=members, filter='data')


def build(profile):
    filename, base_url, expected, prefix, size_mib = SOURCES[profile]
    cache = DATA / 'downloads/profiles'
    cache.mkdir(parents=True, exist_ok=True)
    source = cache / filename
    if not source.exists():
        subprocess.run(['curl', '-fsSL', '--retry', '2', base_url + filename, '-o', str(source)], check=True)
    with source.open('rb') as stream:
        if hashlib.file_digest(stream, 'sha256').hexdigest() != expected:
            raise ValueError('profile rootfs checksum mismatch')
    alpine = DATA / 'downloads/alpine-minirootfs-3.24.2-x86_64.tar.gz'
    with alpine.open('rb') as stream:
        if hashlib.file_digest(stream, 'sha256').hexdigest() != 'c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677':
            raise ValueError('management helper source checksum mismatch')
    image = DATA / 'guest' / f'{profile}.ext4'
    if image.exists():
        raise ValueError('profile already exists; refusing to overwrite it')
    with tempfile.TemporaryDirectory(prefix=f'build-{profile}-', dir=DATA / 'guest') as temporary:
        tree = Path(temporary) / 'root'
        tree.mkdir()
        with tarfile.open(source) as archive:
            extract(archive, tree, prefix)
        for name in ('proc', 'sys', 'dev', 'run', 'persist', 'ow/bin', 'root', 'lib'):
            (tree / name).mkdir(parents=True, exist_ok=True)
        with tarfile.open(alpine) as archive:
            (tree / 'ow/bin/busybox').write_bytes(archive.extractfile('./bin/busybox').read())
            (tree / 'lib/ld-musl-x86_64.so.1').write_bytes(archive.extractfile('./lib/ld-musl-x86_64.so.1').read())
        (tree / 'ow/bin/busybox').chmod(0o755)
        (tree / 'lib/ld-musl-x86_64.so.1').chmod(0o755)
        for name in ('ip', 'wget', 'mountpoint', 'vi', 'hostname'):
            (tree / 'ow/bin' / name).symlink_to('busybox')
        for directory in ('tmp', 'var/tmp'):
            (tree / directory).mkdir(parents=True, exist_ok=True)
            (tree / directory).chmod(0o1777)
        init = tree / 'init'
        init.write_text('''#!/ow/bin/busybox sh
/ow/bin/busybox mount -t devtmpfs devtmpfs /dev
/ow/bin/busybox mount -t proc proc /proc
/ow/bin/busybox mount -t sysfs sysfs /sys
/ow/bin/busybox mount -t tmpfs tmpfs /run
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/ow/bin
/ow/bin/busybox ip link set lo up
/ow/bin/busybox stty -echo
export PS1='OW> '
echo OW_GUEST_READY
exec /ow/bin/busybox sh -i
''')
        init.chmod(0o755)
        # A simple independent Bash environment; no desktop configs or host dotfiles.
        (tree / 'root/.bashrc').write_text("export PS1='\\u@\\h:\\w\\$ '\nexport TERM=xterm-256color\nalias ll='ls -alF'\n")
        (tree / 'etc/ow-image').write_text(profile + '\n')
        with image.with_suffix('.tmp').open('xb') as stream:
            stream.truncate(size_mib * 1024 * 1024)
        # File owners become guest root through a dedicated user namespace.
        subprocess.run(['unshare', '--user', '--map-root-user', '--', 'mkfs.ext4', '-q', '-F',
                        '-O', '^metadata_csum_seed,^orphan_file', '-E', 'root_owner=0:0',
                        '-d', str(tree), str(image.with_suffix('.tmp'))], check=True)
        image.with_suffix('.tmp').rename(image)
    with image.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    manifest = {'profile': profile, 'source': base_url + filename, 'source_sha256': expected,
                'image_sha256': digest, 'disk_mib': size_mib, 'kernel': 'shared pinned Firecracker Linux 6.1.186',
                'init': 'project minimal init, not systemd', 'omarchy': False}
    image.with_suffix('.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps(manifest))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('profile', choices=SOURCES)
    build(parser.parse_args().profile)
