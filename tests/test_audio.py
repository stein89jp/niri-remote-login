"""Actual private PipeWire playback and monitor capture; no host service changes."""
import importlib.machinery
import importlib.util
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


class CaptureBackpressureTests(unittest.TestCase):
    def test_stalled_frontend_keeps_recent_aligned_sound_and_capture_drain(self):
        loader = importlib.machinery.SourceFileLoader("session_audio", str(ROOT / "broker/config/session-audio"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        module = importlib.util.module_from_spec(spec)
        loader.exec_module(module)
        source_fd, writer_fd = os.pipe()
        source = os.fdopen(source_fd, "rb", buffering=0)
        client, capture = socket.socketpair()
        errors = []
        def run():
            try:
                module.relay_capture(capture, source)
            except OSError:
                # Closing the client may race the final socket write.
                pass
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=run)
        worker.start()
        try:
            # No frontend reads while capture advances by two seconds. The
            # source writer must finish, proving socket stalls do not back up
            # parec's stdout and then its monitor stream.
            def produce():
                for number in range(100):
                    block = bytes([number]) * module.BLOCK_BYTES
                    # Deliberately fragment one stereo frame across writes.
                    os.write(writer_fd, block[:7])
                    os.write(writer_fd, block[7:])
                    time.sleep(0.001)
            writer = threading.Thread(target=produce, daemon=True)
            writer.start()
            writer.join(timeout=2)
            self.assertFalse(writer.is_alive(), "capture must drain while client stalls")
            client.settimeout(2)
            received = bytearray()
            while len(received) < module.BLOCK_BYTES or received[-module.BLOCK_BYTES:] != bytes([99]) * module.BLOCK_BYTES:
                data = client.recv(module.BLOCK_BYTES)
                self.assertTrue(data)
                received.extend(data)
            self.assertLess(len(received), module.BLOCK_BYTES * 16, "buffered capture must stay below 320 ms")
            self.assertEqual(len(received) % module.BLOCK_BYTES, 0)
            for offset in range(0, len(received), module.BLOCK_BYTES):
                block = received[offset:offset + module.BLOCK_BYTES]
                self.assertEqual(block, bytes([block[0]]) * module.BLOCK_BYTES,
                                 "partial sends and reads must retain sample alignment")
        finally:
            client.close()
            worker.join(timeout=2)
            os.close(writer_fd)
            capture.close()
            source.close()
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])


    def test_regular_pipewire_quantum_batching_preserves_every_sample(self):
        loader = importlib.machinery.SourceFileLoader("session_audio_quantum", str(ROOT / "broker/config/session-audio"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        module = importlib.util.module_from_spec(spec)
        loader.exec_module(module)
        source_fd, writer_fd = os.pipe()
        source = os.fdopen(source_fd, "rb", buffering=0)
        client, capture = socket.socketpair()
        errors = []
        def relay():
            try:
                module.relay_capture(capture, source)
            except OSError:
                pass
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=relay)
        worker.start()
        # 1024 stereo samples per21.33ms PipeWire quantum periodically
        # complete TWO3840-byte waves. Neither may be discarded when healthy.
        quantum = 4096
        samples = b"".join(struct.pack("<I", number) for number in range(quantum * 32 // 4))
        def produce():
            started = time.monotonic()
            for position in range(0, len(samples), quantum):
                deadline = started + position / 192000
                time.sleep(max(0, deadline - time.monotonic()))
                os.write(writer_fd, samples[position:position + quantum])
        writer = threading.Thread(target=produce, daemon=True)
        writer.start()
        try:
            client.settimeout(2)
            expected = samples[:len(samples) // module.BLOCK_BYTES * module.BLOCK_BYTES]
            received = bytearray()
            while len(received) < len(expected):
                data = client.recv(module.BLOCK_BYTES)
                self.assertTrue(data)
                received.extend(data)
            self.assertEqual(received, expected, "healthy quantum batching must preserve every PCM sample")
        finally:
            writer.join(timeout=2)
            client.close()
            worker.join(timeout=2)
            os.close(writer_fd)
            capture.close()
            source.close()
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])


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
