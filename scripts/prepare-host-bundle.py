#!/usr/bin/env python3
"""Stage only fixed, verified minimal runtime files. No service/cloud writes.

--source must be a fresh preparation directory, never live workspace/snapshot data.
The dedicated guest/base.ext4 must be accompanied by clean-alpine.json written by
prepare-guest.py; its digest prevents accidentally selecting a modified live disk.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile

FILES = {
    'firecracker': ('official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64', 32 << 20, '99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5'),
    'vmlinux': ('downloads/vmlinux-6.1.186', 64 << 20, 'ea0e55d03dbaebc79a58644308e0517b7a33f1530a84848d9edf47ffa61f69c8'),
    'base.ext4': ('guest/base.ext4', 256 << 20, None),
    'ow-guest': ('guest/ow-guest', 32 << 20, None),
    'slirp4netns': ('bin/slirp4netns', 32 << 20, '411167abceff1fc8a7c562f53ed9f26d3ec8a17df4e2fe6eaa3bd53f1c1e1d48'),
}

def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def regular(path, maximum):
    for parent in [path, *path.parents]:
        if parent.is_symlink():
            raise ValueError('Symlink source/parent refused')
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= maximum:
        raise ValueError('Missing/nonregular/oversized source')
    return info.st_size

def prepare(source, output, cli, build_root, ubuntu_dev=False):
    if not build_root.is_absolute() or (build_root / '.ow-host-build').read_text() != 'open-workspaces dedicated host build v1\n':
        raise ValueError('Explicit marked dedicated build root required')
    for path in [source, output, cli]:
        if not path.is_relative_to(build_root) or path == build_root or any(p.is_symlink() for p in [path,*path.parents]):
            raise ValueError('Artifact paths must stay in non-symlink dedicated build storage')
    if build_root.stat().st_uid != os.getuid() or build_root.stat().st_mode & 0o077:
        raise ValueError('Dedicated build root must be owned/private')
    if output.exists() or output.is_symlink():
        raise ValueError('Output must be new; prior bundles are never overwritten')
    regular(source / 'guest/clean-alpine.json', 4096)
    clean = json.loads((source / 'guest/clean-alpine.json').read_text())
    if clean.get('recipe') != 'alpine-minirootfs-3.24.2-minimal-v1':
        raise ValueError('Fresh minimal Alpine recipe required')
    regular(cli, 32 << 20)
    regular(cli.parent / 'build.json', 1024 * 1024)
    build = json.loads((cli.parent / 'build.json').read_text())
    cli_hash = digest(cli)
    if not build.get('compiler','').startswith('rustc 1.97.0 ') or build.get('zig')!='0.15.2' or build.get('target')!='x86_64-unknown-linux-musl' or build.get('profile')!='release':
        raise ValueError('Expected pinned release toolchain provenance')
    if not isinstance(build.get('source'),dict) or 'crates/ow/src/onboarding.rs' not in build['source'] or 'crates/ow-guest/src/main.rs' not in build['source']:
        raise ValueError('Missing reviewed host/guest source manifest')
    if hashlib.sha256(json.dumps(build['source'],sort_keys=True).encode()).hexdigest()!=build.get('source_sha256'):
        raise ValueError('Build source manifest hash mismatch')
    if build.get('cli_sha256') != cli_hash or build.get('guest_sha256') != digest(source / 'guest/ow-guest'):
        raise ValueError('CLI/guest does not match recorded source build')
    files = dict(FILES)
    if ubuntu_dev:
        # Reviewed independent developer-v1 template; never accept arbitrary matching caller metadata.
        regular(source / 'guest/ubuntu.json', 16384)
        provenance = json.loads((source / 'guest/ubuntu.json').read_text())
        approved = '87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b'
        if not (provenance.get('profile') == 'ubuntu' and provenance.get('revision') == 'developer-v1' and provenance.get('disk_mib') == 8192 and provenance.get('developer_user') == 'dev' and provenance.get('sudo') == 'guest-only passwordless' and provenance.get('source') == 'https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-amd64.tar.gz' and provenance.get('source_sha256') == 'e77b6f10c2590cef872b33ee9f635a0e3fd1f57fb074c0e52b5c7f56147a0c86' and provenance.get('image_sha256') == approved and provenance.get('package_inventory_sha256') == '5138ec41754e0b7ae48a430a744fea09bd88dad3854ac3034f1e73fd96400e4c'):
            raise ValueError('Reviewed pristine Ubuntu developer-v1 provenance required')
        files['ubuntu.ext4'] = ('guest/ubuntu.ext4', 8 << 30, approved)
        files['network-tools.tar.gz'] = ('guest/network-tools.tar.gz', 32 << 20, 'aedb91f6409189bb6a32184268824e21efd672b3c5a37f64ecd7075f8e6ceb37')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='host-bundle-', dir=output.parent) as stage:
        bundle = Path(stage) / 'bundle'
        bundle.mkdir(mode=0o700)
        manifest = {'version': 2 if ubuntu_dev else 1, 'runtime': 'firecracker-v1.17.0', 'arch': 'x86_64', 'files': []}
        for name, (relative, maximum, pinned) in files.items():
            path = source / relative
            size = regular(path, maximum)
            actual = digest(path)
            expected = pinned or (clean['sha256'] if name == 'base.ext4' else actual)
            if actual != expected:
                raise ValueError(f'Hash mismatch: {name}')
            subprocess.run(['cp', '--sparse=auto', '--reflink=auto', '--', str(path), str(bundle / name)], check=True)
            (bundle / name).chmod(0o400)
            # Verify the copied bytes before publication.
            if digest(bundle / name) != actual:
                raise ValueError('Incomplete copy')
            manifest['files'].append({'name': name, 'size': size, 'sha256': actual})
        (bundle / 'manifest.json').write_text(json.dumps(manifest, sort_keys=True))
        (bundle / 'manifest.json').chmod(0o400)
        # CLI is separately selected for the gateway's existing bin/ contract.
        shutil.copyfile(cli, bundle / 'ow-linux-amd64')
        (bundle / 'ow-linux-amd64').chmod(0o500)
        if digest(bundle / 'ow-linux-amd64') != cli_hash:
            raise ValueError('CLI source changed or copied CLI incomplete')
        (bundle / 'ow-linux-amd64.sha256').write_text(cli_hash + '\n')
        (bundle / 'build.json').write_text(json.dumps(build, sort_keys=True))
        (bundle / 'build.json').chmod(0o400)
        help_result=subprocess.run([str(bundle/'ow-linux-amd64'),'host','join','--help'],capture_output=True,timeout=10,env={'PATH':'/usr/bin:/bin','LANG':'C'})
        if help_result.returncode or not all(flag in help_result.stdout for flag in [b'--ca-cert',b'--invite-file',b'--memory',b'--storage-gib']):
            raise ValueError('Copied CLI does not expose the accepted guided host interface')
        (bundle / 'ow-linux-amd64.sha256').chmod(0o400)
        for file in bundle.iterdir():
            with file.open('rb') as f:
                os.fsync(f.fileno())
        os.rename(bundle, output)
        fd = os.open(output.parent, os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    return manifest

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--ubuntu-dev', action='store_true', help='Include reviewed pristine Ubuntu developer template; clients opt in')
    p.add_argument('--source', required=True, type=Path)
    p.add_argument('--build-root', required=True, type=Path)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--linux-cli', required=True, type=Path)
    a = p.parse_args()
    os.umask(0o077)
    prepare(a.source.absolute(), a.output.absolute(), a.linux_cli.absolute(), a.build_root.absolute(), a.ubuntu_dev)
    print('Prepared immutable fixed host bundle and matching Linux CLI; deployment remains planner-owned.')
