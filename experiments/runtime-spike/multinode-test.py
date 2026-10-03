#!/usr/bin/env python3
"""Independent same-host TLS/node/VM acceptance; never deploys or uses existing data.

Requires a compiled Linux ow, prepared read-only assets, openssl and cc. Only
stdlib Python is used. The gateway alone receives a generated networking shim
for a synthetic Access JWKS fixture; node TLS/transport is unmodified.
"""
import argparse
import base64
import concurrent.futures
import errno
import fcntl
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import ssl
import struct
import subprocess
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[2]
TEAM = 'multinode-fixture.cloudflareaccess.com'
HOST = 'multinode.example.test'


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def wait_for(fn, timeout=30):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = fn()
            if value:
                return value
        except (OSError, sqlite3.Error, ValueError):
            pass
        time.sleep(.05)
    raise TimeoutError('condition not observed before deadline')


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def host_budget(extra=0):
    count = memory = 0
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            name = (proc / 'comm').read_text().strip()
        except FileNotFoundError:
            if not proc.exists():
                continue
            raise RuntimeError('live host process name is unavailable; guest demand unresolved')
        except PermissionError:
            raise RuntimeError('cannot inspect host process names; guest budget unknown')
        if not name.startswith('firecracker'):
            continue
        try:
            args = (proc / 'cmdline').read_bytes().split(b'\0')
            if b'--config-file' in args:
                config = Path(os.fsdecode(args[args.index(b'--config-file') + 1]))
                if not config.is_absolute():
                    config = proc / 'cwd' / config
                configured = json.loads(config.read_text())['machine-config']['mem_size_mib']
            else:
                api = os.fsdecode(args[args.index(b'--api-sock') + 1])
                with socket.socket(socket.AF_UNIX) as vm:
                    vm.settimeout(2)
                    vm.connect(api)
                    vm.sendall(b'GET /machine-config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
                    response = http.client.HTTPResponse(vm)
                    response.begin()
                    require(response.status == 200, 'cannot read running VM reservation')
                    configured = json.loads(response.read())['mem_size_mib']
            require(configured > 0, 'VM reservation unknown during boot')
            memory += configured
            count += 1
        except FileNotFoundError:
            if not proc.exists():
                continue
            raise RuntimeError('live Firecracker process has missing VM configuration; guest demand unresolved')
        except (OSError, ValueError, KeyError) as error:
            raise RuntimeError('running Firecracker reservation cannot be resolved') from error
    require(count + extra <= 4 and memory + extra * 256 <= 2048,
            'host-wide four-guest/2048-MiB verification budget unavailable')
    return {'guests': count, 'reserved_mib': memory}


def b64(data):
    return base64.urlsafe_b64encode(data).decode().rstrip('=')


class WebSocket:
    """Minimal bounded RFC6455 client for test frames (no external package)."""
    def __init__(self, address, path, headers=None, tls=None):
        self.s = socket.create_connection(address, timeout=10)
        if tls:
            self.s = tls.wrap_socket(self.s, server_hostname=address[0])
        self.buffer = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        fields = {'Host': f'{address[0]}:{address[1]}', 'Upgrade': 'websocket',
                  'Connection': 'Upgrade', 'Sec-WebSocket-Key': key,
                  'Sec-WebSocket-Version': '13', **(headers or {})}
        self.s.sendall((f'GET {path} HTTP/1.1\r\n' + ''.join(
            f'{k}: {v}\r\n' for k, v in fields.items()) + '\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            chunk = self.s.recv(4096)
            require(chunk, 'WebSocket handshake closed')
            self.buffer.extend(chunk)
            require(len(self.buffer) <= 16384, 'oversize WebSocket handshake')
        head, rest = bytes(self.buffer).split(b'\r\n\r\n', 1)
        self.buffer = bytearray(rest)
        self.status = int(head.split(b' ', 2)[1])
        if self.status == 101:
            expected = base64.b64encode(hashlib.sha1(
                (key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
            require(expected.lower() in head.lower(), 'invalid WebSocket accept')

    def exact(self, n):
        while len(self.buffer) < n:
            data = self.s.recv(max(4096, n - len(self.buffer)))
            if not data:
                raise EOFError('WebSocket closed')
            self.buffer.extend(data)
        result = bytes(self.buffer[:n])
        del self.buffer[:n]
        return result

    def send(self, data, kind=2):
        if isinstance(data, str):
            data = data.encode()
            kind = 1
        mask = os.urandom(4)
        n = len(data)
        header = bytes([0x80 | kind, 0x80 | (n if n < 126 else 126 if n < 65536 else 127)])
        if n >= 126:
            header += struct.pack('!H' if n < 65536 else '!Q', n)
        self.s.sendall(header + mask + bytes(v ^ mask[i % 4] for i, v in enumerate(data)))

    def recv(self):
        while True:
            a, length = self.exact(2)
            require(a & 128, 'fragmented test response not supported')
            require(not length & 128, 'server must not mask frames')
            if length == 126:
                length = struct.unpack('!H', self.exact(2))[0]
            elif length == 127:
                length = struct.unpack('!Q', self.exact(8))[0]
            require(length <= 1024 * 1024, 'oversize test response')
            data = self.exact(length)
            kind = a & 15
            if kind == 9:
                self.send(data, 10)
                continue
            if kind == 10:
                continue
            if kind == 8:
                raise EOFError('WebSocket close frame')
            return kind, data

    def close(self):
        self.s.close()

    def require_peer_close(self, timeout=10):
        """Timeout/silence is never closure evidence."""
        self.s.settimeout(.25)
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                self.recv()
            except (socket.timeout, TimeoutError):
                continue
            except EOFError:
                return 'EOF-or-WebSocket-Close'
            except OSError as error:
                if error.errno in (errno.ECONNRESET, errno.EPIPE, errno.ECONNABORTED):
                    return 'connection-reset'
                raise
        raise AssertionError('peer remains open or silent; no EOF/Close/reset observed')


class Harness:
    def __init__(self, args):
        self.args = args
        os.umask(0o077)
        self.root = Path(tempfile.mkdtemp(prefix='mnv-', dir=REPO / 'data'))
        self.controller = self.root / 'c'
        self.workers = {n: self.root / n for n in ['a', 'b']}
        self.processes = []
        self.agents = {}
        self.servers = []
        self.started_workers = []
        self.checks = []
        # Disjoint hard caps: both workers together <= four guests/1024 MiB/four vCPUs.
        self.env = dict(os.environ, OW_ASSET_DIR=str(args.assets.resolve()),
                        OW_MAX_MEMORY_MIB='512', OW_MAX_RUNNING='2', OW_MAX_VCPUS='2')
        for k in ['OW_SERVER', 'OW_DATA_DIR', 'LD_PRELOAD', 'HTTPS_PROXY', 'HTTP_PROXY',
                  'ALL_PROXY', 'https_proxy', 'http_proxy', 'all_proxy']:
            self.env.pop(k, None)
        self.node_port = port()
        self.gateway_port = port()
        self.origin = f'https://127.0.0.1:{self.node_port}'
        self.ca = self.root / 'ca.pem'
        self.result = {'state': 'running', 'checks': self.checks,
                       'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                       'limits': {'peak_planned_guests': 3, 'peak_planned_mib': 768},
                       'evidence': 'per-check labels; same physical host; guest evidence only in full mode; synthetic signed human identities',
                       'physical_host_nat': 'not exercised', 'deployment': 'not performed'}
        if args.source_manifest:
            self.result['tested_product_source'] = json.loads(args.source_manifest.read_text())
        self.save()

    def save(self):
        (self.root / 'result.json').write_text(json.dumps(self.result, indent=2))

    def command(self, root, *args):
        return [str(self.args.binary.resolve()), '--local', '--data-dir', str(root), *map(str, args)]

    def cli(self, root, *args, ok=True, timeout=120):
        r = subprocess.run(self.command(root, *args), env=self.env, capture_output=True,
                           timeout=timeout, text=True)
        # Never include raw output in failures: enrollment/config may contain secrets.
        require((r.returncode == 0) == ok, f'CLI {args[0]} unexpected exit {r.returncode}')
        return r

    def launch(self, root, *args, env=None):
        log = open(self.root / f'process-{len(self.processes)}.log', 'ab', buffering=0)
        p = subprocess.Popen(self.command(root, *args), env=env or self.env,
                             stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                             start_new_session=True)
        log.close()
        self.processes.append(p)
        return p

    def stop(self, p):
        if p.poll() is None:
            os.killpg(p.pid, signal.SIGTERM)
            try:
                p.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL)
                p.wait(timeout=5)

    def check(self, name, kind, fn):
        started = time.monotonic()
        entry = {'name': name, 'evidence': kind}
        try:
            detail = fn()
            entry.update(status='passed', detail=detail)
        except Exception as error:
            # Test assertions contain only deliberately non-secret text.
            entry.update(status='failed', error=type(error).__name__ + ': ' + str(error))
            raise
        finally:
            entry['elapsed_s'] = round(time.monotonic() - started, 3)
            self.checks.append(entry)
            self.save()
            print(name + ': ' + entry['status'], flush=True)

    def sql(self, root, query, params=()):
        with sqlite3.connect(f'file:{root / "catalog.sqlite3"}?mode=ro', uri=True) as db:
            return db.execute(query, params).fetchall()

    def nodes(self):
        return json.loads(self.cli(self.controller, 'nodes').stdout)

    def online(self, node):
        return any(n['id'] == node and n['online'] for n in self.nodes())

    def wire(self, root, request):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(120)
            s.connect(str(root / 'control.sock'))
            s.sendall(json.dumps(request, separators=(',', ':')).encode() + b'\n')
            f = s.makefile('rb')
            line = f.readline(1024 * 1024 + 1)
            require(line.endswith(b'\n'), 'worker/proxy response lost')
            return json.loads(line)

    def routed(self, node, request):
        response = self.wire(self.controller / 'nodes' / node, request)
        require(response.get('ok'), 'routed worker operation rejected')
        return response['result']

    def certificates(self):
        def openssl(*args):
            subprocess.run(['openssl', *map(str, args)], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                '-subj', '/CN=Multi-node temporary test CA',
                '-addext', 'basicConstraints=critical,CA:TRUE',
                '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
                '-keyout', self.root / 'ca.key', '-out', self.ca)
        for name, san in [('node', 'DNS:localhost,IP:127.0.0.1'),
                          ('jwks', f'DNS:{TEAM}'), ('wrong', 'DNS:wrong.example.test')]:
            key, csr, cert = [self.root / (name + suffix) for suffix in ['.key', '.csr', '.pem']]
            ext = self.root / (name + '.ext')
            ext.write_text(f'subjectAltName={san}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
            openssl('req', '-new', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=Test fixture',
                    '-keyout', key, '-out', csr)
            openssl('x509', '-req', '-in', csr, '-CA', self.ca, '-CAkey', self.root / 'ca.key',
                    '-CAcreateserial', '-days', '1', '-extfile', ext, '-out', cert)
        self.tls = ssl.create_default_context(cafile=str(self.ca))

    def http(self, path, data=None, headers=None, address=None, tls=False):
        address = address or ('127.0.0.1', self.node_port)
        c = http.client.HTTPSConnection(*address, context=self.tls, timeout=120) if tls else http.client.HTTPConnection(*address, timeout=120)
        try:
            body = None if data is None else json.dumps(data).encode()
            c.request('GET' if data is None else 'POST', path, body,
                      {'Content-Type': 'application/json', **(headers or {})})
            r = c.getresponse()
            raw = r.read(8 * 1024 * 1024)
            try:
                value = json.loads(raw)
            except ValueError:
                value = {}
            return r.status, value
        finally:
            c.close()

    def start_controller(self):
        self.control_process = self.launch(self.controller, 'node-controller', '--listen',
            f'127.0.0.1:{self.node_port}', '--tls-cert', self.root / 'node.pem', '--tls-key', self.root / 'node.key')
        wait_for(lambda: self.http('/enroll', {}, tls=True)[0] == 401)

    def mint(self, node, ttl=600, memory=512, slots=2):
        path = self.root / (node + '-join.json')
        self.cli(self.controller, 'node-join', node, '--output', path, '--ttl', ttl,
                 '--memory', memory, '--slots', slots)
        return path

    def start_agent(self, node, join=None):
        args = ['node-agent', '--controller', self.origin, '--credential', self.root / (node + '-credential.json'), '--ca-cert', self.ca]
        if join:
            args += ['--join', join]
        self.agents[node] = self.launch(self.workers[node], *args)
        wait_for(lambda: self.online(node))
        wait_for(lambda: (self.controller / 'nodes' / node / 'control.sock').exists())

    def enrollment_tests(self):
        for length in [0, 1, 63, 65, 4096]:
            status, _ = self.http('/enroll', {'node': 'invalid', 'secret': 'a' * length}, tls=True)
            require(status in [401, 413], 'invalid enrollment length accepted')
        exp = self.mint('expired', ttl=1)
        time.sleep(1.2)
        require(self.http('/enroll', json.loads(exp.read_text()), tls=True)[0] == 401,
                'expired enrollment accepted')
        for node in ['a', 'b']:
            path = self.mint(node)
            replay = json.loads(path.read_text())
            if node == 'a':
                # Client CA refusal must happen before single-use secret consumption.
                self.cli(self.workers[node], 'node-agent', '--controller', self.origin,
                    '--credential', self.root / 'untrusted.json', '--join', path, ok=False, timeout=15)
                require(not (self.root / 'untrusted.json').exists(), 'untrusted TLS enrolled')
            self.start_agent(node, path)
            require(self.http('/enroll', replay, tls=True)[0] == 401, 'join replay accepted')
            require(self.http('/enroll', {'node': 'other', 'secret': replay['secret']}, tls=True)[0] == 401,
                    'join secret reusable by another identity')
            cred = self.root / (node + '-credential.json')
            require(cred.stat().st_mode & 0o077 == 0, 'credential is not private')
        for length in [0, 1, 63, 65, 4096]:
            ws = WebSocket(('127.0.0.1', self.node_port), '/node/a',
                           {'Authorization': 'Bearer ' + 'b' * length}, self.tls)
            try:
                require(ws.status != 101, 'invalid node credential length accepted')
            finally:
                ws.close()
        return {'agents': 2, 'ca': 'temporary trusted CA; default trust rejected', 'replay': 'rejected'}

    def redirect_and_hostname(self):
        hits = []
        class Redirect(http.server.BaseHTTPRequestHandler):
            def do_POST(handler):
                hits.append(handler.path)
                handler.rfile.read(int(handler.headers.get('Content-Length', '0')))
                handler.send_response(307)
                handler.send_header('Location', '/secret-sink')
                handler.send_header('Content-Length', '0')
                handler.end_headers()
            def log_message(*args):
                pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Redirect)
        self.servers.append(server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        testroot = self.root / 'negative'
        self.cli(testroot, 'nodes')  # initialize an empty owned root
        path = self.mint('redirect-test')
        self.cli(testroot, 'node-agent', '--controller', f'http://127.0.0.1:{server.server_port}',
                 '--insecure-loopback-test', '--join', path, '--credential', self.root / 'redirect-cred.json', ok=False)
        require(hits == ['/enroll'], 'enrollment followed a credential-bearing redirect')
        bad_port = port()
        wrongroot = self.root / 'wrong-controller'
        self.cli(wrongroot, 'nodes')
        process = self.launch(wrongroot, 'node-controller', '--listen', f'127.0.0.1:{bad_port}',
                              '--tls-cert', self.root / 'wrong.pem', '--tls-key', self.root / 'wrong.key')
        wait_for(lambda: process.poll() is None and self.listening(bad_port))
        self.cli(testroot, 'node-agent', '--controller', f'https://127.0.0.1:{bad_port}',
                 '--ca-cert', self.ca, '--join', path, '--credential', self.root / 'wrong-cred.json', ok=False)
        require(not (self.root / 'wrong-cred.json').exists(), 'wrong hostname TLS accepted')
        self.stop(process)
        self.cli(testroot, 'node-agent', '--controller', 'http://192.0.2.1:9999',
                 '--insecure-loopback-test', '--credential', self.root / 'absent.json', ok=False)
        return {'redirect_requests': len(hits), 'hostname_mismatch': 'rejected', 'remote_plaintext': 'rejected'}

    def listening(self, number):
        try:
            with socket.create_connection(('127.0.0.1', number), timeout=.2):
                return True
        except OSError:
            return False

    def jwt_fixture(self):
        key = self.root / 'jwt.key'
        subprocess.run(['openssl', 'genpkey', '-algorithm', 'RSA', '-pkeyopt', 'rsa_keygen_bits:2048', '-out', str(key)],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        modulus = subprocess.check_output(['openssl', 'rsa', '-in', str(key), '-noout', '-modulus'], stderr=subprocess.DEVNULL)
        n = bytes.fromhex(modulus.decode().strip().split('=')[1])
        keys = json.dumps({'keys': [{'kty': 'RSA', 'kid': 'fixture', 'alg': 'RS256', 'use': 'sig', 'n': b64(n), 'e': 'AQAB'}]}).encode()
        class JWKS(http.server.BaseHTTPRequestHandler):
            def do_GET(handler):
                require(handler.path == '/cdn-cgi/access/certs', 'unexpected fixture issuer path')
                handler.send_response(200)
                handler.send_header('Content-Type', 'application/json')
                handler.send_header('Content-Length', str(len(keys)))
                handler.end_headers()
                handler.wfile.write(keys)
            def log_message(*args):
                pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), JWKS)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(self.root / 'jwks.pem', self.root / 'jwks.key')
        server.socket = ctx.wrap_socket(server.socket, server_side=True)
        self.servers.append(server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        # Deliberately synthetic network routing, applied to gateway process only.
        source = self.root / 'fixture.c'
        source.write_text(r'''
#define _GNU_SOURCE
#include <dlfcn.h>
#include <netdb.h>
#include <arpa/inet.h>
#include <stdlib.h>
#include <string.h>
int getaddrinfo(const char *n,const char *s,const struct addrinfo *h,struct addrinfo **r){
  int(*real)(const char*,const char*,const struct addrinfo*,struct addrinfo**)=dlsym(RTLD_NEXT,"getaddrinfo");
  if(n && !strcmp(n,"multinode-fixture.cloudflareaccess.com")) return real("127.0.0.1",getenv("OW_TEST_JWKS_PORT"),h,r);
  return real(n,s,h,r);
}
int connect(int fd,const struct sockaddr *a,socklen_t len){
  int(*real)(int,const struct sockaddr*,socklen_t)=dlsym(RTLD_NEXT,"connect");
  if(a->sa_family==AF_INET && len>=sizeof(struct sockaddr_in)){
    struct sockaddr_in copy=*(const struct sockaddr_in*)a;
    if(copy.sin_addr.s_addr==htonl(INADDR_LOOPBACK) && copy.sin_port==htons(443)){
      copy.sin_port=htons(atoi(getenv("OW_TEST_JWKS_PORT")));
      return real(fd,(const struct sockaddr*)&copy,sizeof(copy));
    }
  }
  return real(fd,a,len);
}
''')
        shim = self.root / 'fixture.so'
        subprocess.run(['cc', '-shared', '-fPIC', '-o', str(shim), str(source), '-ldl'], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.gateway_env = dict(self.env, LD_PRELOAD=str(shim), SSL_CERT_FILE=str(self.ca),
                                OW_TEST_JWKS_PORT=str(server.server_port))
        self.tokens = {}
        for user in ['alice', 'bob']:
            header = b64(json.dumps({'alg': 'RS256', 'kid': 'fixture'}).encode())
            claims = b64(json.dumps({'iss': 'https://' + TEAM, 'aud': 'fixture-audience', 'sub': user + '-sub',
                                     'email': user + '@example.test', 'exp': int(time.time()) + 1800}).encode())
            message = (header + '.' + claims).encode()
            signature = subprocess.run(['openssl', 'dgst', '-sha256', '-sign', str(key)],
                                       input=message, capture_output=True, check=True).stdout
            self.tokens[user] = message.decode() + '.' + b64(signature)
        self.config = self.root / 'gateway.json'
        self.config.write_text(json.dumps({'hostname': HOST, 'team_domain': TEAM, 'audience': 'fixture-audience',
            'allowed_emails': ['alice@example.test', 'bob@example.test'], 'upstream': 'http://127.0.0.1:9'}))
        self.start_gateway()
        require(self.api(user='alice')['workspaces'] == [], 'fixture first owner unexpectedly has machines')
        require(self.api(user='bob')['workspaces'] == [], 'fixture second owner unexpectedly has machines')
        status, _ = self.http('/api/state', headers={'Host': HOST}, address=('127.0.0.1', self.gateway_port))
        require(status == 401, 'unsigned gateway request accepted')
        return {'signed_users': 2, 'unsigned_request': 'rejected', 'public_access_login': 'not exercised'}

    def start_gateway(self):
        self.gateway = self.launch(self.controller, 'dashboard', '--config', self.config,
             '--listen', f'127.0.0.1:{self.gateway_port}', env=self.gateway_env)
        wait_for(lambda: self.listening(self.gateway_port), timeout=40)

    def headers(self, user):
        return {'Host': HOST, 'Origin': 'https://' + HOST, 'cf-access-jwt-assertion': self.tokens[user], 'x-ow-request': 'dashboard'}

    def api(self, request=None, user='alice', ok=True):
        status, value = self.http('/api/state' if request is None else '/api/operation', request,
                                 self.headers(user), ('127.0.0.1', self.gateway_port))
        require((status == 200 and value.get('ok') is True) == ok,
                f'gateway {request["op"] if request else "state"} unexpected status {status}')
        return value.get('result') if ok else (status, value)

    def op(self, op, id='demo', **fields):
        return self.api({'op': op, 'id': id, **fields})

    def physical(self, name, owner='alice'):
        return self.sql(self.controller, "SELECT physical,node FROM resources WHERE kind='machine' AND name=? AND owner IN (SELECT id FROM users WHERE email=?)", (name, owner + '@example.test'))[0]

    def guest(self, command, id='demo'):
        value = self.op('exec', id, command=command)
        require(value['exit_code'] == 0, 'real guest command failed')
        return value['output']

    def journal_count(self):
        total = 0
        for root in self.workers.values():
            path = root / 'node-operations.sqlite3'
            if path.exists():
                with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
                    total += db.execute('SELECT count(*) FROM effects').fetchone()[0]
        return total

    def worker_caps(self):
        namespaces = []
        for node, root in self.workers.items():
            status = self.wire(root, {'op': 'status'})
            require(status.get('ok'), 'worker status unavailable')
            caps = status['result']
            require((caps['max_memory_mib'], caps['max_running'], caps['max_vcpus']) == (512, 2, 2),
                    'worker did not inherit disjoint hard caps; refusing guest launch')
            require(caps['running'] == 0, 'new worker unexpectedly has guests')
            pid = caps['worker_pid']
            worker = Path('/proc') / str(pid)
            inherited = (worker / 'environ').read_bytes().split(b'\0')
            require(all((key + '=' + value).encode() in inherited for key, value in
                        [('OW_MAX_MEMORY_MIB', '512'), ('OW_MAX_RUNNING', '2'), ('OW_MAX_VCPUS', '2')]),
                    'worker process environment does not carry all hard caps')
            namespace = os.readlink(worker / 'ns/net')
            require(namespace != os.readlink('/proc/self/ns/net'), 'worker shares verifier host network namespace')
            namespaces.append(namespace)
            require(root.stat().st_mode & 0o077 == 0, 'worker root not private')
        require(len(set(namespaces)) == 2, 'workers share a network namespace')
        return {'per_worker': {'memory_mib': 512, 'guests': 2, 'vcpus': 2},
                'aggregate_max_memory_mib': 1024, 'distinct_rootless_networks': 2,
                'reported_and_inherited_caps': True}

    def vm_operations(self):
        host_budget(1)
        created = self.op('create', node='a', memory_mib=256, operation_key='create-demo')
        require(created['node'] == 'a', 'explicit placement ignored')
        require('REAL_VM_ALICE' in self.guest('uname -r; echo REAL_VM_ALICE; echo ALICE >/persist/owner; sync'), 'guest exec missing')
        physical, node = self.physical('demo')
        payload = os.urandom(4096)
        self.routed(node, {'op': 'put', 'id': physical, 'path': '/persist/binary', 'data': base64.b64encode(payload).decode()})
        got = self.routed(node, {'op': 'get', 'id': physical, 'path': '/persist/binary'})
        require(base64.b64decode(got['data']) == payload, 'routed binary transfer mismatch')
        self.op('stop')
        host_budget(1)
        self.op('start')
        require('ALICE' in self.guest('cat /persist/owner'), 'cold disk persistence missing')
        self.guest('sleep 600 >/dev/null 2>&1 & echo $! >/persist/sleeper; sync')
        snap = self.op('snapshot', name='prepared')
        require(snap['node'] == 'a', 'snapshot changed node')
        second = self.op('snapshot', name='second-capture')
        require(second['node'] == 'a' and second['name'] != snap['name'], 'two named captures conflated')
        host_budget(1)
        child = self.op('fork', child='branch', snapshot='prepared')
        require(child['node'] == 'a', 'fork changed node')
        child_before = self.sql(self.controller, "SELECT physical,node,reserved_mib,reserved_cpus,metadata FROM resources WHERE kind='machine' AND name='branch'")
        journal_before = self.journal_count()
        self.api({'op': 'fork', 'id': 'demo', 'child': 'branch', 'snapshot': 'second-capture'}, ok=False)
        require(self.journal_count() == journal_before and child_before == self.sql(self.controller,
                "SELECT physical,node,reserved_mib,reserved_cpus,metadata FROM resources WHERE kind='machine' AND name='branch'"),
                'changed fork snapshot fingerprint dispatched or altered existing child')
        require('RESTORED_PROCESS' in self.guest('kill -0 $(cat /persist/sleeper) && echo RESTORED_PROCESS', 'branch'), 'snapshot RAM process not restored')
        self.guest('echo CHILD >/persist/owner; sync', 'branch')
        require('ALICE' in self.guest('cat /persist/owner'), 'fork modified parent')
        require('CHILD' in self.guest('cat /persist/owner', 'branch'), 'child write missing')
        self.op('stop', 'branch')
        host_budget(1)
        auto = self.op('create', 'automatic', memory_mib=256)
        require(auto['node'] == 'b', 'automatic placement did not choose less-reserved node')
        before = self.sql(self.controller, "SELECT owner,kind,name,physical,node,reserved_mib,reserved_cpus,metadata FROM resources WHERE kind='machine' AND name='automatic'")
        dispatch = self.journal_count()
        self.api({'op': 'fork', 'id': 'demo', 'child': 'automatic', 'snapshot': 'prepared'}, ok=False)
        self.api({'op': 'restore', 'id': 'automatic', 'name': 'prepared'}, ok=False)
        after = self.sql(self.controller, "SELECT owner,kind,name,physical,node,reserved_mib,reserved_cpus,metadata FROM resources WHERE kind='machine' AND name='automatic'")
        require(after == before and self.journal_count() == dispatch,
                'denied cross-node existing-child fork changed placement/reservation or dispatched')
        self.op('stop', 'automatic')
        self.op('hibernate')
        host_budget(1)
        self.op('start')
        require('ALICE' in self.guest('cat /persist/owner'), 'hibernate persistence missing')
        self.op('restore', name='prepared')
        self.guest('kill -0 $(cat /persist/sleeper)')
        return {'fixed_node': 'a', 'automatic_node': 'b', 'binary_bytes': len(payload),
                'ram_process_restored': True, 'distinct_named_captures': 2,
                'existing_cross_node_child_preserved': True}

    def ownership_and_capacity(self):
        before = self.journal_count()
        physical, _ = self.physical('demo')
        for op in ['exec', 'start', 'stop', 'hibernate', 'snapshot', 'fork', 'restore']:
            fields = {'command': 'true'} if op == 'exec' else {'name': 'prepared'} if op in ['snapshot', 'restore'] else {'child': 'steal', 'snapshot': 'prepared'} if op == 'fork' else {}
            self.api({'op': op, 'id': 'demo', **fields}, user='bob', ok=False)
        self.api({'op': 'exec', 'id': physical, 'command': 'true'}, user='bob', ok=False)
        ws = WebSocket(('127.0.0.1', self.gateway_port), '/api/terminal/demo', self.headers('bob'))
        require(ws.status != 101, 'cross-owner terminal accepted')
        ws.close()
        require(self.journal_count() == before, 'cross-owner denial dispatched a mutation')
        before_inventory = {n: self.wire(r, {'op': 'list'}) for n, r in self.workers.items()}
        self.api({'op': 'create', 'id': 'oversized', 'node': 'a', 'memory_mib': 1024}, ok=False)
        self.api({'op': 'create', 'id': 'bad-cpu', 'node': 'a', 'vcpu_count': 4}, ok=False)
        require(before_inventory == {n: self.wire(r, {'op': 'list'}) for n, r in self.workers.items()}, 'capacity denial created worker resources')
        host_budget(1)
        bob = self.api({'op': 'create', 'id': 'demo', 'node': 'b', 'memory_mib': 256}, user='bob')
        require(bob['node'] == 'b', 'same-name second owner placement failed')
        self.api({'op': 'exec', 'id': 'demo', 'command': 'echo BOB >/persist/owner; cat /persist/owner'}, user='bob')
        require('ALICE' in self.guest('cat /persist/owner'), 'second owner changed first owner disk')
        self.api({'op': 'restore', 'id': 'demo', 'name': 'prepared'}, user='bob', ok=False)
        bob_physical, _ = self.physical('demo', owner='bob')
        before = self.journal_count()
        self.api({'op': 'exec', 'id': bob_physical, 'command': 'true'}, ok=False)
        require(self.journal_count() == before, 'raw second-owner physical-ID guessing dispatched')
        self.api({'op': 'stop', 'id': 'demo'}, user='bob')
        node_credential = json.loads((self.root / 'a-credential.json').read_text())['credential']
        status, _ = self.http('/api/state', headers={'Host': HOST, 'cf-access-jwt-assertion': node_credential}, address=('127.0.0.1', self.gateway_port))
        require(status == 401, 'node secret accepted as human JWT')
        ws = WebSocket(('127.0.0.1', self.node_port), '/node/a', {'Authorization': 'Bearer ' + self.tokens['alice']}, self.tls)
        require(ws.status != 101, 'human JWT accepted as node credential')
        ws.close()
        return {'cross_owner_mutation_dispatch_delta': 0, 'same_names_independent': True}

    def terminal(self):
        ws = WebSocket(('127.0.0.1', self.gateway_port), '/api/terminal/demo', self.headers('alice'))
        try:
            require(ws.status == 101, 'owned terminal upgrade rejected')
            # Command echo cannot itself contain the full success marker.
            ws.send(b"test -t 0 && printf 'ROUTED_%s\\n' 'PTY'; cat /persist/owner\r")
            output = bytearray()
            end = time.monotonic() + 15
            while time.monotonic() < end:
                kind, data = ws.recv()
                if kind == 2:
                    output.extend(data)
                    ws.send(json.dumps({'type': 'ack', 'bytes': len(data)}))
                    if b'ROUTED_PTY' in output and b'ALICE' in output:
                        break
            require(b'ROUTED_PTY' in output and b'ALICE' in output, 'real routed guest TTY missing')
            ws.send(b'exit\r')
            return {'guest_isatty': True, 'placement': 'a'}
        finally:
            ws.close()

    def recovery(self):
        placement = self.physical('demo')
        self.stop(self.agents['a'])
        wait_for(lambda: not self.online('a'), timeout=25)
        before = self.journal_count()
        self.api({'op': 'exec', 'id': 'demo', 'command': 'echo forbidden'}, ok=False)
        self.api({'op': 'create', 'id': 'offline-create', 'node': 'a'}, ok=False)
        require(self.journal_count() == before, 'offline request dispatched')
        self.start_agent('a')
        require(self.physical('demo') == placement, 'reconnect changed identity/placement')
        self.stop(self.control_process)
        self.start_controller()
        wait_for(lambda: self.online('a') and self.online('b'))
        self.stop(self.gateway)
        self.start_gateway()
        require(self.physical('demo') == placement, 'restart changed identity/placement')
        require('ALICE' in self.guest('cat /persist/owner'), 'restart guest unreachable')
        return {'offline_dispatch_delta': 0, 'placement_retained': True}

    def failure_reservations(self):
        # Only disposable harness artifacts move temporarily; restore them in finally.
        child, node = self.physical('branch')
        disk = self.workers[node] / 'machines' / child / 'disk.ext4'
        held = disk.with_name('disk.verify-held')
        require(disk.exists() and not held.exists(), 'disposable child disk fixture unavailable')
        disk.rename(held)
        try:
            self.api({'op': 'start', 'id': 'branch', 'operation_key': 'failed-child-start'}, ok=False)
            reserved = self.sql(self.controller, "SELECT node,reserved_mib,reserved_cpus,json_extract(metadata,'$.state') FROM resources WHERE kind='machine' AND name='branch'")[0]
            require(reserved == ('a', 0, 0, 'stopped'), 'failed start did not reconcile confirmed stopped demand')
        finally:
            held.rename(disk)
        snapshot = self.sql(self.controller, "SELECT physical,node FROM resources WHERE kind='snapshot' AND name='second-capture'")[0]
        path = self.workers[snapshot[1]] / 'snapshots' / snapshot[0]
        held = path.with_name(path.name + '.verify-held')
        require(path.is_dir() and not held.exists(), 'disposable snapshot fixture unavailable')
        before = self.sql(self.controller, "SELECT node,reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND name='demo' AND owner IN (SELECT id FROM users WHERE email='alice@example.test')")
        path.rename(held)
        try:
            self.api({'op': 'restore', 'id': 'demo', 'name': 'second-capture', 'operation_key': 'failed-parent-restore'}, ok=False)
            after = self.sql(self.controller, "SELECT node,reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND name='demo' AND owner IN (SELECT id FROM users WHERE email='alice@example.test')")
            require(after == before and after[0][1:] == (256, 1), 'failed restore released live guest reservation')
            require('ALICE' in self.guest('cat /persist/owner'), 'failed restore stopped original running guest')
        finally:
            held.rename(path)
        return {'missing_disk_start': 'positive stopped evidence releases demand',
                'missing_capture_restore': 'live guest memory/CPU reservation retained',
                'artifact_moves': 'disposable fixtures restored byte-for-byte'}

    def browser_reload(self):
        module = REPO / 'data/dashboard-tests/node_modules/playwright'
        require(module.is_dir() and Path('/usr/bin/chromium').exists(),
                'existing Playwright/Chromium unavailable; browser reload acceptance remains a gap')
        config = self.root / 'browser-input.json'
        config.write_text(json.dumps({'playwright': str(module), 'origin': 'https://' + HOST,
            'upstream': f'http://127.0.0.1:{self.gateway_port}', 'headers': self.headers('alice')}))
        script = self.root / 'browser-reload.cjs'
        script.write_text(r'''
const fs=require('node:fs'), assert=require('node:assert/strict');
const input=JSON.parse(fs.readFileSync(process.argv[2],'utf8'));
const {chromium}=require(input.playwright);
(async()=>{
 const browser=await chromium.launch({executablePath:'/usr/bin/chromium',headless:true});
 try {
  const context=await browser.newContext();
  const page=await context.newPage();
  const seen={exec:[],restore:[]};
  const aborted=new Set();
  // Browser origin/edge are synthetic. Actual HTTP gateway validates signed JWT
  // and dispatches through the real node transport/guest for each operation.
  await page.route(input.origin+'/**',async route=>{
   const req=route.request();
   const path=new URL(req.url()).pathname;
   const data=req.postDataJSON();
   const response=await context.request.fetch(input.upstream+path,{
    method:req.method(),headers:{...req.headers(),...input.headers},
    data:req.postData() || undefined,timeout:120000});
   if(path==='/api/operation' && data && (data.op==='exec' || data.op==='restore')) {
    seen[data.op].push(data);
    if(!aborted.has(data.op)) {aborted.add(data.op);await route.abort('failed');return;}
   }
   await route.fulfill({response});
  });
  await page.goto(input.origin+'/');
  await page.locator('[data-machine="demo"]').click();
  const command="echo BROWSER_ONCE >>/persist/browser-effects; sync; printf 'COUNT='; wc -l </persist/browser-effects";
  await page.locator('#command').fill(command);
  await page.locator('#run-command').click();
  await page.waitForFunction(()=>!document.getElementById('run-command').disabled &&
    Object.keys(sessionStorage).some(k=>k.startsWith('ow-operation:')));
  const saved=await page.evaluate(()=>Object.fromEntries(Object.entries(sessionStorage)));
  await page.reload();
  assert.deepEqual(await page.evaluate(()=>Object.fromEntries(Object.entries(sessionStorage))),saved);
  await page.locator('[data-machine="demo"]').click();
  await page.locator('#command').fill(command);
  await page.locator('#run-command').click();
  await page.waitForFunction(()=>!document.getElementById('run-command').disabled &&
    document.getElementById('console-output').textContent.includes('COUNT=1'));
  assert.equal(seen.exec.length,2);
  assert.equal(seen.exec[0].operation_key,seen.exec[1].operation_key);
  assert.equal(await page.evaluate(()=>Object.keys(sessionStorage).filter(k=>k.startsWith('ow-operation:')).length),0);
  await page.locator('[data-view="snapshots"]').click();
  const row=page.locator('#snapshot-list .snapshot-row').filter({has:page.getByRole('heading',{name:'prepared',exact:true})});
  await row.getByRole('button',{name:'Restore',exact:true}).click();
  await page.locator('#dialog-submit').click();
  await page.locator('#dialog-error').waitFor({state:'visible'});
  await page.waitForFunction(()=>!document.getElementById('cancel-dialog').disabled);
  await page.locator('#cancel-dialog').click();
  await row.getByRole('button',{name:'Restore',exact:true}).click();
  await page.locator('#dialog-submit').click();
  await page.locator('#operation-dialog').waitFor({state:'hidden'});
  assert.equal(seen.restore.length,2);
  assert.equal(seen.restore[0].operation_key,seen.restore[1].operation_key);
  assert.equal(await page.evaluate(()=>Object.keys(sessionStorage).filter(k=>k.startsWith('ow-operation:')).length),0);
  fs.writeFileSync(process.argv[3],JSON.stringify({passed:true,exec_page_reload_same_key:true,
    exec_guest_count:1,restore_dialog_reopen_same_key:true,keys_cleared_after_success:true,
    fault:'actual gateway reply intentionally lost at browser interception; agent/controller complete'}));
 } finally {await browser.close();}
})().catch(()=>{console.error('Browser reload acceptance failed; inspect private process artifacts');process.exit(1);});
''')
        output = self.root / 'browser-result.json'
        log = self.root / 'browser.log'
        with log.open('wb') as stream:
            result = subprocess.run(['node', str(script), str(config), str(output)],
                                    env=self.env, stdout=stream, stderr=stream, timeout=120)
        require(result.returncode == 0 and output.exists(), 'actual browser saved-key reload/reopen acceptance failed')
        return json.loads(output.read_text())

    def lost_response(self, create=False):
        name = 'lost-create' if create else 'demo'
        key = 'lost-create-key' if create else 'unknown-exec-key'
        request = {'op': 'create' if create else 'exec', 'id': name, 'operation_key': key}
        if create:
            host_budget(1)
            request.update(node='b', memory_mib=256)
        else:
            request['command'] = 'echo ONCE >>/persist/effects; sync; sleep 5; echo FINISHED'
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(self.api, request)
            rows = wait_for(lambda: self.sql(self.controller, 'SELECT id,state,node FROM operations WHERE retry_key=?', (key,)))
            operation, _, node = rows[0]
            journal = self.workers[node] / 'node-operations.sqlite3'
            def accepted():
                if not journal.exists():
                    return False
                with sqlite3.connect(f'file:{journal}?mode=ro', uri=True) as db:
                    return db.execute('SELECT count(*) FROM effects WHERE key=?', (f'op-{operation}',)).fetchone()[0] == 1
            wait_for(accepted)
            require(not future.done(), 'fault injected too late; response already received')
            self.stop(self.gateway)
            try:
                future.result(timeout=10)
                raise AssertionError('fault failed to lose original gateway response')
            except (OSError, http.client.HTTPException):
                pass
        def completed():
            with sqlite3.connect(f'file:{journal}?mode=ro', uri=True) as db:
                return db.execute('SELECT response IS NOT NULL FROM effects WHERE key=?', (f'op-{operation}',)).fetchone()[0]
        wait_for(completed, timeout=120)
        pinned = self.physical(name)
        self.start_gateway()
        visible = self.api()['operations']
        require(any(v['id'] == operation and v['state'] == 'uncertain' for v in visible),
                'restarted gateway did not expose unknown operation outcome')
        private = self.api(user='bob')['operations']
        require(all(v['id'] != operation for v in private), 'unknown operation visible to another owner')
        result = self.api(request)
        require(result['node'] == node and self.physical(name) == pinned, 'lost response retry changed placement/identity')
        rows = self.sql(self.controller, 'SELECT state FROM operations WHERE retry_key=?', (key,))
        require(rows == [('succeeded',)], 'retry did not resolve durable operation')
        if create:
            physical, _ = pinned
            inventory = self.wire(self.workers[node], {'op': 'list'})['result']
            require(sum(v['id'] == physical for v in inventory) == 1, 'lost create duplicated runtime identity')
            self.op('stop', name)
        else:
            require(self.guest('wc -l </persist/effects').strip().endswith('1'), 'unknown exec replayed')
            changed = dict(request, command='echo TWICE >>/persist/effects')
            self.api(changed, ok=False)
        return {'fault': 'gateway terminated after real agent journal acceptance; journal completion observed before retry',
                'agent_crash_pending_recovery': 'not exercised', 'original_node_retained': True, 'one_effect': True}

    def fencing_and_revocation(self):
        self.stop(self.agents['a'])
        credential = json.loads((self.root / 'a-credential.json').read_text())['credential']
        headers = {'Authorization': 'Bearer ' + credential}
        old = WebSocket(('127.0.0.1', self.node_port), '/node/a', headers, self.tls)
        replacement = WebSocket(('127.0.0.1', self.node_port), '/node/a', headers, self.tls)
        try:
            require(old.status == replacement.status == 101, 'duplicate-session fixture could not connect')
            old.require_peer_close()
        finally:
            old.close()
            replacement.close()
        self.start_agent('a')
        # Freeze the live agent, keeping its existing job TCP connections open.
        # A synthetic authenticated main-session replacement must close the already
        # open guest stream independently of killing the old job process.
        active = WebSocket(('127.0.0.1', self.gateway_port), '/api/terminal/demo', self.headers('alice'))
        require(active.status == 101, 'PTY unavailable for active replacement test')
        frozen = self.agents['a']
        replacement = None
        try:
            os.kill(frozen.pid, signal.SIGSTOP)
            replacement = WebSocket(('127.0.0.1', self.node_port), '/node/a', headers, self.tls)
            require(replacement.status == 101, 'replacement main session rejected')
            replacement.send(json.dumps({'type': 'heartbeat', 'protocol': 1, 'backend': 'firecracker',
                'arch': 'x86_64', 'runtime': 'firecracker-v1.17.0', 'memory_mib': 512, 'slots': 2,
                'vcpus': 2, 'images': ['alpine']}))
            active_close = active.require_peer_close()
        finally:
            if replacement:
                replacement.close()
            os.kill(frozen.pid, signal.SIGCONT)
            active.close()
        wait_for(lambda: self.online('a'))
        # Confirm a real replacement agent route, not merely a stale heartbeat.
        wait_for(lambda: self.routed('a', {'op': 'status'}).get('max_memory_mib') == 512)
        terminal = WebSocket(('127.0.0.1', self.gateway_port), '/api/terminal/demo', self.headers('alice'))
        require(terminal.status == 101, 'terminal could not open before revoke')
        try:
            self.cli(self.controller, 'node-revoke', 'a')
            wait_for(lambda: not self.online('a'))
            before = self.journal_count()
            self.api({'op': 'exec', 'id': 'demo', 'command': 'true'}, ok=False)
            require(self.journal_count() == before, 'revoked node dispatched new mutation')
            revoke_close = terminal.require_peer_close()
        finally:
            terminal.close()
        ws = WebSocket(('127.0.0.1', self.node_port), '/node/a', headers, self.tls)
        require(ws.status == 401, 'revoked credential reconnected')
        ws.close()
        self.stop(self.control_process)
        self.start_controller()
        ws = WebSocket(('127.0.0.1', self.node_port), '/node/a', headers, self.tls)
        require(ws.status == 401, 'revoke lost across controller restart')
        ws.close()
        return {'old_session_closed': True, 'active_replacement_stream': active_close,
                'replacement_identity': 'synthetic authenticated main session; real guest job held open',
                'revoked_active_pty': revoke_close, 'revoke_durable': True}

    def cleanup(self):
        for p in reversed(self.processes):
            self.stop(p)
        for server in self.servers:
            server.shutdown()
            server.server_close()
        for root in self.started_workers:
            try:
                self.cli(root, 'down', timeout=30)
            except Exception as error:
                self.result.setdefault('cleanup_errors', []).append(type(error).__name__)
        self.result['host_after_cleanup'] = host_budget()
        self.save()

    def run(self):
        print('Private artifacts:', self.root, flush=True)
        try:
            self.result['host_preflight'] = host_budget(0 if self.args.prepare_only or self.args.transport_only else 3)
            if not self.args.prepare_only:
                require(os.access('/dev/kvm', os.R_OK | os.W_OK), 'KVM inaccessible')
            self.certificates()
            self.cli(self.controller, 'nodes')
            self.start_controller()
            if self.args.prepare_only:
                self.check('redirect, hostname and remote plaintext refusal', 'real client TLS/HTTP negative fixtures; no workers/guests', self.redirect_and_hostname)
                self.check('signed user gateway fixture', 'real RS256 validation; local issuer-routing fixture; no workers/guests', self.jwt_fixture)
                self.result['state'] = 'preparation-passed'
                return 0
            for root in self.workers.values():
                self.cli(root, 'up')
                self.started_workers.append(root)
            self.check('worker caps and namespace inheritance', 'real rootless workers; no guests', self.worker_caps)
            self.check('enrollment, token lengths, CA trust and replay', 'real TLS/agents; no guests', self.enrollment_tests)
            self.check('redirect, hostname and remote plaintext refusal', 'real client TLS/HTTP negative fixtures', self.redirect_and_hostname)
            if not self.args.transport_only:
                self.check('signed user gateway fixture', 'real RS256 validation; local issuer-routing fixture', self.jwt_fixture)
                self.check('fixed-node exec/files/cold persistence/snapshot/fork/hibernate/restore', 'real routed Firecracker VMs', self.vm_operations)
                self.check('ownership, same names and capacity before side effects', 'signed user fixtures + real transport/VM', self.ownership_and_capacity)
                self.check('routed guest PTY', 'real gateway WebSocket/node job/guest PTY', self.terminal)
                self.check('start/restore rejection reservation correctness', 'real worker/guest + disposable artifact faults', self.failure_reservations)
                self.check('browser reload and dialog reopen saved-key reuse', 'actual Chromium UI + signed HTTP gateway + real guest; synthetic browser edge and lost reply', self.browser_reload)
                self.check('offline/reconnect/controller and gateway restart', 'real transport/process faults/guest', self.recovery)
                self.check('lost gateway exec response and no replay', 'real guest effect + gateway kill; completed agent journal before retry', self.lost_response)
                self.check('lost gateway create response retains one guest', 'real guest create + gateway kill; completed agent journal before retry', lambda: self.lost_response(create=True))
                self.check('duplicate-session fencing and durable revoke/PTY close', 'real node TLS/WebSocket/guest PTY', self.fencing_and_revocation)
            self.result['state'] = 'passed'
        except Exception as error:
            self.result['state'] = 'failed'
            self.result['failure'] = type(error).__name__ + ': ' + str(error)
            print('Verification failed:', type(error).__name__, str(error), flush=True)
        finally:
            self.cleanup()
        return 0 if self.result['state'] == 'passed' and not self.result.get('cleanup_errors') else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--assets', type=Path, default=REPO / 'data/runtime-spike')
    parser.add_argument('--transport-only', action='store_true')
    parser.add_argument('--prepare-only', action='store_true', help='validate TLS/auth fixtures without starting workers or guests')
    parser.add_argument('--vm-turn-released', action='store_true', help='set only after planner exclusive VM release or confirmed core cleanup')
    parser.add_argument('--source-manifest', type=Path, help='pre/post-build verified product file hashes to retain with results')
    args = parser.parse_args()
    require(args.binary.is_file(), 'build ow in separate verifier target first')
    require(args.prepare_only or args.transport_only or args.vm_turn_released,
            'full acceptance requires the planner/core exclusive VM turn; use --prepare-only meanwhile')
    # Prevent concurrent invocations of this harness. Other builders must honor report reservations.
    with open(REPO / 'data/multinode-verifier.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return Harness(args).run()


if __name__ == '__main__':
    raise SystemExit(main())
