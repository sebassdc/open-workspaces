"""In-place upgrades for ow-host (docs/UPGRADES.md): fetch, plan, stage, gate,
then a journaled state machine per host with health checks, abort before the
switch and rollback after it.

The planner (`plan`) is pure: the class a preview shows is the class the host runs.
"""
import contextlib
import fcntl
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

import ow_host as h
from ow_host import Refused, p, say

JOURNAL = f"{h.DATA}/upgrade.json"
LOG = "/var/log/open-workspaces-stage.log"
LOCK = f"{h.DATA}/upgrade.lock"
# The pin (unit) each change moves; host-layout moves the units whose rendered files differ.
MOVES = {"dashboard": "dashboard", "controller": "agent", "agent": "agent",
         "worker": "worker", "vmm": "worker", "guest-kernel": "worker"}
PIN_CLASS = {"worker": 2, "agent": 1, "dashboard": 1}
BEFORE_SWITCH = ("catalog-backup", "drain", "hibernate", "stop", "sync-images")
AFTER_SWITCH = ("switch", "start", "verify")
TERMINAL = ("done", "aborted", "rolled-back", "failed")


def version(text):
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", str(text))
    if not match:
        raise Refused(f"release version {text!r} is not X.Y.Z")
    return tuple(int(n) for n in match.groups())


# ---------------------------------------------------------------- planner


def gate(rule, captures):
    """The captures the target worker cannot resume (they would boot cold and be deleted);
    refused unless the rule accepts cold boots."""
    cold = sorted(c["name"] for c in captures if not (
        c.get("runtime") in rule["resumes"] and c.get("cpu") == rule["cpu"]
        and c.get("kernel_sha256") == rule["kernel_sha256"]))
    if cold and not rule["accept_cold"]:
        raise Refused(f"{rule['target']} cannot resume {', '.join(cold)} (another runtime, CPU or guest "
                      "kernel): their processes would be lost. Pass --accept-cold to boot them cold")
    return cold


def plan(manifest, pins, target, *, rendered=(), pending=(), runners=(), captures=(), compat=None,
         accept_cold=False, max_class=2, controller=None, installed=None):
    """The upgrade of one host from `pins` (pin -> release version) to version `target`.

    rendered: pins whose unit files the target release renders differently.
    pending:  pins to restart for a config change (their release stays).
    runners:  other runners of this controller, as {"id", "protocol"}.
    captures: every capture the worker could resume later, as {"name", "runtime", "cpu",
              "kernel_sha256"}: hibernated boxes, named snapshots, and running boxes
              (which will be captured with the old worker's tuple).
    compat:   the running worker's tuple ({"cpu": ...}): the CPU string comes from the host.
    controller: on a runner host, the release its controller runs: the new agent must speak
              a protocol that controller accepts (controllers upgrade first).
    installed: optional installed unit pins; dormant pins still move, but do not restart.
    Returns the plan; raises Refused, naming what to do instead."""
    entries = {e["version"]: e for e in manifest["releases"]}
    if target not in entries:
        raise Refused(f"the target's release-impact.json has no entry for {target}")
    goal = entries[target]
    for name, current in pins.items():
        if current not in entries:
            raise Refused(f"{name} runs {current}, which the target's release-impact.json does not list")
    oldest = min(pins.values(), key=version)

    def crossed(current):
        lo, hi = sorted((version(current), version(target)))
        return [e for e in entries.values() if lo < version(e["version"]) <= hi]

    for e in crossed(oldest):
        if "stage0" in e["changes"] or e.get("in_place_from") is None or version(e["in_place_from"]) > min(
                version(oldest), version(target)):
            raise Refused(f"{e['version']} cannot be applied in place from {oldest}: it needs a host "
                          "replacement (hibernate the boxes, then pulumi up)")
    move = {}
    for name, current in pins.items():
        changes = {c for e in crossed(current) for c in e["changes"]}
        if any(MOVES.get(c) == name for c in changes) or name in rendered:
            move[name] = target
    plan_ = {"from": dict(pins), "to": target, "move": move, "catalog_backup": False, "rollback_auto": True,
             "cold": []}
    # Catalog: the controller and dashboard move together around a schema change, after a copy.
    # A runner host has no catalog.
    schema = {name: entries[pins[name]]["catalog"]["schema"] for name in ("agent", "dashboard") if name in pins}
    if controller is None and any(s != goal["catalog"]["schema"] for s in schema.values()):
        plan_["catalog_backup"] = True
        plan_["rollback_auto"] = min(schema.values()) >= goal["catalog"]["min_reader"]
        move.update({name: target for name in schema})
    # Agent and worker: the pair each ends on must be inside the agent's worker window;
    # if moving one alone breaks that, the other moves too.
    def paired():
        agent = entries[move.get("agent", pins["agent"])]["worker_protocol"]["accepts"]
        speaks = entries[move.get("worker", pins["worker"])]["worker_protocol"]["speaks"]
        return agent[0] <= speaks <= agent[1]

    if "worker" in pins and not paired():
        move["worker" if "agent" in move else "agent"] = target
        if not paired():
            raise Refused(f"the {target} agent cannot drive the {target} worker (worker protocol window)")
    # A runner's new agent must be one its controller accepts; else it would sit offline.
    if controller is not None and "agent" in move:
        if controller not in entries:
            raise Refused(f"the controller runs {controller}, which the target's release-impact.json does not "
                          f"list: upgrade this runner to {controller} instead")
        lo, hi = entries[controller]["protocol"]["accepts"]
        if not lo <= goal["protocol"]["speaks"] <= hi:
            raise Refused(f"the controller ({controller}) accepts protocol {lo}..{hi}, and {target} speaks "
                          f"{goal['protocol']['speaks']}: upgrade the controller first")
    # Protocol: every other runner must speak what the new controller accepts.
    if controller is None and "agent" in move:
        lo, hi = goal["protocol"]["accepts"]
        low = sorted(r["id"] for r in runners if not lo <= r.get("protocol", 0) <= hi)
        if low:
            now = entries[pins["agent"]]["protocol"]["accepts"]
            bridges = [e for e in entries.values() if now[0] <= e["protocol"]["speaks"] <= now[1]
                       and lo <= e["protocol"]["speaks"] <= hi]
            if not bridges:
                raise Refused(f"runners {', '.join(low)} speak a protocol outside {lo}..{hi} and no release "
                              "bridges both controllers")
            bridge = max(bridges, key=lambda e: version(e["version"]))["version"]
            raise Refused(f"upgrade runners {', '.join(low)} to {bridge} first: the target controller accepts "
                          f"protocol {lo}..{hi} only")
    restart = set(move) | (set(pending) & set(pins))
    restart = sorted(restart if installed is None else restart & set(installed))
    plan_["restart"] = restart
    plan_["class"] = max([PIN_CLASS[name] for name in restart], default=0)
    # Capture gate: a capture the target worker cannot resume boots cold and is deleted.
    if "worker" in move:
        # Kept in the plan: the state machine checks the boxes again once the worker is quiesced.
        plan_["gate"] = {"target": target, "resumes": goal["runtime"]["resumes"], "accept_cold": accept_cold,
                         "kernel_sha256": goal["runtime"]["guest_kernel_sha256"], "cpu": (compat or {}).get("cpu")}
        plan_["cold"] = gate(plan_["gate"], captures)
    if plan_["class"] > max_class:
        raise Refused(f"class {plan_['class']} ({', '.join(restart)}) exceeds --max-class {max_class}")
    return plan_


# ---------------------------------------------------------------- fetch and stage


def git(*args, check=True):
    return h.run(["git", "-C", str(p(f"{h.SRV}/src")), *args], user="ow", check=check)


def fetch(config, ref=None):
    """Fetches the release ref (git objects only) and reads its manifest and version."""
    repository, ref = config["release"]["repository"], ref or config["release"]["ref"]
    if not h.REF.match(ref) or ".." in ref:
        raise Refused(f"release ref {ref!r} is not a tag, branch or commit")
    src = p(f"{h.SRV}/src")
    if not (src / ".git").exists():
        h.run(["git", "clone", "-q", repository, str(src)], user="ow")
    git("remote", "set-url", "origin", repository)
    git("fetch", "-q", "--tags", "--force", "origin")
    commit = (git("rev-parse", "--verify", "-q", f"origin/{ref}^{{commit}}", check=False).strip()
              or git("rev-parse", "--verify", "-q", f"{ref}^{{commit}}", check=False).strip())
    if not commit:
        raise Refused(f"{ref} is not in {repository}")
    try:
        manifest = json.loads(git("show", f"{commit}:release-impact.json"))
    except RuntimeError:
        raise Refused(f"{ref} has no release-impact.json: it predates in-place upgrades")
    cargo = git("show", f"{commit}:Cargo.toml")
    found = re.search(r'^\[workspace\.package\][^[]*?^version = "([^"]+)"', cargo, re.M | re.S)
    if not found:
        raise Refused(f"{ref}: no workspace version in Cargo.toml")
    v = found.group(1)
    version(v)
    return {"ref": ref, "commit": commit, "version": v, "manifest": manifest, "release": f"{v}-{commit[:12]}",
            "repository": repository}


def migrate_layout_v1():
    """A data volume from a layout v1 host keeps its build output: the shared target
    directory moves (same filesystem); the first release seeds its assets from the
    old asset directory, which stays where it is."""
    old, new = p(f"{h.SRV}/src/target"), p(f"{h.SRV}/build/target")
    if old.is_dir() and not old.is_symlink() and not new.exists():
        p(f"{h.SRV}/build").mkdir(parents=True, exist_ok=True)
        h.chown(p(f"{h.SRV}/build"), "ow")
        os.rename(old, new)
        say(f"moved the build output to {h.SRV}/build/target")


STAGE = r"""
rel=$1 src=$2 commit=$3 seed=$4 target=$5 role=$6 prebuilt=$7
mkdir -p "$rel" "$target"
git -C "$src" worktree prune
git -C "$src" worktree add -q --detach "$rel/src" "$commit"
# Its own asset set: a reflink copy on Btrfs, so staging never writes a file a running worker uses.
if [ -n "$seed" ]; then
  if cp --help 2>&1 | grep -q reflink; then cp -a --reflink=auto "$seed" "$rel/assets"; else cp -a "$seed" "$rel/assets"; fi
else
  mkdir "$rel/assets"
fi
mkdir -p "$rel/src/data" "$rel/bin"
ln -s ../../assets "$rel/src/data/runtime-spike"
ln -s "$target" "$rel/src/target"
cd "$rel/src"
# Tests swap the build for a stand-in; on a host (root) the environment is reset.
if [ -n "$prebuilt" ]; then
  # Released binaries the upgrader verified: a runner still fetches its pinned downloads.
  if [ "$role" != controller ]; then
    export OW_PREBUILT_GUEST="$prebuilt/ow-guest"
    eval "${OW_HOST_BUILD:-bash scripts/setup-local.sh}"
  fi
  # The same static binary is the one the dashboard offers as the Linux CLI.
  mkdir -p "$rel/assets/bin"
  cp "$prebuilt/ow" "$rel/assets/bin/ow-linux-amd64"
  sha256sum "$rel/assets/bin/ow-linux-amd64" | cut -d ' ' -f 1 > "$rel/assets/bin/ow-linux-amd64.sha256"
  cp "$prebuilt/ow" "$rel/bin/ow.tmp"
elif [ "$role" = controller ]; then
  eval "${OW_HOST_BUILD:-bash scripts/setup-local.sh --controller-only}"
  cp target/release/ow "$rel/bin/ow.tmp"
else
  eval "${OW_HOST_BUILD:-bash scripts/setup-local.sh}"
  cp target/release/ow "$rel/bin/ow.tmp"
fi
mv "$rel/bin/ow.tmp" "$rel/bin/ow"
"""


def release_url(repository, tag):
    """Where a GitHub repository's release assets download from; None for any other repository."""
    found = re.fullmatch(r"https://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(?:\.git)?/?", repository)
    return found and f"https://github.com/{found[1]}/{found[2]}/releases/download/{tag}"


def download(url):
    """The bytes at url, or None when it does not exist."""
    try:
        with urllib.request.urlopen(url, timeout=300) as response:
            return response.read()
    except OSError as e:  # urllib's HTTP and network errors, and timeouts
        if isinstance(e, urllib.error.HTTPError) and e.code == 404:
            return None
        raise Refused(f"cannot download {url}: {e}; retry (ow-host upgrade --from-source builds instead)")


def released(target, role):
    """The pinned release's published binaries ({"ow": bytes[, "ow-guest": bytes]}), verified
    against its SHA256SUMS and source-commit.txt; None, saying why, when it has none to use.
    Refused on any mismatch: nothing is installed."""
    tag, commit = f"v{target['version']}", target["commit"]
    url = release_url(target["repository"], tag)
    files = {"ow": "ow-linux-amd64"} if role == "controller" else {"ow": "ow-linux-amd64",
                                                                    "ow-guest": "ow-guest-linux-amd64"}
    if git("rev-parse", "--verify", "-q", f"refs/tags/{tag}^{{commit}}", check=False).strip() != commit:
        why = f"{commit[:12]} is not the release tag {tag}"
    elif not url:
        why = f"{target['repository']} publishes no release assets"
    elif os.uname().machine != "x86_64":
        why = f"no released binary for {os.uname().machine}"
    else:
        sums = download(f"{url}/SHA256SUMS")
        if sums is None:
            why = f"no published release {tag} with a SHA256SUMS"
        else:
            sums = dict(line.split()[::-1] for line in sums.decode().splitlines() if len(line.split()) == 2)
            missing = [asset for asset in files.values() if asset not in sums]
            why = missing and f"release {tag} publishes no {', '.join(missing)}"
    if why:
        say(f"building {target['release']} from source: {why}")
        return None

    def fetched(asset):
        data = download(f"{url}/{asset}")
        if data is None or hashlib.sha256(data).hexdigest() != sums.get(asset):
            raise Refused(f"release {tag}: {asset} does not match its SHA256SUMS")
        return data

    source = fetched("source-commit.txt").decode().strip()
    if source != commit:
        raise Refused(f"release {tag} was built from {source[:40]!r}, not the pinned {commit}")
    got = {name: fetched(asset) for name, asset in files.items()}
    say(f"installing {target['release']} from the released binaries of {tag} ({', '.join(files.values())}; "
        "verified against SHA256SUMS and source-commit.txt)")
    return got


def stage(target, from_source=False):
    """Installs the release into its own directory next to the running ones, from its
    verified released binaries or else built from source; reuses a complete one.
    Nothing running reads it until a pin moves."""
    name = target["release"]
    done = h.staged(name)
    if done and done.get("commit") == target["commit"]:
        return name
    rel = p(f"{h.SRV}/releases/{name}")
    lock = p(f"{h.SRV}/build/stage.lock")
    lock.parent.mkdir(parents=True, exist_ok=True)
    h.chown(lock.parent, "ow")
    with open(lock, "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        if rel.exists():  # a partial staging: start over
            git("worktree", "remove", "--force", str(rel / "src"), check=False)
            shutil.rmtree(rel)
        free = shutil.disk_usage(p(h.SRV)).free
        if free < 4 << 30:
            raise Refused(f"only {free >> 20} MiB free on {h.SRV}; staging needs 4 GiB")
        if from_source:
            say(f"building {name} from source: --from-source")
        got = None if from_source else released(target, h.role())
        prebuilt = p(f"{h.SRV}/build/prebuilt")
        shutil.rmtree(prebuilt, ignore_errors=True)
        for file, data in (got or {}).items():
            prebuilt.mkdir(exist_ok=True)
            (prebuilt / file).write_bytes(data)
            (prebuilt / file).chmod(0o755)
            h.chown(prebuilt / file, "ow")
        if got:
            h.chown(prebuilt, "ow")
        asset_pin = "agent" if h.role() == "controller" else "worker"
        seed = next((str(s) for s in (p(f"{h.SRV}/run/{asset_pin}/assets"), p(f"{h.SRV}/src/data/runtime-spike"))
                     if s.is_dir()), "")
        cmd = ["bash", "-euo", "pipefail", "-c", STAGE, "stage", str(rel), str(p(f"{h.SRV}/src")),
               target["commit"], seed, str(p(f"{h.SRV}/build/target")), h.role(), str(prebuilt) if got else ""]
        env = None
        if os.geteuid() == 0:
            # Niced and CPU-weighted: running boxes keep their vCPUs while it builds.
            cmd = ["systemd-run", "--scope", "--quiet", "-p", "CPUWeight=10", "nice", "-n", "10",
                   "runuser", "-u", "ow", "--", "env", "HOME=/srv/ow", *cmd]
            env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"}
        log = p(LOG)
        log.parent.mkdir(parents=True, exist_ok=True)
        say(f"staging {name} ({'released binaries' if got else 'building on the host'}; output in {LOG})")
        started = time.monotonic()
        with open(log, "a") as out:
            code = subprocess.run(cmd, stdout=out, stderr=subprocess.STDOUT, env=env).returncode
        if code:
            tail = log.read_text(errors="replace").splitlines()[-30:]
            raise RuntimeError(f"staging {name} failed ({code}):\n" + "\n".join(tail))
        built = h.run([str(rel / "bin/ow"), "--version"]).split()
        if built[-1:] != [target["version"]]:
            raise Refused(f"{name}: built ow reports {' '.join(built)}, not {target['version']}")
        digest = hashlib.sha256((rel / "bin/ow").read_bytes()).hexdigest()
        if got and digest != hashlib.sha256(got["ow"]).hexdigest():
            raise Refused(f"{name}: installed bin/ow is not the verified released binary")
        h.write_json(f"{h.SRV}/releases/{name}/STAGED", {
            "version": target["version"], "commit": target["commit"], "ref": target["ref"], "ow_sha256": digest,
            "staged": int(time.time()), "seconds": round(time.monotonic() - started),
            "source": "release" if got else "build"}, mode=0o644)
        shutil.rmtree(prebuilt, ignore_errors=True)
    return name


def prune(keep=2):
    """Keeps every release a pin points to, plus the `keep` newest others."""
    used = set(h.pins().values())
    root = p(f"{h.SRV}/releases")
    if not root.is_dir():
        return
    others = sorted((r for r in root.iterdir() if r.name not in used and not r.name.startswith(".")),
                    key=lambda r: (h.staged(r.name) or {}).get("staged", 0), reverse=True)
    for rel in others[keep:]:
        git("worktree", "remove", "--force", str(rel / "src"), check=False)
        shutil.rmtree(rel, ignore_errors=True)
        say(f"removed release {rel.name}")
    git("worktree", "prune", check=False)


def render_diff(target):
    """Pins whose unit files the target release renders differently from what is installed."""
    cmd = ["git", "-C", str(p(f"{h.SRV}/src")), "archive", target["commit"], "deploy/aws/host"]
    if os.geteuid() == 0:
        cmd = ["runuser", "-u", "ow", "--", *cmd]
    archive = subprocess.run(cmd, capture_output=True, check=True).stdout
    with tempfile.TemporaryDirectory() as tmp:
        tarfile.open(fileobj=io.BytesIO(archive)).extractall(tmp, filter="data")
        out = h.run(["python3", "-I", f"{tmp}/deploy/aws/host/ow_host.py", "render", "--config", str(p(h.CONFIG))])
    rendered = json.loads(out)
    changed = {path for path, text in rendered["files"].items() if not p(path).is_file() or p(path).read_text() != text}
    return {rendered["restarts"][path] for path in changed if path in rendered["restarts"]}


# ---------------------------------------------------------------- host state


def captures(compat):
    """Every capture the worker may resume later, with the tuple it was taken with."""
    found = []
    root = p(f"{h.RUNNER}/snapshots")
    for manifest in sorted(root.glob("*/manifest.json")) if root.is_dir() else []:
        if manifest.parent.name.startswith("."):
            continue
        try:
            m = json.loads(manifest.read_text())
        except ValueError:
            continue
        found.append({"name": f"{m.get('workspace')}/{m.get('name')}", "runtime": m.get("runtime"),
                      "cpu": m.get("cpu"), "kernel_sha256": m.get("kernel_sha256")})
    for box in (h.read_json(f"{h.RUNNER}/state.json") or {}).get("workspaces", {}).values():
        if box.get("state") == "running":
            found.append({"name": box["id"], **(compat or {})})
    return found


def host_cpu():
    """The worker's CPU string formula, for when no worker answers."""
    info = p("/proc/cpuinfo").read_text().splitlines()
    model = next(line for line in info if line.startswith("model name"))
    flags = next(line for line in info if line.startswith("flags"))
    return f"{model}\n{flags}\nhost-kernel:{p('/proc/sys/kernel/osrelease').read_text().strip()}"


class HostOps:
    """The host side effects the state machine uses; tests use a fake with the same methods."""

    def units(self):
        # An unenrolled runner's agent has nothing to run (its unit's condition keeps it down).
        idle = h.role() == "runner" and not p(f"{h.RUNNER}/node.json").exists()
        names = ("agent", "dashboard") if h.role() == "controller" else h.PINS
        return [name for name in names if h.unit_exists(h.UNITS[name]) and not (idle and name == "agent")]

    def active(self, name):
        return h.active(h.UNITS[name])

    def start(self, name):
        h.systemctl("start", h.UNITS[name])

    def stop(self, name):
        h.systemctl("stop", h.UNITS[name])

    def captures(self):
        return captures((self.worker_status() or {}).get("compat"))

    def worker_status(self):
        if h.role() == "controller":
            return None
        try:
            return h.ow("status", timeout=60)
        except (RuntimeError, ValueError, subprocess.TimeoutExpired):
            return None

    def boxes(self):
        if h.role() == "controller":
            return {}
        return {b["id"]: b["state"] for b in h.ow("list", timeout=1200)}

    def hibernate_all(self, release):
        # The old worker's own binary: it quiesces, hibernates each box and only then
        # shuts the worker down; on a failure it calls the shutdown off (boxes keep running).
        h.run([str(p(f"{h.SRV}/releases/{release}/bin/ow")), "--data-dir", h.RUNNER, "down", "--hibernate"],
              user="ow", capture=False, timeout=1800,
              env={"HOME": "/srv/ow", "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
                   "OW_ASSET_DIR": f"{h.SRV}/run/worker/assets"})
        h.systemctl("stop", h.UNITS["worker"])  # its stop hook finds no worker: a no-op

    def _quiesce(self, closing):
        script = ("import json,socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(1200); "
                  "s.connect(sys.argv[1]); s.sendall(json.dumps({'op':'quiesce','closing':sys.argv[2]=='1'}).encode()+b'\\n'); "
                  "r=json.loads(s.makefile().readline()); sys.exit(0 if r.get('ok') else 1)")
        return h.ok(["python3", "-c", script, str(p(f"{h.RUNNER}/control.sock")), "1" if closing else "0"], user="ow")

    def quiesce(self):
        """Makes the worker refuse new starts and wakes (what down --hibernate does first)."""
        if not self._quiesce(True):
            raise RuntimeError("the worker did not accept quiesce")

    def quiesce_off(self):
        """Lets a quiesced worker take starts and wakes again (idempotent)."""
        return h.role() == "controller" or self._quiesce(False)

    def start_box(self, box):
        h.ow("start", box, timeout=1200)

    def drain(self, on):
        if h.role() == "controller":
            return
        flag = p(f"{h.RUNNER}/drain")
        if on:
            flag.write_text("upgrade\n")
        else:
            flag.unlink(missing_ok=True)

    def backup_catalog(self, target):
        out = f"{h.DATA}/backups/pre-{target}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}.sqlite3"
        p(f"{h.DATA}/backups").mkdir(exist_ok=True)
        h.chown(p(f"{h.DATA}/backups"), "ow")
        h.ow("backup", "catalog", "--output", out)
        return out

    def sync_images(self, release):
        """Developer images built into the live set since `release` was staged."""
        live, staged = p(f"{h.SRV}/run/worker/assets/guest"), p(f"{h.SRV}/releases/{release}/assets/guest")
        for meta in sorted(live.glob("*.json")) if live.is_dir() else []:
            try:
                developer = json.loads(meta.read_text()).get("developer_user")
            except ValueError:
                continue
            other = staged / meta.name
            if developer and (not other.exists() or other.read_bytes() != meta.read_bytes()):
                for path in (meta.with_suffix(".ext4"), meta):
                    h.run(["cp", "--reflink=auto", "-p", str(path), str(staged / f".{path.name}.tmp")], user="ow")
                    os.replace(staged / f".{path.name}.tmp", staged / path.name)
                say(f"carried developer image {meta.stem} into {release}")

    def switch(self, to):
        for name, release in to.items():
            h.set_pin(name, release)
        # The release's own setup: a new layout re-renders the units (daemon-reload if so).
        h.run(["python3", "-I", str(p(f"{h.SRV}/releases/{to['current']}/src/deploy/aws/host/ow_host.py")), "setup"],
              capture=False)

    def verify(self, expected, deadline=180):
        """Health per pin: each unit runs its own pin's binary, the worker answers with its
        release, the runner is online with the agent's and worker's releases, the controller
        listens on the tailnet, the dashboard answers."""
        end = time.monotonic() + deadline
        problem = ""
        while time.monotonic() < end:
            problem = self._check(expected)
            if not problem:
                return
            time.sleep(3)
        raise RuntimeError(f"verify: {problem}")

    def _check(self, expected):
        pins = h.pins()
        for name in self.units():
            if not self.active(name):
                return f"{h.UNITS[name]} is not active"
            pid = h.run(["systemctl", "show", "-p", "MainPID", "--value", h.UNITS[name]], check=False).strip()
            exe = os.path.realpath(f"/proc/{pid}/exe") if pid not in ("", "0") else ""
            want = os.path.realpath(p(f"{h.SRV}/releases/{pins[name]}/bin/ow"))
            if exe != want:
                return f"{h.UNITS[name]} runs {exe or 'nothing'}, not {want}"
        if h.role() != "controller":
            status = self.worker_status() or {}
            if status.get("internet") is not True or status.get("version") != expected["worker"]:
                return f"worker answers {status.get('version')!r} (internet {status.get('internet')}), want {expected['worker']}"
            if "agent" not in self.units():
                return ""
            try:
                if not h.node_online(expected["agent"], expected["worker"]):
                    return f"runner not online with agent {expected['agent']} and worker {expected['worker']}"
            except (RuntimeError, ValueError):
                return "ow node list failed"
        if h.role() == "runner":
            return ""
        t = h.tailnet()
        if t == "wait":
            return "tailnet is not ready"
        if h.role() == "controller" and t == "none" and not h.listening():
            return "controller is not listening"
        if t not in ("none", "wait") and h.listening() != f"{t}:8790":
            return f"controller listens on {h.listening() or 'nothing'}, not {t}:8790"
        if "dashboard" in self.units() and t != "none" and not h.dashboard_answers():
            return "dashboard does not answer on 127.0.0.1:8787"
        return ""

    def canary(self):
        name = f"upgrade-canary-{int(time.time())}"
        h.ow("create", name, "--memory", "512")
        try:
            h.ow("exec", name, "--", "true")
            h.ow("stop", name)
        finally:
            h.ow("delete", name)


# ---------------------------------------------------------------- state machine


class Journal:
    """/srv/ow/.ow/upgrade.json, written atomically before each step acts."""

    def __init__(self, value):
        self.value = value
        self.mark = time.monotonic()

    def step(self, name):
        now = time.monotonic()
        last = self.value.get("step")
        if last:
            self.value.setdefault("seconds", {})[last] = round(now - self.mark, 1)
        self.mark = now
        self.value.update(step=name, updated=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
        h.write_json(JOURNAL, self.value)
        if name not in TERMINAL:
            say(f"upgrade: {name}")

    def __getitem__(self, key):
        return self.value[key]


def execute(plan_, ops, frm, to, target_name):
    """Runs the plan: pins `frm` -> `to` (release names, current included)."""
    restart = [name for name in plan_["restart"] if name in ops.units()]
    worker = "worker" in restart
    j = Journal({"id": f"u-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}", "from": frm, "to": to,
                 "class": plan_["class"], "restart": restart, "resume": [], "catalog_backup": None,
                 "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "error": None})
    if plan_.get("catalog_backup") and not plan_.get("rollback_auto", True):
        j.value["rollback_blocked"] = ("the old releases cannot read the migrated catalog: restore the copy in "
                                       f"{h.DATA}/backups with every writer stopped, or fix forward")
    j.step("gated")
    try:
        if plan_["catalog_backup"] and any(name in restart for name in ("agent", "dashboard")):
            j.step("catalog-backup")
            j.value["catalog_backup"] = ops.backup_catalog(target_name)
        if worker:
            j.step("drain")
            ops.drain(True)
            # Starts and wakes are refused from here on (a start the worker already took
            # lands first: it answers one request at a time), so this list is every box
            # the hibernate below captures.
            ops.quiesce()
            j.value["resume"] = sorted(box for box, state in ops.boxes().items() if state == "running")
            if plan_.get("gate"):
                # The last gate: nothing can start now, so this is every capture the new worker meets.
                j.value["cold"] = gate(plan_["gate"], ops.captures())
            j.step("hibernate")
            ops.hibernate_all(frm["worker"])
        j.step("stop")
        for name in ("dashboard", "agent"):
            if name in restart and name != "worker":
                ops.stop(name)
        if worker and to["worker"] != frm["worker"]:
            j.step("sync-images")
            ops.sync_images(to["worker"])
    except Exception as e:
        j.value["error"] = f"{e}"
        abort(j, ops)
        raise
    try:
        j.step("switch")
        ops.switch(to)
        j.step("start")
        start_quiesced(ops, restart)
        j.step("verify")
        ops.verify({name: version_of(to[name]) for name in j["from"] if name != "current"})
    except Exception as e:
        j.value["error"] = f"{e}"
        rollback(j, ops)
        raise
    finish(j, ops)


def version_of(release):
    return (h.staged(release) or {}).get("version") or release.split("-", 1)[0]


def finish(j, ops):
    """Resume and undrain; a box that does not resume stays hibernated (it wakes on its
    next connection) and is reported, never rolled back: its capture may be spent."""
    j.step("resume")
    lift(j, ops)
    failed = resume(j, ops)
    if j["class"] == 2 and not j["resume"]:
        ops.canary()
    j.step("undrain")
    ops.drain(False)
    h.clear_pending(set(j["restart"]))
    if failed:
        j.value["error"] = f"still hibernated (each wakes on its next connection): {', '.join(failed)}"
        say(j.value["error"])
    j.step("done")


def resume(j, ops):
    failed = []
    states = ops.boxes()
    for box in j["resume"]:
        if states.get(box) == "hibernated":
            try:
                ops.start_box(box)
                say(f"resumed {box}")
            except Exception as e:  # noqa: BLE001 - every box gets its turn
                say(f"could not resume {box}: {e}")
                failed.append(box)
    return failed


def abort(j, ops):
    """Before the switch: no pin moved. The old worker runs again, every box recorded
    as running is resumed, stopped units start again, the drain is lifted."""
    if "worker" in ops.units() and not ops.active("worker"):
        ops.start("worker")
        wait_worker(ops)
    elif not ops.quiesce_off():
        j.value["error"] = f"{j.value.get('error')}; the worker did not lift its quiesce (restart it)"
    failed = resume(j, ops)
    for name in ("agent", "dashboard"):
        if name in ops.units() and not ops.active(name):
            ops.start(name)
    ops.drain(False)
    if failed:
        j.value["error"] = f"{j.value.get('error')}; still hibernated: {', '.join(failed)}"
    j.step("aborted")
    say(f"upgrade aborted: {j.value['error']}")


def rollback(j, ops):
    """After the switch: the pins go back, the restarted units start on the old
    releases and are verified, then as abort."""
    if j.value.get("rollback_blocked"):
        raise RuntimeError(j.value["rollback_blocked"])
    j.step("rolling-back")
    restart = j["restart"]
    for name in ("dashboard", "agent", "worker"):
        if name in restart and ops.active(name):
            ops.stop(name)
    ops.switch(j["from"])
    start_quiesced(ops, restart)
    ops.verify({name: version_of(j["from"][name]) for name in j["from"] if name != "current"})
    if "worker" in restart:
        wait_worker(ops)
    lift(j, ops)
    resume(j, ops)
    ops.drain(False)
    j.step("rolled-back")
    say(f"upgrade rolled back: {j.value['error']}")


def start_quiesced(ops, restart):
    """Starts the restarted units; a started worker is quiesced before the agent and the
    gateway can wake a box on it: a resume hashes the box's snapshot first and would hold
    the one-request worker past verify's deadline. lift() opens it again."""
    for name in ("worker", "agent", "dashboard"):
        if name in restart:
            ops.start(name)
        if name == "worker" and name in restart:
            wait_worker(ops, limit=180)
            ops.quiesce()


def lift(j, ops):
    # Raises so the journal stays before "done" and recover() lifts it again.
    if "worker" in j["restart"] and not ops.quiesce_off():
        raise RuntimeError("the worker did not lift its quiesce")


def wait_worker(ops, limit=120):
    end = time.monotonic() + limit
    while not (ops.worker_status() or {}).get("internet"):
        if time.monotonic() > end:
            raise RuntimeError("the worker did not come back")
        time.sleep(1)


@contextlib.contextmanager
def locked():
    p(h.DATA).mkdir(parents=True, exist_ok=True)
    with open(p(LOCK), "w") as held:
        try:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Refused("another upgrade or restart is running")
        yield


def recover(ops=None, boot=False):
    """Reads the journal first, as every upgrade and every boot does."""
    ops = ops or HostOps()
    value = h.read_json(JOURNAL)
    if not value or value.get("step") in TERMINAL:
        return
    j = Journal(value)
    step = value["step"]
    say(f"recovering an upgrade interrupted at {step}")
    j.value["error"] = j.value.get("error") or f"interrupted at {step}"
    if boot:
        # A new instance: its pins came from the config, so only the boxes and the drain remain.
        resume(j, ops)
        ops.drain(False)
        j.step("aborted")
    elif step in BEFORE_SWITCH or step == "gated":
        abort(j, ops)
    elif step in AFTER_SWITCH or step == "rolling-back":
        rollback(j, ops)
    else:  # resume or undrain: the new releases run; finish the job
        finish(j, ops)


def upgrade(ref=None, plan_only=False, accept_cold=False, max_class=2, ops=None, controller=None, from_source=False):
    ops = ops or HostOps()
    with locked():
        recover(ops)
        config = h.validate(h.read_json(h.CONFIG))
        target = fetch(config, ref)
        frm = h.pins()
        if config["role"] == "controller":
            # The optional dashboard pin must remain readable after catalog migrations.
            frm = {name: value for name, value in frm.items() if name in (*h.role_pins(config), "current")}
        pin_names = [name for name in frm if name != "current"]
        if None in frm.values():
            raise Refused("this host has no release pins (layout v1): replace it with this release first")
        newest = max((h.staged(frm[name]) or {"version": "0.0.0"} for name in pin_names), key=lambda s: version(s["version"]))
        if version(newest["version"]) > version(target["version"]):
            # Going back: an older release's manifest does not list what runs now; the newest running one does.
            target["manifest"] = json.loads(git("show", f"{newest['commit']}:release-impact.json"))
        status = (ops.worker_status() or {}) if config["role"] != "controller" else {}
        compat = status.get("compat") or ({"cpu": host_cpu()} if config["role"] != "controller" else {})
        if config["role"] == "runner":
            # Its controller is another host: ow-aws passes the release that runs there.
            if controller is None:
                raise Refused("a runner host needs --controller-version (the release its controller's agent runs; "
                              "ow-aws upgrade passes it)")
            version(controller)
            runners = []
        else:
            controller = None
            runners = [{"id": n["id"], "protocol": (n.get("capabilities") or {}).get("protocol", 0)}
                       for n in h.ow("node", "list", timeout=30) if n["id"] != h.local_node() and not n.get("revoked")]
        common = dict(rendered=render_diff(target), pending=set(h.pending()), runners=runners,
                      compat=compat, accept_cold=accept_cold, max_class=max_class, controller=controller,
                      installed=ops.units() if config["role"] == "controller" else None)
        current = {name: version_of(frm[name]) for name in pin_names}
        result = plan(target["manifest"], current, target["version"], captures=captures(compat) if "worker" in current else (), **common)
        say(json.dumps({"plan": result, "release": target["release"]}, indent=2))
        if plan_only:
            return 0
        if not result["move"] and not result["restart"] and frm["current"] == target["release"]:
            say("nothing to do")
            return 0
        release = stage(target, from_source)
        if "worker" in result["move"]:
            # The gate again: the staged kernel and the boxes as they are now.
            kernels = {hashlib.sha256(k.read_bytes()).hexdigest()
                       for k in p(f"{h.SRV}/releases/{release}/assets/downloads").glob("vmlinux-*")}
            entry = next(e for e in target["manifest"]["releases"] if e["version"] == target["version"])
            want = entry["runtime"]["guest_kernel_sha256"]
            if want not in kernels:
                raise Refused(f"the staged release has no guest kernel with the manifest's hash ({want})")
            result = plan(target["manifest"], current, target["version"],
                          captures=captures((ops.worker_status() or {}).get("compat") or compat), **common)
            if result["cold"] and p("/etc/open-workspaces/backup.env").exists():
                say("backing up before cold boots")
                h.run(["/usr/local/bin/ow-backup"], capture=False)
        to = {name: release if name in result["move"] else frm[name] for name in pin_names}
        to["current"] = release
        execute(result, ops, frm, to, target["version"])
        prune()
        say(f"upgraded to {target['release']} (class {result['class']})")
        return 0


def restart_worker(ops=None):
    """Class 2 restart on the same releases: drain, hibernate, restart, resume."""
    if h.role() == "controller":
        raise Refused("controller role has no worker to restart")
    ops = ops or HostOps()
    with locked():
        recover(ops)
        frm = h.pins()
        execute({"class": 2, "restart": ["worker"], "move": {}, "catalog_backup": False}, ops, frm, dict(frm),
                version_of(frm["worker"]))
        return 0
