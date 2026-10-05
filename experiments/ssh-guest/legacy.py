#!/usr/bin/env python3
"""One fresh <=256MiB Alpine regression; needs an exclusive planner host turn."""
import os,sys,json,subprocess,hashlib,time
from pathlib import Path
root=Path(sys.argv[1]).resolve();assert not root.exists();root.mkdir(parents=True,mode=0o700)
binary=Path(os.environ['OW_SSH_TEST_BINARY']).resolve();source=Path(os.environ['OW_SSH_TEST_ASSETS']).resolve()
def vmm():
 out={}
 for p in Path('/proc').iterdir():
  try:
   if 'firecracker' in (p/'comm').read_text():out[p.name]=(p/'stat').read_text().split(') ')[1].split()[19]
  except (FileNotFoundError,PermissionError,NotADirectoryError):pass
 return out
baseline=vmm();available=int(next(l.split()[1] for l in Path('/proc/meminfo').read_text().splitlines() if l.startswith('MemAvailable:')));assert available>8*1024**2
assets=root/'assets';(assets/'guest').mkdir(parents=True)
for name in ['official','downloads','bin']:(assets/name).symlink_to(source/name,target_is_directory=True)
subprocess.run(['cp','--reflink=always',str(source/'guest/base.ext4'),str(assets/'guest/base.ext4')],check=True)
env={**os.environ,'OW_ASSET_DIR':str(assets),'OW_MAX_MEMORY_MIB':'1024','OW_MAX_RUNNING':'1','OW_MAX_VCPUS':'2','OW_MIN_FREE_GIB':'8'}
worker=root/'w';checks=[];clean=False
owner=Path('/home/sebassdc/dev/open-workspaces/data/prototype/machines/ubuntu-dev/disk.ext4');inode=owner.stat().st_ino
log=(root/'commands.log').open('w')
def run(*args,ok=True):
 log.write(repr(args)+'\n');log.flush();r=subprocess.run([str(binary),'--local','--data-dir',str(worker),*args],env=env,capture_output=True,timeout=60)
 if ok:assert r.returncode==0,(args,r.stderr)
 return r
try:
 run('up');run('create','legacy','--memory','256','--cpus','1')
 cmd="test ! -e /etc/ow-ssh-v1 && test ! -e /etc/ow-ssh/authorized_keys && printf LEGACY && exit 19"
 r=run('exec','legacy','--',cmd,ok=False);assert r.returncode==19 and r.stdout.strip()==b'LEGACY';checks.append('legacy Alpine management exec/exit19, no SSH provisioning')
 r=run('ssh-proxy','legacy',ok=False);assert r.returncode and not r.stdout;checks.append('unenrolled legacy guest SSH denied')
 run('snapshot','legacy','saved');run('stop','legacy');run('fork','legacy','child','--snapshot','saved')
 r=run('exec','child','--',cmd,ok=False);assert r.returncode==19 and r.stdout.strip()==b'LEGACY';checks.append('sequential legacy fork retains no SSH marker/authorization')
 run('stop','child');run('restore','legacy','saved');r=run('exec','legacy','--',cmd,ok=False);assert r.returncode==19 and r.stdout.strip()==b'LEGACY';checks.append('legacy restore management remains reliable')
 run('stop','legacy');run('start','legacy');r=run('exec','legacy','--',cmd,ok=False);assert r.returncode==19 and r.stdout.strip()==b'LEGACY';checks.append('legacy cold boot remains reliable')
finally:
 run('down');time.sleep(.2);clean=vmm()==baseline and owner.stat().st_ino==inode and not (worker/'control.sock').exists()
 result={'checks':checks,'cleanup_and_owner_unchanged':clean,'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()};(root/'result.json').write_text(json.dumps(result,indent=2));log.close();assert clean,result
print(json.dumps(result,indent=2))
