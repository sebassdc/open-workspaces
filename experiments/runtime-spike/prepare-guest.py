#!/usr/bin/env python3
"""Build a disposable Alpine guest image without mounting filesystems as root."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / 'data/runtime-spike'
ASSETS = {
    'alpine-minirootfs-3.24.2-x86_64.tar.gz': (
        'https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-minirootfs-3.24.2-x86_64.tar.gz',
        'c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677'),
    'vmlinux-6.1.186': (
        'https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260930-a738f18a8db0-0/x86_64/vmlinux-6.1.186',
        'ea0e55d03dbaebc79a58644308e0517b7a33f1530a84848d9edf47ffa61f69c8'),
}

for name, (url, expected) in ASSETS.items():
    path = DATA / 'downloads' / name
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        subprocess.run(['curl', '-fsSL', '--retry', '2', url, '-o', str(path)], check=True)
    with path.open('rb') as stream:
        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
    if actual != expected:
        raise SystemExit(f'Artifact hash mismatch: {name}')

image_dir = DATA / 'guest'
image_dir.mkdir(exist_ok=True)
image = image_dir / 'base.ext4'
if image.exists():
    raise SystemExit('Base image already exists; refusing to overwrite it.')
with tempfile.TemporaryDirectory(prefix='build-', dir=image_dir) as temporary:
    tree = Path(temporary) / 'root'
    tree.mkdir()
    subprocess.run(['tar', '-xzf', str(DATA / 'downloads/alpine-minirootfs-3.24.2-x86_64.tar.gz'),
                    '-C', str(tree), '--no-same-owner'], check=True)
    for directory in ('proc', 'sys', 'dev', 'run', 'persist', 'www'):
        (tree / directory).mkdir(exist_ok=True)
    (tree / 'www/index.html').write_text('open-workspaces guest HTTP ready\n')
    init = tree / 'init'
    init.write_text('''#!/bin/sh
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /run
ip link set lo up
stty -echo
export PS1='OW> '
echo OW_GUEST_READY
exec /bin/sh -i
''')
    init.chmod(0o755)
    environment = dict(os.environ)
    environment.update(RUSTUP_HOME=str(DATA / 'rustup'), CARGO_HOME=str(DATA / 'cargo'))
    subprocess.run([str(DATA / 'cargo/bin/rustc'), '+1.97.0', '--edition=2024',
                    '--target', 'x86_64-unknown-linux-musl', '-C', 'opt-level=2',
                    str(ROOT / 'experiments/runtime-spike/http-fixture.rs'),
                    '-o', str(tree / 'http-fixture')], env=environment, check=True)
    with image.open('xb') as stream:
        stream.truncate(128 * 1024 * 1024)
    subprocess.run(['mkfs.ext4', '-q', '-F', '-O', '^metadata_csum_seed,^orphan_file',
                    '-E', 'root_owner=0:0', '-d', str(tree), str(image)], check=True)
print(image)
