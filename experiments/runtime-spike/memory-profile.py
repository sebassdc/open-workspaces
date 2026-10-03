#!/usr/bin/env python3
"""Observe clean versus dirty clone memory with eight real VMs and a 64 MiB heap."""
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
directory = Path(tempfile.mkdtemp(prefix='memory-', dir=ROOT / 'data'))
binary = ROOT / 'target/release/ow'
report = {'passed': False, 'timestamp_utc': datetime.now(timezone.utc).isoformat(),
          'vm_count': 8, 'guest_memory_mib': 256, 'application_heap_mib': 64,
          'sample_count': 1, 'host_cache': 'warm', 'measure': 'VMM process PSS; not total host memory'}

def cli(*args):
    result = subprocess.run([str(binary), '--data-dir', str(directory), *args],
                            capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise RuntimeError(f'{args}: {result.stderr}')
    return result.stdout

def request(url):
    with urllib.request.urlopen(url, timeout=10) as response:
        assert response.status == 200
        assert response.read().startswith(b'open-workspaces guest HTTP ready\n')

try:
    cli('up')
    cli('create', 'parent')
    cli('exec', 'parent', '--', '( /http-fixture --memory-mib 64 >/run/http.log 2>&1 </dev/null & )')
    parent_url = cli('publish', 'parent').strip()
    request(parent_url)
    cli('snapshot', 'parent', 'prepared')
    urls = [parent_url]
    for index in range(1, 8):
        cli('fork', 'parent', f'child{index}', '--snapshot', 'prepared')
        url = cli('publish', f'child{index}').strip()
        request(url)  # Fault in/read all heap pages, keeping them clean.
        urls.append(url)
    report['clean'] = json.loads(cli('stats'))
    for url in urls:
        request(url + '/dirty')  # Touch one byte of every 4 KiB application heap page.
    report['dirty'] = json.loads(cli('stats'))
    assert report['dirty']['total_pss_kib'] > report['clean']['total_pss_kib']
    assert report['clean']['reserved_memory_mib'] == report['dirty']['reserved_memory_mib'] == 2048
    report['passed'] = True
except Exception as error:
    report['error'] = str(error)
    raise
finally:
    cli('down')
    (directory / 'memory-result.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    print(f'Local results: {directory}')
