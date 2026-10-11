"""Exercise first-start TLS provisioning without changing the host."""
import importlib.util
from importlib.machinery import SourceFileLoader
import os
import json
from pathlib import Path
import subprocess
import shutil
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
portals_loader = SourceFileLoader("session_portals", str(ROOT / "broker/config/session-portals"))
portals = importlib.util.module_from_spec(importlib.util.spec_from_loader(portals_loader.name, portals_loader))
portals_loader.exec_module(portals)


class HostSetupTests(unittest.TestCase):
    def test_portal_startup_exports_only_private_activation_environment(self):
        environment = {"NIRI_RDP_RUNTIME_DIR": "/private/remote",
                       "XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-1",
                       "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus",
                       "XDG_CURRENT_DESKTOP": "niri", "XDG_CONFIG_HOME": "/user/settings",
                       "PULSE_SERVER": "unix:/private/remote/pulse/native"}
        with patch.dict(os.environ, environment, clear=True), \
                patch.object(portals.subprocess, "run") as update, \
                patch.object(portals, "replace_stale_backends") as replace, \
                patch.object(portals.os, "execve") as execute:
            portals.main()
            update.assert_called_once()
            arguments = update.call_args.args[0]
            self.assertNotIn("--systemd", arguments)
            self.assertIn("XDG_RUNTIME_DIR=/private/remote", arguments)
            self.assertIn("WAYLAND_DISPLAY=wayland-1", arguments)
            self.assertIn("DISPLAY=", arguments)
            self.assertIn("XDG_CURRENT_DESKTOP=niri:niri-rdp", arguments)
            self.assertEqual(update.call_args.kwargs["env"]["DBUS_SESSION_BUS_ADDRESS"],
                             environment["DBUS_SESSION_BUS_ADDRESS"])
            execute.assert_called_once()
            replace.assert_called_once_with(update.call_args.kwargs["env"])
            self.assertEqual(execute.call_args.args[2]["XDG_CONFIG_HOME"], "/user/settings")
            self.assertEqual(os.environ["XDG_CURRENT_DESKTOP"], "niri")

    def test_portal_backend_owner_query_addresses_only_private_bus(self):
        environment = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        with patch.object(portals.subprocess, "run",
                          return_value=subprocess.CompletedProcess([], 0, 's ":1.7"\n')) as query:
            self.assertEqual(portals.bus_owner("org.example.Backend", environment), 's ":1.7"')
            self.assertIn("--address=unix:path=/private/bus", query.call_args.args[0])
            self.assertIn("GetNameOwner", query.call_args.args[0])
            self.assertNotIn("StartServiceByName", query.call_args.args[0])

    def test_portal_backend_replacement_only_restarts_existing_owners(self):
        environment = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        with patch.object(portals, "bus_owner", side_effect=['s ":1.1"', 's ":1.1"', 's ":1.2"', None]), \
                patch.object(portals.Path, "is_file", return_value=True), \
                patch.object(portals.subprocess, "Popen") as start, \
                patch.object(portals.time, "sleep"):
            start.return_value.poll.return_value = None
            portals.replace_stale_backends(environment)
            start.assert_called_once_with(["/usr/lib/xdg-desktop-portal-gnome", "--replace"],
                                          env=environment)
        with patch.object(portals, "bus_owner", return_value=None), \
                patch.object(portals.subprocess, "Popen") as start:
            portals.replace_stale_backends(environment)
            start.assert_not_called()

    def test_portal_backend_replacement_is_bounded(self):
        with patch.object(portals, "bus_owner", return_value='s ":1.1"'), \
                patch.object(portals.Path, "is_file", return_value=True), \
                patch.object(portals.subprocess, "Popen") as start, \
                patch.object(portals.time, "monotonic", side_effect=[0, 0, 6]), \
                patch.object(portals.time, "sleep"):
            start.return_value.poll.return_value = None
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                portals.replace_stale_backends({"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"})
            start.return_value.terminate.assert_not_called()

    def test_portal_startup_rejects_non_remote_context_and_failed_export(self):
        contexts = [{}, {"NIRI_RDP_RUNTIME_DIR": "/private/remote",
                         "XDG_RUNTIME_DIR": "/run/user/1000", "WAYLAND_DISPLAY": "wayland-0"},
                    {"NIRI_RDP_RUNTIME_DIR": "/private/remote", "XDG_RUNTIME_DIR": "/private/remote",
                     "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}]
        for environment in contexts:
            with self.subTest(environment=environment), patch.dict(os.environ, environment, clear=True), \
                    patch.object(portals.subprocess, "run") as update, \
                    patch.object(portals.os, "execve") as execute:
                with self.assertRaises((KeyError, RuntimeError)):
                    portals.main()
                update.assert_not_called()
                execute.assert_not_called()
        with patch.dict(os.environ, {"NIRI_RDP_RUNTIME_DIR": "/private/remote",
                                    "XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-1",
                                    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}, clear=True), \
                patch.object(portals.subprocess, "run",
                             side_effect=subprocess.CalledProcessError(1, "export")), \
                patch.object(portals.os, "execve") as execute:
            with self.assertRaises(subprocess.CalledProcessError):
                portals.main()
            execute.assert_not_called()

    @unittest.skipUnless(all(shutil.which(command) for command in
                         ("dbus-run-session", "dbus-update-activation-environment", "busctl")),
                         "requires D-Bus command-line tools")
    def test_private_bus_activation_receives_remote_runtime_and_display(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for runtime in (directory / "physical", directory / "private"):
                runtime.mkdir(mode=0o700)
            services = directory / "data/dbus-1/services"
            services.mkdir(parents=True)
            output = directory / "activated.json"
            probe = directory / "probe.py"
            probe.write_text("import json, os\nfrom pathlib import Path\n" +
                             "Path(" + repr(str(output)) + ").write_text(json.dumps(dict(os.environ)))\n")
            (services / "org.example.NiriRdpActivationProbe.service").write_text(
                "[D-BUS Service]\nName=org.example.NiriRdpActivationProbe\nExec=/usr/bin/python3 " +
                str(probe) + "\n")
            runner = directory / "runner.py"
            runner.write_text(
                "import os, runpy, subprocess\nfrom unittest.mock import patch\n" +
                "os.environ.update(XDG_RUNTIME_DIR=" + repr(str(directory / "private")) +
                ", NIRI_RDP_RUNTIME_DIR=" + repr(str(directory / "private")) +
                ", WAYLAND_DISPLAY='wayland-remote', PULSE_SERVER='unix:/private/pulse/native')\n" +
                "with patch('os.execve'):\n    runpy.run_path(" +
                repr(str(ROOT / "broker/config/session-portals")) + ", run_name='__main__')\n" +
                # The probe records its activation environment and exits without owning the name.
                "subprocess.run(['busctl', '--address=' + os.environ['DBUS_SESSION_BUS_ADDRESS'], " +
                "'--timeout=2', 'call', 'org.freedesktop.DBus', '/org/freedesktop/DBus', " +
                "'org.freedesktop.DBus', 'StartServiceByName', 'su', " +
                "'org.example.NiriRdpActivationProbe', '0'], capture_output=True)\n")
            environment = dict(os.environ, XDG_RUNTIME_DIR=str(directory / "physical"),
                               XDG_DATA_HOME=str(directory / "data"))
            for key in ("DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"):
                environment.pop(key, None)
            result = subprocess.run(["dbus-run-session", "--", "/usr/bin/python3", str(runner)],
                                    env=environment, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            activated = json.loads(output.read_text())
            self.assertEqual(activated["XDG_RUNTIME_DIR"], str(directory / "private"))
            self.assertEqual(activated["WAYLAND_DISPLAY"], "wayland-remote")
            self.assertEqual(activated["XDG_CURRENT_DESKTOP"], "niri:niri-rdp")
            self.assertEqual(activated["PULSE_SERVER"], "unix:/private/pulse/native")
            self.assertEqual(activated["DISPLAY"], "")

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
            result = subprocess.run(["/bin/sh", "-c", worker], env=environment,
                                    text=True, capture_output=True)
            if remote_config:
                self.assertEqual(remote.read_text(), "// private remote configuration\n")
            if desktop_config:
                self.assertEqual(desktop.read_text(), 'binds { Mod+D { spawn "fuzzel"; } }\n')
            return result

    def test_worker_selects_explicit_remote_config_without_importing_local_config(self):
        for custom in (False, True):
            with self.subTest(custom_config_home=custom):
                result = self.run_worker("auto", remote_config=True, custom_config_home=custom,
                                         desktop_config=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
                self.assertIn('include "' + environment["NIRI_TEST_EXPECTED_CONFIG"] + '"', result.stdout)
                self.assertIn('spawn-at-startup "/usr/lib/niri-rdp/session-portals"', result.stdout)
                self.assertNotIn('spawn-at-startup "/usr/lib/niri-rdp/session-desktop"', result.stdout)
                self.assertNotIn('niri/config.kdl"', result.stdout)

    def test_worker_reuses_host_config_with_private_bar_and_headless_output(self):
        for custom in (False, True):
            with self.subTest(custom_config_home=custom):
                result = self.run_worker("auto", desktop_config=True, custom_config_home=custom)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('niri/config.kdl"', result.stdout)
                self.assertIn('output "HEADLESS-1" { scale 1; transform "normal"; }', result.stdout)
                self.assertIn('spawn-at-startup "/usr/lib/niri-rdp/session-desktop"', result.stdout)
                self.assertIn('spawn-at-startup "/usr/lib/niri-rdp/session-portals"', result.stdout)

    def test_worker_uses_system_remote_config_by_default(self):
        result = self.run_worker("auto")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('include "/etc/niri-rdp/headless.kdl"', result.stdout)
        self.assertIn('spawn-at-startup "/usr/lib/niri-rdp/session-portals"', result.stdout)

    def test_worker_auto_and_hardware_allow_gpu_without_local_desktop(self):
        for policy in (None, "auto", "hardware"):
            with self.subTest(policy=policy):
                result = self.run_worker(policy)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
                self.assertEqual(environment["NIRI_RDP_RENDERER"], policy or "auto")
                for key in ("LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "DISPLAY", "WAYLAND_DISPLAY"):
                    self.assertNotIn(key, environment)
                self.assertEqual(environment["XDG_RUNTIME_DIR"], environment["NIRI_RDP_RUNTIME_DIR"])

    def test_worker_explicit_software_retains_fallback(self):
        result = self.run_worker("software")
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
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
