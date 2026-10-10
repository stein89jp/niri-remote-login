#!/usr/bin/env python3
"""Inspect/extract project packages in a workspace staging root; never install."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
EXPECTED = {
    "niri-rdp-headless": {"usr/lib/niri-rdp/niri-headless", "usr/share/licenses/niri-rdp-headless/LICENSE",
                          "usr/share/licenses/niri-rdp-headless/THIRD-PARTY-NOTICES.txt"},
    "niri-remote-login": {
        "usr/lib/niri-rdp/niri-rdp-bridge", "usr/lib/niri-rdp/niri-rdp-broker",
        "usr/lib/niri-rdp/encoder-probe",
        "usr/lib/niri-rdp/session-worker", "etc/pam.d/niri-rdp", "etc/niri-rdp/headless.kdl",
        "etc/niri-rdp/frontend.conf", "etc/niri-rdp/renderer.conf", "usr/lib/systemd/system/niri-rdp-broker.service",
        "usr/lib/systemd/system/niri-rdp-frontend.service", "usr/lib/sysusers.d/niri-rdp.conf",
        "usr/share/doc/niri-remote-login/install.md", "usr/share/doc/niri-remote-login/uninstall.md",
        "usr/share/doc/niri-remote-login/upgrade.md", "usr/share/doc/niri-remote-login/acceptance.md",
        "usr/share/doc/niri-remote-login/nvidia-frontend.conf",
        "usr/share/licenses/niri-remote-login/LICENSE", "usr/share/licenses/niri-remote-login/THIRD-PARTY-NOTICES.txt",
    },
}


def inspect(package):
    # Use the platform's zstd reader; inspect metadata before any extraction.
    decompressor = subprocess.Popen(["zstd", "-dc", str(package)], stdout=subprocess.PIPE)
    files = set()
    with tarfile.open(fileobj=decompressor.stdout, mode="r|") as archive:
        for member in archive:
            path = Path(member.name)
            if path.is_absolute() or ".." in path.parts or member.issym() or member.islnk():
                raise RuntimeError(f"unsafe package entry: {member.name}")
            if member.mode & 0o6000:
                raise RuntimeError(f"unexpected setuid/setgid file: {member.name}")
            if member.isfile():
                files.add(path.as_posix().removeprefix("./"))
    if decompressor.wait() != 0:
        raise RuntimeError("zstd package decompression failed")
    info = subprocess.check_output(["bsdtar", "-xOf", str(package), ".PKGINFO"], text=True)
    fields = {}
    for line in info.splitlines():
        if " = " in line:
            key, value = line.split(" = ", 1)
            fields.setdefault(key, []).append(value)
    name = fields["pkgname"][0]
    if name not in EXPECTED or fields["arch"] != ["x86_64"]:
        raise RuntimeError("unexpected package identity/architecture")
    if missing := EXPECTED[name] - files:
        raise RuntimeError(f"missing package files: {sorted(missing)}")
    for path in files:
        if path == ".INSTALL" or (path.startswith("usr/bin/") and path != "usr/bin/niri-rdp-setup") or path.startswith(("usr/share/wayland-sessions/", "etc/gdm/", "etc/gnome/")):
            raise RuntimeError(f"unexpected host-desktop or install-hook entry: {path}")
        if path.startswith("etc/pam.d/") and path != "etc/pam.d/niri-rdp":
            raise RuntimeError(f"unexpected PAM service: {path}")
        if "systemd/system/" in path and path not in EXPECTED["niri-remote-login"]:
            raise RuntimeError(f"unexpected service/enablement link: {path}")
    return {"name": name, "version": fields["pkgver"][0], "sha256": hashlib.sha256(package.read_bytes()).hexdigest(),
            "dependencies": fields.get("depend", []), "files": sorted(files)}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("packages", nargs="+", type=Path)
    p.add_argument("--stage", type=Path, default=ROOT / "artifacts/package-root")
    args = p.parse_args()
    stage = args.stage.resolve()
    if not stage.is_relative_to(ROOT / "artifacts"):
        p.error("staging root must be inside repository artifacts/")
    reports = [inspect(pkg.resolve(strict=True)) for pkg in args.packages]
    names = [report["name"] for report in reports]
    combined = names == ["niri-remote-login"]
    if combined:
        if missing := EXPECTED["niri-rdp-headless"] - set(reports[0]["files"]):
            raise RuntimeError(f"combined package missing headless files: {sorted(missing)}")
        if {"usr/bin/niri-rdp-setup", "usr/lib/niri-rdp/prepare-tls"} - set(reports[0]["files"]):
            raise RuntimeError("combined package missing host setup helpers")
    elif len(names) != 2 or set(names) != set(EXPECTED):
        raise RuntimeError("supply one combined package or exactly one package of each split type")
    stage.mkdir(parents=True, exist_ok=True)
    for package in args.packages:
        subprocess.run(["bsdtar", "-xf", str(package.resolve()), "-C", str(stage)], check=True)
    for binary in ("niri-headless", "niri-rdp-bridge", "niri-rdp-broker", "encoder-probe"):
        path = stage / "usr/lib/niri-rdp" / binary
        if path.read_bytes()[:4] != b"\x7fELF" or not path.stat().st_mode & 0o111:
            raise RuntimeError(f"missing executable ELF: {binary}")
    (ROOT / "artifacts/package-validation.json").write_text(json.dumps({
        "packages": reports, "stage": str(stage), "result": "structure and preservation checks passed",
        "scope": "workspace extraction; not a pacman installation or boot test",
    }, indent=2) + "\n")
    print(f"Packages inspected and extracted into {stage}; no installation performed")


if __name__ == "__main__":
    main()
