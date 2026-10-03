#!/usr/bin/env python3
"""Build developer templates using only isolated project-owned microVMs."""
import argparse,hashlib,json,os,subprocess,tempfile,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[2]
ASSETS=REPO/'data/runtime-spike'
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('profile',choices=['ubuntu','arch'])
parser.add_argument('--adopt-build',type=Path,help='Resume this project build root after successful provisioning')
args=parser.parse_args()
profile=args.profile
root=args.adopt_build.resolve() if args.adopt_build else Path(tempfile.mkdtemp(prefix='build-',dir=REPO/'data/dev-images')).resolve()
id=profile+'-dev-build'
cli=[str(REPO/'ow'),'--local','--data-dir',str(root)]
def run(*cmd):
    r=subprocess.run(cli+list(cmd),capture_output=True,text=True,timeout=150)
    if r.returncode:raise RuntimeError((cmd,r.stdout,r.stderr))
    return r.stdout
def guest(command):return run('exec',id,'--',command)
def fsck(disk,discard=False):
    r=subprocess.run(['e2fsck','-fy']+(['-E','discard'] if discard else [])+[str(disk)],capture_output=True,text=True)
    if r.returncode not in [0,1]:raise RuntimeError(r.stdout+r.stderr)
try:
    if not args.adopt_build:
        run('up');run('create',id,'--image',profile,'--memory','1024');run('stop',id)
        disk=root/'machines'/id/'disk.ext4'
        with disk.open('r+b') as stream:stream.truncate(8*1024**3)
        fsck(disk);subprocess.run(['resize2fs',str(disk)],check=True,capture_output=True);run('start',id)
        run('put',id,str(REPO/'experiments/guest-dev/provision.sh'),'/run/provision-dev.sh')
        guest(f"/bin/bash -c 'if [ $(id -u) = 0 ]; then bash /run/provision-dev.sh {profile}; else sudo -n bash /run/provision-dev.sh {profile}; fi >/persist/provision.log 2>&1; echo $? > /persist/provision.exit' </dev/null >/dev/null 2>&1 &")
        deadline=time.monotonic()+1800
        while guest('test ! -f /persist/provision.exit || cat /persist/provision.exit').strip()=='':
            if time.monotonic()>deadline:raise RuntimeError('guest provision timed out; build data retained')
            time.sleep(5)
    else:
        # Adoption must be explicit and belongs to an existing project-marked worker root.
        assert root.is_relative_to(REPO/'data/dev-images') and (root/'.ow-data').exists()
        disk=root/'machines'/id/'disk.ext4'
    assert guest('cat /persist/provision.exit').strip()=='0','provisioning failed; inspect retained guest log'
    # All privileged cleanup is confined to the guest. Works with old root exec and new dev exec.
    def privileged(command):
        return guest('if [ $(id -u) = 0 ]; then /bin/bash -c '+__import__('shlex').quote(command)+'; else sudo -n /bin/bash -c '+__import__('shlex').quote(command)+'; fi')
    checks=privileged("su - dev -c 'sudo -n true; git --version; curl --version | head -1; nvim --version | head -1; gcc --version | head -1; mise exec -- node --version; mise exec -- python --version; mise exec -- cargo --version; mise exec -- rustc --version'")
    toolchains=json.loads(privileged('cat /etc/ow-dev/toolchains.json'))
    packages=privileged('cat /etc/ow-dev/packages.txt')
    (root/'packages.txt').write_text(packages);(root/'toolchains.json').write_text(json.dumps(toolchains,indent=2))
    privileged('rm -f /persist/provision.log /persist/provision.exit /etc/machine-id /etc/ssh/ssh_host_* /root/.bash_history /home/dev/.bash_history; sync')
    run('stop',id);fsck(disk,discard=True)
    image=ASSETS/'guest'/f'{profile}.ext4'
    backup=image.with_name(profile+'.minimal.ext4')
    if not backup.exists():subprocess.run(['cp','--reflink=always',str(image),str(backup)],check=True)
    original=json.loads(image.with_suffix('.json').read_text())
    manifest_backup=image.with_name(profile+'.minimal.json')
    if not manifest_backup.exists():manifest_backup.write_text(json.dumps(original,indent=2))
    temporary=image.with_suffix('.developer.tmp')
    subprocess.run(['e2image','-ra',str(disk),str(temporary)],check=True,capture_output=True)
    check=subprocess.run(['e2fsck','-fn',str(temporary)],capture_output=True,text=True)
    if check.returncode:raise RuntimeError('Sparse template verification failed: '+check.stdout+check.stderr)
    with temporary.open('rb') as stream:digest=hashlib.file_digest(stream,'sha256').hexdigest()
    manifest={**original,'revision':'developer-v1','disk_mib':8192,'image_sha256':digest,'mise':'2026.10.0','toolchains':toolchains,'package_inventory_sha256':hashlib.sha256(packages.encode()).hexdigest(),'developer_user':'dev','sudo':'guest-only passwordless','build_date_utc':time.strftime('%Y-%m-%d',time.gmtime()),'physical_allocated_bytes':temporary.stat().st_blocks*512,'validation':checks}
    manifest_tmp=image.with_suffix('.json.tmp');manifest_tmp.write_text(json.dumps(manifest,indent=2)+'\n')
    os.replace(temporary,image);os.replace(manifest_tmp,image.with_suffix('.json'))
    (root/'result.json').write_text(json.dumps({'passed':True,'profile':profile,'image_sha256':digest,'checks':checks},indent=2))
    print(json.dumps({'profile':profile,'revision':'developer-v1','disk_mib':8192,'allocated_mib':manifest['physical_allocated_bytes']/1024**2,'checks':checks},indent=2))
finally:
    run('down')
