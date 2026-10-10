#!/usr/bin/env python3
"""Record a file-integrity checkpoint without requiring Git metadata."""
import argparse
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SKIP = {".git", ".agents", ".aws", ".codex", "target", ".cargo-cache", "cargo-home", "__pycache__", "upstream", "sysroot", "artifacts", "build", "private", "downloads", "results", "generated"}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("name")
    args = p.parse_args()
    if not args.name.replace("-", "").isalnum():
        p.error("name must contain letters, numbers or dashes")
    out = ROOT / "checkpoints" / f"{args.name}.json"
    files = {}
    for item in sorted(ROOT.rglob("*")):
        relative = item.relative_to(ROOT)
        if any(part in SKIP for part in relative.parts) or item == out or item.is_symlink():
            continue
        if item.is_file() and item.suffix not in {".pyc", ".o"}:
            if item.read_bytes()[:4] != b"\x7fELF":
                files[relative.as_posix()] = hashlib.sha256(item.read_bytes()).hexdigest()
    out.write_text(json.dumps({"created_utc": datetime.now(timezone.utc).isoformat(),
                               "files_sha256": files}, indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    main()
