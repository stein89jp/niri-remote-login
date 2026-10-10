#!/usr/bin/python3
"""Authorize a remote-login account and prepare the host's TLS certificate."""
import argparse
import os
import pwd
import re
import subprocess


def account_name(value):
    if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", value):
        raise argparse.ArgumentTypeError("use a local Linux account name (no domain prefix)")
    return value


def authorize(name, create=False):
    try:
        account = pwd.getpwnam(name)
    except KeyError:
        account = None
    if create:
        if account is not None:
            raise ValueError(f"account {name} already exists; use --user {name}")
        subprocess.run(["/usr/bin/useradd", "-m", "-s", "/bin/bash", "--", name], check=True)
        subprocess.run(["/usr/bin/passwd", "--", name], check=True)
        account = pwd.getpwnam(name)
    if account is None:
        raise ValueError(f"account {name} does not exist; use --create-user {name}")
    if account.pw_uid < 1000 or name == "niri-rdp" or not account.pw_shell or \
            "nologin" in account.pw_shell or account.pw_shell in ("/bin/false", "/usr/bin/false"):
        raise ValueError("choose a normal login account with UID >= 1000")
    subprocess.run(["/usr/bin/systemd-sysusers", "/usr/lib/sysusers.d/niri-rdp.conf"], check=True)
    subprocess.run(["/usr/bin/usermod", "-aG", "niri-rdp-users", "--", name], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    accounts = parser.add_mutually_exclusive_group()
    accounts.add_argument("--user", type=account_name, help="authorize an existing account")
    accounts.add_argument("--create-user", type=account_name, help="create an account and prompt for its password")
    parser.add_argument("--hostname", help="DNS name or IP for a new certificate")
    arguments = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run with sudo")
    name = arguments.user or arguments.create_user or os.environ.get("SUDO_USER")
    if not name:
        parser.error("specify --user or --create-user")
    try:
        account_name(name)
        authorize(name, create=arguments.create_user is not None)
        command = ["/usr/lib/niri-rdp/prepare-tls"]
        if arguments.hostname:
            command.extend(["--hostname", arguments.hostname])
        subprocess.run(command, check=True)
        fingerprint = subprocess.check_output([
            "/usr/bin/openssl", "x509", "-in", "/etc/niri-rdp/tls/cert.pem",
            "-noout", "-fingerprint", "-sha256",
        ], text=True).strip()
    except (ValueError, argparse.ArgumentTypeError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Setup failed: {error}\n")
    print(f"Account {name} is authorized for Niri remote login.")
    print(fingerprint)
    print("Start at boot: sudo systemctl enable --now niri-rdp-frontend.service")
    print("Connect with Windows Remote Desktop to HOST:33991 using this Linux account.")
    print("Log out of this account's local graphical desktop before connecting remotely.")


if __name__ == "__main__":
    main()
