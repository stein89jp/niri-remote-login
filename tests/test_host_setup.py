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
    def run_worker(self, policy):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            config = directory / "renderer.conf"
            if policy is not None:
                config.write_text(policy + "\n")
            executable = directory / "niri-headless"
            executable.write_text("#!/bin/sh\nexec /usr/bin/env\n")
            executable.chmod(0o755)
            worker = (ROOT / "broker/config/session-worker").read_text()
            worker = worker.replace("/etc/niri-rdp/renderer.conf", str(config))
            worker = worker.replace("/usr/lib/niri-rdp/niri-headless", str(executable))
            environment = dict(os.environ, NIRI_RDP_RUNTIME_DIR=str(directory),
                               DISPLAY=":0", WAYLAND_DISPLAY="local-wayland",
                               LIBGL_ALWAYS_SOFTWARE="1", GALLIUM_DRIVER="llvmpipe")
            return subprocess.run(["/bin/sh", "-c", worker], env=environment,
                                  text=True, capture_output=True)

    def test_worker_auto_and_hardware_allow_gpu_without_local_desktop(self):
        for policy in (None, "auto", "hardware"):
            with self.subTest(policy=policy):
                result = self.run_worker(policy)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in result.stdout.splitlines())
                self.assertEqual(environment["NIRI_RDP_RENDERER"], policy or "auto")
                for key in ("LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "DISPLAY", "WAYLAND_DISPLAY"):
                    self.assertNotIn(key, environment)
                self.assertEqual(environment["XDG_RUNTIME_DIR"], environment["NIRI_RDP_RUNTIME_DIR"])

    def test_worker_explicit_software_retains_fallback(self):
        result = self.run_worker("software")
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = dict(line.split("=", 1) for line in result.stdout.splitlines())
        self.assertEqual(environment["LIBGL_ALWAYS_SOFTWARE"], "1")
        self.assertEqual(environment["GALLIUM_DRIVER"], "llvmpipe")
        self.assertEqual(environment["NIRI_RDP_RENDERER"], "software")

    def test_worker_rejects_invalid_policy_without_executing_shell_text(self):
        result = self.run_worker("auto; echo unsafe")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("must contain auto, hardware or software", result.stderr)

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
