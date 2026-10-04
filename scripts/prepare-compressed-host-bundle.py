#!/usr/bin/env python3
"""Prepare a new immutable raw+compressed bundle from reviewed raw assets.
No compression, services, guests, ingress or cloud mutation. The input .zst must
be verified by full decoded-byte acceptance before planner publication.
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
import importlib.util

spec = importlib.util.spec_from_file_location('raw_bundle', Path(__file__).with_name('prepare-host-bundle.py'))
raw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(raw)
APPROVED = '87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b'

def prepare(bundle, encoded, cli, output, build_root):
    for path in [bundle, encoded, cli, output, build_root]:
        if not path.is_absolute() or any(p.is_symlink() for p in [path, *path.parents]):
            raise ValueError('Absolute non-symlink paths required')
    if ((build_root / '.ow-host-build').read_text() != 'open-workspaces dedicated host build v1\n'
            or build_root.stat().st_uid != os.getuid() or build_root.stat().st_mode & 0o077
            or not output.is_relative_to(build_root) or output == build_root):
        raise ValueError('Owned private marked build root/output required')
    if output.exists():
        raise ValueError('Never overwrite an accepted release')
    raw.regular(bundle / 'manifest.json', 16384)
    manifest_bytes = (bundle / 'manifest.json').read_bytes()
    manifest = json.loads(manifest_bytes)
    files = dict(raw.FILES)
    files.update({'ubuntu.ext4': ('guest/ubuntu.ext4', 8 << 30, APPROVED),
                  'network-tools.tar.gz': ('guest/network-tools.tar.gz', 32 << 20, 'aedb91f6409189bb6a32184268824e21efd672b3c5a37f64ecd7075f8e6ceb37')})
    if (set(manifest) != {'version','runtime','arch','files'} or manifest['version'] != 2
            or manifest['runtime'] != 'firecracker-v1.17.0' or manifest['arch'] != 'x86_64'
            or len(manifest['files']) != 7 or {a['name'] for a in manifest['files']} != set(files)):
        raise ValueError('Fixed complete canonical v2 raw manifest required')
    for a in manifest['files']:
        _, maximum, pin = files[a['name']]
        if (set(a) != {'name','size','sha256'} or raw.regular(bundle / a['name'], maximum) != a['size']
                or raw.digest(bundle / a['name']) != a['sha256'] or (pin and a['sha256'] != pin)):
            raise ValueError('Raw bundle bytes/pins mismatch')
    if next(a for a in manifest['files'] if a['name']=='ubuntu.ext4')['size'] != 8 << 30:
        raise ValueError('Pristine sparse 8 GiB image required')
    raw.regular(cli, 32 << 20)
    raw.regular(cli.parent / 'build.json', 1 << 20)
    build = json.loads((cli.parent / 'build.json').read_text())
    if (build['cli_sha256'] != raw.digest(cli) or build['target'] != 'x86_64-unknown-linux-musl'
            or build['profile'] != 'release' or not build['compiler'].startswith('rustc 1.97.0 ')
            or build['zig'] != '0.15.2'
            or hashlib.sha256(json.dumps(build['source'],sort_keys=True).encode()).hexdigest() != build['source_sha256']):
        raise ValueError('Pinned static CLI source/build provenance required')
    encoded_size = raw.regular(encoded, 8 << 30)
    with encoded.open('rb') as f: header = f.read(18)
    if len(header) < 6 or header[:4] != bytes.fromhex('28b52ffd') or header[4] & 0x1b:
        raise ValueError('Ordinary dictionary-free zstd frame required')
    if header[4] & 0x20:
        width = [1, 2, 4, 8][header[4] >> 6]
        if len(header) < 5 + width:
            raise ValueError('Truncated zstd frame header')
        window = int.from_bytes(header[5:5+width], 'little') + (256 if width == 2 else 0)
    else:
        window = (1 << (10 + (header[5] >> 3))) * (8 + (header[5] & 7)) // 8
    if window > 1 << 26:
        raise ValueError('Zstd window exceeds 64 MiB')
    transport = dict(version=1, manifest=manifest,
                     ubuntu=dict(name='ubuntu.ext4.zst',codec='zstd',size=encoded_size,sha256=raw.digest(encoded)))
    output.parent.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='compressed-bundle-',dir=output.parent) as temp:
        stage = Path(temp)/'bundle'; stage.mkdir(mode=0o700)
        for name in files:
            subprocess.run(['cp','--reflink=auto','--sparse=auto','--',str(bundle/name),str(stage/name)],check=True)
            (stage/name).chmod(0o400)
        shutil.copyfile(bundle/'manifest.json',stage/'manifest.json')
        subprocess.run(['cp','--reflink=auto','--',str(encoded),str(stage/'ubuntu.ext4.zst')],check=True)
        shutil.copyfile(cli,stage/'ow-linux-amd64')
        (stage/'ow-linux-amd64').chmod(0o500)
        (stage/'ow-linux-amd64.sha256').write_text(build['cli_sha256']+'\n')
        # Preserve raw guest/runtime bytes; the CLI build pair is recorded separately.
        (stage/'build.json').write_text(json.dumps(build,sort_keys=True))
        (stage/'compressed-manifest.json').write_text(json.dumps(transport,sort_keys=True))
        # Bind publication to copied bytes, not merely an earlier source read.
        for asset in manifest['files']:
            path = stage / asset['name']
            if path.stat().st_size != asset['size'] or raw.digest(path) != asset['sha256']:
                raise ValueError('Incomplete/changed staged raw asset')
        if ((stage/'manifest.json').read_bytes() != manifest_bytes
                or raw.digest(stage/'ubuntu.ext4.zst') != transport['ubuntu']['sha256']
                or (stage/'ubuntu.ext4.zst').stat().st_size != encoded_size
                or raw.digest(stage/'ow-linux-amd64') != build['cli_sha256']):
            raise ValueError('Incomplete/changed staged manifest, encoded asset or CLI')
        for path in stage.iterdir():
            if path.name != 'ow-linux-amd64': path.chmod(0o400)
            with path.open('rb') as f: os.fsync(f.fileno())
        fd=os.open(stage,os.O_DIRECTORY)
        try: os.fsync(fd)
        finally: os.close(fd)
        os.rename(stage,output)
        fd=os.open(output.parent,os.O_DIRECTORY)
        try: os.fsync(fd)
        finally: os.close(fd)
    return transport

if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ['raw-bundle','encoded-ubuntu','linux-cli','output','build-root']:
        parser.add_argument('--'+name,required=True,type=Path)
    a=parser.parse_args();os.umask(0o077)
    prepare(a.raw_bundle.absolute(),a.encoded_ubuntu.absolute(),a.linux_cli.absolute(),a.output.absolute(),a.build_root.absolute())
    print('Prepared immutable raw+compressed bundle; full TLS byte acceptance and parent publication required.')
