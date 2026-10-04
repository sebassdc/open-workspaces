#!/usr/bin/env python3
"""Offline, fail-closed project-only ingress plan/rollback. Never calls Cloudflare.
Input is a private captured state, not guessed identifiers. Planner applies the
reviewed app intent/config patch after independent acceptance and private backups.
"""
import argparse
import copy
import fnmatch
import subprocess
import stat
import hashlib
import json
import os
from pathlib import Path
import re

NODE_REGEX = r'^/_nodes/(enroll|node/[A-Za-z0-9_-]+|job/[A-Za-z0-9_-]+/[A-Za-z0-9_-]+)$'
OLD_CLI_REGEX = r'^/cli/(install\.sh|ow-(linux-amd64|darwin-(arm64|amd64))(\.sha256)?)$'
CLI_REGEX = r'^/cli/(install\.sh|ow-(linux-amd64|darwin-(arm64|amd64))(\.sha256)?|host-manifest\.json|host/(firecracker|vmlinux|base\.ext4|ow-guest|slirp4netns))$'
PUBLIC_POLICY = {'name': 'Fixed node protocol only', 'decision': 'bypass', 'include': [{'everyone': {}}], 'exclude': [], 'require': [], 'precedence': 1}

def fingerprint(state):
    return hashlib.sha256(json.dumps(state, sort_keys=True, separators=(',', ':')).encode()).hexdigest()

def normalize_policy(policy):
    keys = {'id', 'name', 'decision', 'include', 'exclude', 'require', 'precedence', 'session_duration'}
    metadata = {'created_at', 'updated_at', 'app_id', 'reusable'}
    if set(policy) - keys - metadata:
        raise ValueError('Unknown policy field requires explicit review')
    return {k: policy[k] for k in sorted(keys) if k in policy}

def private_regular(path):
    path = Path(path)
    if not path.is_absolute():
        raise ValueError('Absolute private file required')
    for part in [path, *path.parents]:
        if part.is_symlink():
            raise ValueError('Symlink private file/parent refused')
    m = path.stat()
    if not stat.S_ISREG(m.st_mode) or m.st_uid != os.getuid() or m.st_mode & 0o077:
        raise ValueError('Owned/private regular file required')
    return path

def approved_app(state, approved, key):
    expected = approved[key]
    actual = next(a for a in state['apps'] if a['id'] == state[key + '_id'])
    for field in ['id', 'domain', 'account_id', 'type', 'aud', 'team_domain', 'session_duration']:
        if actual.get(field) != expected.get(field) or field not in expected:
            raise ValueError('Approved application identity/session/audience drift')
    if [normalize_policy(p) for p in actual['policies']] != [normalize_policy(p) for p in expected['policies']]:
        raise ValueError('Approved application policy drift')

def app(state, key, domain, public=False):
    identity = state.get(key)
    matches = [a for a in state['apps'] if a.get('domain') == domain]
    if not identity or len(matches) != 1 or matches[0]['id'] != identity:
        raise ValueError('Missing/duplicate/foreign recorded application')
    if matches[0].get('account_id') != state['account_id']:
        raise ValueError('Foreign application account')
    policies = matches[0].get('policies', [])
    if public:
        if len(policies) != 1 or policies[0].get('decision') != 'bypass' or policies[0].get('include') != [{'everyone': {}}] or policies[0].get('exclude', []) or policies[0].get('require', []):
            raise ValueError('Public policy drift')
    elif not policies or any(p.get('decision') == 'bypass' for p in policies):
        raise ValueError('Protected dashboard policy drift')
    return matches[0]

def validate(state, approved, allow_partial=False):
    host = state['hostname']
    if state.get('capture_schema') != 1 or state.get('app_inventory_complete') is not True:
        raise ValueError('Complete paginated normalized application capture required')
    for key in ['account_id', 'zone_id', 'tunnel_id', 'hostname', 'dashboard_app_id', 'cli_app_id', 'dashboard_audience', 'team_domain']:
        if state.get(key) != approved.get(key) or key not in approved:
            raise ValueError('Independently saved approved ownership mismatch')
    if not re.fullmatch(r'[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+', host):
        raise ValueError('Invalid hostname')
    for key in ['account_id', 'zone_id', 'tunnel_id', 'dashboard_app_id', 'cli_app_id']:
        if not isinstance(state.get(key), str) or not state[key]:
            raise ValueError('Missing recorded ownership')
    ownership = state['ownership']
    if any(ownership.get(k) != state[k] for k in ['account_id', 'zone_id', 'tunnel_id', 'hostname']):
        raise ValueError('Account/zone/tunnel/hostname ownership mismatch')
    app(state, 'dashboard_app_id', host)
    app(state, 'cli_app_id', host + '/cli/*', True)
    approved_app(state, approved, 'dashboard_app')
    approved_app(state, approved, 'cli_app')
    # Unknown path apps can override effective Access selection. Refuse to guess.
    allowed = {host, host + '/cli/*', host + '/_nodes/*'}
    for a in state['apps']:
        domain = a.get('domain', '')
        app_host = domain.split('/')[0]
        if fnmatch.fnmatchcase(host.lower(), app_host.lower()) and domain not in allowed:
            raise ValueError('Overlapping/shadowing Access application')
    nodes = [a for a in state['apps'] if a.get('domain') == host + '/_nodes/*']
    if nodes:
        app(state, 'node_app_id', host + '/_nodes/*', True)
        if [{k:v for k,v in normalize_policy(p).items() if k!='id'} for p in nodes[0]['policies']] != [PUBLIC_POLICY]:
            raise ValueError('Node policy drift')
    elif state.get('node_app_id'):
        raise ValueError('Recorded node application disappeared')
    ingress = state['tunnel']['ingress']
    if state['tunnel'].get('tunnel') != state['tunnel_id']:
        raise ValueError('Tunnel identity mismatch')
    fallback = [i for i, route in enumerate(ingress) if route.get('hostname') == host and not route.get('path')]
    if len(fallback) != 1:
        raise ValueError('Missing/duplicate protected hostname fallback')
    protected = ingress[fallback[0]]
    if protected.get('service') != 'http://127.0.0.1:8787' or not protected.get('originRequest', {}).get('access', {}).get('required'):
        raise ValueError('Protected origin gate changed')
    if protected['originRequest']['access'].get('audTag') != state['dashboard_audience']:
        raise ValueError('Protected audience mismatch')
    trust = state['native_trust']
    native = approved['native']
    if trust != {k:native[k] for k in ['caPool','originServerName']}:
        raise ValueError('Approved native endpoint trust drift')
    defaults = state['tunnel'].get('originRequest', {})
    if defaults.get('noTLSVerify') or any(k in defaults and defaults[k] != trust[k] for k in ['caPool','originServerName']):
        raise ValueError('Unsafe inherited TLS defaults; no global mutation allowed')
    ca = private_regular(trust['caPool'])
    cert = private_regular(native['serverCert'])
    if hashlib.sha256(ca.read_bytes()).hexdigest() != native['ca_sha256'] or hashlib.sha256(cert.read_bytes()).hexdigest() != native['server_cert_sha256']:
        raise ValueError('Approved native certificate fingerprint drift')
    checked = subprocess.run(['openssl','verify','-CAfile',str(ca),'-verify_hostname',trust['originServerName'],str(cert)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={'PATH':'/usr/bin:/bin','LANG':'C'}, timeout=10)
    if checked.returncode:
        raise ValueError('Native TLS certificate/hostname verification failed')
    if not trust.get('caPool', '').startswith('/') or not re.fullmatch(r'[A-Za-z0-9.-]+', trust.get('originServerName', '')) or trust.get('noTLSVerify'):
        raise ValueError('Explicit existing private-CA TLS trust/server name required')
    cli = {'hostname': host, 'path': OLD_CLI_REGEX, 'service': 'http://127.0.0.1:8787', 'originRequest': {'access': {'required': False}}}
    cli_new = dict(cli, path=CLI_REGEX)
    node = {'hostname': host, 'path': NODE_REGEX, 'service': 'https://127.0.0.1:8790', 'originRequest': {**trust, 'noTLSVerify': False, 'access': {'required': False}}}
    selected = []
    for i, route in enumerate(ingress):
        if route.get('hostname') not in [None, host] and route.get('hostname'):
            if '*' in route['hostname'] and fnmatch.fnmatchcase(host.lower(),route['hostname'].lower()):
                raise ValueError('Wildcard routing ownership requires manual review')
            continue
        if not route.get('hostname') and i < fallback[0]:
            raise ValueError('Earlier global fallback shadows project')
        if route.get('hostname') == host and route.get('path'):
            if route not in [cli, cli_new, node] or i > fallback[0]:
                raise ValueError('Foreign/drifted/shadowed project path ingress')
            selected.append(route)
    if sum(r in [cli, cli_new] for r in selected) != 1 or selected.count(node) > 1:
        raise ValueError('Missing/duplicate exact owned ingress')
    if not allow_partial and bool(nodes) != (node in selected):
        raise ValueError('Partial node state: apply only recorded compensation before re-planning')
    return cli, cli_new, node

def prepare(state, approved):
    old, cli, node = validate(state, approved)
    before = copy.deepcopy(state)
    after = copy.deepcopy(state)
    ingress = after['tunnel']['ingress']
    if old in ingress:
        ingress[ingress.index(old)] = cli
    if node not in ingress:
        fallback = next(i for i,r in enumerate(ingress) if r.get('hostname') == state['hostname'] and not r.get('path'))
        ingress.insert(fallback, node)
    create = None if state.get('node_app_id') else {'name': 'Open Workspaces Node Protocol', 'type': 'self_hosted', 'domain': state['hostname'] + '/_nodes/*', 'app_launcher_visible': False, 'policies': [PUBLIC_POLICY]}
    return {'version': 1, 'approved':copy.deepcopy(approved), 'before': before, 'precondition_sha256': fingerprint(before), 'tunnel_after': after['tunnel'], 'node_app_create': create,
            'compensation': 'If app creation succeeds but ingress write fails, delete only the returned app ID after checking its exact recorded domain/policy/account. Never adopt an unrecorded collision. Save the returned ID privately before ingress apply.',
            'worker_boundary': 'Restart scoped gateway/controller channels only. Never stop workers or run wholesale manage.py stop/rollback. Ubuntu must remain live.'}

def rollback(plan, current, created_app_id=None):
    # Preserve unrelated later ingress/apps. Refuse changed owned entries.
    before = plan['before']
    approved = plan['approved']
    _, cli, node = validate(before, approved)
    validate(current, approved, allow_partial=True)
    result = copy.deepcopy(current)
    for key in ['account_id', 'zone_id', 'tunnel_id', 'hostname']:
        if current[key] != before[key]:
            raise ValueError('Rollback ownership changed')
    routes = result['tunnel']['ingress']
    if node not in before['tunnel']['ingress']:
        owned = [r for r in routes if r.get('hostname') == before['hostname'] and r.get('path') == NODE_REGEX]
        if owned not in [[], [node]]:
            raise ValueError('Owned node route changed; rollback refused')
        if owned:
            routes.remove(node)
    original_cli = next(r for r in before['tunnel']['ingress'] if r.get('hostname') == before['hostname'] and r.get('path') in [OLD_CLI_REGEX, CLI_REGEX])
    owned_cli = [r for r in routes if r.get('hostname') == before['hostname'] and r.get('path') in [OLD_CLI_REGEX, CLI_REGEX]]
    if owned_cli not in [[cli], [original_cli]]:
        raise ValueError('Owned CLI route changed; rollback refused')
    if cli in routes:
        routes[routes.index(cli)] = original_cli
    if plan['node_app_create']:
        if not created_app_id:
            raise ValueError('Explicit privately recorded created app ID required')
        matches = [a for a in result['apps'] if a.get('domain') == before['hostname'] + '/_nodes/*' or a.get('id') == created_app_id]
        if matches:
            if len(matches) != 1 or matches[0]['id'] != created_app_id or current.get('node_app_id') != created_app_id:
                raise ValueError('Rollback created app identity changed')
            existing = app(current, 'node_app_id', before['hostname'] + '/_nodes/*', True)
            if [{k:v for k,v in normalize_policy(p).items() if k!='id'} for p in existing['policies']] != plan['node_app_create']['policies']:
                raise ValueError('Created node policy changed')
            result['apps'].remove(existing)
            result.pop('node_app_id', None)
        elif current.get('node_app_id'):
            raise ValueError('Recorded node app missing; rollback refused')
    validate(result, approved)
    return result

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--state', type=Path, required=True)
    p.add_argument('--output', type=Path)
    p.add_argument('--approved', type=Path, required=True)
    p.add_argument('--check-plan', type=Path)
    p.add_argument('--rollback-plan', type=Path)
    p.add_argument('--created-app-id')
    a = p.parse_args()
    state = json.loads(private_regular(a.state.absolute()).read_text())
    approved = json.loads(private_regular(a.approved.absolute()).read_text())
    if a.check_plan:
        plan=json.loads(private_regular(a.check_plan.absolute()).read_text())
        validate(state, approved)
        if plan['approved'] != approved or fingerprint(state) != plan['precondition_sha256']:
            raise SystemExit('Fresh captured state differs; refuse all writes and re-plan')
        print('Fresh-state precondition matches; no mutation performed.')
        raise SystemExit(0)
    if not a.output:
        raise SystemExit('--output required for plan/rollback')
    result = rollback(json.loads(private_regular(a.rollback_plan.absolute()).read_text()), state, a.created_app_id) if a.rollback_plan else prepare(state, approved)
    parent=a.output.absolute().parent
    if parent.is_symlink() or parent.stat().st_uid!=os.getuid() or parent.stat().st_mode & 0o077:
        raise SystemExit('Output parent must be owned/private')
    os.umask(0o077)
    with a.output.open('x') as f:
        json.dump(result, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    print('Private offline plan written; no cloud, tunnel, service or VM action performed.')
