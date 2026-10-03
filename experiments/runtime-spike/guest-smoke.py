#!/usr/bin/env python3
"""Disposable VM smoke experiment. Run inside a fresh user/network namespace."""
import base64
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import subprocess
import tempfile
import termios
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / 'data/runtime-spike'
BINARY = DATA / 'official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64'
KERNEL = DATA / 'downloads/vmlinux-6.1.186'


def clone(source, destination):
    with source.open('rb') as src, destination.open('xb') as dst:
        fcntl.ioctl(dst.fileno(), 0x40049409, src.fileno())
        os.fsync(dst.fileno())


class VM:
    def __init__(self, root, name, index, disk):
        self.path = root / name
        self.path.mkdir()
        clone(disk, self.path / 'disk.ext4')
        self.tap = f'ow{index}'
        self.address = f'192.0.2.{index * 4 + 2}'
        subprocess.run(['ip', 'tuntap', 'add', 'dev', self.tap, 'mode', 'tap'], check=True)
        subprocess.run(['ip', 'addr', 'add', f'192.0.2.{index * 4 + 1}/30', 'dev', self.tap], check=True)
        self.socket = self.path / 'api.sock'
        self.master, slave = pty.openpty()
        attributes = termios.tcgetattr(slave)
        attributes[3] &= ~termios.ECHO
        termios.tcsetattr(slave, termios.TCSANOW, attributes)
        self.log = (self.path / 'serial.log').open('ab', buffering=0)
        self.process = subprocess.Popen([str(BINARY), '--api-sock', str(self.socket)],
                                        cwd=self.path, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        self.buffer = b''
        deadline = time.monotonic() + 10
        while not self.socket.exists():
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(f'{name}: VMM failed to create socket')
            time.sleep(0.01)

    def api(self, method, endpoint, value):
        result = subprocess.run(['curl', '-sS', '--max-time', '30', '--unix-socket', str(self.socket),
                                 '-X', method, '-H', 'Content-Type: application/json',
                                 '--data', json.dumps(value), '-w', '\n%{http_code}',
                                 'http://localhost' + endpoint], capture_output=True, text=True, check=True)
        body, code = result.stdout.rsplit('\n', 1)
        if not 200 <= int(code) < 300:
            raise RuntimeError(f'{endpoint}: HTTP {code}: {body}')

    def expect(self, pattern, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            match = re.search(pattern, self.buffer)
            if match:
                output = self.buffer[:match.end()]
                self.buffer = self.buffer[match.end():]
                return output.decode(errors='replace'), match
            if select.select([self.master], [], [], 0.1)[0]:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError as error:
                    raise RuntimeError(f'Guest serial closed: {self.path.name}') from error
                self.log.write(chunk)
                self.buffer += chunk
        raise TimeoutError(f'{self.path.name}: missing serial marker; inspect {self.path / "serial.log"}')

    def command(self, command, expected_exit=0):
        token = ('OW_DONE_' + uuid.uuid4().hex).encode()
        payload = command + '; ow_rc=$?; printf "\\n' + token.decode() + ':%s\\n" "$ow_rc"\n'
        os.write(self.master, payload.encode())
        output, match = self.expect(token + rb':(\d+)\r*\n')
        actual = int(match.group(1))
        if actual != expected_exit:
            raise RuntimeError(f'{self.path.name}: exit {actual}, expected {expected_exit}: {output}')
        return output

    def boot(self):
        self.api('PUT', '/machine-config', {'vcpu_count': 1, 'mem_size_mib': 256})
        self.api('PUT', '/boot-source', {'kernel_image_path': str(KERNEL),
            'boot_args': 'console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init'})
        # Relative disk path resolves in each VMM's own cwd, including snapshot restore.
        self.api('PUT', '/drives/rootfs', {'drive_id': 'rootfs', 'path_on_host': 'disk.ext4',
                                          'is_root_device': True, 'is_read_only': False})
        self.api('PUT', '/network-interfaces/net1', {'iface_id': 'net1', 'host_dev_name': self.tap,
                                                   'guest_mac': '06:00:00:00:00:01'})
        self.api('PUT', '/actions', {'action_type': 'InstanceStart'})
        self.expect(rb'OW_GUEST_READY\r*\n')
        self.command('true')

    def network(self):
        # Keep host tap down until the clone has a unique address and test identity.
        self.command(f'ip addr flush dev eth0; ip addr add {self.address}/30 dev eth0; '
                     f'ip link set eth0 up; hostname {self.path.name}; '
                     'rm -f /etc/machine-id; cat /proc/sys/kernel/random/uuid > /etc/machine-id')
        subprocess.run(['ip', 'link', 'set', self.tap, 'up'], check=True)

    def http(self, expected_counter):
        result = subprocess.run(['curl', '-fsS', '-i', '--max-time', '5', f'http://{self.address}:8080'],
                                capture_output=True, text=True, check=True)
        if 'open-workspaces guest HTTP ready\n' not in result.stdout:
            raise RuntimeError('Wrong HTTP response')
        if f'X-Counter: {expected_counter}\n' not in result.stdout:
            raise RuntimeError(f'Wrong in-memory HTTP counter: {result.stdout}')

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        os.close(self.master)
        self.log.close()


def memory_usage(vm):
    fields = {}
    for line in Path(f'/proc/{vm.process.pid}/smaps_rollup').read_text().splitlines():
        if ':' in line:
            key, value = line.split(':', 1)
            if value.strip().endswith('kB'):
                fields[key] = int(value.strip().split()[0])
    return {key: fields[key] for key in ('Rss', 'Pss', 'Private_Dirty')}


def capture(vm, destination):
    started = time.monotonic()
    vm.command('sync')
    vm.api('PATCH', '/vm', {'state': 'Paused'})
    destination.mkdir()
    try:
        vm.api('PUT', '/snapshot/create', {'snapshot_type': 'Full',
            'snapshot_path': str(destination / 'state'), 'mem_file_path': str(destination / 'memory')})
        clone(vm.path / 'disk.ext4', destination / 'disk.ext4')
    finally:
        vm.api('PATCH', '/vm', {'state': 'Resumed'})
    return (time.monotonic() - started) * 1000


def restore(vm, snapshot):
    vm.api('PUT', '/snapshot/load', {'snapshot_path': str(snapshot / 'state'),
        'mem_backend': {'backend_path': str(snapshot / 'memory'), 'backend_type': 'File'},
        'network_overrides': [{'iface_id': 'net1', 'host_dev_name': vm.tap}], 'resume_vm': False})
    vm.api('PATCH', '/vm', {'state': 'Resumed'})


def run():
    os.umask(0o077)
    # Require an isolated namespace with no pre-existing external interfaces.
    links = json.loads(subprocess.check_output(['ip', '-j', 'link', 'show']))
    if {item['ifname'] for item in links} != {'lo'}:
        raise SystemExit('Run with unshare --user --map-root-user --net; refusing host networking.')
    (DATA / 'runs').mkdir(exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix='r-', dir=DATA / 'runs'))
    print(f'Local artifacts: {root}', flush=True)
    machines = []
    report = {'passed': False, 'checks': [], 'sample_count': 1, 'benchmark': False,
              'timestamp_utc': datetime.now(timezone.utc).isoformat(), 'timings_ms': {},
              'guest_memory_mib': 256, 'guest_vcpus': 1, 'network': 'temporary isolated namespace',
              'production_clone_identity_validated': False}
    try:
        started = time.monotonic()
        parent = VM(root, 'parent', 0, DATA / 'guest/base.ext4')
        machines.append(parent)
        parent.boot()
        report['timings_ms']['cold_request_to_shell'] = (time.monotonic() - started) * 1000
        parent.network()
        parent.command('( /http-fixture > /run/http.log 2>&1 < /dev/null & )')
        parent.http(1)
        report['timings_ms']['cold_request_to_http'] = (time.monotonic() - started) * 1000
        parent.command('(exit 7)', expected_exit=7)
        payload = base64.b64encode(b'file-transfer-roundtrip\n').decode()
        parent.command(f'printf %s {payload} | base64 -d > /persist/upload; sync')
        output = parent.command('cat /persist/upload | base64')
        assert payload in output
        report['checks'] += ['guest boot and shell command', 'nonzero exit status', 'file roundtrip']
        report['checks'].append('host-to-guest HTTP response inside namespace')
        parent.command('COUNT=41; printf "MEMORY_COUNTER=%s\\n" "$COUNT"; sync')
        snapshot = root / 'checkpoint'
        report['timings_ms']['paired_capture'] = capture(parent, snapshot)
        parent.command('COUNT=100; echo parent > /persist/branch; sync')
        for index in (1, 2):
            started = time.monotonic()
            child = VM(root, f'child{index}', index, snapshot / 'disk.ext4')
            machines.append(child)
            restore(child, snapshot)
            output = child.command('printf "RESTORED_COUNTER=%s\\n" "$COUNT"; test "$COUNT" = 41')
            assert 'RESTORED_COUNTER=41' in output
            report['timings_ms'][f'child{index}_request_to_shell'] = (time.monotonic() - started) * 1000
            child.command('test ! -e /persist/branch; test "$(cat /persist/upload)" = file-transfer-roundtrip')
            child.command(f'COUNT=$((COUNT+{index})); echo child{index} > /persist/branch; sync')
            child.network()
            child.http(2)  # Existing server process resumed from RAM, no restart command.
            report['timings_ms'][f'child{index}_request_to_http'] = (time.monotonic() - started) * 1000
        parent.command('test "$COUNT" = 100; test "$(cat /persist/branch)" = parent')
        parent.http(2)
        parent.http(3)
        for index, child in enumerate(machines[1:], 1):
            child.command(f'test "$COUNT" = {41 + index}; test "$(cat /persist/branch)" = child{index}')
            child.http(3)
        report['checks'] += ['paired full RAM/disk capture', 'two concurrent restored children',
                            'running shell counter restored', 'resumed HTTP server counter',
                            'independent parent/child memory and disk writes']
        report['three_vm_memory_kib'] = {vm.path.name: memory_usage(vm) for vm in machines}
        report['disk_extent_accounting'] = subprocess.check_output(
            ['btrfs', 'filesystem', 'du', '-s', '--raw'] +
            [str(vm.path / 'disk.ext4') for vm in machines] + [str(snapshot / 'disk.ext4')], text=True)
        hibernated = root / 'hibernated'
        report['timings_ms']['hibernate_capture'] = capture(parent, hibernated)
        for vm in machines:
            vm.command('sync')
            vm.close()
        machines.clear()
        started = time.monotonic()
        wake = VM(root, 'wake', 4, hibernated / 'disk.ext4')
        machines.append(wake)
        restore(wake, hibernated)
        wake.command('test "$COUNT" = 100; test "$(cat /persist/branch)" = parent')
        wake.network()
        wake.http(4)
        report['timings_ms']['hibernate_request_to_http'] = (time.monotonic() - started) * 1000
        report['checks'].append('full-state restore after all original VMM processes exited')
        # Cold boot from the persisted parent disk in a fresh VMM.
        cold = VM(root, 'cold', 3, root / 'parent/disk.ext4')
        machines.append(cold)
        cold.boot()
        cold.command('test "$(cat /persist/branch)" = parent; test "$(cat /persist/upload)" = file-transfer-roundtrip')
        report['checks'].append('disk persistence across cold VMM restart')
        report['passed'] = True
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        for vm in machines:
            vm.close()
        (root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    run()
