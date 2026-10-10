#!/usr/bin/env python3
"""Collect built, validated packages and exact source inputs; never install."""
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / "artifacts"
OUT = ARTIFACTS / "installable"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def copy(source, destination):
    if source.is_symlink() or not source.is_file():
        raise RuntimeError(f"missing or nonregular delivery input: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def main():
    if OUT.exists():
        raise RuntimeError("artifacts/installable already exists; preserve/version the previous delivery first")
    report = json.loads((ARTIFACTS / "package-validation.json").read_text())
    packages = {item["name"]: item for item in report["packages"]}
    # Validate all package/source identities before creating the delivery.
    for name, item in packages.items():
        directory = ARTIFACTS / "release" / name
        package = directory / f"{name}-{item['version']}-x86_64.pkg.tar.zst"
        if digest(package) != item["sha256"]:
            raise RuntimeError(f"package changed since validation: {name}")
        build = subprocess.check_output(["bsdtar", "-xOf", str(package), ".BUILDINFO"], text=True)
        if f"pkgbuild_sha256sum = {digest(directory / 'PKGBUILD')}\n" not in build:
            raise RuntimeError(f"PKGBUILD differs from built package: {name}")
    OUT.mkdir()
    for name, item in packages.items():
        directory = ARTIFACTS / "release" / name
        copy(directory / f"{name}-{item['version']}-x86_64.pkg.tar.zst",
             OUT / f"{name}-{item['version']}-x86_64.pkg.tar.zst")
        copy(directory / "PKGBUILD", OUT / name / "PKGBUILD")
    for filename in ("niri-remote-login-0.1.0.tar.gz",):
        copy(ARTIFACTS / "release/niri-remote-login" / filename, OUT / "niri-remote-login" / filename)
    headless = ARTIFACTS / "release/niri-rdp-headless"
    for filename in ("0001-remote-headless-bounds-and-startup.patch", "collect-licenses.py"):
        copy(headless / filename, OUT / "niri-rdp-headless" / filename)
    pins = json.loads((ROOT / "compositor/pins.json").read_text())["selected"]
    source = ROOT / "compositor" / f"niri-{pins['revision']}.tar.gz"
    if digest(source) != pins["archive_sha256"]:
        raise RuntimeError("Niri source archive hash differs from pin")
    copy(source, OUT / "niri-rdp-headless" / source.name)
    copy(ARTIFACTS / "release/source-manifest.json", OUT / "source-manifest.json")
    for directory in ("docs", "checkpoints"):
        for path in sorted((ROOT / directory).rglob("*")):
            if path.is_file():
                copy(path, OUT / path.relative_to(ROOT))
    for path in ("README.md", "LICENSE", "compositor/pins.json", "compositor/README.md",
                 "compositor/CHECKPOINT.md", "bridge/README.md", "bridge/CHECKPOINT.md",
                 "broker/README.md", "broker/CHECKPOINT.md"):
        copy(ROOT / path, OUT / path)
    for filename in ("package-validation.json", "packaged-integration.json", "packaged-tls-integration.json"):
        copy(ARTIFACTS / filename, OUT / "validation" / filename)
    (OUT / "SHA256SUMS").write_text("".join(
        f"{digest(path)}  {path.relative_to(OUT).as_posix()}\n"
        for path in sorted(OUT.rglob("*")) if path.is_file()))
    archive = ARTIFACTS / "niri-remote-login-arch-0.1.0.tar.gz"
    with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|") as tar:
            for path in sorted(OUT.rglob("*")):
                if not path.is_file():
                    continue
                info = tar.gettarinfo(str(path), "installable/" + path.relative_to(OUT).as_posix())
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = "root"
                with path.open("rb") as stream:
                    tar.addfile(info, stream)
    (archive.with_name(archive.name + ".sha256")).write_text(f"{digest(archive)}  {archive.name}\n")
    print(f"Prepared {archive}; no installation performed")


if __name__ == "__main__":
    main()
