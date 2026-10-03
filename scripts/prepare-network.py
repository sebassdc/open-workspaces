#!/usr/bin/env python3
"""Install one verified upstream network helper in project data, without sudo."""
import hashlib
from pathlib import Path
import urllib.request
VERSION = '1.3.6'
SHA256 = '411167abceff1fc8a7c562f53ed9f26d3ec8a17df4e2fe6eaa3bd53f1c1e1d48'
path = Path(__file__).resolve().parents[1] / 'data/runtime-spike/bin/slirp4netns'
if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == SHA256:
    print('Verified slirp4netns', VERSION)
else:
    data = urllib.request.urlopen(f'https://github.com/rootless-containers/slirp4netns/releases/download/v{VERSION}/slirp4netns-x86_64', timeout=60).read()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise SystemExit('slirp4netns checksum mismatch')
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    temporary.write_bytes(data)
    temporary.chmod(0o700)
    temporary.replace(path)
    print('Installed verified slirp4netns', VERSION)

# Reuse only HTTPS helper files from the already pinned Alpine rootfs.
# Private /ow libraries cannot replace another distro's native OpenSSL libraries.
import io, tarfile
repo = Path(__file__).resolve().parents[1]
archive = repo / 'data/runtime-spike/downloads/alpine-minirootfs-3.24.2-x86_64.tar.gz'
if hashlib.sha256(archive.read_bytes()).hexdigest() != 'c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677':
    raise SystemExit('Alpine helper source checksum mismatch')
bundle = repo / 'data/runtime-spike/guest/network-tools.tar.gz'
mappings = {'./usr/bin/ssl_client':'ow/bin/ssl_client-native', './usr/lib/libssl.so.3':'ow/lib/libssl.so.3', './usr/lib/libcrypto.so.3':'ow/lib/libcrypto.so.3','./etc/ssl/certs/ca-certificates.crt':'ow/etc/ca-certificates.crt'}
# gzip mtime=0 makes the bundle content hash stable across setup runs.
import gzip
with bundle.with_suffix('.tmp').open('wb') as stream, gzip.GzipFile(fileobj=stream,mode='wb',mtime=0,filename='') as compressed, tarfile.open(fileobj=compressed,mode='w') as dest, tarfile.open(archive) as source:
    for directory in ["ow", "ow/bin", "ow/lib", "ow/etc"]:
        info=tarfile.TarInfo(directory);info.type=tarfile.DIRTYPE;info.mode=0o755;dest.addfile(info)
    for original,target in mappings.items():
        data=source.extractfile(original).read()
        info=tarfile.TarInfo(target);info.size=len(data);info.mode=0o755 if target.endswith('native') else 0o644
        dest.addfile(info,io.BytesIO(data))
    wrapper=b'#!/ow/bin/busybox sh\nSSL_CERT_FILE=/ow/etc/ca-certificates.crt LD_LIBRARY_PATH=/ow/lib exec /ow/bin/ssl_client-native "$@"\n'
    info=tarfile.TarInfo('ow/bin/ssl_client');info.size=len(wrapper);info.mode=0o755
    dest.addfile(info,io.BytesIO(wrapper))
bundle.with_suffix('.tmp').replace(bundle)
print('Prepared guest HTTPS helper bundle:', hashlib.sha256(bundle.read_bytes()).hexdigest())
