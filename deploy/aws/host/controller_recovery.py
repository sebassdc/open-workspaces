"""Explicit class-1 recovery steps. No enrollment, cloud calls or worker lifecycle."""
import hashlib
import json
import os
from pathlib import Path
import stat
import re
import ssl

import ow_host as h

FREEZE = f"{h.DATA}/controller-recovery.json"
TRANSITION = f"{h.DATA}/runner-transition.json"
# Include maintenance/upgrade entry points; future writers must join this list.
WRITERS = (h.UNITS['agent'], h.UNITS['dashboard'], 'ow-backup.service',
           'ow-recovery.service', 'open-workspaces-images.service',
           'open-workspaces-recover.service', 'ow-gc.service', 'ow-scaler.service')
TIMERS = ('ow-backup.timer', 'ow-recovery.timer', 'ow-gc.timer', 'ow-scaler.timer')


def sync(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def durable(path, value):
    """Persist intent before side effects, including across power loss."""
    target = h.p(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(target.name + '.new')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, target)
    sync(target.parent)


def guard(unit):
    return f'/etc/systemd/system/{unit}.d/90-controller-recovery.conf'


def frozen():
    return h.p(FREEZE).exists()


def guards_ready():
    text = f'[Unit]\nConditionPathExists=!{FREEZE}\n'
    return all(h.p(guard(unit)).is_file() and h.p(guard(unit)).read_text() == text for unit in WRITERS)


def writer_stopped(unit):
    # is-active alone treats activating/deactivating as false while code may run.
    raw = h.run(['systemctl', 'show', unit, '--property=ActiveState,MainPID,ControlPID'])
    fields = dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)
    return fields.get('ActiveState') in ('inactive', 'failed') and fields.get('MainPID') == '0' and fields.get('ControlPID') == '0'


def require_stopped():
    if not guards_ready() or not frozen() or any(not writer_stopped(unit) for unit in WRITERS + TIMERS if h.unit_exists(unit)):
        raise h.Refused('recovery-freeze must finish with all controller/maintenance writers stopped')


def freeze():
    """Retry after any interruption. Does not fence another host or root operator."""
    if not frozen():
        durable(FREEZE, {'schema': 1, 'state': 'freezing', 'role': h.role()})
    for unit in WRITERS:
        # Drop-ins survive render/setup and host reboot; no worker unit is changed.
        h.write_atomic(guard(unit), f'[Unit]\nConditionPathExists=!{FREEZE}\n')
        sync(h.p(guard(unit)))
        sync(h.p(guard(unit)).parent)
    sync(h.p('/etc/systemd/system'))
    h.systemctl('daemon-reload')
    for unit in TIMERS + WRITERS:
        if h.unit_exists(unit):
            h.systemctl('stop', unit)
    require_stopped()
    durable(FREEZE, {'schema': 1, 'state': 'stopped', 'role': h.role()})
    return status()


def status():
    units = {u: not writer_stopped(u) for u in WRITERS + TIMERS if h.unit_exists(u)}
    value = {'frozen': frozen(), 'role': h.role(), 'units': units,
             'stopped': frozen() and guards_ready() and not any(units.values()),
             'worker_pin': h.pin('worker'), 'node': h.local_node(),
             'transition': (h.read_json(TRANSITION) or {}).get('state'),
             'cross_host_fenced': False}
    print(json.dumps(value))
    return 0


def private_input(path):
    source = Path(path)
    info = source.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise h.Refused('recovery input must be an owned private regular file')
    return source.read_text()


def runner(config_file, controller, ca_file):
    """Repoint an existing credential; host -> runner is the only role conversion.

    Leaves all services frozen. An operator reopens the runner only after the
    external single-authority gate; this command never starts any service.
    """
    require_stopped()
    old = h.validate(h.read_json(h.CONFIG))
    new = h.validate(json.loads(private_input(config_file)))
    if old['role'] not in ('host', 'runner') or new['role'] != 'runner':
        raise h.Refused('only an existing host/runner may become a runner')
    if not re.fullmatch(r'https://[A-Za-z0-9.-]+:[0-9]{1,5}', controller):
        raise h.Refused('controller must be an explicit TLS URL with a port')
    from urllib.parse import urlsplit
    if not 1 <= urlsplit(controller).port <= 65535:
        raise h.Refused('invalid controller port')
    hostname = urlsplit(controller).hostname
    target = new['runner']['controller'].lower()
    if hostname != target and not hostname.startswith(target + '.'):
        raise h.Refused('controller URL does not match the configured authority')
    credential = h.p(f'{h.RUNNER}/node.json').read_bytes()
    node = json.loads(credential)
    if node.get('node') != new['runner']['node'] or not re.fullmatch(r'[0-9a-f]{64}', str(node.get('credential', ''))):
        raise h.Refused('preserve the existing node ID and credential; do not re-enroll')
    ca = private_input(ca_file)
    ssl.create_default_context(cadata=ca)
    # Trust is private operator input, not stored in the transition journal.
    digest = lambda b: hashlib.sha256(b).hexdigest()
    budget = h.budgets(old)
    before, after = h.render(old, budget), h.render(new, budget)
    for path in before:
        if 'worker' in path and before[path] != after.get(path):
            raise h.Refused(f'runner conversion would change worker file {path}')
        if 'worker' in path and (not h.p(path).is_file() or h.p(path).read_text() != after[path][0]
                                  or h.p(path).stat().st_mode & 0o777 != after[path][1]):
            raise h.Refused(f'installed worker file differs: {path}; resolve separately')
    # No unrelated tailnet/release/budget/image/storage changes in a role transition.
    for key in ('release', 'tailscale', 'developerImages', 'idleMinutes', 'vcpuOvercommit', 'store', 'backup'):
        if old.get(key) != new.get(key):
            raise h.Refused(f'runner conversion must preserve {key}')
    intent = {'schema': 1, 'state': 'prepared', 'config': new, 'controller': controller,
              'ca_digest': digest(ca.encode()), 'credential_digest': digest(credential),
              'worker_pin': h.pin('worker')}
    previous = h.read_json(TRANSITION)
    if previous and {**previous, 'state': 'prepared'} != intent:
        raise h.Refused('an existing runner transition has different inputs; restore its inputs to resume')
    durable(TRANSITION, intent)
    # Keep the freeze on every failure. Setup never starts a surviving worker unit.
    h.write_atomic(h.CA, ca, 0o600, owner='ow')
    h.write_atomic(h.CONTROLLER_ENV, f'OW_CONTROLLER={controller}\n', 0o600, owner='ow')
    h.setup(new)
    if h.pin('worker') != intent['worker_pin'] or h.p(f'{h.RUNNER}/node.json').read_bytes() != credential:
        raise h.Refused('worker pin or credential changed during conversion; remain frozen')
    # Discard old readiness reports so a recent old-authority heartbeat cannot
    # satisfy readiness after repointing. Node ID/credential and worker stay put.
    for report in ('agent.json', 'node-channel.json'):
        h.p(f'{h.RUNNER}/{report}').unlink(missing_ok=True)
    h.write_json(h.CONFIG, new)
    for path in (h.CA, h.CONTROLLER_ENV, h.CONFIG, '/etc/systemd/system/open-workspaces.service'):
        sync(h.p(path))
        sync(h.p(path).parent)
    durable(TRANSITION, {**intent, 'state': 'configured'})
    return status()
