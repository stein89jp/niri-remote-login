#!/usr/bin/python3
"""Known Unicode text through real CLIPRDR and two isolated Wayland desktops.

Run with lab/run-isolated.sh. No host display, clipboard or bus is consulted.
"""
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def wait_path(path, process):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("private compositor exited before readiness")
        if path.exists():
            return
        time.sleep(0.05)
    raise RuntimeError("private compositor startup deadline")


def marker(process, expected):
    deadline = time.monotonic() + 10
    pending = getattr(process, "clipboard_pending", "")
    while time.monotonic() < deadline:
        if "\n" in pending:
            line, pending = pending.split("\n", 1)
            process.clipboard_pending = pending
            if line.strip() == expected:
                return
            continue
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(max(0, deadline - time.monotonic())):
                break
        data = os.read(process.stdout.fileno(), 4096)
        if not data:
            raise RuntimeError("CLIPRDR client exited: " + process.stderr.read())
        pending += data.decode()
    raise RuntimeError("missing clipboard marker: " + expected)


def copy(environment, text):
    process = subprocess.Popen(["wl-copy", "--foreground", "--type", "text/plain;charset=utf-8"],
                               env=environment, stdin=subprocess.PIPE,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    process.stdin.write(text.encode())
    process.stdin.close()
    return process


def paste(environment):
    return subprocess.run(["wl-paste", "--no-newline", "--type", "text/plain;charset=utf-8"],
                          env=environment, capture_output=True, timeout=3)


def main():
    if Path.cwd() != Path("/work") or os.environ.get("HOME") != "/home/test":
        raise RuntimeError("run only through lab/run-isolated.sh")
    processes = []
    logs = []
    with tempfile.TemporaryDirectory(prefix="niri-clipboard-") as temporary:
        private = Path(temporary)
        base = dict(os.environ)
        for name in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "NIRI_SOCKET", "DBUS_SESSION_BUS_ADDRESS"):
            base.pop(name, None)
        try:
            environments = []
            desktops = []
            for index in range(2):
                runtime = private / str(index)
                runtime.mkdir(mode=0o700)
                environment = dict(base, XDG_RUNTIME_DIR=str(runtime))
                log = open(private / f"niri-{index}.log", "wb")
                logs.append(log)
                desktop = subprocess.Popen([str(ROOT / os.environ.get(
                    "NIRI_RDP_LAB_NIRI", "compositor/upstream/target/debug/niri")),
                    "--config", str(ROOT / "lab/headless.kdl")], env=environment,
                    stdout=log, stderr=log, start_new_session=True)
                processes.append(desktop)
                desktops.append(desktop)
                wait_path(runtime / "wayland-0", desktop)
                environments.append(dict(environment, WAYLAND_DISPLAY=str(runtime / "wayland-0")))
            isolated = copy(environments[1], "second private desktop")
            processes.append(isolated)
            subprocess.run(["openssl", "req", "-config", "/dev/null", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                            "-subj", "/CN=private-clipboard-fixture", "-keyout", str(private / "key.pem"),
                            "-out", str(private / "cert.pem")], check=True, capture_output=True)
            username, password = "clipboard-fixture", secrets.token_hex(20)
            environment = dict(base, NIRI_RDP_MODE="disposable", NIRI_RDP_ENCODER="bitmap",
                               NIRI_RDP_CERT=str(private / "cert.pem"), NIRI_RDP_KEY=str(private / "key.pem"),
                               NIRI_RDP_USERNAME=username, NIRI_RDP_PASSWORD=password,
                               NIRI_RDP_WAYLAND_SOCKET=environments[0]["WAYLAND_DISPLAY"])
            log = open(private / "bridge.log", "wb")
            logs.append(log)
            bridge = subprocess.Popen([str(ROOT / os.environ.get("NIRI_RDP_LAB_BRIDGE",
                "bridge/target/debug/niri-rdp-bridge"))], env=environment, stdout=log, stderr=log)
            processes.append(bridge)
            time.sleep(1)
            if bridge.poll() is not None:
                raise RuntimeError("frontend startup failed: " + (private / "bridge.log").read_text())
            receipts = []
            for cycle, empty in enumerate((False, True, False)):
                client_environment = dict(base, WLOG_LEVEL="OFF", NIRI_RDP_TEST_NLA="1",
                                          NIRI_RDP_TEST_USERNAME=username, NIRI_RDP_TEST_PASSWORD=password)
                if empty:
                    client_environment["NIRI_RDP_TEST_CLIPBOARD_EMPTY"] = "1"
                client = subprocess.Popen([str(ROOT / "artifacts/rdp-clipboard-client")], env=client_environment,
                                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                          text=True, bufsize=1)
                processes.append(client)
                marker(client, "NIRI_RDP_CLIPBOARD_READY")
                if cycle == 0:
                    oversized = copy(environments[0], "x" * (1024 * 1024 + 1))
                    processes.append(oversized)
                    time.sleep(0.3)
                    if client.poll() is not None:
                        raise RuntimeError("oversize native selection broke the clipboard channel")
                    for _ in range(3):
                        processes.append(copy(environments[0], "stale"))
                        time.sleep(0.01)
                native = copy(environments[0], "" if empty else "Wayland 日本😀\nline 2")
                processes.append(native)
                marker(client, "NIRI_RDP_CLIPBOARD_NATIVE_TO_RDP_OK")
                marker(client, "NIRI_RDP_CLIPBOARD_RDP_DATA_SENT")
                deadline = time.monotonic() + 5
                expected = b"" if empty else "RDP 日本😀\nline 2".encode()
                while True:
                    result = paste(environments[0])
                    types = subprocess.run(["wl-paste", "--list-types"], env=environments[0], capture_output=True, timeout=3)
                    if result.returncode == 0 and result.stdout == expected and b"application/x-niri-rdp-private-text" in types.stdout:
                        break
                    if time.monotonic() > deadline:
                        raise RuntimeError("RDP text did not become the native selection")
                    time.sleep(0.05)
                if paste(environments[1]).stdout != b"second private desktop":
                    raise RuntimeError("clipboard escaped its authorized compositor")
                client.stdin.write("stop\n")
                client.stdin.flush()
                if client.wait(timeout=5) != 0:
                    raise RuntimeError("clipboard client failed: " + client.stderr.read())
                if any(desktop.poll() is not None for desktop in desktops):
                    raise RuntimeError("clipboard reconnect replaced or ended a desktop")
                receipts.append({"empty": empty, "unicode_and_multiline": not empty,
                                 "native_to_rdp": True, "rdp_to_native": True, "other_desktop_unchanged": True,
                                 "native_oversize_then_recovery": cycle == 0,
                                 "rapid_ownership_converged": cycle == 0})
                time.sleep(0.2)
            rendered = json.dumps({"client": "FreeRDP CLIPRDR", "compositors": 2, "results": receipts,
                                   "actual_windows_acceptance": "pending"}, indent=2)
            print(rendered)
            if report := os.environ.get("NIRI_RDP_CLIPBOARD_REPORT"):
                Path(report).write_text(rendered + "\n")
        except Exception:
            for log in private.glob("*.log"):
                print(log.name + ":\n" + log.read_text(errors="replace"))
            raise
        finally:
            for process in reversed(processes):
                if process.poll() is None:
                    if process in desktops:
                        os.killpg(process.pid, signal.SIGTERM)
                    else:
                        process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            for log in logs:
                log.close()


if __name__ == "__main__":
    main()
