#!/usr/bin/env python3
"""Focused no-VM onboarding preparation. Never starts/stops services or guests.

inspect records private host/source/deployment evidence; cli checks an installed
binary in a fresh private home; http executes private local HTTPS request cases.
no-vm adds actual host CLI and narrow gateway/controller auth adapters after a
core-ready receipt. VM/browser/physical-host checks remain gated. Evidence is ignored.
"""
import argparse
import base64
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import socket
import ssl
import sqlite3
import stat
import subprocess
import tempfile
import time
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parents[2]
PRIVATE = REPO / 'data/host-verify'
REQUIRED = (
    'installed-host-commands', 'doctor', 'clean-user-install-wizard',
    'owner-invite', 'nonowner-invite-denial', 'owner-revoke',
    'enrollment', 'replay-denial', 'expiry-denial', 'wrong-node-denial',
    'forged-capacity-denial', 'node-token-human-denial', 'human-token-node-denial',
    'prefixed-node-websocket', 'prefixed-job-websocket', 'tls-untrusted-denial',
    'redirect-denial', 'path-method-denial', 'private-config-credential',
    'doctor-kvm-storage-namespace-tools-capacity', 'artifact-hash-size-corruption',
    'artifact-path-symlink-extraction', 'start-stop-reconnect-revoke-fencing',
    'guest-cold-persistence', 'synthetic-browser-owner-controls',
    'baseline-continuity', 'test-cleanup', 'stable-source-shipped-artifacts',
    'public-human-protection', 'public-node-protocol', 'physical-host-nat',
)


def require(value, message):
    if not value:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1048576), b''):
            h.update(block)
    return h.hexdigest()


def save(path, value):
    require(path.resolve().is_relative_to(PRIVATE.resolve()), 'output outside owned ignored root')
    require(not path.is_symlink(), 'symlink output refused')
    with path.open('w') as stream:
        os.chmod(path, 0o600)
        json.dump(value, stream, indent=2)
        stream.write('\n')


def private_file(path):
    require(not path.is_symlink() and path.is_file(), 'private input must be a regular file')
    for item in (path, path.parent):
        info = item.stat()
        require(info.st_uid == os.getuid() and info.st_mode & 0o077 == 0,
                'private input and parent must be operator-owned and private')


def process(pid):
    proc = Path('/proc') / str(pid)
    info = proc.stat()
    fields = (proc / 'stat').read_text().rsplit(')', 1)[1].split()
    return {'pid': int(pid), 'uid': info.st_uid, 'ticks': fields[19],
            'state': fields[0], 'argv': (proc / 'cmdline').read_bytes().split(b'\0')[:-1]}


def guests():
    found = []
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            name = (proc / 'comm').read_text().strip()
            if not name.startswith('firecracker'):
                continue
            before = process(proc.name)
            if before['state'] == 'Z':
                continue
            args = before['argv']
            if b'--config-file' in args:
                path = Path(os.fsdecode(args[args.index(b'--config-file') + 1]))
                if not path.is_absolute():
                    path = proc / 'cwd' / path
                machine = json.loads(path.read_text())['machine-config']
            else:
                path = Path(os.fsdecode(args[args.index(b'--api-sock') + 1]))
                if not path.is_absolute():
                    path = proc / 'cwd' / path
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(3)
                    client.connect(str(path))
                    client.sendall(b'GET /machine-config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
                    response = http.client.HTTPResponse(client)
                    response.begin()
                    require(response.status == 200, 'VMM machine config unavailable')
                    body = response.read(65537)
                    require(len(body) <= 65536, 'VMM response oversized')
                    machine = json.loads(body)
            require(before['ticks'] == process(proc.name)['ticks'], 'VMM PID changed during scan')
            memory, cpus = machine['mem_size_mib'], machine['vcpu_count']
            require(type(memory) is int and memory > 0 and type(cpus) is int and cpus > 0,
                    'VMM demand unresolved')
            found.append({'pid': before['pid'], 'ticks': before['ticks'],
                          'memory_mib': memory, 'cpus': cpus, 'config_or_socket': str(path)})
        except FileNotFoundError:
            if not proc.exists():
                continue
            raise RuntimeError('live process configuration missing; block VM admission') from None
        except (OSError, ValueError, KeyError, IndexError):
            raise RuntimeError('host process/config unreadable; block VM admission') from None
    return found


def unrelated_supervisor(path):
    """Independent read-only exact systemd/logind proof; never comm-only allowlist."""
    before = process(int(path.name))
    fields = (path/'stat').read_text().rsplit(')',1)[1].split()
    parent = fields[1]; comm = (path/'comm').read_text().strip()
    cmd = (path/'cmdline').read_bytes(); group = (path/'cgroup').read_text().strip()
    require(group.startswith('0::') and '\n' not in group, 'unexpected supervisor cgroup')
    group = group[3:]
    def query(tool, *argv):
        info = Path(tool).stat()
        require(info.st_uid == 0 and info.st_mode & 0o022 == 0, 'untrusted supervisor query tool')
        value = subprocess.run([tool,'--no-pager',*argv],env={'PATH':'/usr/bin:/bin','LANG':'C'},
                               capture_output=True, timeout=5, check=True).stdout
        require(len(value)<=4096, 'supervisor query too large')
        return dict(line.split('=',1) for line in value.decode().splitlines())
    manager = query('/usr/bin/systemctl','show',f'user@{os.getuid()}.service','-p','MainPID','-p','ControlGroup')
    verified = group == manager['ControlGroup']+'/init.scope' and (
        (path.name == manager['MainPID'] and parent=='1' and comm=='systemd' and cmd==b'/usr/lib/systemd/systemd\0--user\0') or
        (parent==manager['MainPID'] and comm=='(sd-pam)' and cmd==b'(sd-pam)\0'))
    if not verified and comm=='tailscaled' and cmd.startswith(b'/usr/bin/tailscaled\0be-child\0ssh\0'):
        daemon = query('/usr/bin/systemctl','show','tailscaled.service','-p','MainPID')
        scope = group.rsplit('/',1)[-1]
        if daemon['MainPID']==parent and Path('/proc',parent).stat().st_uid==0 and scope.startswith('session-') and scope.endswith('.scope'):
            session=scope[8:-6]
            require(all(c.isalnum() or c in '_-' for c in session), 'invalid session identifier')
            login=query('/usr/bin/loginctl','show-session',session,'-p','Leader','-p','Service','-p','Remote','-p','Scope','-p','User')
            verified=login=={'Leader':path.name,'Service':'tailscaled','Remote':'yes','Scope':scope,'User':str(os.getuid())}
    after = process(int(path.name))
    require(before==after, 'supervisor lifetime changed during query')
    return verified


def source_manifest(lane):
    paths = [lane / 'Cargo.lock', lane / 'Cargo.toml']
    paths += sorted(p for p in (lane / 'crates').rglob('*') if p.is_file())
    paths += sorted(p for p in (lane / 'scripts').rglob('*') if p.is_file())
    return {str(p.relative_to(lane)): digest(p) for p in paths}


def ready_record(path, lane):
    """Accept core's exact final build receipt, retaining older scoped fixtures."""
    private_file(path)
    value = json.loads(path.read_text())
    if 'source_manifest' in value:
        return value
    build_path = Path(value['build']); private_file(build_path)
    build = json.loads(build_path.read_text())
    frozen = source_manifest(lane)
    require(value['units'] == '28 passed, 0 failed, 3 ignored', 'core units not ready')
    require(value['source_sha256'] == build['source_sha256'] and
            value['cli_sha256'] == build['cli_sha256'] and
            value['guest_sha256'] == build['guest_sha256'], 'core receipt/build mismatch')
    require(hashlib.sha256(json.dumps(build['source'], sort_keys=True).encode()).hexdigest() == value['source_sha256'],
            'core source fingerprint mismatch')
    require(all(frozen.get(k) == v for k,v in build['source'].items()), 'core receipt/current source mismatch')
    bundle = Path(value['bundle']); private_file(bundle / 'manifest.json')
    if 'bundle_manifest_sha256' in value:
        require(digest(bundle / 'manifest.json') == value['bundle_manifest_sha256'], 'core bundle receipt mismatch')
    require(json.loads((bundle / 'build.json').read_text()) == build, 'core bundle build mismatch')
    return {**value, 'unit_build_ready': True, 'source_manifest': frozen,
            'binary_sha256': value['cli_sha256']}


def wire_status(root):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(3)
        client.connect(str(root / 'control.sock'))
        client.sendall(b'{"op":"status"}\n')
        with client.makefile('rb') as stream:
            body = stream.readline(1048577)
        require(len(body) <= 1048576, 'worker status oversized')
        value = json.loads(body)
        require(value.get('ok'), 'worker status rejected')
        return value['result']


def inspect(args, result):
    inventory = guests()
    result['host_inventory'] = inventory
    count = len(inventory)
    reserved = sum(v['memory_mib'] for v in inventory)
    available = int(next(x.split()[1] for x in Path('/proc/meminfo').read_text().splitlines()
                         if x.startswith('MemAvailable:'))) // 1024
    result['headroom'] = {'guests': count, 'configured_mib': reserved, 'available_mib': available,
                          'planned_test_guests': 3, 'planned_test_mib': 768,
                          'minimum_host_reserve_mib': args.host_reserve_mib}
    require(count == 1 and reserved == 2048 and inventory[0]['cpus'] == 2,
            'baseline differs from planner running1/2048MiB/2vCPU; resolve before VM turn')
    require(count + 3 <= 4 and reserved + 768 <= 3072,
            'global admission cap unavailable')
    require(available >= 768 + args.host_reserve_mib, 'physical RAM reserve unavailable')
    # Never copy/hash running owner disks. Record protected path metadata only.
    protected = {}
    for root in (REPO / 'data/prototype/machines', REPO / 'data/prototype/snapshots'):
        for path in root.rglob('*'):
            info = path.lstat()
            protected[str(path.relative_to(REPO))] = {
                'inode': info.st_ino, 'size': info.st_size, 'mode': stat.S_IMODE(info.st_mode)}
    result['protected_artifact_metadata'] = protected
    with sqlite3.connect('file:' + str(REPO / 'data/prototype/catalog.sqlite3') + '?mode=ro', uri=True) as db:
        unresolved = db.execute("SELECT count(*) FROM operations WHERE state IN ('pending','uncertain')").fetchone()[0]
        usage = db.execute('SELECT node,extra_mib,extra_cpus,extra_slots FROM node_usage').fetchall()
        result['catalog_demand'] = {'unresolved_operations': unresolved, 'extra_usage': usage}
        require(unresolved == 0 and all(row[1:] == (0, 0, 0) for row in usage),
                'pending/uncertain or unaccounted node demand blocks VM admission')
    result['worker_status'] = {}
    for name, root in [('legacy', REPO / 'data/prototype'), ('a', REPO / 'data/nl/a'),
                       ('b', REPO / 'data/nl/b')]:
        result['worker_status'][name] = wire_status(root)
    require(result['worker_status']['legacy']['running'] == 1,
            'legacy inventory differs from protected baseline')
    for name in ('a', 'b'):
        status = result['worker_status'][name]
        require(status['running'] == 0, 'unexpected live node guest; resolve demand')
        require(tuple(status[k] for k in ('max_memory_mib', 'max_running', 'max_vcpus')) == (512, 2, 2),
                'existing node budget differs from planner baseline')
    descriptors = list((REPO / 'data/nl/processes').glob('*.json'))
    descriptors += list((REPO / 'data/remote-access').glob('*-process.json'))
    result['baseline_processes'] = {}
    for path in descriptors:
        record = json.loads(path.read_text())
        observed = process(record['pid'])
        require(observed['uid'] == os.getuid(), 'descriptor process owner mismatch')
        require(observed['ticks'] == str(record.get('ticks', record.get('start_ticks'))),
                'descriptor process start mismatch')
        require(observed['state'] != 'Z' and observed['argv'], 'baseline service exited')
        if 'argv' in record:
            require(observed['argv'] == [x.encode() for x in record['argv']], 'baseline argv mismatch')
        observed['argv'] = [os.fsdecode(x) for x in observed['argv']]
        result['baseline_processes'][str(path.relative_to(REPO))] = observed
    result['baseline_files'] = {}
    for name in ('tunnel.json', 'gateway.json', 'managed-access.json', 'cli-access.json'):
        path = REPO / 'data/remote-access' / name
        result['baseline_files'][name] = digest(path)
    result['source'] = source_manifest(args.lane)
    result['source_commit'] = subprocess.check_output(
        ['git', '-C', str(args.lane), 'rev-parse', 'HEAD'], text=True).strip()
    result['checks']['baseline-continuity'] = 'observed baseline only; after-test comparison pending'


def cli(args, result):
    binary = args.binary.resolve(strict=True)
    require(binary.is_file() and os.access(binary, os.X_OK), 'installed binary unavailable')
    result['binary_sha256'] = digest(binary)
    with tempfile.TemporaryDirectory(prefix='hv-', dir=REPO / 'data') as temporary:
        root = Path(temporary)
        env = {'HOME': str(root), 'PATH': '/usr/bin:/bin', 'XDG_CONFIG_HOME': str(root / 'config'),
               'XDG_DATA_HOME': str(root / 'share'), 'OW_DATA_DIR': str(root / 'host')}
        checks = [('installed-host-commands', ['host', '--help'], 0),
                  ('doctor', ['host', 'doctor'], None)]
        for name, command, expected in checks:
            p = subprocess.run([str(binary), *command], env=env, cwd=root,
                               capture_output=True, timeout=30)
            # Raw doctor output is private; never echo host values or credentials.
            (root / (name + '.stdout')).write_bytes(p.stdout)
            (root / (name + '.stderr')).write_bytes(p.stderr)
            require(expected is None or p.returncode == expected, 'host command unavailable')
            if name == 'installed-host-commands':
                require(all(x in p.stdout for x in (b'doctor', b'join', b'start', b'status', b'stop')),
                        'host subcommands missing from installed binary')
            result['checks'][name] = {'exit': p.returncode, 'stdout_sha256': hashlib.sha256(p.stdout).hexdigest(),
                                      'stderr_sha256': hashlib.sha256(p.stderr).hexdigest(),
                                      'meaning': 'command execution only; diagnostic content requires review'}
    require(digest(binary) == result['binary_sha256'], 'binary changed during CLI checks')


def http_cases(args, result):
    private_file(args.fixture)
    require(args.fixture.resolve().is_relative_to(PRIVATE.resolve()), 'fixture outside owned ignored root')
    fixture = json.loads(args.fixture.read_text())
    origin = urlsplit(fixture['origin'])
    require(origin.scheme == 'https' and not origin.username and not origin.query
            and not origin.fragment and origin.path in ('', '/'), 'fixture needs HTTPS origin only')
    require(ipaddress.ip_address(origin.hostname).is_loopback, 'only literal loopback fixture permitted')
    require(origin.port not in (None, 443, 8787, 8790), 'live/default service ports prohibited')
    server = process(fixture['server_pid'])
    require(server['uid'] == os.getuid() and server['state'] != 'Z', 'fixture server identity invalid')
    require(server['ticks'] == str(fixture['server_ticks']), 'fixture server PID reused')
    require(any(os.fsdecode(x).startswith(str(PRIVATE) + '/') for x in server['argv']),
            'server command must identify dedicated fixture data/config under private root')
    require(server['pid'] not in [json.loads(p.read_text())['pid'] for p in
            list((REPO / 'data/nl/processes').glob('*.json')) +
            list((REPO / 'data/remote-access').glob('*-process.json'))], 'managed live service refused')
    context = ssl.create_default_context(cafile=fixture['ca_file'])
    frozen = fixture['source_manifest']
    require(frozen == source_manifest(args.lane), 'fixture source manifest differs from frozen lane')
    cases = fixture['cases']
    require(isinstance(cases, list) and len(cases) <= 100, 'invalid/big request fixture')
    before = guests()
    seen = set()
    for case in cases:
        name = case['id']
        require(name in REQUIRED and name not in seen, 'unknown/duplicate case id')
        seen.add(name)
        expected_status = case['expect_status']
        require(isinstance(expected_status, list) and expected_status, 'missing status assertion')
        if 'denial' in name:
            require(set(expected_status) <= {400, 401, 403, 404, 405, 409, 410, 422},
                    'denial case cannot accept success/redirect/server failure')
        path, method = case['path'], case.get('method', 'GET')
        require(path.startswith('/') and not any(x in path for x in '\r\n?#'), 'invalid request path')
        require(method in ('GET', 'POST', 'PUT', 'DELETE'), 'invalid request method')
        # Excludes guest API and host lifecycle. POSTs are only enrollment/control denial fixtures.
        require(path.startswith('/_nodes') or path in ('/api/hosts/invite', '/api/hosts/revoke', '/'),
                'request outside bounded onboarding fixture paths')
        headers = dict(case.get('headers', {}))
        body = json.dumps(case['json']).encode() if 'json' in case else None
        require(body is None or len(body) <= 65536, 'fixture body oversized')
        if body is not None:
            headers['Content-Type'] = 'application/json'
        wskey = None
        if case.get('websocket'):
            require(method == 'GET', 'WebSocket must use GET')
            wskey = base64.b64encode(os.urandom(16)).decode()
            headers.update({'Upgrade': 'websocket', 'Connection': 'Upgrade',
                            'Sec-WebSocket-Key': wskey, 'Sec-WebSocket-Version': '13'})
        connection = http.client.HTTPSConnection(origin.hostname, origin.port or 443,
                                                 context=context, timeout=10)
        try:
            connection.request(method, path, body=body, headers=headers)
            response = connection.getresponse()
            require(response.status in case['expect_status'], 'unexpected HTTPS status')
            require(response.status not in (301, 302, 303, 307, 308), 'node fixture redirected')
            if response.status == 101:
                expected = base64.b64encode(hashlib.sha1((wskey +
                    '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
                require(response.getheader('Sec-WebSocket-Accept') == expected, 'bad WebSocket accept')
            else:
                data = response.read(1048577)
                require(len(data) <= 1048576, 'response oversized')
                if 'expect_json' in case:
                    value = json.loads(data)
                    require(all(value.get(k) == v for k, v in case['expect_json'].items()),
                            'response JSON mismatch')
            result['checks'][name] = {'status': response.status,
                                     'meaning': 'local synthetic HTTP or upgrade handshake; no lifecycle proof'}
        finally:
            connection.close()
    require(frozen == source_manifest(args.lane), 'product source changed during HTTP checks')
    require(before == guests(), 'baseline VMM identity/demand changed during no-VM requests')


def compare(args, result, allow_other=False):
    baseline = json.loads((PRIVATE / 'inspect-result.json').read_text())
    require('error_type' not in baseline, 'baseline scan unresolved')
    current = guests()
    if allow_other:
        require(all(any(x['pid']==b['pid'] and x['ticks']==b['ticks'] and x['memory_mib']==b['memory_mib']
                        for x in current) for b in baseline['host_inventory']), 'owner VMM baseline identity/demand changed')
    else:
        require(current == baseline['host_inventory'],
                'baseline VMM changed or additional guests remain; owner guest is expected')
    result['host_inventory'] = current
    for record in baseline['baseline_processes'].values():
        observed = process(record['pid'])
        require(observed['uid'] == record['uid'] and observed['ticks'] == record['ticks']
                and observed['state'] != 'Z', 'baseline service identity lost')
        require(observed['argv'] == [x.encode() for x in record['argv']], 'baseline argv changed')
    for name, expected in baseline['baseline_files'].items():
        require(digest(REPO / 'data/remote-access' / name) == expected, 'live ingress/config changed')
    for name, expected in baseline['protected_artifact_metadata'].items():
        info = (REPO / name).lstat()
        require(info.st_ino == expected['inode'] and info.st_size == expected['size']
                and stat.S_IMODE(info.st_mode) == expected['mode'],
                'protected disk/checkpoint metadata changed')
    result['checks']['baseline-continuity'] = 'VMM/process identities, ingress hashes and artifact metadata matched'
    result['checks']['test-cleanup'] = 'no additional VMMs; no processes/guests started by this preparatory harness'


# Appended into the owned focused harness below.
POOL_POLICY = ('Trusted shared private pool: all admitted workspace users may select this host. '
               'The host operator can inspect guest data and stop participation. '
               'This invitation grants node participation only, not human workspace management.')
ASSET_LAYOUT = {
    'firecracker': ('official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64', True),
    'vmlinux': ('downloads/vmlinux-6.1.186', False),
    'base.ext4': ('guest/base.ext4', False),
    'ow-guest': ('guest/ow-guest', True),
    'slirp4netns': ('bin/slirp4netns', True),
}


class NoVM:
    """Actual CLI paths against synthetic files/TLS and bounded fake worker status.

    No worker, agent or VMM is launched. `join` always has --no-start. `start`
    cases must fail asset validation or a deliberately mismatched fake worker.
    """
    def __init__(self, args, result):
        import shutil
        self.args, self.result = args, result
        self.binary = args.binary.resolve(strict=True)
        private_file(args.ready_receipt)
        receipt = ready_record(args.ready_receipt, args.lane)
        require(receipt.get('unit_build_ready') is True, 'core unit-build readiness required')
        self.frozen = source_manifest(args.lane)
        require(receipt['source_manifest'] == self.frozen, 'readiness receipt source differs')
        self.binary_hash = digest(self.binary)
        require(receipt['binary_sha256'] == self.binary_hash, 'readiness receipt binary differs')
        self.result['tested_product_source'] = self.frozen
        self.result['binary_sha256'] = self.binary_hash
        self.result['details'] = {}
        # Short real canonical roots are required by onboarding's <60-byte bound.
        self.root = Path(tempfile.mkdtemp(prefix='hv-', dir=REPO / 'data'))
        require(len(str(self.root / 'h')) < 60, 'dedicated root exceeds onboarding bound')
        self.result['fixture_root'] = str(self.root.relative_to(REPO))
        self.home = self.root / 'h'
        self.home.mkdir(mode=0o700)
        self.env = {'HOME': str(self.home), 'PATH': '/usr/sbin:/usr/bin:/sbin:/bin',
                    'XDG_CONFIG_HOME': str(self.home / '.config'), 'LANG': 'C'}
        self.counter = 0
        self.servers, self.threads = [], []
        self.hits = []
        self.failures = 0
        self.install = self.home / '.local/bin/ow'
        self.install.parent.mkdir(mode=0o700, parents=True)
        shutil.copyfile(self.binary, self.install)
        self.install.chmod(0o700)
        self.result['install_evidence'] = 'private clean-HOME binary copy, not curl-installer acceptance'

    def new_root(self):
        self.counter += 1
        root = self.root / str(self.counter)
        root.mkdir(mode=0o700)
        require(len(str(root)) < 60, 'case root exceeds onboarding bound')
        return root

    def check(self, name, fn):
        try:
            detail = fn()
            self.result['details'][name] = {'state': 'passed', 'evidence': detail}
        except Exception as error:
            self.failures += 1
            self.result['details'][name] = {'state': 'failed', 'error_type': type(error).__name__,
                                           'private_error': str(error), 'private_traceback': __import__('traceback').format_exc()}
        save(PRIVATE / 'no-vm-result.json', self.result)

    def call(self, root, action, success=None, contains=None, env=None):
        allowed = action[0] in ('doctor', 'status', 'join', 'start', 'stop', '--help')
        require(allowed and action[0] != 'run', 'unbounded host command refused')
        if action[0] == 'join':
            require('--no-start' in action, 'join adapter must never start processes')
        p = subprocess.run([str(self.install), '--data-dir', str(root), 'host', *action],
                           cwd=self.home, env=env or self.env, input=b'',
                           capture_output=True, timeout=40)
        stem = f'{self.counter}-{len(list(self.root.glob("*.stdout")))}'
        (self.root / (stem + '.stdout')).write_bytes(p.stdout)
        (self.root / (stem + '.stderr')).write_bytes(p.stderr)
        if success is not None:
            require((p.returncode == 0) == success, 'unexpected installed host command exit')
        if contains is not None:
            require(contains in p.stdout + p.stderr, 'required actionable diagnostic absent')
        return p

    def no_effect(self, root, previous_hits):
        require(len(self.hits) == previous_hits, 'invalid input contacted download/enrollment server')
        require(not any((root / p).exists() for p in ('node.json', 'host.json', 'control.sock')),
                'denied join wrote credentials/config or control socket')

    def invitation(self):
        return {'node': 'synthetic-host', 'secret': 'a' * 64, 'expires': int(time.time()) + 600,
                'controller': self.origin + '/_nodes', 'memory': 512, 'slots': 2,
                'cpus': 2, 'policy': POOL_POLICY}

    def invite_file(self, value, mode=0o600):
        self.counter += 1
        file = self.root / f'invite-{self.counter}.json'
        file.write_text(json.dumps(value))
        file.chmod(mode)
        return file

    def join(self, root, invite, success, contains=None, extra=None):
        return self.call(root, ['join', '--invite-file', str(invite), '--memory', '512',
                               '--slots', '2', '--cpus', '2', '--storage-gib', '1',
                               '--accept-shared-pool', '--no-start', '--ca-cert', str(self.ca_file), *(extra or [])],
                         success, contains)

    def tls_server(self):
        import http.server
        import threading
        cert, key = self.root / 'local.pem', self.root / 'local.key'
        ca, ca_key, csr, ext = [self.root / n for n in ('ca.pem', 'ca.key', 'local.csr', 'local.ext')]
        def openssl(*argv):
            subprocess.run(['openssl', *map(str, argv)], check=True, capture_output=True, timeout=30)
        openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', ca_key,
                '-out', ca, '-days', '1', '-subj', '/CN=Host verification fixture CA',
                '-addext', 'basicConstraints=critical,CA:TRUE',
                '-addext', 'keyUsage=critical,keyCertSign,cRLSign')
        openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
                '-out', csr, '-subj', '/CN=localhost')
        ext.write_text('subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
        openssl('x509', '-req', '-in', csr, '-CA', ca, '-CAkey', ca_key, '-CAcreateserial',
                '-out', cert, '-days', '1', '-extfile', ext)
        self.blobs = {name: ('synthetic inert asset ' + name).encode() for name in ASSET_LAYOUT}
        self.manifest = {'version': 1, 'runtime': 'firecracker-v1.17.0', 'arch': 'x86_64',
                         'files': [{'name': n, 'size': len(b), 'sha256': hashlib.sha256(b).hexdigest()}
                                   for n, b in self.blobs.items()]}
        self.response_mode = 'valid'
        self.enrollment_status = 200
        outer = self
        class Edge(http.server.BaseHTTPRequestHandler):
            def log_message(*args):
                pass
            def respond(handler, status, body, **headers):
                handler.send_response(status)
                handler.send_header('Content-Length', str(len(body)))
                for k, v in headers.items():
                    handler.send_header(k, v)
                handler.end_headers()
                handler.wfile.write(body)
            def do_GET(handler):
                outer.hits.append(('GET', handler.path))
                if handler.path == '/cli/ow-linux-amd64':
                    body = outer.binary.read_bytes()
                    if outer.response_mode == 'installer-corrupt':
                        body = body[:-1] + bytes([body[-1] ^ 1])
                    return handler.respond(200, body)
                if handler.path == '/cli/ow-linux-amd64.sha256':
                    return handler.respond(200, (outer.binary_hash + '\n').encode())
                if handler.path == '/cli/host-manifest.json':
                    if outer.response_mode == 'redirect':
                        return handler.respond(302, b'', Location=outer.origin + '/redirect-sink')
                    manifest = json.loads(json.dumps(outer.manifest))
                    if outer.response_mode == 'path':
                        manifest['files'][0]['name'] = '../outside'
                    if outer.response_mode == 'oversized-declaration':
                        manifest['files'][0]['size'] = 2 ** 40
                    if outer.response_mode == 'duplicate':
                        manifest['files'][0] = manifest['files'][1]
                    if outer.response_mode == 'unknown-destination':
                        manifest['files'][0]['destination'] = '../outside'
                    body = json.dumps(manifest).encode()
                    if outer.response_mode == 'manifest-overflow':
                        body += b' ' * 17000
                    return handler.respond(200, body)
                name = handler.path.removeprefix('/cli/host/')
                if name not in outer.blobs:
                    return handler.respond(404, b'unknown')
                blob = outer.blobs[name]
                if outer.response_mode == 'corrupt':
                    blob = b'X' * len(blob)
                if outer.response_mode == 'truncated':
                    blob = blob[:-1]
                if outer.response_mode == 'body-overflow':
                    blob += b'X'
                return handler.respond(200, blob)
            def do_POST(handler):
                length = int(handler.headers.get('Content-Length', 0))
                require(length < 65536, 'synthetic enrollment body exceeded bound')
                body = json.loads(handler.rfile.read(length))
                outer.hits.append(('POST', handler.path))
                if getattr(outer, 'enrollment_hook', None) and handler.path == '/_nodes/enroll':
                    outer.enrollment_hook()
                if handler.path == '/api/hosts/invite':
                    return handler.respond(200, json.dumps({'ok': True, 'result': outer.invitation()}).encode())
                require(handler.path == '/_nodes/enroll', 'join did not use prefixed enrollment')
                require(set(body) == {'node', 'secret'}, 'join sends excess enrollment authority')
                return handler.respond(outer.enrollment_status,
                    json.dumps({'node': body['node'], 'credential': 'b' * 64}).encode())
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Edge)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
        self.servers.append(server)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        self.threads.append(thread)
        thread.start()
        self.origin = f'https://127.0.0.1:{server.server_port}'
        self.ca_file = ca
        self.env['SSL_CERT_FILE'] = str(ca)
        self.env['SSL_CERT_DIR'] = str(self.root / 'no-cert-dir')

    def installer(self):
        # Real Linux installer and curl TLS/downloads. Helper acquisition is excluded
        # by a private preinstalled inert helper; no GitHub requests are made.
        helper = self.home / '.local/lib/open-workspaces/cloudflared'
        helper.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        helper.write_text('#!/bin/sh\nexit 99\n'); helper.chmod(0o700)
        installer = self.root / 'install.sh'
        template = (self.args.lane / 'crates/ow/cli/install.sh').read_text()
        require('@@ORIGIN@@' in template, 'installer origin template changed')
        installer.write_text(template.replace('@@ORIGIN@@', self.origin))
        env = dict(self.env, CURL_CA_BUNDLE=self.env['SSL_CERT_FILE'], TMPDIR=str(self.root))
        before = len(self.hits)
        p = subprocess.run(['/bin/sh', str(installer)], env=env, cwd=self.home,
                           capture_output=True, timeout=40)
        require(p.returncode == 0 and digest(self.install) == self.binary_hash,
                'real user-space portable installer failed or binary digest differs')
        require(any(path == '/cli/ow-linux-amd64' for _, path in self.hits[before:]),
                'installer did not download served portable artifact')
        self.installed_help()
        self.response_mode = 'installer-corrupt'
        p = subprocess.run(['/bin/sh', str(installer)], env=env, cwd=self.home,
                           capture_output=True, timeout=40)
        self.response_mode = 'valid'
        require(p.returncode != 0 and digest(self.install) == self.binary_hash,
                'corrupt CLI download replaced prior verified install')
        self.result['install_evidence'] = 'actual Linux installer/curl TLS; private preinstalled inert cloudflared helper'
        return {'portable_binary_sha256': self.binary_hash, 'corrupt_update': 'rejected; previous install retained',
                'helper_download': 'excluded; inert preinstalled fixture helper', 'public_release': False}

    def owner_output_preflight(self):
        # HR-10: a synthetic HTTPS responder records any remote mutation attempt.
        provider = self.root / 'fake-token-provider'
        provider.write_text("#!/bin/sh\nprintf '%s\\n' a.b.c\n")
        provider.chmod(0o700)
        env = dict(self.env, OW_CLOUDFLARED=str(provider))
        public = self.root / 'public'
        public.mkdir(mode=0o755); public.chmod(0o755)
        for output in (public / 'invite.json', self.root / 'missing/invite.json', None):
            before = len(self.hits)
            command = [str(self.install), '--server', self.origin, 'host', 'invite', 'synthetic-output']
            if output is not None:
                command += ['--output', str(output)]
            p = subprocess.run(command, env=env, cwd=self.home, input=b'', capture_output=True, timeout=20)
            require(p.returncode != 0, 'unsafe invitation output accepted')
            require(not any(method == 'POST' for method, _ in self.hits[before:]),
                    'HR-10 unsafe output validation happened after remote invitation mutation')
        return 'public/missing output parent and non-TTY stdout denied before HTTPS POST'

    def installed_help(self):
        p = self.call(self.new_root(), ['--help'], True)
        require(all(x in p.stdout for x in (b'doctor', b'join', b'start', b'status', b'stop', b'invite', b'revoke')),
                'installed binary host action list incomplete')
        return 'all current HostAction commands advertised by clean-HOME installed copy'

    def doctor(self):
        root = self.new_root()
        # Ambient/saved human login must not redirect participation.
        config = self.home / '.config/open-workspaces/client.json'
        config.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        config.write_text('{"server":"https://127.0.0.1:9"}')
        env = dict(self.env, OW_SERVER='https://127.0.0.1:9', OW_CLOUDFLARED='/definitely-absent')
        p = self.call(root, ['doctor'], True, env=env)
        report = json.loads(p.stdout)
        require(type(report.get('supported')) is bool and isinstance(report.get('diagnostics'), list),
                'doctor lacks structured support diagnostics')
        require(all(k in report['demand'] for k in ('existing_vmm_count', 'host_available_mib')),
                'doctor missing host demand')
        require(not list(root.glob('doctor-*')), 'doctor left reflink probe files')
        self.host_supported = report['supported']
        self.result['checks']['doctor'] = 'executed structured diagnostic; actual support captured privately'
        self.result['checks']['doctor-kvm-storage-namespace-tools-capacity'] = 'actual host only; unsupported-host matrix pending'
        return report

    def invalid_envelopes(self):
        modifications = [('policy', 'private'), ('expires', int(time.time()) - 1),
                         ('controller', self.origin + '/evil'), ('controller', 'http://127.0.0.1:9'),
                         ('controller', 'https://user:pass@127.0.0.1/_nodes'),
                         ('cpus', 17), ('slots', 0), ('memory', 2 ** 64 - 1), ('secret', 'bad')]
        for field, value in modifications:
            root = self.new_root()
            invite = self.invitation()
            invite[field] = value
            before = len(self.hits)
            self.join(root, self.invite_file(invite), False)
            self.no_effect(root, before)
        root = self.new_root()
        before = len(self.hits)
        self.join(root, self.invite_file(self.invitation(), 0o644), False, b'private')
        self.no_effect(root, before)
        self.result['checks']['expiry-denial'] = 'CLI envelope denial before network; server expiry separately pending'
        return {'invalid_cases': len(modifications) + 1, 'network_requests': 0}

    def non_tty(self):
        root = self.new_root()
        before = len(self.hits)
        self.call(root, ['join', '--no-start'], False, b'--invite-file')
        self.no_effect(root, before)
        return 'bare join non-TTY gives actionable private-file alternative'

    def explicit_server(self):
        root = self.new_root()
        p = subprocess.run([str(self.install), '--server', self.origin, '--data-dir', str(root),
                            'host', 'doctor'], env=self.env, cwd=self.home,
                           capture_output=True, input=b'', timeout=10)
        require(p.returncode != 0 and b'local' in p.stderr, 'explicit human server accepted for participation')
        return 'explicit --server rejected before host action'

    def hidden_cancel(self):
        import pty
        import select
        import termios
        root = self.new_root()
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        p = subprocess.Popen([str(self.install), '--data-dir', str(root), 'host', 'join', '--no-start'],
                             env=self.env, cwd=self.home, stdin=slave, stdout=slave, stderr=slave)
        output = bytearray()
        try:
            end = time.monotonic() + 8
            while b'Paste invitation' not in output:
                require(time.monotonic() < end and p.poll() is None, 'hidden prompt did not appear')
                if select.select([master], [], [], .1)[0]:
                    output.extend(os.read(master, 65536))
            # Wait for the actual echo-disabled state rather than racing prompt output.
            while termios.tcgetattr(slave)[3] & termios.ECHO:
                require(time.monotonic() < end, 'hidden prompt did not disable echo')
                time.sleep(.01)
            marker = b'SYNTHETIC_SECRET_MUST_NOT_ECHO'
            os.write(master, marker + b'\x03' + b'TAIL_SECRET_MUST_BE_DISCARDED')
            require(p.wait(timeout=5) != 0, 'cancelled join succeeded')
            while select.select([master], [], [], .05)[0]:
                output.extend(os.read(master, 65536))
            require(marker not in output, 'hidden input was echoed')
            require(termios.tcgetattr(slave) == original, 'join cancellation failed to restore termios')
            import fcntl
            flags = fcntl.fcntl(slave, fcntl.F_GETFL)
            probe = termios.tcgetattr(slave); probe[3] &= ~termios.ICANON
            try:
                termios.tcsetattr(slave, termios.TCSANOW, probe)
                fcntl.fcntl(slave, fcntl.F_SETFL, flags | os.O_NONBLOCK)
                try: pending = os.read(slave, 65536)
                except BlockingIOError: pending = b''
                require(not pending, 'cancelled secret suffix remained queued for parent shell')
            finally:
                fcntl.fcntl(slave, fcntl.F_SETFL, flags)
                termios.tcsetattr(slave, termios.TCSANOW, original)
            require(not (root / 'node.json').exists() and not (root / 'host.json').exists(),
                    'cancellation persisted participation')
            return 'real PTY Ctrl-C without Enter, hidden echo, restored termios, no queued secret/config/credential'
        finally:
            if p.poll() is None:
                p.terminate()
                p.wait(timeout=5)
            os.close(master)
            os.close(slave)

    def synthetic_config(self, root):
        config = {'version': 1, 'controller': self.origin + '/_nodes', 'memory': 512,
                  'slots': 2, 'cpus': 2, 'storage_gib': 1, 'policy': POOL_POLICY}
        (root / 'host.json').write_text(json.dumps(config))
        (root / 'host.json').chmod(0o600)
        (root / 'node.json').write_text(json.dumps({'node': 'synthetic-host', 'credential': 'b' * 64}))
        (root / 'node.json').chmod(0o600)

    def put_assets(self, root):
        assets = root / 'assets'
        assets.mkdir(mode=0o700)
        for name, (dest, executable) in ASSET_LAYOUT.items():
            target = assets / dest
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            target.write_bytes(self.blobs[name])
            target.chmod(0o700 if executable else 0o600)
        (assets / 'manifest.json').write_text(json.dumps(self.manifest))
        (assets / 'manifest.json').chmod(0o600)
        return assets

    def asset_rejection(self):
        # Every start test is deliberately invalid; inert fake binaries are never spawned.
        mutations = ('corrupt', 'truncated', 'oversized', 'symlink-file', 'symlink-parent',
                     'public-file', 'missing', 'manifest-path', 'manifest-extra', 'manifest-size')
        for mutation in mutations:
            root = self.new_root()
            self.synthetic_config(root)
            assets = self.put_assets(root)
            target = assets / ASSET_LAYOUT['firecracker'][0]
            if mutation == 'corrupt':
                target.write_bytes(b'X' * len(self.blobs['firecracker']))
            elif mutation == 'truncated':
                target.write_bytes(self.blobs['firecracker'][:-1])
            elif mutation == 'oversized':
                target.write_bytes(self.blobs['firecracker'] + b'X')
            elif mutation == 'symlink-file':
                target.unlink(); target.symlink_to(self.install)
            elif mutation == 'symlink-parent':
                target.rename(root / 'outside')
                target.parent.rmdir(); target.parent.symlink_to(self.home, target_is_directory=True)
            elif mutation == 'public-file':
                target.chmod(0o777)
            elif mutation == 'missing':
                target.unlink()
            else:
                manifest = json.loads(json.dumps(self.manifest))
                if mutation == 'manifest-path':
                    manifest['files'][0]['name'] = '../outside'
                if mutation == 'manifest-extra':
                    manifest['files'][0]['destination'] = '../outside'
                if mutation == 'manifest-size':
                    manifest['files'][0]['size'] = 2 ** 40
                (assets / 'manifest.json').write_text(json.dumps(manifest))
            p = self.call(root, ['start'], False)
            require(b'worker startup' not in p.stderr, 'asset rejection reached worker startup')
            require(not (root / 'control.sock').exists(), 'invalid asset fixture started worker')
        self.result['checks']['artifact-hash-size-corruption'] = 'installed CLI start denied local malformed inert assets'
        self.result['checks']['artifact-path-symlink-extraction'] = 'fixed names and symlink components rejected; bundle has no archive consumer'
        return {'actual_start_denials': len(mutations), 'executed_assets': 0}

    def fake_worker(self, root, status):
        import threading
        server = socket.socket(socket.AF_UNIX)
        server.bind(str(root / 'control.sock'))
        os.chmod(root / 'control.sock', 0o600)
        server.listen(5)
        server.settimeout(.1)
        stopping = threading.Event()
        operations = []
        def serve():
            while not stopping.is_set():
                try:
                    client, _ = server.accept()
                except socket.timeout:
                    continue
                except OSError:
                    return
                with client:
                    client.settimeout(3)
                    request = json.loads(client.makefile('rb').readline(65536))
                    operations.append(request['op'])
                    client.sendall(json.dumps({'ok': True, 'result': status}).encode() + b'\n')
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        return server, stopping, thread, operations

    def lifecycle_denials(self):
        root = self.new_root()
        self.call(root, ['status'], False)
        self.call(root, ['stop'], False)
        self.synthetic_config(root)
        self.put_assets(root)
        status = {'max_memory_mib': 4096, 'max_running': 8, 'max_vcpus': 16,
                  'host_managed': False, 'min_free_gib': 0, 'host_asset_hash': 'wrong'}
        server, stopping, thread, operations = self.fake_worker(root, status)
        try:
            self.call(root, ['start'], False, b'refusing reuse')
            self.call(root, ['stop'], False, b'refusing to stop')
            p = self.call(root, ['status'], True)
            require(json.loads(p.stdout)['worker_matches_saved_limits'] is False,
                    'mismatched worker advertised as matching')
            require(operations and set(operations) == {'status'}, 'denial sent shutdown/mutation to fake worker')
            return {'fake_worker_operations': operations, 'real_worker_start_stop': False}
        finally:
            stopping.set(); server.close(); thread.join(timeout=3)
            (root / 'control.sock').unlink(missing_ok=True)

    def download_cases(self):
        if not hasattr(self, 'host_supported'):
            self.doctor()
        require(self.host_supported, 'unsupported local host; actual join downloads cannot bypass doctor')
        modes = ('redirect', 'path', 'duplicate', 'unknown-destination', 'oversized-declaration',
                 'manifest-overflow', 'corrupt', 'truncated', 'body-overflow')
        for mode in modes:
            self.response_mode = mode
            root = self.new_root()
            before = len(self.hits)
            self.join(root, self.invite_file(self.invitation()), False)
            requests = self.hits[before:]
            require(requests and requests[0][1] == '/cli/host-manifest.json',
                    'download fixture failed before actual HTTPS manifest request')
            require(not any(method == 'POST' for method, _ in requests), 'invalid bundle redeemed invitation')
            require(not (root / 'assets').exists() and not list(root.glob('assets-stage-*')),
                    'invalid download published assets or left staging')
            require(not (root / 'node.json').exists(), 'invalid download persisted credentials')
        self.response_mode = 'valid'
        self.enrollment_status = 401
        root = self.new_root()
        self.join(root, self.invite_file(self.invitation()), False, b'enrollment rejected')
        require((root / 'assets/manifest.json').exists() and not (root / 'node.json').exists(),
                'rejected enrollment state invalid')
        # Actual TLS/download/private persistence with synthetic controller authority.
        self.enrollment_status = 200
        root = self.new_root()
        p = self.join(root, self.invite_file(self.invitation()), True)
        require(b'b' * 64 not in p.stdout + p.stderr and b'a' * 64 not in p.stdout + p.stderr,
                'join exposed synthetic enrollment secrets')
        for name in ('node.json', 'host.json', '.ow-data'):
            private_file(root / name)
        require(not (root / 'control.sock').exists() and not (root / 'node-agent.lock').exists(),
                '--no-start launched participation')
        self.call(root, ['status'], True)
        self.result['checks']['private-config-credential'] = 'actual no-start join mode0600/0700 and status, synthetic enrollment'
        return {'negative_download_modes': list(modes), 'positive_join': '--no-start; synthetic TLS controller/inert verified assets',
                'runtime_authentication': 'not established by synthetic enrollment'}

    def guided_and_recovery(self):
        import pty
        import select
        import termios
        self.doctor()
        require(self.host_supported, 'V2 needs a supported scratch Btrfs root')
        root = self.new_root()
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        process = subprocess.Popen([str(self.install), '--data-dir', str(root), 'host', 'join', '--no-start', '--ca-cert', str(self.ca_file)],
                                   env=self.env, cwd=self.home, stdin=slave, stdout=slave, stderr=slave)
        output = bytearray()
        invitation = self.invitation()
        def prompt(marker):
            end = time.monotonic() + 15
            while marker not in output:
                require(time.monotonic() < end and process.poll() is None, 'guided prompt missing')
                if select.select([master], [], [], .1)[0]:
                    output.extend(os.read(master, 65536))
        try:
            prompt(b'Paste invitation')
            end = time.monotonic() + 3
            while termios.tcgetattr(slave)[3] & termios.ECHO:
                require(time.monotonic() < end, 'hidden invitation echo remained enabled')
                time.sleep(.01)
            os.write(master, json.dumps(invitation).encode() + b'\n')
            for marker, answer in [(b'Type yes:', b'yes'), (b'Disjoint guest RAM budget', b'512'),
                                   (b'Running guest slots', b'2'), (b'Guest vCPU budget', b'2'),
                                   (b'Minimum free disk GiB', b'1')]:
                prompt(marker); os.write(master, answer + b'\n')
            end = time.monotonic() + 30
            while process.poll() is None:
                require(time.monotonic() < end, 'guided no-start join did not finish')
                if select.select([master], [], [], .1)[0]:
                    output.extend(os.read(master, 65536))
            while select.select([master], [], [], .05)[0]:
                output.extend(os.read(master, 65536))
            require(process.returncode == 0, 'guided no-start join failed')
            require(invitation['secret'].encode() not in output, 'guided invitation leaked')
            require(termios.tcgetattr(slave) == original, 'successful guided join did not restore termios')
            require(not (root / 'control.sock').exists(), 'guided no-start spawned a worker')
        finally:
            if process.poll() is None:
                process.terminate(); process.wait(timeout=5)
            os.close(master); os.close(slave)
        # Deterministic fault after credential persistence, before marker completion.
        recovery = self.new_root()
        self.enrollment_hook = lambda: (recovery / '.ow-data').mkdir(mode=0o700)
        try:
            self.join(recovery, self.invite_file(self.invitation()), False)
        finally:
            self.enrollment_hook = None
        require((recovery / 'node.json').is_file() and (recovery / '.ow-data').is_dir(),
                'fault did not reach persisted credential/marker window')
        (recovery / '.ow-data').rmdir()
        before = len(self.hits)
        env = {k:v for k,v in self.env.items() if k != 'HOME'}
        self.call(recovery, ['join', '--no-start'], True, env=env)
        require(len(self.hits) == before, 'local recovery redeemed/downloaded again')
        require((recovery / '.ow-data').read_bytes() == b'open-workspaces local prototype v1\n',
                'marker completion failed')
        self.call(recovery, ['status'], True, env=env)
        self.result['checks']['clean-user-install-wizard'] = 'actual installed guided PTY consent/bounds with --no-start; full start pending V6'
        return 'V2 guided PTY success; credential/marker failure recovered locally without HOME or redemption'

    def state_and_unknown_stop(self):
        import fcntl
        root = self.new_root()
        self.synthetic_config(root); self.put_assets(root)
        lock = (root / 'worker.lock').open('w'); os.chmod(root / 'worker.lock', 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            self.call(root, ['stop'], False, b'UNKNOWN')
        finally:
            lock.close()
        # Fresh cached acknowledgement with no matching live instance must not be online.
        (root / 'node-channel.json').write_text(json.dumps({'state':'dispatchable', 'updated':int(time.time()),
                                                         'instance':'old', 'generation':'old-generation'}))
        (root / 'node-channel.json').chmod(0o600)
        p = self.call(root, ['status'], True)
        require(json.loads(p.stdout)['controller_dispatchable'] is False, 'stale acknowledgement reported online')
        self.call(root, ['stop'], True, b'stopped')
        return 'V3 held ownership lock/missing socket UNKNOWN; stale ack offline; released empty owned root confirmed stopped'

    def native_auth(self):
        """Reuse only prior synthetic certificates/JWKS transport, never its VM runner."""
        import struct
        elf = self.binary.read_bytes()
        require(elf[:6] == b'\x7fELF\x02\x01', 'auth fixture requires Linux ELF64')
        offset = struct.unpack_from('<Q', elf, 32)[0]
        size, count = struct.unpack_from('<HH', elf, 54)
        require(size >= 56 and offset + size * count <= len(elf), 'ELF program table invalid')
        require(any(struct.unpack_from('<I', elf, offset + size * i)[0] == 3 for i in range(count)),
                'auth fixture requires dynamic DNS shim; static gateway disables proxy; use source-matched dynamic evidence separately')
        import concurrent.futures
        import importlib.util
        from types import SimpleNamespace
        fixture_path = REPO / 'experiments/runtime-spike/multinode-test.py'
        fixture_hash = digest(fixture_path)
        spec = importlib.util.spec_from_file_location('host_auth_transport', fixture_path)
        transport = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(transport)
        h = transport.Harness(SimpleNamespace(binary=self.install,
            assets=REPO / 'data/runtime-spike', source_manifest=None))
        old_launch = h.launch
        def bounded_launch(root, *argv, env=None):
            require(root == h.controller and argv[0] in ('dashboard', 'node-controller'),
                    'auth fixture cannot launch worker/agent/guest')
            return old_launch(root, *argv, env=env)
        h.launch = bounded_launch
        result = {'fixture_root': str(h.root.relative_to(REPO)),
                  'transport_helper_sha256': fixture_hash, 'identities': 'synthetic signed alice/bob',
                  'tls': 'actual local native TLS and prefixed aliases', 'vm': False}
        def database_state():
            with sqlite3.connect('file:' + str(h.controller / 'catalog.sqlite3') + '?mode=ro', uri=True) as db:
                return db.execute('SELECT id,revoked,memory_mib,slots FROM nodes ORDER BY id').fetchall(), \
                       db.execute('SELECT count(*) FROM node_joins').fetchone()[0]
        def admin(node, user='alice', route='invite', **fields):
            return h.http('/api/hosts/' + route, {'node': node, **fields}, h.headers(user),
                          ('127.0.0.1', h.gateway_port))
        def mint(node, **fields):
            status, value = admin(node, ttl=600, memory=512, slots=2, cpus=2, **fields)
            require(status == 200 and value.get('ok') is True, 'synthetic owner invitation rejected')
            invitation = value['result']
            require(invitation['policy'] == POOL_POLICY and invitation['cpus'] == 2,
                    'invitation missing pool policy/CPU cap')
            return invitation
        def enroll(invitation, path='/_nodes/enroll', **changes):
            body = {'node': invitation['node'], 'secret': invitation['secret'], **changes}
            return h.http(path, body, tls=True)
        try:
            h.certificates()
            h.jwt_fixture()
            before = database_state()
            status, _ = admin('unconfigured-owner')
            require(status == 403 and before == database_state(), 'unconfigured owner mutated invitation state')
            with sqlite3.connect(h.controller / 'catalog.sqlite3') as db:
                user = db.execute('SELECT id FROM users WHERE subject=?', ('alice-sub',)).fetchone()[0]
            config = json.loads(h.config.read_text())
            config.update(node_owner_subject='alice-sub', node_owner_user_id=user)
            h.config.write_text(json.dumps(config)); h.config.chmod(0o600)
            h.stop(h.gateway); h.start_gateway()
            before = database_state()
            status, _ = admin('bob-denied', user='bob')
            require(status == 403 and before == database_state(), 'nonowner invite changed database')
            headers = h.headers('bob')
            headers['cf-access-authenticated-user-email'] = 'alice@example.test'
            status, _ = h.http('/api/hosts/invite', {'node': 'spoofed-email'}, headers,
                               ('127.0.0.1', h.gateway_port))
            require(status == 403 and before == database_state(), 'spoofed email gained owner authority')
            # Owner pair must match, not merely correct subject.
            config['node_owner_user_id'] = user + 1000
            h.config.write_text(json.dumps(config)); h.stop(h.gateway); h.start_gateway()
            status, _ = admin('wrong-bound-id')
            require(status == 403 and before == database_state(), 'wrong owner catalog ID accepted')
            config['node_owner_user_id'] = user
            h.config.write_text(json.dumps(config)); h.stop(h.gateway); h.start_gateway()
            h.start_controller()
            invitation = mint('verify-one')
            self.result['checks']['owner-invite'] = 'real gateway, synthetic verified owner subject/catalog pair'
            self.result['checks']['nonowner-invite-denial'] = 'bob/email-spoof/unconfigured/wrong-ID denied before DB mutation'
            status, _ = enroll(invitation, node='verify-wrong')
            require(status == 401, 'wrong-node redemption accepted')
            self.result['checks']['wrong-node-denial'] = 'real prefixed native controller wrong-node enrollment rejected'
            status, credential = enroll(invitation)
            require(status == 200 and credential['node'] == invitation['node'], 'prefixed enrollment failed')
            status, _ = enroll(invitation)
            require(status == 401, 'replayed invitation redeemed')
            self.result['checks']['enrollment'] = 'real local prefixed TLS controller, no node agent/worker'
            self.result['checks']['replay-denial'] = 'real controller one-use replay denied'
            expiring = mint('verify-expired')
            with sqlite3.connect(h.controller / 'catalog.sqlite3') as db:
                db.execute('UPDATE node_joins SET expires=? WHERE node=?', (int(time.time()) - 1, expiring['node']))
            require(enroll(expiring)[0] == 401, 'expired database invitation accepted')
            self.result['checks']['expiry-denial'] = 'real controller against expired private-fixture DB row'
            first, replacement = mint('verify-reissue'), mint('verify-reissue')
            require(enroll(first)[0] == 401 and enroll(replacement)[0] == 200, 'reissue did not invalidate older invitation')
            concurrent_invite = mint('verify-concurrent')
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                statuses = list(pool.map(lambda _: enroll(concurrent_invite)[0], range(2)))
            require(sorted(statuses) == [200, 401], 'parallel enrollment not exactly one success')
            # Browser credential never becomes node session authority.
            ws = transport.WebSocket(('127.0.0.1', h.node_port), '/_nodes/node/verify-one',
                                     {'Authorization': 'Bearer ' + h.tokens['alice']}, h.tls)
            try:
                require(ws.status == 401, 'browser token authenticated node session')
            finally:
                ws.close()
            self.result['checks']['human-token-node-denial'] = 'actual native TLS upgrade rejected synthetic browser JWT bearer'
            headers = {'Host': transport.HOST, 'Origin': 'https://' + transport.HOST,
                       'Authorization': 'Bearer ' + credential['credential'], 'x-ow-request': 'dashboard'}
            status, _ = h.http('/api/hosts/invite', {'node': 'node-cannot-admin'}, headers,
                               ('127.0.0.1', h.gateway_port))
            require(status == 401, 'node bearer authenticated human invitation')
            self.result['checks']['node-token-human-denial'] = 'actual gateway human admin denied valid node bearer'
            # V1: RAM/slots remain available so an independent CPU denial cannot be masked.
            cpu_socket = transport.WebSocket(('127.0.0.1', h.node_port), '/_nodes/node/verify-one',
                       {'Authorization':'Bearer ' + credential['credential']}, h.tls)
            try:
                require(cpu_socket.status == 101, 'valid V1 synthetic node could not open real TLS session')
                transport.wait_for(lambda: (h.controller/'nodes/verify-one/control.sock').exists(), timeout=5)
                cpu_socket.send(json.dumps({'type':'heartbeat','ready_ack':True,'protocol':1,'backend':'firecracker',
                    'arch':'x86_64','runtime':'firecracker-v1.17.0','memory_mib':512,'slots':2,'vcpus':16,'images':['alpine']}), kind=1)
                _, ack = cpu_socket.recv(); require(json.loads(ack)['type'] == 'ready', 'CPU fixture missing ready acknowledgement')
                with sqlite3.connect(h.controller / 'catalog.sqlite3') as db:
                    cap = db.execute('SELECT vcpu_cap FROM node_limits WHERE node=?',('verify-one',)).fetchone()[0]
                    effective = json.loads(db.execute('SELECT capabilities FROM nodes WHERE id=?',('verify-one',)).fetchone()[0])
                require(cap == effective['vcpus'] == 2 and effective['memory_mib'] == 512 and effective['slots'] == 2,
                        'forged heartbeat raised CPU cap or RAM/slots unavailable')
                import threading
                stop_jobs = threading.Event(); observed_jobs = []; job_errors = []
                def inventory_jobs():
                    try:
                        cpu_socket.s.settimeout(2)
                        while not stop_jobs.is_set():
                            kind, frame = cpu_socket.recv()
                            require(kind == 1, 'unexpected inventory notification protocol')
                            if frame.startswith(b'{'):
                                require(json.loads(frame)['type']=='ready', 'unexpected readiness frame'); continue
                            job_id = frame.decode()
                            require(len(job_id)==24 and all(c in '0123456789abcdef' for c in job_id), 'invalid job ID')
                            peer = transport.WebSocket(('127.0.0.1',h.node_port), '/_nodes/job/verify-one/'+job_id,
                                  {'Authorization':'Bearer '+credential['credential']}, h.tls)
                            try:
                                require(peer.status==101, 'inventory job handshake rejected')
                                kind, payload = peer.recv(); require(kind==2, 'inventory request must use binary wire')
                                request = json.loads(payload); observed_jobs.append(request['op'])
                                require(request['op'] in ('list','snapshots','status'), 'CPU denial dispatched effectful operation')
                                value = {'max_memory_mib':512,'max_running':2,'max_vcpus':2,'running':0} if request['op']=='status' else []
                                peer.send((json.dumps({'ok':True,'result':value})+'\n').encode(),kind=2)
                            finally: peer.close()
                    except TimeoutError:
                        pass
                    except Exception as error:
                        job_errors.append(str(error))
                thread = threading.Thread(target=inventory_jobs,daemon=True); thread.start()
                connection = http.client.HTTPConnection('127.0.0.1', h.gateway_port, timeout=10)
                try:
                    headers = h.headers('alice'); headers['Content-Type'] = 'application/json'
                    connection.request('POST','/api/operation',json.dumps({'op':'create','id':'cpu-denied',
                        'node':'verify-one','memory_mib':256,'vcpu_count':3,'image':'alpine','operation_key':'cpu-denied-key'}),headers)
                    response = connection.getresponse(); response.read(65536)
                    require(response.status == 409, 'CPU3 was not denied with RAM/slot headroom')
                finally:
                    connection.close(); stop_jobs.set(); thread.join(timeout=4)
                require(not thread.is_alive() and not job_errors, 'inventory fixture failure: '+repr(job_errors))
                with sqlite3.connect(h.controller/'catalog.sqlite3') as db:
                    require(db.execute("SELECT count(*) FROM resources WHERE name='cpu-denied'").fetchone()[0]==0,
                            'CPU denial accepted a resource/reservation')
                self.result['cpu_fixture_inventory_jobs'] = observed_jobs
                self.result['checks']['forged-capacity-denial'] = 'V1 real heartbeat16 clamped2; CPU3 denied with RAM/slots free before effectful dispatch; only empty read-only reconciliation allowed; fresh DB reopened'
            finally:
                cpu_socket.close()

            for path, method in [('/_nodes/enroll', 'GET'), ('/_nodes/enroll?x=1', 'POST'),
                                 ('/_nodes//enroll', 'POST'), ('/_nodes/enroll/extra', 'POST'),
                                 ('/_nodes/%65nroll', 'POST'), ('/_nodes/api/state', 'GET')]:
                connection = http.client.HTTPSConnection('127.0.0.1', h.node_port, context=h.tls, timeout=10)
                try:
                    connection.request(method, path, body=b'{}', headers={'Content-Type': 'application/json'})
                    response = connection.getresponse(); response.read(65536)
                    require(response.status in (400, 401, 403, 404, 405, 422), 'node path/method boundary accepted')
                finally:
                    connection.close()
            self.result['checks']['path-method-denial'] = 'real native prefixed controller method/query/lookalike/encoded-path denial'
            status, _ = admin('verify-one', route='revoke')
            require(status == 200, 'owner revocation failed')
            ws = transport.WebSocket(('127.0.0.1', h.node_port), '/_nodes/node/verify-one',
                                     {'Authorization': 'Bearer ' + credential['credential']}, h.tls)
            try:
                require(ws.status == 401, 'revoked credential authenticated session')
            finally:
                ws.close()
            self.result['checks']['owner-revoke'] = 'real owner revoke and subsequent native TLS node upgrade denial'
            # HR-11 deliberately catches current revoked-unused-ID behavior.
            mint('verify-unused-revoked')
            require(admin('verify-unused-revoked', route='revoke')[0] == 200, 'unused revoke failed')
            before = database_state()
            require(admin('verify-unused-revoked')[0] == 409 and database_state() == before,
                    'HR-11 revoked unused node reissued unusable invitation')
            result['parallel_enrollment_statuses'] = statuses
            result['remaining'] = 'successful agent/job streams, forged heartbeat caps, in-flight fencing/recovery and public edge pending'
            return result
        finally:
            # Only children launched by this narrow fixture; never baseline descriptors.
            for p in reversed(h.processes):
                h.stop(p)
            for server in h.servers:
                server.shutdown(); server.server_close()
            require(digest(fixture_path) == fixture_hash, 'reused transport fixture source changed')

    def run(self):
        before = guests()
        try:
            self.tls_server()
            groups = {
                'V2-guided-recovery': self.guided_and_recovery,
                'V3-state-unknown-stop': self.state_and_unknown_stop,
                'real-user-space-linux-installer': self.installer,
                'installed-host-commands': self.installed_help,
                'doctor-local-with-saved-human-login': self.doctor,
                'non-tty-guided-join': self.non_tty,
                'participation-explicit-server-denial': self.explicit_server,
                'invalid-invitation-before-network': self.invalid_envelopes,
                'hidden-pty-cancellation': self.hidden_cancel,
                'local-assets-rejection': self.asset_rejection,
                'lifecycle-worker-mismatch-denial': self.lifecycle_denials,
                'actual-https-download-and-no-start-join': self.download_cases,
                'actual-gateway-and-native-controller-auth': self.native_auth,
                'owner-invitation-output-before-mutation': self.owner_output_preflight,
            }
            selected = self.args.cases or list(groups)
            require(all(name in groups for name in selected), 'unknown execution group')
            for name in selected:
                self.check(name, groups[name])
            if self.result['details'].get('installed-host-commands', {}).get('state') == 'passed':
                self.result['checks']['installed-host-commands'] = 'installed binary host help; public publication pending'
        finally:
            for server in self.servers:
                server.shutdown(); server.server_close()
            for thread in self.threads:
                thread.join(timeout=3)
            after = guests()
            self.result['concurrent_host_inventory'] = {'before':before,'after':after}
            protected = json.loads((PRIVATE/'inspect-result.json').read_text())['host_inventory']
            require(all(any(x['pid']==b['pid'] and x['ticks']==b['ticks'] and x['memory_mib']==b['memory_mib']
                            for x in after) for b in protected), 'owner VMM identity/demand changed during no-VM adapters')
            require(not any(str(self.root) in x['config_or_socket'] for x in after), 'no-VM adapter left a test VMM')
            require(self.frozen == source_manifest(self.args.lane), 'core source changed during no-VM adapters')
            require(digest(self.binary) == self.binary_hash, 'compiled binary changed during no-VM adapters')
        require(self.failures == 0, 'focused no-VM adapters have failures; private evidence recorded')


def no_vm(args, result):
    require(args.binary is not None and args.ready_receipt is not None,
            'no-vm requires compiled --binary and core --ready-receipt')
    compare(args, result, allow_other=True)
    NoVM(args, result).run()


def ingress(args, result):
    """Execute a hash-pinned copy of the actual offline planner script; no API calls."""
    import copy
    script = args.lane / 'scripts/prepare-host-ingress.py'
    script_hash = digest(script)
    root = Path(tempfile.mkdtemp(prefix='ingress-', dir=PRIVATE))
    pinned = root / 'prepare-host-ingress.py'
    pinned.write_bytes(script.read_bytes())
    require(digest(pinned) == script_hash, 'ingress script changed during capture')
    # Import constants only; execution of every case uses the actual CLI entry point.
    import importlib.util
    spec = importlib.util.spec_from_file_location('offline_ingress', pinned)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    host = 'host.example.test'
    protected_policy = {'decision': 'allow', 'include': [{'email': {'email': 'owner@example.test'}}]}
    state = {'hostname': host, 'account_id': 'fixture-account', 'zone_id': 'fixture-zone',
             'tunnel_id': 'fixture-tunnel', 'dashboard_app_id': 'dashboard', 'cli_app_id': 'cli',
             'dashboard_audience': ['fixture-audience'],
             'ownership': {'hostname': host, 'account_id': 'fixture-account',
                           'zone_id': 'fixture-zone', 'tunnel_id': 'fixture-tunnel'},
             'native_trust': {'caPool': '/private/fixture/native-ca.pem', 'originServerName': 'localhost'},
             'apps': [{'id': 'dashboard', 'account_id': 'fixture-account', 'domain': host,
                       'policies': [protected_policy]},
                      {'id': 'cli', 'account_id': 'fixture-account', 'domain': host + '/cli/*',
                       'policies': [{'decision': 'bypass', 'include': [{'everyone': {}}]}]}],
             'tunnel': {'tunnel': 'fixture-tunnel', 'credentials-file': '/private/fixture/tunnel.json',
                        'ingress': [{'hostname': host, 'path': module.OLD_CLI_REGEX,
                                     'service': 'http://127.0.0.1:8787', 'originRequest': {'access': {'required': False}}},
                                    {'hostname': host, 'service': 'http://127.0.0.1:8787',
                                     'originRequest': {'access': {'required': True, 'audTag': ['fixture-audience']}}},
                                    {'service': 'http_status:404'}]}}
    approved = None
    if 'approved' in __import__('inspect').signature(module.prepare).parameters:
        state.update(capture_schema=1, app_inventory_complete=True, team_domain='fixture.cloudflareaccess.com')
        for application in state['apps']:
            application.update(type='self_hosted', aud=['fixture-audience'],
                               team_domain=state['team_domain'], session_duration='24h')
        ca, key, cert, csr, ext = [root / name for name in ('native-ca.pem','native-ca.key','native.pem','native.csr','native.ext')]
        def openssl(*argv):
            subprocess.run(['openssl', *map(str, argv)], check=True, capture_output=True, timeout=30)
        openssl('req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=Ingress fixture CA',
                '-keyout',key,'-out',ca,'-addext','basicConstraints=critical,CA:TRUE')
        leaf_key = root / 'native.key'
        openssl('req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=localhost','-keyout',leaf_key,'-out',csr)
        ext.write_text('subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\n')
        openssl('x509','-req','-in',csr,'-CA',ca,'-CAkey',key,'-CAcreateserial','-days','1','-extfile',ext,'-out',cert)
        ca.chmod(0o600); cert.chmod(0o600)
        state['native_trust']['caPool'] = str(ca)
        approved = {k:copy.deepcopy(state[k]) for k in ['account_id','zone_id','tunnel_id','hostname',
                    'dashboard_app_id','cli_app_id','dashboard_audience','team_domain']}
        approved.update(dashboard_app=copy.deepcopy(state['apps'][0]), cli_app=copy.deepcopy(state['apps'][1]),
                        native={**state['native_trust'], 'serverCert':str(cert),
                                'ca_sha256':digest(ca), 'server_cert_sha256':digest(cert)})
        approved_path = root / 'approved.json'; approved_path.write_text(json.dumps(approved))
    result['script_sha256'] = script_hash
    result['fixture_root'] = str(root.relative_to(REPO))
    result['details'] = {}
    counter = 0
    def execute(value, success, plan=None, created=None):
        nonlocal counter
        counter += 1
        before = root / f'state-{counter}.json'; output = root / f'output-{counter}.json'
        before.write_text(json.dumps(value))
        command = ['python3', str(pinned), '--state', str(before), '--output', str(output)]
        if approved is not None:
            command += ['--approved', str(approved_path)]
        if plan:
            file = root / f'plan-{counter}.json'; file.write_text(json.dumps(plan))
            command += ['--rollback-plan', str(file)]
        if created:
            command += ['--created-app-id', created]
        p = subprocess.run(command, capture_output=True, timeout=10)
        (root / f'stderr-{counter}.txt').write_bytes(p.stderr)
        if success is not None:
            require((p.returncode == 0) == success, 'unexpected actual ingress script exit')
        require(json.loads(before.read_text()) == value, 'offline input was mutated')
        return json.loads(output.read_text()) if p.returncode == 0 else None
    def check(name, fn):
        try:
            result['details'][name] = {'state': 'passed', 'evidence': fn()}
        except Exception as e:
            result['details'][name] = {'state': 'failed', 'error_type': type(e).__name__, 'private_error': str(e)}
        save(PRIVATE / 'ingress-result.json', result)
    plan = execute(state, True)
    require(plan['before'] == state and plan['precondition_sha256'] == module.fingerprint(state),
            'plan did not back up exact original state')
    applied = copy.deepcopy(state)
    applied['tunnel'] = plan['tunnel_after']; applied['node_app_id'] = 'created-node'
    applied['apps'].append({**plan['node_app_create'], 'id': 'created-node', 'account_id': 'fixture-account'})
    def initial():
        require(plan['worker_boundary'].find('Never stop workers') >= 0, 'worker preservation boundary absent')
        routes = plan['tunnel_after']['ingress']
        node = next(x for x in routes if x.get('path') == module.NODE_REGEX)
        require(node['service'] == 'https://127.0.0.1:8790' and not node['originRequest'].get('noTLSVerify'), 'native TLS route incorrect')
        require(routes.index(node) < next(i for i,x in enumerate(routes) if x.get('hostname') == host and not x.get('path')),
                'node route behind protected fallback')
        for path, expect in [('/_nodes/enroll', 'https://127.0.0.1:8790'),
                             ('/_nodes/node/friend', 'https://127.0.0.1:8790'),
                             ('/_nodes/job/friend/job', 'https://127.0.0.1:8790'),
                             ('/_nodes/enroll/extra', 'http://127.0.0.1:8787'),
                             ('/_nodesx/enroll', 'http://127.0.0.1:8787'),
                             ('/api/state', 'http://127.0.0.1:8787'),
                             ('/cli/host/base.ext4', 'http://127.0.0.1:8787')]:
            selected = next(x for x in routes if x.get('hostname') == host and
                            (not x.get('path') or __import__('re').match(x['path'], path)))
            require(selected['service'] == expect, 'effective route precedence wrong')
        return 'exact preimage, private CA/server name, native TLS and effective path ordering'
    check('initial-prepare-and-effective-routing', initial)
    def reapply():
        again = execute(applied, True)
        require(again['node_app_create'] is None and again['tunnel_after'] == applied['tunnel'], 'reapply duplicated app/ingress')
        return 'already-applied plan leaves exact tunnel unchanged'
    check('reapply-idempotent-plan', reapply)
    def narrow_rollback():
        current = copy.deepcopy(applied)
        later = {'hostname': 'later.example.test', 'service': 'http://127.0.0.1:9999'}
        current['tunnel']['ingress'].insert(0, later)
        foreign = {'id': 'later-app', 'account_id': 'fixture-account', 'domain': 'later.example.test', 'policies': [protected_policy]}
        current['apps'].append(foreign)
        rolled = execute(current, True, plan, 'created-node')
        require(later in rolled['tunnel']['ingress'] and foreign in rolled['apps'], 'rollback lost unrelated later state')
        require(rolled['apps'] == state['apps'] + [foreign], 'rollback changed protected apps')
        require(rolled['tunnel']['ingress'] == [later] + state['tunnel']['ingress'], 'rollback did not restore exact owned routes')
        return 'created node app/route removed; original CLI and unrelated later app/route preserved'
    check('narrow-rollback-preserves-unrelated-later-state', narrow_rollback)
    variations = {}
    v = copy.deepcopy(state); v['apps'].append(copy.deepcopy(v['apps'][0])); variations['duplicate-dashboard-app'] = v
    v = copy.deepcopy(state); v['apps'][0]['id'] = 'foreign'; variations['foreign-dashboard-app'] = v
    v = copy.deepcopy(state); v['apps'][0]['policies'][0]['decision'] = 'bypass'; variations['dashboard-bypass-drift'] = v
    v = copy.deepcopy(state); v['apps'][1]['policies'][0]['include'] = []; variations['cli-policy-drift'] = v
    v = copy.deepcopy(state); v['apps'].append({'id': 'shadow', 'domain': host + '/_nodes/node/*'}); variations['shadow-access-app'] = v
    v = copy.deepcopy(state); v['ownership']['zone_id'] = 'other'; variations['foreign-zone'] = v
    v = copy.deepcopy(state); v['tunnel']['originRequest'] = {'noTLSVerify':True}; variations['inherited-global-tls-bypass'] = v
    v = copy.deepcopy(state); v['native_trust']['noTLSVerify'] = True; variations['tls-bypass-drift'] = v
    v = copy.deepcopy(state); v['native_trust']['caPool'] = ''; variations['missing-ca-trust'] = v
    v = copy.deepcopy(state); v['tunnel']['ingress'].insert(0, {'service': 'http_status:404'}); variations['global-shadowing-route'] = v
    v = copy.deepcopy(state); v['tunnel']['ingress'][0]['service'] = 'http://127.0.0.1:1'; variations['cli-origin-drift'] = v
    v = copy.deepcopy(state); v['node_app_id'] = 'missing'; variations['missing-recorded-node-app'] = v
    v = copy.deepcopy(applied); v['tunnel'] = copy.deepcopy(state['tunnel']); variations['partial-app-without-route'] = v
    v = copy.deepcopy(state); v['tunnel'] = copy.deepcopy(applied['tunnel']); variations['partial-route-without-app'] = v
    # Concrete HR-18 regression: changing a protected allow policy cannot be silently accepted.
    v = copy.deepcopy(state); v['apps'][0]['policies'][0]['include'] = [{'everyone': {}}]; variations['protected-allow-policy-drift-HR18'] = v
    for name, value in variations.items():
        check(name, lambda value=value: execute(value, False))
    def rollback_denial(name, alter):
        current = copy.deepcopy(applied); alter(current)
        check(name, lambda: execute(current, False, plan, 'created-node'))
    rollback_denial('rollback-owned-node-service-drift', lambda v: next(r for r in v['tunnel']['ingress'] if r.get('path') == module.NODE_REGEX).update(service='https://127.0.0.1:1'))
    rollback_denial('rollback-native-tunnel-id-drift-HR18', lambda v: v['tunnel'].update(tunnel='other'))
    rollback_denial('rollback-dashboard-policy-drift-HR18', lambda v: v['apps'][0]['policies'][0].update(include=[{'everyone': {}}]))
    check('rollback-wrong-created-app-id', lambda: execute(applied, False, plan, 'foreign-node'))
    rolled = execute(applied, True, plan, 'created-node')
    def repeat_rollback():
        repeated = execute(rolled, None, plan, 'created-node')
        require(repeated is None or repeated == rolled, 'repeat rollback changed already-restored state')
        return 'already-restored state unchanged or harmless refusal'
    check('rollback-repeat-harmless-or-idempotent-HR19', repeat_rollback)
    def compensation():
        partial = copy.deepcopy(applied); partial['tunnel'] = copy.deepcopy(state['tunnel'])
        require(execute(partial, True, plan, 'created-node') == state,
                'app-created/route-failed compensation did not restore exact owned state')
        return 'offline actual rollback removed only recorded newly-created app after route failure'
    check('app-created-route-failed-compensation', compensation)
    require(digest(script) == script_hash, 'evolving ingress script changed; result applies only to captured copy')
    failures = [k for k,v in result['details'].items() if v['state'] == 'failed']
    result['verdict'] = 'offline script regressions found' if failures else 'offline fixture checks passed; cloud behavior untested'
    result['failed_cases'] = failures
    if not failures: result['checks']['V4'] = 'actual offline script drift/compensation/reapply/rollback checks passed; no external writes'
    require(not failures, 'offline ingress regression cases failed; inspect private details')


def artifact(args, result):
    """V5: execute actual bundle publisher offline and verify exact static artifacts."""
    import shutil
    private_file(args.fixture)
    fixture = json.loads(args.fixture.read_text())
    source, binary = Path(fixture['prepared_source']).resolve(), Path(fixture['linux_cli']).resolve()
    require(source.is_relative_to(REPO / 'data') and not source.is_relative_to(REPO / 'data/prototype'),
            'clean prepared source must be new ignored test storage')
    ready = ready_record(args.ready_receipt, args.lane)
    frozen = source_manifest(args.lane)
    require(ready['source_manifest'] == frozen and ready['binary_sha256'] == digest(binary), 'V5 frozen source/binary mismatch')
    build = json.loads((binary.parent / 'build.json').read_text())
    require(build['cli_sha256'] == digest(binary), 'recorded static CLI differs')
    require(all(frozen.get(k) == v for k,v in build['source'].items()), 'build source differs from frozen tree')
    require(hashlib.sha256(json.dumps(build['source'], sort_keys=True).encode()).hexdigest() == build['source_sha256'],
            'build source record fingerprint differs')
    elf = binary.read_bytes()
    require(elf[:6] == b'\x7fELF\x02\x01', 'Linux x86-64 ELF required')
    import struct
    require(struct.unpack_from('<H', elf, 18)[0] == 62, 'ELF architecture differs')
    offset = struct.unpack_from('<Q', elf, 32)[0]
    size, count = struct.unpack_from('<HH', elf, 54)
    require(size >= 56 and count > 0 and offset + size * count <= len(elf), 'ELF program table invalid')
    require(all(struct.unpack_from('<I', elf, offset + size * i)[0] != 3 for i in range(count)),
            'dynamic PT_INTERP found; portable static CLI required')
    marker = json.loads((source / 'guest/clean-alpine.json').read_text())
    require(marker['recipe'] == 'alpine-minirootfs-3.24.2-minimal-v1' and marker['sha256'] == digest(source / 'guest/base.ext4'),
            'clean image marker/hash mismatch')
    root = Path(tempfile.mkdtemp(prefix='artifact-', dir=PRIVATE))
    script = args.lane / 'scripts/prepare-host-bundle.py'; pinned = root / script.name
    pinned.write_bytes(script.read_bytes()); script_hash = digest(pinned)
    (root / '.ow-host-build').write_text('open-workspaces dedicated host build v1\n')
    original_source, original_binary = source, binary
    source = root / 'source'; source.mkdir(mode=0o700)
    for rel in [dest for dest,_ in ASSET_LAYOUT.values()] + ['guest/clean-alpine.json']:
        target = source / rel; target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        subprocess.run(['cp', '--reflink=always', str(original_source / rel), str(target)],
                       check=True, capture_output=True, timeout=30)
    release = root / 'release'; release.mkdir(mode=0o700)
    binary = release / 'ow-linux-amd64'; shutil.copyfile(original_binary, binary); binary.chmod(0o700)
    shutil.copyfile(original_binary.parent / 'build.json', release / 'build.json')
    def publish(src, output, success):
        p = subprocess.run(['python3', str(pinned), '--source', str(src), '--output', str(output),
                            '--linux-cli', str(binary), '--build-root', str(root)], capture_output=True, timeout=120)
        require((p.returncode == 0) == success, 'actual bundle publisher unexpected exit')
        if not success:
            require(not output.exists(), 'unsafe failed bundle published')
    output = root / 'bundle'; publish(source, output, True)
    manifest = json.loads((output / 'manifest.json').read_text())
    require({x['name'] for x in manifest['files']} == set(ASSET_LAYOUT), 'bundle fixed file allowlist differs')
    for item in manifest['files']:
        path = output / item['name']
        require(path.stat().st_size == item['size'] and digest(path) == item['sha256'] and not path.is_symlink(),
                'prepared bundle file size/hash/type differs')
    shipped = Path(ready['bundle'])
    require(json.loads((shipped / 'manifest.json').read_text()) == manifest,
            'shipped and independently published manifests differ')
    for item in manifest['files']:
        require(digest(shipped / item['name']) == item['sha256'] and
                (shipped / item['name']).stat().st_size == item['size'], 'shipped bundle bytes differ')
    require(digest(shipped / 'ow-linux-amd64') == digest(binary), 'shipped CLI differs')
    installed = root / 'installed-ow'; shutil.copyfile(output / 'ow-linux-amd64', installed); installed.chmod(0o700)
    require(digest(installed) == digest(binary) == digest(output / 'ow-linux-amd64'), 'copied/installed CLI mismatch')
    cli(args.__class__(**{**vars(args), 'binary': installed}), result)
    # A disposable reflink copy permits destructive negative bytes without modifying clean source.
    clone = root / 'negative-source'
    clone.mkdir(mode=0o700)
    for rel in [dest for dest,_ in ASSET_LAYOUT.values()] + ['guest/clean-alpine.json']:
        target = clone / rel; target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        subprocess.run(['cp','--reflink=always',str(source / rel),str(target)], check=True, capture_output=True, timeout=30)
    negative_disk = clone / 'guest/base.ext4'
    negative_disk.chmod(0o600)
    with negative_disk.open('r+b') as stream:
        old = stream.read(1); stream.seek(0); stream.write(bytes([old[0] ^ 1]))
    publish(clone, root / 'bad-disk-output', False)
    shutil.rmtree(clone)
    link = root / 'symlink-source'; link.symlink_to(source, target_is_directory=True)
    publish(link, root / 'bad-symlink-output', False)
    # Serving selection is compared read-only, never copied into live gateway storage.
    if fixture.get('gateway_selected_cli'):
        require(digest(Path(fixture['gateway_selected_cli'])) == digest(binary), 'gateway-selected candidate CLI differs')
    result['checks']['stable-source-shipped-artifacts'] = 'V5 static CLI/installed copy/five-file clean bundle matched frozen build; public release remains separate'
    result['artifact_manifest'] = manifest
    result['build_source_sha256'] = build['source_sha256']
    result['bundle_path'] = str(output)
    result['installed_cli_sha256'] = digest(installed)
    result['publisher_sha256'] = script_hash
    require(digest(script) == script_hash and source_manifest(args.lane) == frozen, 'V5 source changed during artifact test')


def guest(args, result):
    """V6 serial real-guest adapter. No execution without planner cleanup/turn receipt.

    Planner supplies a dedicated scratch TLS edge/controller with actual static
    bundle routes and a private real invitation. Advanced --ca-cert is explicit;
    inherited SSL_CERT_FILE is never used for managed agent trust.
    """
    private_file(args.vm_turn); turn = json.loads(args.vm_turn.read_text())
    require(turn.get('planner_exclusive_turn') is True and turn.get('core_cleanup_complete') is True,
            'planner must release exclusive turn after core guest cleanup')
    require(turn.get('expires', 0) > time.time(), 'exclusive turn receipt expired')
    private_file(args.fixture); f = json.loads(args.fixture.read_text())
    ready = ready_record(args.ready_receipt, args.lane)
    binary = args.binary.resolve(); frozen = source_manifest(args.lane)
    result['binary_sha256'] = digest(binary); result['build_source_sha256'] = ready.get('source_sha256')
    result['tested_product_source'] = frozen
    require(ready['source_manifest'] == frozen and ready['binary_sha256'] == digest(binary), 'V6 source/binary mismatch')
    require(turn['binary_sha256'] == digest(binary), 'turn release belongs to another binary')
    compare(args, result)
    baseline = guests()
    require(len(baseline) == 1 and baseline[0]['memory_mib'] == 2048, 'core has not cleaned guest demand')
    ca = Path(f['ca_cert']).resolve(); private_file(ca)
    invite = Path(f['invite_file']).resolve(); private_file(invite)
    controller_root = Path(f['controller_root']).resolve()
    require(controller_root.is_relative_to(REPO / 'data') and not controller_root.is_relative_to(REPO / 'data/prototype'),
            'V6 controller must be dedicated scratch root')
    require((controller_root / '.host-verify-owned').read_text().strip() == 'host-verify fixture v1', 'controller fixture ownership marker missing')
    descriptor = f['controller_process']; observed = process(descriptor['pid'])
    require(observed['uid'] == os.getuid() and observed['ticks'] == descriptor['ticks']
            and Path(f'/proc/{observed["pid"]}/exe').resolve() == binary, 'scratch controller identity mismatch')
    node = json.loads(invite.read_text())['node']
    root = Path(tempfile.mkdtemp(prefix='hg-', dir=REPO / 'data'))
    (root / '.host-verify-owned').write_text('host-verify fixture v1\n')
    # Keep fixture marker outside the managed root; wizard correctly refuses unrelated files.
    managed = root / 'h'; managed.mkdir(mode=0o700)
    require(len(str(managed)) < 60, 'V6 managed root too long')
    result['guest_root'] = str(managed.relative_to(REPO)); result['method'] = 'one256MiB/1vCPU guest; one512MiB/2slot/2vCPU worker'
    env = {'PATH':'/usr/sbin:/usr/bin:/sbin:/bin', 'LANG':'C', 'HOME':str(root)}
    def command(*argv, success=True, timeout=60):
        p = subprocess.run([str(binary), '--local', '--data-dir', str(managed), *argv],
                           env=env, cwd=root, capture_output=True, timeout=timeout)
        stem = root / ('command-' + str(time.time_ns()))
        stem.with_suffix('.stdout').write_bytes(p.stdout); stem.with_suffix('.stderr').write_bytes(p.stderr)
        require((p.returncode == 0) == success, 'V6 CLI operation unexpected exit; private command logs saved')
        return p
    def budget(extra=0):
        live = guests(); total = sum(x['memory_mib'] for x in live)
        require(len(live) + extra <= 4 and total + 256 * extra <= 3072, 'global host cap unavailable')
        tests = [x for x in live if (x['pid'],x['ticks']) not in {(b['pid'],b['ticks']) for b in baseline}]
        require(len(tests) + extra <= 3 and sum(x['memory_mib'] for x in tests) + extra * 256 <= 768, 'team test cap unavailable')
        require(all(any(x['pid']==b['pid'] and x['ticks']==b['ticks'] for x in live) for b in baseline), 'owner VMM identity lost')
        available = int(next(x.split()[1] for x in Path('/proc/meminfo').read_text().splitlines() if x.startswith('MemAvailable:'))) // 1024
        require(available >= 4096 + extra * 256, 'physical host reserve unavailable')
        with sqlite3.connect('file:' + str(controller_root / 'catalog.sqlite3') + '?mode=ro', uri=True) as db:
            tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if 'operations' in tables:
                require(db.execute("SELECT count(*) FROM operations WHERE state IN ('pending','uncertain')").fetchone()[0] == 0,
                        'unresolved controller demand blocks VM admission')
            # A newly created native-only fixture has no human operations schema.
            # Host-wide live/catalog inventory was checked before the fixture exists.
    def routed(request):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(60); client.connect(str(controller_root / 'nodes' / node / 'control.sock'))
            client.sendall(json.dumps(request).encode() + b'\n')
            with client.makefile('rb') as stream:
                response = json.loads(stream.readline(1048577))
            require(response.get('ok'), 'actual node route operation rejected')
            return response['result']
    def await_ready(timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            p = command('host','status')
            status = json.loads(p.stdout)
            if status.get('controller_dispatchable') is True:
                return status
            time.sleep(.1)
        raise RuntimeError('managed node never became controller-dispatchable')
    guest_id = 'host-acceptance'
    try:
        budget()
        # Explicit advanced CA contract; host starts its managed root with sanitized environment.
        command('host','join','--invite-file',str(invite),'--ca-cert',str(ca),
                '--memory','512','--slots','2','--cpus','2','--storage-gib','1','--accept-shared-pool', timeout=180)
        status = await_ready()
        require(tuple(status['worker'][k] for k in ('max_memory_mib','max_running','max_vcpus')) == (512,2,2), 'effective V6 worker caps differ')
        worker_pid = status['worker']['worker_pid']
        raw_env = Path(f'/proc/{worker_pid}/environ').read_bytes().split(b'\0')
        require(not any(x.startswith(b'SSL_CERT_FILE=') for x in raw_env), 'ambient trust leaked into sanitized worker')
        budget(1)
        routed({'op':'create','id':guest_id,'memory_mib':256,'vcpu_count':1,'image':'alpine'})
        result['vm_run'] = True
        budget()
        nonce = hashlib.sha256(os.urandom(32)).hexdigest()
        value = routed({'op':'exec','id':guest_id,'command':"uname -r; printf '%s' " + nonce + ' > /root/host-acceptance'})
        require(value['exit_code'] == 0, 'real guest exec failed')
        routed({'op':'stop','id':guest_id}); budget(1); routed({'op':'start','id':guest_id})
        cold_value = routed({'op':'exec','id':guest_id,'command':'cat /root/host-acceptance'})
        result['cold_exec'] = cold_value
        require(cold_value['exit_code']==0 and cold_value['output'].replace('\nOW> ', '\n').strip()==nonce, 'cold disk persistence failed')
        # Managed stop/restart changes only this owned worker/agent; guest disk stays.
        generation = status['node_channel'].get('generation')
        command('host','stop'); require(guests() == baseline, 'owned stop left test VM or changed owner baseline')
        command('host','start'); status = await_ready()
        require(status['node_channel'].get('generation') != generation, 'reconnect did not replace generation')
        budget(1); routed({'op':'start','id':guest_id})
        require(routed({'op':'exec','id':guest_id,'command':'cat /root/host-acceptance'})['output'].strip() == nonce, 'managed restart lost guest disk')
        import pty, select, termios, signal
        master, slave = pty.openpty(); terminal = termios.tcgetattr(slave)
        shell = subprocess.Popen([str(binary),'--local','--data-dir',str(managed),'shell',guest_id],
                    env=env,cwd=root,stdin=slave,stdout=slave,stderr=slave)
        output = bytearray()
        try:
            os.write(master,b"test -t 0 && printf 'GUEST_%s\\n' 'PTY'; exit 0\r")
            end = time.monotonic()+15
            while b'GUEST_PTY' not in output:
                require(time.monotonic()<end, 'real guest PTY marker missing')
                if select.select([master],[],[],.1)[0]: output.extend(os.read(master,65536))
            require(shell.wait(timeout=5)==0 and termios.tcgetattr(slave)==terminal, 'PTY exit/restoration failed')
        finally:
            if shell.poll() is None: shell.terminate(); shell.wait(timeout=5)
            os.close(master); os.close(slave)
        def stop_owned_agent():
            record=json.loads((managed/'node-agent.lock').read_text()); pid=record['pid']
            fd=os.pidfd_open(pid)
            try:
                identity=process(pid)
                require(identity['uid']==os.getuid() and identity['ticks']==record['start'], 'agent process identity mismatch')
                require(Path(f'/proc/{pid}/exe').resolve()==binary and str(managed).encode() in identity['argv'], 'agent root/executable mismatch')
                signal.pidfd_send_signal(fd, signal.SIGTERM)
            finally: os.close(fd)
            end=time.monotonic()+10
            while time.monotonic()<end:
                try:
                    current=process(pid)
                    if current['ticks']!=identity['ticks'] or current['state']=='Z': return
                except FileNotFoundError: return
                time.sleep(.05)
            raise RuntimeError('owned agent has not exited')
        previous = status['node_channel']['generation']; stop_owned_agent()
        require(json.loads(command('host','status').stdout)['controller_dispatchable'] is False,
                'V3/V6 stale acknowledgement survived agent exit inside15seconds')
        native=f['native_controller']; url=urlsplit(native)
        require(url.scheme=='https' and ipaddress.ip_address(url.hostname).is_loopback and url.path in ('','/'),
                'native advanced fixture needs exact loopback HTTPS origin')
        agent_env={**env,'OW_HOST_MANAGED':'1','OW_ASSET_DIR':str(managed/'assets')}
        native_agent=subprocess.Popen([str(binary),'--local','--data-dir',str(managed),'node-agent',
                       '--controller',native,'--credential',str(managed/'node.json'),'--ca-cert',str(ca)],
                       env=agent_env,cwd=root,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
        try:
            status=await_ready()
            require(status['node_channel']['generation']!=previous, 'native advanced reconnect reused old generation')
            native_value = routed({'op':'exec','id':guest_id,'command':'cat /root/host-acceptance'})
            result['native_exec'] = native_value
            require(native_value['exit_code']==0 and native_value['output'].replace('\nOW> ', '\n').strip()==nonce,
                    'native agent could not dispatch persisted guest; retained exact output')
        finally:
            native_agent.terminate(); native_agent.wait(timeout=10)
        import importlib.util
        spec=importlib.util.spec_from_file_location('v6_transport',REPO/'experiments/runtime-spike/multinode-test.py')
        transport=importlib.util.module_from_spec(spec);spec.loader.exec_module(transport)
        tls=ssl.create_default_context(cafile=str(ca)); token=json.loads((managed/'node.json').read_text())['credential']
        prefix_peer=transport.WebSocket((url.hostname,url.port or 443),'/_nodes/node/'+node,{'Authorization':'Bearer '+token},tls)
        native_peer=None
        try:
            require(prefix_peer.status==101,'V6 prefix generation could not connect')
            native_peer=transport.WebSocket((url.hostname,url.port or 443),'/node/'+node,{'Authorization':'Bearer '+token},tls)
            require(native_peer.status==101,'V6 native replacement could not connect')
            prefix_peer.require_peer_close(timeout=8)
            p=subprocess.run([str(binary),'--local','--data-dir',str(controller_root),'node-revoke',node],
                             env=env,capture_output=True,timeout=10)
            require(p.returncode==0,'fixture owner revoke failed')
            native_peer.require_peer_close(timeout=8)
        finally:
            prefix_peer.close()
            if native_peer: native_peer.close()

        result['checks']['guest-cold-persistence'] = 'real prefixed managed guest exec/cold and host stop/start disk retention'
        result['checks']['start-stop-reconnect-revoke-fencing'] = 'managed prefix/native reconnect, generation replacement peer closure, active revoke closure and real guest PTY'
        result['V6_remaining'] = ['live a/b continuity read-only comparison','human/node auth via separate frozen V1 group','actual public Access/tunnel and second-host/NAT outside local fixture']
    finally:
        if (managed / 'host.json').exists():
            command('host','stop',timeout=30)
        require(guests() == baseline, 'V6 cleanup left VM or lost owner baseline')
        for procdir in Path('/proc').iterdir():
            if not procdir.name.isdigit(): continue
            try:
                if procdir.stat().st_uid != os.getuid(): continue
                require(not Path(os.readlink(procdir / 'cwd')).is_relative_to(managed), 'V6 process remains in test root')
                require(str(managed).encode() not in (procdir / 'cmdline').read_bytes().split(b'\0'), 'V6 worker/agent argv still references test root')
            except FileNotFoundError:
                continue
            except PermissionError:
                require(unrelated_supervisor(procdir), 'V6 unreadable unresolved process prevents positive cleanup')
        require(not (managed / 'control.sock').exists(), 'V6 control socket remains')
        require(frozen == source_manifest(args.lane) and digest(binary) == ready['binary_sha256'], 'V6 tested product changed')
        result['cleanup'] = {'test_processes':0,'test_guests':0,'owner_baseline':'preserved','test_artifacts':'retained privately; no owner artifact deletion'}


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['inspect', 'compare', 'cli', 'http', 'no-vm', 'ingress', 'artifact', 'guest'])
    parser.add_argument('--lane', type=Path, default=REPO.parent / 'open-workspaces-lanes/host-core')
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--fixture', type=Path)
    parser.add_argument('--ready-receipt', type=Path)
    parser.add_argument('--vm-turn', type=Path)
    parser.add_argument('--cases', nargs='+', help='Run only named no-vm groups after fixture/source corrections')
    parser.add_argument('--host-reserve-mib', type=int, default=4096)
    args = parser.parse_args()
    require(not PRIVATE.is_symlink(), 'symlink private root refused')
    PRIVATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(PRIVATE.stat().st_uid == os.getuid() and PRIVATE.stat().st_mode & 0o077 == 0,
            'owned private root must have mode0700')
    result = {'mode': args.mode, 'time_unix': time.time(), 'checks': {}, 'verdict': 'preparation only',
              'vm_run': False, 'live_cloud_write': False, 'physical_host': 'not tested'}
    failed = False
    try:
        if args.mode == 'inspect':
            require(args.host_reserve_mib >= 4096, 'host reserve cannot be reduced below4096MiB')
            inspect(args, result)
        elif args.mode == 'compare':
            compare(args, result)
        elif args.mode == 'artifact':
            require(args.fixture is not None and args.ready_receipt is not None, 'artifact inputs required')
            artifact(args, result)
        elif args.mode == 'guest':
            require(args.vm_turn is not None and args.fixture is not None and args.ready_receipt is not None and args.binary is not None, 'planner turn and guest inputs required')
            guest(args, result)
        elif args.mode == 'ingress':
            ingress(args, result)
        elif args.mode == 'no-vm':
            no_vm(args, result)
        elif args.mode == 'cli':
            require(args.binary is not None, '--binary required')
            cli(args, result)
        else:
            require(args.fixture is not None, '--fixture required')
            http_cases(args, result)
    except Exception as error:
        # Avoid leaking secret fixture content, arguments or host identifiers.
        result['error_type'] = type(error).__name__
        result['private_error'] = str(error)
        result['verdict'] = 'blocked; inspect private result and input locally'
        failed = True
    # Minimal acceptance labels reflect completed bounded adapters, independently
    # of the older broad framework checklist.
    if not failed:
        details = result.get('details', {})
        for label, name in [('V1','actual-gateway-and-native-controller-auth'),
                            ('V2','V2-guided-recovery'), ('V3','V3-state-unknown-stop')]:
            if details.get(name, {}).get('state') == 'passed':
                result['checks'][label] = 'focused adapter passed; see exact source/binary attribution'
        if args.mode == 'artifact' and 'stable-source-shipped-artifacts' in result['checks']:
            result['checks']['V5'] = 'exact frozen static CLI/clean published bundle verified offline'
        if args.mode == 'guest' and result.get('cleanup', {}).get('test_guests') == 0 and 'start-stop-reconnect-revoke-fencing' in result['checks']:
            result['checks']['V6'] = 'actual managed TLS/guest/lifecycle/PTY; positive cleanup and owner continuity'
    if args.ready_receipt:
        result['ready_receipt'] = str(args.ready_receipt.resolve())
        result['ready_receipt_sha256'] = digest(args.ready_receipt)
    result['pending'] = [name for name in REQUIRED if name not in result['checks']]
    result['minimal_pending'] = [name for name in ('V1','V2','V3','V4','V5','V6') if name not in result['checks']]
    output = PRIVATE / (args.mode + '-result.json')
    save(output, result)
    print(f'{args.mode}: {"blocked" if failed else "recorded"}; {len(result["pending"])} pending; {output.relative_to(REPO)}')
    return int(failed)


if __name__ == '__main__':
    raise SystemExit(main())
