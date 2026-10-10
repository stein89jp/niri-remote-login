"""Actual private PipeWire playback and monitor capture; no host service changes."""
import math
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(all(shutil.which(x) for x in
    ("pipewire", "pipewire-pulse", "wireplumber", "pactl", "parec", "pacat", "dbus-run-session")),
    "private PipeWire integration dependencies unavailable")
class AudioIntegrationTests(unittest.TestCase):
    def test_private_sink_monitor_contains_playback_and_survives_reconnect(self):
        with tempfile.TemporaryDirectory(prefix="niri-audio-") as directory:
            environment = dict(os.environ, NIRI_RDP_RUNTIME_DIR=directory,
                               XDG_RUNTIME_DIR=directory,
                               PULSE_SERVER="unix:" + directory + "/pulse/native")
            with tempfile.TemporaryFile() as log:
                server = subprocess.Popen(["dbus-run-session", "--", "python3",
                    str(ROOT / "broker/config/session-audio")], env=environment,
                    stdout=log, stderr=log, start_new_session=True)
                try:
                    for _ in range(150):
                        if Path(directory, "audio-0").exists():
                            break
                        if server.poll() is not None:
                            log.seek(0)
                            self.fail(log.read().decode())
                        time.sleep(0.1)
                    else:
                        log.seek(0)
                        self.fail("Audio startup timed out: " + log.read().decode())
                    sinks = subprocess.check_output(["pactl", "list", "short", "sinks"], env=environment)
                    self.assertEqual(len(sinks.splitlines()), 1)
                    self.assertIn(b"niri-rdp-sink", sinks)
                    samples = b"".join(struct.pack("<hh", *([int(10000 * math.sin(
                        2 * math.pi * 440 * n / 48000))] * 2)) for n in range(48000))
                    for _ in range(2):
                        with socket.socket(socket.AF_UNIX) as connection:
                            connection.settimeout(3)
                            connection.connect(directory + "/audio-0")
                            connection.settimeout(0.1)
                            with self.assertRaises(TimeoutError):
                                connection.recv(1)
                            connection.settimeout(3)
                            connection.sendall(b"A")
                            player = subprocess.Popen(["pacat", "--playback", "--raw",
                                "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=20"],
                                env=environment, stdin=subprocess.PIPE)
                            writer = threading.Thread(target=lambda: player.communicate(samples))
                            writer.start()
                            try:
                                data = bytearray()
                                while len(data) < 192000:
                                    packet = connection.recv(3840)
                                    self.assertTrue(packet, "monitor closed during playback")
                                    data.extend(packet)
                                values = struct.unpack("<" + "h" * (len(data) // 2), data[:len(data)//2*2])
                                self.assertGreater(max(map(abs, values)), 1000)
                            finally:
                                player.terminate()
                                writer.join(timeout=3)
                                player.wait(timeout=3)
                finally:
                    if server.poll() is None:
                        os.killpg(server.pid, signal.SIGTERM)
                    server.wait(timeout=5)
