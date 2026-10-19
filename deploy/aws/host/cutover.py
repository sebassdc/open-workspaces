#!/usr/bin/env python3
"""Private operator checklist, not a controller lock or an automatic failover service.

Receipts attest external fencing and live checks. This tool never starts a writer,
transfers trust, changes endpoints or turns inventory into catalog ownership.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys

STAGES = ('planned', 'frozen', 'isolated', 'catalogued', 'restored',
          'write_possible', 'reconciled', 'open')
REQUIRED = {
    'frozen': ('old_stopped', 'replacement_stopped', 'maintenance_disabled', 'out_of_band_writers_stopped'),
    'isolated': ('old_autostart_disabled', 'external_fence_verified'),
    'catalogued': ('all_ownership_and_roots_resolved',),
    'restored': ('private_trust_and_config_verified', 'same_runner_credentials', 'reader_compatible'),
    'write_possible': ('old_fence_rechecked', 'replacement_still_stopped'),
    'reconciled': ('credentials_reconnected_to_replacement', 'stale_sessions_rejected',
                   'stale_operations_rejected', 'all_ownership_and_roots_resolved'),
    'open': ('worker_and_guest_identity_unchanged', 'strict_host_key_ssh',
             'preview_access_and_revocation', 'rollback_before_write_checked',
             'rollback_latest_catalog_checked'),
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def private(path):
    stat = path.lstat()
    require(path.is_file() and not path.is_symlink() and stat.st_uid == os.geteuid()
            and stat.st_mode & 0o077 == 0, 'receipt/journal must be an owned private regular file')
    value = json.loads(path.read_text())
    require(isinstance(value, dict), 'receipt/journal must be a JSON object')
    return value


def persist(path, state):
    temporary = path.with_name(path.name + '.new')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(state, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def catalog(ow, path):
    result = subprocess.run([ow, 'backup', 'catalog-check', path], check=True,
                            capture_output=True, text=True)
    return json.loads(result.stdout)


def advance(state, receipt, ow):
    index = STAGES.index(state['stage']) + 1
    require(index < len(STAGES), 'checklist already complete')
    stage = STAGES[index]
    require(receipt.get('stage') == stage, f'next stage is {stage}')
    require(receipt.get('drill') == state['drill'], 'receipt belongs to another cutover')
    require(receipt.get('evidence'), 'name the private evidence for this receipt')
    for field in REQUIRED[stage]:
        require(receipt.get(field) is True, f'{stage} requires {field}')
    if stage == 'isolated':
        require(receipt.get('fence_method') in ('provider-control-plane-isolation', 'service-and-network-isolation'),
                'host-local flock, heartbeat loss and checklist receipts are not cross-host fences')
        require(receipt.get('fence_reference'), 'record how the external fence is independently verified')
    if stage == 'catalogued':
        require(receipt.get('backup_gap') == 'none' or
                (receipt.get('backup_gap') == 'resolved' and receipt.get('gap_resolution')
                 and receipt.get('resolution_includes_parked_roots') is True),
                'stale backup: reconstruct ownership and every root across the gap, including parked boxes')
        state = {**state, 'catalog': catalog(ow, receipt['catalog'])}
    if stage == 'restored':
        require(catalog(ow, receipt['catalog']) == state['catalog'],
                'restored catalog differs; preserve epochs/high-watermarks and all roots')
    if stage == 'reconciled':
        for field in ('unknown_running_copies', 'contradictory_running_copies', 'unresolved_operations'):
            require(type(receipt.get(field)) is int and receipt[field] == 0, f'blocked: {field}')
    return {**state, 'stage': stage, 'receipts': [*state['receipts'], receipt]}


def rollback(state, receipt, ow):
    require(receipt.get('drill') == state['drill'], 'receipt belongs to another cutover')
    require(receipt.get('old_stopped') is True and receipt.get('replacement_stopped') is True
            and receipt.get('maintenance_disabled') is True and receipt.get('external_fence_verified') is True,
            'rollback requires both writers stopped and externally fenced')
    require(receipt.get('evidence'), 'record fresh stop/fence and final-backup evidence')
    current = catalog(ow, receipt['latest_catalog'])
    restored = catalog(ow, receipt['restore_catalog'])
    require(current == restored, 'rollback restore differs from latest stopped-authority catalog')
    if STAGES.index(state['stage']) >= STAGES.index('write_possible'):
        require(receipt.get('source') == 'replacement' and receipt.get('taken_after_both_stopped') is True,
                'possible new writes: take latest replacement catalog after stopping BOTH writers')
        mode = 'latest-replacement-only'
    else:
        require(current == state.get('catalog'), 'pre-write rollback requires the final catalog')
        mode = 'final-pre-write-catalog'
    # A successful check is not a completed rollback. Remain frozen until a new
    # cutover checklist is created from this latest catalog and a new fence receipt.
    return {'rollback': mode, 'action': 'keep both stopped; restore checked catalog, reconcile before reopening'}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('init', 'status', 'advance', 'rollback-check'))
    parser.add_argument('journal', type=Path)
    parser.add_argument('--drill', help='unique dedicated-fixture/cutover identifier')
    parser.add_argument('--receipt', type=Path)
    parser.add_argument('--ow', default='ow')
    args = parser.parse_args(argv)
    path = args.journal.absolute()
    try:
        info = path.parent.stat()
        require(info.st_uid == os.geteuid() and info.st_mode & 0o077 == 0, 'use an owned mode-0700 journal directory')
        # Only serialize this private checklist. This lock does NOT fence controllers.
        fd = os.open(str(path) + '.lock', os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if args.command == 'init':
                require(args.drill and not path.exists(), 'use a unique drill and a new journal')
                state = {'schema': 1, 'drill': args.drill, 'stage': 'planned', 'receipts': []}
                persist(path, state)
            else:
                state = private(path)
                require(state.get('schema') == 1 and state.get('stage') in STAGES, 'invalid checklist')
                if args.command == 'advance':
                    require(args.receipt, '--receipt required')
                    state = advance(state, private(args.receipt), args.ow)
                    persist(path, state)
                elif args.command == 'rollback-check':
                    require(args.receipt, '--receipt required')
                    print(json.dumps(rollback(state, private(args.receipt), args.ow)))
                    return 0
            index = STAGES.index(state['stage'])
            print(json.dumps({'stage': state['stage'], 'next': STAGES[index+1] if index+1 < len(STAGES) else None,
                              'writes_possible': index >= STAGES.index('write_possible'),
                              'cross_host_fence': 'external operator evidence required'}))
            return 0
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(f'blocked: {error}', file=sys.stderr)
        return 3


if __name__ == '__main__':
    sys.exit(main())
