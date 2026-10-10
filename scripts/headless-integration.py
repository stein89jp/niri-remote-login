#!/usr/bin/env python3
"""Real Niri/app/RDP test; run inside lab/run-isolated.sh, never a host desktop."""
import json
import os
import re
from pathlib import Path
import secrets
import selectors
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from mock_broker import MockBroker

ROOT = Path(__file__).resolve().parents[1]
DESKTOP_SIZES = [(1001, 701), (702, 1001), (1002, 701), (702, 1001)]


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


def native_output_size(ipc_path):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect(str(ipc_path))
        connection.sendall(b'"Outputs"\n')
        with connection.makefile("rb") as reply:
            outputs = json.loads(reply.readline())["Ok"]["Outputs"]
    output = outputs["HEADLESS-1"]
    mode = output["modes"][output["current_mode"]]
    logical = output["logical"]
    return (mode["width"], mode["height"]), (logical["width"], logical["height"])


def resize_roundtrip(environment, ipc_path, log_path):
    """Hold each decoded size until IPC confirms the native compositor mode."""
    decoded_sizes = []
    output = []
    with log_path.open("w+") as error_log:
        process = subprocess.Popen([str(ROOT / "artifacts/rdp-desktop-client")],
                                   env=dict(environment, NIRI_RDP_TEST_RESIZE_ACK="1"),
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=error_log, text=True, start_new_session=True)
        try:
            deadline = time.monotonic() + 25
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while True:
                    if time.monotonic() >= deadline:
                        raise RuntimeError("dynamic resolution client timed out")
                    if not selector.select(timeout=0.1):
                        continue
                    line = process.stdout.readline()
                    if not line:
                        break
                    output.append(line)
                    if line.startswith("NIRI_RDP_DESKTOP_SIZE "):
                        size = tuple(map(int, line.split()[1:]))
                        expected = DESKTOP_SIZES[len(decoded_sizes)]
                        if size != expected:
                            raise RuntimeError(f"decoded desktop size {size}, expected {expected}")
                        native, logical = native_output_size(ipc_path)
                        if native != size or logical != size:
                            raise RuntimeError(f"decoded {size} but native output={native}, logical={logical}")
                        decoded_sizes.append(size)
                        process.stdin.write("continue\n")
                        process.stdin.flush()
            result = process.wait(timeout=3)
            error_log.flush()
            error_log.seek(0)
            errors = error_log.read()
            rendered = "".join(output)
            if result or decoded_sizes != DESKTOP_SIZES or "NIRI_RDP_DYNAMIC_RESOLUTION_OK" not in rendered:
                raise RuntimeError(f"dynamic resolution roundtrip failed: rc={result}, sizes={decoded_sizes} "
                                   f"{rendered} {errors}")
            return subprocess.CompletedProcess(process.args, result, rendered, errors)
        finally:
            stop(process)
            process.stdin.close()
            process.stdout.close()
            error_log.flush()
            error_log.seek(0)
            errors = error_log.read()
            log_path.write_text("".join(output) + errors)


def main():
    gpu_rendering = os.environ.get("NIRI_RDP_LAB_NVIDIA_RENDERER") == "1"
    dri = Path("/dev/dri")
    if dri.exists():
        if not gpu_rendering or any(
            entry.is_symlink() or not entry.is_char_device() or
            not (entry.name.startswith("renderD") and entry.name[7:].isdigit())
            for entry in dri.iterdir()
        ):
            raise RuntimeError("DRM access must contain only render nodes in explicit NVIDIA renderer mode")
    if (ROOT != Path("/work") or
            Path("/dev/input").exists() or os.environ.get("DISPLAY")):
        raise RuntimeError("run only inside lab/run-isolated.sh with private devices")
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    encoder = os.environ.get("NIRI_RDP_LAB_ENCODER", "bitmap")
    if encoder not in ("bitmap", "nvenc"):
        raise RuntimeError("NIRI_RDP_LAB_ENCODER must be bitmap or nvenc")
    if encoder == "nvenc" and os.environ.get("NIRI_RDP_LAB_NVENC") != "1":
        raise RuntimeError("NVENC integration requires lab/run-isolated.sh --nvenc or --nvidia-renderer")
    gfx_setting = os.environ.get("NIRI_RDP_LAB_GFX", "1")
    if gfx_setting not in ("0", "1"):
        raise RuntimeError("NIRI_RDP_LAB_GFX must be 0 or 1")
    gfx = encoder == "nvenc" and gfx_setting == "1"
    username, password = "disposable-" + secrets.token_hex(8), secrets.token_hex(24)
    tls_contract = os.environ.get("NIRI_RDP_LAB_TLS_CONTRACT") == "1"
    login_screen = os.environ.get("NIRI_RDP_LAB_LOGIN_SCREEN") == "1"
    if login_screen and not tls_contract:
        raise RuntimeError("NIRI_RDP_LAB_LOGIN_SCREEN requires NIRI_RDP_LAB_TLS_CONTRACT=1")
    # The namespace supplies only these explicit environment values.
    env = {k: os.environ[k] for k in ("PATH", "HOME", "XDG_RUNTIME_DIR", "LD_LIBRARY_PATH",
                                    "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "XKB_CONFIG_ROOT",
                                    "NIRI_RDP_RENDERER")
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
            ipc_path = runtime / f"niri.wayland-0.{niri.pid}.sock"
            wait_for(ipc_path, niri)
            if os.environ.get("NIRI_RDP_LAB_NVIDIA_RENDERER") == "1":
                # Wayland/IPC sockets are created before EGL initialization.
                # Wait for the renderer rather than treating socket creation as
                # evidence that GPU startup has already finished.
                deadline = time.monotonic() + 15
                while True:
                    renderer_log = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", (private / "niri.log").read_text())
                    if all(marker in renderer_log for marker in (
                        "headless renderer: hardware EGL device", "nvidia=true",
                        "headless renderer: advertised hardware DMA-BUF formats",
                    )):
                        break
                    if niri.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("requested NVIDIA renderer and DMA-BUF support were not confirmed")
                    time.sleep(0.05)
                results["nvidia_renderer"] = "hardware EGL device and application DMA-BUF formats confirmed"
            app = launch([str(ROOT / "lab/dynamic-app")], dict(env, WAYLAND_DISPLAY="wayland-0"), "app.log")
            time.sleep(1)
            bridge_env = dict(env, NIRI_RDP_MODE="disposable", NIRI_RDP_CERT=str(private / "cert.pem"),
                              NIRI_RDP_KEY=str(private / "key.pem"), NIRI_RDP_USERNAME=username,
                              NIRI_RDP_PASSWORD=password, NIRI_RDP_WAYLAND_SOCKET=str(runtime / "wayland-0"))
            bridge_env["NIRI_RDP_ENCODER"] = encoder
            if fps := os.environ.get("NIRI_RDP_LAB_FPS"):
                bridge_env["NIRI_RDP_FPS"] = fps
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
            if gfx:
                client_env["NIRI_RDP_TEST_GFX"] = "1"
            bad = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")],
                                 env=dict(client_env, NIRI_RDP_TEST_PASSWORD=secrets.token_hex(24)),
                                 capture_output=True, text=True, timeout=25)
            (private / "client-rejected.log").write_text(bad.stdout + bad.stderr)
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
            if login_screen:
                client_env["NIRI_RDP_TEST_LOGIN_SCREEN"] = "1"
            for i in range(2):
                good = resize_roundtrip(client_env, ipc_path, private / f"client-{i+1}.log")
                if good.returncode or "NIRI_RDP_APP_KEY_POINTER_RELEASE_OK" not in good.stdout:
                    raise RuntimeError(f"real desktop RDP roundtrip {i} failed: rc={good.returncode} "
                                       f"{good.stdout} {good.stderr}")
                if gfx and "NIRI_RDP_AVC420_DECODED_FRAMES" not in good.stdout:
                    raise RuntimeError("NVENC roundtrip used bitmap fallback instead of decoded AVC420")
                if login_screen:
                    for marker in ("NIRI_RDP_LOGIN_SCREEN_WITHOUT_CLIENT_CREDENTIALS",
                                   "NIRI_RDP_LOGIN_SCREEN_WRONG_PASSWORD_REJECTED",
                                   "NIRI_RDP_LOGIN_SCREEN_PAM_BINDING_OK"):
                        if marker not in good.stdout:
                            raise RuntimeError(f"login screen roundtrip missing {marker}")
                results[f"real_desktop_roundtrip_{i+1}"] = "passed"
                time.sleep(0.3)
            results["client_initial_resolution_native_output_and_decoded_frame"] = "passed"
            results["live_disp_portrait_landscape_odd_height_native_output_decode_input"] = "passed"
            log = (private / "app.log").read_text()
            for event in ("KEY 30 1", "KEY 30 0", "BUTTON 272 1", "BUTTON 272 0", "MOTION"):
                if event not in log:
                    raise RuntimeError(f"Wayland app did not acknowledge {event}")
            if "FRAME" not in log:
                raise RuntimeError("animated app did not reach frame callback checkpoint")
            results["wayland_application_events_and_animation"] = "passed"
            if gpu_rendering:
                results["no_physical_input_or_drm_modesetting_or_inherited_display"] = "passed; DRM render nodes only"
            else:
                results["no_physical_input_or_dri_devices_or_inherited_display"] = "passed"
            held = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")],
                                  env=dict(client_env, NIRI_RDP_TEST_HOLD="1"),
                                  capture_output=True, text=True, timeout=25)
            (private / "client-held.log").write_text(held.stdout + held.stderr)
            if held.returncode:
                raise RuntimeError(f"held-input connection failed: {held.stderr}")
            time.sleep(0.5)
            # A fresh connection must see the app's original color, which
            # requires the disconnected virtual keyboard and pointer to release.
            released = subprocess.run([str(ROOT / "artifacts/rdp-desktop-client")], env=client_env,
                                      capture_output=True, text=True, timeout=25)
            (private / "client-released.log").write_text(released.stdout + released.stderr)
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
            crash_output, crash_error = crashing.communicate(timeout=5)
            (private / "client-crash.log").write_text(crash_output + crash_error)
            if crashing.returncode != 2 or bridge.poll() is not None:
                raise RuntimeError("compositor crash did not close the active RDP connection cleanly")
            results["compositor_crash_closes_active_rdp_frontend_survives"] = "passed"
            if encoder == "nvenc":
                bridge_log = (private / "bridge.log").read_text()
                if "NVENC H.264 preflight passed" not in bridge_log:
                    raise RuntimeError("requested NVENC encoder did not pass hardware preflight")
                # Three normal clients recreate graphics at every live layout;
                # held-input and crash probes retain their initial desktop.
                expected_graphics = 3 * len(DESKTOP_SIZES) + 2 if gfx else 0
                actual_graphics = bridge_log.count("RDP graphics using NVENC H.264 AVC420")
                if actual_graphics != expected_graphics:
                    raise RuntimeError(f"expected {expected_graphics} NVENC AVC420 initializations, "
                                       f"observed {actual_graphics}")
                key = ("nvenc_avc420_graphics_real_app_decode_input_reconnect" if gfx else
                       "nvenc_bitmap_fallback_for_client_without_graphics")
                results[key] = "passed"
            if fixture:
                deadline = time.monotonic() + 3
                while fixture.closed != fixture.accepted and time.monotonic() < deadline:
                    time.sleep(0.05)
                expected_rejections = 6 if login_screen else 1
                if fixture.rejected < expected_rejections or fixture.accepted != 5 or fixture.closed != 5:
                    raise RuntimeError("TLS broker FD/lease lifecycle counts do not match the RDP connections: "
                                       f"rejected={fixture.rejected}, accepted={fixture.accepted}, closed={fixture.closed}")
                key = ("tls_login_screen_rejection_authentication_desktop_and_lease_eof" if login_screen else
                       "tls_clientinfo_broker_fd_binding_and_lease_eof")
                results[key] = "passed using namespace test broker; no PAM"
            results["pam_logind_boot_windows_monitor_off"] = "pending separate machines"
        except Exception:
            for log in logs:
                log.flush()
                log.seek(0)
                print(f"--- {Path(log.name).name} ---\n{log.read()[-6000:]}")
            failure = ROOT / "artifacts/integration-failures" / secrets.token_hex(6)
            failure.mkdir(parents=True)
            for path in private.glob("*.log"):
                shutil.copy2(path, failure / path.name)
            print(f"Complete failure logs preserved in {failure}")
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
