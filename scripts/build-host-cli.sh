#!/usr/bin/env bash
# Build reviewed source in explicitly marked isolated storage; never deploy.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
: "${CARGO_HOME:?select installed toolchain storage}"
: "${RUSTUP_HOME:?select installed toolchain storage}"
: "${OW_HOST_BUILD_ROOT:?select dedicated marked build root}"
: "${CARGO_TARGET_DIR:?select isolated target directory}"
: "${OW_CROSS_BUILD_ROOT:?select verified Zig build tools}"
: "${OW_HOST_BUILD_OUTPUT:?select new private artifact output directory}"
python3 - <<'PYBUILD'
import os,stat,hashlib,tarfile
from pathlib import Path
root=Path(os.environ['OW_HOST_BUILD_ROOT']);target=Path(os.environ['CARGO_TARGET_DIR']);out=Path(os.environ['OW_HOST_BUILD_OUTPUT']);cross=Path(os.environ['OW_CROSS_BUILD_ROOT'])
for path in [root,target,out,cross]:
    if not path.is_absolute() or any(p.is_symlink() for p in [path,*path.parents]):raise SystemExit('Absolute non-symlink build paths required')
if not root.is_dir() or (root/'.ow-host-build').read_text()!='open-workspaces dedicated host build v1\n':raise SystemExit('Dedicated build root marker required')
if root.stat().st_uid!=os.getuid() or root.stat().st_mode&0o077:raise SystemExit('Build root must be owned/private')
if not target.is_relative_to(root) or not out.is_relative_to(root) or target==root or out==root or out.is_relative_to(target) or target.is_relative_to(out):raise SystemExit('Separate target/output must stay inside dedicated build root')
if out.exists():raise SystemExit('Output exists; preserve it and select a new output')
archive=cross/'zig-0.15.2.tar.xz'
with archive.open('rb') as f:
    if hashlib.file_digest(f,'sha256').hexdigest()!='02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239':raise SystemExit('Zig archive pin mismatch')
with tarfile.open(archive) as t:
    data=t.extractfile('zig-x86_64-linux-0.15.2/zig').read()
if hashlib.sha256(data).digest()!=hashlib.sha256((cross/'zig-x86_64-linux-0.15.2/zig').read_bytes()).digest():raise SystemExit('Installed Zig does not match pinned archive')
out.parent.mkdir(parents=True,exist_ok=True)
PYBUILD
cd "$repo_root"
umask 077
mkdir -p "$CARGO_TARGET_DIR"
export OW_ZIG_CACHE_DIR="$CARGO_TARGET_DIR/zig-cache"
export CC_x86_64_unknown_linux_musl="$repo_root/scripts/musl-cc.sh"
source_record=$(mktemp "$CARGO_TARGET_DIR/source-XXXXXX.json")
staging=$(mktemp -d "$(dirname "$OW_HOST_BUILD_OUTPUT")/host-build-XXXXXX")
trap 'rm -f "$source_record"; rm -rf "$staging"' EXIT
python3 - "$source_record" <<'PYBUILD'
import hashlib,json,sys
from pathlib import Path
paths=[Path('Cargo.toml'),Path('Cargo.lock'),*Path('crates').rglob('*.rs'),*Path('crates').rglob('Cargo.toml'),*Path('crates/ow/ui').rglob('*'),*Path('crates/ow/cli').rglob('*')]
source={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(set(paths)) if p.is_file()}
Path(sys.argv[1]).write_text(json.dumps(source,sort_keys=True))
PYBUILD
export OW_BUILD_SOURCE_SHA256=$(sha256sum "$source_record" | cut -d ' ' -f 1)
"$CARGO_HOME/bin/cargo" +1.97.0 build --locked --release -p ow -p ow-guest --target x86_64-unknown-linux-musl -j 2
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/ow" "$staging/ow-linux-amd64"
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/ow-guest" "$staging/ow-guest"
"$staging/ow-linux-amd64" host join --help > "$staging/host-join-help.txt"
compiler=$("$CARGO_HOME/bin/rustc" +1.97.0 --version)
zig_version=$("$OW_CROSS_BUILD_ROOT/zig-x86_64-linux-0.15.2/zig" version)
python3 - "$source_record" "$staging" "$OW_HOST_BUILD_OUTPUT" "$compiler" "$zig_version" <<'PYBUILD'
import hashlib,json,sys,os
from pathlib import Path
source=json.loads(Path(sys.argv[1]).read_text());stage=Path(sys.argv[2]);out=Path(sys.argv[3])
for path,expected in source.items():
    if hashlib.sha256(Path(path).read_bytes()).hexdigest()!=expected:raise SystemExit('Source changed during build; no publication')
def digest(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
cli=digest(stage/'ow-linux-amd64');guest=digest(stage/'ow-guest')
(stage/'ow-linux-amd64.sha256').write_text(cli+'\n')
(stage/'build.json').write_text(json.dumps({'source_sha256':digest(Path(sys.argv[1])),'source':source,'cli_sha256':cli,'guest_sha256':guest,'compiler':sys.argv[4],'zig':sys.argv[5],'target':'x86_64-unknown-linux-musl','profile':'release'},sort_keys=True))
for p in stage.iterdir():
    with p.open('rb') as f:os.fsync(f.fileno())
os.rename(stage,out)
fd=os.open(out.parent,os.O_DIRECTORY)
try:os.fsync(fd)
finally:os.close(fd)
print('Private release pair staged atomically with source/tool/binary provenance; no deployment.')
PYBUILD
