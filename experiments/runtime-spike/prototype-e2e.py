#!/usr/bin/env python3
"""Real-VM regression test using only a newly allocated project data directory."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / 'target/release/ow'
DATA = ROOT / 'data'


def run():
    directory = Path(tempfile.mkdtemp(prefix='e2e-', dir=DATA))
    checks = []
    timings = {}
    report = {'passed': False, 'checks': checks, 'timings_ms': timings}
    print(f'Local test artifacts: {directory}', flush=True)

    def cli(*args, code=0):
        result = subprocess.run([str(BINARY), '--local', '--data-dir', str(directory), *args],
                                capture_output=True, text=True, timeout=120)
        if result.returncode != code:
            raise AssertionError(f'{args}: exit {result.returncode}, expected {code}\n{result.stdout}\n{result.stderr}')
        return result.stdout

    def value(*args):
        return json.loads(cli(*args))

    def http(url, expected):
        with urllib.request.urlopen(url, timeout=10) as response:
            assert response.status == 200
            assert response.read().startswith(b'open-workspaces guest HTTP ready\n')
            assert int(response.headers['X-Counter']) == expected

    try:
        assert value('up')['running'] == 0
        started = time.monotonic()
        first = value('create', 'dev')
        timings['create_to_exec'] = (time.monotonic() - started) * 1000
        assert first == value('create', 'dev')
        assert value('status')['running'] == 1
        assert 'prototype-ready' in cli('exec', 'dev', '--', 'echo prototype-ready')
        cli('exec', 'dev', '--', 'exit 7', code=7)
        checks.extend(['create idempotency', 'real guest exec and exit status'])
        checks.append('interactive shell is covered by terminal-cli-test.py')

        payload = os.urandom(48 * 1024)
        upload = directory / 'upload.bin'
        upload.write_bytes(payload)
        cli('put', 'dev', str(upload), '/persist/payload')
        downloaded = directory / 'download.bin'
        cli('get', 'dev', '/persist/payload', str(downloaded))
        assert downloaded.read_bytes() == payload
        cli('get', 'dev', '/persist/payload', str(downloaded), code=1)
        checks.append('48 KiB binary file roundtrip; local overwrite rejected')

        cli('exec', 'dev', '--', '( /http-fixture >/run/http.log 2>&1 </dev/null & )')
        url = cli('publish', 'dev').strip()
        assert cli('publish', 'dev').strip() == url
        http(url, 1)
        checks.append('loopback HTTP publishing and publish idempotency')
        started = time.monotonic()
        snapshot = value('snapshot', 'dev', 'ready')
        timings['snapshot_with_integrity_hashes'] = (time.monotonic() - started) * 1000
        assert snapshot == value('snapshot', 'dev', 'ready')
        http(url, 2)
        started = time.monotonic()
        child = value('fork', 'dev', 'branch', '--snapshot', 'ready')
        timings['fork_from_verified_snapshot'] = (time.monotonic() - started) * 1000
        assert child == value('fork', 'dev', 'branch', '--snapshot', 'ready')
        child_url = cli('publish', 'branch').strip()
        http(child_url, 2)  # Snapshot had counter 1; parent has since advanced.
        cli('exec', 'dev', '--', 'echo parent > /persist/branch; sync')
        cli('exec', 'branch', '--', 'echo child > /persist/branch; sync')
        assert 'parent' in cli('exec', 'dev', '--', 'cat /persist/branch')
        assert 'child' in cli('exec', 'branch', '--', 'cat /persist/branch')
        assert cli('exec', 'dev', '--', 'cat /etc/machine-id') != cli('exec', 'branch', '--', 'cat /etc/machine-id')
        checks.extend(['immutable paired snapshot and snapshot idempotency', 'fork idempotency',
                       'HTTP process memory restored', 'independent guest disk writes and machine identities'])
        cli('exec', 'dev', '--', 'echo 1 > /proc/sys/net/ipv4/ip_forward; ip route replace 198.18.0.4/30 via 198.18.0.1; ping -c 1 -W 1 198.18.0.6', code=1)
        checks.append('guest root enabling its own forwarding cannot reach peer VM through worker')

        value('hibernate', 'branch')
        assert value('inspect', 'branch')['state'] == 'hibernated'
        assert value('status')['running'] == 1
        time.sleep(1.1)
        value('start', 'branch')
        http(child_url, 3)
        guest_time = int(cli('exec', 'branch', '--', 'date +%s').strip())
        assert abs(guest_time - time.time()) < 3
        checks.append('hibernate releases VMM; resume preserves HTTP counter')
        checks.append('resume advances guest wall clock to current time')

        value('stop', 'dev')
        value('stop', 'dev')
        value('start', 'dev')
        assert 'parent' in cli('exec', 'dev', '--', 'cat /persist/branch')
        cli('get', 'dev', '/persist/payload', str(directory / 'after-restart.bin'))
        assert (directory / 'after-restart.bin').read_bytes() == payload
        checks.append('cold restart preserves binary files and parent disk')

        value('restore', 'dev', 'ready')
        http(url, 2)
        cli('exec', 'dev', '--', 'test ! -e /persist/branch')
        checks.append('restore rewinds paired RAM and disk')

        value('snapshot', 'dev', 'corrupt')
        state_file = directory / 'snapshots/corrupt/state'
        state_file.chmod(0o600)  # Deliberately tamper with this test's own immutable artifact.
        with state_file.open('r+b') as stream:
            byte = stream.read(1)
            stream.seek(0)
            stream.write(bytes([byte[0] ^ 0xff]))
        cli('fork', 'dev', 'rejected', '--snapshot', 'corrupt', code=1)
        assert all(workspace['id'] != 'rejected' for workspace in value('list'))
        checks.append('corrupt snapshot rejected before child creation')

        cli('create', '../outside', code=1)
        cli('create', 'bad-size', '--memory', '999', code=1)
        checks.append('unsafe workspace names and unsupported resource sizes rejected')

        cli('exec', 'dev', '--', 'echo recovered > /persist/recovery; sync')
        pid = value('status')['worker_pid']
        assert Path(f'/proc/{pid}/exe').resolve() == BINARY.resolve()
        os.kill(pid, signal.SIGKILL)  # Only this test's authenticated worker PID.
        time.sleep(0.2)
        assert value('up')['running'] == 0
        assert value('inspect', 'dev')['state'] == 'stopped'
        value('start', 'dev')
        assert 'recovered' in cli('exec', 'dev', '--', 'cat /persist/recovery')
        assert len(value('list')) == 2
        checks.append('abrupt worker loss: reconcile stopped, recover disk, no duplicate workspace')

        cli('down')
        assert value('up')['running'] == 0
        value('start', 'branch')
        assert 'child' in cli('exec', 'branch', '--', 'cat /persist/branch')
        checks.append('graceful worker restart preserves registry and disks')
        value('start', 'dev')
        for index in range(3):
            value('create', f'large{index}', '--memory', '1024')
        value('create', 'last-slot', '--memory', '512')
        cli('create', 'over-capacity', code=1)
        assert all(workspace['id'] != 'over-capacity' for workspace in value('list'))
        checks.append('4 GiB memory admission limit rejects excess without creating workspace')
        stats = value('stats')
        assert stats['reserved_memory_mib'] == 4096
        assert stats['total_pss_kib'] > 0
        checks.append('live memory statistics distinguish reservations from proportional resident memory')
        report['passed'] = True
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        try:
            cli('down')
        except Exception as error:
            report['cleanup_error'] = str(error)
        (directory / 'e2e-result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    run()
