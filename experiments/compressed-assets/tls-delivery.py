#!/usr/bin/env python3
"""Small synthetic HTTPS update-assets evidence; no VM/service/controller actions.

Runs the actual CLI in a private user/PID/mount namespace so stopped-host process
inventory sees only the test. Uses an explicit private test CA, never insecure TLS.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import select
import shutil
import ssl
import struct
import subprocess
import tempfile
import termios
import threading
import time
import fcntl

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--evidence', type=Path, required=True)
parser.add_argument('--real-bundle', type=Path)
args = parser.parse_args()
os.umask(0o077)
args.evidence.mkdir(parents=True, exist_ok=False)
binary = args.binary.resolve()
names = ['firecracker', 'vmlinux', 'base.ext4', 'ow-guest', 'slirp4netns', 'ubuntu.ext4', 'network-tools.tar.gz']
dests = ['official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64', 'downloads/vmlinux-6.1.186', 'guest/base.ext4', 'guest/ow-guest', 'bin/slirp4netns', 'guest/ubuntu.ext4', 'guest/network-tools.tar.gz']
payloads = {n: (bytes(1024 * 1024) if n == 'ubuntu.ext4' else (n.encode() * 32768)[:262144]) for n in names}
def digest(data):
    return hashlib.sha256(data).hexdigest()
def manifest(data, version):
    return dict(version=version, runtime='firecracker-v1.17.0', arch='x86_64', files=[dict(name=n, size=len(b), sha256=digest(b)) for n, b in data.items()])
encoded = subprocess.run(['zstd', '-19', '-T2', '-c'], input=payloads['ubuntu.ext4'], capture_output=True, check=True).stdout
if args.real_bundle:
    original = json.loads((args.real_bundle / 'manifest.json').read_text())
    transport = json.loads((args.real_bundle / 'compressed-manifest.json').read_text())
else:
    original = manifest(payloads, 2)
    transport = dict(version=1, manifest=original, ubuntu=dict(name='ubuntu.ext4.zst',codec='zstd',size=len(encoded),sha256=digest(encoded)))
wire_manifest = json.dumps(original).encode()
def variant():
    m = json.loads(json.dumps(transport)); b = encoded; mode = state['mode']
    if mode == 'truncated': b = b[:-1]
    if mode == 'trailing': b += b'x'
    if mode == 'concatenated': b += encoded
    if mode == 'skippable': b = bytes.fromhex('502a4d1800000000') + b
    if mode == 'dictionary': b = b[:4] + bytes([b[4] | 1]) + b[5:]
    if mode == 'window': b = b[:4] + bytes([b[4] & ~32, 255]) + b[6:]
    if mode in ['truncated','trailing','concatenated','skippable','dictionary','window']:
        m['ubuntu'].update(size=len(b), sha256=digest(b))
    if mode == 'corrupt': b = b[:-1] + bytes([b[-1]^1])
    if mode == 'oversized': b += b'x'
    if mode == 'decoded-oversized': m['manifest']['files'][5]['size'] -= 1
    if mode == 'decoded-hash': m['manifest']['files'][5]['sha256'] = '0'*64
    if mode == 'path': m['ubuntu']['name'] = '../ubuntu.ext4.zst'
    if mode == 'codec': m['ubuntu']['codec'] = 'xz'
    if mode == 'manifest': m['ubuntu']['size'] = 0
    return m, b
cert = args.evidence / 'test-ca.pem'
ca_key = args.evidence / 'test-ca-key.pem'
key = args.evidence / 'test-server-key.pem'
leaf = args.evidence / 'test-server.pem'
csr = args.evidence / 'test-server.csr'
extensions = args.evidence / 'test-server.ext'
extensions.write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n')
def openssl(*argv):
    subprocess.run(['openssl', *map(str, argv)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', ca_key, '-out', cert, '-days', '1', '-subj', '/CN=assets-progress test CA', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign')
openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', csr, '-subj', '/CN=localhost')
openssl('x509', '-req', '-in', csr, '-CA', cert, '-CAkey', ca_key, '-set_serial', '1', '-out', leaf, '-days', '1', '-extfile', extensions)
state = {'mode': 'success', 'completed': False}
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *unused):
        pass
    def do_GET(self):
        if self.path == '/cli/host-compressed-manifest.json':
            if state['mode'].startswith('fallback'):
                self.send_error(404); return
            if state['mode'].startswith('status'):
                self.send_error(int(state['mode'][6:])); return
            if state['mode'] == 'json':
                self.send_response(200); self.end_headers(); self.wfile.write(b'{bad'); return
            body = json.dumps(variant()[0]).encode()
        elif self.path == '/cli/host-manifest.json':
            state['raw_requests'] += 1
            raw = json.loads(wire_manifest)
            if state['mode'] == 'fallback-v1':
                raw['version'] = 1; raw['files'] = raw['files'][:5]
            body = json.dumps(raw).encode()
        else:
            name = self.path.removeprefix('/cli/host/')
            if args.real_bundle and name in names + ['ubuntu.ext4.zst']:
                path = args.real_bundle / name
                self.send_response(200); self.send_header('Content-Length', str(path.stat().st_size)); self.end_headers()
                try:
                    with path.open('rb') as f:
                        while chunk := f.read(65536): self.wfile.write(chunk)
                except (BrokenPipeError, ConnectionResetError, ssl.SSLError): pass
                return
            if name == 'ubuntu.ext4.zst': body = variant()[1]
            elif name in payloads: body = payloads[name]
            else: self.send_error(404); return
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            for offset in range(0, len(body), 16384):
                self.wfile.write(body[offset:offset + 16384])
                self.wfile.flush()
                if self.path != '/cli/host-manifest.json':
                    time.sleep(.005)
            if self.path.endswith('/network-tools.tar.gz'):
                state['completed'] = True
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(leaf, key)
server.socket = context.wrap_socket(server.socket, server_side=True)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
policy = 'Trusted shared private pool: all admitted workspace users may select this host. The host operator can inspect guest data and stop participation. This invitation grants node participation only, not human workspace management.'
results = []
try:
    for case in (['real'] if args.real_bundle else ['pty', 'nonTTY', 'corrupt', 'truncated', 'oversized', 'decoded-oversized', 'decoded-hash', 'window', 'dictionary', 'skippable', 'trailing', 'concatenated', 'path', 'codec', 'manifest', 'status503', 'status403', 'status302', 'json', 'fallback-v1', 'fallback-v2']):
        state.update(mode='success' if case in ('pty', 'nonTTY', 'real') else case, completed=False, raw_requests=0)
        root = Path(tempfile.mkdtemp(prefix='ow-ap-'))
        try:
            old = {n: n.encode() for n in names[:5]}
            for n, dest in zip(names[:5], dests[:5]):
                path = root / 'assets' / dest
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(old[n])
                path.chmod(0o700 if n in ('firecracker', 'ow-guest', 'slirp4netns') else 0o600)
            before = json.dumps(manifest(old, 1)).encode()
            (root / 'assets/manifest.json').write_bytes(before)
            shutil.copyfile(cert, root / 'controller-ca.pem')
            (root / 'node.json').write_text('{}')
            (root / 'sentinel-disk').write_bytes(b'private existing disk')
            (root / 'host.json').write_text(json.dumps(dict(version=1, controller=f'https://localhost:{server.server_port}/_nodes', memory=512, slots=2, cpus=2, storage_gib=1, policy=policy, ca_cert=str(root / 'controller-ca.pem'))))
            command = ['unshare', '--user', '--map-current-user', '--pid', '--fork', '--mount-proc', str(binary), '--local', '--data-dir', str(root), 'host', 'update-assets', '--ubuntu-dev']
            if case == 'fallback-v1': command.pop()
            started = time.monotonic()
            live = []
            if case == 'pty':
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=slave)
                os.close(slave)
                stderr = bytearray()
                try:
                    while True:
                        if select.select([master], [], [], .1)[0]:
                            try:
                                chunk = os.read(master, 65536)
                            except OSError:
                                break
                            if not chunk:
                                break
                            stderr.extend(chunk)
                            if b'MiB/s' in chunk and b'Download [' in chunk:
                                live.append(dict(seconds=round(time.monotonic() - started, 3), before_server_completion=not state['completed'], process_running=process.poll() is None, text=chunk.decode()))
                        if time.monotonic() - started > 30:
                            process.kill()
                            raise RuntimeError('PTY timeout')
                    stdout = process.communicate(timeout=5)[0]
                finally:
                    os.close(master)
                stderr = bytes(stderr)
            else:
                process = subprocess.run(command, capture_output=True, timeout=600)
                stdout, stderr = process.stdout, process.stderr
            (args.evidence / f'{case}.stdout').write_bytes(stdout)
            (args.evidence / f'{case}.stderr').write_bytes(stderr)
            (args.evidence / f'{case}.live.json').write_text(json.dumps(live, indent=2))
            total = sum(a['size'] for a in original['files']) - original['files'][5]['size'] + transport['ubuntu']['size']
            if case.startswith('fallback'): total = sum(len(payloads[n]) for n in (names[:5] if case == 'fallback-v1' else names))
            selection = f'Verified bundle selection: {total} bytes to transfer; per-file TLS deadline 1800 seconds\n'.encode()
            success = case in ('pty', 'nonTTY', 'real', 'fallback-v1', 'fallback-v2')
            assert (process.returncode == 0) == success, (case, stderr)
            if success: assert stdout == selection + b'Runtime assets updated while stopped; immutable assets-rollback-* retained. Start explicitly.\n', (case, stdout)
            assert state['raw_requests'] == int(case.startswith('fallback')), (case, state)
            assert (root / 'node.json').read_text() == '{}'
            assert (root / 'sentinel-disk').read_bytes() == b'private existing disk'
            assert not list(root.glob('assets-stage-*')), case
            if success:
                assert b'Assets verified and published.' in stderr
                assert stderr.index(b'Download complete:') < stderr.index(b'Publishing verified assets') < stderr.index(b'Assets verified and published.')
                expected = original if not case.startswith('fallback') else manifest({n:payloads[n] for n in (names[:5] if case == 'fallback-v1' else names)}, 1 if case == 'fallback-v1' else 2)
                local = json.loads((root / 'assets/manifest.json').read_text())
                assert local == expected
                for asset in expected['files']:
                    dest = dests[names.index(asset['name'])]
                    with (root / 'assets' / dest).open('rb') as f: assert hashlib.file_digest(f,'sha256').hexdigest() == asset['sha256']
                if case != 'fallback-v1':
                    sparse = (root / 'assets/guest/ubuntu.ext4').stat()
                    assert sparse.st_blocks * 512 < sparse.st_size
                    (args.evidence / f'{case}.decoded.json').write_text(json.dumps(dict(logical_bytes=sparse.st_size,allocated_bytes=sparse.st_blocks*512,sha256=expected['files'][5]['sha256'])))
                assert len(list(root.glob('assets-rollback-*'))) == 1
            else:
                assert b'Assets verified and published.' not in stderr and b'Runtime assets updated' not in stdout
                assert (root / 'assets/manifest.json').read_bytes() == before
                assert not list(root.glob('assets-rollback-*'))
            if case == 'pty':
                assert any(x['before_server_completion'] and x['process_running'] and '  0%' not in x['text'] and '100%' not in x['text'] for x in live)
                frames = stderr.split(b'\r\x1b[2K')
                assert all(len(f.split(b'\r')[0].split(b'\n')[0]) <= 79 for f in frames if f.startswith(b'Download ['))
            else:
                assert b'\r' not in stderr and b'\x1b' not in stderr
            results.append(dict(case=case, exit_code=process.returncode, seconds=round(time.monotonic() - started, 3), transfer_bytes=total, stdout_exact=True, live_updates=len(live), passed=True))
        finally:
            shutil.rmtree(root)
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
(args.evidence / 'result.json').write_text(json.dumps(dict(binary=str(binary), binary_sha256=digest(binary.read_bytes()), method='actual static CLI; private TLS CA; private user/PID/mount namespace; real pristine bundle when --real-bundle selected', cases=results, cleanup='scratch roots removed; TLS server/thread stopped; no VMs/services/owner/remote/cloud actions'), indent=2) + '\n')
print(json.dumps(results, indent=2))
