"""Disposable namespace-only NRB2 fixture; never authenticates Linux accounts."""
import array
import socket
import struct
import threading
from pathlib import Path


def read_exact(stream, count):
    data = b""
    while len(data) < count:
        chunk = stream.recv(count - len(data))
        if not chunk:
            raise EOFError("fixture request ended early")
        data += chunk
    return data


class MockBroker:
    def __init__(self, runtime, username, password, audio_runtime=None):
        if Path(__file__).resolve().parents[1] != Path("/work"):
            raise RuntimeError("mock broker may run only in lab/run-isolated.sh")
        Path("/run/niri-rdp").mkdir(mode=0o711, exist_ok=True)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind("/run/niri-rdp/broker.sock")
        self.listener.listen(4)
        self.runtime, self.username, self.password = runtime, username, password
        # Like the root broker: playback audio-0, and audio-in-0 only after it.
        self.audio_runtime = audio_runtime
        self.accepted = self.closed = self.rejected = 0
        self.descriptor_counts = []
        self.lock = threading.Lock()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            stream, _ = self.listener.accept()
            threading.Thread(target=self.handle, args=(stream,), daemon=True).start()

    def handle(self, stream):
        with stream:
            stream.settimeout(30)
            # Wire contract v2: the header ends with the client address family.
            magic, user_length, pass_length, family = struct.unpack("!4sHHB", read_exact(stream, 9))
            if (magic != b"NRB2" or not 1 <= user_length <= 64 or not 1 <= pass_length <= 512
                    or family not in (0, 4, 6)):
                stream.sendall(b"NRB2\x03")
                return
            read_exact(stream, {0: 0, 4: 4, 6: 16}[family])
            user = read_exact(stream, user_length).decode()
            password = read_exact(stream, pass_length).decode()
            if (user, password) != (self.username, self.password):
                with self.lock:
                    self.rejected += 1
                stream.sendall(b"NRB2\x01")
                return
            handed = []
            try:
                desktop = socket.socket(socket.AF_UNIX)
                handed.append(desktop)
                desktop.connect(str(self.runtime / "wayland-0"))
                for name in ("audio-0", "audio-in-0") if self.audio_runtime else ():
                    session = socket.socket(socket.AF_UNIX)
                    try:
                        session.connect(str(Path(self.audio_runtime) / name))
                    except OSError:
                        session.close()
                        break
                    handed.append(session)
                stream.sendmsg([b"NRB2\x00"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                               array.array("i", [s.fileno() for s in handed]))])
            finally:
                for session in handed:
                    session.close()
            with self.lock:
                self.accepted += 1
                self.descriptor_counts.append(len(handed))
            # The lease lasts as long as the RDP connection, however long.
            stream.settimeout(None)
            while stream.recv(1):
                pass
            with self.lock:
                self.closed += 1
