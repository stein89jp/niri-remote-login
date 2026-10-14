#!/usr/bin/python3
"""Blackhole the actual Rust transport in an unshared user/network namespace."""
import argparse
import errno
import json
import os
from pathlib import Path
import selectors
import socket
import subprocess
import sys
import time


def line(process, timeout=40):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        if not selector.select(timeout):
            raise RuntimeError("transport fixture did not meet its liveness deadline")
    data = process.stdout.readline().strip()
    if not data:
        raise RuntimeError("transport fixture exited: " + process.stderr.read())
    return data


def firewall(port):
    rules = ("table inet niri_rdp_test {\n chain output {\n"
             "  type filter hook output priority 0; policy accept;\n"
             f"  tcp sport {port} drop\n  tcp dport {port} drop\n" + " }\n}\n")
    result = subprocess.run(["nft", "-f", "-"], input=rules, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError("private firewall fixture failed: " + result.stderr)


def clear_firewall():
    subprocess.run(["nft", "delete", "table", "inet", "niri_rdp_test"],
                   check=True, capture_output=True)


def scenario(binary, busy):
    environment = dict(os.environ)
    if busy:
        environment["NIRI_RDP_NETWORK_PROBE_BUSY"] = "1"
    process = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, bufsize=1, env=environment)
    try:
        address = line(process).split()[1]
        host, port = address.split(":")
        port = int(port)
        with socket.create_connection((host, port)) as client:
            client.settimeout(3)
            assert line(process) == "ACCEPTED"
            if not busy:
                # A genuinely quiet client survives several keepalive exchanges.
                time.sleep(22)
                client.sendall(b"H")
                assert client.recv(1) == b"H"
                firewall(port)
                time.sleep(2)
                clear_firewall()
                client.sendall(b"H")
                assert client.recv(1) == b"H", "short stall must retain the connection"
            else:
                assert len(client.recv(256)) > 0
            firewall(port)
            began = time.monotonic()
            receipt = line(process)
            elapsed = time.monotonic() - began
            assert receipt.startswith("DEAD "), receipt
            assert int(receipt.split()[2]) == errno.ETIMEDOUT, receipt
            assert 10 <= elapsed <= 35, (elapsed, receipt)
            clear_firewall()
        with socket.create_connection((host, port)) as resumed:
            resumed.settimeout(3)
            resumed.sendall(b"R")
            assert resumed.recv(1) == b"R"
        assert line(process) == "RECONNECTED"
        assert process.wait(timeout=3) == 0
        return {"traffic": "busy" if busy else "quiet", "blackhole_detection_s": round(elapsed, 3),
                "reconnect": True, "healthy_idle_s": 0 if busy else 22,
                "short_stall_s": 0 if busy else 2, "physical_presentation_measured": False}
    finally:
        if process.poll() is None:
            process.terminate()
        process.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--parent-netns")
    args = parser.parse_args()
    # Record the actual parent namespace before unsharing; /proc/1/ns may be
    # unreadable from a user namespace. Firewall commands run only in the child.
    current_namespace = str(Path("/proc/self/ns/net").readlink())
    if args.parent_netns is None:
        command = ["unshare", "-Urn", sys.executable, str(Path(__file__).resolve()),
                   str(args.binary.resolve()), "--parent-netns", current_namespace]
        if args.report:
            command += ["--report", str(args.report.resolve())]
        subprocess.run(command, check=True)
        return
    assert current_namespace != args.parent_netns, "an unshared network namespace is required"
    subprocess.run(["ip", "link", "set", "lo", "up"], check=True)
    results = [scenario(args.binary.resolve(), busy) for busy in [False, True]]
    rendered = json.dumps({"transport_source": "bridge/src/network.rs", "results": results}, indent=2)
    print(rendered)
    if args.report:
        args.report.write_text(rendered + "\n")


if __name__ == "__main__":
    main()
