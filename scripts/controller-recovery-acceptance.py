#!/usr/bin/env python3
"""Opt-in real KVM controller replacement on one already-authorized host.

Creates one private worker and disposable Ubuntu guest, loopback controllers and
an explicit process-scoped identity adapter. Never changes shared units, pins,
network ingress, provider resources or operational trust. Deletes its disposable
guest; retain private evidence then remove the scratch root after the drill.
Run only with separately authorized existing capacity.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request


def require(value, message):
    if not value:
        raise RuntimeError(message)


def process(pid):
    p = Path(f'/proc/{pid}')
    fields = p.joinpath('stat').read_text().rsplit(')', 1)[1].split()
    require(fields[0] != 'Z', 'process exited')
    return {'pid': pid, 'start_ticks': fields[19],
            'exe_sha256': hashlib.sha256(p.joinpath('exe').read_bytes()).hexdigest()}


class WebSocket:
    def __init__(self, config, path, viewer=False):
        self.s = socket.create_connection(('127.0.0.1', config['gateway_port']), timeout=10)
        self.buffer = b''
        key = base64.b64encode(os.urandom(16)).decode()
        email, peer = ('viewer@example.test', '100.64.0.98') if viewer else ('owner@example.test', '100.64.0.99')
        self.s.sendall((f'GET {path} HTTP/1.1\r\nHost: {config["hostname"]}\r\n'
                       f'Origin: https://{config["hostname"]}\r\nTailscale-User-Login: {email}\r\n'
                       f'X-Forwarded-For: {peer}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                       f'Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            data = self.s.recv(4096)
            if not data:
                raise EOFError('upgrade connection closed')
            self.buffer += data
            require(len(self.buffer) <= 16384, 'upgrade headers too large')
        header, self.buffer = self.buffer.split(b'\r\n\r\n', 1)
        require(header.startswith(b'HTTP/1.1 101'), f'WebSocket denied: {header!r}')
        accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        fields = {line.split(b':', 1)[0].lower(): line.split(b':', 1)[1].strip() for line in header.split(b'\r\n')[1:] if b':' in line}
        require(fields.get(b'sec-websocket-accept') == accept, 'bad upgrade accept')

    def send(self, data, opcode=2):
        require(len(data) < 65536, 'frame too large')
        length = bytes([0x80 | len(data)]) if len(data) < 126 else bytes([0x80 | 126])+struct.pack('!H', len(data))
        mask = os.urandom(4)
        self.s.sendall(bytes([0x80 | opcode])+length+mask+bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def read(self, size):
        while len(self.buffer) < size:
            data = self.s.recv(32768)
            if not data:
                raise EOFError()
            self.buffer += data
        data, self.buffer = self.buffer[:size], self.buffer[size:]
        return data

    def frame(self):
        header = self.read(2)
        size = header[1] & 127
        if size == 126:
            size = struct.unpack('!H', self.read(2))[0]
        elif size == 127:
            size = struct.unpack('!Q', self.read(8))[0]
        require(not header[1] & 128 and size <= 65536, 'invalid server frame')
        return header[0] & 15, self.read(size)

    def close(self):
        self.s.close()


def ssh_proxy(root):
    config = json.loads((root/'access.json').read_text())
    ws = WebSocket(config, '/api/ssh/'+config['box'])
    def upload():
        try:
            while data := os.read(0, 16384):
                ws.send(data)
        except OSError:
            pass
    threading.Thread(target=upload, daemon=True).start()
    try:
        while True:
            opcode, data = ws.frame()
            if opcode == 2:
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
            elif opcode == 9:
                ws.send(data, 10)
            elif opcode == 8:
                break
    except (EOFError, OSError):
        pass
    finally:
        ws.close()


class Drill:
    def __init__(self, args):
        self.args = args
        self.root = args.root.absolute()
        require(not self.root.exists() and len(str(self.root)) < 60, 'use a fresh short private root')
        require(self.root == self.root.resolve(), 'fixture root must not traverse symlinks')
        protected = args.protect_worker.resolve(strict=True)
        require(not self.root.is_relative_to(protected), 'fixture must be outside the protected worker')
        parent = self.root.parent.stat()
        require(parent.st_uid == os.geteuid() and parent.st_mode & 0o077 == 0, 'fixture parent must be owned and private')
        require(Path('/dev/kvm').is_char_device(), 'real KVM required')
        require(Path('/usr/local/lib/open-workspaces/preview-serve').is_file(), 'installed preview support required; no shared helper is installed by the drill')
        self.binary, self.worker_binary, self.assets = [p.resolve(strict=True) for p in (args.binary, args.worker_binary, args.assets)]
        require(not self.root.is_relative_to(self.assets), 'fixture must be outside installed assets')
        self.root.mkdir(mode=0o700)
        require(args.protect_worker.resolve() != self.root, 'protected worker cannot be the fixture root')
        self.token = os.urandom(6).hex()
        self.box = 'recovery-'+self.token
        self.old = self.root/'old'
        self.runner = self.old/'runner'
        self.children = []
        self.config = {'hostname': 'recovery.fixture.test', 'box': self.box, 'gateway_port': self.port()}
        self.env = {**os.environ, 'HOME': str(self.root), 'OW_ASSET_DIR': str(self.assets),
                    'OW_MAX_MEMORY_MIB': '512', 'OW_MAX_RUNNING': '1', 'OW_MAX_VCPUS': '1',
                    'OW_MIN_FREE_GIB': '1', 'OW_MAX_DISK_GIB': '16', 'OW_IDLE_HIBERNATE_SECS': '0'}
        bindir = self.root/'auth-bin'
        bindir.mkdir(mode=0o700)
        adapter = bindir/'tailscale'
        adapter.write_text('''#!/bin/sh
case "$1" in
status) echo '{"BackendState":"Stopped"}';;
whois) case "${@: -1}" in *98) id=98; mail=viewer@example.test;; *) id=99; mail=owner@example.test;; esac
printf '{"UserProfile":{"ID":%s,"LoginName":"%s"},"Node":{"Tags":[]}}\n' "$id" "$mail";;
*) exit 1;; esac
'''.replace('#!/bin/sh', '#!/bin/bash'))
        adapter.chmod(0o700)
        self.env['PATH'] = str(bindir)+':/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
        config = {'auth': 'tailscale', 'hostname': self.config['hostname'], 'upstream': 'http://127.0.0.1:9/',
                  'allowed_emails': ['owner@example.test', 'viewer@example.test']}
        self.private('dashboard.json', config)
        self.private('access.json', self.config)
        self.protected = self.protection()
        self.emit('protected-before', data=self.protected)

    def private(self, name, value):
        path = self.root/name
        path.write_text(json.dumps(value, indent=2))
        path.chmod(0o600)

    def emit(self, phase, **values):
        value = {'phase': phase, 'at': time.time(), **values}
        with (self.root/'events.jsonl').open('a') as stream:
            stream.write(json.dumps(value, sort_keys=True)+'\n')
            stream.flush()
            os.fsync(stream.fileno())
        print(json.dumps(value), flush=True)

    @staticmethod
    def port():
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0))
            return s.getsockname()[1]

    def command(self, binary, *args, timeout=120):
        p = subprocess.run([str(binary), *map(str, args)], env=self.env, capture_output=True, text=True, timeout=timeout)
        require(p.returncode == 0, f'{args}: {p.stderr[-4000:]}')
        return p.stdout

    def call(self, where, *args, raw=False):
        value = self.command(self.worker_binary if where == self.runner else self.binary, '--local', '--data-dir', where, *args)
        return value if raw else json.loads(value)

    @staticmethod
    def wait(check, label, timeout=60):
        end = time.monotonic()+timeout
        error = ''
        while time.monotonic() < end:
            try:
                result = check()
                if result:
                    return result
            except Exception as e:
                error = str(e)
            time.sleep(.25)
        raise RuntimeError(f'timeout {label}: {error}')

    def protection(self):
        status = json.loads(subprocess.check_output([str(self.worker_binary), '--local', '--data-dir', str(self.args.protect_worker), 'status'], text=True))
        pids = {status['worker_pid']}
        for unit in ('open-workspaces-worker.service', 'open-workspaces.service', 'open-workspaces-dashboard.service'):
            value = subprocess.run(['systemctl', 'show', '-p', 'MainPID', '--value', unit], capture_output=True, text=True)
            if value.returncode == 0 and value.stdout.strip().isdigit() and int(value.stdout) > 0:
                pids.add(int(value.stdout))
        for p in Path('/proc').iterdir():
            try:
                if p.name.isdecimal() and p.joinpath('comm').read_text().startswith('firecracker') and str(self.root) not in p.joinpath('cmdline').read_bytes().decode().replace('\0', ' '):
                    pids.add(int(p.name))
            except (OSError, UnicodeError):
                pass
        return {'processes': {str(p): process(p) for p in sorted(pids)},
                'pins': {n: os.readlink('/srv/ow/run/'+n) for n in ('worker', 'agent', 'dashboard')},
                'host_boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip()}

    def protect(self):
        require(self.protection() == self.protected, 'shared processes or pins changed')

    def spawn(self, name, args):
        with (self.root/(name+'.log')).open('ab') as log:
            child = subprocess.Popen([str(self.binary), *map(str, args)], env=self.env, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=log, start_new_session=True)
        self.children.append(child)
        self.private('children.json', [{'pid': p.pid, 'identity': process(p.pid)} for p in self.children if p.poll() is None])
        return child

    def stop(self, child):
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=30)

    def api(self, path='/api/state', data=None, viewer=False, host=None):
        mail, peer = ('viewer@example.test', '100.64.0.98') if viewer else ('owner@example.test', '100.64.0.99')
        headers = {'Host': host or self.config['hostname'], 'Origin': 'https://'+(host or self.config['hostname']),
                   'Tailscale-User-Login': mail, 'X-Forwarded-For': peer, 'X-OW-Request': 'dashboard', 'Content-Type': 'application/json'}
        request = urllib.request.Request(f'http://127.0.0.1:{self.config["gateway_port"]}'+path, data=json.dumps(data).encode() if data is not None else None, headers=headers)
        with urllib.request.urlopen(request, timeout=120) as reply:
            if host:
                return reply.read().decode()
            value = json.load(reply)
            require(value['ok'], str(value))
            return value['result']

    def operation(self, op, **values):
        return self.api('/api/operation', {'op': op, 'id': self.box, **values})

    def observe(self):
        status = self.call(self.runner, 'status')
        worker = status['worker_pid']
        pids = Path(f'/proc/{worker}/task/{worker}/children').read_text().split()
        vmms = [int(p) for p in pids if Path(f'/proc/{p}/comm').read_text().startswith('firecracker')]
        require(len(vmms) == 1, 'fixture must own exactly one VMM')
        output = self.call(self.runner, 'exec', self.physical, '--',
                           f"printf 'COUNT=%s\\n' \"$(cat {self.guest}/count)\"; printf 'STAT=%s\\n' \"$(cat /proc/$(cat {self.guest}/pid)/stat)\"; printf 'BOOT=%s\\n' \"$(cat /proc/sys/kernel/random/boot_id)\"", raw=True)
        counters = re.findall(r'COUNT=(\d+)', output)
        stats = re.findall(r'STAT=(\d+ [^\r\n]+)', output)
        boots = re.findall(r'BOOT=([a-f0-9-]{36})', output)
        require(len(counters) == len(stats) == len(boots) == 1, 'incomplete guest identity observation')
        counter, stat, boot = int(counters[0]), stats[0], boots[0]
        return {'worker': process(worker), 'vmm': process(vmms[0]), 'guest_pid': int(stat.split()[0]),
                'guest_start_ticks': stat.rsplit(')', 1)[1].split()[19], 'guest_boot_id': boot, 'counter': counter}

    def continuity(self, before, after):
        require({k:v for k,v in before.items() if k != 'counter'} == {k:v for k,v in after.items() if k != 'counter'}, 'worker/VMM/guest identity changed')
        require(after['counter'] > before['counter'], 'guest counter did not continue')
        self.protect()

    def access(self, phase, revoked=False):
        args = ['-F', '/dev/null', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-o', 'IdentitiesOnly=yes',
                '-o', 'HostKeyAlias=recovery-proof', '-o', f'UserKnownHostsFile={self.root}/known_hosts',
                '-o', f'ProxyCommand=python3 {shlex.quote(str(Path(__file__).resolve()))} --ssh-proxy {shlex.quote(str(self.root))}', '-i', str(self.root/'key'),
                'dev@recovery-proof', 'echo STRICT_SSH_RECONNECTED; cat /proc/sys/kernel/random/boot_id']
        ssh = self.command('ssh', *args)
        require('STRICT_SSH_RECONNECTED' in ssh and self.baseline['guest_boot_id'] in ssh, 'SSH did not reach preserved guest')
        if phase == 'before':
            self.command('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', self.root/'wrong-key')
            wrong = self.root/'wrong-known-hosts'
            wrong.write_text('recovery-proof '+(self.root/'wrong-key.pub').read_text())
            wrong_args = [f'UserKnownHostsFile={wrong}' if a.startswith('UserKnownHostsFile=') else a for a in args]
            denied = subprocess.run(['ssh', *wrong_args], env=self.env, capture_output=True, text=True, timeout=30)
            require(denied.returncode != 0 and 'Host key verification failed' in denied.stderr,
                    'strict SSH did not reject the incorrect host key')
        preview = self.api('/', viewer=not revoked, host=self.config['hostname']+':'+str(self.preview_port))
        require(self.token in preview, 'preview did not reach real guest')
        if revoked:
            try:
                self.api('/', viewer=True, host=self.config['hostname']+':'+str(self.preview_port))
            except urllib.error.HTTPError as e:
                require(e.code == 403, 'wrong preview denial')
            else:
                raise RuntimeError('revoked preview remained accessible')
        require(hashlib.sha256((self.root/'known_hosts').read_bytes()).hexdigest() == self.known_hosts_hash, 'host key pin changed')
        self.emit('access', stage=phase, strict_ssh=True, preview=True, revoked_denied=revoked)

    def freeze(self):
        before = self.observe()
        self.stop(self.gateway)
        if self.agent is not None:
            self.stop(self.agent)
        self.stop(self.authority)
        require(all(p.poll() is not None for p in (self.gateway, self.authority)) and (self.agent is None or self.agent.poll() is not None), 'old authority still running')
        try:
            self.api()
        except (OSError, urllib.error.URLError):
            pass
        else:
            raise RuntimeError('controller access survived positive stop')
        time.sleep(2)
        after = self.observe()
        self.continuity(before, after)
        self.emit('controller-loss', before=before, during=after, control_access_failed=True, vm_continued=True)

    def backup(self, base, name):
        path = self.root/(name+'.sqlite3')
        self.command(self.binary, 'backup', 'catalog', '--from', base/'controller', '--output', path)
        return path

    def restore(self, backup, name):
        import shutil
        base = self.root/name
        (base/'controller').mkdir(parents=True, mode=0o700)
        base.chmod(0o700)
        self.command(self.binary, '--local', '--data-dir', base, 'node', 'list')
        for suffix in ('-wal', '-shm'):
            (base/'controller'/('catalog.sqlite3'+suffix)).unlink(missing_ok=True)
        shutil.copy2(backup, base/'controller/catalog.sqlite3')
        shutil.copytree(self.old/'tls', base/'tls')
        self.command(self.binary, 'backup', 'catalog-check', base/'controller/catalog.sqlite3', '--against', backup)
        return base

    def activate(self, base, label):
        require(all(p.poll() is not None for p in self.children), 'simultaneous old/new control processes refused')
        port = self.port()
        self.authority = self.spawn(label+'-controller', ['--data-dir', base, 'host', 'up', '--controller-only', '--listen', f'127.0.0.1:{port}'])
        self.wait(lambda: (base/'host.json').exists(), 'replacement listener')
        self.agent = self.spawn(label+'-agent', ['--data-dir', self.runner, 'node-agent', '--controller', f'https://127.0.0.1:{port}',
                                               '--credential', self.runner/'node.json', '--ca-cert', base/'tls/ca.pem'])
        self.wait(lambda: any(n['id'] == 'recovery-runner' and n['online'] for n in self.call(base, 'node', 'list')), 'existing credential reconnect')
        self.gateway = self.spawn(label+'-gateway', ['--data-dir', base/'controller', 'dashboard', '--config', self.root/'dashboard.json', '--listen', f'127.0.0.1:{self.config["gateway_port"]}'])
        self.wait(lambda: self.api(), 'gateway reconnect')
        require((self.runner/'node.json').read_bytes() == self.credential, 'runner credential changed')
        self.continuity(self.baseline, self.observe())
        self.emit('authority-active', stage=label, root=base.name)

    def run(self):
        self.old.mkdir(mode=0o700)
        self.call(self.runner, 'up')
        require(self.call(self.runner, 'list') == [], 'fresh worker was not empty')
        self.agent = None
        self.authority = self.spawn('old-controller', ['--data-dir', self.old, 'host', 'up', '--external-worker', '--node', 'recovery-runner', '--memory', '512', '--slots', '1', '--cpus', '1', '--listen', f'127.0.0.1:{self.port()}'])
        self.wait(lambda: (self.old/'host.json').exists() and (self.runner/'node.json').exists(), 'combined host enrollment')
        limits = self.old/'controller/limits.json'
        limits.write_text(json.dumps({'pool': {'default_type': 'self-hosted', 'hourly_cents': {'self-hosted': 0}}}))
        limits.chmod(0o600)
        self.gateway = self.spawn('old-gateway', ['--data-dir', self.old/'controller', 'dashboard', '--config', self.root/'dashboard.json', '--listen', f'127.0.0.1:{self.config["gateway_port"]}'])
        self.wait(lambda: self.api(), 'initial gateway')
        self.wait(lambda: any(n['online'] for n in self.call(self.old, 'node', 'list')), 'initial runner')
        self.operation('create', node='recovery-runner', image='ubuntu', memory_mib=256, vcpu_count=1, disk_gib=8)
        rows = self.call(self.runner, 'list')
        require(len(rows) == 1, 'own disposable worker has unexpected guests')
        self.physical = rows[0]['id']
        self.guest = '/tmp/recovery-'+self.token
        counter = f'mkdir -p {self.guest}; echo $$ >{self.guest}/pid; n=0; while :; do n=$((n+1)); echo "$n" >{self.guest}/new; mv {self.guest}/new {self.guest}/count; sleep 1; done'
        self.operation('exec', command='nohup sh -c '+shlex.quote(counter)+' </dev/null >/tmp/recovery-counter.log 2>&1 &')
        self.wait(lambda: self.call(self.runner, 'exec', self.physical, '--', f'test -s {self.guest}/count', raw=True) is not None, 'counter')
        self.command('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', self.root/'key')
        probe = self.call(self.runner, 'exec', self.physical, '--',
                          'test -f /etc/ow-ssh-v1 || echo NEEDS_SSH_UPGRADE', raw=True)
        enrolled = self.operation('ssh-keys', keys=[(self.root/'key.pub').read_text().strip()],
                                  upgrade='NEEDS_SSH_UPGRADE' in probe)
        (self.root/'known_hosts').write_text('recovery-proof '+enrolled['host_key']+'\n')
        self.known_hosts_hash = hashlib.sha256((self.root/'known_hosts').read_bytes()).hexdigest()
        self.operation('exec', command=f'echo {self.token} >{self.guest}/index.html; nohup python3 -m http.server 8080 --bind 0.0.0.0 --directory {self.guest} </dev/null >/tmp/recovery-http.log 2>&1 &')
        self.api(viewer=True)
        self.api('/api/operation', {'op':'share', 'id':self.box, 'with':'viewer@example.test'})
        self.preview_port = 21000+int(self.token[:3], 16)
        # Isolated catalog-only publication avoids executing the shared Serve helper.
        # Real HTTP authorization/proxy/revocation still uses production gateway code.
        with sqlite3.connect(self.old/'controller/catalog.sqlite3') as db:
            db.execute('INSERT INTO publications(https_port,physical,guest_port,active) VALUES(?,?,8080,1)', (self.preview_port, self.physical))
        self.credential = (self.runner/'node.json').read_bytes()
        self.baseline = self.observe()
        self.access('before')
        self.freeze()
        first = self.backup(self.old, 'before-write')
        rollback = self.restore(first, 'rollback-before')
        self.activate(rollback, 'rollback-before')
        self.access('rollback-before')
        self.freeze()
        final = self.backup(rollback, 'final')
        replacement = self.restore(final, 'replacement')
        self.activate(replacement, 'replacement')
        self.access('replacement')
        self.api('/api/operation', {'op':'unshare', 'id':self.box, 'with':'viewer@example.test'})
        self.access('after-new-write', revoked=True)
        self.freeze()
        latest = self.backup(replacement, 'latest')
        p = subprocess.run([str(self.binary), 'backup', 'catalog-check', str(latest), '--against', str(final)], env=self.env, capture_output=True)
        require(p.returncode != 0, 'stale pre-cutover catalog accepted after new write')
        rollback = self.restore(latest, 'rollback-after')
        self.activate(rollback, 'rollback-after')
        self.access('rollback-after', revoked=True)
        self.freeze()
        final_latest = self.backup(rollback, 'latest-after-rollback')
        restored = self.restore(final_latest, 'final-replacement')
        self.activate(restored, 'final-replacement')
        self.access('final-replacement', revoked=True)
        self.emit('passed', before=self.baseline, after=self.observe(), external_host_fence_claimed=False,
                  real_kvm=True, private_loopback_identity=True, latest_catalog_rollback=True)

    def cleanup(self):
        for child in reversed(self.children):
            self.stop(child)
        if (self.runner/'control.sock').exists():
            rows = self.call(self.runner, 'list')
            require(all(r['id'] == getattr(self, 'physical', None) for r in rows), 'unknown fixture guest; retain for inspection')
            for row in rows:
                self.call(self.runner, 'stop', row['id'])
                self.call(self.runner, 'delete', row['id'])
            require(self.call(self.runner, 'list') == [], 'disposable guest cleanup incomplete')
            self.call(self.runner, 'down', raw=True)
        self.protect()
        self.emit('cleaned', protected_unchanged=True, disposable_guests_remaining=0,
                  evidence_retained=True, scratch_removal_required=True)


def main():
    if len(sys.argv) == 3 and sys.argv[1] == '--ssh-proxy':
        ssh_proxy(Path(sys.argv[2]))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('root', 'binary', 'worker-binary', 'assets', 'protect-worker'):
        parser.add_argument('--'+option, type=Path, required=True)
    args = parser.parse_args()
    drill = Drill(args)
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    try:
        drill.run()
    finally:
        drill.cleanup()


if __name__ == '__main__':
    main()
