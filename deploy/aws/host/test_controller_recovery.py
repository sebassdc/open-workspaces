"""Disposable host/config and journal fixtures; no cloud, secrets or real VMs."""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import ow_host as h
import controller_recovery as r
import cutover as c
from test_ow_host import CONFIG, BUDGET


class RecoveryHost(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        patch = mock.patch.object(h, 'ROOT', Path(self.tmp.name))
        patch.start()
        self.addCleanup(patch.stop)
        h.write_json(h.CONFIG, h.validate(CONFIG))
        self.credential = b'{"node":"fixture","credential":"' + b'a' * 64 + b'"}'
        h.write_atomic(f'{h.RUNNER}/node.json', self.credential.decode(), 0o600)
        h.set_pin('worker', '0.9.0-unchanged')
        for unit in r.WRITERS:
            h.write_atomic(r.guard(unit), f'[Unit]\nConditionPathExists=!{r.FREEZE}\n')
        self.new = {**copy.deepcopy(CONFIG), 'role': 'runner', 'dashboard': {},
                    'runner': {'node': 'fixture', 'controller': 'replacement',
                               'joinSecretArn': 'arn:aws:secretsmanager:us-east-1:111122223333:secret:fixture'}}
        self.config = Path(self.tmp.name) / 'new.json'
        self.config.write_text(json.dumps(self.new))
        self.ca = Path(self.tmp.name) / 'ca.pem'
        self.ca.write_text('fixture CA')
        os.chmod(self.config, 0o600)
        os.chmod(self.ca, 0o600)
        # A real installed worker definition must match; conversion may not rewrite it.
        for path, (text, mode) in h.render(h.validate(CONFIG), BUDGET).items():
            h.write_atomic(path, text, mode)

    def test_writer_stopped_rejects_transitional_states_and_live_processes(self):
        for state, pid, control, expected in [('inactive', '0', '0', True), ('failed', '0', '0', True),
                                               ('activating', '0', '0', False), ('deactivating', '42', '0', False),
                                               ('failed', '42', '0', False), ('inactive', '0', '42', False)]:
            raw = f'ActiveState={state}\nMainPID={pid}\nControlPID={control}\n'
            with mock.patch.object(h, 'run', return_value=raw):
                self.assertEqual(r.writer_stopped(h.UNITS['agent']), expected)
        with mock.patch.object(h, 'run', return_value=''):
            self.assertFalse(r.writer_stopped(h.UNITS['agent']))

    def test_freeze_survives_interruption_and_retries_without_worker_action(self):
        for fail_at in range(1, 10):
            calls = []
            def command(*args, **kw):
                calls.append(args)
                if len(calls) == fail_at:
                    raise RuntimeError('crash')
            with mock.patch.object(h, 'systemctl', side_effect=command), mock.patch.object(r, 'writer_stopped', return_value=True):
                try:
                    r.freeze()
                except RuntimeError:
                    pass
            self.assertTrue(r.frozen())
            with mock.patch.object(h, 'systemctl', side_effect=lambda *a, **k: calls.append(a)), \
                 mock.patch.object(r, 'writer_stopped', return_value=True), contextlib.redirect_stdout(io.StringIO()):
                r.freeze()
            self.assertFalse(any(h.UNITS['worker'] in call for call in calls))
            for unit in r.WRITERS:
                self.assertIn(f'ConditionPathExists=!{r.FREEZE}', h.p(r.guard(unit)).read_text())
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(h.main(['restart', '--worker']), 3)
                self.assertEqual(h.main(['recover']), 3)
                self.assertEqual(h.main(['apply']), 3)

    def test_live_writers_or_wrong_role_and_identity_refuse_conversion(self):
        r.durable(r.FREEZE, {'state': 'freezing'})
        with mock.patch.object(r, 'writer_stopped', return_value=False), self.assertRaisesRegex(h.Refused, 'all controller'):
            r.runner(self.config, 'https://replacement:8790', self.ca)
        self.new['runner']['node'] = 'another'
        self.config.write_text(json.dumps(self.new))
        with mock.patch.object(r, 'writer_stopped', return_value=True), self.assertRaisesRegex(h.Refused, 'node ID'):
            r.runner(self.config, 'https://replacement:8790', self.ca)
        self.assertFalse(h.p(r.TRANSITION).exists())

    def test_transition_crashes_resume_and_preserve_worker_and_credential(self):
        r.durable(r.FREEZE, {'state': 'stopped'})
        calls = []
        with mock.patch.object(r, 'writer_stopped', return_value=True), mock.patch.object(h, 'budgets', return_value=BUDGET), \
             mock.patch.object(h, 'chown'), mock.patch.object(r.ssl, 'create_default_context'), \
             mock.patch.object(h, 'run', side_effect=lambda cmd, **kw: calls.append(cmd) or ''):
            with mock.patch.object(h, 'setup', side_effect=RuntimeError('crash after trust')):
                with self.assertRaises(RuntimeError):
                    r.runner(self.config, 'https://replacement:8790', self.ca)
            self.assertEqual(h.read_json(r.TRANSITION)['state'], 'prepared')
            with self.assertRaisesRegex(h.Refused, 'different inputs'):
                r.runner(self.config, 'https://replacement:9999', self.ca)
            with contextlib.redirect_stdout(io.StringIO()):
                r.runner(self.config, 'https://replacement:8790', self.ca)
                r.runner(self.config, 'https://replacement:8790', self.ca)
        self.assertEqual(h.read_json(r.TRANSITION)['state'], 'configured')
        self.assertEqual(h.role(), 'runner')
        self.assertEqual(h.pin('worker'), '0.9.0-unchanged')
        self.assertEqual(h.p(f'{h.RUNNER}/node.json').read_bytes(), self.credential)
        self.assertTrue(r.frozen())
        self.assertFalse(any(h.UNITS['worker'] in cmd for cmd in calls))
        self.assertFalse(any(cmd[:2] == ['systemctl', 'start'] for cmd in calls))
        self.assertIn('node-agent', h.p('/etc/systemd/system/open-workspaces.service').read_text())
        self.assertFalse(h.unit_exists(h.UNITS['dashboard']))

    def test_role_transition_cannot_redirect_or_disable_existing_backups(self):
        r.durable(r.FREEZE, {'state': 'stopped'})
        self.new['backup'] = {'url': 's3://another-bucket/elsewhere', 'schedule': ''}
        self.config.write_text(json.dumps(self.new))
        with mock.patch.object(r, 'writer_stopped', return_value=True), mock.patch.object(h, 'budgets', return_value=BUDGET), \
             mock.patch.object(r.ssl, 'create_default_context'), self.assertRaisesRegex(h.Refused, 'preserve backup'):
            r.runner(self.config, 'https://replacement:8790', self.ca)
        self.assertFalse(h.p(r.TRANSITION).exists())

    def test_private_inputs_and_stale_readiness(self):
        os.chmod(self.config, 0o644)
        with self.assertRaisesRegex(h.Refused, 'private regular file'):
            r.private_input(self.config)
        os.chmod(self.config, 0o600)
        r.durable(r.FREEZE, {'state': 'stopped'})
        for report in ('agent.json', 'node-channel.json'):
            h.write_json(f'{h.RUNNER}/{report}', {'online': True, 'authority': 'old'})
        with mock.patch.object(r, 'writer_stopped', return_value=True), mock.patch.object(h, 'budgets', return_value=BUDGET), \
             mock.patch.object(h, 'chown'), mock.patch.object(r.ssl, 'create_default_context'), \
             mock.patch.object(h, 'run', return_value=''), contextlib.redirect_stdout(io.StringIO()):
            r.runner(self.config, 'https://replacement:8790', self.ca)
        self.assertFalse(h.p(f'{h.RUNNER}/agent.json').exists())
        self.assertFalse(h.p(f'{h.RUNNER}/node-channel.json').exists())

    def test_worker_drift_and_changed_transition_inputs_fail_closed(self):
        r.durable(r.FREEZE, {'state': 'stopped'})
        h.write_atomic('/etc/open-workspaces/worker.env', 'different')
        with mock.patch.object(r, 'writer_stopped', return_value=True), mock.patch.object(h, 'budgets', return_value=BUDGET), \
             mock.patch.object(r.ssl, 'create_default_context'), self.assertRaisesRegex(h.Refused, 'installed worker'):
            r.runner(self.config, 'https://replacement:8790', self.ca)
        self.assertFalse(h.p(r.TRANSITION).exists())


class Checklist(unittest.TestCase):
    def initial(self):
        return {'schema': 1, 'drill': 'disposable', 'stage': 'planned', 'receipts': []}

    def receipt(self, stage):
        return {'stage': stage, 'drill': 'disposable', 'evidence': 'private fixture log',
                **{key: True for key in c.REQUIRED.get(stage, ())},
                'fence_method': 'service-and-network-isolation', 'fence_reference': 'fixture firewall + stopped units',
                'backup_gap': 'none', 'catalog': 'fixture.sqlite3',
                'unknown_running_copies': 0, 'contradictory_running_copies': 0, 'unresolved_operations': 0}

    def test_cli_persists_and_rejects_non_private_receipts(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'journal.json'
            receipt = Path(folder) / 'receipt.json'
            receipt.write_text(json.dumps(self.receipt('frozen')))
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(c.main(['init', str(path), '--drill', 'disposable']), 0)
                self.assertEqual(c.main(['init', str(path), '--drill', 'disposable']), 3)
                self.assertEqual(c.main(['advance', str(path), '--receipt', str(receipt)]), 3)
                self.assertEqual(c.private(path)['stage'], 'planned')
                os.chmod(receipt, 0o600)
                self.assertEqual(c.main(['advance', str(path), '--receipt', str(receipt)]), 0)
                self.assertEqual(c.main(['status', str(path)]), 0)
                self.assertEqual(c.private(path)['stage'], 'frozen')
                self.assertEqual(c.main(['advance', str(path), '--receipt', str(receipt)]), 3)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    @mock.patch.object(c, 'catalog', return_value={'tables': {'all': 'fixture'}, 'schema': 6})
    def test_every_stage_crash_resumes_one_next_step_and_never_repeats(self, catalog):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'journal.json'
            state = self.initial()
            c.persist(path, state)
            for stage in c.STAGES[1:]:
                # A crash before persist leaves the same next step. After persist,
                # a replay of the previous stage is refused rather than opening writes twice.
                receipt = self.receipt(stage)
                advanced = c.advance(c.private(path), receipt, 'ow')
                self.assertEqual(c.private(path)['stage'], state['stage'])
                c.persist(path, advanced)
                self.assertEqual(c.private(path), advanced)
                with self.assertRaises(ValueError):
                    c.advance(c.private(path), receipt, 'ow')
                state = advanced

    @mock.patch.object(c, 'catalog', return_value={'tables': 'same'})
    def test_stale_gap_dual_authority_unknown_running_copies_and_stale_restore_block(self, catalog):
        for stage, field, bad in [('frozen', 'old_stopped', False), ('isolated', 'fence_method', 'flock'),
                                  ('catalogued', 'backup_gap', 'stale'),
                                  ('reconciled', 'unknown_running_copies', 1),
                                  ('reconciled', 'contradictory_running_copies', 1),
                                  ('reconciled', 'stale_operations_rejected', False)]:
            state = {**self.initial(), 'stage': c.STAGES[c.STAGES.index(stage)-1]}
            with self.assertRaises(ValueError, msg=f'{stage}/{field}'):
                c.advance(state, {**self.receipt(stage), field: bad}, 'ow')
        state = {**self.initial(), 'stage': 'catalogued', 'catalog': {'tables': 'new'}}
        with self.assertRaisesRegex(ValueError, 'differs'):
            c.advance(state, self.receipt('restored'), 'ow')

    @mock.patch.object(c, 'catalog', return_value={'tables': 'same'})
    def test_rollback_before_and_after_possible_first_write(self, catalog):
        receipt = {'drill': 'disposable', 'evidence': 'fresh stop/fence + backup log', 'old_stopped': True,
                   'replacement_stopped': True, 'maintenance_disabled': True, 'external_fence_verified': True,
                   'latest_catalog': 'latest', 'restore_catalog': 'restore'}
        state = {**self.initial(), 'stage': 'restored', 'catalog': {'tables': 'same'}}
        self.assertEqual(c.rollback(state, receipt, 'ow')['rollback'], 'final-pre-write-catalog')
        state['stage'] = 'write_possible'
        with self.assertRaisesRegex(ValueError, 'latest replacement'):
            c.rollback(state, receipt, 'ow')
        receipt.update(source='replacement', taken_after_both_stopped=True)
        self.assertEqual(c.rollback(state, receipt, 'ow')['rollback'], 'latest-replacement-only')
        catalog.side_effect = [{'tables': 'latest'}, {'tables': 'pre-cutover'}]
        with self.assertRaisesRegex(ValueError, 'differs'):
            c.rollback(state, receipt, 'ow')


if __name__ == '__main__':
    unittest.main()
