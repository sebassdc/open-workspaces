#!/usr/bin/env python3
"""Verify developer templates in real microVMs; never modifies existing workspaces."""
import json, subprocess, tempfile, time
from pathlib import Path
repo=Path(__file__).resolve().parents[2]
root=Path(tempfile.mkdtemp(prefix='dev-test-',dir=repo/'data'))
cli=[str(repo/'ow'),'--local','--data-dir',str(root)]
checks=[]
def run(*args):
    p=subprocess.run(cli+list(args),text=True,capture_output=True,timeout=150)
    assert p.returncode==0,(args,p.stdout,p.stderr)
    return p.stdout

def guest(id,cmd):return run('exec',id,'--',cmd)
try:
    run('up')
    for image in ['ubuntu','arch']:
        run('create',image,'--image',image,'--memory','1024')
        assert guest(image,'id -u').strip()=='1000'
        assert guest(image,'pwd').strip()=='/home/dev'
        guest(image,'sudo -n true; test -w /persist; test ! -e /root/.cloudflared; test ! -e /var/run/docker.sock')
        versions=guest(image,'git --version; curl --version | head -1; nvim --version | head -1; gcc --version | head -1; mise --version; node --version; python --version; cargo --version; rustc --version')
        (root/(image+'-versions.txt')).write_text(versions)
        # Signed distribution repositories, tested as an ordinary developer using guest sudo.
        install='sudo -n apt-get update && sudo -n apt-get install -y --no-install-recommends tree' if image=='ubuntu' else 'sudo -n pacman -Syu --noconfirm tree'
        script=root/'packages.sh';script.write_text('#!/bin/bash\n'+install+'\nresult=$?\necho "$result" > /persist/packages.exit\n')
        run('put',image,str(script),'/persist/packages.sh')
        assert guest(image,'stat -c %u /persist/packages.sh').strip()=='1000'
        guest(image,'bash /persist/packages.sh >/persist/packages.log 2>&1 </dev/null &')
        deadline=time.monotonic()+180
        while guest(image,'test ! -f /persist/packages.exit || cat /persist/packages.exit').strip()=='':
            assert time.monotonic()<deadline,'package install timeout'
            time.sleep(2)
        assert guest(image,'cat /persist/packages.exit').strip()=='0',guest(image,'tail -30 /persist/packages.log')
        guest(image,"tree --version; mkdir -p ~/smoke; cd ~/smoke; printf '#include <stdio.h>\nint main(){puts(\"C_OK\");}\n' > main.c; gcc main.c -o c-smoke; ./c-smoke | grep C_OK; git init -q; python -m venv .venv; .venv/bin/python -c 'print(\"PY_OK\")'; node -e 'console.log(\"NODE_OK\")'; cargo new --vcs none rust-smoke; cd rust-smoke; cargo run --offline; cargo fmt --check; cargo clippy --offline; nvim --headless -u NONE '+q'")
        checks.append(image+': dev UID, sudo, package repositories, uploaded-file ownership, C/Node/Python venv/Rust/fmt/clippy/Git/Neovim')
        # Preserve process state as well as disk state, and fork without modifying the parent.
        guest(image,"python -c 'import socket;s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind((\"127.0.0.1\",8080));s.listen();n=0\nwhile True:\n c,a=s.accept();c.recv(4096);n+=1;b=str(n).encode();c.sendall(b\"HTTP/1.1 200 OK\\r\\nContent-Length: \"+str(len(b)).encode()+b\"\\r\\n\\r\\n\"+b);c.close()' >/persist/counter.log 2>&1 </dev/null &")
        guest(image,'echo captured > /persist/state; sleep .2')
        assert guest(image,'curl -fsS http://127.0.0.1:8080').strip()=='1'
        snap=image+'-dev-checkpoint';run('snapshot',image,snap)
        guest(image,'echo newer > /persist/state')
        child=image+'-child';run('fork',image,child,'--snapshot',snap)
        assert guest(child,'id -u; cat /persist/state; curl -fsS http://127.0.0.1:8080').strip().splitlines()==['1000','captured','2']
        guest(child,'echo independent > /persist/state')
        assert guest(image,'cat /persist/state').strip()=='newer'
        run('hibernate',image);run('start',image)
        assert guest(image,'curl -fsS http://127.0.0.1:8080').strip()=='2'
        run('restore',image,snap)
        assert guest(image,'cat /persist/state; curl -fsS http://127.0.0.1:8080').strip().splitlines()==['captured','2']
        run('stop',image);run('start',image)
        cold=guest(image,'id -u; cat /persist/state').strip().splitlines(); assert cold==['1000','captured'], repr(cold)
        run('stop',image);run('stop',child)
        checks.append(image+': paired RAM/disk snapshot, independent fork, hibernate, restore, cold-start persistence')
        print(checks[-2:],flush=True)
    (root/'result.json').write_text(json.dumps({'passed':True,'checks':checks},indent=2)+'\n')
    print('Artifacts:',root,flush=True)
finally:
    run('down')
