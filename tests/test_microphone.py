"""Remote microphone helper (NRM1 -> private PipeWire source); no host service changes."""
import fcntl
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
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "broker/config/session-microphone"
HEADER = struct.Struct("!4sBBHIIIQ")
REPLY = struct.Struct("!4sBBH")
CAPTURE_HEADER = struct.Struct("!4sIQQQ")


def load_helper():
    loader = importlib.machinery.SourceFileLoader("session_microphone", str(HELPER))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def record(kind, channels=0, rate=0, sequence=0, payload=b"", received_ns=0, flags=0, magic=b"NRM1"):
    return HEADER.pack(magic, kind, channels, flags, rate, sequence, len(payload), received_ns) + payload


def tone(frequency, frames, start=0, rate=48000, amplitude=8000):
    return b"".join(struct.pack("<h", int(amplitude * math.sin(2 * math.pi * frequency * (start + n) / rate)))
                    for n in range(frames))


def tone_fraction(values, frequency, rate=48000):
    energy = sum(value * value for value in values)
    if not energy:
        return 0
    cosine = sum(value * math.cos(2 * math.pi * frequency * index / rate) for index, value in enumerate(values))
    sine = sum(value * math.sin(2 * math.pi * frequency * index / rate) for index, value in enumerate(values))
    return 2 * (cosine * cosine + sine * sine) / (len(values) * energy)


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.module = load_helper()

    def test_header_layout_matches_design(self):
        self.assertEqual(self.module.HEADER.size, 28)
        self.assertEqual(self.module.REPLY.size, 8)
        self.assertEqual(self.module.reply(self.module.IN_USE), b"NRM1\x01\x00\x00\x00")
        self.assertEqual(self.module.reply(self.module.IDLE), b"NRM1\x02\x00\x00\x00")
        self.assertEqual(self.module.reply(self.module.REPLY_HELLO), b"NRM1\x00\x00\x00\x00")

    def test_valid_records_parse(self):
        parse = lambda data: self.module.parse_header(data[:28])
        self.assertEqual(parse(record(0))[0], 0)
        self.assertEqual(parse(record(2))[0], 2)
        self.assertEqual(parse(record(1, 1, 48000))[:3], (1, 1, 48000))
        self.assertEqual(parse(record(1, 2, 44100))[:3], (1, 2, 44100))
        self.assertEqual(parse(record(3, 1, 48000, 7, b"\0" * 1920, 99)),
                         (3, 1, 48000, 7, 1920, 99))
        self.assertEqual(parse(record(3, 2, 48000, 0, b"\0" * 19200))[4], 19200)

    def test_every_violation_is_a_protocol_error(self):
        bad = {
            "magic": record(0, magic=b"NRA1"),
            "flags": record(3, 1, 48000, payload=b"\0\0", flags=1),
            "kind": record(4),
            "hello with format": record(0, 1),
            "hello with payload": record(0, payload=b"\0\0"),
            "unavailable with format": record(2, 2),
            "available with payload": record(1, 1, 48000, payload=b"\0\0"),
            "zero channels": record(3, 0, 48000, payload=b"\0\0"),
            "three channels": record(3, 3, 48000, payload=b"\0" * 6),
            "16 kHz": record(3, 1, 16000, payload=b"\0\0"),
            "unknown rate available": record(1, 2, 96000),
            "oversized": record(3, 2, 48000, payload=b"\0" * 19204),
            "odd mono": record(3, 1, 48000, payload=b"\0" * 3),
            "misaligned stereo": record(3, 2, 48000, payload=b"\0" * 6),
        }
        for name, data in bad.items():
            with self.subTest(name), self.assertRaises(self.module.ProtocolError):
                self.module.parse_header(data[:28])

    def test_reader_reassembles_split_records_and_requires_one_leading_hello(self):
        reader = self.module.Reader()
        stream = record(0) + record(1, 1, 48000) + record(3, 1, 48000, 5, b"\1\0" * 960, 42)
        records = []
        for index in range(len(stream)):
            records.extend(reader.feed(stream[index:index + 1]))
        self.assertEqual([item.kind for item in records], [0, 1, 3])
        self.assertEqual(records[2].payload, b"\1\0" * 960)
        self.assertEqual((records[2].sequence, records[2].received_ns), (5, 42))
        with self.assertRaises(self.module.ProtocolError):
            reader.feed(record(0))
        with self.assertRaises(self.module.ProtocolError):
            self.module.Reader().feed(record(1, 1, 48000))
        # An invalid header is rejected before its claimed payload is awaited.
        reader = self.module.Reader()
        reader.feed(record(0))
        with self.assertRaises(self.module.ProtocolError):
            reader.feed(HEADER.pack(b"NRM1", 3, 1, 0, 48000, 0, 1 << 30, 0))


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.module = load_helper()

    def test_in_use_is_immediate_and_idle_is_debounced(self):
        debouncer = self.module.Debouncer()
        self.assertFalse(debouncer.observe(False, 0))
        self.assertTrue(debouncer.observe(True, 1))
        self.assertTrue(debouncer.in_use)
        self.assertFalse(debouncer.observe(False, 2))
        self.assertEqual(debouncer.deadline(), 7)
        self.assertFalse(debouncer.observe(False, 6.9))
        # A recorder returning within the quiet period cancels IDLE.
        self.assertFalse(debouncer.observe(True, 6.95))
        self.assertIsNone(debouncer.deadline())
        self.assertFalse(debouncer.observe(False, 8))
        self.assertFalse(debouncer.observe(False, 12.99))
        self.assertTrue(debouncer.observe(False, 13))
        self.assertFalse(debouncer.in_use)
        self.assertIsNone(debouncer.deadline())

    def test_pw_cat_budget_is_finite_backs_off_and_resets_after_healthy_run(self):
        budget = self.module.Budget()
        self.assertEqual([budget.failed(1) for _ in range(5)], [0.5, 1.0, 2.0, 2.0, None])
        budget = self.module.Budget()
        self.assertEqual([budget.failed(1), budget.failed(1)], [0.5, 1.0])
        self.assertEqual(budget.failed(600), 0.5)
        self.assertEqual([budget.failed(599.9) for _ in range(4)], [1.0, 2.0, 2.0, None])

    def test_playback_constants_are_not_shared(self):
        self.assertEqual(self.module.SOCKET_NAME, "audio-in-0")
        self.assertEqual(self.module.PW_CAT_ATTEMPTS, 5)
        self.assertFalse(hasattr(self.module, "GRAPH_ATTEMPTS"))
        self.assertNotIn("audio-0\"", HELPER.read_text())
        self.assertNotIn("niri-rdp-sink", HELPER.read_text())


class FallbackPanelTests(unittest.TestCase):
    def test_fallback_waybar_shows_a_microphone_indicator(self):
        import json
        config = json.loads((ROOT / "broker/config/waybar.json").read_text())
        self.assertIn("privacy", config["modules-right"])
        self.assertEqual([module["type"] for module in config["privacy"]["modules"]], ["audio-in"])


class FakeFeeder:
    instances = []
    exit_after = None  # seconds of fake clock before the fake pw-cat exits

    def __init__(self, rate, channels, clock):
        self.format = (rate, channels)
        self.clock = clock
        self.started = clock()
        self.writes = []
        self.closed = False
        self.full = False
        self.pidfd = None
        self.discards = 0
        FakeFeeder.instances.append(self)

    def discard(self):
        self.discards += 1
        return 0

    def write(self, payload):
        if self.full:
            return 1
        self.writes.append(payload)
        return 0

    def exited(self):
        return self.exit_after is not None and self.clock() - self.started >= self.exit_after

    def close(self):
        self.closed = True


class HelperStateTests(unittest.TestCase):
    def setUp(self):
        self.module = load_helper()
        FakeFeeder.instances = []
        FakeFeeder.exit_after = None
        self.now = [100.0]
        self.logs = []
        patcher = patch.object(self.module, "Feeder", FakeFeeder)
        patcher.start()
        self.addCleanup(patcher.stop)
        # Recorder detection talks to pactl; these tests drive it directly.
        for name in ("start_subscriber", "start_query", "set_default"):
            patcher = patch.object(self.module.Microphone, name, lambda *_: None)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(self.listener.close)
        self.helper = self.module.Microphone(self.listener, clock=lambda: self.now[0],
                                             now_ns=lambda: int(self.now[0] * 1e9), log=self.logs.append)
        self.addCleanup(self.helper.close)

    def connect(self):
        frontend, helper_side = socket.socketpair()
        self.addCleanup(frontend.close)
        frontend.settimeout(1)
        self.helper.accept(helper_side)
        return frontend

    def replies(self, frontend, count):
        data = b"".join(frontend.recv(REPLY.size) for _ in range(count))
        return [REPLY.unpack(data[index:index + 8])[1] for index in range(0, len(data), 8)]

    def send(self, frontend, data):
        frontend.sendall(data)
        self.helper.receive()

    def set_recording(self, running):
        listing = b"29\tniri-rdp-sink.monitor\tPipeWire\tfloat32le 2ch 48000Hz\tIDLE\n"
        listing += b"54\tniri-rdp-microphone\tPipeWire\ts16le 1ch 48000Hz\t" + \
            (b"RUNNING" if running else b"IDLE") + b"\n"
        self.helper.sources(listing)

    def audio(self, sequence, age=0.0, frames=960, channels=1, rate=48000):
        return record(3, channels, rate, sequence, b"\1\0" * frames * channels,
                      int((self.now[0] - age) * 1e9))

    @staticmethod
    def audio_frames(feeder):
        data = b"".join(feeder.writes)
        return sum(1 for index in range(0, len(data), 2) if data[index:index + 2] == b"\1\0")

    def advance(self, seconds, step=0.01):
        for _ in range(round(seconds / step)):
            self.now[0] += step
            self.helper.pace()

    def test_audio_is_forwarded_only_while_recorded_and_fresh(self):
        frontend = self.connect()
        self.assertEqual(self.replies(frontend, 2), [0, 2])
        # Before AVAILABLE no source exists, so nothing records and no data flows.
        self.send(frontend, record(0) + self.audio(0))
        self.assertEqual(FakeFeeder.instances, [])
        self.send(frontend, record(1, 1, 48000))
        self.assertEqual([feeder.format for feeder in FakeFeeder.instances], [(48000, 1)])
        feeder = FakeFeeder.instances[0]
        self.send(frontend, self.audio(1))
        self.advance(0.1)
        self.assertEqual(feeder.writes, [], "an unused source gets no input at all")
        self.set_recording(True)
        self.assertEqual(self.replies(frontend, 1), [1])
        self.send(frontend, self.audio(2) + self.audio(3, age=0.101) + self.audio(5, age=0.05))
        self.advance(0.1)
        self.assertEqual(self.audio_frames(feeder), 1920)
        counters = self.helper.counters
        self.assertEqual((counters["forwarded"], counters["aged"], counters["idle"], counters["gaps"]), (2, 1, 2, 1))
        feeder.full = True
        self.send(frontend, self.audio(6))
        self.advance(0.02)
        self.assertGreater(self.helper.counters["full"], 0)
        feeder.full = False
        # IDLE is debounced, but nothing records meanwhile: no stale input queues.
        self.set_recording(False)
        self.now[0] += 4.5
        self.helper.timers()
        written = len(feeder.writes)
        self.send(frontend, self.audio(7))
        self.advance(0.1)
        self.assertEqual(len(feeder.writes), written)
        self.assertEqual(self.helper.pending, bytearray())
        self.now[0] += 0.5
        self.helper.timers()
        self.assertEqual(self.replies(frontend, 1), [2])
        self.assertEqual(self.helper.counters["idle"], 3)
        self.assertFalse(feeder.closed, "the source stays for the desktop session")

    def test_pacing_keeps_a_fixed_lead_pads_silence_and_never_bursts(self):
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000))
        feeder = FakeFeeder.instances[0]
        self.set_recording(True)
        lead = round(self.module.LEAD_SECONDS * 48000)
        self.assertEqual(sum(len(data) for data in feeder.writes), lead * 2, "the lead is primed at once")
        self.advance(1.0)
        written = sum(len(data) for data in feeder.writes) // 2
        self.assertAlmostEqual(written, 48000 + lead, delta=48)
        self.assertEqual(self.audio_frames(feeder), 0, "no data means silence")
        self.assertLessEqual(max(len(data) for data in feeder.writes), self.module.PIPE_BYTES)
        # A stalled helper resumes with a fresh lead instead of a burst.
        feeder.writes.clear()
        self.now[0] += 0.5
        self.helper.pace()
        self.assertEqual(sum(len(data) for data in feeder.writes) // 2, lead)
        # Packets interleave with the clock; a late packet costs silence once,
        # after which the jitter buffer holds the audio a little longer.
        feeder.writes.clear()
        for sequence in range(10):
            self.send(frontend, self.audio(sequence))
            self.advance(0.02)
        self.assertEqual(self.audio_frames(feeder), 9600 - len(self.helper.pending) // 2)
        self.assertLessEqual(len(self.helper.pending), 960 * 2)

    def test_stopped_recorder_discards_unread_input_and_restart_has_a_fresh_lead(self):
        # Regression (Phase 3 lab): pw-cat stops reading when its recorder
        # leaves, so the lead already written stayed in the pipe; each stop
        # and start replayed it and added ~40 ms of delay.
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000))
        feeder = FakeFeeder.instances[0]
        lead = round(self.module.LEAD_SECONDS * 48000) * 2
        self.set_recording(True)
        self.advance(0.5)
        self.assertEqual(feeder.discards, 0, "a running source keeps its input")
        self.set_recording(False)
        self.assertEqual(feeder.discards, 1)
        self.set_recording(False)
        self.assertEqual(feeder.discards, 1, "only the stop itself discards")
        feeder.writes.clear()
        self.set_recording(True)
        self.assertEqual(sum(len(data) for data in feeder.writes), lead)
        self.assertEqual(feeder.discards, 1)

    def test_jitter_buffer_is_cut_back_when_audio_piles_up(self):
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 2, 44100))
        self.set_recording(True)
        self.send(frontend, b"".join(self.audio(sequence, frames=882, channels=2, rate=44100)
                                     for sequence in range(4)))
        self.assertEqual(self.helper.counters["skipped"], 1)
        target = round(self.module.JITTER_TARGET_SECONDS * 44100) * 4
        self.assertLessEqual(len(self.helper.pending), target)
        self.assertEqual(len(self.helper.pending) % 4, 0, "cuts keep whole stereo frames")

    def test_new_connection_supersedes_old_and_learns_current_state(self):
        first = self.connect()
        self.assertEqual(self.replies(first, 2), [0, 2])
        self.send(first, record(0) + record(1, 1, 48000))
        self.set_recording(True)
        self.assertEqual(self.replies(first, 1), [1])
        second = self.connect()
        self.assertEqual(first.recv(16), b"", "superseded frontend is closed")
        self.assertEqual(self.replies(second, 2), [0, 1])
        self.send(second, record(0) + self.audio(0))
        self.advance(0.05)
        self.assertEqual(self.audio_frames(FakeFeeder.instances[0]), 960)
        self.assertEqual(len(FakeFeeder.instances), 1, "a reconnect keeps the same source")

    def test_frontend_reset_with_unread_replies_is_an_ordinary_disconnect(self):
        # The frontend may close while the helper's replies sit unread; the
        # helper then reads ECONNRESET instead of end-of-file. That must end
        # only this connection, like EOF: no crash, no restart-budget use.
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000))
        self.set_recording(True)  # queues IN_USE behind the unread HELLO/IDLE
        frontend.close()
        with self.assertRaises(ConnectionResetError):
            # What the helper's next read sees, shown on a twin pair.
            twin_frontend, twin_helper = socket.socketpair()
            twin_helper.sendall(b"NRM1\0\0\0\0")
            twin_frontend.close()
            try:
                twin_helper.recv(16)
            finally:
                twin_helper.close()
        self.helper.receive()
        self.assertIsNone(self.helper.connection)
        self.assertTrue(any("frontend closed" in line for line in self.logs))
        self.assertFalse(FakeFeeder.instances[0].closed, "the source outlives the connection")
        self.assertEqual(self.helper.budget.failures, 0)
        again = self.connect()
        self.assertEqual(self.replies(again, 2), [0, 1])

    def test_protocol_error_closes_only_that_connection(self):
        frontend = self.connect()
        self.replies(frontend, 2)
        self.send(frontend, record(0) + record(1, 1, 48000))
        self.send(frontend, record(3, 1, 48000, payload=b"\0" * 3))
        self.assertEqual(frontend.recv(16), b"")
        self.assertIsNone(self.helper.connection)
        self.assertFalse(FakeFeeder.instances[0].closed)
        self.assertTrue(any("protocol error" in line for line in self.logs))
        again = self.connect()
        self.assertEqual(self.replies(again, 2), [0, 2])

    def test_frontend_that_stops_reading_is_dropped_not_waited_for(self):
        frontend = self.connect()
        frontend.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        self.helper.connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1024)
        self.send(frontend, record(0) + record(1, 1, 48000))
        started = time.monotonic()
        for _ in range(10000):
            if self.helper.connection is None:
                break
            self.set_recording(True)
            self.helper.debouncer.in_use = False
        self.assertIsNone(self.helper.connection)
        self.assertLess(time.monotonic() - started, 2)

    def test_format_change_recreates_source_without_spending_budget(self):
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000) + record(1, 1, 48000))
        self.assertEqual(len(FakeFeeder.instances), 1, "a repeated AVAILABLE keeps the source")
        self.send(frontend, record(1, 2, 44100))
        self.assertTrue(FakeFeeder.instances[0].closed)
        self.assertEqual(FakeFeeder.instances[1].format, (44100, 2))
        self.set_recording(True)
        self.send(frontend, self.audio(0, frames=882, channels=1, rate=48000))
        self.assertEqual(FakeFeeder.instances[2].format, (48000, 1))
        self.assertEqual(self.helper.budget.failures, 0)

    def test_pw_cat_exits_back_off_and_a_finite_budget_ends_the_helper(self):
        FakeFeeder.exit_after = 1
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000))
        delays = []
        with self.assertRaises(SystemExit) as stopped:
            for _ in range(200):
                self.now[0] += 1
                self.helper.check_feeder()
                if self.helper.restart_at is not None:
                    delays.append(round(self.helper.restart_at - self.now[0], 2))
                    self.now[0] = self.helper.restart_at
                self.helper.timers()
        self.assertEqual(stopped.exception.code, 1)
        self.assertEqual(len(FakeFeeder.instances), 5)
        self.assertEqual(delays, [0.5, 1.0, 2.0, 2.0])

    def test_healthy_pw_cat_run_renews_the_budget(self):
        FakeFeeder.exit_after = 1
        frontend = self.connect()
        self.send(frontend, record(0) + record(1, 1, 48000))
        for exit_after in (1, 1, 1, 600, 1):
            FakeFeeder.exit_after = exit_after
            self.now[0] += exit_after
            self.helper.check_feeder()
            self.now[0] = self.helper.restart_at
            self.helper.timers()
        self.assertEqual(self.helper.budget.failures, 2)
        self.assertEqual(len(FakeFeeder.instances), 6)


class FeederTests(unittest.TestCase):
    def test_full_pipe_drops_whole_chunks_without_blocking(self):
        module = load_helper()
        with tempfile.TemporaryDirectory(prefix="niri-microphone-feeder-") as directory:
            # A stalled pw-cat that never reads its input.
            fake = Path(directory, "pw-cat")
            fake.write_text("#!/usr/bin/sh\nexec sleep 30\n")
            fake.chmod(0o700)
            with patch.object(module, "PW_CAT", str(fake)):
                feeder = module.Feeder(48000, 2, time.monotonic)
            try:
                self.assertEqual(fcntl.fcntl(feeder.process.stdin.fileno(), fcntl.F_GETPIPE_SZ),
                                 module.PIPE_CAPACITY)
                started = time.monotonic()
                dropped = feeder.write(b"\1\0" * 9600)  # 19200 bytes: 4096 x4 + 2816
                dropped += feeder.write(b"\1\0" * 960)
                self.assertLess(time.monotonic() - started, 0.5)
                self.assertEqual(dropped, 2, "four atomic 4 KiB chunks fill the pipe; the rest drop whole")
                self.assertFalse(feeder.exited())
            finally:
                feeder.close()
            self.assertTrue(feeder.exited())


    def test_discard_empties_unread_input_and_keeps_pw_cat_stdin_blocking(self):
        module = load_helper()
        with tempfile.TemporaryDirectory(prefix="niri-microphone-feeder-") as directory:
            fake = Path(directory, "pw-cat")
            fake.write_text("#!/usr/bin/sh\nexec sleep 30\n")
            fake.chmod(0o700)
            with patch.object(module, "PW_CAT", str(fake)):
                feeder = module.Feeder(48000, 1, time.monotonic)
            try:
                self.assertEqual(feeder.write(b"\1\0" * 4096), 0)
                self.assertEqual(feeder.discard(), 8192)
                self.assertEqual(feeder.discard(), 0, "an empty pipe never blocks")
                self.assertEqual(feeder.write(b"\1\0" * 960), 0, "the source keeps its input pipe")
                self.assertEqual(feeder.discard(), 1920)
                # pw-cat's own stdin stays blocking: draining uses a separate
                # open file description of the pipe.
                flags = Path(f"/proc/{feeder.process.pid}/fdinfo/0").read_text().split("flags:")[1].split()[0]
                self.assertFalse(int(flags, 8) & os.O_NONBLOCK)
                self.assertFalse(feeder.exited())
            finally:
                feeder.close()
            self.assertIsNone(feeder.drain_fd)


class SessionWorkerMicrophoneTests(unittest.TestCase):
    def run_worker(self, directory, audio="healthy", microphone="fail", healthy_run_attempt=None,
                   desktop_seconds=4.5, replacements=()):
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
        prologue = ("#!/usr/bin/python3\nimport os,socket,time\nfrom pathlib import Path\n"
                    "p=Path(os.environ['NIRI_RDP_RUNTIME_DIR'])\n")
        if audio == "healthy":
            (binaries / "session-audio").write_text(
                prologue + "with (p/'audio-attempts').open('a') as f: f.write('attempt\\n')\n"
                "s=socket.socket(socket.AF_UNIX); s.bind(str(p/'audio-0')); s.listen(1)\ntime.sleep(60)\n")
        else:
            (binaries / "session-audio").write_text(
                prologue + "with (p/'audio-attempts').open('a') as f: f.write('attempt\\n')\nraise SystemExit(1)\n")
        body = {"fail": "raise SystemExit(1)\n",
                "hang": "(p/'microphone-pid').write_text(str(os.getpid()))\ntime.sleep(60)\n"}[microphone]
        (binaries / "session-microphone").write_text(
            prologue + "q=p/'microphone-attempts'\nwith q.open('a') as f: f.write('attempt\\n')\n"
            f"if len(q.read_text().splitlines()) == {healthy_run_attempt!r}: time.sleep(3)\n" + body)
        (binaries / "niri-headless").write_text(
            prologue + "(p/'desktop-started').write_text(str(time.monotonic()))\n"
            f"time.sleep({desktop_seconds!r})\n(p/'desktop-finished').touch()\n")
        for executable in binaries.iterdir():
            executable.chmod(0o700)
        environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                           NIRI_RDP_RUNTIME_DIR=directory)
        started = time.monotonic()
        process = subprocess.Popen(["/usr/bin/sh", str(root / "worker"), "--environment-ready"],
                                   env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            _, errors = process.communicate(timeout=desktop_seconds + 6)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        self.assertEqual(process.returncode, 0, errors.decode())
        self.assertTrue((root / "desktop-finished").exists())
        desktop_delay = float((root / "desktop-started").read_text()) - started
        attempts = lambda name: (root / name).read_text().splitlines() if (root / name).exists() else []
        return attempts("audio-attempts"), attempts("microphone-attempts"), errors, desktop_delay

    def assert_playback_wording_untouched(self, errors):
        for line in errors.splitlines():
            if b"microphone" in line:
                self.assertNotRegex(line, rb"restart \d of 2|without audio")

    def test_failing_microphone_never_delays_or_ends_desktop_and_retries_are_bounded(self):
        with tempfile.TemporaryDirectory(prefix="niri-microphone-worker-") as directory:
            audio, microphone, errors, delay = self.run_worker(directory, desktop_seconds=6.5)
            self.assertEqual(audio, ["attempt"], "playback helper is never restarted for the microphone")
            self.assertEqual(microphone, ["attempt"] * 3)
            self.assertLess(delay, 2, "the desktop never waits for the microphone")
            self.assertEqual(errors.count(b"remote microphone helper stopped; retry 1/2"), 1, errors.decode())
            self.assertEqual(errors.count(b"remote microphone helper stopped; retry 2/2"), 1, errors.decode())
            self.assertIn(b"microphone off for this session", errors)
            self.assertNotIn(b"restart 1 of 2", errors)
            self.assert_playback_wording_untouched(errors)

    def test_healthy_microphone_run_renews_only_its_own_budget(self):
        with tempfile.TemporaryDirectory(prefix="niri-microphone-worker-") as directory:
            audio, microphone, errors, _ = self.run_worker(
                directory, healthy_run_attempt=3, desktop_seconds=8.5,
                replacements=(("mic_healthy_seconds=600", "mic_healthy_seconds=2"),
                              ("mic_retry_ticks=$((mic_restarts * 2))", "mic_retry_ticks=1")))
            self.assertEqual(audio, ["attempt"])
            self.assertEqual(microphone, ["attempt"] * 5)
            self.assertEqual(errors.count(b"retry 1/2"), 2, errors.decode())
            self.assertEqual(errors.count(b"retry 2/2"), 2, errors.decode())

    def test_playback_failures_and_budget_are_independent_of_the_microphone(self):
        with tempfile.TemporaryDirectory(prefix="niri-microphone-worker-") as directory:
            audio, microphone, errors, _ = self.run_worker(directory, audio="fail", microphone="hang")
            self.assertEqual(audio, ["attempt"] * 3)
            self.assertEqual(errors.count(b"restart 1 of 2"), 1, errors.decode())
            self.assertEqual(errors.count(b"restart 2 of 2"), 1, errors.decode())
            self.assertEqual(microphone, ["attempt"], "playback failures never restart the microphone")
            self.assertNotIn(b"remote microphone helper stopped", errors)

    def test_microphone_helper_ends_with_the_desktop(self):
        with tempfile.TemporaryDirectory(prefix="niri-microphone-worker-") as directory:
            self.run_worker(directory, microphone="hang", desktop_seconds=1.5)
            pid = int(Path(directory, "microphone-pid").read_text())
            deadline = time.monotonic() + 3
            while not process_gone(pid):
                self.assertLess(time.monotonic(), deadline, "worker cleanup must stop the microphone helper")
                time.sleep(0.05)

    def test_playback_lines_of_the_worker_are_unchanged(self):
        worker = (ROOT / "broker/config/session-worker").read_text()
        self.assertEqual(worker.count("/usr/lib/niri-rdp/session-audio &"), 2)
        for line in ("audio_healthy_seconds=600",
                     'echo "private audio helper restart $audio_restarts of 2; active clients reconnect for fresh capture" >&2',
                     'while [ ! -S "$NIRI_RDP_RUNTIME_DIR/audio-0" ]; do'):
            self.assertEqual(worker.count(line), 1, line)
        self.assertNotIn("audio-in-0\" ]", worker, "nothing waits for the microphone socket")


def process_gone(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split()[2] in ("Z", "X")
    except (FileNotFoundError, ProcessLookupError):
        return True


def pcm_values(data):
    return struct.unpack("<" + "h" * (len(data) // 2), data[:len(data) // 2 * 2])


def read_exact(connection, length):
    data = bytearray()
    while len(data) < length:
        piece = connection.recv(length - len(data))
        if not piece:
            raise EOFError("connection ended mid-record")
        data.extend(piece)
    return bytes(data)


class MonitorReader(threading.Thread):
    """Collect playback monitor frames (NRA1) from audio-0 in the background."""

    def __init__(self, path):
        super().__init__(daemon=True)
        self.connection = socket.socket(socket.AF_UNIX)
        self.connection.settimeout(3)
        self.connection.connect(path)
        self.connection.sendall(b"NRA1")
        self.frames = []
        self.stopping = False
        self.error = None

    def run(self):
        try:
            while not self.stopping:
                magic, length, sequence, _, _ = CAPTURE_HEADER.unpack(
                    read_exact(self.connection, CAPTURE_HEADER.size))
                if magic != b"NRA1":
                    raise ValueError("bad monitor framing")
                self.frames.append((sequence, read_exact(self.connection, length)))
        except (OSError, EOFError, ValueError) as error:
            if not self.stopping:
                self.error = error

    def stop(self):
        self.stopping = True
        self.connection.close()
        self.join(timeout=3)

    def samples(self):
        return pcm_values(b"".join(payload for _, payload in self.frames))


@unittest.skipUnless(all(shutil.which(tool) for tool in ("pipewire", "wireplumber", "pipewire-pulse",
                                                           "pw-cat", "pw-record", "pactl", "pacat",
                                                           "dbus-run-session")),
                     "private PipeWire graph tools required")
class MicrophoneIntegrationTests(unittest.TestCase):
    """The real private graph from session-audio with the real microphone helper."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="niri-microphone-")
        self.addCleanup(self.temporary.cleanup)
        directory = self.directory = self.temporary.name
        self.environment = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory + "/config",
                                XDG_CACHE_HOME=directory + "/cache", XDG_DATA_HOME=directory + "/data",
                                XDG_STATE_HOME=directory + "/state", GIO_USE_VFS="local",
                                NIRI_RDP_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory,
                                PIPEWIRE_RUNTIME_DIR=directory, PIPEWIRE_REMOTE="pipewire-0",
                                PULSE_SERVER="unix:" + directory + "/pulse/native")
        self.log = tempfile.TemporaryFile()
        self.addCleanup(self.log.close)
        self.graph = subprocess.Popen(["dbus-run-session", "--", "python3", str(ROOT / "broker/config/session-audio")],
                                      env=self.environment, stdout=self.log, stderr=self.log, start_new_session=True)
        self.addCleanup(self.stop, self.graph)
        self.wait_for(lambda: Path(directory, "audio-0").is_socket(), "playback graph startup")
        self.helper = self.start_helper()

    def start_helper(self):
        helper = subprocess.Popen(["python3", str(HELPER)], env=self.environment,
                                  stdout=self.log, stderr=self.log, start_new_session=True)
        self.addCleanup(self.stop, helper)
        self.wait_for(lambda: Path(self.directory, "audio-in-0").is_socket(), "microphone helper startup")
        return helper

    @staticmethod
    def stop(process):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()

    def wait_for(self, condition, what, timeout=15):
        deadline = time.monotonic() + timeout
        while not condition():
            if self.graph.poll() is not None or time.monotonic() > deadline:
                self.log.seek(0)
                self.fail(what + " failed: " + self.log.read().decode(errors="replace")[-4000:])
            time.sleep(0.05)

    def pactl(self, *arguments):
        return subprocess.run(["pactl", *arguments], env=self.environment, capture_output=True,
                              text=True, timeout=5).stdout

    def frontend(self, rate=48000, channels=1):
        connection = socket.socket(socket.AF_UNIX)
        connection.settimeout(10)
        connection.connect(self.directory + "/audio-in-0")
        self.addCleanup(connection.close)
        self.assertEqual(REPLY.unpack(read_exact(connection, 8)), (b"NRM1", 0, 0, 0))
        self.assertEqual(REPLY.unpack(read_exact(connection, 8))[1], 2, "IDLE before any recorder")
        connection.sendall(record(0) + record(1, channels, rate))
        return connection

    def source_present(self):
        return any(line.split("\t")[1:2] == ["niri-rdp-microphone"]
                   for line in self.pactl("list", "short", "sources").splitlines())

    def recorder(self):
        recorder = subprocess.Popen(["pw-record", "--rate=48000", "--channels=1", "--format=s16", "--raw", "-"],
                                    env=self.environment, stdout=subprocess.PIPE, start_new_session=True)
        self.addCleanup(recorder.stdout.close)
        self.addCleanup(self.stop, recorder)
        chunks = []
        reader = threading.Thread(target=lambda: chunks.extend(iter(lambda: recorder.stdout.read(4096), b"")),
                                  daemon=True)
        reader.start()
        return recorder, chunks, reader

    def send_tone(self, connection, frequency, seconds, first_sequence=0, stall_after=None,
                  rate=48000, channels=1):
        start = time.monotonic()
        sequence = first_sequence
        packets = int(seconds / 0.02)
        frames = rate // 50
        for index in range(packets):
            if stall_after is not None and index == stall_after:
                start += 1.0
            while time.monotonic() < start + index * 0.02:
                time.sleep(0.002)
            payload = tone(frequency, frames, index * frames, rate=rate)
            if channels == 2:
                payload = b"".join(payload[offset:offset + 2] * 2 for offset in range(0, len(payload), 2))
            connection.sendall(record(3, channels, rate, sequence, payload, time.monotonic_ns()))
            sequence += 1
        return sequence

    def helper_children(self):
        try:
            children = Path(f"/proc/{self.helper.pid}/task/{self.helper.pid}/children").read_text().split()
        except FileNotFoundError:
            return {}
        result = {}
        for child in children:
            try:
                result[int(child)] = Path(f"/proc/{child}/comm").read_text().strip()
            except FileNotFoundError:
                pass
        return result

    def test_tone_reaches_recorders_never_playback_and_idle_follows(self):
        directory = self.directory
        monitor = MonitorReader(directory + "/audio-0")
        monitor.start()
        self.addCleanup(monitor.stop)
        connection = self.frontend()
        self.wait_for(self.source_present, "remote microphone source")
        self.wait_for(lambda: self.pactl("get-default-source").strip() == "niri-rdp-microphone",
                      "default source")
        self.assertIn("Description: Remote microphone (RDP)", self.pactl("list", "sources"))
        self.assertEqual(self.pactl("get-default-sink").strip(), "niri-rdp-sink")
        sinks = self.pactl("list", "short", "sinks").splitlines()
        self.assertEqual([line.split("\t")[1] for line in sinks], ["niri-rdp-sink"])
        recorder, chunks, reader = self.recorder()
        started = time.monotonic()
        self.assertEqual(REPLY.unpack(read_exact(connection, 8))[1], 1, "IN_USE once a recorder runs")
        self.assertLess(time.monotonic() - started, 3)
        # One second of tone, a one-second stall, then another second.
        self.send_tone(connection, 1000, 2.0, stall_after=50)
        time.sleep(0.3)
        recorder.send_signal(signal.SIGINT)
        recorder.wait(timeout=5)
        reader.join(timeout=5)
        values = pcm_values(b"".join(chunks))
        loud = [index for index, value in enumerate(values) if abs(value) > 2000]
        self.assertTrue(loud, "the recorder must hear the remote microphone")
        body = values[loud[0]:loud[-1] + 1]
        windows = [body[start:start + 4096] for start in range(0, len(body) - 4095, 4096)]
        self.assertGreater(max(tone_fraction(window, 1000) for window in windows), 0.95,
                           "recorded audio must be the tone, not noise")
        self.assertGreater(max(map(abs, body)), 6000)
        # The stall is one run of silence, then the tone resumes. Paced input
        # leaves no regular gaps (unpaced pw-cat input lost 20-25 %), and
        # nothing stale is replayed: the span matches the audio actually sent.
        runs = [len(run) for run in "".join("q" if abs(v) < 50 else "l" for v in body).split("l") if len(run) >= 5]
        stall = max(runs)
        self.assertGreater(stall, 48000 * 0.7, "a stalled frontend gives silence")
        self.assertLess(stall, 48000 * 1.3)
        self.assertLess(sum(runs) - stall, 48000 * 0.02, "gaps in the paced microphone stream")
        self.assertLess(abs(len(body) - stall - 96000), 4800, "lost or replayed microphone audio")
        # Never routed into playback: the monitor stays silent throughout.
        self.assertIsNone(monitor.error)
        self.assertGreater(len(monitor.frames), 50)
        self.assertLess(max(map(abs, monitor.samples())), 50, "microphone audio leaked into playback")
        started = time.monotonic()
        self.assertEqual(REPLY.unpack(read_exact(connection, 8))[1], 2, "IDLE after the recorder stops")
        self.assertGreater(time.monotonic() - started, 3, "IDLE is debounced")

    def test_44k_stereo_fallback_is_announced_to_the_graph_not_resampled(self):
        connection = self.frontend(rate=44100, channels=2)
        self.wait_for(self.source_present, "remote microphone source")
        self.assertIn("s16le 2ch 44100Hz", self.pactl("list", "short", "sources"))
        recorder, chunks, reader = self.recorder()
        self.assertEqual(REPLY.unpack(read_exact(connection, 8))[1], 1)
        self.send_tone(connection, 1000, 1.0, rate=44100, channels=2)
        time.sleep(0.3)
        recorder.send_signal(signal.SIGINT)
        recorder.wait(timeout=5)
        reader.join(timeout=5)
        values = pcm_values(b"".join(chunks))
        loud = [index for index, value in enumerate(values) if abs(value) > 2000]
        self.assertTrue(loud, "the recorder must hear the 44.1 kHz microphone")
        body = values[loud[0]:loud[-1] + 1]
        self.assertAlmostEqual(len(body), 48000, delta=4800, msg="the graph converts 44.1 kHz to 48 kHz")
        self.assertGreater(max(tone_fraction(body[start:start + 4096], 1000)
                               for start in range(0, len(body) - 4095, 4096)), 0.95)

    def test_stalled_microphone_never_gaps_playback_and_helper_death_spares_it(self):
        directory = self.directory
        connection = self.frontend()
        self.wait_for(self.source_present, "remote microphone source")
        recorder, _, _ = self.recorder()
        self.assertEqual(REPLY.unpack(read_exact(connection, 8))[1], 1)
        monitor = MonitorReader(directory + "/audio-0")
        monitor.start()
        self.addCleanup(monitor.stop)
        player = subprocess.Popen(["pacat", "--playback", "--raw", "--format=s16le", "--rate=48000",
                                   "--channels=2", "--latency-msec=20"], env=self.environment,
                                  stdin=subprocess.PIPE, start_new_session=True)
        self.addCleanup(self.stop, player)
        samples = b"".join(struct.pack("<hh", value, value) for value in
                           struct.unpack("<" + "h" * 48000 * 4, tone(440, 48000 * 4, amplitude=10000)))
        writer = threading.Thread(target=lambda: player.communicate(samples), daemon=True)
        writer.start()
        pw_cat = next(pid for pid, name in self.helper_children().items() if name == "pw-cat")
        sender = threading.Thread(target=self.send_tone, args=(connection, 1000, 3.5), daemon=True)
        sender.start()
        time.sleep(1.0)
        os.kill(pw_cat, signal.SIGSTOP)
        time.sleep(1.5)
        os.kill(pw_cat, signal.SIGCONT)
        sender.join(timeout=10)
        writer.join(timeout=10)
        monitor.stop()
        self.assertIsNone(monitor.error)
        sequences = [sequence for sequence, _ in monitor.frames]
        self.assertEqual(sequences, list(range(sequences[0], sequences[0] + len(sequences))),
                         "playback relay dropped frames while the microphone stalled")
        left = monitor.samples()[::2]
        loud = [index for index, value in enumerate(left) if abs(value) > 2000]
        self.assertGreater(len(loud), 48000 * 3)
        body = left[loud[0]:loud[-1]]
        longest_quiet = max(len(run) for run in "".join("q" if abs(v) < 200 else "l" for v in body).split("l"))
        self.assertLess(longest_quiet, 48, "playback gap while the microphone node was stopped")
        self.assertGreater(tone_fraction(body[48000:48000 + 4096], 440), 0.9)
        # Killing the helper removes the source; playback capture carries on.
        os.killpg(self.helper.pid, signal.SIGKILL)
        self.helper.wait(timeout=5)
        self.wait_for(lambda: not self.source_present(), "source removal with its helper")
        self.wait_for(lambda: process_gone(pw_cat), "pw-cat exit with its helper")
        with socket.socket(socket.AF_UNIX) as resumed:
            resumed.settimeout(3)
            resumed.connect(directory + "/audio-0")
            resumed.sendall(b"NRA1")
            magic, length, _, _, _ = CAPTURE_HEADER.unpack(read_exact(resumed, CAPTURE_HEADER.size))
            self.assertEqual((magic, length), (b"NRA1", 3840))
        self.assertEqual(self.pactl("get-default-sink").strip(), "niri-rdp-sink")
        recorder.terminate()


if __name__ == "__main__":
    unittest.main()
