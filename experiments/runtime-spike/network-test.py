#!/usr/bin/env python3
"""Real guest egress, anti-spoofing, private destinations, snapshot/fork regression."""
import json, os, signal, socket, subprocess, tempfile, time
from pathlib import Path
repo=Path(__file__).resolve().parents[2]
root=Path(tempfile.mkdtemp(prefix='egress-',dir=repo/'data'))
cli=[str(repo/'ow'),'--local','--data-dir',str(root)]
checks=[]
def run(*args):
    r=subprocess.run(cli+list(args),capture_output=True,text=True,timeout=120)
    assert r.returncode==0,(args,r.stdout,r.stderr)
    return r.stdout
def guest(id,command):return run('exec',id,'--',command)
def reachable(id,url):
    return guest(id,f"if wget -T 2 -qO- '{url}' >/dev/null 2>&1; then echo ACCESSIBLE; else echo BLOCKED; fi").strip()=='ACCESSIBLE'
try:
    assert json.loads(run('up'))['internet']
    run('create','parent');run('create','peer')
    for image in ('alpine','arch','ubuntu'):
        id='parent' if image=='alpine' else image
        if image!='alpine':run('create',id,'--image',image)
        assert 'Example Domain' in guest(id,'wget -T 15 -qO- http://example.com')
        checks.append(image+' DNS + public HTTP')
        # BusyBox HTTPS is tested here for routing, not claimed as certificate validation.
        assert 'Example Domain' in guest(id,'wget -T 15 -qO- https://example.com')
        checks.append(image+' public HTTPS routing')
    guest('peer','/http-fixture >/run/network-http.log 2>&1 &')
    peer=json.loads(run('inspect','peer'))
    ip=f"198.18.{peer['index']//64}.{peer['index']%64*4+2}"
    for url in [f'http://{ip}:8080','http://10.0.2.2:8787','http://10.0.2.3:8787','http://169.254.169.254/latest/meta-data/','http://192.168.1.1','http://127.0.0.1:8787']:
        assert not reachable('parent',url),url
    checks.append('peer, uplink aliases, metadata, LAN and loopback blocked')
    host=json.loads(subprocess.check_output(['ip','-j','-4','addr','show']))
    for link in host:
        for a in link.get('addr_info',[]):
            assert not reachable('parent',f"http://{a['local']}:8787"),a['local']
    checks.append('all actual host interface addresses blocked')
    # Guest root tries to impersonate another VM and use the uplink subnet.
    guest('parent',f'ip addr add {ip}/32 dev eth0; ip addr add 10.0.2.100/32 dev eth0; echo 1 > /proc/sys/net/ipv4/ip_forward')
    for source in [ip,'10.0.2.100']:
        guest('parent',f'ip route replace default via 198.18.0.1 src {source}')
        assert not reachable('parent','http://example.com'),source
    guest('parent','ip route replace default via 198.18.0.1 src 198.18.0.2')
    checks.append('guest root source spoofing blocked')
    guest('parent',f'ip addr del {ip}/32 dev eth0; ip addr del 10.0.2.100/32 dev eth0')
    run('snapshot','parent','connected');run('fork','parent','forked','--snapshot','connected')
    assert 'Example Domain' in guest('forked','wget -T 15 -qO- http://example.com')
    assert not reachable('forked',f'http://{ip}:8080')
    run('hibernate','parent');run('start','parent')
    assert 'Example Domain' in guest('parent','wget -T 15 -qO- http://example.com')
    checks.append('fork and hibernation restore route + DNS + policy')
    worker=json.loads(run('status'))['worker_pid']
    supervisor=int(Path(f'/proc/{worker}/stat').read_text().rsplit(')',1)[1].split()[1])
    helper=None
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():continue
        try:
            if int((proc/'stat').read_text().rsplit(')',1)[1].split()[1])==supervisor and b'slirp4netns' in (proc/'cmdline').read_bytes():helper=int(proc.name)
        except (FileNotFoundError,PermissionError):pass
    assert helper is not None
    os.kill(helper,signal.SIGTERM)
    deadline=time.monotonic()+5
    while subprocess.run(cli+['status'],capture_output=True).returncode==0:
        assert time.monotonic()<deadline,'worker survived failed uplink'
        time.sleep(.1)
    run('up');assert json.loads(run('status'))['running']==0
    run('start','parent');assert 'Example Domain' in guest('parent','wget -T 15 -qO- http://example.com')
    checks.append('uplink failure stops worker; preserved guest disk restarts with egress')
    result={'passed':True,'checks':checks,'network_helper':'slirp4netns 1.3.6 / libslirp 4.9.5','scope':'rootless IPv4 TCP/UDP egress; no bandwidth benchmark'}
    (root/'result.json').write_text(json.dumps(result,indent=2));print(json.dumps(result));print(root)
finally:run('down')
