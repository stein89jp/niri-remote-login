"""Disposable namespace-only NRB1 fixture; never authenticates Linux accounts."""
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
    def __init__(self, runtime, username, password):
        if Path(__file__).resolve().parents[1] != Path("/work"):
            raise RuntimeError("mock broker may run only in lab/run-isolated.sh")
        Path("/run/niri-rdp").mkdir(mode=0o711, exist_ok=True)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind("/run/niri-rdp/broker.sock")
        self.listener.listen(4)
        self.runtime, self.username, self.password = runtime, username, password
        self.accepted = self.closed = self.rejected = 0
        self.lock = threading.Lock()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            stream, _ = self.listener.accept()
            threading.Thread(target=self.handle, args=(stream,), daemon=True).start()

    def handle(self, stream):
        with stream:
            stream.settimeout(30)
            magic, user_length, pass_length = struct.unpack("!4sHH", read_exact(stream, 8))
            if magic != b"NRB1" or not 1 <= user_length <= 64 or not 1 <= pass_length <= 512:
                stream.sendall(b"NRB1\x03")
                return
            user = read_exact(stream, user_length).decode()
            password = read_exact(stream, pass_length).decode()
            if (user, password) != (self.username, self.password):
                with self.lock:
                    self.rejected += 1
                stream.sendall(b"NRB1\x01")
                return
            with socket.socket(socket.AF_UNIX) as desktop:
                desktop.connect(str(self.runtime / "wayland-0"))
                stream.sendmsg([b"NRB1\x00"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                               array.array("i", [desktop.fileno()]))])
            with self.lock:
                self.accepted += 1
            while stream.recv(1):
                pass
            with self.lock:
                self.closed += 1
