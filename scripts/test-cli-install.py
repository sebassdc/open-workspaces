#!/usr/bin/env python3
"""Hermetic download fixtures; OW_TEST_CLIENT adds acceptance of a real release build."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / 'crates/ow/cli/install.sh'
VERSION = 'v' + json.loads((ROOT / 'deploy/aws/package.json').read_text())['version']
SYSTEM = platform.system()
ARCH = platform.machine()
ARTIFACT = {('Darwin', 'arm64'): 'ow-darwin-arm64',
            ('Darwin', 'x86_64'): 'ow-darwin-amd64',
            ('Linux', 'x86_64'): 'ow-linux-amd64'}[(SYSTEM, ARCH)]


class InstallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory(prefix='ow-client-build-')
        if os.environ.get('OW_TEST_PUBLISHED_RELEASE'):
            cls.binary = b'not used by the published-download check'
        elif os.environ.get('OW_TEST_CLIENT'):
            cls.binary = Path(os.environ['OW_TEST_CLIENT']).read_bytes()
        else:
            # Tiny native fixture: positive tests must execute a real platform binary.
            source = Path(cls.build.name) / 'version.c'
            source.write_text('#include <stdio.h>\nint main(void) { puts("ow ' + VERSION[1:] + '"); return 0; }\n')
            binary = source.with_suffix('')
            subprocess.run(['cc', str(source), '-o', str(binary)], check=True)
            cls.binary = binary.read_bytes()

    @classmethod
    def tearDownClass(cls):
        cls.build.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ow-install-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / 'home'
        self.home.mkdir()
        self.assets = self.root / 'assets'
        self.assets.mkdir()
        self.tools = self.root / 'tools'
        self.tools.mkdir()
        self.dest = self.home / 'bin with spaces'
        self.dest.mkdir()
        self.client = self.dest / 'ow'
        self.client.write_bytes(b'previous working client')
        self.client.chmod(0o755)
        self.before = self.client.stat()
        for tool in ['mkdir', 'mktemp', 'rm', 'od', 'tr', 'chmod', 'mv', 'grep', 'awk', 'sha256sum', 'shasum']:
            found = shutil.which(tool)
            if found:
                (self.tools / tool).symlink_to(found)
        self.script = self.root / 'install.sh'
        self.script.write_bytes(INSTALLER.read_bytes())
        self.log = self.root / 'downloads.jsonl'
        self.env = {**os.environ, 'HOME': str(self.home), 'PATH': str(self.tools),
                    'OW_INSTALL_DIR': str(self.dest), 'FIXTURE_ASSETS': str(self.assets),
                    'FIXTURE_LOG': str(self.log), 'FIXTURE_SYSTEM': SYSTEM, 'FIXTURE_ARCH': ARCH}
        self.sh = shutil.which('sh')
        self.write_tool('uname', "import os,sys\nprint(os.environ['FIXTURE_SYSTEM' if sys.argv[1] == '-s' else 'FIXTURE_ARCH'])\n")
        self.write_tool('curl', '''import json,os,pathlib,sys
args=sys.argv[1:]
url=next(x for x in args if x.startswith('https://'))
with open(os.environ['FIXTURE_LOG'],'a') as f: f.write(json.dumps(url)+'\\n')
assert args[:4] == ['--proto','=https','--tlsv1.2','-fsSL']
assert url.startswith('https://github.com/sebassdc/open-workspaces/releases/download/') or url.startswith('https://fixture.invalid/cli/') or url.startswith('https://github.com/cloudflare/cloudflared/releases/download/2026.9.3/')
p=pathlib.Path(os.environ['FIXTURE_ASSETS']) / url.rsplit('/',1)[1]
if not p.is_file(): sys.exit(22)
data=p.read_bytes()
if '-o' in args: pathlib.Path(args[args.index('-o')+1]).write_bytes(data)
else: sys.stdout.buffer.write(data)
''')
        self.asset(self.binary)

    def write_tool(self, name, code):
        path = self.tools / name
        path.write_text('#!' + sys.executable + '\n' + code)
        path.chmod(0o755)

    def asset(self, data, checksum=None):
        (self.assets / ARTIFACT).write_bytes(data)
        (self.assets / (ARTIFACT + '.sha256')).write_text(checksum or hashlib.sha256(data).hexdigest() + '\n')

    def run_install(self, *args):
        return subprocess.run([self.sh, str(self.script), *args], env=self.env, capture_output=True, text=True)

    def assert_preserved(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.client.read_bytes(), b'previous working client')
        self.assertEqual(self.client.stat().st_ino, self.before.st_ino)
        self.assertEqual(self.client.stat().st_mtime_ns, self.before.st_mtime_ns)
        self.assertEqual(list(self.dest.glob('.ow-install.*')), [])

    def test_clean_install_and_repeat_without_rust_root_or_helper(self):
        self.client.unlink()
        for _ in range(2):
            result = self.run_install('--version', VERSION)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(subprocess.check_output([str(self.client), '--version'], env=self.env, text=True).strip(), 'ow ' + VERSION[1:])
            self.assertEqual(self.client.stat().st_mode & 0o777, 0o755)
            self.assertIn('export PATH=', result.stdout)
            self.assertIn('login --tailnet', result.stdout)
            self.assertFalse((self.home / '.local/lib/open-workspaces').exists())
            self.assertEqual(list(self.dest.glob('.ow-install.*')), [])
        urls = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(urls, [f'https://github.com/sebassdc/open-workspaces/releases/download/{VERSION}/{ARTIFACT}',
                                f'https://github.com/sebassdc/open-workspaces/releases/download/{VERSION}/{ARTIFACT}.sha256'] * 2)
        self.assertIsNone(shutil.which('cargo', path=self.env['PATH']))
        self.assertIsNone(shutil.which('cloudflared', path=self.env['PATH']))
        self.assertIsNone(shutil.which('sudo', path=self.env['PATH']))
        self.assertFalse((self.home / '.profile').exists())

    @unittest.skipUnless(os.environ.get('OW_TEST_PUBLISHED_RELEASE'), 'requires a published release')
    def test_published_release_download(self):
        self.check_published_release_download()

    def test_published_download_contract_with_fixtures(self):
        curl = self.root / 'fixture-curl'
        shutil.copyfile(self.tools / 'curl', curl)
        curl.chmod(0o755)
        (self.assets / 'install.sh').write_bytes(INSTALLER.read_bytes())
        (self.assets / 'source-commit.txt').write_text('1' * 40 + '\n')
        (self.assets / 'SHA256SUMS').write_text(''.join(
            hashlib.sha256((self.assets / name).read_bytes()).hexdigest() + '  ' + name + '\n'
            for name in ['install.sh', 'source-commit.txt', ARTIFACT]))
        with patch('shutil.which', return_value=str(curl)), patch.dict(os.environ, OW_TEST_SOURCE_COMMIT='1' * 40):
            self.check_published_release_download()

    def check_published_release_download(self):
        # Real GitHub HTTPS downloads, with the same constrained client PATH.
        (self.tools / 'curl').unlink()
        (self.tools / 'curl').symlink_to(shutil.which('curl'))
        base = f'https://github.com/sebassdc/open-workspaces/releases/download/{VERSION}'
        def download(name):
            return subprocess.check_output([str(self.tools / 'curl'), '--proto', '=https', '--tlsv1.2', '-fsSL', f'{base}/{name}'], env=self.env)
        sums = dict(line.split()[::-1] for line in download('SHA256SUMS').decode().splitlines())
        installer = download('install.sh')
        self.assertEqual(hashlib.sha256(installer).hexdigest(), sums['install.sh'])
        source = download('source-commit.txt')
        self.assertEqual(hashlib.sha256(source).hexdigest(), sums['source-commit.txt'])
        self.assertEqual(source.decode().strip(), os.environ['OW_TEST_SOURCE_COMMIT'])
        self.script.write_bytes(installer)
        self.client.unlink()
        for _ in range(2):
            result = self.run_install('--version', VERSION)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(subprocess.check_output([str(self.client), '--version'], env=self.env, text=True).strip(), 'ow ' + VERSION[1:])
            self.assertEqual(hashlib.sha256(self.client.read_bytes()).hexdigest(), sums[ARTIFACT])
            self.assertFalse((self.home / '.local/lib/open-workspaces').exists())

    def test_bad_or_missing_assets_preserve_working_client(self):
        for mode in ['missing-binary', 'missing-checksum', 'corrupt', 'bad-hash', 'short-hash', 'html', 'wrong-arch', 'wrong-version']:
            with self.subTest(mode=mode):
                self.asset(self.binary)
                if mode == 'missing-binary': (self.assets / ARTIFACT).unlink()
                if mode == 'missing-checksum': (self.assets / (ARTIFACT + '.sha256')).unlink()
                if mode == 'corrupt': (self.assets / ARTIFACT).write_bytes(b'corrupt')
                if mode == 'bad-hash': self.asset(self.binary, 'not a checksum\n')
                if mode == 'short-hash': self.asset(self.binary, 'a' * 63)
                if mode == 'html': self.asset(b'<html>not a binary</html>')
                if mode == 'wrong-arch':
                    data = bytearray(self.binary)
                    if SYSTEM == 'Darwin': data[4:8] = bytes.fromhex('07000001' if ARCH == 'arm64' else '0c000001')
                    else: data[18:20] = bytes.fromhex('b700')
                    self.asset(bytes(data))
                result = self.run_install('--version', 'v999.0.0' if mode == 'wrong-version' else VERSION)
                self.assert_preserved(result)

    def test_invalid_version_arguments_and_unsupported_platform(self):
        for args in [[], ['--version'], ['--version', 'latest'], ['--version', '../v1.0.0'], ['--version', 'v01.0.0'],
                     ['--version', VERSION, 'surprise'], ['--version', 'v1.0.0\nlatest']]:
            with self.subTest(args=args): self.assert_preserved(self.run_install(*args))
        for system, arch in [('Linux', 'aarch64'), ('Windows', 'x86_64'), ('Darwin', 'unknown')]:
            self.env.update(FIXTURE_SYSTEM=system, FIXTURE_ARCH=arch)
            self.assert_preserved(self.run_install('--version', VERSION))
        self.assertFalse(self.log.exists())

    def test_symlink_target_preserves_both_files(self):
        self.client.rename(self.dest / 'real-ow')
        self.client.symlink_to('real-ow')
        self.assert_preserved(self.run_install('--version', VERSION))
        self.assertTrue(self.client.is_symlink())

    def test_shasum_fallback(self):
        if not (self.tools / 'shasum').exists():
            self.skipTest('shasum not installed')
        (self.tools / 'sha256sum').unlink(missing_ok=True)
        result = self.run_install('--version', VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_access_helper_failure_preserves_client(self):
        result = self.run_install('--version', VERSION, '--access')
        self.assert_preserved(result)
        self.assertIn('cloudflare/cloudflared', self.log.read_text())

    def test_existing_access_server_install_and_pinned_access_mode(self):
        helper = self.home / '.local/lib/open-workspaces/cloudflared'
        helper.parent.mkdir(parents=True)
        helper.write_text('existing helper')
        helper.chmod(0o755)
        for legacy in [True, False]:
            if legacy: self.script.write_text(INSTALLER.read_text().replace('@@ORIGIN@@', 'https://fixture.invalid'))
            else: self.script.write_bytes(INSTALLER.read_bytes())
            result = self.run_install(*([] if legacy else ['--version', VERSION, '--access']))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn('login --tailnet', result.stdout)
            self.assertEqual(helper.read_text(), 'existing helper')
        self.assertNotIn('cloudflare/cloudflared', self.log.read_text())


class PublishTests(unittest.TestCase):
    def test_release_retry_and_conflict_preserve_published_bytes(self):
        with tempfile.TemporaryDirectory(prefix='ow-release-') as directory:
            root = Path(directory)
            assets = root / 'assets'
            assets.mkdir()
            remote = root / 'remote'
            remote.mkdir()
            (assets / 'ow-linux-amd64').write_bytes(b'new binary')
            (assets / 'ow-linux-amd64.sha256').write_bytes(b'checksum fixture')
            gh = root / 'gh'
            gh.write_text('#!' + sys.executable + '\n' + '''import os,pathlib,shutil,sys
args=sys.argv[1:]
assert args[0] == 'release' and args[2] == 'v1.2.3'
remote=pathlib.Path(os.environ['FIXTURE_REMOTE'])
if args[1] == 'view': print('\\n'.join(p.name for p in remote.iterdir()))
elif args[1] == 'download': shutil.copyfile(remote / args[4], pathlib.Path(args[6]) / args[4])
elif args[1] == 'upload':
    assert '--clobber' not in args
    source=pathlib.Path(args[3])
    assert not (remote / source.name).exists()
    shutil.copyfile(source,remote / source.name)
else: raise AssertionError(args)
''')
            gh.chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH'], 'FIXTURE_REMOTE': str(remote)}
            command = ['bash', str(ROOT / 'scripts/publish-release-clients.sh'), 'v1.2.3', str(assets)]
            for _ in range(2):
                result = subprocess.run(command, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((remote / 'ow-linux-amd64').read_bytes(), b'new binary')
                self.assertEqual((remote / 'ow-linux-amd64.sha256').read_bytes(), b'checksum fixture')
            (assets / 'ow-linux-amd64').write_bytes(b'different build')
            result = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('choose a fresh version', result.stderr)
            self.assertEqual((remote / 'ow-linux-amd64').read_bytes(), b'new binary')


if __name__ == '__main__':
    unittest.main()
