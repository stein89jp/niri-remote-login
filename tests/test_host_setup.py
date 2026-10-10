"""Exercise first-start TLS provisioning without changing the host."""
import importlib.util
from importlib.machinery import SourceFileLoader
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
desktop_loader = SourceFileLoader("session_desktop", str(ROOT / "broker/config/session-desktop"))
desktop = importlib.util.module_from_spec(importlib.util.spec_from_loader(desktop_loader.name, desktop_loader))
desktop_loader.exec_module(desktop)


class HostSetupTests(unittest.TestCase):
    def test_private_bar_detection_ignores_local_desktop_and_other_applications(self):
        with tempfile.TemporaryDirectory() as temporary:
            proc = Path(temporary)
            process = proc / "123"
            process.mkdir()
            (process / "cmdline").write_bytes(b"/usr/bin/dms\0run\0")
            with patch.object(desktop, "PROC", proc), patch.dict(os.environ, {
                    "XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-0"}):
                (process / "environ").write_bytes(b"XDG_RUNTIME_DIR=/run/user/1000\0WAYLAND_DISPLAY=wayland-0\0")
                self.assertFalse(desktop.bar_running())
                (process / "environ").write_bytes(b"XDG_RUNTIME_DIR=/private/remote\0WAYLAND_DISPLAY=wayland-1\0")
                self.assertFalse(desktop.bar_running())
                (process / "environ").write_bytes(b"XDG_RUNTIME_DIR=/private/remote\0WAYLAND_DISPLAY=wayland-0\0")
                self.assertTrue(desktop.bar_running())
                (process / "cmdline").write_bytes(b"/usr/bin/foot\0")
                self.assertFalse(desktop.bar_running())

    def test_private_bar_startup_waits_bounded_time_and_does_not_duplicate(self):
        with patch.object(desktop, "bar_running", side_effect=[False, False, True]), \
                patch.object(desktop.time, "sleep") as sleep, patch.object(desktop.os, "execvp") as execute:
            desktop.main()
            self.assertEqual(sleep.call_count, 2)
            execute.assert_not_called()
        with patch.object(desktop, "bar_running", return_value=False), \
                patch.object(desktop.time, "sleep") as sleep, \
                patch.object(desktop.shutil, "which", return_value="/usr/bin/dms"), \
                patch.object(desktop.os, "execvp") as execute:
            desktop.main()
            self.assertEqual(sleep.call_count, 20)
            execute.assert_called_once_with("dms", ["dms", "run"])

    def run_worker(self, policy, remote_config=False, custom_config_home=False, desktop_config=False):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            config = directory / "renderer.conf"
            if policy is not None:
                config.write_text(policy + "\n")
            executable = directory / "niri-headless"
            executable.write_text('#!/bin/sh\nexport NIRI_TEST_CONFIG="$2"\n'
                                 'if [ "$2" = "$NIRI_RDP_RUNTIME_DIR/config.kdl" ]; then\n'
                                 'export NIRI_TEST_WRAPPER="$(cat "$2")"\nfi\nexec /usr/bin/env\n')
            executable.chmod(0o755)
            worker = (ROOT / "broker/config/session-worker").read_text()
            worker = worker.replace("/etc/niri-rdp/renderer.conf", str(config))
            worker = worker.replace("/usr/lib/niri-rdp/session-audio &", "true &")
            worker = worker.replace('while [ ! -S "$NIRI_RDP_RUNTIME_DIR/audio-0" ]; do', 'while false; do')
            worker = worker.replace("/usr/lib/niri-rdp/niri-headless", str(executable))
            environment = dict(os.environ, NIRI_RDP_RUNTIME_DIR=str(directory),
                               HOME=str(directory),
                               DISPLAY=":0", WAYLAND_DISPLAY="local-wayland",
                               LIBGL_ALWAYS_SOFTWARE="1", GALLIUM_DRIVER="llvmpipe")
            environment.pop("XDG_CONFIG_HOME", None)
            config_home = directory / ("custom config" if custom_config_home else ".config")
            if custom_config_home:
                environment["XDG_CONFIG_HOME"] = str(config_home)
            if remote_config:
                remote = config_home / "niri-rdp" / "config.kdl"
                remote.parent.mkdir(parents=True)
                remote.write_text("// private remote configuration\n")
                environment["NIRI_TEST_EXPECTED_CONFIG"] = str(remote)
            if desktop_config:
                desktop = config_home / "niri" / "config.kdl"
                desktop.parent.mkdir(parents=True)
                desktop.write_text('binds { Mod+D { spawn "fuzzel"; } }\n')
            return subprocess.run(["/bin/sh", "-c", worker], env=environment,
                                  text=True, capture_output=True)

    def test_worker_selects_explicit_remote_config_without_importing_local_config(self):
        for custom in (False, True):
            with self.subTest(custom_config_home=custom):
                result = self.run_worker("auto", remote_config=True, custom_config_home=custom,
                                         desktop_config=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in result.stdout.splitlines())
                self.assertEqual(environment["NIRI_TEST_CONFIG"], environment["NIRI_TEST_EXPECTED_CONFIG"])

    def test_worker_reuses_host_config_with_private_bar_and_headless_output(self):
        for custom in (False, True):
            with self.subTest(custom_config_home=custom):
                result = self.run_worker("auto", desktop_config=True, custom_config_home=custom)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('niri/config.kdl"', result.stdout)
                self.assertIn('output "HEADLESS-1" { scale 1; transform "normal"; }', result.stdout)
                self.assertIn('spawn-at-startup "/usr/lib/niri-rdp/session-desktop"', result.stdout)

    def test_worker_uses_system_remote_config_by_default(self):
        result = self.run_worker("auto")
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = dict(line.split("=", 1) for line in result.stdout.splitlines())
        self.assertEqual(environment["NIRI_TEST_CONFIG"], "/etc/niri-rdp/headless.kdl")

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
