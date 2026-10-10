#!/usr/bin/python3
"""Create the service's first TLS identity; preserve existing certificates."""
import argparse
import fcntl
import ipaddress
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import tempfile


TLS_DIRECTORY = Path("/etc/niri-rdp/tls")


def run_openssl(*arguments):
    return subprocess.run(
        ["/usr/bin/openssl", *map(str, arguments)], check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60,
    ).stdout


def identity_names(hostname):
    """Validate the name before inserting it into OpenSSL's subject/SAN syntax."""
    try:
        address = ipaddress.ip_address(hostname)
        return str(address), f"IP:{address},DNS:localhost,IP:127.0.0.1"
    except ValueError:
        pass
    hostname = hostname.rstrip(".")
    labels = hostname.split(".")
    if not (1 <= len(hostname) <= 253) or any(
        not re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?", label)
        for label in labels
    ):
        raise ValueError("use a DNS hostname or numeric IP address for the certificate")
    names = [hostname, hostname.split(".")[0], "localhost"]
    san = ",".join(f"DNS:{name}" for name in dict.fromkeys(names))
    return hostname, san + ",IP:127.0.0.1"


def regular_owned(path, owner_uid, private=False):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != owner_uid:
        raise ValueError(f"{path} must be a regular file owned by root")
    if info.st_mode & (0o077 if private else 0o022):
        raise ValueError(f"unsafe permissions on {path}; use chmod {'600' if private else '644'}")


def ensure_tls(directory=TLS_DIRECTORY, hostname=None, owner_uid=0):
    directory = Path(directory)
    hostname, san = identity_names(hostname or socket.gethostname())
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != owner_uid or info.st_mode & 0o077:
        raise ValueError(f"{directory} must be a root-owned directory with mode 700")
    lock_fd = os.open(directory / ".prepare.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, "w") as lock:
        regular_owned(directory / ".prepare.lock", owner_uid, private=True)
        fcntl.flock(lock, fcntl.LOCK_EX)
        cert, key = directory / "cert.pem", directory / "key.pem"
        exists = [path.exists() or path.is_symlink() for path in (cert, key)]
        if any(exists) and not all(exists):
            raise ValueError("incomplete TLS identity: provide both cert.pem and key.pem; existing files were preserved")
        created = not any(exists)
        if created:
            with tempfile.TemporaryDirectory(prefix=".new-", dir=directory) as temporary:
                temporary = Path(temporary)
                new_cert, new_key = temporary / "cert.pem", temporary / "key.pem"
                run_openssl(
                    "req", "-x509", "-newkey", "rsa:3072", "-nodes", "-days", "365",
                    "-subj", f"/CN={hostname}", "-addext", f"subjectAltName={san}",
                    "-keyout", new_key, "-out", new_cert,
                )
                new_key.chmod(0o600)
                new_cert.chmod(0o644)
                new_key.rename(key)
                new_cert.rename(cert)
        regular_owned(cert, owner_uid)
        regular_owned(key, owner_uid, private=True)
        try:
            run_openssl("x509", "-in", cert, "-noout", "-checkend", "0")
            cert_public = run_openssl("x509", "-in", cert, "-pubkey", "-noout")
            key_public = run_openssl("pkey", "-in", key, "-pubout", "-passin", "pass:")
            if cert_public != key_public:
                raise ValueError("TLS certificate and private key do not match")
        except subprocess.CalledProcessError as error:
            raise ValueError("TLS identity is invalid, expired, or uses an encrypted key; existing files were preserved") from error
        return created


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hostname", help="certificate DNS name or IP, used only for a new identity")
    arguments = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run as root (the broker service invokes this automatically)")
    try:
        created = ensure_tls(hostname=arguments.hostname)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"TLS setup failed: {error}\n")
    print("Created initial self-signed TLS certificate" if created else "Existing TLS identity verified")


if __name__ == "__main__":
    main()
