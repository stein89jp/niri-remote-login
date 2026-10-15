#!/usr/bin/python3
"""Create, renew, or verify the service's TLS identity.

A self-signed identity this script generated is recorded in a sidecar marker
and renewed automatically shortly before it expires. Certificates provided by
an administrator are never replaced: they produce a warning near expiry and
fail startup once expired.
"""
import argparse
import fcntl
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import socket
import ssl
import stat
import subprocess
import sys
import tempfile


TLS_DIRECTORY = Path("/etc/niri-rdp/tls")
CERT_DAYS = 825
LEGACY_CERT_DAYS = 365
RENEW_BEFORE_SECONDS = 30 * 24 * 60 * 60
MARKER_NAME = "generated"
PENDING_NAME = ".pending"
# Commit order for a staged identity; the marker last, so an interrupted commit
# is always completed from the staged copy before the marker is consulted.
IDENTITY_FILES = ("key.pem", "cert.pem", MARKER_NAME)


def run_openssl(*arguments):
    return subprocess.run(
        ["/usr/bin/openssl", *map(str, arguments)], check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60,
    ).stdout


def report(message, warning=False):
    """Print a status line; under systemd, warnings get the journal's warning priority."""
    if warning:
        prefix = "<4>" if os.environ.get("JOURNAL_STREAM") else ""
        print(f"{prefix}warning: {message}", file=sys.stderr, flush=True)
    else:
        print(message, flush=True)


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


def certificate_fingerprint(cert):
    output = run_openssl("x509", "-in", cert, "-noout", "-fingerprint", "-sha256").decode()
    return output.strip().split("=", 1)[1].upper()


def certificate_end(cert):
    return run_openssl("x509", "-in", cert, "-noout", "-enddate").decode().strip().split("=", 1)[1]


def expires_within(cert, seconds):
    """True when the certificate expires within the given number of seconds."""
    result = subprocess.run(
        ["/usr/bin/openssl", "x509", "-in", str(cert), "-noout", "-checkend", str(int(seconds))],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60,
    )
    return result.returncode != 0


def fsync_directory(directory):
    descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def legacy_generated_hostname(cert):
    """Recognize an unmarked identity made by releases before renewal existed.

    Those releases wrote a 365-day, self-signed RSA-3072 certificate whose
    subject is exactly CN=<name> and whose SAN list is identity_names(<name>).
    """
    fields = {}
    lines = run_openssl(
        "x509", "-in", cert, "-noout", "-nameopt", "RFC2253",
        "-subject", "-issuer", "-startdate", "-enddate", "-ext", "subjectAltName",
    ).decode().splitlines()
    for line in lines:
        name, separator, value = line.partition("=")
        if separator and name in ("subject", "issuer", "notBefore", "notAfter"):
            fields[name] = value.strip()
    san = "".join(line.strip() for line in lines if line.startswith(" "))
    san = san.replace("IP Address:", "IP:").replace(" ", "")
    subject = fields.get("subject", "")
    if not subject.startswith("CN=") or fields.get("issuer") != subject:
        return None
    try:
        hostname, expected_san = identity_names(subject[3:])
        lifetime = ssl.cert_time_to_seconds(fields["notAfter"]) - ssl.cert_time_to_seconds(fields["notBefore"])
    except (KeyError, ValueError):
        return None
    text = run_openssl("x509", "-in", cert, "-noout", "-text").decode()
    if (hostname != subject[3:] or san != expected_san or lifetime != LEGACY_CERT_DAYS * 86400
            or "Public Key Algorithm: rsaEncryption" not in text or "Public-Key: (3072 bit)" not in text):
        return None
    return hostname


def generated_hostname(directory, cert, owner_uid):
    """Return the recorded hostname when the current certificate was generated here, else None."""
    marker = directory / MARKER_NAME
    try:
        if marker.exists() or marker.is_symlink():
            regular_owned(marker, owner_uid)
            try:
                record = json.loads(marker.read_text())
                fingerprint = record["sha256_fingerprint"]
                hostname = record.get("hostname") or ""
            except (ValueError, KeyError, TypeError, AttributeError):
                return None
            if not isinstance(fingerprint, str) or fingerprint.upper() != certificate_fingerprint(cert):
                return None
            return hostname if isinstance(hostname, str) else ""
        return legacy_generated_hostname(cert)
    except subprocess.CalledProcessError:
        return None


def finish_pending(directory, owner_uid):
    """Complete a staged identity commit that a crash or power loss interrupted."""
    pending = directory / PENDING_NAME
    try:
        info = pending.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != owner_uid:
        raise ValueError(f"{pending} must be a root-owned directory")
    for name in IDENTITY_FILES:
        source = pending / name
        if source.exists() or source.is_symlink():
            source.replace(directory / name)
    pending.rmdir()
    fsync_directory(directory)


def remove_abandoned_staging(directory):
    for path in directory.glob(".new-*"):
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path, ignore_errors=True)


def commit_new_identity(directory, hostname, san):
    """Generate a fresh identity in a staging directory, then move it into place.

    The staging directory becomes `.pending` with one atomic rename, after
    which each file replaces its live counterpart with an atomic rename. If
    anything interrupts the per-file renames, finish_pending() completes them
    on the next start, so the live key and certificate never stay mismatched.
    """
    temporary = Path(tempfile.mkdtemp(prefix=".new-", dir=directory))
    try:
        new_cert, new_key, new_marker = (temporary / name for name in ("cert.pem", "key.pem", MARKER_NAME))
        run_openssl(
            "req", "-x509", "-newkey", "rsa:3072", "-nodes", "-days", CERT_DAYS,
            "-subj", f"/CN={hostname}", "-addext", f"subjectAltName={san}",
            "-keyout", new_key, "-out", new_cert,
        )
        new_key.chmod(0o600)
        new_cert.chmod(0o644)
        fingerprint = certificate_fingerprint(new_cert)
        new_marker.write_text(json.dumps({"sha256_fingerprint": fingerprint, "hostname": hostname}) + "\n")
        new_marker.chmod(0o644)
        for path in (new_key, new_cert, new_marker):
            with open(path, "rb") as handle:
                os.fsync(handle.fileno())
        fsync_directory(temporary)
        temporary.rename(directory / PENDING_NAME)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    fsync_directory(directory)
    return fingerprint


def ensure_tls(directory=TLS_DIRECTORY, hostname=None, owner_uid=0, log=report,
               renew_before=RENEW_BEFORE_SECONDS):
    """Prepare the identity; return True when a new certificate was written."""
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
        finish_pending(directory, owner_uid)
        remove_abandoned_staging(directory)
        cert, key = directory / "cert.pem", directory / "key.pem"
        exists = [path.exists() or path.is_symlink() for path in (cert, key)]
        if any(exists) and not all(exists):
            raise ValueError("incomplete TLS identity: provide both cert.pem and key.pem; existing files were preserved")
        changed = not any(exists)
        generated = None
        if changed:
            fingerprint = commit_new_identity(directory, hostname, san)
            finish_pending(directory, owner_uid)
            log(f"Created self-signed TLS certificate for {hostname}; SHA-256 fingerprint {fingerprint}")
        regular_owned(cert, owner_uid)
        regular_owned(key, owner_uid, private=True)
        if not changed:
            generated = generated_hostname(directory, cert, owner_uid)
        if generated is not None and expires_within(cert, renew_before):
            try:
                renew_name, renew_san = identity_names(generated)
            except ValueError:
                renew_name, renew_san = hostname, san
            previous_end = certificate_end(cert)
            fingerprint = commit_new_identity(directory, renew_name, renew_san)
            finish_pending(directory, owner_uid)
            changed = True
            log(f"Renewed self-signed TLS certificate for {renew_name} (previous one expires {previous_end}); "
                f"new SHA-256 fingerprint {fingerprint}; clients will ask to trust the new certificate")
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
        if not changed:
            if generated is None and expires_within(cert, renew_before):
                log(f"TLS certificate {cert} expires {certificate_end(cert)}; it is not generated by Niri-RDP and "
                    "will not be renewed automatically; replace it before then or startup will fail", warning=True)
            log("Existing TLS identity verified")
        return changed


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--hostname", help="certificate DNS name or IP, used only for a new identity")
    arguments = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run as root (the broker service invokes this automatically)")
    try:
        ensure_tls(hostname=arguments.hostname)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"TLS setup failed: {error}\n")


if __name__ == "__main__":
    main()
