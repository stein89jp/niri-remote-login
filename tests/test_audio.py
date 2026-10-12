"""Actual private PipeWire playback and monitor capture; no host service changes."""
import importlib.machinery
import importlib.util
import math
import os
from pathlib import Path
import shutil
import shlex
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]


class AudioCodecTests(unittest.TestCase):
    def test_ms_adpcm_roundtrip_and_independent_dropped_blocks(self):
        if not shutil.which("cc") or not shutil.which("pkg-config"):
            self.skipTest("C compiler and FFmpeg development libraries required")
        flags = subprocess.run(
            ["pkg-config", "--cflags", "--libs", "libavcodec", "libavutil"],
            capture_output=True, text=True,
        )
        if flags.returncode:
            self.skipTest("FFmpeg development libraries required")
        with tempfile.TemporaryDirectory(prefix="niri-audio-codec-") as directory:
            executable = str(Path(directory) / "audio-codec")
            subprocess.run(
                ["cc", "-std=c17", "-O2", "-Wall", "-Wextra", "-Werror",
                 str(ROOT / "tests/audio_codec.c"), "-o", executable,
                 *shlex.split(flags.stdout), "-lm"],
                check=True, capture_output=True, text=True,
            )
            result = subprocess.run([executable], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("PASS:", result.stdout)


class AudioStartupTests(unittest.TestCase):
    def setUp(self):
        loader = importlib.machinery.SourceFileLoader("session_audio_startup", str(ROOT / "broker/config/session-audio"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        self.module = importlib.util.module_from_spec(spec)
        loader.exec_module(self.module)
        self.processes = [Mock(), Mock(), Mock()]
        for process in self.processes:
            process.poll.return_value = None

    @staticmethod
    def result(stdout=b"", stderr=b"", status=0):
        return subprocess.CompletedProcess([], status, stdout, stderr)

    def test_sink_visible_before_default_metadata_is_retried(self):
        sinks = self.result(b"42\tniri-rdp-sink\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n")
        responses = [sinks, self.result(stderr=b"Failure: Not supported\n", status=1),
                     sinks, self.result(), self.result(b"niri-rdp-sink\n")]
        with patch.object(self.module.subprocess, "run", side_effect=responses) as run:
            self.module.wait_for_sink(self.processes, {"PULSE_SERVER": "unix:/private/native"})
        selections = [call for call in run.call_args_list if call.args[0][1] == "set-default-sink"]
        self.assertEqual(len(selections), 2, "sink visibility must not make a transient metadata failure fatal")
        self.assertEqual(run.call_args.args[0][1:], ["get-default-sink"])

    def test_readiness_requires_observed_default_sink(self):
        sinks = self.result(b"42\tniri-rdp-sink\tPipeWire\n")
        responses = [sinks, self.result(), self.result(b"auto_null\n"),
                     sinks, self.result(), self.result(b"niri-rdp-sink\n")]
        with patch.object(self.module.subprocess, "run", side_effect=responses) as run:
            self.module.wait_for_sink(self.processes, {})
        observed = [call for call in run.call_args_list if call.args[0][1] == "get-default-sink"]
        self.assertEqual(len(observed), 2, "wait until policy has applied the selected default")

    def test_persistent_default_failure_respects_deadline(self):
        elapsed = [0.0]
        def sleep(duration):
            elapsed[0] += duration
        def query(command, **kwargs):
            self.assertGreater(kwargs["timeout"], 0)
            self.assertLessEqual(kwargs["timeout"], 0.1 - elapsed[0])
            if command[1] == "list":
                return self.result(b"42\tniri-rdp-sink\tPipeWire\n")
            return self.result(stderr=b"Failure: Not supported\n", status=1)
        with patch.object(self.module.time, "monotonic", side_effect=lambda: elapsed[0]), \
                patch.object(self.module.time, "sleep", side_effect=sleep), \
                patch.object(self.module.subprocess, "run", side_effect=query):
            with self.assertRaisesRegex(RuntimeError, "private audio sink unavailable: Failure: Not supported"):
                self.module.wait_for_sink(self.processes, {}, timeout=0.1)
        self.assertEqual(elapsed[0], 0.1)

    def test_daemon_exit_while_waiting_fails_promptly(self):
        self.processes[2].poll.side_effect = [None, 1]
        with patch.object(self.module.subprocess, "run", return_value=self.result(status=1)) as run:
            with self.assertRaisesRegex(RuntimeError, "private audio daemon exited"):
                self.module.wait_for_sink(self.processes, {})
        self.assertEqual(run.call_count, 1)

    def test_readiness_failure_cleans_up_every_audio_daemon(self):
        with tempfile.TemporaryDirectory(prefix="niri-audio-startup-") as directory:
            Path(directory, "pipewire-0").touch()
            with patch.dict(self.module.os.environ, {"NIRI_RDP_RUNTIME_DIR": directory}), \
                    patch.object(self.module.signal, "signal"), \
                    patch.object(self.module.Path, "read_text", return_value="context.objects = [\n]\nwireplumber.profiles = {\n}\n"), \
                    patch.object(self.module.subprocess, "Popen", side_effect=self.processes), \
                    patch.object(self.module, "wait_for_sink", side_effect=RuntimeError("private audio sink unavailable")):
                with self.assertRaisesRegex(RuntimeError, "private audio sink unavailable"):
                    self.module.main()
            self.assertFalse(Path(directory, "audio-0").exists())
        for process in self.processes:
            process.terminate.assert_called_once()
            process.wait.assert_called_once_with(timeout=2)


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
            # Delay policy startup to exercise the real interval where the
            # sink is visible but set-default-sink returns "Not supported".
            launcher = """
import runpy
import subprocess
import sys
spawn = subprocess.Popen
def delayed_policy(command, *args, **kwargs):
    if command == ["/usr/bin/wireplumber", "--profile=remote"]:
        command = ["/usr/bin/sh", "-c", "sleep 0.5; exec /usr/bin/wireplumber --profile=remote"]
    return spawn(command, *args, **kwargs)
subprocess.Popen = delayed_policy
runpy.run_path(sys.argv[1], run_name="__main__")
"""
            with tempfile.TemporaryFile() as log:
                server = subprocess.Popen(["dbus-run-session", "--", "python3", "-c", launcher,
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
                    default = subprocess.check_output(["pactl", "get-default-sink"], env=environment)
                    self.assertEqual(default.strip(), b"niri-rdp-sink")
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
