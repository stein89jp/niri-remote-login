"""Configured input settings follow the host while display transport stays private."""
import importlib.machinery
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
loader = importlib.machinery.SourceFileLoader(
    "session_environment", str(ROOT / "broker/config/session-environment"))
spec = importlib.util.spec_from_loader(loader.name, loader)
environment_helper = importlib.util.module_from_spec(spec)
loader.exec_module(environment_helper)


class SessionEnvironmentTests(unittest.TestCase):
    def test_generator_quotes_and_shell_punctuation_are_data(self):
        output = ('XMODIFIERS=@im=ibus\n'
                  'QT_IM_MODULES="wayland;ibus"\n'
                  'XDG_CONFIG_HOME="/home/test/config with spaces"\n'
                  'XKB_DEFAULT_OPTIONS="quote\\\" slash\\\\ dollar\\$ backtick\\`"\n'
                  'GTK_IM_MODULE="$(touch /tmp/niri-rdp-must-not-execute)"\n'
                  'INPUT_METHOD="Japanese 日本語\\nline\\tend\\001"\n')
        with patch.object(environment_helper.subprocess, "run") as run:
            settings = environment_helper.generator_settings(output)
        run.assert_not_called()
        self.assertEqual(settings["XMODIFIERS"], "@im=ibus")
        self.assertEqual(settings["QT_IM_MODULES"], "wayland;ibus")
        self.assertEqual(settings["XDG_CONFIG_HOME"], "/home/test/config with spaces")
        self.assertEqual(settings["XKB_DEFAULT_OPTIONS"], 'quote" slash\\ dollar$ backtick`')
        self.assertEqual(settings["GTK_IM_MODULE"], "$(touch /tmp/niri-rdp-must-not-execute)")
        self.assertEqual(settings["INPUT_METHOD"], "Japanese 日本語\nline\tend\x01")

    def test_generator_rejects_invalid_quoted_values(self):
        for value in ('"unfinished', '"bad\\q"', '"nul\\000"', '"bad\\777"',
                      '"quote"inside"', 'unquoted spaces'):
            with self.subTest(value=value), self.assertRaises(ValueError):
                environment_helper.generator_settings("QT_IM_MODULE=" + value)

    def test_private_transport_and_unrelated_settings_are_never_imported(self):
        output = '\n'.join(name + '=host-value' for name in (
            "DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS", "NIRI_SOCKET", "SSH_AUTH_SOCK", "PATH",
            "HOME", "PULSE_SERVER", "PIPEWIRE_REMOTE", "SECRET_TOKEN", "LANG",
            "LC_CTYPE", "XKB_DEFAULT_LAYOUT", "XDG_DATA_DIRS", "SDL_IM_MODULE"))
        self.assertEqual(environment_helper.generator_settings(output), {
            "LANG": "host-value", "LC_CTYPE": "host-value",
            "XKB_DEFAULT_LAYOUT": "host-value", "XDG_DATA_DIRS": "host-value",
            "SDL_IM_MODULE": "host-value"})

    def test_locale_fallback_reads_assignments_without_running_shell(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "locale.conf"
            path.write_text('# host locale\nLANG="sv_SE.UTF-8"\nLC_CTYPE=ja_JP.UTF-8\n'
                            'QT_IM_MODULE=ignored\nSECRET_TOKEN=ignored\n')
            self.assertEqual(environment_helper.locale_settings(path), {
                "LANG": "sv_SE.UTF-8", "LC_CTYPE": "ja_JP.UTF-8"})

    def test_manager_query_uses_original_bus_and_filters_response(self):
        environment = {"XDG_RUNTIME_DIR": "/run/user/1000",
                       "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        reply = {"type": "as", "data": ["QT_IM_MODULE=ibus", "LANG=en_US.UTF-8",
                                           "WAYLAND_DISPLAY=local", "SECRET_TOKEN=hidden"]}
        result = subprocess.CompletedProcess([], 0, json.dumps(reply))
        with patch.object(environment_helper.subprocess, "run", return_value=result) as run:
            self.assertEqual(environment_helper.manager_settings(environment), {
                "QT_IM_MODULE": "ibus", "LANG": "en_US.UTF-8"})
        self.assertIn("--address=unix:path=/run/user/1000/bus", run.call_args.args[0])
        self.assertEqual(run.call_args.kwargs["env"], environment)

    def test_manager_unavailable_is_optional(self):
        with patch.object(environment_helper.subprocess, "run",
                          side_effect=subprocess.CalledProcessError(1, ["busctl"])):
            self.assertEqual(environment_helper.manager_settings({
                "XDG_RUNTIME_DIR": "/run/user/1000"}), {})

    def test_wrapper_executes_only_fixed_worker_as_authenticated_user(self):
        base = {"NIRI_RDP_RUNTIME_DIR": "/private/runtime"}
        arguments = ["session-environment", environment_helper.WORKER, "--environment-ready"]
        with patch.object(environment_helper.sys, "argv", arguments), \
                patch.object(environment_helper.os, "geteuid", return_value=1000), \
                patch.object(environment_helper.os, "environ", base), \
                patch.object(environment_helper, "session_environment", return_value=base), \
                patch.object(environment_helper.os, "execve") as execute:
            environment_helper.main()
            execute.assert_called_once_with(environment_helper.WORKER, arguments[1:], base)
        for arguments, uid, variables in (
                (["helper", "/bin/sh", "--environment-ready"], 1000, base),
                (["helper", environment_helper.WORKER, "--environment-ready"], 0, base),
                (["helper", environment_helper.WORKER, "--environment-ready"], 1000, {})):
            with self.subTest(arguments=arguments, uid=uid, variables=variables), \
                    patch.object(environment_helper.sys, "argv", arguments), \
                    patch.object(environment_helper.os, "geteuid", return_value=uid), \
                    patch.object(environment_helper.os, "environ", variables), \
                    self.assertRaises(RuntimeError):
                environment_helper.main()

    def test_environment_d_overrides_manager_without_guessing_framework(self):
        base = {"LANG": "C.UTF-8", "XDG_RUNTIME_DIR": "/run/user/1000",
                "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        generated = subprocess.CompletedProcess([], 0,
            'QT_IM_MODULE=ibus\nXMODIFIERS=@im=ibus\nXKB_DEFAULT_LAYOUT=us,se\n'
            'XDG_RUNTIME_DIR=/host\nDBUS_SESSION_BUS_ADDRESS=host-bus\n')
        with patch.object(environment_helper, "locale_settings", return_value={"LANG": "sv_SE.UTF-8"}), \
                patch.object(environment_helper, "manager_settings", return_value={"QT_IM_MODULE": "fcitx"}), \
                patch.object(environment_helper.subprocess, "run", return_value=generated):
            actual = environment_helper.session_environment(base)
        self.assertEqual(actual, dict(base, LANG="sv_SE.UTF-8", QT_IM_MODULE="ibus",
                                     XMODIFIERS="@im=ibus", XKB_DEFAULT_LAYOUT="us,se"))
        self.assertNotIn("GTK_IM_MODULE", actual)

    def test_real_native_generator_supports_home_config_and_variable_expansion(self):
        if not Path(environment_helper.GENERATOR).exists():
            self.skipTest("systemd environment.d generator unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            configuration = Path(temporary) / "configuration"
            directory = configuration / "environment.d"
            directory.mkdir(parents=True)
            (directory / "99-niri-rdp-test.conf").write_text(
                'QT_IM_MODULE=ibus\nXMODIFIERS=@im=ibus\n'
                'XDG_DATA_HOME=${HOME}/data with spaces\n'
                'INPUT_METHOD="literal $ dollar and ` backtick and 日本語"\n')
            generated = subprocess.run([environment_helper.GENERATOR],
                env={"HOME": temporary, "XDG_CONFIG_HOME": str(configuration)},
                capture_output=True, text=True, check=True)
            settings = environment_helper.generator_settings(generated.stdout)
            self.assertEqual(settings["QT_IM_MODULE"], "ibus")
            self.assertEqual(settings["XDG_DATA_HOME"], temporary + "/data with spaces")
            self.assertEqual(settings["INPUT_METHOD"], "literal $ dollar and ` backtick and 日本語")


if __name__ == "__main__":
    unittest.main()
