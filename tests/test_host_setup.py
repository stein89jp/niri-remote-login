"""Exercise first-start TLS provisioning without changing the host."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "packaging" / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tls = load("prepare_tls", "prepare-tls.py")
setup = load("host_setup", "niri-rdp-setup.py")


class HostSetupTests(unittest.TestCase):
    def test_certificate_identity_rejects_openssl_syntax_injection(self):
        for value in ("host,IP:1.2.3.4", "host/CN=other", "-host", "", "a..b"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                tls.identity_names(value)
        self.assertEqual(tls.identity_names("192.168.1.20")[1],
                         "IP:192.168.1.20,DNS:localhost,IP:127.0.0.1")
        self.assertIn("DNS:arch.local,DNS:arch", tls.identity_names("arch.local")[1])

    def test_partial_identity_is_preserved_and_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            cert = directory / "cert.pem"
            cert.write_text("existing certificate")
            with self.assertRaisesRegex(ValueError, "incomplete TLS identity"):
                tls.ensure_tls(directory, "test-host", os.getuid())
            self.assertEqual(cert.read_text(), "existing certificate")
            self.assertFalse((directory / "key.pem").exists())

    def test_key_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "cert.pem").write_text("certificate")
            (directory / "target").write_text("preserve")
            (directory / "key.pem").symlink_to(directory / "target")
            with self.assertRaisesRegex(ValueError, "regular file"):
                tls.ensure_tls(directory, "test-host", os.getuid())
            self.assertEqual((directory / "target").read_text(), "preserve")

    @unittest.skipUnless(Path("/usr/bin/openssl").exists(), "OpenSSL not installed")
    def test_real_tls_creation_and_restart_preserve_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.assertTrue(tls.ensure_tls(directory, "arch.local", os.getuid()))
            cert = (directory / "cert.pem").read_bytes()
            key = (directory / "key.pem").read_bytes()
            self.assertEqual((directory / "key.pem").stat().st_mode & 0o777, 0o600)
            self.assertFalse(tls.ensure_tls(directory, "different-host", os.getuid()))
            self.assertEqual((directory / "cert.pem").read_bytes(), cert)
            self.assertEqual((directory / "key.pem").read_bytes(), key)
            result = subprocess.run(["/usr/bin/openssl", "x509", "-in", str(directory / "cert.pem"),
                                     "-noout", "-checkhost", "arch.local"],
                                    check=True, capture_output=True, text=True)
            self.assertIn("does match", result.stdout)
            (directory / "key.pem").chmod(0o644)
            with self.assertRaisesRegex(ValueError, "unsafe permissions"):
                tls.ensure_tls(directory, "arch.local", os.getuid())

    def test_system_accounts_cannot_be_authorized(self):
        account = type("Account", (), {"pw_uid": 0, "pw_shell": "/bin/bash"})()
        with patch.object(setup.pwd, "getpwnam", return_value=account), \
                patch.object(setup.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "normal login account"):
                setup.authorize("root")
            run.assert_not_called()

    def test_existing_account_gets_supplementary_membership_without_password_reset(self):
        account = type("Account", (), {"pw_uid": 1000, "pw_shell": "/bin/bash"})()
        with patch.object(setup.pwd, "getpwnam", return_value=account), \
                patch.object(setup.subprocess, "run") as run:
            setup.authorize("alice")
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args.args[0],
                             ["/usr/bin/usermod", "-aG", "niri-rdp-users", "--", "alice"])


if __name__ == "__main__":
    unittest.main()
