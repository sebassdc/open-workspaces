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

import ow_host as h
from ow_host import Refused, p, say

JOURNAL = f"{h.DATA}/upgrade.json"
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


def plan(manifest, pins, target, *, rendered=(), pending=(), runners=(), captures=(), compat=None,
         accept_cold=False, max_class=2):
    """The upgrade of one host from `pins` (pin -> release version) to version `target`.

    rendered: pins whose unit files the target release renders differently.
    pending:  pins to restart for a config change (their release stays).
    runners:  other runners of this controller, as {"id", "protocol"}.
    captures: every capture the worker could resume later, as {"name", "runtime", "cpu",
              "kernel_sha256"}: hibernated boxes, named snapshots, and running boxes
              (which will be captured with the old worker's tuple).
    compat:   the running worker's tuple ({"cpu": ...}): the CPU string comes from the host.
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
    schema = {name: entries[pins[name]]["catalog"]["schema"] for name in ("agent", "dashboard")}
    if any(s != goal["catalog"]["schema"] for s in schema.values()):
        plan_["catalog_backup"] = True
        plan_["rollback_auto"] = min(schema.values()) >= goal["catalog"]["min_reader"]
        move.update(agent=target, dashboard=target)
    # Protocol: every other runner must speak what the new controller accepts.
    if "agent" in move:
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
    restart = sorted(set(move) | set(pending))
    plan_["restart"] = restart
    plan_["class"] = max([PIN_CLASS[name] for name in restart], default=0)
    # Capture gate: a capture the target worker cannot resume boots cold and is deleted.
    if "worker" in move:
        resumes = goal["runtime"]["resumes"]
        kernel = goal["runtime"]["guest_kernel_sha256"]
        cpu = (compat or {}).get("cpu")
        plan_["cold"] = sorted(c["name"] for c in captures if not (
            c.get("runtime") in resumes and c.get("cpu") == cpu and c.get("kernel_sha256") == kernel))
        if plan_["cold"] and not accept_cold:
            raise Refused(f"{target} cannot resume {', '.join(plan_['cold'])} (another runtime, CPU or guest "
                          "kernel): their processes would be lost. Pass --accept-cold to boot them cold")
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
    return {"ref": ref, "commit": commit, "version": v, "manifest": manifest, "release": f"{v}-{commit[:12]}"}


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
rel=$1 src=$2 commit=$3 seed=$4 target=$5
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
eval "${OW_HOST_BUILD:-bash scripts/setup-local.sh}"
cp target/release/ow "$rel/bin/ow.tmp"
mv "$rel/bin/ow.tmp" "$rel/bin/ow"
"""


def stage(target):
    """Builds the release into its own directory next to the running ones; reuses a
    complete one. Nothing running reads it until a pin moves."""
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
        seed = next((str(s) for s in (p(f"{h.SRV}/run/worker/assets"), p(f"{h.SRV}/src/data/runtime-spike"))
                     if s.is_dir()), "")
        cmd = ["bash", "-euo", "pipefail", "-c", STAGE, "stage", str(rel), str(p(f"{h.SRV}/src")),
               target["commit"], seed, str(p(f"{h.SRV}/build/target"))]
        env = None
        if os.geteuid() == 0:
            # Niced and CPU-weighted: running boxes keep their vCPUs while it builds.
            cmd = ["systemd-run", "--scope", "--quiet", "-p", "CPUWeight=10", "nice", "-n", "10",
                   "runuser", "-u", "ow", "--", "env", "HOME=/srv/ow", *cmd]
            env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"}
        say(f"staging {name} (building on the host)")
        started = time.monotonic()
        h.run(cmd, capture=False, env=env)
        built = h.run([str(rel / "bin/ow"), "--version"]).split()
        if built[-1:] != [target["version"]]:
            raise Refused(f"{name}: built ow reports {' '.join(built)}, not {target['version']}")
        digest = hashlib.sha256((rel / "bin/ow").read_bytes()).hexdigest()
        h.write_json(f"{h.SRV}/releases/{name}/STAGED", {
            "version": target["version"], "commit": target["commit"], "ref": target["ref"], "ow_sha256": digest,
            "staged": int(time.time()), "seconds": round(time.monotonic() - started)}, mode=0o644)
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
        return [name for name in h.PINS if h.unit_exists(h.UNITS[name])]

    def active(self, name):
        return h.active(h.UNITS[name])

    def start(self, name):
        h.systemctl("start", h.UNITS[name])

    def stop(self, name):
        h.systemctl("stop", h.UNITS[name])

    def worker_status(self):
        try:
            return h.ow("status", timeout=60)
        except (RuntimeError, ValueError, subprocess.TimeoutExpired):
            return None

    def boxes(self):
        return {b["id"]: b["state"] for b in h.ow("list", timeout=1200)}

    def hibernate_all(self, release):
        # The old worker's own binary: it quiesces, hibernates each box and only then
        # shuts the worker down; on a failure it calls the shutdown off (boxes keep running).
        h.run([str(p(f"{h.SRV}/releases/{release}/bin/ow")), "--data-dir", h.RUNNER, "down", "--hibernate"],
              user="ow", capture=False, timeout=1800,
              env={"HOME": "/srv/ow", "PATH": "/usr/sbin:/usr/bin:/sbin:/bin",
                   "OW_ASSET_DIR": f"{h.SRV}/run/worker/assets"})
        h.systemctl("stop", h.UNITS["worker"])  # its stop hook finds no worker: a no-op

    def quiesce_off(self):
        """Lets a quiesced worker take starts and wakes again (idempotent)."""
        script = ("import json,socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(1200); "
                  "s.connect(sys.argv[1]); s.sendall(b'{\"op\":\"quiesce\",\"closing\":false}\\n'); "
                  "s.recv(4096)")
        h.run(["python3", "-c", script, f"{h.RUNNER}/control.sock"], user="ow", check=False)

    def start_box(self, box):
        h.ow("start", box, timeout=1200)

    def drain(self, on):
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
        status = self.worker_status() or {}
        if status.get("internet") is not True or status.get("version") != expected["worker"]:
            return f"worker answers {status.get('version')!r} (internet {status.get('internet')}), want {expected['worker']}"
        try:
            if not h.node_online(expected["agent"], expected["worker"]):
                return f"runner not online with agent {expected['agent']} and worker {expected['worker']}"
        except (RuntimeError, ValueError):
            return "ow node list failed"
        t = h.tailnet()
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
            j.value["resume"] = sorted(box for box, state in ops.boxes().items() if state == "running")
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
        for name in ("worker", "agent", "dashboard"):
            if name in restart:
                ops.start(name)
        j.step("verify")
        ops.verify({name: version_of(to[name]) for name in h.PINS})
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
    else:
        ops.quiesce_off()
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
    for name in ("worker", "agent", "dashboard"):
        if name in restart:
            ops.start(name)
    ops.verify({name: version_of(j["from"][name]) for name in h.PINS})
    if "worker" in restart:
        wait_worker(ops)
    resume(j, ops)
    ops.drain(False)
    j.step("rolled-back")
    say(f"upgrade rolled back: {j.value['error']}")


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


def upgrade(ref=None, plan_only=False, accept_cold=False, max_class=2, ops=None):
    ops = ops or HostOps()
    with locked():
        recover(ops)
        config = h.validate(h.read_json(h.CONFIG))
        target = fetch(config, ref)
        frm = h.pins()
        if None in frm.values():
            raise Refused("this host has no release pins (layout v1): replace it with this release first")
        status = ops.worker_status() or {}
        compat = status.get("compat") or {"cpu": host_cpu()}
        runners = [{"id": n["id"], "protocol": (n.get("capabilities") or {}).get("protocol", 0)}
                   for n in h.ow("node", "list", timeout=30) if n["id"] != h.local_node() and not n.get("revoked")]
        common = dict(rendered=render_diff(target), pending=set(h.pending()), runners=runners,
                      compat=compat, accept_cold=accept_cold, max_class=max_class)
        current = {name: version_of(frm[name]) for name in h.PINS}
        result = plan(target["manifest"], current, target["version"], captures=captures(compat), **common)
        say(json.dumps({"plan": result, "release": target["release"]}, indent=2))
        if plan_only:
            return 0
        if not result["restart"] and frm["current"] == target["release"]:
            say("nothing to do")
            return 0
        release = stage(target)
        if "worker" in result["move"]:
            # The gate again: the staged kernel and the boxes as they are now.
            kernel = next(p(f"{h.SRV}/releases/{release}/assets/downloads").glob("vmlinux-*"), None)
            digest = hashlib.sha256(kernel.read_bytes()).hexdigest() if kernel else None
            entry = next(e for e in target["manifest"]["releases"] if e["version"] == target["version"])
            want = entry["runtime"]["guest_kernel_sha256"]
            if digest != want:
                raise Refused(f"the staged guest kernel ({digest}) is not the manifest's ({want})")
            result = plan(target["manifest"], current, target["version"],
                          captures=captures((ops.worker_status() or {}).get("compat") or compat), **common)
            if result["cold"] and p("/etc/open-workspaces/backup.env").exists():
                say("backing up before cold boots")
                h.run(["/usr/local/bin/ow-backup"], capture=False)
        to = {name: release if name in result["move"] else frm[name] for name in h.PINS}
        to["current"] = release
        execute(result, ops, frm, to, target["version"])
        prune()
        say(f"upgraded to {target['release']} (class {result['class']})")
        return 0


def restart_worker(ops=None):
    """Class 2 restart on the same releases: drain, hibernate, restart, resume."""
    ops = ops or HostOps()
    with locked():
        recover(ops)
        frm = h.pins()
        execute({"class": 2, "restart": ["worker"], "move": {}, "catalog_backup": False}, ops, frm, dict(frm),
                version_of(frm["worker"]))
        return 0
