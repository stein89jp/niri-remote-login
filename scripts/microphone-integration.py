#!/usr/bin/env python3
"""Microphone (MS-RDPEAI) lab: real Niri, frontend, session graph and FreeRDP.

Run only inside lab/run-isolated.sh. It never touches host services.

Session side: the lab compositor, the real broker/config/session-audio graph
and broker/config/session-microphone helper in a private runtime directory, and
the frontend in broker mode behind scripts/mock_broker.py, which hands over the
desktop, playback (audio-0) and microphone (audio-in-0) descriptors like the
root broker. Client side: a second private PipeWire graph with two null sinks
and lab/rdp-audio-client.c, whose FreeRDP PulseAudio backends play RDPSND into
client-speaker and capture the microphone from client-mic-sink.monitor.

Signals: a 1 kHz microphone tone and a 440 Hz playback tone, 700 ms and 900 ms bursts
every second at -15 dBFS RMS, written on CLOCK_MONOTONIC (shared by both sides
of the namespace). Latency is measured from the write of a burst's first sample
(an upper bound: it includes the writer's 20 ms lead) to the arrival of that
sample in the recorder's output.

Phases: A playback only with NIRI_RDP_MICROPHONE=off; B microphone and playback
together, idle close and reopen; C reconnect and an abrupt client kill; D helper
SIGKILL while recording and playing, helper restart and recovery on reconnect.
"""
import json
import math
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import wave

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mock_broker import MockBroker  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
RATE = 48000
AMPLITUDE = 0.25
EXPECTED_DBFS = 20 * math.log10(AMPLITUDE / math.sqrt(2))
MIC_BURST_SECONDS = 0.7
# Playback is almost continuous, so its underruns rarely fall into a gap.
PLAYBACK_BURST_SECONDS = 0.9
PERIOD_SECONDS = 1.0
MIC_TONE = 1000.0
PLAYBACK_TONE = 440.0
LEAD_SECONDS = 0.02
IDLE_CLOSE_LIMIT = 8.0  # helper debounce (5 s) plus recycle and slack
LATENCY_LIMIT_MS = 200
# The approved low-delay path pads a late packet with silence (a few samples)
# instead of buffering more; allow that, but not a steady stream of gaps.
MIC_SETTLED_GLITCHES_PER_MINUTE = 30


def wait_until(predicate, timeout, what):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise RuntimeError("timed out waiting for " + what)


def stop(process, sig=signal.SIGTERM):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


class ToneWriter(threading.Thread):
    """Pace tone bursts into a pw-cat stream, LEAD ahead, never blocking."""

    def __init__(self, process, frequency, channels, burst_seconds):
        super().__init__(daemon=True)
        self.process, self.frequency, self.channels = process, frequency, channels
        self.burst_seconds = burst_seconds
        self.onsets = []  # (burst index, CLOCK_MONOTONIC ns of its first write)
        self.dropped = 0
        self.finished = threading.Event()
        os.set_blocking(process.stdin.fileno(), False)

    def run(self):
        start = time.monotonic()
        written = 0
        period = int(PERIOD_SECONDS * RATE)
        burst = int(self.burst_seconds * RATE)
        while not self.finished.is_set():
            due = int((time.monotonic() - start + LEAD_SECONDS) * RATE) - written
            if due > 0:
                index = np.arange(written, written + due)
                values = np.where(index % period < burst,
                                  AMPLITUDE * np.sin(2 * np.pi * self.frequency * index / RATE), 0.0)
                pcm = np.repeat(np.round(values * 32767).astype("<i2")[:, None], self.channels, axis=1).tobytes()
                first = -(-written // period) * period
                now = time.monotonic_ns()
                for onset in range(first, written + due, period):
                    self.onsets.append((onset // period, now))
                step = 4096 - 4096 % (2 * self.channels)
                for offset in range(0, len(pcm), step):
                    try:
                        os.write(self.process.stdin.fileno(), pcm[offset:offset + step])
                    except BlockingIOError:
                        self.dropped += 1
                    except BrokenPipeError:
                        return
                written += due
            time.sleep(0.005)


class Recorder(threading.Thread):
    """Read a raw S16LE recorder and timestamp every chunk on arrival."""

    def __init__(self, argv, environment, channels, log):
        super().__init__(daemon=True)
        self.channels = channels
        self.chunks = []
        self.process = subprocess.Popen(argv, env=environment, stdout=subprocess.PIPE, stderr=log,
                                        stdin=subprocess.DEVNULL, bufsize=0, start_new_session=True)
        self.started_ns = time.monotonic_ns()
        self.start()

    def run(self):
        while True:
            data = os.read(self.process.stdout.fileno(), 65536)
            if not data:
                return
            self.chunks.append((time.monotonic_ns(), data))

    def finish(self):
        stop(self.process)
        self.join(timeout=5)
        return self.chunks

    def first_tone_ns(self, threshold=0.3 * AMPLITUDE):
        """Arrival time of the first sample above the threshold, or None."""
        frame = 2 * self.channels
        carry = b""
        for arrived, data in list(self.chunks):
            data = carry + data
            usable = len(data) - len(data) % frame
            carry = data[usable:]
            samples = np.frombuffer(data[:usable], dtype="<i2")[::self.channels]
            if samples.size and np.max(np.abs(samples)) > threshold * 32768:
                return arrived
        return None


def analyze(chunks, channels, frequency, onsets, other=None):
    burst_seconds = MIC_BURST_SECONDS if frequency == MIC_TONE else PLAYBACK_BURST_SECONDS
    """Bursts, frequency, level, latency and discontinuities of one recording."""
    frame = 2 * channels
    data = b"".join(chunk for _, chunk in chunks)
    count = len(data) // frame
    result = {"seconds": round(count / RATE, 2)}
    if count < RATE:
        result["bursts"] = 0
        return result
    x = np.frombuffer(data[:count * frame], dtype="<i2").reshape(count, channels)[:, 0] / 32768.0
    ends = np.cumsum([len(chunk) for _, chunk in chunks]) // frame
    times = np.array([arrived for arrived, _ in chunks], dtype=np.int64)
    window = 48
    rms = np.sqrt(np.mean(x[:count // window * window].reshape(-1, window) ** 2, axis=1))
    expected = AMPLITUDE / math.sqrt(2)
    bursts, active, quiet, begin = [], False, 0, 0
    for index, value in enumerate(rms):
        if not active and value > 0.5 * expected:
            low = max(0, (index - 1) * window)
            segment = np.abs(x[low:(index + 1) * window]) > 0.3 * AMPLITUDE
            begin, active, quiet = low + int(np.argmax(segment)), True, 0
        elif active:
            quiet = quiet + 1 if value < 0.1 * expected else 0
            if quiet >= 20:
                bursts.append((begin, (index - 19) * window))
                active = False
    complete, irregular = [], 0
    for begin, end in bursts:
        before = x[max(0, begin - RATE // 20):begin]
        if begin < RATE // 20 or np.sqrt(np.mean(before ** 2)) > 0.1 * expected:
            continue  # recording or capture started inside this burst
        if abs((end - begin) / RATE - burst_seconds) > 0.02:
            irregular += 1
            continue
        complete.append((begin, end))
    frequencies, levels, latencies, glitches, dropouts, steady_seconds = [], [], [], 0, 0, 0.0
    glitch_times = []
    onset_times = np.array([at for _, at in onsets], dtype=np.int64)
    for begin, end in complete:
        steady = x[begin + 480:end - 480]
        spectrum = np.abs(np.fft.rfft(steady * np.hanning(steady.size), 8 * steady.size))
        peak = int(np.argmax(spectrum[1:-1])) + 1
        a, b, c = np.log(spectrum[peak - 1:peak + 2] + 1e-12)
        measured = (peak + 0.5 * (a - c) / (a - 2 * b + c)) * RATE / (8 * steady.size)
        frequencies.append(measured)
        levels.append(20 * math.log10(float(np.sqrt(np.mean(steady ** 2)))))
        # A pure sinusoid satisfies x[n] = 2cos(w)x[n-1] - x[n-2]; any lost,
        # repeated or zero-filled sample leaves a residual far above rounding.
        omega = 2 * math.pi * measured / RATE
        residual = np.abs(steady[2:] - 2 * math.cos(omega) * steady[1:-1] + steady[:-2])
        hits = np.flatnonzero(residual > 0.01)
        if hits.size:
            starts = hits[np.concatenate(([True], np.diff(hits) > 480))]
            glitch_times.extend(round((begin + 480 + int(at)) / RATE, 3) for at in starts)
        glitches += int(hits.size and 1 + np.count_nonzero(np.diff(hits) > 480))
        steady_seconds += steady.size / RATE
        inner = np.sqrt(np.mean(steady[:steady.size // window * window].reshape(-1, window) ** 2, axis=1))
        dropouts += int(np.count_nonzero(inner < 0.5 * expected))
        arrived = int(times[np.searchsorted(ends, begin, side="right")])
        written = onset_times[onset_times <= arrived]
        if written.size and arrived - written[-1] < PERIOD_SECONDS * 1e9:
            latencies.append((arrived - int(written[-1])) / 1e6)
    result.update(bursts=len(complete), irregular_bursts=irregular, glitches=glitches, dropout_ms=dropouts,
                  tone_seconds=round(steady_seconds, 2),
                  glitches_per_minute=round(60 * glitches / steady_seconds, 1) if steady_seconds else None,
                  glitch_times_s=glitch_times)
    if steady_seconds and count > 3 * RATE:
        # After the first 1.5 s the low-delay jitter buffer has settled.
        settled = sum(1 for at in glitch_times if at > 1.5)
        result["settled_glitches_per_minute"] = round(
            60 * settled / (steady_seconds * (1 - 1.5 * RATE / count)), 1)
    if frequencies:
        result.update(
            frequency_hz=[round(min(frequencies), 2), round(float(np.median(frequencies)), 2), round(max(frequencies), 2)],
            level_dbfs=[round(min(levels), 2), round(float(np.median(levels)), 2), round(max(levels), 2)],
            expected_dbfs=round(EXPECTED_DBFS, 2))
    if latencies:
        result.update(latency_ms={"p50": round(float(np.percentile(latencies, 50)), 1),
                                  "p95": round(float(np.percentile(latencies, 95)), 1),
                                  "max": round(max(latencies), 1), "count": len(latencies)},
                      latency_series_ms=[round(value) for value in latencies])
    if other is not None:
        # Cross-talk: energy of the other direction's tone in this recording.
        spectrum = np.abs(np.fft.rfft(x)) ** 2
        bins = np.fft.rfftfreq(x.size, 1 / RATE)
        near = lambda f: spectrum[(bins > f - 5) & (bins < f + 5)].sum()  # noqa: E731
        total = spectrum.sum() or 1
        result["crosstalk_fraction"] = round(float(near(other) / total), 6)
    return result


def require_tone(name, result, frequency, minimum_bursts, latency=True, glitches_per_minute=None):
    problems = []
    if result.get("bursts", 0) < minimum_bursts:
        problems.append(f"{result.get('bursts', 0)} complete bursts < {minimum_bursts}")
    if "frequency_hz" in result and max(abs(f - frequency) for f in result["frequency_hz"]) > 2:
        problems.append(f"frequency {result['frequency_hz']} is not {frequency} Hz")
    if "level_dbfs" in result and max(abs(level - EXPECTED_DBFS) for level in result["level_dbfs"]) > 1:
        problems.append(f"level {result['level_dbfs']} is not {EXPECTED_DBFS:.2f} dBFS")
    if glitches_per_minute is not None and (result.get("settled_glitches_per_minute") or 0) > glitches_per_minute:
        problems.append(f"settled discontinuities {result['settled_glitches_per_minute']}/min "
                        f"exceed {glitches_per_minute}/min")
    if latency and result.get("latency_ms", {}).get("p95", 1e9) > LATENCY_LIMIT_MS:
        problems.append(f"latency {result.get('latency_ms')} exceeds {LATENCY_LIMIT_MS} ms")
    if result.get("crosstalk_fraction", 0) > 0.001:
        problems.append(f"cross-talk {result['crosstalk_fraction']}")
    if problems:
        raise RuntimeError(name + ": " + "; ".join(problems))


def save_wave(path, chunks, channels):
    path.parent.mkdir(parents=True, exist_ok=True)
    data = b"".join(chunk for _, chunk in chunks)
    with wave.open(str(path), "wb") as output:
        output.setnchannels(channels)
        output.setsampwidth(2)
        output.setframerate(RATE)
        output.writeframes(data[:len(data) - len(data) % (2 * channels)])


def no_creep(name, result, first):
    """Each stop/start or reconnect starts from the same delay, never a higher one."""
    if result["latency_ms"]["p50"] > first["latency_ms"]["p50"] + 30:
        raise RuntimeError(f"{name}: latency {result['latency_ms']} grew from {first['latency_ms']}")


def same_playback(name, result, baseline):
    """Playback with the microphone path active matches the microphone-off run."""
    problems = []
    if (result.get("glitches_per_minute") or 0) > (baseline.get("glitches_per_minute") or 0) + 6:
        problems.append(f"discontinuities {result['glitches_per_minute']}/min vs "
                        f"{baseline['glitches_per_minute']}/min without the microphone")
    if result["latency_ms"]["p50"] > baseline["latency_ms"]["p50"] + 20:
        problems.append(f"latency {result['latency_ms']} vs {baseline['latency_ms']} without the microphone")
    if problems:
        raise RuntimeError(name + ": " + "; ".join(problems))


def graph_configuration(directory, sinks):
    configuration = directory / "audio-config"
    configuration.mkdir(parents=True)
    objects = "".join(f''' {{ factory = adapter args = {{
   factory.name = support.null-audio-sink
   node.name = {name}
   node.description = "{name}"
   media.class = Audio/Sink
   audio.position = [ {position} ]
   node.virtual = true
   node.always-process = true
 }} }}
''' for name, position in sinks)
    (configuration / "pipewire.conf").write_text(Path("/usr/share/pipewire/pipewire.conf").read_text().replace(
        "context.objects = [", "context.objects = [\n" + objects))
    (configuration / "pipewire-pulse.conf").write_text(Path("/usr/share/pipewire/pipewire-pulse.conf").read_text())
    (configuration / "wireplumber.conf").write_text(
        Path("/usr/share/wireplumber/wireplumber.conf").read_text().replace("wireplumber.profiles = {", '''wireplumber.profiles = {
 remote = {
  inherits = [ main ]
  hardware.audio = disabled
  hardware.bluetooth = disabled
  hardware.video-capture = disabled
 }
'''))
    return configuration


class ClientCaptureWatch(threading.Thread):
    """Track the FreeRDP audin record stream on the client graph.

    A single long-lived `pactl subscribe` client, and one listing per
    source-output event, so polling never reconfigures the measured graph.
    """

    def __init__(self, environment, log):
        super().__init__(daemon=True)
        self.environment = environment
        self.history = []  # (CLOCK_MONOTONIC ns, number of audin streams)
        self.process = subprocess.Popen(["pactl", "subscribe"], env=environment, stdout=subprocess.PIPE,
                                        stderr=log, stdin=subprocess.DEVNULL, start_new_session=True)
        # Streams present before any RDP connection are the lab's own reference.
        self.ignore = 0
        self.ignore = self.count()
        self.history.append((time.monotonic_ns(), 0))
        self.start()

    def count(self):
        def listing(kind):
            return subprocess.run(["pactl", "list", "short", kind], env=self.environment,
                                  capture_output=True, text=True, timeout=5).stdout.splitlines()
        index = next((line.split("\t")[0] for line in listing("sources")
                      if line.split("\t")[1:2] == ["client-mic-sink.monitor"]), None)
        return sum(1 for line in listing("source-outputs") if line.split("\t")[1:2] == [index]) - self.ignore

    def run(self):
        for line in self.process.stdout:
            if b"source-output" in line:
                at = time.monotonic_ns()
                self.history.append((at, self.count()))

    def current(self):
        return self.history[-1][1]

    def change_after(self, since_ns, wanted):
        return next((at for at, count in self.history if at >= since_ns and (count > 0) == wanted), None)


def main():
    if ROOT != Path("/work") or Path("/dev/input").exists() or os.environ.get("DISPLAY"):
        raise RuntimeError("run only inside lab/run-isolated.sh with private devices")
    for tool in ("pipewire", "pipewire-pulse", "wireplumber", "pactl", "parec", "pw-cat", "pw-record",
                 "cc", "pkg-config", "openssl"):
        if not shutil.which(tool):
            raise RuntimeError(tool + " is required")
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    base = {key: os.environ[key] for key in ("PATH", "HOME", "XDG_RUNTIME_DIR", "LD_LIBRARY_PATH",
                                             "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "XKB_CONFIG_ROOT",
                                             "NIRI_RDP_RENDERER") if key in os.environ}
    base.update(XKB_DEFAULT_RULES="evdev", XKB_DEFAULT_MODEL="jp106", XKB_DEFAULT_LAYOUT="jp",
                XKB_DEFAULT_VARIANT="", XKB_DEFAULT_OPTIONS="", NIRI_BACKEND="headless")
    username = "disposable-" + secrets.token_hex(8)
    password = secrets.token_hex(24)
    results = {"date": time.strftime("%Y-%m-%d"), "phases": {}}
    children = []
    logs = []
    evidence = Path(os.environ.get("NIRI_RDP_LAB_EVIDENCE", "/work/artifacts/plan3-p3/evidence"))
    with tempfile.TemporaryDirectory(prefix="niri-microphone-", dir="/tmp") as folder:
        private = Path(folder)

        def log_file(name):
            handle = (private / name).open("a+")
            logs.append(handle)
            return handle

        def launch(argv, environment, name, **arguments):
            process = subprocess.Popen(argv, env=environment, stdout=arguments.pop("stdout", log_file(name)),
                                       stderr=subprocess.STDOUT, start_new_session=True, **arguments)
            children.append(process)
            return process

        def text(name):
            path = private / name
            return path.read_text(errors="replace") if path.exists() else ""

        try:
            library = ROOT / "lab/sysroot/usr/lib"
            flags = subprocess.run(["pkg-config", "--cflags", "--libs", "freerdp-client3", "freerdp3", "winpr3"],
                                   env=dict(base, PKG_CONFIG_PATH=str(library / "pkgconfig")),
                                   capture_output=True, text=True, check=True).stdout.split()
            client_binary = private / "rdp-audio-client"
            subprocess.run(["cc", "-std=c17", "-O2", "-Wall", "-Wextra", "-Werror", "-Wno-deprecated-declarations",
                            str(ROOT / "lab/rdp-audio-client.c"), "-o", str(client_binary), *flags,
                            "-Wl,-rpath-link," + str(library)], check=True)
            subprocess.run(["openssl", "req", "-config", "/dev/null", "-x509", "-newkey", "rsa:2048", "-nodes",
                            "-days", "1", "-subj", "/CN=localhost", "-keyout", str(private / "key.pem"),
                            "-out", str(private / "cert.pem")], check=True, capture_output=True)
            (private / "key.pem").chmod(0o600)

            niri = launch([str(ROOT / os.environ.get("NIRI_RDP_LAB_NIRI", "compositor/upstream/target/debug/niri")),
                           "--config", str(ROOT / "lab/headless.kdl")], base, "niri.log")
            wait_until(lambda: (runtime / "wayland-0").exists() or niri.poll() is not None, 15, "compositor")
            if niri.poll() is not None:
                raise RuntimeError("compositor exited at startup")

            # Session graph and microphone helper, as session-worker starts them.
            session = private / "session"
            session.mkdir(mode=0o700)
            session_env = dict(base, NIRI_RDP_RUNTIME_DIR=str(session), XDG_RUNTIME_DIR=str(session),
                               PIPEWIRE_RUNTIME_DIR=str(session), PIPEWIRE_REMOTE="pipewire-0",
                               PULSE_SERVER="unix:" + str(session / "pulse/native"),
                               XDG_CONFIG_HOME=str(session / "config"), XDG_CACHE_HOME=str(session / "cache"),
                               XDG_DATA_HOME=str(session / "data"), GIO_USE_VFS="local")
            # No session bus exists in the namespace; the graph does not need one.
            graph = launch(["python3", str(ROOT / "broker/config/session-audio")],
                           session_env, "session-audio.log")
            wait_until(lambda: (session / "audio-0").is_socket() or graph.poll() is not None, 20, "session audio")
            if graph.poll() is not None:
                raise RuntimeError("session-audio exited: " + text("session-audio.log"))

            def start_helper():
                helper = launch(["python3", str(ROOT / "broker/config/session-microphone")], session_env,
                                "session-microphone.log")
                wait_until(lambda: (session / "audio-in-0").is_socket() or helper.poll() is not None, 10,
                           "microphone helper socket")
                if helper.poll() is not None:
                    raise RuntimeError("session-microphone exited: " + text("session-microphone.log"))
                return helper
            helper = start_helper()
            first_helper_pid = helper.pid

            # Client graph: the client's speakers and its microphone signal.
            client = private / "client"
            client.mkdir(mode=0o700)
            configuration = graph_configuration(client, (("client-speaker", "FL FR"), ("client-mic-sink", "MONO")))
            client_env = dict(base, XDG_RUNTIME_DIR=str(client), PIPEWIRE_RUNTIME_DIR=str(client),
                              PIPEWIRE_REMOTE="pipewire-0", XDG_STATE_HOME=str(client / "state"),
                              PULSE_SERVER="unix:" + str(client / "pulse/native"), HOME=str(client))
            # Only the daemons read the generated configuration; clients keep the defaults.
            daemon_env = dict(client_env, PIPEWIRE_CONFIG_DIR=str(configuration),
                              WIREPLUMBER_CONFIG_DIR=str(configuration))
            launch(["bash", "-c",
                    'pipewire & until [ -S "$XDG_RUNTIME_DIR/pipewire-0" ]; do sleep 0.05; done; '
                    'pipewire-pulse & wireplumber --profile=remote & wait'], daemon_env, "client-graph.log")

            def client_sinks():
                listing = subprocess.run(["pactl", "list", "short", "sinks"], env=client_env,
                                         capture_output=True, text=True).stdout
                return "client-speaker" in listing and "client-mic-sink" in listing
            wait_until(client_sinks, 20, "client graph")

            mic_cat = launch(["pw-cat", "--playback", "--raw", "--format=s16", "--rate=48000", "--channels=1",
                              "--latency=10ms", "--target=client-mic-sink", "-"], client_env, "client-tone.log",
                             stdin=subprocess.PIPE, pipesize=16384)
            mic_tone = ToneWriter(mic_cat, MIC_TONE, 1, MIC_BURST_SECONDS)
            mic_tone.start()
            play_cat = launch(["pw-cat", "--playback", "--raw", "--format=s16", "--rate=48000", "--channels=2",
                               "--latency=10ms", "-"], session_env, "session-tone.log",
                              stdin=subprocess.PIPE, pipesize=16384)
            play_tone = ToneWriter(play_cat, PLAYBACK_TONE, 2, PLAYBACK_BURST_SECONDS)
            play_tone.start()
            # Reference: the microphone signal as the client graph presents it,
            # which splits the latency into client and frontend/session parts.
            mic_reference = Recorder(["parec", "--device=client-mic-sink.monitor", "--raw", "--format=s16le",
                                      "--rate=48000", "--channels=1", "--latency-msec=10"], client_env, 1,
                                     log_file("client-mic-reference.log"))
            wait_until(mic_reference.first_tone_ns, 5, "client microphone reference")
            watch = ClientCaptureWatch(client_env, log_file("client-watch.log"))
            if watch.ignore != 1:
                raise RuntimeError("expected only the reference stream before connecting")
            broker = MockBroker(runtime, username, password, audio_runtime=session)

            def start_bridge(mode, name):
                bridge_env = dict(base, NIRI_RDP_MODE="broker", NIRI_RDP_CERT=str(private / "cert.pem"),
                                  NIRI_RDP_KEY=str(private / "key.pem"), NIRI_RDP_ENCODER="bitmap",
                                  NIRI_RDP_MICROPHONE=mode)
                bridge = launch([str(ROOT / os.environ.get("NIRI_RDP_LAB_BRIDGE",
                                                           "bridge/target/debug/niri-rdp-bridge"))],
                                bridge_env, name)

                def listening():
                    if bridge.poll() is not None:
                        raise RuntimeError("bridge exited at startup: " + text(name))
                    try:
                        with socket.create_connection(("127.0.0.1", 33991), timeout=0.1):
                            return True
                    except OSError:
                        return False
                wait_until(listening, 15, "frontend listener")
                return bridge

            connections = [0]

            def connect(bridge_log, microphone=True):
                connections[0] += 1
                name = f"client-{connections[0]}.log"
                negotiated = text(bridge_log).count("RDP audio PCM")
                process = launch([str(client_binary)], dict(
                    client_env, WLOG_LEVEL="WARN", NIRI_RDP_TEST_USERNAME=username, NIRI_RDP_TEST_PASSWORD=password,
                    NIRI_RDP_TEST_SPEAKER="client-speaker",
                    NIRI_RDP_TEST_MICROPHONE="client-mic-sink.monitor" if microphone else ""), name)
                wait_until(lambda: "NIRI_RDP_AUDIO_CLIENT_CONNECTED" in text(name) or process.poll() is not None,
                           20, "RDP connection")
                if process.poll() is not None:
                    raise RuntimeError("client failed: " + text(name))
                wait_until(lambda: text(bridge_log).count("RDP audio PCM") > negotiated, 15,
                           "RDPSND PCM negotiation")
                return process

            def summaries(bridge_log):
                return [line for line in text(bridge_log).splitlines() if "RDP connection summary" in line]

            def disconnect(process, bridge_log, sig=signal.SIGTERM):
                before = len(summaries(bridge_log))
                stop(process, sig)
                wait_until(lambda: len(summaries(bridge_log)) > before, 15, "disconnect summary")
                line = summaries(bridge_log)[-1]
                return dict(field.split("=", 1) for field in line.split("summary ", 1)[1].split() if "=" in field)

            def session_sources():
                return subprocess.run(["pactl", "list", "short", "sources"], env=session_env,
                                      capture_output=True, text=True).stdout

            def record_microphone(name, seconds, *, expect_open=True):
                started = time.monotonic_ns()
                recorder = Recorder(["pw-record", "--raw", "--format=s16", "--rate=48000", "--channels=1",
                                     "--latency=10ms", "-"], session_env, 1, log_file(name + ".log"))
                try:
                    opened = wait_until(lambda: watch.change_after(started, True), 10,
                                        "client capture to open") if expect_open else None
                    tone_at = wait_until(recorder.first_tone_ns, 10, "first microphone tone") if expect_open else None
                    time.sleep(seconds)
                finally:
                    stopped = time.monotonic_ns()
                    chunks = recorder.finish()
                    save_wave(evidence / (name + ".wav"), chunks, 1)
                analysis = analyze(chunks, 1, MIC_TONE, mic_tone.onsets, other=PLAYBACK_TONE)
                window = [(at, data) for at, data in list(mic_reference.chunks) if started <= at <= stopped]
                reference = analyze(window, 1, MIC_TONE, mic_tone.onsets)
                analysis["client_side_latency_ms"] = reference.get("latency_ms")
                analysis["client_side_glitches"] = reference.get("glitches")
                if expect_open:
                    analysis["client_capture_open_ms"] = round((opened - started) / 1e6, 1)
                    analysis["first_tone_ms"] = round((tone_at - started) / 1e6, 1)
                return analysis, stopped

            def wait_closed(stopped, bridge_log, before):
                closed = wait_until(lambda: watch.change_after(stopped, False), IDLE_CLOSE_LIMIT + 4,
                                    "client capture to close")
                return {"client_capture_closed_after_s": round((closed - stopped) / 1e9, 2),
                        "bridge_stop_logged": text(bridge_log).count("RDP microphone stopping") > before}

            def playback_recorder(name):
                return Recorder(["parec", "--device=client-speaker.monitor", "--raw", "--format=s16le",
                                 "--rate=48000", "--channels=2", "--latency-msec=10"], client_env, 2,
                                log_file(name + ".log"))

            # Phase A: playback only, microphone off in the frontend.
            bridge = start_bridge("off", "bridge-off.log")
            rdp = connect("bridge-off.log")
            time.sleep(2)
            recorder = playback_recorder("playback-a")
            # Reference: the same tone at the session sink, before RDP.
            reference = Recorder(["parec", "--device=niri-rdp-sink.monitor", "--raw", "--format=s16le",
                                  "--rate=48000", "--channels=2", "--latency-msec=10"], session_env, 2,
                                 log_file("playback-a-source.log"))
            time.sleep(25)
            source_chunks = reference.finish()
            chunks = recorder.finish()
            save_wave(evidence / "playback-a-source.wav", source_chunks, 2)
            save_wave(evidence / "playback-a-client.wav", chunks, 2)
            playback_source = analyze(source_chunks, 2, PLAYBACK_TONE, play_tone.onsets, other=MIC_TONE)
            playback_a = analyze(chunks, 2, PLAYBACK_TONE, play_tone.onsets, other=MIC_TONE)
            summary_a = disconnect(rdp, "bridge-off.log")
            stop(bridge)
            phase = {"playback": playback_a, "playback_at_session_sink": playback_source, "summary": summary_a,
                     "client_capture_streams": watch.current(),
                     "microphone_log_lines": text("bridge-off.log").count("RDP microphone")}
            results["phases"]["A_playback_only_microphone_off"] = phase
            require_tone("A playback", playback_a, PLAYBACK_TONE, 20, latency=False)
            if summary_a.get("mic") != "none" or summary_a.get("audio_codec") != "pcm" or phase["client_capture_streams"]:
                raise RuntimeError("phase A: microphone must stay off and playback must use PCM: " + str(phase))

            # Phase B: microphone and playback together; idle close and reopen.
            bridge = start_bridge("auto", "bridge.log")
            rdp = connect("bridge.log")
            wait_until(lambda: "niri-rdp-microphone" in session_sources(), 10, "remote microphone source")
            appeared = time.monotonic()

            def default_source():
                return subprocess.run(["pactl", "get-default-source"], env=session_env,
                                      capture_output=True, text=True).stdout.strip()
            try:
                wait_until(lambda: default_source() == "niri-rdp-microphone", 5, "default source")
            except RuntimeError:
                pass
            default_after = round(time.monotonic() - appeared, 2)
            default_source = default_source()
            description = subprocess.run(["pactl", "list", "sources"], env=session_env,
                                         capture_output=True, text=True).stdout
            default_sink = subprocess.run(["pactl", "get-default-sink"], env=session_env,
                                          capture_output=True, text=True).stdout.strip()
            time.sleep(2)
            idle_streams = watch.current()
            recorder = playback_recorder("playback-b")
            mic_b1, stopped = record_microphone("mic-b1", 10)
            stops = text("bridge.log").count("RDP microphone stopping")
            closing = wait_closed(stopped, "bridge.log", stops)
            mic_b2, stopped = record_microphone("mic-b2", 6)
            closing_b2 = wait_closed(stopped, "bridge.log", stops + 1)
            chunks = recorder.finish()
            save_wave(evidence / "playback-b-client.wav", chunks, 2)
            playback_b = analyze(chunks, 2, PLAYBACK_TONE, play_tone.onsets, other=MIC_TONE)
            summary_b = disconnect(rdp, "bridge.log")
            results["phases"]["B_microphone_with_playback"] = {
                "default_source": default_source, "default_source_after_s": default_after,
                "default_sink": default_sink,
                "described": "Remote microphone (RDP)" in description,
                "client_capture_streams_before_recorder": idle_streams,
                "microphone_first": mic_b1, "idle_close": closing, "microphone_reopened": mic_b2,
                "idle_close_after_reopen": closing_b2, "playback": playback_b, "summary": summary_b}
            if default_source != "niri-rdp-microphone" or default_sink != "niri-rdp-sink" or \
                    "Remote microphone (RDP)" not in description or idle_streams:
                raise RuntimeError("phase B setup: " + json.dumps(results["phases"]["B_microphone_with_playback"]))
            require_tone("B microphone", mic_b1, MIC_TONE, 8, glitches_per_minute=MIC_SETTLED_GLITCHES_PER_MINUTE)
            require_tone("B microphone reopened", mic_b2, MIC_TONE, 4, glitches_per_minute=MIC_SETTLED_GLITCHES_PER_MINUTE)
            no_creep("B microphone reopened", mic_b2, mic_b1)
            require_tone("B playback", playback_b, PLAYBACK_TONE, 15, latency=False)
            same_playback("B playback with microphone", playback_b, playback_a)
            for close in (closing, closing_b2):
                if close["client_capture_closed_after_s"] > IDLE_CLOSE_LIMIT or not close["bridge_stop_logged"]:
                    raise RuntimeError("phase B: capture must close about 5 s after the recorder: " + str(close))
            if summary_b.get("mic") != "pcm_48000_mono" or summary_b.get("audio_codec") != "pcm" or \
                    int(summary_b.get("mic_opens", 0)) != 2 or int(summary_b.get("mic_errors", 1)):
                raise RuntimeError("phase B summary: " + str(summary_b))

            # Phase C: reconnect, then an abrupt client kill.
            rdp = connect("bridge.log")
            wait_until(lambda: "niri-rdp-microphone" in session_sources(), 10, "remote microphone source")
            # A longer run shows whether the low-delay jitter buffer settles.
            mic_c, _ = record_microphone("mic-c", 30)
            summary_c = disconnect(rdp, "bridge.log", signal.SIGKILL)
            results["phases"]["C_reconnect_and_abrupt_kill"] = {"microphone": mic_c, "summary": summary_c}
            require_tone("C microphone after reconnect", mic_c, MIC_TONE, 20, glitches_per_minute=MIC_SETTLED_GLITCHES_PER_MINUTE)
            no_creep("C microphone after reconnect", mic_c, mic_b1)

            # Phase D: kill the helper while recording and playing.
            rdp = connect("bridge.log")
            wait_until(lambda: "niri-rdp-microphone" in session_sources(), 10, "remote microphone source")
            recorder = playback_recorder("playback-d")
            mic_recorder = Recorder(["pw-record", "--raw", "--format=s16", "--rate=48000", "--channels=1",
                                     "--latency=10ms", "-"], session_env, 1, log_file("mic-d.log"))
            wait_until(mic_recorder.first_tone_ns, 10, "microphone tone before the helper kill")
            time.sleep(3)
            helper_alive_before_kill = helper.poll() is None and helper.pid == first_helper_pid
            killed_at = time.monotonic_ns()
            os.kill(helper.pid, signal.SIGKILL)
            helper.wait(timeout=5)
            wait_until(lambda: "RDP microphone helper connection ended" in text("bridge.log"), 5,
                       "frontend to notice the helper loss")
            time.sleep(6)
            client_alive = rdp.poll() is None
            mic_recorder.finish()
            chunks = recorder.finish()
            save_wave(evidence / "playback-d-client.wav", chunks, 2)
            after = [(at, data) for at, data in chunks if at > killed_at]
            playback_d = analyze(after, 2, PLAYBACK_TONE, play_tone.onsets, other=MIC_TONE)
            playback_d_all = analyze(chunks, 2, PLAYBACK_TONE, play_tone.onsets, other=MIC_TONE)
            # session-worker retries the helper after about one second.
            time.sleep(1)
            helper = start_helper()
            summary_d = disconnect(rdp, "bridge.log")
            rdp = connect("bridge.log")
            wait_until(lambda: "niri-rdp-microphone" in session_sources(), 10, "recovered microphone source")
            mic_d, _ = record_microphone("mic-d-recovered", 6)
            summary_d2 = disconnect(rdp, "bridge.log")
            results["phases"]["D_helper_kill"] = {
                "client_connected_after_kill": client_alive,
                "first_helper_survived_phases_a_to_c": helper_alive_before_kill, "playback_after_kill": playback_d,
                "playback_whole_window": playback_d_all, "summary_during_kill": summary_d,
                "microphone_after_restart_and_reconnect": mic_d, "summary_recovered": summary_d2}
            if not client_alive or not helper_alive_before_kill:
                raise RuntimeError("phase D: the RDP session must survive the helper kill")
            require_tone("D playback after helper kill", playback_d, PLAYBACK_TONE, 5, latency=False)
            same_playback("D playback after helper kill", playback_d, playback_a)
            require_tone("D microphone recovered", mic_d, MIC_TONE, 4, glitches_per_minute=MIC_SETTLED_GLITCHES_PER_MINUTE)
            no_creep("D microphone recovered", mic_d, mic_b1)
            if int(summary_d.get("mic_errors", 0)) < 1 or summary_d.get("audio_codec") != "pcm":
                raise RuntimeError("phase D summary: " + str(summary_d))
            stop(bridge)

            helper_log = text("session-microphone.log")
            results["helper"] = {
                "tracebacks": helper_log.count("Traceback"),
                "frontend_lines": [line for line in helper_log.splitlines() if "frontend" in line]}
            results["tone_writer_drops"] = {"microphone": mic_tone.dropped, "playback": play_tone.dropped}
            results["bridge_microphone_lines"] = [line for line in text("bridge.log").splitlines()
                                                  if "microphone" in line.lower()]
            if results["helper"]["tracebacks"]:
                raise RuntimeError("session-microphone crashed:\n" + helper_log)
            results["result"] = "passed"
        except Exception as error:
            results["result"] = "failed"
            results["error"] = str(error)
            raise
        finally:
            for process in reversed(children):
                stop(process)
            for handle in logs:
                handle.close()
            evidence.mkdir(parents=True, exist_ok=True)
            for path in private.glob("*.log"):
                shutil.copy(path, evidence / path.name)
            report = Path(os.environ.get("NIRI_RDP_LAB_REPORT", str(evidence.parent / "report.json")))
            report.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
