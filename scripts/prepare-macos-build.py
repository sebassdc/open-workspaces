#!/usr/bin/env python3
"""Pinned project-local Linux cross-build tools. SDK is build-only, not published."""
import concurrent.futures
import hashlib
from pathlib import Path
import tarfile
import urllib.request
ROOT = Path(__file__).resolve().parents[1] / 'data/macos-build'
ARTIFACTS = [
    ('zig-0.15.2.tar.xz', 'https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz',
     '02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239', 'zig-x86_64-linux-0.15.2/zig'),
    ('cargo-zigbuild.tar.xz', 'https://github.com/rust-cross/cargo-zigbuild/releases/download/v0.23.4/cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz',
     '9e3cf73485edbd45905c8aadbc0fdf869c7ddc3848f0c898229f2680db52e44b', 'cargo-zigbuild-x86_64-unknown-linux-musl/cargo-zigbuild'),
    ('MacOSX14.5.sdk.tar.xz', 'https://github.com/joseluisq/macosx-sdks/releases/download/14.5/MacOSX14.5.sdk.tar.xz',
     '6e146275d19f027faa2e8354da5e0267513abf013b8f16ad65a231653a2b1c5d', 'MacOSX14.5.sdk/SDKSettings.json'),
]

def prepare(item):
    name, url, digest, marker = item
    path = ROOT / name
    if not path.exists():
        temporary = path.with_suffix('.download')
        with urllib.request.urlopen(url, timeout=60) as response, temporary.open('wb') as dest:
            while chunk := response.read(1024 * 1024): dest.write(chunk)
        temporary.replace(path)
    with path.open('rb') as source:
        if hashlib.file_digest(source, 'sha256').hexdigest() != digest:
            raise RuntimeError(f'{name}: checksum mismatch')
    if not (ROOT / marker).exists():
        with tarfile.open(path) as archive: archive.extractall(ROOT, filter='data')
    print('Verified build dependency:', name)

if __name__ == '__main__':
    ROOT.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(prepare, ARTIFACTS))
    alias = ROOT / 'cargo-zigbuild'
    if alias.is_symlink(): alias.unlink()
    if alias.exists(): raise RuntimeError('Unexpected cargo-zigbuild path')
    alias.symlink_to('cargo-zigbuild-x86_64-unknown-linux-musl/cargo-zigbuild')
