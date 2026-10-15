#!/usr/bin/python3
"""Known Unicode text, HTML and images through real CLIPRDR and two isolated
Wayland desktops.

Run with lab/run-isolated.sh. No host display, clipboard or bus is consulted.
"""
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import subprocess
import struct
import tempfile
import time
import zlib

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


# Rich-mode fixtures, mirrored in scripts/rdp-clipboard-client.c.
NATIVE_RGBA = [(255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 128),
               (10, 20, 30, 255), (40, 50, 60, 0), (200, 100, 50, 255)]
NATIVE_FRAGMENT = "<p>Wayland <i>rich</i> 日本</p>"
REMOTE_DOCUMENT = "<html><body>\r\n<!--StartFragment--><b>RDP rich</b><!--EndFragment-->\r\n</body>\r\n</html>"
REMOTE_RGB = [(1, 2, 3), (4, 5, 6), (7, 8, 9), (250, 251, 252), (128, 64, 32), (0, 0, 0)]


def png_chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def encode_png(width, height, pixels):
    rows = b"".join(b"\0" + bytes(c for p in pixels[y * width:(y + 1) * width] for c in p) for y in range(height))
    return (b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + png_chunk(b"IDAT", zlib.compress(rows)) + png_chunk(b"IEND", b""))


def decode_png(data):
    """8-bit RGB/RGBA, non-interlaced: what the bridge writes."""
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise RuntimeError("pasted image is not PNG")
    position, compressed, header = 8, b"", None
    while position < len(data):
        length, kind = struct.unpack(">I4s", data[position:position + 8])
        body = data[position + 8:position + 8 + length]
        position += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            compressed += body
    width, height, depth, color, _, _, interlace = header
    if depth != 8 or color not in (2, 6) or interlace:
        raise RuntimeError("unexpected PNG layout")
    channels = 3 if color == 2 else 4
    raw, stride, previous, pixels = zlib.decompress(compressed), width * channels, bytes(width * channels), []
    for y in range(height):
        kind, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            left = line[i - channels] if i >= channels else 0
            up, corner = previous[i], previous[i - channels] if i >= channels else 0
            p = left + up - corner
            paeth = min((abs(p - left), 0, left), (abs(p - up), 1, up), (abs(p - corner), 2, corner))[2]
            line[i] = (line[i] + (0, left, up, (left + up) // 2, paeth)[kind]) & 0xff
        previous = bytes(line)
        pixels += [tuple(line[x * channels:(x + 1) * channels]) for x in range(width)]
    return width, height, pixels


def copy(environment, text, mime="text/plain;charset=utf-8"):
    process = subprocess.Popen(["wl-copy", "--foreground", "--type", mime],
                               env=environment, stdin=subprocess.PIPE,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    process.stdin.write(text if isinstance(text, bytes) else text.encode())
    process.stdin.close()
    return process


def paste(environment, mime="text/plain;charset=utf-8"):
    return subprocess.run(["wl-paste", "--no-newline", "--type", mime],
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
            for cycle, mode in enumerate(("text", "empty", "text", "rich")):
                empty, rich = mode == "empty", mode == "rich"
                client_environment = dict(base, WLOG_LEVEL="OFF", NIRI_RDP_TEST_NLA="1",
                                          NIRI_RDP_TEST_USERNAME=username, NIRI_RDP_TEST_PASSWORD=password)
                if empty:
                    client_environment["NIRI_RDP_TEST_CLIPBOARD_EMPTY"] = "1"
                if rich:
                    client_environment["NIRI_RDP_TEST_CLIPBOARD_RICH"] = "1"
                client = subprocess.Popen([str(ROOT / os.environ.get(
                    "NIRI_RDP_LAB_CLIPBOARD_CLIENT", "artifacts/rdp-clipboard-client"))], env=client_environment,
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
                if rich:
                    # Native PNG -> CF_DIB (plus a "PNG" offer), then native HTML -> CF_HTML.
                    processes.append(copy(environments[0], encode_png(3, 2, NATIVE_RGBA), "image/png"))
                    marker(client, "NIRI_RDP_CLIPBOARD_IMAGE_OK")
                    processes.append(copy(environments[0], NATIVE_FRAGMENT, "text/html"))
                else:
                    processes.append(copy(environments[0], "" if empty else "Wayland 日本😀\nline 2"))
                marker(client, "NIRI_RDP_CLIPBOARD_NATIVE_TO_RDP_OK")
                marker(client, "NIRI_RDP_CLIPBOARD_RDP_DATA_SENT")
                deadline = time.monotonic() + 5
                expected = b"" if empty else "RDP 日本😀\nline 2".encode()
                while True:
                    types = subprocess.run(["wl-paste", "--list-types"], env=environments[0], capture_output=True, timeout=3)
                    own = b"application/x-niri-rdp-private-text" in types.stdout
                    if rich:
                        # CF_HTML and a 24-bit bottom-up CF_DIB -> text/html and image/png.
                        listed = types.stdout.split()
                        if own and b"image/png" in listed and b"text/html" in listed:
                            image = paste(environments[0], "image/png")
                            html = paste(environments[0], "text/html")
                            if decode_png(image.stdout) != (3, 2, REMOTE_RGB):
                                raise RuntimeError("RDP image did not convert to the expected PNG")
                            if html.stdout.decode() != '<meta charset="utf-8">' + REMOTE_DOCUMENT:
                                raise RuntimeError("RDP HTML did not convert to the expected text/html")
                            break
                    else:
                        result = paste(environments[0])
                        if result.returncode == 0 and result.stdout == expected and own:
                            break
                    if time.monotonic() > deadline:
                        raise RuntimeError("RDP content did not become the native selection")
                    time.sleep(0.05)
                if paste(environments[1]).stdout != b"second private desktop":
                    raise RuntimeError("clipboard escaped its authorized compositor")
                client.stdin.write("stop\n")
                client.stdin.flush()
                if client.wait(timeout=5) != 0:
                    raise RuntimeError("clipboard client failed: " + client.stderr.read())
                if any(desktop.poll() is not None for desktop in desktops):
                    raise RuntimeError("clipboard reconnect replaced or ended a desktop")
                receipts.append({"empty": empty, "unicode_and_multiline": mode == "text",
                                 "images_and_html": rich,
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
