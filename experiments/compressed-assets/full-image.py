#!/usr/bin/env python3
"""Full pristine pinned image compression and hash acceptance; single run."""
import os,resource,subprocess,time,json,hashlib,argparse
os.nice(10)
os.sched_setaffinity(0,sorted(os.sched_getaffinity(0))[:2])
resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
from pathlib import Path
a=argparse.ArgumentParser();a.add_argument('--image',type=Path,required=True);a.add_argument('--evidence',type=Path,required=True);args=a.parse_args();p=args.image;out=args.evidence;out.mkdir(parents=True,exist_ok=False)
t=time.monotonic(); raw=hashlib.file_digest(p.open('rb'),'sha256').hexdigest(); hash_seconds=time.monotonic()-t
assert raw=='87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b'
t=time.monotonic(); subprocess.run(['zstd','-19','-T2','--memory=1024MB','-f',str(p),'-o',str(out/'ubuntu.ext4.zst')],check=True); enc=time.monotonic()-t
t=time.monotonic(); proc=subprocess.Popen(['zstd','-d','-c',str(out/'ubuntu.ext4.zst')],stdout=subprocess.PIPE); h=hashlib.sha256(); size=0
while b:=proc.stdout.read(1<<20): h.update(b);size+=len(b)
assert proc.wait()==0 and h.hexdigest()==raw and size==p.stat().st_size
result=dict(original_bytes=size,original_sha256=raw,original_hash_seconds=hash_seconds,encoded_bytes=(out/'ubuntu.ext4.zst').stat().st_size,encoded_sha256=hashlib.file_digest((out/'ubuntu.ext4.zst').open('rb'),'sha256').hexdigest(),encode_seconds=enc,decode_hash_and_encoded_hash_seconds=time.monotonic()-t,level=19,threads=2,memory_limit_bytes=2<<30,nice=10,method='single run; installed zstd1.5.7; full pristine bytes; decode pipe64KiB/1MiB hash blocks; no VM')
(out/'full.json').write_text(json.dumps(result,indent=2));print(result)
