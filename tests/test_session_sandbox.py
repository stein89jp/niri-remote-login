import json
import os
from pathlib import Path
import shutil
import stat
import sys
import time
import unittest
from unittest.mock import patch

from prototype.session_sandbox import SessionSandbox


class SessionIsolationTests(unittest.TestCase):
    def test_parent_desktop_credentials_and_startup_are_not_inherited(self):
        poison = {
            "WAYLAND_DISPLAY": "physical-desktop",
            "WAYLAND_SOCKET": "7",
            "DISPLAY": ":0",
            "NIRI_SOCKET": "/tmp/physical-niri",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/local-user-bus",
            "XDG_CONFIG_HOME": "/tmp/existing-gnome-config",
            "SSH_AUTH_SOCK": "/tmp/ssh-agent",
            "TEST_LOGIN_PASSWORD": "do-not-inherit",
            "LD_PRELOAD": "/tmp/untrusted.so",
        }
        with patch.dict(os.environ, poison), SessionSandbox() as session:
            proc = session.start(
                [sys.executable, "-c", "import json,os; print(json.dumps(dict(os.environ)))"],
                private_bus=False,
            )
            output, error = proc.communicate(timeout=5)
            self.assertEqual(proc.returncode, 0, error)
            child = json.loads(output)
            for key in poison:
                if key == "XDG_CONFIG_HOME":
                    self.assertEqual(child[key], str(session.root / "config"))
                else:
                    self.assertNotIn(key, child)
            self.assertEqual(child["HOME"], str(session.root / "home"))

    def test_two_sessions_have_private_runtime_and_data(self):
        with SessionSandbox() as first, SessionSandbox() as second:
            self.assertNotEqual(first.root, second.root)
            for session in (first, second):
                for path in (session.root, *session.root.iterdir()):
                    self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
                self.assertEqual(list((session.root / "config").iterdir()), [])
                self.assertEqual(list((session.root / "home").iterdir()), [])
            (first.root / "runtime" / "marker").write_text("owned by first")
            self.assertFalse((second.root / "runtime" / "marker").exists())

    @unittest.skipUnless(shutil.which("dbus-run-session"), "requires dbus-run-session")
    def test_bus_is_created_without_an_inherited_login_bus(self):
        with SessionSandbox() as session:
            proc = session.start([sys.executable, "-c", "import os; print(os.environ['DBUS_SESSION_BUS_ADDRESS'])"])
            output, error = proc.communicate(timeout=5)
            self.assertEqual(proc.returncode, 0, error)
            self.assertTrue(output.decode().strip().startswith("unix:"))
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", session.env)

    def test_disconnect_terminates_worker_and_cleans_runtime(self):
        session = SessionSandbox()
        root = session.root
        proc = session.start([sys.executable, "-c", "import time; time.sleep(60)"], private_bus=False)
        self.assertIsNone(proc.poll())
        session.close()
        self.assertIsNotNone(proc.poll())
        self.assertFalse(root.exists())
        session.close()
        with self.assertRaises(RuntimeError):
            session.start([sys.executable, "-c", "pass"])

    def test_worker_failure_is_visible_and_can_be_cleaned(self):
        with SessionSandbox() as session:
            proc = session.start([sys.executable, "-c", "raise SystemExit(23)"], private_bus=False)
            proc.communicate(timeout=5)
            self.assertEqual(proc.returncode, 23)
            root = session.root
        self.assertFalse(root.exists())

    def test_cleanup_reaches_child_after_worker_exits(self):
        session = SessionSandbox()
        script = (
            "import subprocess,sys; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], "
            "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(child.pid,flush=True)"
        )
        proc = session.start([sys.executable, "-c", script], private_bus=False)
        output, error = proc.communicate(timeout=5)
        self.assertEqual(proc.returncode, 0, error)
        child_pid = int(output)
        session.close()
        # A killed descendant may briefly remain a zombie until reaped by init.
        deadline = time.monotonic() + 1
        while True:
            try:
                status = Path(f"/proc/{child_pid}/stat").read_text().split(") ", 1)[1].split()[0]
            except FileNotFoundError:
                status = None
            if status in (None, "Z") or time.monotonic() >= deadline:
                break
            time.sleep(0.01)
        self.assertIn(status, (None, "Z"))

    def test_no_shell_or_path_search_and_no_root(self):
        with SessionSandbox() as session:
            with self.assertRaises(ValueError):
                session.start(["niri"])
            with self.assertRaises(ValueError):
                session.start([])
        with patch("os.geteuid", return_value=0), self.assertRaises(PermissionError):
            SessionSandbox()


if __name__ == "__main__":
    unittest.main()

