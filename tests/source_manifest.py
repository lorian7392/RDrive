#!/usr/bin/env python3
"""Freeze first-party application inputs before tests and verify them afterward."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
PYTHON = ["macos/build.py", "scripts/generate-cli-docs.py", "scripts/install-skills.py", "scripts/mcp-config.py"]


def snapshot(root):
    paths = list((root / "src").rglob("*.rs"))
    paths += [path for path in (root / "macos").rglob("*.swift")
              if not {"tests", "build"}.intersection(path.relative_to(root / "macos").parts)]
    paths += [root / name for name in PYTHON]
    return {path.relative_to(root).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(paths)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args(argv)
    sources = snapshot(args.root.resolve())
    if args.check:
        if json.loads(args.output.read_text())["sources"] != sources:
            raise ValueError("Application source inventory or hashes changed during tests")
        print("Source identity verified")
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        # Never replace an earlier snapshot, which would hide a source change.
        with args.output.open("x") as output:
            json.dump({"schema": 1, "sources": sources}, output, indent=2)
            output.write("\n")
        print("Frozen source manifest: " + str(args.output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
