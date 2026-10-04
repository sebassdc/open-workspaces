#!/usr/bin/env python3
"""Bounded single-run codec comparison; never reads live guest disks."""
import os,resource,subprocess,time,json,hashlib
from pathlib import Path
os.nice(10)
os.sched_setaffinity(0,sorted(os.sched_getaffinity(0))[:2])
resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
import argparse
a=argparse.ArgumentParser(); a.add_argument('--image',type=Path,required=True); a.add_argument('--evidence',type=Path,required=True); args=a.parse_args()
p=args.image;out=args.evidence;out.mkdir(parents=True,exist_ok=False);corpus=out/'corpus.bin'
with p.open('rb') as f,corpus.open('wb') as w:
 w.write(f.read(128<<20))
 for i in range(16):
  f.seek((p.stat().st_size-(8<<20))*i//15); w.write(f.read(8<<20))
results=[]
for codec,level in [('zstd',9),('zstd',15),('zstd',19),('xz',6)]:
 dest=out/f'corpus.{codec}{level}'
 cmd=([codec,f'-{level}','-T2','--memory=1024MB','-f',str(corpus),'-o',str(dest)] if codec=='zstd' else [codec,f'-{level}','-T2','--memlimit=1536MiB','-c',str(corpus)])
 t=time.monotonic()
 with dest.open('wb') as w:
  r=subprocess.run(cmd,stdout=w if codec=='xz' else subprocess.DEVNULL,stderr=subprocess.PIPE)
 if r.returncode: raise RuntimeError(r.stderr)
 enc=time.monotonic()-t; t=time.monotonic()
 r=subprocess.run([codec,'-d','-c',str(dest)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=True)
 results.append(dict(codec=codec,level=level,bytes=dest.stat().st_size,encode_seconds=enc,decode_seconds=time.monotonic()-t))
 (out/'benchmark.json').write_text(json.dumps(dict(corpus_bytes=corpus.stat().st_size,corpus_sha256=hashlib.file_digest(corpus.open('rb'),'sha256').hexdigest(),method='first128MiB +16 evenly spaced8MiB; single run; nice10; affinity2; RLIMIT_AS2GiB',results=results),indent=2))
 print(results[-1],flush=True)
