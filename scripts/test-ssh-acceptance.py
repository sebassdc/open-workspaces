"""Preservation checks use a fake /proc tree; no service or VM is touched."""
import ast
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'experiments/ssh-guest/acceptance.py'


class PreservationTests(unittest.TestCase):
    def test_protected_root_is_required_before_creating_fixtures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = {k: v for k, v in os.environ.items() if k != 'OW_SSH_PROTECTED_ROOT'}
            env.update(OW_SSH_TEST_BINARY=str(root / 'ow'), OW_SSH_TEST_ASSETS=str(root / 'assets'))
            for script, args in [(SCRIPT, ['--prepare']), (SCRIPT.with_name('legacy.py'), [])]:
                for protected in [None, str(root / 'missing-checkout')]:
                    with self.subTest(script=script.name, protected=protected):
                        if protected is not None:
                            env['OW_SSH_PROTECTED_ROOT'] = protected
                        else:
                            env.pop('OW_SSH_PROTECTED_ROOT', None)
                        fixture = root / 'fixture'
                        result = subprocess.run([sys.executable, str(script), *args, str(fixture)],
                                                env=env, capture_output=True, text=True)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertFalse(fixture.exists(), 'invalid protection must fail before setup')

    def test_original_checkout_worker_remains_protected_from_a_worktree(self):
        node = next(n for n in ast.parse(SCRIPT.read_text()).body
                    if isinstance(n, ast.FunctionDef) and n.name == 'processes')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proc = root / 'proc'
            process = proc / '123'
            process.mkdir(parents=True)
            protected = root / 'original-checkout'
            command = str(protected / 'target/release/ow').encode()
            (process / 'comm').write_text('ow\n')
            (process / 'cmdline').write_bytes(command + b'\0worker\0')
            (process / 'stat').write_text('123 (ow) ' + ' '.join(['1'] * 22))
            namespace = {
                'Path': lambda p: proc if p == '/proc' else Path(p),
                'REPO': root / 'test-worktree',
                'PROTECTED': protected,
            }
            exec(compile(ast.Module(body=[node], type_ignores=[]), str(SCRIPT), 'exec'), namespace)
            processes = namespace['processes']
            baseline = processes()
            self.assertEqual(set(baseline), {'123'})
            self.assertEqual(processes(), baseline)
            (process / 'stat').write_text('123 (ow) ' + ' '.join(['2'] * 22))
            self.assertNotEqual(processes(), baseline, 'a reused PID must not pass preservation')
            shutil.rmtree(process)
            self.assertNotEqual(processes(), baseline, 'loss of the original worker must fail preservation')


if __name__ == '__main__':
    unittest.main()
