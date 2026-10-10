#!/usr/bin/env python3
"""Collect upstream license/notice files for the locked Cargo dependency graph."""
import json
from pathlib import Path
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
output.write_text("".join(sections))
print(f"Collected notices for {len(resolved)} resolved packages into {output}")
