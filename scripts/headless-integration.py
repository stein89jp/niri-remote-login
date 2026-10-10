#!/usr/bin/env python3
"""Real Niri/app/RDP test; run inside lab/run-isolated.sh, never a host desktop."""
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time
from mock_broker import MockBroker

ROOT = Path(__file__).resolve().parents[1]


def wait_for(path, process):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"child exited before readiness: {process.returncode}")
        if path.exists():
            return
        time.sleep(0.05)
    raise RuntimeError("private compositor socket startup timed out")


def stop(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def main():
    if ROOT != Path("/work") or Path("/dev/dri").exists() or os.environ.get("DISPLAY"):
        raise RuntimeError("run only inside lab/run-isolated.sh with private devices")
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    username, password = "disposable-" + secrets.token_hex(8), secrets.token_hex(24)
    tls_contract = os.environ.get("NIRI_RDP_LAB_TLS_CONTRACT") == "1"
    # The namespace supplies only these explicit environment values.
    env = {k: os.environ[k] for k in ("PATH", "HOME", "XDG_RUNTIME_DIR", "LD_LIBRARY_PATH",
                                    "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "XKB_CONFIG_ROOT")
           if k in os.environ}
    env["NIRI_BACKEND"] = "headless"
    with socket.socket() as port:
        port.bind(("127.0.0.1", 33991))
    children = []
    results = {}
    with tempfile.TemporaryDirectory(prefix="niri-real-integration-", dir="/tmp") as folder:
        private = Path(folder)
        subprocess.run(["openssl", "req", "-config", "/dev/null", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                        "-subj", "/CN=localhost", "-keyout", str(private / "key.pem"),
                        "-out", str(private / "cert.pem")], check=True, capture_output=True)
        (private / "key.pem").chmod(0o600)
        logs = []

        def launch(argv, environment, name):
            log = (private / name).open("w+")
            logs.append(log)
            proc = subprocess.Popen(argv, env=environment, stdout=log, stderr=log, start_new_session=True)
            children.append(proc)
            return proc

        try:
            niri = launch([str(ROOT / os.environ.get("NIRI_RDP_LAB_NIRI", "compositor/upstream/target/debug/niri")),
                           "--config", str(ROOT / "lab/headless.kdl")], env, "niri.log")
            wait_for(runtime / "wayland-0", niri)
            app = launch([str(ROOT / "lab/dynamic-app")], dict(env, WAYLAND_DISPLAY="wayland-0"), "app.log")
            time.sleep(1)
            bridge_env = dict(env, NIRI_RDP_MODE="disposable", NIRI_RDP_CERT=str(private / "cert.pem"),
                              NIRI_RDP_KEY=str(private / "key.pem"), NIRI_RDP_USERNAME=username,
                              NIRI_RDP_PASSWORD=password, NIRI_RDP_WAYLAND_SOCKET=str(runtime / "wayland-0"))
            fixture = None
            if tls_contract:
                fixture = MockBroker(runtime, username, password)
                bridge_env["NIRI_RDP_MODE"] = "broker"
                bridge_env.pop("NIRI_RDP_USERNAME")
                bridge_env.pop("NIRI_RDP_PASSWORD")
                bridge_env.pop("NIRI_RDP_WAYLAND_SOCKET")
            bridge = launch([str(ROOT / os.environ.get("NIRI_RDP_LAB_BRIDGE", "bridge/target/debug/niri-rdp-bridge"))], bridge_env, "bridge.log")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if bridge.poll() is not None:
                    raise RuntimeError("bridge exited at startup")
                try:
                    with socket.create_connection(("127.0.0.1", 33991), timeout=0.1):
                        break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("bridge startup timed out")
            client_env = dict(env, WLOG_LEVEL=os.environ.get("NIRI_RDP_LAB_WLOG", "OFF"), NIRI_RDP_TEST_NLA="1",
                              NIRI_RDP_TEST_USERNAME=username, NIRI_RDP_TEST_PASSWORD=password)
            if tls_contract:
                client_env.pop("NIRI_RDP_TEST_NLA")
            bad = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")],
                                 env=dict(client_env, NIRI_RDP_TEST_PASSWORD=secrets.token_hex(24)),
                                 capture_output=True, text=True, timeout=25)
            # TLS-only validation rejection currently appears as a transport
            # failure in FreeRDP. Require positive broker rejection evidence,
            # rather than accepting any failed connection as an auth test.
            rejected = (fixture.rejected >= 1 and fixture.accepted == 0 and bad.returncode != 0) if fixture else (
                bad.returncode == 4 and "RDP_AUTHENTICATION_REJECTED" in bad.stdout)
            if not rejected:
                if fixture:
                    print(f"test broker counters: rejected={fixture.rejected}, accepted={fixture.accepted}, closed={fixture.closed}")
                raise RuntimeError(f"wrong-password rejection failed: rc={bad.returncode} {bad.stderr}")
            results["wrong_disposable_credentials"] = "passed"
            for i in range(2):
                good = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")], env=client_env,
                                      capture_output=True, text=True, timeout=25)
                if good.returncode or "NIRI_RDP_APP_KEY_POINTER_RELEASE_OK" not in good.stdout:
                    raise RuntimeError(f"real desktop RDP roundtrip {i} failed: {good.stderr}")
                results[f"real_desktop_roundtrip_{i+1}"] = "passed"
                time.sleep(0.3)
            log = (private / "app.log").read_text()
            for event in ("KEY 30 1", "KEY 30 0", "BUTTON 272 1", "BUTTON 272 0", "MOTION"):
                if event not in log:
                    raise RuntimeError(f"Wayland app did not acknowledge {event}")
            if "FRAME" not in log:
                raise RuntimeError("animated app did not reach frame callback checkpoint")
            results["wayland_application_events_and_animation"] = "passed"
            results["no_physical_devices_or_inherited_display"] = "passed"
            held = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")],
                                  env=dict(client_env, NIRI_RDP_TEST_HOLD="1"),
                                  capture_output=True, text=True, timeout=25)
            if held.returncode:
                raise RuntimeError(f"held-input connection failed: {held.stderr}")
            time.sleep(0.5)
            # A fresh connection must see the app's original color, which
            # requires the disconnected virtual keyboard and pointer to release.
            released = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")], env=client_env,
                                      capture_output=True, text=True, timeout=25)
            if released.returncode:
                raise RuntimeError(f"disconnect did not restore app input state: {released.stderr}")
            results["held_key_and_button_disconnect_release"] = "passed"
            before = (private / "app.log").read_text().count("BUTTON 272 1")
            crashing = subprocess.Popen([str(ROOT / "artifacts/rdp-desktop-client")],
                                        env=dict(client_env, NIRI_RDP_TEST_CRASH="1"),
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                        start_new_session=True)
            children.append(crashing)
            deadline = time.monotonic() + 10
            while (private / "app.log").read_text().count("BUTTON 272 1") == before:
                if time.monotonic() >= deadline or crashing.poll() is not None:
                    raise RuntimeError("crash test did not establish an active desktop")
                time.sleep(0.05)
            niri.kill()
            niri.wait(timeout=5)
            crashing.communicate(timeout=5)
            if crashing.returncode != 2 or bridge.poll() is not None:
                raise RuntimeError("compositor crash did not close the active RDP connection cleanly")
            results["compositor_crash_closes_active_rdp_frontend_survives"] = "passed"
            if fixture:
                deadline = time.monotonic() + 3
                while fixture.closed != fixture.accepted and time.monotonic() < deadline:
                    time.sleep(0.05)
                if fixture.rejected < 1 or fixture.accepted != 5 or fixture.closed != 5:
                    raise RuntimeError("TLS broker FD/lease lifecycle counts do not match the RDP connections")
                results["tls_clientinfo_broker_fd_binding_and_lease_eof"] = "passed using namespace test broker; no PAM"
            results["pam_logind_boot_windows_monitor_off"] = "pending separate machines"
        except Exception:
            for log in logs:
                log.flush()
                log.seek(0)
                print(f"--- {Path(log.name).name} ---\n{log.read()[-6000:]}")
            raise
        finally:
            for proc in reversed(children):
                stop(proc)
            for log in logs:
                log.close()
        rendered = json.dumps(results, indent=2) + "\n"
        if report := os.environ.get("NIRI_RDP_LAB_REPORT"):
            Path(report).write_text(rendered)
        print(rendered)


if __name__ == "__main__":
    main()
