#!/usr/bin/env python3
"""ow-host: the host side of an AWS host in layout v2 (docs/UPGRADES.md).

Runs as root on the host, usually through SSM from `ow-aws`. Each release ships
it; /usr/local/sbin/ow-host runs the copy in /srv/ow/current.

  boot                  stage 0's last step: stage the configured release, pin it, set up, start
  invite-runner NODE --memory M --slots S --cpus C
                        (controller) mint NODE's single-use join into its Secrets Manager secret
  enrol [--wait S]      (runner) read that join, start the node agent, wait until online
  setup                 render units and files from the stored config; print what changed
  apply                 fetch the SSM config, validate, store, set up, act on class 0; report the rest
  ready [--wait S] [--images]
  restart --control-plane | --worker
  upgrade [REF] [--plan] [--accept-cold] [--max-class N] [--controller-version V (runner hosts)]
  recover               finish or undo an upgrade that was interrupted
  status

Python 3.12 standard library only. OW_HOST_ROOT (tests) prefixes every path.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(os.environ.get("OW_HOST_ROOT", "/"))
HERE = Path(__file__).resolve().parent


def p(path):
    """A host path under ROOT."""
    return ROOT / str(path).lstrip("/")


SRV = "/srv/ow"
DATA = f"{SRV}/.ow"
RUNNER = f"{DATA}/runner"
CONTROLLER = f"{DATA}/controller"
# A runner host (role runner) joins another host's controller: the join it reads from
# Secrets Manager, the controller's CA and URL, all on the data volume.
JOIN = f"{RUNNER}/join.json"
CA = f"{DATA}/controller-ca.pem"
CONTROLLER_ENV = f"{DATA}/controller.env"
JOIN_TTL = 600
CONFIG = f"{DATA}/host-config.json"
PENDING = f"{DATA}/pending.json"
HOST_ENV = "/etc/open-workspaces/host.env"
STATUS = "/var/lib/open-workspaces/bootstrap-status"
SETUP_DUE = "/var/lib/open-workspaces/setup-due.json"
UNITS = {
    "worker": "open-workspaces-worker.service",
    "agent": "open-workspaces.service",
    "dashboard": "open-workspaces-dashboard.service",
}
# Releases' own pins: each unit runs from /srv/ow/run/<pin>; current is the CLI and ow-host.
PINS = ("worker", "agent", "dashboard")
SLOTS = 8

LOGIN = re.compile(r"^[A-Za-z0-9._%+@-]+@[A-Za-z0-9.-]+$")
BACKUP = re.compile(r"^s3://[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]/[A-Za-z0-9_-][A-Za-z0-9._-]*(?:/[A-Za-z0-9_-][A-Za-z0-9._-]*)*$")
SCHEDULE = re.compile(r"^[A-Za-z0-9 *:/,.~_-]{1,100}$")
HOSTNAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]{0,62}$")
REF = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$")
REPOSITORY = re.compile(r"^(https://[A-Za-z0-9._~:/@%+-]+|/[A-Za-z0-9._/-]+)$")
SECRET = re.compile(r"^(arn:aws[a-z-]*:secretsmanager:[a-z0-9-]+:\d{12}:secret:[A-Za-z0-9/_+=.@-]+)?$")
NODE = re.compile(r"^[A-Za-z0-9_-]{1,32}$")


class Refused(Exception):
    """A refusal the caller must act on; nothing was changed."""


# ---------------------------------------------------------------- commands


def run(cmd, check=True, capture=True, user=None, env=None, timeout=None, input=None):
    """Runs a command (as `user` when given); returns stdout. Tests replace this."""
    if user and os.geteuid() == 0:
        cmd = ["runuser", "-u", user, "--", *cmd]
    result = subprocess.run(cmd, capture_output=capture, text=True, env=env, timeout=timeout, input=input)
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip() if capture else ""
        raise RuntimeError(f"{' '.join(map(str, cmd))} exited {result.returncode}: {detail[-2000:]}")
    return result.stdout if capture else ""


def ok(cmd, **kw):
    try:
        run(cmd, **kw)
        return True
    except (RuntimeError, OSError, subprocess.TimeoutExpired):
        return False


def ow(*args, timeout=900):
    """`ow ARGS` as the ow user through the installed wrapper; JSON output parsed."""
    out = run(["/usr/local/bin/ow", *args], timeout=timeout)
    return json.loads(out) if out.strip().startswith(("{", "[")) else out


def systemctl(*args, check=True):
    return run(["systemctl", *args], check=check)


def active(unit):
    return run(["systemctl", "is-active", unit], check=False).strip() == "active"


def unit_exists(unit):
    return p(f"/etc/systemd/system/{unit}").exists()


def say(*args):
    print(*args, flush=True)


# ---------------------------------------------------------------- files


def write_atomic(path, text, mode=0o644, owner=None):
    """Writes path (a host path) only if its content or mode differs; True when it changed."""
    target = p(path)
    old = target.read_text() if target.is_file() else None
    if old == text and (target.stat().st_mode & 0o777) == mode:
        return False
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_name(f".{target.name}.tmp")
    tmp.write_text(text)
    os.chmod(tmp, mode)
    if owner:
        chown(tmp, owner)
    os.replace(tmp, target)
    return True


def chown(path, owner):
    if os.geteuid() == 0:
        import pwd

        entry = pwd.getpwnam(owner)
        os.chown(path, entry.pw_uid, entry.pw_gid)


def read_json(path, default=None):
    try:
        return json.loads(p(path).read_text())
    except (OSError, ValueError):
        return default


def write_json(path, value, mode=0o600):
    write_atomic(path, json.dumps(value, indent=2, sort_keys=True) + "\n", mode)


# ---------------------------------------------------------------- config


def validate(config):
    """Checks the SSM parameter (untrusted input) and returns it with defaults filled in."""
    if not isinstance(config, dict) or config.get("schema") != 1:
        raise Refused("config: schema must be 1")
    role = config.get("role", "host")
    if role not in ("host", "runner"):
        raise Refused(f"config: role {role!r} is not supported by this release")
    release = config.get("release") or {}
    if not REPOSITORY.match(str(release.get("repository", ""))):
        raise Refused(f"config: release.repository {release.get('repository')!r} is not an https URL or an absolute path")
    if not REF.match(str(release.get("ref", ""))) or ".." in release["ref"]:
        raise Refused(f"config: release.ref {release.get('ref')!r} is not a tag, branch or commit")
    tailscale = {"secretArn": "", "hostname": "", "ssh": False, **(config.get("tailscale") or {})}
    if not SECRET.match(str(tailscale["secretArn"])):
        raise Refused("config: tailscale.secretArn is not a Secrets Manager secret ARN")
    if tailscale["hostname"] and not HOSTNAME.match(str(tailscale["hostname"])):
        raise Refused(f"config: tailscale.hostname {tailscale['hostname']!r} is invalid")
    if not isinstance(tailscale["ssh"], bool):
        raise Refused("config: tailscale.ssh must be true or false")
    logins = (config.get("dashboard") or {}).get("logins") or []
    for login in logins:
        if not isinstance(login, str) or not LOGIN.match(login) or len(login) > 254:
            raise Refused(f"config: invalid dashboard login {login!r}")
    if logins and not tailscale["secretArn"]:
        raise Refused("config: dashboard logins need tailscale.secretArn (the dashboard is served over the tailnet only)")
    images = config.get("developerImages") or []
    if any(image != "ubuntu" for image in images):
        raise Refused(f"config: developerImages takes ubuntu, not {images!r}")
    backup = {"url": "", "schedule": "", **(config.get("backup") or {})}
    if backup["url"] and not BACKUP.match(backup["url"]) or ".." in backup["url"].split("/"):
        raise Refused(f"config: backup.url {backup['url']!r} must be s3://bucket/prefix")
    if backup["schedule"] and (not backup["url"] or not SCHEDULE.match(backup["schedule"])):
        raise Refused("config: backup.schedule needs backup.url and a systemd calendar expression")
    idle = config.get("idleMinutes", 30)
    if not isinstance(idle, int) or isinstance(idle, bool) or not 0 <= idle <= 10080:
        raise Refused("config: idleMinutes must be 0 to 10080")
    extra = {}
    if role == "runner":
        runner = config.get("runner") or {}
        if not NODE.match(str(runner.get("node", ""))) or runner["node"] == "local":
            raise Refused(f"config: runner.node {runner.get('node')!r} is not a node ID")
        if not HOSTNAME.match(str(runner.get("controller", ""))):
            raise Refused(f"config: runner.controller {runner.get('controller')!r} is not a host name")
        if not runner.get("joinSecretArn") or not SECRET.match(str(runner["joinSecretArn"])):
            raise Refused("config: runner.joinSecretArn is not a Secrets Manager secret ARN")
        if not tailscale["secretArn"]:
            raise Refused("config: a runner reaches its controller over the tailnet: tailscale.secretArn is required")
        if logins:
            raise Refused("config: a runner has no dashboard (it runs no controller): drop dashboard.logins")
        extra["runner"] = {key: runner[key] for key in ("node", "controller", "joinSecretArn")}
    return {
        "schema": 1,
        "role": role,
        "release": {"repository": release["repository"], "ref": release["ref"]},
        "tailscale": tailscale,
        "dashboard": {"logins": list(logins)},
        "developerImages": sorted(set(images)),
        "backup": backup,
        "idleMinutes": idle,
        **extra,
    }


def budgets():
    """Local runner budget: three quarters of RAM (cap 64 GiB), every vCPU (cap 64), 8 slots."""
    meminfo = p("/proc/meminfo").read_text()
    kib = int(re.search(r"^MemTotal:\s+(\d+)", meminfo, re.M).group(1))
    return {"memory": min(kib * 3 // 4 // 1024, 65536), "cpus": min(os.cpu_count() or 1, 64), "slots": SLOTS}


# ---------------------------------------------------------------- rendering


def render(config, budget):
    """Every file setup owns: host path -> (content, mode). Pure; also run from a
    target release's copy to see which units an upgrade changes."""
    files = {}
    asset = f"{SRV}/run/worker/assets"
    worker_bin = f"{SRV}/run/worker/bin/ow"
    files["/etc/open-workspaces/worker.env"] = (
        "HOME=/srv/ow\n"
        f"OW_ASSET_DIR={asset}\n"
        f"OW_MAX_MEMORY_MIB={budget['memory']}\n"
        f"OW_MAX_RUNNING={budget['slots']}\n"
        f"OW_MAX_VCPUS={budget['cpus']}\n"
        f"OW_IDLE_HIBERNATE_SECS={config['idleMinutes'] * 60}\n",
        0o644,
    )
    files["/etc/systemd/system/open-workspaces-worker.service"] = (
        f"""[Unit]
Description=Open Workspaces worker and its VMs (a restart hibernates the running boxes)
After=network-online.target
Wants=network-online.target
RequiresMountsFor=/srv/ow

[Service]
User=ow
EnvironmentFile=/etc/open-workspaces/worker.env
ExecStart={worker_bin} --data-dir {RUNNER} supervisor
# A stop (reboot, instance stop, ow-host restart --worker) hibernates the running
# boxes first, with wakes refused, so their processes survive (about 25 s per 1 GiB).
ExecStop={worker_bin} --data-dir {RUNNER} down --hibernate
TimeoutStopSec=900
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
""",
        0o644,
    )
    # Wants=/After= only: restarting the control plane never restarts the worker, and
    # at shutdown the control plane stops first, so no wake arrives while boxes hibernate.
    if config["role"] == "runner":
        # No controller here: the agent dials the one `ow-host enrol` recorded, and starts
        # only once a credential (or a join to redeem) is in place.
        control = f"""[Unit]
Description=Open Workspaces node agent for controller {config['runner']['controller']} (a restart leaves the VMs running)
After=open-workspaces-worker.service network-online.target tailscaled.service
Wants=open-workspaces-worker.service network-online.target
RequiresMountsFor=/srv/ow
ConditionPathExists=|{RUNNER}/node.json
ConditionPathExists=|{JOIN}

[Service]
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR={asset}
EnvironmentFile={CONTROLLER_ENV}
ExecStartPre=/usr/local/lib/open-workspaces/wait-tailnet
ExecStart={SRV}/run/agent/bin/ow --data-dir {RUNNER} node-agent --controller ${{OW_CONTROLLER}} --credential {RUNNER}/node.json --join {JOIN} --ca-cert {CA}
"""
    else:
        control = f"""[Unit]
Description=Open Workspaces controller and node agent (a restart leaves the VMs running)
After=open-workspaces-worker.service network-online.target tailscaled.service
Wants=open-workspaces-worker.service network-online.target
RequiresMountsFor=/srv/ow

[Service]
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR={asset}
ExecStartPre=/usr/local/lib/open-workspaces/wait-tailnet
ExecStart={SRV}/run/agent/bin/ow host up --external-worker --memory {budget['memory']} --slots {budget['slots']} --cpus {budget['cpus']}
"""
    files["/etc/systemd/system/open-workspaces.service"] = (
        f"""{control}# SIGTERM: no new jobs; jobs in flight (a capture takes up to 900 s) finish first.
TimeoutStopSec=960
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
""",
        0o644,
    )
    files["/etc/systemd/system/open-workspaces-recover.service"] = (
        """[Unit]
Description=Open Workspaces: finish or undo an upgrade a reboot interrupted
After=open-workspaces.service
Wants=open-workspaces.service

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/ow-host recover

[Install]
WantedBy=multi-user.target
""",
        0o644,
    )
    files["/usr/local/lib/open-workspaces/wait-tailnet"] = ((HERE / "files/wait-tailnet").read_text(), 0o755)
    files["/usr/local/bin/ow"] = (
        f"""#!/bin/sh
# The worker checks the caller's UID, so every command runs as the ow user.
[ "$(id -un)" = ow ] || exec runuser -u ow -- "$0" "$@"
export HOME=/srv/ow OW_DATA_DIR={RUNNER} OW_ASSET_DIR={asset}
exec {SRV}/current/bin/ow "$@"
""",
        0o755,
    )
    files["/usr/local/sbin/ow-host"] = (
        f"#!/bin/sh\nexec python3 -I {SRV}/current/src/deploy/aws/host/ow_host.py \"$@\"\n",
        0o755,
    )
    # Ubuntu confines unprivileged user namespaces; allow them for release binaries only
    # (and the shared build output, which developer-image builds run).
    files["/etc/apparmor.d/open-workspaces"] = (
        """abi <abi/4.0>,
include <tunables/global>
profile open-workspaces /srv/ow/{releases/*/bin,build/target/release}/ow flags=(unconfined) {
  userns,
  include if exists <local/open-workspaces>
}
""",
        0o644,
    )
    logins = config["dashboard"]["logins"]
    if logins:
        files["/etc/open-workspaces/dashboard.env"] = (f"OW_DASHBOARD_LOGINS={','.join(logins)}\n", 0o644)
        files["/usr/local/lib/open-workspaces/dashboard-prepare"] = ((HERE / "files/dashboard-prepare").read_text(), 0o755)
        files["/etc/systemd/system/open-workspaces-dashboard.service"] = (
            f"""[Unit]
Description=Open Workspaces dashboard on loopback, served to the tailnet by tailscale serve
After=open-workspaces.service tailscaled.service
Wants=open-workspaces.service
StartLimitIntervalSec=0

[Service]
User=ow
EnvironmentFile=/etc/open-workspaces/dashboard.env
Environment=HOME=/srv/ow OW_ASSET_DIR={asset}
ExecStartPre=+/usr/local/lib/open-workspaces/dashboard-prepare
ExecStart={SRV}/run/dashboard/bin/ow --data-dir {DATA}/controller dashboard --config {DATA}/dashboard.json --listen 127.0.0.1:8787
Restart=always
RestartSec=30

[Install]
WantedBy=multi-user.target
""",
            0o644,
        )
    images = config["developerImages"]
    if images:
        files["/usr/local/lib/open-workspaces/developer-images"] = ((HERE / "files/developer-images").read_text(), 0o755)
        files["/etc/systemd/system/open-workspaces-images.service"] = (
            f"""[Unit]
Description=Open Workspaces developer images ({' '.join(images)}), built once
After=open-workspaces.service

[Service]
Type=oneshot
User=ow
Environment=HOME=/srv/ow OW_ASSET_DIR={asset}
ExecStart=/usr/local/lib/open-workspaces/developer-images {' '.join(images)}
TimeoutStartSec=3600

[Install]
WantedBy=multi-user.target
""",
            0o644,
        )
    backup = config["backup"]
    if backup["url"]:
        # A runner has no catalog to copy: only its own snapshots, under its own prefix.
        catalog = "OW_BACKUP_CATALOG=0\n" if config["role"] == "runner" else ""
        files["/etc/open-workspaces/backup.env"] = (f"OW_BACKUP_URL={backup['url']}\n{catalog}", 0o644)
        files["/usr/local/bin/ow-backup"] = ((HERE / "files/ow-backup").read_text(), 0o755)
        if backup["schedule"]:
            files["/etc/systemd/system/ow-backup.service"] = (
                """[Unit]
Description=Open Workspaces backup to S3 (catalog copy, then new sealed snapshots)

[Service]
Type=oneshot
ExecStart=/usr/local/bin/ow-backup
""",
                0o644,
            )
            files["/etc/systemd/system/ow-backup.timer"] = (
                f"""[Unit]
Description=Open Workspaces backup schedule

[Timer]
OnCalendar={backup['schedule']}
Persistent=true

[Install]
WantedBy=timers.target
""",
                0o644,
            )
    return files


# Every path render can produce: a feature turned off removes its files.
OWNED = (
    "/etc/open-workspaces/worker.env",
    "/etc/open-workspaces/dashboard.env",
    "/etc/open-workspaces/backup.env",
    "/etc/systemd/system/open-workspaces-worker.service",
    "/etc/systemd/system/open-workspaces.service",
    "/etc/systemd/system/open-workspaces-recover.service",
    "/etc/systemd/system/open-workspaces-dashboard.service",
    "/etc/systemd/system/open-workspaces-images.service",
    "/etc/systemd/system/ow-backup.service",
    "/etc/systemd/system/ow-backup.timer",
    "/usr/local/lib/open-workspaces/wait-tailnet",
    "/usr/local/lib/open-workspaces/dashboard-prepare",
    "/usr/local/lib/open-workspaces/developer-images",
    "/usr/local/bin/ow",
    "/usr/local/bin/ow-backup",
    "/usr/local/sbin/ow-host",
    "/etc/apparmor.d/open-workspaces",
)

# Files whose change needs a unit restarted, by pin (the rest take effect by themselves).
RESTARTS = {
    "/etc/open-workspaces/worker.env": "worker",
    "/etc/systemd/system/open-workspaces-worker.service": "worker",
    "/etc/systemd/system/open-workspaces.service": "agent",
    "/usr/local/lib/open-workspaces/wait-tailnet": "agent",
    "/etc/open-workspaces/dashboard.env": "dashboard",
    "/etc/systemd/system/open-workspaces-dashboard.service": "dashboard",
    "/usr/local/lib/open-workspaces/dashboard-prepare": "dashboard",
}


def diff(files):
    """Host paths whose rendered content differs from what is installed."""
    changed = {path for path, (text, mode) in files.items() if not p(path).is_file() or p(path).read_text() != text}
    return changed | {path for path in OWNED if path not in files and p(path).exists()}


def units_changed(paths):
    return {RESTARTS[path] for path in paths if path in RESTARTS}


def setup(config):
    """Writes every owned file that differs, removes those of turned-off features,
    reloads systemd and AppArmor as needed. Returns the changed host paths."""
    p(DATA).mkdir(parents=True, exist_ok=True)
    os.chmod(p(DATA), 0o700)
    chown(p(DATA), "ow")
    files = render(config, budgets())
    # The reloads these writes need are recorded first, so a failure on the way leaves
    # them due for the next setup, which finds no file left to rewrite.
    due = set(read_json(SETUP_DUE, []))
    ahead = diff(files)
    if any(path.startswith("/etc/systemd/") for path in ahead):
        due.add("daemon-reload")
    if "/etc/apparmor.d/open-workspaces" in ahead:
        due.add("apparmor")
    if due:
        write_json(SETUP_DUE, sorted(due))
    changed = set()
    for path, (text, mode) in files.items():
        if write_atomic(path, text, mode):
            changed.add(path)
    for path in OWNED:
        if path not in files and p(path).exists():
            unit = Path(path).name
            if unit.endswith((".service", ".timer")):
                run(["systemctl", "disable", "--now", unit], check=False)
            p(path).unlink()
            changed.add(path)
    if "daemon-reload" in due:
        systemctl("daemon-reload")
    if "apparmor" in due:
        run(["apparmor_parser", "-r", "/etc/apparmor.d/open-workspaces"])
    if due:
        p(SETUP_DUE).unlink()
    return changed


# ---------------------------------------------------------------- tailscale


def tailscale_status():
    out = run(["tailscale", "status", "--json"], check=False)
    try:
        return json.loads(out)
    except ValueError:
        return {}


def tailscale(config, old=None):
    """Installs and joins Tailscale (first boot of a volume), else applies hostname and
    SSH. Returns (state, hostname_changed). The auth key goes to a root-only file under
    /run, by file reference, and is deleted; it is never printed."""
    ts = config["tailscale"]
    hostname = ts["hostname"]
    if not ts["secretArn"]:
        return "skipped", False
    if not ok(["sh", "-c", "command -v tailscale"]):
        codename = re.search(r"^VERSION_CODENAME=(\S+)", p("/etc/os-release").read_text(), re.M).group(1)
        run(["curl", "-fsSL", f"https://pkgs.tailscale.com/stable/ubuntu/{codename}.noarmor.gpg",
             "-o", "/usr/share/keyrings/tailscale-archive-keyring.gpg"])
        run(["curl", "-fsSL", f"https://pkgs.tailscale.com/stable/ubuntu/{codename}.tailscale-keyring.list",
             "-o", "/etc/apt/sources.list.d/tailscale.list"])
        run(["apt-get", "update", "-q"])
        run(["apt-get", "install", "-y", "-q", "tailscale"])
    ssh = "true" if ts["ssh"] else "false"
    state = tailscale_status().get("BackendState", "")
    for _ in range(30):
        if state not in ("", "NoState", "Starting"):
            break
        time.sleep(1)
        state = tailscale_status().get("BackendState", "")
    if state == "Running":
        # Explicit values: state kept from an earlier host must not keep an old setting.
        run(["tailscale", "set", f"--ssh={ssh}", f"--hostname={hostname}"])
        return "joined", bool(old) and old["tailscale"]["hostname"] != hostname
    region = ts["secretArn"].split(":")[3]
    key = Path(run(["mktemp", "-p", "/run", "ow-tailscale.XXXXXX"]).strip())
    try:
        # Piped: some AWS CLI builds write nothing when stdout is a regular file.
        value = run(["/usr/local/bin/aws", "secretsmanager", "get-secret-value", "--region", region,
                     "--secret-id", ts["secretArn"], "--query", "SecretString", "--output", "text"], check=False).strip()
        if not value:
            say("tailscale: no auth key in the secret; not joining")
            return "failed", False
        key.write_text(value)
        joined = ok(["tailscale", "up", "--timeout=60s", f"--auth-key=file:{key}", f"--hostname={hostname}", f"--ssh={ssh}"])
    finally:
        key.unlink(missing_ok=True)
    return ("joined" if joined else "failed"), False


# ---------------------------------------------------------------- pins and releases


def pin(name):
    """The release directory name a pin points to, or None."""
    link = p(f"{SRV}/run/{name}" if name in PINS else f"{SRV}/{name}")
    return os.readlink(link).rsplit("/", 1)[-1] if link.is_symlink() else None


def set_pin(name, release):
    link = p(f"{SRV}/run/{name}" if name in PINS else f"{SRV}/{name}")
    link.parent.mkdir(parents=True, exist_ok=True)
    target = f"../releases/{release}" if name in PINS else f"releases/{release}"
    tmp = link.with_name(f".{link.name}.new")
    tmp.unlink(missing_ok=True)
    os.symlink(target, tmp)
    os.replace(tmp, link)
    fd = os.open(link.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def staged(release):
    """A release's STAGED record (version, commit, ref, ow_sha256), or None if incomplete."""
    return read_json(f"{SRV}/releases/{release}/STAGED") if release else None


def pins():
    return {name: pin(name) for name in (*PINS, "current")}


def versions():
    return {name: (staged(release) or {}).get("version") for name, release in pins().items()}


def pending():
    return read_json(PENDING, {})


def mark_pending(units, reason):
    if units:
        now = pending()
        now.update({unit: reason for unit in units})
        write_json(PENDING, now)


def clear_pending(units):
    now = pending()
    if any(unit in now for unit in units):
        write_json(PENDING, {k: v for k, v in now.items() if k not in units})


# ---------------------------------------------------------------- readiness


def tailnet():
    """The tailnet IPv4 once Tailscale is Running, "wait" while it connects, "none" when not joined."""
    if not ok(["sh", "-c", "command -v tailscale"]):
        return "none"
    s = tailscale_status()
    state = s.get("BackendState")
    ips = [ip for ip in (s.get("TailscaleIPs") or []) if "." in ip]
    if state in ("NoState", "Starting"):
        return "wait"
    return ips[0] if state == "Running" and ips else "none"


def listening():
    out = run(["ss", "-Hltn", "sport = :8790"], check=False)
    return out.split()[3] if out.split() else ""


def local_node():
    # The enrolled ID: a replaced host keeps its runner credential, not its hostname.
    return (read_json(f"{RUNNER}/node.json") or read_json(f"{DATA}/host.json") or {}).get("node")


def dashboard_answers():
    try:
        urllib.request.urlopen("http://127.0.0.1:8787/", timeout=5)
        return True
    except urllib.error.HTTPError as e:
        return e.code < 500  # unauthenticated requests get 401, which proves it serves
    except OSError:
        return False


def bootstrap_done(wait):
    if ok(["sh", "-c", "command -v cloud-init"]):
        code = subprocess.run(["timeout", str(wait), "cloud-init", "status", "--wait"], capture_output=True).returncode
        if code == 124:
            raise RuntimeError(f"cloud-init still running after {wait} s")
    # The status file decides: cloud-init keeps reporting a failed first run that a
    # later stage 0 run fixed.
    status = p(STATUS).read_text().strip() if p(STATUS).exists() else "missing"
    if not status.startswith("ok"):
        raise RuntimeError(f"bootstrap status: {status} (log: /var/log/open-workspaces-bootstrap.log)")


def role():
    return (read_json(CONFIG) or {}).get("role", "host")


def agent_report():
    """A runner host's view of itself (no controller to ask): agent.json, which the
    agent writes once the controller kept its channel open after a heartbeat."""
    report = read_json(f"{RUNNER}/agent.json") or {}
    fresh = report.get("online") is True and time.time() - report.get("updated", 0) < 15
    return report, fresh


def node_online(expect_agent=None, expect_worker=None):
    if role() == "runner":
        report, fresh = agent_report()
        return (fresh and (expect_agent is None or report.get("version") == expect_agent)
                and (expect_worker is None or (report.get("worker") or {}).get("version") == expect_worker))
    node = local_node()
    for entry in ow("node", "list", timeout=30):
        if entry["id"] != node:
            continue
        cap = entry.get("capabilities") or {}
        return (entry.get("online") is True
                and (expect_agent is None or cap.get("version") == expect_agent)
                and (expect_worker is None or (cap.get("worker") or {}).get("version") == expect_worker))
    return False


def ready(wait=600, images=False, booting=False):
    """Waits until the worker answers, the local runner is online and the controller
    listens on the tailnet address (once on the tailnet), then the dashboard; with
    images, also the developer image. A controller that started before the tailnet is
    restarted: a control-plane restart leaves the boxes running. On a runner host:
    the worker, then (once enrolled) its agent online with the controller."""
    deadline = time.monotonic() + wait
    if not booting:  # boot runs inside cloud-init, before its own status is written
        bootstrap_done(wait)
    if role() == "runner":
        runner_ready(deadline, wait)
    else:
        controller_ready(deadline, wait)
    if images:
        unit = "open-workspaces-images.service"
        if not unit_exists(unit):
            raise RuntimeError("no developer images configured (developerImages)")
        while run(["systemctl", "show", "-p", "ActiveState", "--value", unit], check=False).strip() == "activating":
            if time.monotonic() > deadline:
                raise RuntimeError(f"the developer image is still building after {wait} s")
            time.sleep(5)
        built = read_json(f"{SRV}/run/worker/assets/guest/ubuntu.json") or {}
        if not built.get("developer_user"):
            raise RuntimeError("the ubuntu developer image is not built; see systemctl status open-workspaces-images")
        say("developer image: ubuntu")


def runner_ready(deadline, wait):
    """The worker answers; once enrolled, the agent is online with the controller."""
    config = validate(read_json(CONFIG))
    while True:
        try:
            up = ow("status", timeout=30).get("internet") is True
        except (RuntimeError, ValueError, AttributeError, subprocess.TimeoutExpired):
            up = False
        if up and not p(f"{RUNNER}/node.json").exists() and not p(JOIN).exists():
            say(f"worker ready; not enrolled yet: ow-aws --host {config['runner']['controller']} "
                f"runner-add {config['runner']['node']}")
            return
        if up and node_online():
            break
        if time.monotonic() > deadline:
            raise RuntimeError(f"runner not online with its controller after {wait} s; see journalctl -u {UNITS['agent']}")
        time.sleep(2)
    say(f"runner {local_node()} online with controller {controller_url()}")


def controller_url():
    env = dict(line.split("=", 1) for line in p(CONTROLLER_ENV).read_text().split() if "=" in line) \
        if p(CONTROLLER_ENV).exists() else {}
    return env.get("OW_CONTROLLER", "")


def controller_ready(deadline, wait):
    rebound = False
    addr = ""
    while True:
        t = tailnet()
        try:
            up = t != "wait" and ow("status", timeout=30).get("internet") is True and node_online()
        except (RuntimeError, ValueError, AttributeError, subprocess.TimeoutExpired):
            up = False
        if up:
            addr = listening()
            if addr and (t == "none" or addr == f"{t}:8790"):
                break
            if addr and not rebound:
                say(f"the controller started before the tailnet ({addr}); restarting it on {t}")
                systemctl("restart", UNITS["agent"])
                rebound = True
        if time.monotonic() > deadline:
            raise RuntimeError(f"runner not ready after {wait} s; see journalctl -u {UNITS['agent']}")
        time.sleep(2)
    say(f"controller listening on: {addr}")
    if unit_exists(UNITS["dashboard"]) and t != "none":
        while not dashboard_answers():
            if time.monotonic() > deadline:
                raise RuntimeError(f"dashboard not answering after {wait} s")
            time.sleep(2)
        name = (tailscale_status().get("Self") or {}).get("DNSName", "").rstrip(".")
        say(f"dashboard: https://{name}/")


# ---------------------------------------------------------------- apply and boot


def fetch_config():
    env = dict(line.split("=", 1) for line in p(HOST_ENV).read_text().split() if "=" in line)
    out = run(["/usr/local/bin/aws", "ssm", "get-parameter", "--region", env["OW_REGION"],
               "--name", env["OW_CONFIG_PARAMETER"], "--query", "Parameter.Value", "--output", "text"])
    try:
        return json.loads(out)
    except ValueError:
        raise Refused("config: the SSM parameter is not JSON")


def services():
    """The optional oneshots and timers: idempotent, so started on every apply."""
    if unit_exists("open-workspaces-images.service"):
        systemctl("enable", "open-workspaces-images.service")
        systemctl("start", "--no-block", "open-workspaces-images.service")
    if unit_exists("ow-backup.timer"):
        systemctl("enable", "--now", "ow-backup.timer")


def act(config, old):
    """Class 0 work, and the dashboard restart a login change needs; returns the Tailscale state.
    Work it cannot finish stays pending, so a retry after a failure here still does it."""
    if "dashboard" in pending():
        if unit_exists(UNITS["dashboard"]):
            # Logins are read once at start; a dashboard restart drops only its own streams.
            systemctl("enable", UNITS["dashboard"])
            systemctl("restart", "--no-block", UNITS["dashboard"])
        clear_pending({"dashboard"})
    services()
    state, renamed = tailscale(config, old)
    if renamed:
        mark_pending({"agent"}, "tailnet hostname changed")  # the controller's certificate names it
    if p("/proc/mounts").exists() and " /srv/ow btrfs " in p("/proc/mounts").read_text():
        run(["btrfs", "filesystem", "resize", "max", "/srv/ow"], check=False)
    return state


def apply_config():
    try:
        old = validate(read_json(CONFIG))
    except Refused:
        old = None
    config = validate(fetch_config())
    # Recorded before anything is written: a retry after a failure below (in setup too)
    # finds no file left to rewrite.
    mark_pending(units_changed(diff(render(config, budgets()))), "config changed")
    changed = setup(config)
    act(config, old)
    # Last: until everything above succeeded, a retry compares against the old config.
    write_json(CONFIG, config)
    for line in sorted(changed):
        say(f"changed {line}")
    for unit, reason in sorted(pending().items()):
        cls = 2 if unit == "worker" else 1
        say(f"pending: restart {UNITS[unit]} ({reason}; class {cls}): ow-host upgrade")
    current = staged(pin("current")) or {}
    if current.get("ref") != config["release"]["ref"]:
        say(f"pending: release {config['release']['ref']} (running {current.get('ref')}): ow-host upgrade")
    return 0


def boot():
    """Stage 0's last step on a new instance: nothing of ours runs yet, so every pin
    moves to the configured release before the units start."""
    import upgrade

    try:
        config = validate(read_json(CONFIG))
        upgrade.migrate_layout_v1()
        target = upgrade.fetch(config)
        release = upgrade.stage(target)
        for name in (*PINS, "current"):
            set_pin(name, release)
        setup(config)
        state, _ = tailscale(config)
        say(f"tailscale: {state}")
        write_json(PENDING, {})
        # The recover unit runs at later boots; this one recovers below, as a new instance.
        systemctl("enable", "open-workspaces-recover.service")
        units = [UNITS["worker"], UNITS["agent"]]
        if unit_exists(UNITS["dashboard"]):
            # It waits, retrying, until the host is on the tailnet.
            units.append(UNITS["dashboard"])
        systemctl("enable", *units)
        systemctl("start", "--no-block", *units)
        services()
        ready(600, booting=True)
        if config["role"] == "runner" and not p(f"{RUNNER}/node.json").exists():
            try:  # a join left waiting before this host was ready
                enrol()
            except Refused as e:
                say(f"not enrolled yet: {e}")
        with upgrade.locked():
            upgrade.recover(boot=True)
        upgrade.prune()
        p(STATUS).write_text(f"ok tailscale={state}\n")
        say(f"bootstrap done tailscale={state}")
        return 0
    except BaseException:
        p(STATUS).parent.mkdir(parents=True, exist_ok=True)
        p(STATUS).write_text("failed\n")
        raise


# ---------------------------------------------------------------- runner enrolment


def secret(arn_or_name, region):
    """A Secrets Manager secret's current value, "" when it has none yet."""
    return run(["/usr/local/bin/aws", "secretsmanager", "get-secret-value", "--region", region,
                "--secret-id", arn_or_name, "--query", "SecretString", "--output", "text"], check=False).strip()


def put_secret(name, value, region):
    """Writes value through a root-only file: never in an argument or a log."""
    path = Path(run(["mktemp", "-p", "/run", "ow-join.XXXXXX"]).strip())
    try:
        path.write_text(json.dumps(value))
        run(["/usr/local/bin/aws", "secretsmanager", "put-secret-value", "--region", region,
             "--secret-id", name, "--secret-string", f"file://{path}"])
    finally:
        path.unlink(missing_ok=True)


def region():
    env = dict(line.split("=", 1) for line in p(HOST_ENV).read_text().split() if "=" in line)
    return env["OW_REGION"]


def invite_runner(node, memory, slots, cpus):
    """On the controller host: mints NODE's single-use join (10 minutes) and writes it,
    with the controller's CA, into NODE's join secret. The Runner component lets only
    this host write it and only NODE's host read it. An enrolled node is left alone."""
    if role() != "host":
        raise Refused("invite-runner runs on a controller host")
    if not NODE.match(node) or node == "local":
        raise Refused(f"{node!r} is not a node ID")
    for entry in ow("node", "list", timeout=30):
        if entry["id"] == node and entry.get("online"):
            say(f"{node} is enrolled and online; nothing to do")
            return 0
    saved = read_json(f"{DATA}/host.json") or {}
    url = saved.get("controller", "")
    if not url.startswith("https://") or "127.0.0.1" in url or "localhost" in url:
        raise Refused(f"the controller advertises {url!r}, which no other host can reach: join the tailnet first")
    out = f"{DATA}/invite-{node}-{os.getpid()}.json"
    try:
        # Refused for an ID that holds a credential or was revoked: that runner keeps its
        # own identity (data volume), or a lost one comes back under a new node ID.
        ow("--data-dir", CONTROLLER, "node-join", node, "--output", out, "--ttl", str(JOIN_TTL),
           "--memory", str(memory), "--slots", str(slots), "--cpus", str(cpus), "--controller", url, timeout=60)
        join = json.loads(p(out).read_text())
    finally:
        p(out).unlink(missing_ok=True)
    put_secret(f"open-workspaces/{node}/join", {"join": join, "ca": p(f"{DATA}/tls/ca.pem").read_text()}, region())
    say(f"join for {node} written to its secret; it expires in {JOIN_TTL // 60} minutes")
    return 0


def enrol(wait=120):
    """On a runner host: redeems the join the controller left in this runner's secret,
    keeps the controller's CA and URL on the data volume, starts the agent and waits
    until it is online. The secret is overwritten once used."""
    config = validate(read_json(CONFIG))
    if config["role"] != "runner":
        raise Refused("enrol runs on a runner host (role runner)")
    me = config["runner"]
    if p(f"{RUNNER}/node.json").exists():
        say(f"enrolled as {local_node()}")
        return 0
    raw = secret(me["joinSecretArn"], me["joinSecretArn"].split(":")[3])
    try:
        value = json.loads(raw) if raw else {}
    except ValueError:
        value = {}
    join = value.get("join") or {}
    if not join:
        raise Refused(f"no join waiting in {me['joinSecretArn']}: run ow-aws --host {me['controller']} "
                      f"runner-add {me['node']}")
    url = str(join.get("controller", ""))
    if join.get("node") != me["node"] or not re.fullmatch(r"https://[A-Za-z0-9.-]+:\d+", url):
        raise Refused(f"the join in the secret is for {join.get('node')!r} at {url!r}, not {me['node']}")
    if join.get("expires", 0) < time.time() + 15:
        raise Refused(f"the join for {me['node']} has expired: run ow-aws --host {me['controller']} runner-add again")
    ca = str(value.get("ca", ""))
    if not ca.startswith("-----BEGIN CERTIFICATE-----"):
        raise Refused("the join secret holds no controller CA")
    write_atomic(CA, ca, 0o600, owner="ow")
    write_atomic(CONTROLLER_ENV, f"OW_CONTROLLER={url}\n", 0o600, owner="ow")
    write_atomic(JOIN, json.dumps(join), 0o600, owner="ow")
    systemctl("enable", UNITS["agent"])
    systemctl("restart", UNITS["agent"])
    deadline = time.monotonic() + wait
    while not (p(f"{RUNNER}/node.json").exists() and node_online()):
        if time.monotonic() > deadline:
            # A join redeemed but never answered is spent: this ID needs a revoke and a new one.
            systemctl("stop", UNITS["agent"], check=False)
            p(JOIN).unlink(missing_ok=True)
            tail = run(["journalctl", "-u", UNITS["agent"], "-n", "20", "--no-pager"], check=False)
            raise RuntimeError(f"{me['node']} did not come online with {url} in {wait} s:\n{tail}")
        time.sleep(2)
    put_secret(me["joinSecretArn"], {"used": me["node"], "at": int(time.time())}, me["joinSecretArn"].split(":")[3])
    say(f"runner {me['node']} enrolled and online with {url}")
    return 0


def status():
    v = versions()
    out = {"role": role(), "pins": pins(), "versions": v, "budget": budgets(), "node": local_node(),
           "units": {name: run(["systemctl", "is-active", unit], check=False).strip() or "unknown"
                     for name, unit in UNITS.items() if unit_exists(unit)},
           "pending": pending(), "upgrade": read_json(f"{DATA}/upgrade.json")}
    print(json.dumps(out, indent=2))
    return 0


def main(argv):
    parser = argparse.ArgumentParser(prog="ow-host", description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("boot")
    sub.add_parser("setup")
    sub.add_parser("apply")
    sub.add_parser("status")
    sub.add_parser("recover")
    invite = sub.add_parser("invite-runner")
    invite.add_argument("node")
    for limit in ("--memory", "--slots", "--cpus"):
        invite.add_argument(limit, type=int, required=True)
    e = sub.add_parser("enrol")
    e.add_argument("--wait", type=int, default=120)
    r = sub.add_parser("ready")
    r.add_argument("--wait", type=int, default=600)
    r.add_argument("--images", action="store_true")
    restart = sub.add_parser("restart")
    which = restart.add_mutually_exclusive_group(required=True)
    which.add_argument("--control-plane", action="store_true")
    which.add_argument("--worker", action="store_true")
    u = sub.add_parser("upgrade")
    u.add_argument("ref", nargs="?")
    u.add_argument("--plan", action="store_true", help="print the plan and change nothing")
    u.add_argument("--accept-cold", action="store_true", help="allow boxes whose captures cannot resume to boot cold")
    u.add_argument("--max-class", type=int, choices=range(4), default=2)
    u.add_argument("--controller-version", help="runner hosts: the release the controller's agent runs")
    render_cmd = sub.add_parser("render", help=argparse.SUPPRESS)
    render_cmd.add_argument("--config", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "render":
            # Used by an older release's upgrader to diff this release's files.
            config = validate(json.loads(Path(args.config).read_text()))
            files = {k: v[0] for k, v in render(config, budgets()).items()}
            print(json.dumps({"files": files, "restarts": RESTARTS}))
            return 0
        if args.command == "boot":
            return boot()
        if args.command == "setup":
            for path in sorted(setup(validate(read_json(CONFIG)))):
                say(f"changed {path}")
            return 0
        if args.command == "apply":
            return apply_config()
        if args.command == "status":
            return status()
        if args.command == "invite-runner":
            return invite_runner(args.node, args.memory, args.slots, args.cpus)
        if args.command == "enrol":
            return enrol(args.wait)
        if args.command == "ready":
            ready(args.wait, args.images)
            return 0
        import upgrade

        if args.command == "recover":
            with upgrade.locked():
                upgrade.recover()
            return 0
        if args.command == "restart":
            if args.control_plane:
                systemctl("restart", UNITS["agent"])
                clear_pending({"agent"})
                ready(300)
                return 0
            return upgrade.restart_worker()
        return upgrade.upgrade(args.ref, plan_only=args.plan, accept_cold=args.accept_cold, max_class=args.max_class,
                               controller=args.controller_version)
    except Refused as e:
        print(f"refused: {e}", file=sys.stderr)
        return 3


if __name__ == "__main__":
    sys.path.insert(0, str(HERE))
    import ow_host  # one module copy, shared with upgrade.py

    sys.exit(ow_host.main(sys.argv[1:]))
