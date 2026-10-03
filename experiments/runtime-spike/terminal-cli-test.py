#!/usr/bin/env python3
"""Real-VM regression for ow shell's raw mode, resize, signals and exit status."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--image',choices=['alpine','arch','ubuntu'],default='alpine')
args=parser.parse_args()
repo = Path(__file__).resolve().parents[2]
root = Path(tempfile.mkdtemp(prefix='tty-', dir=repo / 'data'))
command = [str(repo / 'ow'), '--data-dir', str(root)]
master = slave = None
process = None


def run(*args):
    return subprocess.run(command + list(args), check=True, capture_output=True, text=True)


try:
    run('up')
    run('create', 'terminal-cli', '--image', args.image)
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
    process = subprocess.Popen(command + ['shell', 'terminal-cli'], stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    output = bytearray()

    def wait(marker, timeout=8):
        start = time.monotonic()
        while marker not in output:
            if time.monotonic() - start > timeout:
                raise AssertionError(bytes(output).decode(errors='replace'))
            if select.select([master], [], [], .1)[0]:
                output.extend(os.read(master, 65536))

    wait(b'#' if args.image == 'alpine' else b'$')
    os.write(master, b"test -t 0 && printf 'CLI_%s\\n' 'PTY'\r")
    wait(b'CLI_PTY')
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 37, 111, 0, 0))
    time.sleep(.15)
    os.write(master, b'stty size\r')
    wait(b'37 111')
    os.write(master, b'sleep 30\r')
    time.sleep(.2)
    os.write(master, b"\x03printf 'CLI_%s\\n' 'INTERRUPTED'\r")
    wait(b'CLI_INTERRUPTED', 3)
    os.write(master, b'exit 42\r')
    assert process.wait(timeout=5) == 42
    assert termios.tcgetattr(slave) == original, 'local terminal attributes were not restored'
    result = {'passed': True, 'image':args.image, 'checks': ['real guest PTY', 'raw input and Ctrl-C', 'window resize', 'exit status 42', 'local terminal attributes restored']}
    (root / 'terminal-cli-result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
    print('Artifacts:', root)
finally:
    if process is not None and process.poll() is None:
        process.terminate()
        process.wait(timeout=5)
    for fd in (master, slave):
        if fd is not None:
            os.close(fd)
    run('down')
