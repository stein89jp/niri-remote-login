#!/usr/bin/env python3
"""Private compositor proofs for damage idle, wakeups, cursor and DPI mapping."""
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def ipc(path, request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect(str(path))
        connection.sendall(json.dumps(request).encode() + b"\n")
        with connection.makefile("rb") as reply:
            return json.loads(reply.readline())["Ok"]


def stop(process):
    if process and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=3)


def case(name, scale=100, idle=False, one_fps=False, reload=False):
    folder = ROOT / "artifacts/capture-policy" / name
    folder.mkdir(parents=True, exist_ok=True)
    runtime = Path("/tmp") / ("niri-rdp-capture-" + name)
    runtime.mkdir(mode=0o700, exist_ok=True)
    environment = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), WAYLAND_DISPLAY="wayland-0")
    configuration = folder / "headless.kdl"
    configuration.write_text('output "HEADLESS-1" { scale 1; transform "normal"; position x=0 y=0; }\n'
        'input { keyboard { xkb { model "jp106"; layout "jp"; }; }; focus-follows-mouse; }\n'
        'layout { gaps 0; default-column-width { proportion 1.0; }; border { off; }; focus-ring { off; }; }\n'
        'animations { off; }\ngestures { hot-corners { off; }; }\n')
    niri = app = None
    with (folder / "compositor.log").open("w") as niri_log, (folder / "app.log").open("w") as app_log:
        try:
            niri = subprocess.Popen([str(ROOT / "compositor/upstream/niri-plan-build"), "--config", str(configuration)],
                                    env=environment, stdout=niri_log, stderr=subprocess.STDOUT, start_new_session=True)
            deadline = time.monotonic() + 10
            while not (runtime / "wayland-0").exists():
                if niri.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("private compositor startup failed")
                time.sleep(0.02)
            if not idle:
                app = subprocess.Popen([str(ROOT / "lab/dynamic-app")], env=environment,
                                       stdout=app_log, stderr=subprocess.STDOUT, start_new_session=True)
                time.sleep(0.5)
            probe_env = dict(environment, NIRI_RDP_PROBE_SCALE_PERCENT=str(scale), NIRI_RDP_DIAGNOSTICS="1")
            if idle:
                probe_env.update(NIRI_RDP_PROBE_IDLE_SECONDS="6", NIRI_RDP_PROBE_CURSOR="1")
            elif one_fps:
                probe_env.update(NIRI_RDP_FPS="1", NIRI_RDP_PROBE_INPUT_LOG=str(folder / "app.log"))
            else:
                probe_env["NIRI_RDP_PROBE_CURSOR"] = "1"
            if reload:
                probe_env["NIRI_RDP_PROBE_RELOAD_CONFIG"] = str(configuration)
            probe = subprocess.run([str(ROOT / "bridge/target/debug/capture-probe"), str(runtime / "wayland-0")],
                                   env=probe_env, capture_output=True, text=True, timeout=20)
            (folder / "probe.log").write_text(probe.stdout + probe.stderr)
            if probe.returncode:
                raise RuntimeError(f"{name}: capture probe failed: {probe.stderr}")
            details = [json.loads(line) for line in probe.stdout.splitlines() if line.startswith("{")]
            output = ipc(next(runtime.glob("niri*.sock")), "Outputs")["Outputs"]["HEADLESS-1"]
            mode = output["modes"][output["current_mode"]]
            logical = output["logical"]
            if (mode["width"], mode["height"]) != (640, 480) or abs(logical["scale"] - scale / 100) > 0.001:
                raise RuntimeError(f"{name}: incorrect physical/logical output geometry: {output}")
            click = None
            if app:
                text = (folder / "app.log").read_text()
                for expected in ["KEY 30 1", "KEY 30 0", "KEY 97 1", "KEY 97 0", "BUTTON 272 1", "BUTTON 272 0"]:
                    if expected not in text:
                        raise RuntimeError(f"{name}: app did not observe {expected}")
                point = None
                for line in text.splitlines():
                    match = re.fullmatch(r"(?:MOTION|POINTER_ENTER) ([0-9.]+) ([0-9.]+)", line)
                    if match:
                        point = tuple(map(float, match.groups()))
                    if line == "BUTTON 272 1":
                        click = point
                        break
                window = next(window for window in ipc(next(runtime.glob("niri*.sock")), "Windows")["Windows"]
                              if window["app_id"] == "niri-rdp-proof")
                layout = window["layout"]
                # This fixture explicitly disables gaps, decorations and animations;
                # the single full-width app starts at output logical origin (0,0).
                (folder / "window.json").write_text(json.dumps(window, indent=2) + "\n")
                expected = (320 / (scale / 100), 240 / (scale / 100))
                if click is None or any(abs(a-b) > 1.1 for a, b in zip(click, expected)):
                    raise RuntimeError(f"{name}: click {click} does not map to logical point {expected}")
                if reload:
                    relative_click = None
                    for line in text.splitlines():
                        match = re.fullmatch(r"(?:MOTION|POINTER_ENTER) ([0-9.]+) ([0-9.]+)", line)
                        if match:
                            point = tuple(map(float, match.groups()))
                        if line == "BUTTON 273 1":
                            relative_click = point
                            break
                    expected_relative = (350 / (scale / 100), 240 / (scale / 100))
                    if relative_click is None or any(abs(a-b) > 1.1 for a, b in zip(relative_click, expected_relative)):
                        raise RuntimeError(f"{name}: relative click {relative_click} does not map to {expected_relative}")
            return {"case": name, "physical": [640, 480], "logical": logical,
                    "application_center_click": click, "probes": details}
        finally:
            stop(app)
            stop(niri)


def main():
    if ROOT != Path("/work") or Path("/dev/input").exists() or os.environ.get("DISPLAY"):
        raise RuntimeError("run only inside lab/run-isolated.sh")
    results = [case("input-one-fps", one_fps=True), case("idle-six-seconds", idle=True)]
    results += [case(f"dpi-{scale}", scale=scale) for scale in (100, 150, 200)]
    results += [case("dpi-reload-150", scale=150, reload=True)]
    destination = ROOT / "artifacts/capture-policy/report.json"
    destination.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps({"capture_policy": "PASS", "cases": len(results), "report": str(destination)}))


if __name__ == "__main__":
    main()
