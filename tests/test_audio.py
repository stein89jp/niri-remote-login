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


CAPTURE_HEADER = struct.Struct("!4sIQQQ")


def read_exact(connection, length):
    data = bytearray()
    while len(data) < length:
        piece = connection.recv(length - len(data))
        if not piece:
            raise EOFError("private capture ended mid-frame")
        data.extend(piece)
    return bytes(data)


def receive_capture(connection):
    magic, length, sequence, source_at, relay_at = CAPTURE_HEADER.unpack(read_exact(connection, CAPTURE_HEADER.size))
    if magic != b"NRA1" or length != 3840 or source_at > relay_at or relay_at > time.monotonic_ns():
        raise ValueError("invalid private PCM framing")
    return read_exact(connection, length), sequence, source_at, relay_at


class AudioCodecTests(unittest.TestCase):
    def test_audio_codecs_roundtrip_and_dropped_blocks(self):
        if not shutil.which("cc") or not shutil.which("pkg-config"):
            self.skipTest("C compiler and FFmpeg development libraries required")
        flags = subprocess.run(
            ["pkg-config", "--cflags", "--libs", "libavcodec", "libavutil", "libswresample"],
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
                    self.module.run_graph(Path(directory), {})
            self.assertFalse(Path(directory, "audio-0").exists())
        for process in self.processes:
            process.terminate.assert_called_once()
            process.wait.assert_called_once_with(timeout=2)


    def test_graph_restart_attempts_are_finite_and_back_off(self):
        with patch.object(self.module, "run_graph", side_effect=RuntimeError("failed")) as run, \
                patch.object(self.module.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "failed"):
                self.module.supervise_graph(Path("/private"), {})
        self.assertEqual(run.call_count, 3)
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [0.25, 0.5])

    def supervise_with_fake_clock(self, run_seconds):
        clock = [1000.0]
        durations = iter(run_seconds)

        def run(*_):
            clock[0] += next(durations)
            raise RuntimeError("failed")
        with patch.object(self.module, "run_graph", side_effect=run) as run_graph, \
                patch.object(self.module.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(self.module.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "failed"):
                self.module.supervise_graph(Path("/private"), {})
        return run_graph.call_count, [call.args[0] for call in sleep.call_args_list]

    def test_graph_attempts_reset_after_ten_healthy_minutes_and_stay_finite(self):
        # A crash after ten healthy minutes renews the budget; the next rapid
        # failures exhaust it again rather than retrying forever.
        self.assertEqual(self.supervise_with_fake_clock([1, 600, 1, 1]), (4, [0.25, 0.25, 0.5]))

    def test_graph_attempts_do_not_reset_before_ten_healthy_minutes(self):
        self.assertEqual(self.supervise_with_fake_clock([1, 599.9, 1]), (3, [0.25, 0.5]))

    def test_capture_rejects_older_unframed_readiness_without_starting_monitor(self):
        client, capture = socket.socketpair()
        client.sendall(b"A")
        client.shutdown(socket.SHUT_WR)
        with patch.object(self.module.subprocess, "Popen") as spawn:
            self.module.capture(capture, {})
        spawn.assert_not_called()
        client.close()


class SessionWorkerRecoveryTests(unittest.TestCase):
    def run_worker(self, directory, healthy_run_attempt=None, desktop_seconds=4.5, replacements=()):
        root = Path(directory)
        binaries = root / "bin"
        binaries.mkdir()
        worker = (ROOT / "broker/config/session-worker").read_text().replace(
            "/usr/lib/niri-rdp", str(binaries)).replace(
            "/etc/niri-rdp/renderer.conf", str(root / "renderer.conf"))
        for old, new in replacements:
            self.assertIn(old, worker)
            worker = worker.replace(old, new)
        (root / "worker").write_text(worker)
        # This private helper fixture fails before readiness, except that the
        # selected attempt first runs for three seconds.
        (binaries / "session-audio").write_text("#!/usr/bin/python3\nimport os,time\nfrom pathlib import Path\n"
            "p=Path(os.environ['NIRI_RDP_RUNTIME_DIR'])/'audio-attempts'\n"
            "with p.open('a') as f: f.write('attempt\\n')\n"
            f"if len(p.read_text().splitlines()) == {healthy_run_attempt!r}: time.sleep(3)\n"
            "raise SystemExit(1)\n")
        (binaries / "niri-headless").write_text("#!/usr/bin/python3\nimport os,time\nfrom pathlib import Path\n"
            "p=Path(os.environ['NIRI_RDP_RUNTIME_DIR'])\n(p/'desktop-started').write_text(str(os.getpid()))\n"
            f"time.sleep({desktop_seconds!r})\n(p/'desktop-finished').touch()\n")
        for executable in binaries.iterdir():
            executable.chmod(0o700)
        environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                           NIRI_RDP_RUNTIME_DIR=directory)
        process = subprocess.Popen(["/usr/bin/sh", str(root / "worker"), "--environment-ready"],
                                   env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            _, errors = process.communicate(timeout=desktop_seconds + 4.5)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        self.assertEqual(process.returncode, 0, errors.decode())
        self.assertTrue((root / "desktop-started").exists())
        self.assertTrue((root / "desktop-finished").exists())
        return (root / "audio-attempts").read_text().splitlines(), errors

    def test_audio_startup_failure_does_not_end_desktop_and_restarts_are_bounded(self):
        with tempfile.TemporaryDirectory(prefix="niri-audio-worker-") as directory:
            # The shell must still run its desktop and exhaust exactly two retries.
            attempts, errors = self.run_worker(directory)
            self.assertEqual(attempts, ["attempt"] * 3)
            self.assertIn(b"starting desktop without audio", errors)
            self.assertIn(b"restart 2 of 2", errors)

    def test_healthy_helper_run_renews_restart_budget_and_rapid_failures_stay_bounded(self):
        with tempfile.TemporaryDirectory(prefix="niri-audio-worker-") as directory:
            # Two rapid failures exhaust the budget; the third helper then runs
            # longer than the (shortened) healthy period, earning two more
            # restarts. Rapid failures afterwards end supervision again, and
            # time spent dead afterwards never renews it.
            attempts, errors = self.run_worker(
                directory, healthy_run_attempt=3, desktop_seconds=8.5,
                replacements=(("audio_healthy_seconds=600", "audio_healthy_seconds=2"),
                              ('sleep "$audio_restarts"', "sleep 0.1")))
            self.assertEqual(attempts, ["attempt"] * 5)
            self.assertEqual(errors.count(b"restart 1 of 2"), 2, errors.decode())
            self.assertEqual(errors.count(b"restart 2 of 2"), 2, errors.decode())


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
                data, _, _, _ = receive_capture(client)
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
                data, _, _, _ = receive_capture(client)
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
            environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                               XDG_CACHE_HOME=directory + "/cache", XDG_DATA_HOME=directory + "/data",
                               GIO_USE_VFS="local", NIRI_RDP_RUNTIME_DIR=directory,
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
                            connection.sendall(b"NRA1")
                            player = subprocess.Popen(["pacat", "--playback", "--raw",
                                "--format=s16le", "--rate=48000", "--channels=2", "--latency-msec=20"],
                                env=environment, stdin=subprocess.PIPE)
                            writer = threading.Thread(target=lambda: player.communicate(samples))
                            writer.start()
                            try:
                                data = bytearray()
                                while len(data) < 192000:
                                    packet, _, source_at, relay_at = receive_capture(connection)
                                    self.assertLess(time.monotonic_ns() - source_at, 200_000_000)
                                    self.assertLessEqual(source_at, relay_at)
                                    self.assertTrue(packet, "monitor closed during playback")
                                    data.extend(packet)
                                values = struct.unpack("<" + "h" * (len(data) // 2), data[:len(data)//2*2])
                                self.assertGreater(max(map(abs, values)), 1000)
                                left, right = values[::2], values[1::2]
                                self.assertEqual(left, right, "identical stereo input must retain channel alignment")
                                # Loud static also passes the amplitude check. Fit
                                # the expected tone in the loudest complete window
                                # to avoid startup silence and playback boundaries.
                                windows = [left[start:start + 4096]
                                           for start in range(0, len(left) - 4095, 4096)]
                                window = max(windows, key=lambda samples: sum(value * value for value in samples))
                                energy = sum(value * value for value in window)
                                cosine = sum(value * math.cos(2 * math.pi * 440 * index / 48000)
                                             for index, value in enumerate(window))
                                sine = sum(value * math.sin(2 * math.pi * 440 * index / 48000)
                                           for index, value in enumerate(window))
                                tone_fraction = 2 * (cosine * cosine + sine * sine) / (len(window) * energy)
                                self.assertGreater(tone_fraction, 0.9, "monitor must contain the played tone, not noise")
                            finally:
                                player.terminate()
                                writer.join(timeout=3)
                                player.wait(timeout=3)
                finally:
                    if server.poll() is None:
                        os.killpg(server.pid, signal.SIGTERM)
                    server.wait(timeout=5)

    @staticmethod
    def private_children(pid):
        try:
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
        except FileNotFoundError:
            return {}
        result = {}
        for child in children:
            try:
                result[int(child)] = Path(f"/proc/{child}/comm").read_text().strip()
            except FileNotFoundError:
                pass
        return result

    def test_private_daemon_loss_recovers_graph_with_fresh_capture(self):
        for failed_name in ("wireplumber", "pipewire-pulse", "pipewire"):
            with self.subTest(daemon=failed_name), tempfile.TemporaryDirectory(prefix="niri-audio-recover-") as directory:
                environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                               XDG_CACHE_HOME=directory + "/cache", XDG_DATA_HOME=directory + "/data",
                               GIO_USE_VFS="local", NIRI_RDP_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory,
                                   PULSE_SERVER="unix:" + directory + "/pulse/native")
                with tempfile.TemporaryFile() as log:
                    server = subprocess.Popen(["dbus-run-session", "--", "python3",
                                               str(ROOT / "broker/config/session-audio")], env=environment,
                                              stdout=log, stderr=log, start_new_session=True)
                    try:
                        deadline = time.monotonic() + 15
                        while not Path(directory, "audio-0").is_socket():
                            self.assertIsNone(server.poll())
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(0.05)
                        helper = next(pid for pid, name in self.private_children(server.pid).items() if name == "python3")
                        old = self.private_children(helper)
                        failed = next(pid for pid, name in old.items() if name == failed_name)
                        with socket.socket(socket.AF_UNIX) as active:
                            active.settimeout(3)
                            active.connect(directory + "/audio-0")
                            active.sendall(b"NRA1")
                            receive_capture(active)
                            os.kill(failed, signal.SIGKILL)
                            # Already framed socket data can finish, but active
                            # capture closes rather than replaying or changing codec.
                            while active.recv(4096):
                                self.assertLess(time.monotonic(), deadline)
                        while True:
                            fresh = self.private_children(helper)
                            if len(fresh) == 3 and set(old).isdisjoint(fresh) and Path(directory, "audio-0").is_socket():
                                break
                            self.assertIsNone(server.poll())
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(0.05)
                        with socket.socket(socket.AF_UNIX) as resumed:
                            resumed.settimeout(3)
                            resumed.connect(directory + "/audio-0")
                            resumed.sendall(b"NRA1")
                            payload, sequence, source_at, _ = receive_capture(resumed)
                            self.assertEqual(sequence, 0, "new descriptor has a fresh capture sequence")
                            self.assertEqual(len(payload), 3840)
                            self.assertLess(time.monotonic_ns() - source_at, 100_000_000)
                        log.seek(0)
                        self.assertIn(b"Restarting private audio graph (attempt 2)", log.read())
                    finally:
                        if server.poll() is None:
                            os.killpg(server.pid, signal.SIGTERM)
                        server.wait(timeout=5)

    def test_monitor_loss_allows_fresh_reconnect_and_helper_death_ends_daemons(self):
        with tempfile.TemporaryDirectory(prefix="niri-audio-helper-") as directory:
            environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                               XDG_CACHE_HOME=directory + "/cache", XDG_DATA_HOME=directory + "/data",
                               GIO_USE_VFS="local", NIRI_RDP_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory,
                               PULSE_SERVER="unix:" + directory + "/pulse/native")
            with tempfile.TemporaryFile() as log:
                server = subprocess.Popen(["dbus-run-session", "--", "python3",
                                           str(ROOT / "broker/config/session-audio")], env=environment,
                                          stdout=log, stderr=log, start_new_session=True)
                try:
                    deadline = time.monotonic() + 15
                    while not Path(directory, "audio-0").is_socket():
                        self.assertIsNone(server.poll())
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.05)
                    helper = next(pid for pid, name in self.private_children(server.pid).items() if name == "python3")
                    daemons = self.private_children(helper)
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(3)
                        connection.connect(directory + "/audio-0")
                        connection.sendall(b"NRA1")
                        receive_capture(connection)
                        monitor = next(pid for pid, name in self.private_children(helper).items() if name == "parec")
                        os.kill(monitor, signal.SIGKILL)
                        while connection.recv(4096):
                            self.assertLess(time.monotonic(), deadline)
                    with socket.socket(socket.AF_UNIX) as resumed:
                        resumed.settimeout(3)
                        resumed.connect(directory + "/audio-0")
                        resumed.sendall(b"NRA1")
                        self.assertEqual(receive_capture(resumed)[1], 0)
                    self.assertEqual(daemons, {pid: name for pid, name in self.private_children(helper).items() if name != "parec"})
                    os.kill(helper, signal.SIGKILL)
                    server.wait(timeout=5)
                    deadline = time.monotonic() + 3
                    while True:
                        live = []
                        for pid in daemons:
                            try:
                                if Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z":
                                    live.append(pid)
                            except (FileNotFoundError, ProcessLookupError):
                                pass
                        if not live:
                            break
                        self.assertLess(time.monotonic(), deadline, "helper death must end its private graph")
                        time.sleep(0.05)
                finally:
                    if server.poll() is None:
                        os.killpg(server.pid, signal.SIGTERM)
                    server.wait(timeout=5)


class AudioRdpInteropTests(unittest.TestCase):
    def test_prepared_freerdp_client_decodes_raw_aac_and_accepts_pcm_fallback(self):
        library = ROOT / "lab/sysroot/usr/lib"
        if not (library / "pkgconfig/freerdp3.pc").is_file() or not (library / "libfreerdp3.so").is_file():
            self.skipTest("prepared lab FreeRDP client development libraries required")
        if not shutil.which("cc") or not shutil.which("pkg-config"):
            self.skipTest("C compiler and pkg-config required")
        environment = dict(os.environ)
        environment["PKG_CONFIG_PATH"] = str(library / "pkgconfig") + (
            os.pathsep + environment["PKG_CONFIG_PATH"] if environment.get("PKG_CONFIG_PATH") else "")
        flags = subprocess.run(
            ["pkg-config", "--cflags", "--libs", "freerdp3", "winpr3",
             "libavcodec", "libavutil", "libswresample"],
            env=environment, capture_output=True, text=True,
        )
        if flags.returncode:
            self.skipTest("prepared lab FreeRDP and FFmpeg development libraries required")
        with tempfile.TemporaryDirectory(prefix="niri-audio-rdpsnd-") as directory:
            executable = str(Path(directory) / "audio-rdpsnd")
            compiled = subprocess.run(
                ["cc", "-std=c17", "-O2", "-Wall", "-Wextra", "-Werror",
                 "-Wno-deprecated-declarations", str(ROOT / "tests/audio_rdpsnd.c"),
                 str(ROOT / "bridge/src/audio.c"),
                 "-o", executable, *shlex.split(flags.stdout), "-lm"],
                env=environment, capture_output=True, text=True,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            environment["LD_LIBRARY_PATH"] = str(library) + (
                os.pathsep + environment["LD_LIBRARY_PATH"] if environment.get("LD_LIBRARY_PATH") else "")
            environment["WLOG_LEVEL"] = "OFF"
            result = subprocess.run([executable], env=environment, capture_output=True,
                                    text=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("FreeRDP client DSP version:", result.stdout)
            self.assertIn("accepts exact AAC_MS and PCM formats; MS ADPCM is filtered", result.stdout)
            self.assertIn("FreeRDP accepts 44.1 kHz and 48 kHz mono and stereo PCM", result.stdout)
            self.assertIn("FreeRDP raw AAC decode: reconnect=0", result.stdout)
            self.assertIn("FreeRDP raw AAC decode: reconnect=1", result.stdout)
