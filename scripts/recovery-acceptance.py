#!/usr/bin/env python3
"""Opt-in real-VM/store acceptance using two already started, empty test workers.

Creates only its own disposable box; leaves it stopped on both runners for inspection.
Never starts/stops hosts, provisions infrastructure or changes controller placement.
"""
import argparse
import json
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ow', required=True, help='Reviewed Linux ow binary')
    parser.add_argument('--source', required=True, type=Path)
    parser.add_argument('--destination', required=True, type=Path)
    parser.add_argument('--store', required=True, help='Authorized private snapshot prefix (S3 requests incur cost)')
    parser.add_argument('--mode', choices=('cold', 'ram'), required=True)
    args = parser.parse_args()
    source, destination = args.source.resolve(), args.destination.resolve()
    if source == destination:
        parser.error('two different empty test workers are required')
    binary = str(Path(args.ow).resolve())

    def call(root, *command, structured=True):
        result = subprocess.run([binary, '--local', '--data-dir', str(root), *command],
                                check=True, capture_output=True, text=True, timeout=3600)
        return json.loads(result.stdout) if structured else result.stdout

    for root in (source, destination):
        if not (root / 'control.sock').exists() or call(root, 'list'):
            parser.error('each worker must already be running with an empty workspace list')
        if not call(root, 'status').get('recovery_points_v1'):
            parser.error('both test workers must support recovery points')
    token = secrets.token_hex(12)
    box = 'rtest-' + token
    started = time.monotonic()
    try:
        call(source, 'create', box, '--memory', '256', '--disk', '1GiB')
        marker = '/root/recovery-marker'
        command = (f'printf %s {shlex.quote(token)} > {marker}; sync; '
                   f'nohup env OW_RECOVERY_TOKEN={token} sleep 86400 </dev/null >/dev/null 2>&1 & echo $!')
        pid = int(call(source, 'exec', box, '--', command, structured=False).strip())
        # The existing host-local backup command transfers the running capture.
        call(source, 'backup', 'recover', '--from', str(source), '--to', args.store, structured=False)
        durability = call(source, 'inspect', box)['durability']
        if not durability.get('last_durable_at') or durability.get('error'):
            raise RuntimeError('capture did not become durable')
        # Fence this disposable source before starting its captured identity elsewhere.
        call(source, 'stop', box)
        restore = ['backup', 'restore', durability['snapshot'], '--from', args.store, '--to', str(destination)]
        if args.mode == 'cold':
            restore.append('--cold')
        call(destination, *restore)
        actual = call(destination, 'exec', box, '--', f'cat {marker}', structured=False).strip()
        if actual != token:
            raise RuntimeError('restored disk marker differs')
        if args.mode == 'ram':
            command = f"tr '\\0' '\\n' < /proc/{pid}/environ | grep -Fx OW_RECOVERY_TOKEN={token}"
            call(destination, 'exec', box, '--', command, structured=False)
        call(destination, 'stop', box)
    finally:
        # Only this invocation's random test ID may be stopped, including failed imports.
        for root in (source, destination):
            try:
                if any(item['id'] == box for item in call(root, 'list')):
                    call(root, 'stop', box)
            except (subprocess.SubprocessError, OSError):
                print(f'Inspect own test box {box} on {root}: cleanup stop failed', file=sys.stderr)
    print(json.dumps({'box': box, 'mode': args.mode, 'disk_marker': 'matched',
                      'ram_process': 'continued' if args.mode == 'ram' else 'not claimed',
                      'capture_to_durable_seconds': durability['uploaded_at'] - durability['last_durable_at'],
                      'total_seconds': round(time.monotonic() - started, 3),
                      'cleanup': 'Own test boxes left stopped; delete deliberately after inspection.'}))


if __name__ == '__main__':
    main()
