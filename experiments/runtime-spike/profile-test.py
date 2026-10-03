#!/usr/bin/env python3
"""Real-VM OS regression and a small warm pilot (n=5 per userspace)."""
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

repo = Path(__file__).resolve().parents[2]
root = Path(tempfile.mkdtemp(prefix='os-', dir=repo / 'data'))
command = [str(repo / 'ow'), '--data-dir', str(root)]


def run(*args):
    result = subprocess.run(command + list(args), capture_output=True, text=True)
    if result.returncode:
        raise AssertionError(result.stderr)
    return result.stdout if args[0] == 'down' else json.loads(result.stdout)


def execute(machine, text):
    result = subprocess.run(command + ['exec',machine,'--',text],capture_output=True,text=True)
    assert result.returncode == 0, result.stderr+result.stdout
    return result.stdout


try:
    run('up')
    measurements = {}
    for image in ('alpine', 'arch', 'ubuntu'):
        samples = []
        for trial in range(5):
            name = f'{image}-{trial}'
            start = time.monotonic_ns()
            workspace = run('create', name, '--image', image)
            elapsed = (time.monotonic_ns()-start)/1e6
            assert workspace['image'] == image
            time.sleep(.25)
            pss = run('stats')['running'][name]['pss_kib']
            samples.append({'create_ms':elapsed, 'idle_pss_kib':pss})
            run('stop', name)
        def percentile(values, p):
            return sorted(values)[math.ceil(len(values)*p)-1]
        measurements[image] = {'samples':samples,'create_ms_p50':statistics.median(s['create_ms'] for s in samples),
                               'create_ms_p95':percentile([s['create_ms'] for s in samples],.95),
                               'idle_pss_kib_p50':statistics.median(s['idle_pss_kib'] for s in samples),
                               'idle_pss_kib_p95':percentile([s['idle_pss_kib'] for s in samples],.95)}
        print(image + ': 5 create/idle-PSS trials completed', flush=True)
    for image in ('arch', 'ubuntu'):
        name = image+'-0'
        run('start', name)
        assert f'ID={image}' in execute(name,'cat /etc/os-release')
        execute(name,'pacman --version' if image=='arch' else 'apt --version')
        # Transfer the test fixture in bounded upload chunks; no runtime upload-limit bypass.
        fixture=(repo/'data/runtime-spike/profiles/http-fixture').read_bytes()
        paths=[]
        for i, offset in enumerate(range(0,len(fixture),256*1024)):
            part=root/f'fixture-{i}'
            part.write_bytes(fixture[offset:offset+256*1024])
            path=f'/persist/fixture-{i}'
            run('put',name,str(part),path)
            paths.append(path)
        execute(name,'cat '+' '.join(paths)+' > /home/dev/http-fixture; chmod 700 /home/dev/http-fixture; rm '+' '.join(paths))
        execute(name,'echo captured > /persist/state; /home/dev/http-fixture >/persist/http.log 2>&1 & sleep .1')
        assert 'Process counter: 1' in execute(name,'wget -qO- http://127.0.0.1:8080')
        snapshot=image+'-checkpoint'
        assert run('snapshot',name,snapshot)['image']==image
        execute(name,'echo newer > /persist/state')
        assert 'Process counter: 2' in execute(name,'wget -qO- http://127.0.0.1:8080')
        child=image+'-fork'
        assert run('fork',name,child,'--snapshot',snapshot)['image']==image
        assert 'captured' in execute(child,'cat /persist/state')
        assert 'Process counter: 2' in execute(child,'wget -qO- http://127.0.0.1:8080')
        execute(child,'echo independent > /persist/state')
        assert 'newer' in execute(name,'cat /persist/state')
        run('hibernate',name)
        run('start',name)
        assert 'Process counter: 3' in execute(name,'wget -qO- http://127.0.0.1:8080')
        run('restore',name,snapshot)
        assert 'Process counter: 2' in execute(name,'wget -qO- http://127.0.0.1:8080')
        assert 'captured' in execute(name,'cat /persist/state')
        run('stop',name)
        run('start',name)
        assert f'ID={image}' in execute(name,'cat /etc/os-release')
        assert 'captured' in execute(name,'cat /persist/state')
        run('stop',name)
        run('stop',child)
        print(image+': boot/OS identity/package manager/persistence/RAM+disk snapshot/fork/hibernate/restore passed',flush=True)
    cpu=next(line.split(':',1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name'))
    result={'passed':True,'host':{'cpu':cpu,'kernel':platform.release(),'memory_kib':next(line for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemTotal'))},
            'method':'5 serial create-to-management-ready trials per image, 256 MiB/1 vCPU; idle PSS read after 250 ms; warm host caches, no cache dropping; shared desktop host with other VMs and possible concurrent browser regression; uncontrolled load; nearest-rank p95; excludes host cache/controller/kernel overhead',
            'guest_kernel':'6.1.186','firecracker':'1.17.0','measurements':measurements}
    (root/'profile-result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result));print('Artifacts:',root)
finally:
    run('down')
