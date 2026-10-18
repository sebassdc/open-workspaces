#!/usr/bin/env python3
"""Credential-free AWS CLI fixture: exercises the real SnapshotStore subprocesses."""
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
root = Path(os.environ['OW_TEST_STORE'])

def option(name):
    return args[args.index(name) + 1]

def missing():
    sys.stderr.write('An error occurred (404) when calling HeadObject: Not Found\n')
    sys.exit(1)

def path(url):
    assert url.startswith('s3://')
    return root / url[5:]

with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')

if args[:2] == ['s3api', 'get-bucket-versioning']:
    print(json.dumps({'Status': 'Suspended' if (root / 'no-versioning').exists() else 'Enabled'}))
elif args[:2] == ['s3api', 'get-bucket-lifecycle-configuration']:
    print(json.dumps({'Rules': [{'Status': 'Enabled', 'NoncurrentVersionExpiration': {'NoncurrentDays': 7}, **({'Expiration': {'Days': 30}} if (root / 'unsafe-lifecycle').exists() else {})}]}))
elif args[:2] == ['s3api', 'head-object']:
    dest = root / option('--bucket') / option('--key')
    if (root / 'deny-head').exists():
        sys.stderr.write('An error occurred (AccessDenied)\n')
        sys.exit(1)
    if not dest.exists():
        missing()
    print('{}')
elif args[:2] == ['s3api', 'put-object']:
    assert option('--if-none-match') == '*'
    dest = root / option('--bucket') / option('--key')
    if (root / 'fail-manifest').exists() and dest.name == 'manifest.json':
        sys.stderr.write('An error occurred (AccessDenied)\n')
        sys.exit(1)
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        with dest.open('xb') as output:
            output.write(Path(option('--body')).read_bytes())
    except FileExistsError:
        sys.stderr.write('An error occurred (PreconditionFailed)\n')
        sys.exit(1)
    print('{}')
elif args[:2] == ['s3', 'cp']:
    source, dest = args[-2:]
    assert dest == '-'
    source = path(source)
    if not source.exists():
        missing()
    sys.stdout.buffer.write(source.read_bytes())
elif args[:2] == ['s3', 'ls']:
    prefix = args[-1]
    location = path(prefix)
    bucket = root / prefix[5:].split('/')[0]
    if location.exists():
        for file in sorted(location.rglob('*')):
            if file.is_file():
                print(f'2026-10-18 00:00:00 {file.stat().st_size} {file.relative_to(bucket)}')
else:
    raise AssertionError(args)
