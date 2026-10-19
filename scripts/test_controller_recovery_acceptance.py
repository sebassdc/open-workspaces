"""Credential-free checks for the real-guest drill's SSH framing and safety gates."""
import importlib.util
from pathlib import Path
import socket
import struct
import tempfile
import threading
import types
import unittest

spec = importlib.util.spec_from_file_location('controller_drill', Path(__file__).with_name('controller-recovery-acceptance.py'))
drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drill)


class Acceptance(unittest.TestCase):
    def stream(self):
        client, server = socket.socketpair()
        client.settimeout(5)
        server.settimeout(5)
        self.addCleanup(client.close)
        self.addCleanup(server.close)
        ws = drill.WebSocket.__new__(drill.WebSocket)
        ws.s, ws.buffer = client, b''
        return ws, server

    def test_bulk_ssh_client_frame_is_masked_and_byte_exact(self):
        ws, server = self.stream()
        payload = bytes(range(256))*64
        sender = threading.Thread(target=ws.send, args=(payload,))
        sender.start()
        data = b''
        while len(data) < len(payload)+8:
            data += server.recv(32768)
        self.assertEqual(data[:4], b'\x82\xfe'+struct.pack('!H', len(payload)))
        mask = data[4:8]
        self.assertEqual(bytes(b ^ mask[i % 4] for i, b in enumerate(data[8:])), payload)
        sender.join(timeout=5)
        self.assertFalse(sender.is_alive())

    def test_ssh_server_frame_and_masked_server_rejection(self):
        ws, server = self.stream()
        server.sendall(b'\x82\x05hello')
        self.assertEqual(ws.frame(), (2, b'hello'))
        server.sendall(b'\x82\x80')
        with self.assertRaisesRegex(RuntimeError, 'invalid server frame'):
            ws.frame()

    def test_never_create_fixture_inside_the_protected_worker(self):
        with tempfile.TemporaryDirectory(dir='/tmp', prefix='ow-drill-') as parent:
            worker = Path(parent).resolve()/'worker'
            worker.mkdir(mode=0o700)
            root = worker/'drill'
            args = types.SimpleNamespace(root=root, protect_worker=worker)
            with self.assertRaisesRegex(RuntimeError, 'outside the protected worker'):
                drill.Drill(args)
            self.assertFalse(root.exists())

    def test_counter_progress_does_not_hide_process_replacement(self):
        fixture = drill.Drill.__new__(drill.Drill)
        fixture.protect = lambda: None
        before = {'worker': {'pid': 7, 'start_ticks': '10'}, 'guest_boot_id': 'original', 'counter': 1}
        fixture.continuity(before, {**before, 'counter': 2})
        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
            fixture.continuity(before, {**before, 'guest_boot_id': 'replacement', 'counter': 2})
        with self.assertRaisesRegex(RuntimeError, 'counter did not continue'):
            fixture.continuity(before, before)

    def test_cleanup_deletes_only_its_disposable_guest_and_refuses_unknowns(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = drill.Drill.__new__(drill.Drill)
            fixture.runner = Path(directory)
            (fixture.runner/'control.sock').touch()
            fixture.physical, fixture.children = 'own-guest', []
            fixture.protect = lambda: None
            fixture.emit = lambda *args, **kwargs: None
            for guest in ('unexpected-guest', 'own-guest'):
                calls = []
                def call(where, *args, **kwargs):
                    self.assertEqual(where, fixture.runner)
                    calls.append(args)
                    if args == ('list',):
                        return [{'id': guest}] if len(calls) == 1 else []
                fixture.call = call
                if guest == 'unexpected-guest':
                    with self.assertRaisesRegex(RuntimeError, 'unknown fixture guest'):
                        fixture.cleanup()
                    self.assertEqual(calls, [('list',)])
                else:
                    fixture.cleanup()
                    self.assertEqual(calls, [('list',), ('stop', guest), ('delete', guest), ('list',), ('down',)])


if __name__ == '__main__':
    unittest.main()
