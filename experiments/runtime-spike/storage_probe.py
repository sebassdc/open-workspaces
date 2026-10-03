#!/usr/bin/env python3
"""Verify reflink support and independent writes; this is NOT a VM fork test."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import tempfile


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def probe(directory):
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='reflink-', dir=directory) as temporary:
        root = Path(temporary)
        parent, child = root / 'parent', root / 'child'
        # Real allocated bytes: a sparse file would give misleading evidence.
        with parent.open('wb') as stream:
            for _ in range(16):
                stream.write(os.urandom(1024 * 1024))
            stream.flush()
            os.fsync(stream.fileno())
        initial = digest(parent)
        with parent.open('rb') as source, child.open('xb') as target:
            fcntl.ioctl(target.fileno(), 0x40049409, source.fileno())  # FICLONE
            os.fsync(target.fileno())
        equal_after_clone = digest(child) == initial
        with child.open('r+b') as stream:
            stream.write(b'child-only' * 4096)
            stream.flush()
            os.fsync(stream.fileno())
        parent_unchanged = digest(parent) == initial
        child_changed = digest(child) != initial
        child_hash = digest(child)
        with parent.open('r+b') as stream:
            stream.seek(1024 * 1024)
            stream.write(b'parent-only' * 4096)
            stream.flush()
            os.fsync(stream.fileno())
        child_unchanged = digest(child) == child_hash
        passed = all((equal_after_clone, parent_unchanged, child_changed, child_unchanged))
        return {
            'test': 'host-file reflink write independence',
            'size_bytes': 16 * 1024 * 1024,
            'equal_after_clone': equal_after_clone,
            'parent_unchanged_after_child_write': parent_unchanged,
            'child_changed': child_changed,
            'child_unchanged_after_parent_write': child_unchanged,
            'passed': passed,
            'vm_fork_tested': False,
        }


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data-dir', type=Path, default=Path('data/runtime-spike/storage'))
    args = parser.parse_args()
    try:
        report = probe(args.data_dir)
    except OSError as error:
        report = {'passed': False, 'error': error.strerror, 'errno': error.errno}
    print(json.dumps(report, indent=2))
    raise SystemExit(0 if report['passed'] else 1)
