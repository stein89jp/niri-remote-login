#!/usr/bin/env python3
"""Manual broker-only smoke client for an installed disposable Arch test machine.

Never install/run the privileged broker on the working host for this test.
Passwords are prompted; never put passwords in argv, files or logs.
"""
import argparse
import array
import getpass
import os
import socket
import struct
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("username")
    parser.add_argument("--expect", choices=("success", "reject"), required=True)
    parser.add_argument("--seconds", type=int, default=5)
    args = parser.parse_args()
    if not 0 <= args.seconds <= 300:
        parser.error("seconds must be 0..300")
    username = args.username.encode("utf-8")
    password = getpass.getpass("Arch account password: ").encode("utf-8")
    if not 1 <= len(username) <= 64 or not 1 <= len(password) <= 512 or b"\0" in password:
        parser.error("credentials exceed broker bounds")
    descriptors = []
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as control:
            control.settimeout(40)
            control.connect("/run/niri-rdp/broker.sock")
            control.sendall(b"NRB1" + struct.pack("!HH", len(username), len(password)) + username + password)
            # Python cannot reliably erase immutable string copies: use this
            # smoke tool only for disposable credentials, not production auth.
            del password
            data, ancillary, flags, _ = control.recvmsg(5, socket.CMSG_SPACE(4), socket.MSG_CMSG_CLOEXEC)
            for level, kind, raw in ancillary:
                if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                    fds = array.array("i")
                    fds.frombytes(raw[:len(raw) - len(raw) % fds.itemsize])
                    descriptors.extend(fds)
            while len(data) < 5:
                chunk = control.recv(5 - len(data))
                if not chunk:
                    raise RuntimeError("truncated broker response")
                data += chunk
            if flags & socket.MSG_CTRUNC or data[:4] != b"NRB1":
                raise RuntimeError("malformed broker response")
            status = data[4]
            if args.expect == "reject":
                if status != 1 or descriptors:
                    raise RuntimeError(f"expected auth rejection; got status {status}")
                print("PASS credentials/account policy rejected; no desktop FD returned")
            else:
                if status != 0 or len(descriptors) != 1:
                    raise RuntimeError(f"expected session with one Wayland FD; got status {status}")
                print("PASS broker opened session and returned a Wayland socket; closing control after hold")
                time.sleep(args.seconds)
        print("Control socket closed; verify logind/process/runtime cleanup separately")
    finally:
        for fd in descriptors:
            os.close(fd)


if __name__ == "__main__":
    main()
