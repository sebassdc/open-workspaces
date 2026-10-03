#!/usr/bin/env python3
"""Offline ownership and Access-policy guard checks; no Cloudflare writes."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('access_setup', ROOT / 'scripts/cloudflare-access.py')
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


class AccessSetupGuards(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        setup.PRIVATE = Path(self.directory.name)
        setup.TOKEN = setup.PRIVATE / 'access-api-token'
        setup.STATE = setup.PRIVATE / 'managed-access.json'
        setup.private_write(setup.TOKEN, 'synthetic-test-token')
        (setup.PRIVATE / 'credentials').mkdir()
        setup.private_write(setup.PRIVATE / 'created-tunnel.json', {'id': 'synthetic-tunnel'})
        self.calls = []
        self.policies = []
        self.apps = []
        self.pin = [{'id': 'pin', 'type': 'onetimepin'}]
        self.args = type('Args', (), {'hostname': 'app.example.test', 'email': ['owner@example.test'],
                                      'upstream': 'http://127.0.0.1:12345'})()

    def tearDown(self):
        self.directory.cleanup()

    def api(self, token, path, method='GET', body=None):
        self.calls.append((path, method, body))
        if path == 'zones/test-zone': return {'name': 'example.test'}
        if path.endswith('/organizations'): return {'auth_domain': 'test-team.cloudflareaccess.com'}
        if path.endswith('/identity_providers'): return self.pin
        if path.endswith('/policies') and method == 'GET': return self.policies
        if '/policies/' in path and method == 'PUT':
            self.policies = [{'id': 'owned-policy', **body}]
            return self.policies[0]
        if path.endswith('/apps') and method == 'POST':
            self.policies = [{'id': 'owned-policy', **body['policies'][0]}]
            app = {'id': 'owned-app', 'aud': 'synthetic-audience', 'allowed_idps': ['pin'], 'domain': body['domain']}
            self.apps = [app]
            return app
        if path.endswith('/apps/owned-app'): return self.apps[0]
        if '/apps?' in path: return self.apps
        raise AssertionError((path, method))

    def configure(self):
        with patch.object(setup, 'credential', return_value={'apiToken': 'synthetic-tunnel-token',
            'accountID': 'test-account', 'zoneID': 'test-zone'}), patch.object(setup, 'api', side_effect=self.api):
            setup.configure(self.args)

    def test_refuses_unrelated_hostname_or_application_before_mutation(self):
        self.args.hostname = 'other.test'
        with self.assertRaises(RuntimeError): self.configure()
        self.assertTrue(all(method == 'GET' for _, method, _ in self.calls))
        self.args.hostname = 'app.example.test'
        self.apps = [{'id': 'unrelated-app', 'domain': self.args.hostname}]
        with self.assertRaises(RuntimeError): self.configure()
        self.assertTrue(all(method == 'GET' for _, method, _ in self.calls))

    def test_missing_pin_does_not_change_global_identity_providers(self):
        self.pin = []
        with self.assertRaises(RuntimeError): self.configure()
        self.assertTrue(all(method == 'GET' for _, method, _ in self.calls))

    def test_reconfigure_full_email_list_only_changes_owned_policy(self):
        self.configure()
        self.assertEqual(sum(method == 'POST' for _, method, _ in self.calls), 1)
        self.args.email.append('second@example.test')
        self.configure()
        self.assertEqual(sum(method == 'POST' for _, method, _ in self.calls), 1)
        puts = [path for path, method, _ in self.calls if method == 'PUT']
        self.assertEqual(puts, ['accounts/test-account/access/apps/owned-app/policies/owned-policy'])
        gateway = json.loads((setup.PRIVATE / 'gateway.json').read_text())
        self.assertEqual(gateway['allowed_emails'], ['owner@example.test', 'second@example.test'])
        self.assertEqual((setup.PRIVATE / 'gateway.json').stat().st_mode & 0o777, 0o600)
        self.assertTrue(all('/dns_records' not in path for path, _, _ in self.calls))
        tunnel = json.loads((setup.PRIVATE / 'tunnel.json').read_text())
        self.assertEqual(tunnel['ingress'][-1]['service'], 'http_status:404')
        self.assertTrue(tunnel['ingress'][0]['originRequest']['access']['required'])


if __name__ == '__main__': unittest.main()
