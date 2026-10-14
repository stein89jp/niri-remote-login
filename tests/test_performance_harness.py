"""Observable delay, byte continuity and teardown of the private lab relay."""
import importlib.util
from pathlib import Path
import socket
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('loopback_link', ROOT / 'scripts/loopback_link.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class LinkRelayTests(unittest.TestCase):
    def test_latency_bandwidth_continuity_and_bounded_shutdown(self):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen(1)
            listener.settimeout(3)
            payload = bytes(range(256)) * 256
            received = bytearray()

            def peer():
                with listener.accept()[0] as connection:
                    connection.settimeout(3)
                    while len(received) < len(payload):
                        data = connection.recv(8192)
                        if not data:
                            return
                        received.extend(data)
                        connection.sendall(data)

            thread = threading.Thread(target=peer)
            thread.start()
            relay = module.LinkRelay(1, 30, listen_port=0, target_port=listener.getsockname()[1])
            with relay:
                with socket.create_connection(('127.0.0.1', relay.listen_port), timeout=3) as client:
                    began = time.monotonic()
                    client.sendall(payload)
                    echoed = bytearray()
                    while len(echoed) < len(payload):
                        data = client.recv(8192)
                        self.assertTrue(data)
                        echoed.extend(data)
                    elapsed = time.monotonic() - began
                    self.assertEqual(echoed, payload)
                    self.assertGreater(elapsed, .3, '1 Mbps pacing and duplex delay must apply')
                    self.assertLess(elapsed, 3)
            thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            self.assertFalse(relay.thread.is_alive())
            self.assertEqual(received, payload)
            self.assertEqual(relay.bytes['server_to_client'], len(payload))
            self.assertEqual(relay.bytes['client_to_server'], len(payload))


if __name__ == '__main__':
    unittest.main()
