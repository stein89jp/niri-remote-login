#!/usr/bin/env python3
"""Create audited local-source makepkg bundles, never install or start services."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import tarfile
import shutil

ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.6.0"
INCLUDE = ("LICENSE", "README.md", "Makefile", "bridge", "broker", "docs", "checkpoints", "scripts", "packaging", "compositor/README.md", "compositor/CHECKPOINT.md", "compositor/pins.json", "compositor/patches", "lab", "prototype", "tests")
EXCLUDE = {"target", ".cargo-cache", "cargo-home", "__pycache__", ".git", "build", "private", "sysroot", "downloads", "results", "generated"}


def selected_files():
    for entry in INCLUDE:
        path = ROOT / entry
        if not path.exists():
            raise RuntimeError(f"missing release input: {entry}")
        files = [path] if path.is_file() else sorted(path.rglob("*"))
        for item in files:
            relative = item.relative_to(ROOT)
            if any(part in EXCLUDE for part in relative.parts):
                continue
            # Repo-local makepkg folders hold downloaded inputs and build trees.
            # Exclude only their products: bridge/src remains release source.
            if relative.parts[:2] == ("packaging", "arch") and len(relative.parts) > 3:
                product = relative.parts[3]
                if (product in {"src", "pkg"} or product.startswith("repo-")
                        or ".tar." in product or product.endswith(".log")):
                    continue
            if item.is_symlink():
                raise RuntimeError(f"release input must not be a symlink: {relative}")
            if item.is_file() and item.suffix not in {".pyc", ".o"}:
                # Generated executables are not sources.
                if item.read_bytes()[:4] == b"\x7fELF":
                    continue
                yield item


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts" / "release")
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    package = out / "niri-remote-login"
    package.mkdir(exist_ok=True)
    archive = package / f"niri-remote-login-{VERSION}.tar.gz"
    manifest = {}
    with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|") as tar:
            for item in selected_files():
                relative = item.relative_to(ROOT).as_posix()
                manifest[relative] = hashlib.sha256(item.read_bytes()).hexdigest()
                info = tar.gettarinfo(str(item), f"niri-remote-login-{VERSION}/{relative}")
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                info.mtime = 0
                with item.open("rb") as stream:
                    tar.addfile(info, stream)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    template = (ROOT / "packaging/arch/niri-remote-login/PKGBUILD").read_text()
    (package / "PKGBUILD").write_text(template.replace("@SOURCE_SHA256@", digest))
    headless = out / "niri-rdp-headless"
    headless.mkdir(exist_ok=True)
    for item in (ROOT / "packaging/arch/niri-rdp-headless").iterdir():
        if item.is_file():
            shutil.copy2(item, headless / item.name)
    # The pinned series is the source of truth; every listed patch must exist.
    pins = json.loads((ROOT / "compositor/pins.json").read_text())["selected"]
    for relative in pins["patches"]:
        item = ROOT / "compositor" / relative
        if not item.is_file():
            raise RuntimeError(f"missing required headless patch: {relative}")
        shutil.copy2(item, headless / item.name)
    licenses = ROOT / "scripts/collect-licenses.py"
    shutil.copy2(licenses, headless / licenses.name)

    def source_digest(match):
        source = headless / match.group(1)
        if not source.is_file():
            raise RuntimeError(f"PKGBUILD names a missing source: {match.group(1)}")
        return hashlib.sha256(source.read_bytes()).hexdigest()

    template = (headless / "PKGBUILD").read_text()
    (headless / "PKGBUILD").write_text(re.sub(r"@SHA256:([A-Za-z0-9._-]+)@", source_digest, template))
    (out / "source-manifest.json").write_text(json.dumps({
        "version": VERSION, "archive_sha256": digest, "files": manifest,
        "status": "source build bundle; installation and boot acceptance require separate Arch test machine",
    }, indent=2) + "\n")
    print(f"Prepared source packages in {out}; no installation performed")


if __name__ == "__main__":
    main()
