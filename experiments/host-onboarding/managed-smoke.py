#!/usr/bin/env python3
"""Exclusive bounded local TLS/managed-host/Alpine slice; never touches live services.
Requires final immutable bundle and previously resolved private admission baseline.
Writes private evidence; retains owned disks/logs for review, stops owned children.
"""
import argparse, fcntl, hashlib, http.client, http.server, json, os, re, select, socket, sqlite3, ssl
import subprocess, threading, time, uuid, pty
from pathlib import Path

def rpc(root, request):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(40); s.connect(str(root/'control.sock')); s.sendall(json.dumps(request).encode()+b'\n')
        data=b''
        while b'\n' not in data:
            chunk=s.recv(65536)
            if not chunk:raise EOFError('control response ended before newline')
            data+=chunk
            if len(data)>1048576: raise RuntimeError('response bound')
        value=json.loads(data.split(b'\n')[0]); assert value.get('ok'), value
        return value['result']

def proc_inventory():
    processes=[]
    units={}
    for unit in ['user@'+str(os.getuid())+'.service','tailscaled.service']:
        properties=subprocess.check_output(['systemctl','show',unit,'-p','MainPID','-p','ControlGroup'],text=True)
        units[unit]=dict(line.split('=',1) for line in properties.splitlines())
    for p in Path('/proc').iterdir():
        if not p.name.isdigit():continue
        try:
            if p.stat().st_uid!=os.getuid():continue
            cmd=(p/'cmdline').read_bytes().replace(b'\0',b' ').decode(errors='replace')
            if not cmd:continue
            stat=(p/'stat').read_text().rsplit(')',1)[1].split();ticks=stat[19]
            try:cwd=str((p/'cwd').readlink());proof=None
            except PermissionError:
                parent=stat[1];comm=(p/'comm').read_text().strip();group=(p/'cgroup').read_text().strip().split(':',2)[-1]
                manager=units['user@'+str(os.getuid())+'.service'];tail=units['tailscaled.service']
                if p.name==manager['MainPID'] and parent=='1' and comm=='systemd' and group==manager['ControlGroup']+'/init.scope' and cmd.strip()=='/usr/lib/systemd/systemd --user':proof='system-manager main PID and exact launch + cgroup'
                elif parent==manager['MainPID'] and comm=='(sd-pam)' and cmd.strip()=='(sd-pam)' and group==manager['ControlGroup']+'/init.scope':proof='manager PAM child and cgroup'
                elif parent==tail['MainPID'] and comm=='tailscaled' and cmd.startswith('/usr/bin/tailscaled be-child ssh ') and 'herdr remote-client-bridge --idle-timeout-v1' in cmd and Path('/proc/'+parent).stat().st_uid==0:
                    session=group.rsplit('/',1)[-1].removeprefix('session-').removesuffix('.scope')
                    properties=subprocess.check_output(['loginctl','show-session',session,'-p','Leader','-p','Service','-p','Remote','-p','Scope','-p','User'],text=True)
                    record=dict(line.split('=',1) for line in properties.splitlines())
                    assert record=={'Leader':p.name,'Service':'tailscaled','Remote':'yes','Scope':'session-'+session+'.scope','User':str(os.getuid())},'unresolved login supervisor'
                    proof='root-supervised Tailscale SSH bridge wrapper + logind session leader'
                else:raise RuntimeError('unresolved unreadable same-UID process '+p.name)
                cwd='<independently resolved non-runtime supervisor>'
            processes.append({'pid':int(p.name),'ticks':ticks,'cwd':cwd,'cmd':cmd,'non_runtime_proof':proof})
        except FileNotFoundError:continue
        except PermissionError:raise RuntimeError('unreadable same-UID process inventory')
    return processes

def held(path):
    if not path.exists():return False
    with path.open('rb') as f:
        try:fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB);return False
        except BlockingIOError:return True

def inventory(data):
    procs=proc_inventory();workers=[];stale=[];catalogs=[]
    for sock in data.rglob('control.sock'):
        root=sock.parent
        if '/nodes/' in str(root):continue # controller proxy, registered nodes separately below
        try:
            status=rpc(root,{'op':'status'});machines=rpc(root,{'op':'list'})
            running=[m for m in machines if m['state']=='running']
            workers.append({'root':str(root),'status':status,'running':running})
        except (OSError,AssertionError,ValueError,EOFError):
            assert not held(root/'worker.lock'), ('unresolved worker lock',str(root))
            assert not any(str(root) in p['cmd'] or p['cwd']==str(root) or p['cwd'].startswith(str(root)+'/') for p in procs),('unresolved root processes',str(root))
            stale.append(str(root))
    for db in data.rglob('catalog.sqlite3'):
        root=db.parent
        active=held(root/'catalog-gateway.lock') or held(root/'node-controller.lock') or any(str(root) in p['cmd'] for p in procs)
        if not active:continue
        con=sqlite3.connect('file:'+str(db)+'?mode=ro',uri=True)
        resources=con.execute("SELECT node,physical,reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND reserved_mib>0").fetchall()
        uncertain=con.execute("SELECT COUNT(*) FROM operations WHERE state IN ('pending','uncertain')").fetchone()[0]
        assert uncertain==0,('uncertain operations',str(db))
        nodes=con.execute('SELECT id,revoked FROM nodes WHERE credential_hash IS NOT NULL').fetchall()
        for node,revoked in nodes:
            if revoked:continue
            if node=='local':continue
            status=rpc(root/'nodes'/node,{'op':'status'})
            assert any(w['status']['worker_pid']==status['worker_pid'] for w in workers),('unresolved registered worker',node)
        catalogs.append({'path':str(db),'resources':resources,'uncertain':uncertain,'nodes':nodes});con.close()
    actual=[m for w in workers for m in w['running']];reserved=sum(r[2] for c in catalogs for r in c['resources'])
    assert len(actual)==1 and actual[0]['id']=='ubuntu-dev' and actual[0]['memory_mib']==2048,actual
    assert reserved==2048,catalogs
    vmms=[p for p in procs if 'firecracker-v1.17.0-x86_64' in p['cmd'] and '/machines/' in p['cwd']]
    assert len(vmms)==1,vmms
    mem=int(re.search(r'MemAvailable:\s+(\d+)',Path('/proc/meminfo').read_text())[1])//1024
    assert mem>2048,('headroom',mem)
    return {'resolved_unreadable_supervisors':[{k:v for k,v in p.items() if k!='cmd'} for p in procs if p['non_runtime_proof']], 'workers':workers,'stale_unowned_sockets':stale,'active_catalogs':catalogs,'vmm_processes':vmms,'mem_available_mib':mem,'actual_running':actual}

def wait_ready(cli,home,root):
    for _ in range(100):
        status=json.loads(subprocess.check_output([str(cli),'host','status'],env=dict(os.environ,HOME=str(home))))
        if status['controller_dispatchable']:return status
        time.sleep(.2)
    raise RuntimeError('controller never dispatchable: '+json.dumps(status))

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--bundle',type=Path,required=True);parser.add_argument('--evidence',type=Path,required=True);parser.add_argument('--data',type=Path,required=True);a=parser.parse_args()
    os.umask(0o077);a.evidence.mkdir(mode=0o700);before=inventory(a.data)
    (a.evidence/'before.json').write_text(json.dumps(before,indent=2))
    ident=uuid.uuid4().hex[:8];home=Path.home()/('.ow-hch-'+ident);controller=Path.home()/('.ow-hcc-'+ident)
    home.mkdir(mode=0o700);controller.mkdir(mode=0o700);root=home/'.ow-host'
    ca=a.evidence/'ca.pem';key=a.evidence/'key.pem'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(key),'-out',str(ca),'-days','1','-subj','/CN=Host smoke CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    cert=a.evidence/'server.pem';server_key=a.evidence/'server-key.pem';csr=a.evidence/'server.csr';extensions=a.evidence/'server.ext'
    extensions.write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
    subprocess.run(['openssl','req','-new','-newkey','rsa:2048','-nodes','-keyout',str(server_key),'-out',str(csr),'-subj','/CN=localhost'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    subprocess.run(['openssl','x509','-req','-in',str(csr),'-CA',str(ca),'-CAkey',str(key),'-CAcreateserial','-out',str(cert),'-days','1','-extfile',str(extensions)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    reserve=socket.socket();reserve.bind(('127.0.0.1',0));native_port=reserve.getsockname()[1];reserve.close()
    binary=a.bundle/'ow-linux-amd64';assert binary.exists()
    log=(a.evidence/'controller.log').open('wb')
    child=subprocess.Popen([str(binary),'--local','--data-dir',str(controller),'node-controller','--listen',f'127.0.0.1:{native_port}','--tls-cert',str(cert),'--tls-key',str(server_key)],stdout=log,stderr=log)
    client_ctx=ssl.create_default_context(cafile=str(ca))
    class Edge(http.server.BaseHTTPRequestHandler):
        protocol_version='HTTP/1.1'
        def log_message(self,*args):pass
        def do_GET(self):self.handle_request()
        def do_POST(self):self.handle_request()
        def handle_request(self):
            if self.path.startswith('/cli/'):
                files={'/cli/host-manifest.json':a.bundle/'manifest.json','/cli/ow-linux-amd64':binary,'/cli/ow-linux-amd64.sha256':a.bundle/'ow-linux-amd64.sha256'}
                for name in ['firecracker','vmlinux','base.ext4','ow-guest','slirp4netns']:files['/cli/host/'+name]=a.bundle/name
                if self.path=='/cli/install.sh':
                    data=(Path(__file__).resolve().parents[2]/'crates/ow/cli/install.sh').read_bytes().replace(b'@@ORIGIN@@',f'https://localhost:{edge.server_port}'.encode())
                elif self.path in files:data=files[self.path].read_bytes()
                else:self.send_error(404);return
                self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data);return
            if not re.fullmatch(r'/(?:_nodes/)?(?:enroll|node/[A-Za-z0-9_-]+|job/[A-Za-z0-9_-]+/[A-Za-z0-9_-]+)',self.path):self.send_error(404);return
            if self.command=='POST':
                connection=http.client.HTTPSConnection('localhost',native_port,context=client_ctx,timeout=10)
                try:
                    n=int(self.headers.get('Content-Length','0'));assert n<=4096
                    connection.request('POST',self.path,body=self.rfile.read(n),headers={'Content-Type':'application/json'})
                    response=connection.getresponse();data=response.read(4097);assert len(data)<=4096
                    self.send_response(response.status);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
                except Exception as error:
                    (a.evidence/'edge-error.log').write_text(type(error).__name__+': '+str(error));self.send_error(502)
                finally:connection.close()
                return
            try:
                with client_ctx.wrap_socket(socket.create_connection(('127.0.0.1',native_port),timeout=5),server_hostname='localhost') as upstream:
                    n=int(self.headers.get('Content-Length','0'));assert n<=4096
                    body=self.rfile.read(n)
                    headers=''.join(f'{k}: {v}\r\n' for k,v in self.headers.items())
                    upstream.sendall(f'{self.command} {self.path} HTTP/1.1\r\n{headers}\r\n'.encode()+body)
                    data=b''
                    while b'\r\n\r\n' not in data:data+=upstream.recv(4096)
                    self.connection.sendall(data)
                    if b' 101 ' in data.split(b'\r\n',1)[0]:
                        sockets=[upstream,self.connection];upstream.settimeout(40);self.connection.settimeout(40)
                        while True:
                            ready=[upstream] if upstream.pending() else select.select(sockets,[],[],40)[0]
                            if not ready:break
                            for source in ready:
                                chunk=source.recv(65536)
                                if not chunk:return
                                (self.connection if source is upstream else upstream).sendall(chunk)
                    else:
                        # Enrollment response: finish bounded body, then close this HTTP connection.
                        head,content=data.split(b'\r\n\r\n',1);length=int(re.search(rb'content-length:\s*(\d+)',head,re.I)[1])
                        while len(content)<length:
                            chunk=upstream.recv(length-len(content));self.connection.sendall(chunk);content+=chunk
                    self.close_connection=True
            except (OSError,AssertionError):self.close_connection=True
    edge=http.server.ThreadingHTTPServer(('127.0.0.1',0),Edge);edge.daemon_threads=True
    server_ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);server_ctx.load_cert_chain(cert,server_key);edge.socket=server_ctx.wrap_socket(edge.socket,server_side=True)
    threading.Thread(target=edge.serve_forever,daemon=True).start()
    evidence={'home':str(home),'controller_root':str(controller),'cases':[]};joined=False
    def stop_owned(cli,env,label):
        result=subprocess.run([str(cli),'host','stop'],env=env,capture_output=True,text=True,timeout=20)
        (a.evidence/(label+'.log')).write_text(result.stdout+result.stderr)
        for _ in range(100):
            if not held(root/'worker.lock') and not held(root/'node-agent.lock') and not any(p['cwd'].startswith(str(root)+'/machines/') for p in proc_inventory()):break
            time.sleep(.1)
        else:raise RuntimeError('owned exit unconfirmed')
        assert result.returncode==0,('unmodified CLI stop failed',result.stderr)
        return {'code':result.returncode,'output':result.stdout,'error':result.stderr}
    try:
        for _ in range(50):
            try:
                with socket.create_connection(('127.0.0.1',native_port),timeout=.2):break
            except OSError:time.sleep(.1)
        tools=a.evidence/'tools';tools.mkdir();helper=tools/'cloudflared';helper.write_text('#!/bin/sh\nexit 0\n');helper.chmod(0o700)
        env=dict(os.environ,HOME=str(home),PATH=str(tools)+':/usr/bin:/bin',CURL_CA_BUNDLE=str(ca))
        install=(a.evidence/'install.sh');install.write_bytes(subprocess.check_output(['curl','--cacert',str(ca),'-fsS',f'https://localhost:{edge.server_port}/cli/install.sh']))
        subprocess.run(['sh',str(install)],env=env,check=True,stdout=(a.evidence/'install.log').open('wb'))
        cli=home/'.local/bin/ow';assert hashlib.sha256(cli.read_bytes()).digest()==hashlib.sha256(binary.read_bytes()).digest();evidence['installed_cli_sha256']=hashlib.sha256(cli.read_bytes()).hexdigest()
        invite=a.evidence/'invite.json'
        subprocess.run([str(binary),'--local','--data-dir',str(controller),'node-join','friend','--memory','512','--slots','2','--cpus','2','--controller',f'https://localhost:{edge.server_port}/_nodes','--output',str(invite)],check=True,stdout=subprocess.DEVNULL)
        invitation=invite.read_bytes().strip();master,slave=pty.openpty()
        join=subprocess.Popen([str(cli),'host','join','--ca-cert',str(ca)],env=env,stdin=slave,stdout=slave,stderr=slave);joined=True
        output=b''
        for prompt,answer in [(b'Paste invitation',invitation),(b'Type yes:',b'yes'),(b'Disjoint guest RAM',b'512'),(b'Running guest slots',b'2'),(b'Guest vCPU budget',b'2'),(b'Minimum free disk',b'2')]:
            end=time.monotonic()+30
            while prompt not in output:
                assert join.poll() is None,output.decode(errors='replace')
                assert time.monotonic()<end,('prompt timeout',output)
                if select.select([master],[],[],.1)[0]:output+=os.read(master,65536)
            os.write(master,answer+b'\n')
        end=time.monotonic()+60
        while join.poll() is None:
            assert time.monotonic()<end,'join timeout'
            if select.select([master],[],[],.1)[0]:output+=os.read(master,65536)
        while select.select([master],[],[],.05)[0]:
            try:output+=os.read(master,65536)
            except OSError:break
        assert join.returncode==0,output.decode(errors='replace');assert json.loads(invitation)['secret'].encode() not in output
        (a.evidence/'guided-join.log').write_bytes(output);os.close(master);os.close(slave)
        status=wait_ready(cli,home,root);evidence['ready']=status;evidence['cases'].append('clean user install + hidden guided join + real prefixed TLS acknowledgement')
        proxy=controller/'nodes/friend';rpc(proxy,{'op':'create','id':'smoke','image':'alpine','memory_mib':256,'vcpu_count':1,'operation_key':'smoke-create'})
        out=rpc(proxy,{'op':'exec','id':'smoke','command':'uname -s; echo host-cold-persist > /root/host-smoke'})
        assert 'Linux' in out['output'];evidence['cases'].append('real 256 MiB Alpine create/exec through outbound node proxy')
        rpc(proxy,{'op':'stop','id':'smoke'});rpc(proxy,{'op':'start','id':'smoke'})
        out=rpc(proxy,{'op':'exec','id':'smoke','command':'cat /root/host-smoke'});assert 'host-cold-persist' in out['output'];evidence['cases'].append('cold guest stop/start disk persistence')
        evidence['first_stop']=stop_owned(cli,env,'stop-first')
        assert not json.loads(subprocess.check_output([str(cli),'host','status'],env=env))['controller_dispatchable']
        # Scoped fixture re-point exercises unchanged native aliases with the same credential.
        config=json.loads((root/'host.json').read_text());config['controller']=f'https://localhost:{native_port}'
        (root/'host.json').write_text(json.dumps(config));ca.rename(a.evidence/'original-ca-removed.pem')
        subprocess.run([str(cli),'host','start'],env=dict(os.environ,HOME=str(home)),check=True,stdout=(a.evidence/'restart-native.log').open('wb'))
        wait_ready(cli,home,root);rpc(proxy,{'op':'start','id':'smoke'})
        assert 'host-cold-persist' in rpc(proxy,{'op':'exec','id':'smoke','command':'cat /root/host-smoke'})['output'];evidence['cases'].append('managed stop/start + saved CA after original removed + native TLS path + disk persistence')
        evidence['passed']=not evidence.get('limitations')
    finally:
        if joined and (root/'host.json').exists():
            evidence['cleanup_stop']=stop_owned(home/'.local/bin/ow',dict(os.environ,HOME=str(home)),'stop-final')
        child.terminate();child.wait(timeout=10);edge.shutdown();edge.server_close();log.close()
        after=inventory(a.data);evidence['ubuntu_identity_preserved']=before['vmm_processes']==after['vmm_processes'];assert evidence['ubuntu_identity_preserved']
        evidence['cleanup_positive']=True;evidence['after']=after
        (a.evidence/'receipt.json').write_text(json.dumps(evidence,indent=2))
    print(json.dumps({'passed':evidence.get('passed',False),'cleanup_positive':True,'receipt':str(a.evidence/'receipt.json')}))
if __name__=='__main__':main()
