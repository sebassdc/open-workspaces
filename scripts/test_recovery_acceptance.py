"""The real-VM acceptance must refuse unsafe targets before invoking a binary."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('recovery-acceptance.py')


class Guards(unittest.TestCase):
    def test_same_or_nonrunning_workers_are_rejected_before_any_command(self):
        with tempfile.TemporaryDirectory() as directory:
            for destination, error in [(directory, 'two different'), (directory + '/other', 'already be running')]:
                result = subprocess.run([sys.executable, str(SCRIPT), '--ow', '/does/not/exist',
                                         '--source', directory, '--destination', destination,
                                         '--store', 's3://example/private', '--mode', 'cold'],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)
                self.assertIn(error, result.stderr)


if __name__ == '__main__':
    unittest.main()
