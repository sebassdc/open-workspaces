#!/usr/bin/env python3
"""Bounded real OpenSSH acceptance. Requires explicit planner-approved fresh root."""
import os,sys,json,time,socket,ssl,urllib.request,subprocess,hashlib,sqlite3,shlex,base64
from pathlib import Path
PREPARE=sys.argv[1]=='--prepare'
ROOT=Path(sys.argv[2] if PREPARE else sys.argv[1]).resolve()
REPO=Path(__file__).resolve().parents[2]
BIN=Path(os.environ['OW_SSH_TEST_BINARY']).resolve()
SOURCE=Path(os.environ['OW_SSH_TEST_ASSETS']).resolve()
PROFILE=os.environ.get('OW_SSH_TEST_PROFILE','ubuntu')
assert PROFILE in ['ubuntu','arch']
ROOT.mkdir(parents=True,exist_ok=True,mode=0o700)
def sources():
 files=[REPO/'Cargo.toml',REPO/'Cargo.lock',REPO/'crates/ow/Cargo.toml',REPO/'experiments/ssh-guest/provision.sh',Path(__file__)]
 files+=list((REPO/'crates/ow/src').glob('*.rs'))+list((REPO/'crates/ow/ui').rglob('*'))
 return {str(p.relative_to(REPO)):hashlib.sha256(p.read_bytes()).hexdigest() for p in files if p.is_file()}
def processes():
 out={}
 for p in Path('/proc').iterdir():
  if not p.name.isdigit():continue
  try:
   comm=(p/'comm').read_text().strip();cmd=(p/'cmdline').read_bytes().replace(b'\0',b' ').decode()
   if 'firecracker' in comm or (cmd.startswith('/home/sebassdc/dev/open-workspaces/') and ('worker' in cmd or 'node-' in cmd or 'dashboard' in cmd or 'supervisor' in cmd)):
    out[p.name]={'comm':comm,'argv':cmd,'start':(p/'stat').read_text().split(') ')[1].split()[19]}
  except (FileNotFoundError,PermissionError):pass
 return out
def headroom():
 mem=int(next(l.split()[1] for l in Path('/proc/meminfo').read_text().splitlines() if l.startswith('MemAvailable:')))
 disk=os.statvfs(ROOT);assert mem>=8*1024**2 and disk.f_bavail*disk.f_frsize>=16*1024**3
 return {'available_kib':mem,'free_disk_bytes':disk.f_bavail*disk.f_frsize}
def call(args,env=None,timeout=120,input=None):
 r=subprocess.run(list(map(str,args)),env=env,capture_output=True,input=input,timeout=timeout)
 if r.returncode:raise RuntimeError(f'{args[:4]} exit {r.returncode}: {r.stderr.decode(errors="replace")[-2000:]} {r.stdout.decode(errors="replace")[-2000:]}')
 return r.stdout
if PREPARE:
 headroom();(ROOT/'baseline.json').write_text(json.dumps(processes(),indent=2))
 # Capture inode only of existing owner disk; never open its content.
 owner=Path('/home/sebassdc/dev/open-workspaces/data/prototype/machines/ubuntu-dev/disk.ext4')
 if owner.exists():(ROOT/'owner-disk.json').write_text(json.dumps({'inode':owner.stat().st_ino,'device':owner.stat().st_dev}))
 for args in [
 ['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=SSH Test CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign','-keyout',ROOT/'ca.key','-out',ROOT/'ca.pem'],
 ['openssl','req','-newkey','rsa:2048','-nodes','-subj','/CN=localhost','-keyout',ROOT/'leaf.key','-out',ROOT/'leaf.csr'],
 ]:call(args)
 (ROOT/'leaf.ext').write_text('subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
 call(['openssl','x509','-req','-in',ROOT/'leaf.csr','-CA',ROOT/'ca.pem','-CAkey',ROOT/'ca.key','-CAcreateserial','-days','1','-extfile',ROOT/'leaf.ext','-out',ROOT/'leaf.pem'])
 for name in ['client','wrong']:call(['ssh-keygen','-q','-t','ed25519','-N','','-f',ROOT/name])
 assets=ROOT/'assets';(assets/'guest').mkdir(parents=True)
 for name in ['official','downloads','bin']:(assets/name).symlink_to(SOURCE/name,target_is_directory=True)
 for name in ['network-tools.tar.gz','ow-guest']:(assets/'guest'/name).symlink_to(SOURCE/'guest'/name)
 call(['cp','--reflink=always',SOURCE/'guest'/f'{PROFILE}.ext4',assets/'guest'/f'{PROFILE}.ext4'])
 (ROOT/'inputs.json').write_text(json.dumps({'binary_sha256':hashlib.sha256(BIN.read_bytes()).hexdigest(),'source_ubuntu_manifest':json.loads((SOURCE/'guest'/f'{PROFILE}.json').read_text()),'headroom':headroom(),'source_files':sources(),'profile':PROFILE,'fixture_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()},indent=2))
 sys.exit(0)
ENV={**os.environ,'OW_ASSET_DIR':str(ROOT/'assets'),'OW_MAX_MEMORY_MIB':'1024','OW_MAX_RUNNING':'1','OW_MAX_VCPUS':'2','OW_MIN_FREE_GIB':'8','SSL_CERT_FILE':str(ROOT/'ca.pem'),'SSL_CERT_DIR':str(ROOT/'empty-ca'),'XDG_CONFIG_HOME':str(ROOT/'client-config')}
(ROOT/'empty-ca').mkdir()
# Private token helper returns only this fixture's synthetic token.
helper=ROOT/'cloudflared';helper.write_text('#!/bin/sh\ncat '+shlex.quote(str(ROOT/'token'))+'\n');helper.chmod(0o700);ENV['OW_CLOUDFLARED']=str(helper)
ORIGIN=(ROOT/'origin').read_text();CTX=ssl.create_default_context(cafile=str(ROOT/'ca.pem'))
checks=[];children=[];worker=ROOT/'w';ctl=ROOT/'c';cleanup=False
log=(ROOT/'commands.log').open('w')
def local(*args):
 log.write('local '+repr(args)+'\n');log.flush();return call([BIN,'--local','--data-dir',worker,*args],env=ENV)
def remote(*args):
 log.write('remote '+repr(args)+'\n');log.flush();return call([BIN,'--server',ORIGIN,*args],env=ENV)
def rpc(**v):
 with socket.socket(socket.AF_UNIX) as s:
  s.settimeout(120);s.connect(str(worker/'control.sock'));s.sendall(json.dumps(v).encode()+b'\n');b=b''
  while not b.endswith(b'\n'):b+=s.recv(65536)
  r=json.loads(b);assert r['ok'],r;return r['result']
def priv(command):return rpc(op='shell-exec',id=physical,command=command)['output']
def api(path,jwt=None):
 req=urllib.request.Request(ORIGIN+path,headers={'origin':ORIGIN,'cf-access-token':jwt or (ROOT/'token').read_text()})
 try:
  with urllib.request.urlopen(req,context=CTX,timeout=5) as r:return r.status,r.read()
 except urllib.error.HTTPError as e:return e.code,e.read()
def check(name):checks.append(name);print(name,flush=True)
def config(mode='remote',name='box'):
 data=remote('ssh-config',name) if mode=='remote' else local('ssh-config',physical)
 file=ROOT/(mode+'-'+name+'-ssh-config');file.write_bytes(data);return file,'ow-'+(name if mode=='remote' else physical)
def ssh(cfg,alias,command,key='client',tty=False):
 r=subprocess.run(['ssh','-F',str(cfg),'-i',str(ROOT/key),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),'-o','StrictHostKeyChecking=yes',*(['-tt'] if tty else []),alias,command],env=ENV,capture_output=True,timeout=30)
 (ROOT/'last-ssh.json').write_text(json.dumps({'exit':r.returncode,'stdout':r.stdout.decode(errors='replace'),'stderr':r.stderr.decode(errors='replace')},indent=2))
 return r
def websocket(target):
 u=urllib.parse.urlsplit(ORIGIN);s=CTX.wrap_socket(socket.create_connection((u.hostname,u.port),timeout=5),server_hostname=u.hostname)
 key=base64.b64encode(os.urandom(16)).decode()
 s.sendall((f'GET /api/ssh/{target} HTTP/1.1\r\nHost: {u.netloc}\r\nOrigin: {ORIGIN}\r\ncf-access-token: {(ROOT/"token").read_text()}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n').encode())
 headers=b''
 while not headers.endswith(b'\r\n\r\n'):
  assert len(headers)<8192;part=s.recv(1);assert part;headers+=part
 assert b'101 Switching Protocols' in headers,headers
 assert base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).lower() in headers.lower()
 return s
def wsframe(s,kind,payload):
 mask=os.urandom(4);size=len(payload);header=bytes([0x80|kind,0x80|size]) if size<126 else bytes([0x80|kind,0x80|126])+size.to_bytes(2,'big')
 s.sendall(header+mask+bytes(b^mask[i%4] for i,b in enumerate(payload)))
def wsclosed(s):
 def read(n):
  b=b''
  while len(b)<n:
   part=s.recv(n-len(b))
   if not part:return None
   b+=part
  return b
 deadline=time.monotonic()+6
 while time.monotonic()<deadline:
  head=read(2)
  if head is None:return True
  size=head[1]&127
  if size==126:size=int.from_bytes(read(2),'big')
  elif size==127:size=int.from_bytes(read(8),'big')
  assert size<=16384
  payload=read(size)
  if head[0]&15==8:return True
  if head[0]&15==9:wsframe(s,10,payload)
 return False
def trust(info):
 with (ROOT/'known_hosts').open('a') as f:f.write('ow-'+info['identity']+' '+info['host_key']+'\n')
try:
 assert processes()==json.loads((ROOT/'baseline.json').read_text()),'shared process baseline changed before boot'
 headroom();local('up')
 assert json.loads(local('status'))['max_running']==1
 # Dedicated TLS node controller, never shared ports.
 with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
 node_origin=f'https://localhost:{port}'
 node_log=(ROOT/'node.log').open('wb')
 children.append(subprocess.Popen([str(BIN),'--local','--data-dir',str(ctl),'node-controller','--listen',f'127.0.0.1:{port}','--tls-cert',str(ROOT/'leaf.pem'),'--tls-key',str(ROOT/'leaf.key')],env=ENV,stdout=node_log,stderr=node_log))
 time.sleep(.5)
 call([BIN,'--local','--data-dir',ctl,'node-join','test-node','--output',worker/'join.json','--memory','1024','--slots','1','--cpus','2'],env=ENV)
 agentargs=[str(BIN),'--local','--data-dir',str(worker),'node-agent','--controller',node_origin,'--credential',str(worker/'credential.json'),'--join',str(worker/'join.json'),'--ca-cert',str(ROOT/'ca.pem')]
 children.append(subprocess.Popen(agentargs,env=ENV,stdout=node_log,stderr=node_log))
 deadline=time.monotonic()+20
 while True:
  nodes=json.loads(call([BIN,'--local','--data-dir',ctl,'nodes'],env=ENV))
  if nodes and nodes[0]['online'] and (ctl/'nodes/test-node/control.sock').exists():break
  assert time.monotonic()<deadline;time.sleep(.2)
 remote('create','box','--image',PROFILE,'--memory','1024','--cpus','2','--node','test-node')
 with sqlite3.connect(ctl/'catalog.sqlite3') as db:physical=db.execute("SELECT physical FROM resources WHERE name='box' AND kind='machine'").fetchone()[0]
 assert json.loads(local('list'))[0]['id']==physical
 check('real outbound guest created; local and node routes reference same isolated guest')
 # Only dedicated test guest provisioned; package operation logs retained in guest.
 remote('ssh-authorize','box','--key',ROOT/'client.pub','--upgrade')
 info=json.loads(remote('ssh-info','box'));trust(info)
 cfg,alias=config();lcfg,lalias=config('local')
 for c,a in [(lcfg,lalias),(cfg,alias)]:
  r=ssh(c,a,'id -un; printf SSH_BYTES; exit 42');assert r.returncode==42 and r.stdout==b'dev\nSSH_BYTES',(r.returncode,r.stdout,r.stderr)
 check('ordinary OpenSSH exec/dev/raw stdout/exit42 through both routes')
 r=subprocess.run([str(BIN),'--server',ORIGIN,'ssh','box','--','-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),'id -un; exit 37'],env=ENV,capture_output=True,timeout=30)
 assert r.returncode==37 and r.stdout==b'dev\n',(r.returncode,r.stdout,r.stderr)
 check('easy ow ssh launcher returns actual OpenSSH guest exit37')
 r=ssh(cfg,alias,'test -t 0 && test -t 1 && echo SSH_PTY_OK; stty size; exit 23',tty=True);assert r.returncode==23 and b'SSH_PTY_OK' in r.stdout,(r.stdout,r.stderr)
 check('guest OpenSSH PTY/exit23')
 forward=subprocess.Popen(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),'-W','127.0.0.1:22',alias],env=ENV,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 children.append(forward);assert forward.stdout.readline().startswith(b'SSH-2.0-OpenSSH');forward.terminate();forward.wait(timeout=5)
 denied=subprocess.run(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),'-o','ExitOnForwardFailure=yes','-R','127.0.0.1:0:127.0.0.1:22',alias,'true'],env=ENV,capture_output=True,timeout=10)
 assert denied.returncode==255 and b'remote port forwarding failed' in denied.stderr
 check('guest local TCP forwarding works; remote forwarding denied')
 payload=bytes(range(256))*128;(ROOT/'payload').write_bytes(payload)
 batch=ROOT/'sftp-batch';batch.write_text(f'put {ROOT}/payload /persist/ssh-payload\nget /persist/ssh-payload {ROOT}/received\n')
 call(['sftp','-F',cfg,'-i',ROOT/'client','-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),'-b',batch,alias],env=ENV)
 assert (ROOT/'received').read_bytes()==payload;check('ordinary SFTP binary roundtrip')
 r=ssh(cfg,alias,'true',key='wrong');assert r.returncode==255;check('unenrolled key denied')
 # Explicit caller trust failure: no auto replacement.
 (ROOT/'known_hosts.bad').write_text('ow-'+info['identity']+' '+(ROOT/'wrong.pub').read_text())
 r=subprocess.run(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts.bad'),alias,'true'],env=ENV,capture_output=True,timeout=15);assert r.returncode==255;check('changed known host denied')
 # Foreign owner resolves no machine; this GET carries no upgrade so must deny first.
 assert api('/api/ssh/box',(ROOT/'foreign-token').read_text())[0]==404
 for path in ['/api/ssh/unknown','/api/ssh/box?port=22','/api/ssh/box/extra','/api/ssh//api/ssh/box']:
  assert api(path)[0] in [400,404]
 check('foreign owner, unknown, malformed target denied')
 for origin,env in [(ORIGIN,{**ENV,'SSL_CERT_FILE':str(ROOT/'leaf.pem')}),(ORIGIN.replace('localhost','127.0.0.1'),ENV)]:
  # leaf without CA trust, and proper CA with wrong hostname.
  r=subprocess.run([str(BIN),'--server',origin,'ssh-proxy','box'],env=env,input=b'not ssh',capture_output=True,timeout=15);assert r.returncode and not r.stdout
 check('wrong CA and hostname deny proxy with empty stdout')
 for name in ['fixture-redirect','fixture-stall']:
  start=time.monotonic();r=subprocess.run([str(BIN),'--server',ORIGIN,'ssh-proxy',name],env=ENV,input=b'SSH-2.0-test\r\n',capture_output=True,timeout=15)
  assert r.returncode and not r.stdout
  if name=='fixture-stall':assert .5<=time.monotonic()-start<=13
 check('TLS fixture redirect refused without follow; silent HTTP handshake rejected within connection bounds')
 for kind,frame_payload in [(1,b'text'),(2,b'x'*17000)]:
  with websocket('box') as ws:wsframe(ws,kind,frame_payload);assert wsclosed(ws)
 check('actual authorized guest stream closes on text and oversized WSS frames')
 # Active stream fence on key replacement; no application byte stream reused.
 p=subprocess.Popen(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),alias,'while :; do echo tick; sleep .2; done'],env=ENV,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 children.append(p)
 assert p.stdout.readline()==b'tick\n'
 remote('ssh-revoke','box');p.wait(timeout=10);assert p.returncode!=0
 assert ssh(cfg,alias,'true').returncode==255;check('active key revoke closes stream and denies reconnect')
 remote('ssh-authorize','box','--key',ROOT/'client.pub');assert json.loads(remote('ssh-info','box'))['host_key']==info['host_key']
 remote('snapshot','box','ssh-saved');remote('ssh-revoke','box');remote('stop','box')
 assert ssh(cfg,alias,'true').returncode==255;check('stopped guest denies SSH')
 remote('restore','box','ssh-saved');assert ssh(cfg,alias,'true').returncode==255
 remote('ssh-authorize','box','--key',ROOT/'client.pub');assert ssh(cfg,alias,'cat /persist/ssh-payload').stdout==payload
 assert json.loads(remote('ssh-info','box'))['host_key']==info['host_key'];check('restore preserves host identity and external revoked-key policy')
 remote('stop','box');remote('fork','box','child','--snapshot','ssh-saved')
 with sqlite3.connect(ctl/'catalog.sqlite3') as db:child=db.execute("SELECT physical FROM resources WHERE name='child' AND kind='machine'").fetchone()[0]
 r=subprocess.run([str(BIN),'--server',ORIGIN,'ssh-proxy','child'],env=ENV,input=b'SSH-2.0-test\r\n',capture_output=True,timeout=15);assert r.returncode and not r.stdout
 assert rpc(op='shell-exec',id=child,command='test ! -e /etc/ow-ssh/authorized_keys && ! pgrep -x sshd')['exit_code']==0
 remote('ssh-authorize','child','--key',ROOT/'client.pub');childinfo=json.loads(remote('ssh-info','child'));assert childinfo['identity']!=info['identity'] and childinfo['host_key']!=info['host_key'];trust(childinfo)
 ccfg,calias=config(name='child');assert ssh(ccfg,calias,'id -un').stdout==b'dev\n';check('sequential RAM/disk fork has independent host identity and no inherited enrollment')
 remote('stop','child');remote('start','box');assert ssh(cfg,alias,'id -un').stdout==b'dev\n';check('cold boot retains host identity and authorization')
 remote('hibernate','box');remote('start','box');assert ssh(cfg,alias,'id -un').stdout==b'dev\n';assert json.loads(remote('ssh-info','box'))['host_key']==info['host_key'];check('hibernate/resume preserves host identity')
 remote('stop','box');local('down');local('up');time.sleep(3);remote('start','box');assert ssh(cfg,alias,'id -un').stdout==b'dev\n';check('worker cold restart recovers external SSH policy and persistent guest files')
 # Applying a bad daemon config must fence the previous listener/policy.
 priv("printf 'InvalidSSHOption yes\\n' >> /etc/ow-ssh/sshd_config")
 failed=subprocess.run([str(BIN),'--server',ORIGIN,'ssh-authorize','box','--key',str(ROOT/'wrong.pub')],env=ENV,capture_output=True,timeout=30)
 assert failed.returncode and not json.loads(remote('ssh-info','box'))['ready'];assert ssh(cfg,alias,'true').returncode==255;assert ssh(cfg,alias,'true',key='wrong').returncode==255
 remote('restore','box','ssh-saved');assert ssh(cfg,alias,'true').returncode==255;assert ssh(cfg,alias,'true',key='wrong').returncode==0
 remote('ssh-authorize','box','--key',ROOT/'client.pub');check('partial key-application failure fences old/new access; restore reapplies current external policy')
 priv("rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub; ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key")
 remote('stop','box')
 conflict=subprocess.run([str(BIN),'--local','--data-dir',str(worker),'start',physical],env=ENV,capture_output=True,timeout=20)
 assert conflict.returncode and b'SSH host identity changed' in conflict.stderr and not json.loads(local('ssh-info',physical))['ready']
 assert ssh(cfg,alias,'true').returncode==255
 remote('restore','box','ssh-saved');assert ssh(cfg,alias,'true').returncode==0;check('changed guest host key blocks cold exposure; matching restore recovers without trust replacement')
 # Latest node connection replaces old generation and invalidates active stream.
 p=subprocess.Popen(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),alias,'while :; do echo fence; sleep .2; done'],env=ENV,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 children.append(p)
 assert p.stdout.readline()==b'fence\n'
 # An authenticated replacement control socket, without bypassing the agent's
 # legitimate same-root process lock. The controller assigns a new generation.
 credential=json.loads((worker/'credential.json').read_text())['credential']
 wskey=base64.b64encode(os.urandom(16)).decode()
 replacement=CTX.wrap_socket(socket.create_connection(('localhost',port),timeout=5),server_hostname='localhost')
 replacement.sendall((f'GET /node/test-node HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {wskey}\r\nAuthorization: Bearer {credential}\r\n\r\n').encode())
 headers=b''
 while not headers.endswith(b'\r\n\r\n'):
  assert len(headers)<8192;part=replacement.recv(1);assert part;headers+=part
 assert b'101 Switching Protocols' in headers
 expected=base64.b64encode(hashlib.sha1((wskey+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
 assert expected.lower() in headers.lower()
 p.wait(timeout=10);assert p.returncode!=0;replacement.close();check('active authenticated node replacement closes SSH')
 time.sleep(4)
 p=subprocess.Popen(['ssh','-F',str(cfg),'-i',str(ROOT/'client'),'-o','BatchMode=yes','-o','UserKnownHostsFile='+str(ROOT/'known_hosts'),alias,'while :; do echo revoke; sleep .2; done'],env=ENV,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 children.append(p)
 assert p.stdout.readline()==b'revoke\n'
 call([BIN,'--local','--data-dir',ctl,'node-revoke','test-node'],env=ENV)
 p.wait(timeout=10);assert p.returncode!=0;assert api('/api/ssh/box')[0]==404;check('active node revocation closes SSH and denies offline reconnect')
except Exception as exc:
 print(repr(exc),file=sys.stderr);(ROOT/'failure.txt').write_text(repr(exc));raise
finally:
 for childproc in reversed(children):
  if childproc.poll() is None:childproc.terminate()
  try:childproc.wait(timeout=10)
  except subprocess.TimeoutExpired:childproc.kill();childproc.wait(timeout=5)
 try:local('down');cleanup=True
 except Exception as exc:(ROOT/'cleanup-error.txt').write_text(repr(exc))
 time.sleep(.3)
 after=processes();baseline=json.loads((ROOT/'baseline.json').read_text());shared_ok=after==baseline
 owner=Path('/home/sebassdc/dev/open-workspaces/data/prototype/machines/ubuntu-dev/disk.ext4');inode_ok=not (ROOT/'owner-disk.json').exists() or json.loads((ROOT/'owner-disk.json').read_text())=={'inode':owner.stat().st_ino,'device':owner.stat().st_dev}
 owned=[]
 for pdir in Path('/proc').iterdir():
  try:
   cmd=(pdir/'cmdline').read_bytes()
   if str(ROOT).encode() in cmd and ('firecracker' in (pdir/'comm').read_text() or b' worker' in cmd or b'node-agent' in cmd or b'node-controller' in cmd or b'supervisor' in cmd):owned.append(pdir.name)
  except (FileNotFoundError,PermissionError,NotADirectoryError):pass
 result={'checks':checks,'worker_down':cleanup,'shared_processes_unchanged':shared_ok,'owner_disk_inode_unchanged':inode_ok,'remaining_owned_processes':owned,'binary_sha256':hashlib.sha256(BIN.read_bytes()).hexdigest(),'source_unchanged':sources()==json.loads((ROOT/'inputs.json').read_text())['source_files']}
 (ROOT/'result.json').write_text(json.dumps(result,indent=2));log.close()
 assert cleanup and shared_ok and inode_ok and not owned and result['source_unchanged'],result
