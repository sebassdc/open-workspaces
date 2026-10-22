#!/usr/bin/env python3
"""Build the dashboard UI bundle (docs/DASHBOARD.md#ui-bundles).

Writes ow-dashboard-ui-<version>.tar.gz and its .sha256 into --out. The tarball holds
manifest.json and the files the controller serves; the same sources give the same bytes.
Version and API range come from <source>/ui.json unless overridden (tests use that).
"""
import argparse
import gzip
import hashlib
import io
import json
import pathlib
import re
import tarfile

FILES = ["index.html", "app.js", "style.css", "vendor/xterm.js", "vendor/addon-fit.js", "vendor/xterm.css"]


def build(source, out, version=None, api=None):
    info = json.loads((source / "ui.json").read_text())
    version = version or info["version"]
    api = api or (info["api"]["min"], info["api"]["max"])
    if not re.fullmatch(r"[0-9A-Za-z_-][0-9A-Za-z._-]{0,63}", version) or version == "builtin":
        raise SystemExit(f"invalid UI bundle version: {version}")
    files = {name: (source / name).read_bytes() for name in FILES}
    manifest = {
        "format": 1,
        "name": "ow-dashboard-ui",
        "version": version,
        "api": {"min": api[0], "max": api[1]},
        "files": {name: hashlib.sha256(body).hexdigest() for name, body in files.items()},
    }
    members = {"manifest.json": (json.dumps(manifest, indent=2) + "\n").encode(), **files}
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name in sorted(members):
            entry = tarfile.TarInfo(name)
            entry.size, entry.mode, entry.mtime = len(members[name]), 0o644, 0
            tar.addfile(entry, io.BytesIO(members[name]))
    out.mkdir(parents=True, exist_ok=True)
    name = f"ow-dashboard-ui-{version}.tar.gz"
    target = out / name
    with open(target, "wb") as handle, gzip.GzipFile(filename="", mode="wb", fileobj=handle, mtime=0) as gz:
        gz.write(raw.getvalue())
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    (out / f"{name}.sha256").write_text(f"{digest}  {name}\n")
    print(target)
    return target


if __name__ == "__main__":
    repo = pathlib.Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--source", type=pathlib.Path, default=repo / "crates/ow/ui")
    parser.add_argument("--version", help="override ui.json version")
    parser.add_argument("--api", type=int, nargs=2, metavar=("MIN", "MAX"), help="override ui.json API range")
    args = parser.parse_args()
    build(args.source, args.out, args.version, args.api)
