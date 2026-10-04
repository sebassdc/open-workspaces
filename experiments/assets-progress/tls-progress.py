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
wire_manifest = json.dumps(manifest(payloads, 2)).encode()
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
        if self.path == '/cli/host-manifest.json':
            body = wire_manifest
        else:
            name = self.path.removeprefix('/cli/host/')
            if name not in payloads:
                self.send_error(404)
                return
            body = payloads[name]
            if name == 'ubuntu.ext4':
                if state['mode'] == 'corrupt':
                    body = body[:-1] + b'x'
                elif state['mode'] == 'truncated':
                    body = body[:-1]
                elif state['mode'] == 'oversized':
                    body += b'x'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            for offset in range(0, len(body), 16384):
                self.wfile.write(body[offset:offset + 16384])
                self.wfile.flush()
                if self.path != '/cli/host-manifest.json':
                    time.sleep(.025)
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
    for case in ['pty', 'nonTTY', 'corrupt', 'truncated', 'oversized']:
        state.update(mode='success' if case in ('pty', 'nonTTY') else case, completed=False)
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
            (root / 'host.json').write_text(json.dumps(dict(version=1, controller=f'https://localhost:{server.server_port}/_nodes', memory=512, slots=2, cpus=2, storage_gib=1, policy=policy, ca_cert=str(root / 'controller-ca.pem'))))
            command = ['unshare', '--user', '--map-current-user', '--pid', '--fork', '--mount-proc', str(binary), '--local', '--data-dir', str(root), 'host', 'update-assets', '--ubuntu-dev']
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
                process = subprocess.run(command, capture_output=True, timeout=30)
                stdout, stderr = process.stdout, process.stderr
            (args.evidence / f'{case}.stdout').write_bytes(stdout)
            (args.evidence / f'{case}.stderr').write_bytes(stderr)
            (args.evidence / f'{case}.live.json').write_text(json.dumps(live, indent=2))
            total = sum(map(len, payloads.values()))
            selection = f'Verified bundle selection: {total} bytes to transfer; per-file TLS deadline 1800 seconds\n'.encode()
            success = case in ('pty', 'nonTTY')
            assert (process.returncode == 0) == success, (case, stderr)
            assert stdout == selection + (b'Runtime assets updated while stopped; immutable assets-rollback-* retained. Start explicitly.\n' if success else b''), (case, stdout)
            assert not list(root.glob('assets-stage-*')), case
            if success:
                assert b'Assets verified and published.' in stderr
                assert stderr.index(b'Download complete:') < stderr.index(b'Publishing verified assets') < stderr.index(b'Assets verified and published.')
                for n, dest in zip(names, dests):
                    assert digest((root / 'assets' / dest).read_bytes()) == digest(payloads[n])
                sparse = (root / 'assets/guest/ubuntu.ext4').stat()
                assert sparse.st_blocks * 512 < sparse.st_size
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
(args.evidence / 'result.json').write_text(json.dumps(dict(binary=str(binary), binary_sha256=digest(binary.read_bytes()), method='actual static CLI; explicit synthetic TLS CA; private user/PID/mount namespace; stderr-only PTY at 80 columns', cases=results, cleanup='scratch roots removed; TLS server/thread stopped; no VMs/services/owner/remote/cloud actions'), indent=2) + '\n')
print(json.dumps(results, indent=2))
