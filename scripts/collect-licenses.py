#!/usr/bin/env python3
"""Collect locked Cargo notices and bridge-only native FFmpeg provenance."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

manifest, output = map(Path, sys.argv[1:])
metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--manifest-path", str(manifest), "--locked", "--offline",
    "--format-version", "1", "--filter-platform", "x86_64-unknown-linux-gnu",
], text=True))
resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
sections = ["Upstream licenses for the locked Linux Cargo dependency graph.\n"
            "Original integration code is covered by the top-level LICENSE.\n"]
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["id"] not in resolved or package["source"] is None:
        continue
    base = Path(package["manifest_path"]).parent
    sections.append(f"\n=== {package['name']} {package['version']} ===\n"
                    f"Source: {package['source']}\nLicense expression: {package['license']}\n")
    candidates = sorted(p for p in base.iterdir() if p.is_file() and
                        p.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")))
    if package.get("license_file"):
        declared = (base / package["license_file"]).resolve()
        if declared not in candidates:
            candidates.append(declared)
    for path in candidates:
        sections.append(f"\n--- {path.name} ---\n{path.read_text(errors='replace')}\n")

# FFmpeg is a native dependency of the bridge, outside Cargo's dependency graph.
# The compositor uses this collector too and does not link these libraries.
root = next(p for p in metadata["packages"] if p["id"] == metadata["resolve"]["root"])
if root["name"] == "niri-rdp-bridge":
    def capture(command):
        return subprocess.check_output(command, text=True, stderr=subprocess.STDOUT).strip()

    libraries = ("libavcodec", "libavutil", "libswscale")
    versions = capture(["pkg-config", "--modversion", *libraries]).splitlines()
    sections.append("\n=== FFmpeg system libraries linked by niri-rdp-bridge ===\n"
                    "These dynamically linked libraries are supplied by the platform FFmpeg package; "
                    "they are not included in this project's source archive.\n"
                    "Upstream source and licensing: https://ffmpeg.org/legal.html\n"
                    "Arch package provenance: https://archlinux.org/packages/extra/x86_64/ffmpeg/\n"
                    "Original bridge source retains its MIT license.\n")
    for library, version in zip(libraries, versions, strict=True):
        sections.append(f"Linked build library: {library} {version}\n")
    sections.append(f"\n--- Platform FFmpeg version and configuration ---\n"
                    f"{capture(['ffmpeg', '-version'])}\n"
                    f"\n--- Platform FFmpeg license notice ---\n"
                    f"{capture(['ffmpeg', '-hide_banner', '-L'])}\n")
    if shutil.which("pacman"):
        sections.append(f"\n--- Arch FFmpeg package metadata ---\n"
                        f"{capture(['pacman', '-Qi', 'ffmpeg'])}\n")
output.write_text("".join(sections))
print(f"Collected notices for {len(resolved)} resolved packages into {output}")
