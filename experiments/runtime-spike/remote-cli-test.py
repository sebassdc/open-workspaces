#!/usr/bin/env python3
"""Remote CLI against test TLS edge + real worker. No production auth bypass."""
import fcntl,json,os,pty,select,struct,subprocess,sys,termios,time
from pathlib import Path
input_path=Path(sys.argv[1]);data=json.loads(input_path.read_text());repo=Path(__file__).resolve().parents[2]
provider=Path(data['artifacts'])/'test-token-provider'
provider.write_text('#!/usr/bin/env python3\nimport json,os\nprint(json.load(open(os.environ["OW_TEST_INPUT"]))["assertion"])\n');provider.chmod(0o700)
env=dict(os.environ,OW_TEST_INPUT=str(input_path),OW_CLOUDFLARED=str(provider),SSL_CERT_FILE=sys.argv[2],SSL_CERT_DIR=str(provider.parent/'no-cert-dir'))
cli=[str(repo/'target/release/ow'),'--server',data['url']]
r=subprocess.run(cli+['list'],env=env,capture_output=True,text=True,timeout=10)
assert r.returncode==0,(r.stdout,r.stderr)
assert any(w['id']=='browser-parent' for w in json.loads(r.stdout))
r=subprocess.run(cli+['exec','browser-parent','--','printf remote-exec; exit 7'],env=env,capture_output=True,text=True,timeout=10)
assert r.returncode==7 and r.stdout.strip()=='remote-exec',(r.stdout,r.stderr,r.returncode)
# A valid Access assertion must not bypass server certificate verification.
untrusted=dict(env);empty=provider.parent/'empty-ca.pem';empty.write_text('');untrusted['SSL_CERT_FILE']=str(empty)
r=subprocess.run(cli+['list'],env=untrusted,capture_output=True,text=True,timeout=10)
assert r.returncode!=0,'untrusted TLS certificate accepted'
master,slave=pty.openpty();original=termios.tcgetattr(slave)
fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',24,80,0,0))
p=subprocess.Popen(cli+['shell','browser-parent'],env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
output=bytearray()
def wait(marker,timeout=8):
    deadline=time.monotonic()+timeout
    while marker not in output:
        if time.monotonic()>deadline:raise AssertionError(output.decode(errors='replace'))
        if select.select([master],[],[],.1)[0]:output.extend(os.read(master,65536))
try:
    wait(b'#');os.write(master,b"test -t 0 && printf 'REMOTE_%s\\n' 'PTY'\r");wait(b'REMOTE_PTY')
    fcntl.ioctl(master,termios.TIOCSWINSZ,struct.pack('HHHH',37,111,0,0));time.sleep(.25)
    os.write(master,b'stty size\r');wait(b'37 111')
    os.write(master,b'sleep 30\r');time.sleep(.2);os.write(master,b"\x03printf 'REMOTE_%s\\n' 'INTERRUPTED'\r");wait(b'REMOTE_INTERRUPTED',3)
    os.write(master,b'exit 42\r');assert p.wait(timeout=5)==42
    assert termios.tcgetattr(slave)==original
    print('Remote CLI HTTPS API + verified TLS + WSS PTY/resize/Ctrl-C/exit 42: passed')
finally:
    if p.poll() is None:p.terminate();p.wait(timeout=5)
    os.close(master);os.close(slave)
