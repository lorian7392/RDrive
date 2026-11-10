#!/usr/bin/env python3
"""Merge same-source Rust collections across hosts without merging LLVM formats.

The FIRST input defines the measured macOS target's executable line inventory
and must run all regular tests. Same-source Linux Samba executions can add hits
to those exact lines; Linux-only platform code is outside this macOS metric.
The complete source inventory must match this checkout. Original reports,
profiles and object hashes remain in the input artifacts.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]


def source_hashes(root):
    return {path.relative_to(root).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted((root / "src").rglob("*.rs"))}


def merge(root, directories, output):
    expected = source_hashes(root)
    if not expected:
        raise ValueError("No current Rust source inventory")
    mappings, inputs, heads = {}, [], set()
    for index, directory in enumerate(directories):
        manifest_path = directory / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        if manifest.get("schema") != 1 or manifest.get("status") != "passed" or manifest.get("source_hashes_verified") is not True:
            raise ValueError("Input is not a completed verified Rust collection: " + str(directory))
        if manifest.get("sources") != expected:
            raise ValueError("Coverage source hashes/inventory differ from current checkout: " + str(directory))
        if index == 0:
            host = next((line.removeprefix("host: ") for line in manifest.get("rustc", "").splitlines()
                         if line.startswith("host: ")), "")
            if not host.endswith("-apple-darwin") or manifest.get("test_suite") != "all" or manifest.get("workflows") not in ("dav", "all"):
                raise ValueError("First collection must be a macOS host with all regular tests and DAV workflows")
        heads.add(manifest["git_head"])
        origin = Path(manifest["root"])
        if not origin.is_absolute():
            raise ValueError("Collection root must be absolute")
        raw = directory / "coverage.lcov"
        coverage_hash = hashlib.sha256(raw.read_bytes()).hexdigest()
        if coverage_hash != manifest.get("coverage_sha256"):
            raise ValueError("Coverage report changed after source/object verification: " + str(directory))
        spans_hash = hashlib.sha256((directory / "test-spans.json").read_bytes()).hexdigest()
        if spans_hash != manifest.get("test_spans_sha256"):
            raise ValueError("Test scope report changed after collection: " + str(directory))
        current = None
        outside_baseline = 0
        for record in raw.read_text().splitlines():
            if record.startswith("SF:"):
                filename = Path(record[3:])
                if not filename.is_absolute():
                    filename = origin / filename
                try:
                    name = filename.relative_to(origin).as_posix()
                except ValueError:
                    name = None
                current = name if name in expected else None
                if current is not None and index == 0:
                    mappings.setdefault(current, {})
            elif current is not None and record.startswith("DA:"):
                number, hits, *_ = record[3:].split(",")
                number, hits = int(number), int(hits)
                count = len((root / current).read_text().splitlines())
                if number < 1 or number > count or hits < 0:
                    raise ValueError("Invalid/outdated LCOV source line: " + current)
                if index > 0 and (current not in mappings or number not in mappings[current]):
                    outside_baseline += 1
                    continue
                lines = mappings[current]
                lines[number] = max(lines.get(number, 0), hits)
            elif record == "end_of_record":
                current = None
        inputs.append({"manifest": str(manifest_path), "root": str(origin),
                       "git_head": manifest["git_head"], "workflows": manifest["workflows"],
                       "lines_outside_macos_target_inventory": outside_baseline,
                       "test_spans_sha256": spans_hash,
                       "coverage_sha256": coverage_hash})
    if len(heads) != 1:
        raise ValueError("Collections were built from different Git revisions")
    if not mappings or not any(mappings.values()):
        raise ValueError("No Rust line mappings in verified inputs")
    content = []
    for name, lines in sorted(mappings.items()):
        content.append("SF:" + str((root / name).resolve()))
        content.extend(f"DA:{number},{hits}" for number, hits in sorted(lines.items()))
        content += ["LF:" + str(len(lines)), "LH:" + str(sum(hits > 0 for hits in lines.values())), "end_of_record"]
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(content) + "\n")
    evidence = {"schema": 1, "sources": expected, "inputs": inputs,
                "metric": "All mapped macOS target production lines; same-source Linux SMB executions supplement hits only for exact macOS executable source lines. No macOS production exclusions. This is not a Linux coverage claim.",
                "git_head": next(iter(heads)), "source_hashes_verified": True}
    output.with_suffix(".manifest.json").write_text(json.dumps(evidence, indent=2) + "\n")
    return evidence


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-dir", action="append", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    merge(args.root.resolve(), args.input_dir, args.output)
    print("Verified Rust line union written to " + str(args.output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
