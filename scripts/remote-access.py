#!/usr/bin/env python3
"""Start/stop only this project's authenticated gateway and named tunnel.

Requires private configs created by cloudflare-access.py, an existing DNS route,
and a running workspace worker. No DNS or Cloudflare policy writes.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT / 'data/remote-access'
SERVICES = {
    'gateway': [str(ROOT / 'target/release/ow'), '--data-dir', str(ROOT / 'data/prototype'), 'dashboard', '--config', str(PRIVATE / 'gateway.json'), '--listen', '127.0.0.1:8787'],
    'tunnel': [str(PRIVATE / 'bin/cloudflared'), 'tunnel', '--config', str(PRIVATE / 'tunnel.json'), '--no-autoupdate', 'run'],
}


def process_ticks(pid):
    # /proc comm is enclosed in parentheses and can contain spaces.
    return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19]


def managed(name):
    descriptor = PRIVATE / f'{name}-process.json'
    if not descriptor.exists(): return None
    record = json.loads(descriptor.read_text())
    try:
        proc = Path(f"/proc/{record['pid']}")
        argv = (proc / 'cmdline').read_bytes().split(b'\0')
        expected = SERVICES[name]
        if proc.stat().st_uid != os.getuid() or process_ticks(record['pid']) != record['start_ticks']:
            raise RuntimeError(f'{name} descriptor does not match the owned process; refusing to signal it')
        if (proc / 'stat').read_text().rsplit(')', 1)[1].split()[0] == 'Z':
            return None  # An exited owned process can remain as a zombie under its supervisor.
        if argv[:len(expected)] != [arg.encode() for arg in expected]:
            # The initial gateway was launched through ./ow, which execs this binary.
            raise RuntimeError(f'{name} descriptor does not match the owned process; refusing to signal it')
        return record
    except FileNotFoundError: return None


def start(name):
    if managed(name):
        print(f'{name} already running')
        return
    args = SERVICES[name].copy()
    if name == 'tunnel':
        args.append(json.loads((PRIVATE / 'created-tunnel.json').read_text())['id'])
    with open(PRIVATE / f'{name}.log', 'ab', buffering=0) as log:
        child = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    record = {'pid': child.pid, 'start_ticks': process_ticks(child.pid)}
    (PRIVATE / f'{name}-process.json').write_text(json.dumps(record) + '\n')
    hostname = json.loads((PRIVATE / 'gateway.json').read_text())['hostname']
    for _ in range(200):
        if child.poll() is not None:
            raise RuntimeError(f'{name} exited; inspect its private log')
        if name == 'gateway':
            try:
                request = urllib.request.Request('http://127.0.0.1:8787/', headers={'Host': hostname})
                urllib.request.urlopen(request, timeout=1)
            except urllib.error.HTTPError as error:
                if error.code == 401:
                    print('gateway started; anonymous origin request denied')
                    return
            except (urllib.error.URLError, TimeoutError): pass
        else:
            pidfile = Path(f"/proc/{child.pid}/fd")
            if pidfile.exists():
                time.sleep(1)
                if child.poll() is None:
                    print('tunnel started; check status/public URL for connectivity')
                    return
        time.sleep(.1)
    raise RuntimeError(f'{name} readiness timed out; inspect its private log')


def stop(name):
    record = managed(name)
    if not record:
        print(f'{name} is stopped')
        return
    os.kill(record['pid'], signal.SIGTERM)
    deadline = time.monotonic() + 15
    while True:
        try:
            proc = Path(f"/proc/{record['pid']}")
            if proc.stat().st_uid != os.getuid() or process_ticks(record['pid']) != record['start_ticks']:
                break  # The originally verified process exited; never signal a reused PID.
            if (proc / 'stat').read_text().rsplit(')', 1)[1].split()[0] == 'Z':
                break
        except FileNotFoundError:
            break
        if time.monotonic() > deadline:
            raise RuntimeError(f'{name} shutdown has not completed; refusing a duplicate start')
        time.sleep(.1)
    print(f'{name} stopped; workspace worker and disks are retained')


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['start', 'stop', 'status'])
    args = parser.parse_args()
    if args.action == 'start':
        start('gateway')
        start('tunnel')
    elif args.action == 'stop':
        stop('tunnel')
        stop('gateway')
    else:
        for name in SERVICES: print(name + ': ' + ('running' if managed(name) else 'stopped'))


if __name__ == '__main__':
    try: main()
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print('Remote access:', error, file=sys.stderr)
        sys.exit(1)
