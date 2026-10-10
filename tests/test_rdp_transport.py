"""Opt-in, displayless FreeRDP interop. Never launch a listener in default discovery.

POC_RDP_BINARY=$PWD/poc/target/debug/niri-rdp-transport-poc \
    python3 -m unittest tests.test_rdp_transport -v
"""
import os
from pathlib import Path
import secrets
import selectors
import shlex
import shutil
import socket
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.environ.get("POC_RDP_BINARY"), "set POC_RDP_BINARY to opt into loopback interop")
class RdpTransportTests(unittest.TestCase):
    def test_nla_rejection_decoded_frames_and_roundtrip_input(self):
        binary = Path(os.environ["POC_RDP_BINARY"]).resolve(strict=True)
        for command in ("cc", "pkg-config", "openssl"):
            self.assertIsNotNone(shutil.which(command), f"requires installed {command}")
        # Do not connect to, replace, or stop any existing listener on this port.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 33990))
        with tempfile.TemporaryDirectory(prefix="niri-rdp-interop-", dir="/tmp") as directory:
            root = Path(directory)
            flags = subprocess.check_output(
                ["pkg-config", "--cflags", "--libs", "freerdp3", "winpr3"], text=True
            )
            subprocess.run(
                ["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-Wno-deprecated-declarations", "-O2",
                 str(Path(__file__).with_name("rdp_client.c")), "-o", str(root / "client"),
                 *shlex.split(flags)], check=True, capture_output=True
            )
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                 "-subj", "/CN=localhost", "-addext", "subjectAltName=IP:127.0.0.1",
                 "-keyout", str(root / "key.pem"), "-out", str(root / "cert.pem")],
                check=True, capture_output=True
            )
            (root / "key.pem").chmod(0o600)
            # No GUI or host desktop/session credentials are inherited by either process.
            environment = {"PATH": "/usr/bin:/bin", "HOME": str(root),
                           "XDG_CONFIG_HOME": str(root / "config"), "WLOG_LEVEL": "OFF",
                           "POC_CERT": str(root / "cert.pem"), "POC_KEY": str(root / "key.pem"),
                           "POC_USERNAME": "disposable-" + secrets.token_hex(8),
                           "POC_PASSWORD": secrets.token_hex(24)}
            server = subprocess.Popen([str(binary)], env=environment, stdout=subprocess.PIPE,
                                      stderr=subprocess.DEVNULL, text=True)
            try:
                # The startup message identifies this child; every client call is bounded.
                with selectors.DefaultSelector() as selector:
                    selector.register(server.stdout, selectors.EVENT_READ)
                    self.assertTrue(selector.select(5), "server startup timed out")
                    self.assertIn("127.0.0.1:33990", server.stdout.readline())
                self.assertIsNone(server.poll(), "server exited during startup")
                rejected = dict(environment, POC_PASSWORD=secrets.token_hex(24))
                bad = subprocess.run([str(root / "client")], env=rejected,
                                     capture_output=True, text=True, timeout=20)
                self.assertEqual(bad.returncode, 4, "wrong credentials must be rejected by NLA")
                self.assertIn("NLA_AUTHENTICATION_REJECTED", bad.stdout)
                for _ in range(2):
                    good = subprocess.run([str(root / "client")], env=environment,
                                          capture_output=True, text=True, timeout=20)
                    self.assertEqual(good.returncode, 0,
                                     "FreeRDP frame/input exchange failed: " + good.stderr)
                    self.assertIn("NLA_TLS_FRAME_KEY_POINTER_RELEASE_OK", good.stdout)
            finally:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
                server.stdout.close()


if __name__ == "__main__":
    unittest.main()
