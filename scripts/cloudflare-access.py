#!/usr/bin/env python3
"""Configure only this project's Access app; keep all live values in ignored data.

The Tunnel login certificate supplies account/zone context. A separate scoped
API token supplies Access permissions. This helper does not publish DNS or start
an Internet listener; configure and verify policy before doing either.
"""
import argparse
import base64
import getpass
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PRIVATE = ROOT / 'data/remote-access'
TOKEN = PRIVATE / 'access-api-token'
STATE = PRIVATE / 'managed-access.json'


def private_write(path, value):
    content = value if isinstance(value, str) else json.dumps(value, indent=2) + '\n'
    temporary = path.with_name(path.name + '.staging')
    with open(temporary, 'w', encoding='utf8') as stream:
        os.chmod(temporary, 0o600)
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def credential():
    cert = (Path.home() / '.cloudflared/cert.pem').read_text()
    payload = re.search(r'-----BEGIN ARGO TUNNEL TOKEN-----\s*(.*?)\s*-----END ARGO TUNNEL TOKEN-----', cert, re.S)
    if not payload:
        raise RuntimeError('Run scripts/cloudflare-login.sh first')
    return json.loads(base64.b64decode(payload.group(1)))


def api(token, path, method='GET', body=None):
    request = urllib.request.Request('https://api.cloudflare.com/client/v4/' + path,
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'},
        data=None if body is None else json.dumps(body).encode(), method=method)
    try:
        with urllib.request.urlopen(request, timeout=25) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        try:
            details = json.loads(error.read())
            codes = ','.join(str(e.get('code')) for e in details.get('errors', []))
        except (ValueError, AttributeError):
            codes = 'unknown'
        # Never echo token, request headers or raw responses to the console.
        raise RuntimeError(f'Cloudflare denied {method}: HTTP {error.code}, codes {codes}') from None
    if not result.get('success'):
        raise RuntimeError('Cloudflare operation returned success=false')
    return result['result']


def account_path(context, suffix):
    return f"accounts/{context['accountID']}/access/{suffix}"


def authorize():
    if TOKEN.exists():
        raise RuntimeError('Access token file already exists; check its permissions with the check command')
    value = getpass.getpass('Paste the scoped Cloudflare Access API token (hidden): ').strip()
    if not value or '\n' in value:
        raise RuntimeError('Empty or invalid token')
    context = credential()
    org = api(value, account_path(context, 'organizations'))
    if not org or not org.get('auth_domain'):
        raise RuntimeError('Finish Cloudflare Zero Trust onboarding first; then retry')
    api(value, account_path(context, 'apps'))
    private_write(TOKEN, value + '\n')
    print('Scoped token saved privately; Access organization/application reads succeeded.')
    print('Application write permission will be checked when configuring the dedicated app.')


def access_token():
    metadata = TOKEN.stat()
    if metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
        raise RuntimeError('Access token must belong to this user and have mode 600')
    return TOKEN.read_text().strip()


def check():
    context = credential()
    token = access_token()
    org = api(token, account_path(context, 'organizations'))
    if not org or not org.get('auth_domain'):
        raise RuntimeError('Zero Trust organization has not been configured')
    apps = api(token, account_path(context, 'apps'))
    print('Access organization is configured; application read permission works.')
    print('Visible applications:', len(apps or []))


def configure(args):
    context = credential()
    token = access_token()
    # The Tunnel login limits zone context; do not adopt another zone or root domain.
    zone = api(context['apiToken'], 'zones/' + context['zoneID'])
    if not args.hostname.endswith('.' + zone['name']) or args.hostname.startswith('*.'):
        raise RuntimeError('Hostname must be a concrete subdomain of the selected login zone')
    if not re.fullmatch(r'[a-z0-9]+(?:[a-z0-9.-]*[a-z0-9])?', args.hostname):
        raise RuntimeError('Invalid hostname')
    if any(not e or e.count('@') != 1 or any(c.isspace() for c in e) for e in args.email):
        raise RuntimeError('Supply explicit valid email addresses')
    upstream = urllib.parse.urlsplit(args.upstream)
    if not (upstream.scheme == 'http' and upstream.hostname == '127.0.0.1'
            and upstream.port and not upstream.username and not upstream.password
            and upstream.path in ('', '/') and not upstream.query and not upstream.fragment):
        raise RuntimeError('Upstream must be a published http://127.0.0.1:PORT guest route')
    org = api(token, account_path(context, 'organizations'))
    team = (org or {}).get('auth_domain', '')
    if not re.fullmatch(r'[a-z0-9-]+\.cloudflareaccess\.com', team):
        raise RuntimeError('Complete Zero Trust onboarding and select a standard team domain first')
    # Select an existing provider; do not silently change global login methods.
    idps = api(token, account_path(context, 'identity_providers')) or []
    login_method = getattr(args, 'login_method', 'onetimepin')
    pin = [item['id'] for item in idps if item.get('type') == login_method]
    if not pin:
        raise RuntimeError('Selected login provider is unavailable; existing identity providers are unchanged')
    emails = sorted(set(e.lower() for e in args.email))
    policy = {'name': 'Open Workspaces allowed users', 'decision': 'allow',
        'include': [{'email': {'email': e}} for e in emails], 'exclude': [], 'require': [],
        'precedence': 1, 'session_duration': '1h'}
    apps = api(token, account_path(context, 'apps') + '?per_page=1000') or []
    matches = [app for app in apps if app.get('domain') == args.hostname]
    managed = json.loads(STATE.read_text()) if STATE.exists() else None
    if matches and (not managed or len(matches) != 1 or matches[0]['id'] != managed['app_id']):
        raise RuntimeError('Refusing to change an Access application not created by this helper')
    if managed and managed['hostname'] != args.hostname:
        raise RuntimeError('This private deployment state already owns another hostname')
    if not matches:
        if managed:
            raise RuntimeError('Previously managed app is missing; inspect before recreating it')
        app = api(token, account_path(context, 'apps'), 'POST', {
            'name': 'Open Workspaces Sandbox', 'type': 'self_hosted', 'domain': args.hostname,
            'session_duration': '1h', 'app_launcher_visible': False,
            'allowed_idps': pin, 'policies': [policy]})
        managed = {'app_id': app['id'], 'hostname': args.hostname}
        # Retain ownership immediately if later verification fails; safe reruns.
        private_write(STATE, managed)
    else:
        app = api(token, account_path(context, 'apps/' + managed['app_id']))
        if app.get('allowed_idps') != pin:
            raise RuntimeError('Login provider configuration changed; inspect before editing')
    endpoint = account_path(context, 'apps/' + managed['app_id'] + '/policies')
    policies = api(token, endpoint) or []
    if len(policies) != 1 or policies[0].get('name') != policy['name']:
        raise RuntimeError('Expected exactly the helper-owned policy; refusing to adopt others')
    if policies[0].get('decision') != 'allow':
        raise RuntimeError('Managed policy changed unexpectedly; inspect first')
    if matches:
        api(token, endpoint + '/' + policies[0]['id'], 'PUT', policy)
        policies = api(token, endpoint)
    actual = policies[0]
    if actual.get('decision') != 'allow' or actual.get('include') != policy['include'] \
            or actual.get('exclude') or actual.get('require'):
        raise RuntimeError('Access policy verification failed; no public route was created')
    gateway = {'hostname': args.hostname, 'team_domain': team, 'audience': app['aud'],
        'allowed_emails': emails, 'upstream': args.upstream}
    private_write(PRIVATE / 'gateway.json', gateway)
    tunnel = json.loads((PRIVATE / 'created-tunnel.json').read_text())
    credentials = PRIVATE / 'credentials/tunnel.json'
    # JSON is valid YAML, avoiding YAML interpolation of user-supplied values.
    config = {'tunnel': tunnel['id'], 'credentials-file': str(credentials),
        'metrics': '127.0.0.1:0', 'ingress': [
            {'hostname': args.hostname, 'service': 'http://127.0.0.1:8787',
             'originRequest': {'access': {'required': True,
                 'teamName': team.removesuffix('.cloudflareaccess.com'), 'audTag': [app['aud']]}}},
            {'service': 'http_status:404'}]}
    private_write(PRIVATE / 'tunnel.json', config)
    print('Dedicated Access policy verified; private gateway/tunnel configs prepared.')
    print('Allowed users:', len(emails))
    print('DNS has not been published. Start and verify the origin gate before routing it.')
    print('Changing the user list requires rerunning configure with the full list and restarting ow serve.')


def main():
    os.umask(0o077)
    PRIVATE.mkdir(parents=True, exist_ok=True)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('authorize', help='Secure hidden token entry; store only after read permission checks')
    sub.add_parser('check', help='Read-only checks of saved Access permissions')
    config = sub.add_parser('configure', help='Create/update only the helper-owned Access app; no DNS publication')
    config.add_argument('--hostname', required=True)
    config.add_argument('--login-method', choices=['onetimepin', 'cloudflare'], default='onetimepin')
    config.add_argument('--email', action='append', required=True, help='Repeat for every allowed user; replaces full list')
    config.add_argument('--upstream', required=True)
    args = parser.parse_args()
    if args.command == 'authorize': authorize()
    elif args.command == 'check': check()
    else: configure(args)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print('Cloudflare setup:', str(error), file=sys.stderr)
        sys.exit(1)
