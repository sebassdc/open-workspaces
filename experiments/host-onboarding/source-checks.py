#!/usr/bin/env python3
"""No-VM CLI PTY and offline ingress denial/recovery checks. Synthetic secrets only."""
import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import subprocess
import tempfile
import termios
import time

ROOT=Path(__file__).resolve().parents[2]

def load(name,file):
    spec=importlib.util.spec_from_file_location(name,ROOT/file);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module

def pty_cancel(binary,root,kind):
    master,slave=pty.openpty();before=termios.tcgetattr(slave)
    process=subprocess.Popen([str(binary),'--data-dir',str(root),'host','join'],stdin=slave,stdout=slave,stderr=slave,env={'PATH':'/usr/bin:/bin'})
    output=b'';end=time.monotonic()+5
    while b'Paste invitation' not in output:
        assert time.monotonic()<end,'prompt timeout'
        if select.select([master],[],[],.1)[0]:output+=os.read(master,65536)
    assert not termios.tcgetattr(slave)[3]&termios.ECHO
    secret=b'SYNTHETIC_PRIVATE_INVITATION'
    if kind=='ctrl-c':os.write(master,secret+b'\x03')
    elif kind=='eof':os.write(master,secret+b'\x04')
    elif kind=='oversized':os.write(master,secret+b'x'*5000)
    elif kind=='multiline':os.write(master,secret+b'\nSYNTHETIC_QUEUED_COMMAND\n')
    else:
        os.write(master,secret)
        process.send_signal(signal.SIGINT if kind=='sigint' else signal.SIGTERM)
    assert process.wait(timeout=5)!=0
    while select.select([master],[],[],.05)[0]:
        try:output+=os.read(master,65536)
        except OSError:break
    assert secret not in output and b'SYNTHETIC_QUEUED_COMMAND' not in output,(kind,output)
    assert termios.tcgetattr(slave)==before,(kind,'terminal not restored')
    os.set_blocking(slave,False)
    try:remaining=os.read(slave,4096)
    except BlockingIOError:remaining=b''
    assert not remaining,(kind,'queued private input')
    assert not (root/'node.json').exists() and not (root/'worker.lock').exists()
    os.close(master);os.close(slave)

def ingress_checks(base):
    module=load('host_ingress','scripts/prepare-host-ingress.py')
    ca=base/'ca.pem';key=base/'key.pem'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(key),'-out',str(ca),'-days','1','-subj','/CN=controller.test','-addext','subjectAltName=DNS:controller.test'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    ca.chmod(0o600);key.chmod(0o600)
    human={'id':'allow-id','name':'Owner','decision':'allow','include':[{'email':{'email':'owner@example.test'}}],'exclude':[],'require':[],'precedence':1}
    cli={'id':'cli-policy','name':'Fixed CLI','decision':'bypass','include':[{'everyone':{}}],'exclude':[],'require':[],'precedence':1}
    common={'account_id':'test-account','type':'self_hosted','aud':'test-aud','team_domain':'team.cloudflareaccess.com','session_duration':'24h'}
    apps=[dict(common,id='dashboard-id',domain='pool.example.test',policies=[human]),dict(common,id='cli-id',domain='pool.example.test/cli/*',policies=[cli])]
    state={'capture_schema':1,'app_inventory_complete':True,'hostname':'pool.example.test','account_id':'test-account','zone_id':'test-zone','tunnel_id':'test-tunnel','dashboard_app_id':'dashboard-id','cli_app_id':'cli-id','dashboard_audience':['test-aud'],'team_domain':'team.cloudflareaccess.com','apps':apps,'native_trust':{'caPool':str(ca),'originServerName':'controller.test'},'ownership':{'account_id':'test-account','zone_id':'test-zone','tunnel_id':'test-tunnel','hostname':'pool.example.test'},'tunnel':{'tunnel':'test-tunnel','ingress':[{'hostname':'pool.example.test','path':module.OLD_CLI_REGEX,'service':'http://127.0.0.1:8787','originRequest':{'access':{'required':False}}},{'hostname':'pool.example.test','service':'http://127.0.0.1:8787','originRequest':{'access':{'required':True,'audTag':['test-aud']}}},{'service':'http_status:404'}]}}
    approved={k:copy.deepcopy(state[k]) for k in ['account_id','zone_id','tunnel_id','hostname','dashboard_app_id','cli_app_id','dashboard_audience','team_domain']}
    approved.update(dashboard_app=copy.deepcopy(apps[0]),cli_app=copy.deepcopy(apps[1]),native={'caPool':str(ca),'originServerName':'controller.test','serverCert':str(ca),'ca_sha256':hashlib.sha256(ca.read_bytes()).hexdigest(),'server_cert_sha256':hashlib.sha256(ca.read_bytes()).hexdigest()})
    # Actual route language: fixed optional Ubuntu asset, no wildcard/traversal expansion.
    for path in ['/cli/install.sh','/cli/host-manifest.json','/cli/host-compressed-manifest.json','/cli/host/ubuntu.ext4.zst','/cli/host/base.ext4','/cli/host/ubuntu.ext4','/cli/host/network-tools.tar.gz','/cli/ow-linux-amd64.sha256']:
        assert re.fullmatch(module.CLI_REGEX,path),path
    for path in ['/cli/host/arch.ext4','/cli/host/ubuntu.ext4.zst/extra','/cli/host/ubuntu.ext4Xzst','/cli/host/ubuntuXext4','/cli/host/ubuntu.ext4/extra','/cli/api/state','/cli/host/../ubuntu.ext4','/api/state']:
        assert not re.fullmatch(module.CLI_REGEX,path),path
    prior=copy.deepcopy(state);prior['tunnel']['ingress'][0]['path']=module.PRIOR_HOST_CLI_REGEX
    prior_plan=module.prepare(prior,approved)
    upgraded=copy.deepcopy(prior);upgraded['tunnel']=prior_plan['tunnel_after'];upgraded['node_app_id']='new-node-id';upgraded['apps'].append(dict(prior_plan['node_app_create'],id='new-node-id',account_id='test-account'))
    assert module.prepare(upgraded,approved)['tunnel_after']==upgraded['tunnel']
    foreign_route={'hostname':'unrelated.example.test','service':'http://127.0.0.1:9999'}
    upgraded['tunnel']['ingress'].insert(0,foreign_route)
    rolled=module.rollback(prior_plan,upgraded,'new-node-id')
    assert foreign_route in rolled['tunnel']['ingress']
    assert any(r.get('path')==module.PRIOR_HOST_CLI_REGEX for r in rolled['tunnel']['ingress'])
    assert module.rollback(prior_plan,rolled,'new-node-id')==rolled
    plan=module.prepare(state,approved)
    after=copy.deepcopy(state);after['tunnel']=plan['tunnel_after'];after['node_app_id']='new-node-id';after['apps'].append(dict(plan['node_app_create'],id='new-node-id',account_id='test-account'))
    assert module.prepare(after,approved)['node_app_create'] is None
    later={'hostname':'unrelated.example.test','service':'http://127.0.0.1:9999'};after['tunnel']['ingress'].insert(0,later)
    restored=module.rollback(plan,after,'new-node-id');assert later in restored['tunnel']['ingress'];assert module.rollback(plan,restored,'new-node-id')==restored
    partial=copy.deepcopy(state);partial['node_app_id']='new-node-id';partial['apps'].append(dict(plan['node_app_create'],id='new-node-id',account_id='test-account'))
    assert module.rollback(plan,partial,'new-node-id')==state
    mutations=[lambda s:s['apps'][0]['policies'][0].update(include=[{'everyone':{}}]),lambda s:s['tunnel'].update(originRequest={'noTLSVerify':True}),lambda s:s.update(app_inventory_complete=False),lambda s:s['apps'].append(dict(common,id='foreign',domain='*.example.test/_nodes/*',policies=[cli])),lambda s:s['native_trust'].update(originServerName='wrong.test'),lambda s:s['tunnel'].update(tunnel='foreign'),lambda s:s.update(dashboard_app_id='foreign'),lambda s:s['apps'][1]['policies'][0].update(precedence=9),lambda s:s['tunnel']['ingress'].insert(0,{'service':'http_status:404'})]
    for mutate in mutations:
        bad=copy.deepcopy(state);mutate(bad)
        try:module.prepare(bad,approved)
        except (ValueError,KeyError):pass
        else:raise AssertionError('Unsafe ingress state accepted')
    ca.chmod(0o644)
    try:module.prepare(state,approved)
    except ValueError:pass
    else:raise AssertionError('Public CA accepted')
    return 10

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',required=True,type=Path);a=p.parse_args()
    with tempfile.TemporaryDirectory(prefix='ow-hcheck-') as temporary:
        base=Path(temporary);base.chmod(0o700)
        for kind in ['ctrl-c','eof','oversized','multiline','sigint','sigterm']:
            root=base/kind;root.mkdir(mode=0o700);pty_cancel(a.binary.absolute(),root,kind)
        root=base/'explicit';root.mkdir(mode=0o700)
        cleared=subprocess.run([str(a.binary.absolute()),'--data-dir',str(root),'host','status'],capture_output=True,env={'PATH':'/usr/bin:/bin'})
        assert b'HOME required' not in cleared.stderr and cleared.returncode!=0
        # Owner output validation occurs before token acquisition/network mutation.
        helper=base/'helper';sentinel=base/'token-requested';helper.write_text('#!/bin/sh\ntouch '+str(sentinel)+'\nexit 1\n');helper.chmod(0o700)
        public=base/'public';public.mkdir(mode=0o755)
        env={'PATH':'/usr/bin:/bin','OW_CLOUDFLARED':str(helper)}
        rejected=subprocess.run([str(a.binary.absolute()),'--server','https://pool.example.test','host','invite','friend','--output',str(public/'invite')],capture_output=True,env=env)
        assert rejected.returncode!=0 and not sentinel.exists()
        rejected=subprocess.run([str(a.binary.absolute()),'--server','https://pool.example.test','host','invite','friend'],capture_output=True,env=env)
        assert rejected.returncode!=0 and not sentinel.exists()
        groups=ingress_checks(base)
    print(f'Passed: 6 PTY cancellation cases, cleared-HOME explicit root, 2 owner output denials, {groups} offline ingress denial/repeat/compensation checks. No VMs/services/cloud changes.')
