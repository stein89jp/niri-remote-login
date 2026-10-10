"""Run trusted test workers with a private desktop environment and process group.

This is a current-UID experiment, not a login broker or a security sandbox against
hostile code. It creates no PAM/logind session and grants no device access.
"""

from __future__ import annotations

import os
from pathlib import Path
import signal
import subprocess
import tempfile


class SessionSandbox:
    def __init__(self, parent: Path | None = None):
        if os.geteuid() == 0:
            raise PermissionError("the local experiment must run unprivileged")
        self._temp = tempfile.TemporaryDirectory(prefix="niri-rdp-", dir=parent)
        self.root = Path(self._temp.name)
        self._processes: list[subprocess.Popen] = []
        self._closed = False
        for name in ("runtime", "home", "config", "data", "cache", "state"):
            (self.root / name).mkdir(mode=0o700)
        self.env = {
            "PATH": "/usr/bin:/bin",
            "LANG": "C.UTF-8",
            "HOME": str(self.root / "home"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_SESSION_TYPE": "wayland",
            "XDG_CURRENT_DESKTOP": "niri",
        }

    def start(self, command: list[str], *, private_bus: bool = True) -> subprocess.Popen:
        """Only pass trusted argv; no shell, parent environment, or --session wrapper."""
        if self._closed:
            raise RuntimeError("session is closed")
        if not command or not Path(command[0]).is_absolute():
            raise ValueError("worker executable must be an absolute path")
        argv = ["/usr/bin/dbus-run-session", "--", *command] if private_bus else command
        process = subprocess.Popen(
            argv,
            cwd=self.root,
            env=self.env.copy(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self._processes.append(process)
        return process

    def close(self):
        if self._closed:
            return
        self._closed = True
        # Kill the group even when its original worker has already exited.
        # This includes dbus-daemon and subprocesses left by that worker.
        for process in reversed(self._processes):
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=2)
            for pipe in (process.stdout, process.stderr):
                if pipe is not None:
                    pipe.close()
        self._temp.cleanup()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

