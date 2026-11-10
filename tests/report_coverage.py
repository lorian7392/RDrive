#!/usr/bin/env python3
"""Combine actual production coverage; never substitute test counts for coverage.

Rust/Swift consume LLVM LCOV DA records. Rust test-only spans are obtained from
the syn-based tools/coverage-scope helper, not regex or production exclusions.
Python consumes coverage.py JSON. Run this after collecting all three inputs.
This is test infrastructure, not part of the installed application.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
PYTHON_SOURCES = ["macos/build.py", "scripts/install-skills.py", "scripts/mcp-config.py", "scripts/generate-cli-docs.py"]


def relative(root, filename):
    path = Path(filename)
    if not path.is_absolute():
        path = root / path
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None


def lcov(root, path):
    files, current = {}, None
    for record in Path(path).read_text().splitlines():
        if record.startswith("SF:"):
            current = relative(root, record[3:])
            if current is not None:
                files.setdefault(current, {})
        elif record.startswith("DA:") and current is not None:
            number, hits, *_ = record[3:].split(",")
            number, hits = int(number), int(hits)
            if number < 1 or hits < 0:
                raise ValueError("Invalid LCOV line or execution count")
            source = root / current
            if not source.is_file() or number > len(source.read_text().splitlines()):
                raise ValueError("LCOV line outside current source: " + current)
            lines = files[current]
            # Instantiations/objects can report the same physical line more
            # than once. Any observed execution covers that one source line.
            lines[number] = max(lines.get(number, 0), hits)
        elif record == "end_of_record":
            current = None
    return files


def file_result(root, name, lines, status="instrumented"):
    missing = sorted(number for number, hits in lines.items() if not hits)
    return {"file": name, "sha256": hashlib.sha256((root / name).read_bytes()).hexdigest(),
            "status": status, "lines": len(lines), "covered": len(lines) - len(missing), "missing": missing}


def summarize(files):
    count = sum(item["lines"] for item in files)
    covered = sum(item["covered"] for item in files)
    unmapped = [item["file"] for item in files if item["status"] == "not_instrumented"]
    return {"lines": count, "covered": covered, "percent": 100 * covered / count if count and not unmapped else None,
            "scope_complete": not unmapped, "not_instrumented": unmapped, "files": files}


def rust_report(root, path, spans_path):
    mappings = lcov(root, path)
    scopes = json.loads(Path(spans_path).read_text())
    files = []
    functions = scopes.get("production_function_items", {})
    for source in sorted((root / "src").rglob("*.rs")):
        name = source.relative_to(root).as_posix()
        if name not in scopes["files"]:
            raise ValueError("Rust scope missing source: " + name)
        spans = scopes["files"][name]
        lines = {number: hits for number, hits in mappings.get(name, {}).items()
                 if not any(span["start"] <= number <= span["end"] for span in spans)}
        test_only = any(span["start"] == 1 and span["end"] >= len(source.read_text().splitlines()) for span in spans)
        if test_only:
            continue
        # Modules with only declarations (e.g. lib.rs) have no executable
        # mapping. Missing mappings for a file with code must fail the gate.
        mapped = bool(lines) or functions.get(name) == 0
        files.append(file_result(root, name, lines, "instrumented" if lines else ("no_executable_items" if mapped else "not_instrumented")))
    return summarize(files)


def swift_report(root, path):
    mappings = lcov(root, path)
    files = []
    for source in sorted((root / "macos").rglob("*.swift")):
        if "tests" in source.relative_to(root / "macos").parts or "build" in source.relative_to(root / "macos").parts:
            continue
        name = source.relative_to(root).as_posix()
        files.append(file_result(root, name, mappings.get(name, {}), "instrumented" if mappings.get(name) else "not_instrumented"))
    return summarize(files)


def python_report(root, paths):
    mappings, branches = {}, {}
    for path in paths:
        for filename, entry in json.loads(Path(path).read_text())["files"].items():
            name = relative(root, filename)
            if name not in PYTHON_SOURCES:
                continue
            if entry.get("excluded_lines"):
                raise ValueError("Production Python exclusions are not allowed: " + name)
            length = len((root / name).read_text().splitlines())
            if any(type(number) is not int or not 1 <= number <= length
                   for number in entry["missing_lines"] + entry["executed_lines"]):
                raise ValueError("Python coverage line outside current source: " + name)
            lines = mappings.setdefault(name, {})
            for number in entry["missing_lines"]:
                lines.setdefault(number, 0)
            for number in entry["executed_lines"]:
                lines[number] = 1
            branch = branches.setdefault(name, {"executed": set(), "missing": set()})
            branch["executed"].update(map(tuple, entry.get("executed_branches", [])))
            branch["missing"].update(map(tuple, entry.get("missing_branches", [])))
    files = []
    for name in PYTHON_SOURCES:
        result = file_result(root, name, mappings.get(name, {}), "instrumented" if mappings.get(name) else "not_instrumented")
        branch = branches.get(name, {"executed": set(), "missing": set()})
        result["branches"] = {"covered": len(branch["executed"]), "count": len(branch["executed"] | branch["missing"])}
        files.append(result)
    return summarize(files)


def verify_source_manifest(root, path):
    """Check frozen compiler/test inputs before attributing hits to current code."""
    manifest = json.loads(Path(path).read_text())
    sources = ({item["source"]: item["sha256"] for item in manifest}
               if isinstance(manifest, list) else manifest.get("sources", manifest))
    if not sources:
        raise ValueError("Empty source manifest: " + str(path))
    for name, expected in sources.items():
        source = root / name
        if relative(root, source) != name or not source.is_file():
            raise ValueError("Invalid manifest source: " + name)
        if hashlib.sha256(source.read_bytes()).hexdigest() != expected:
            raise ValueError("Source changed after coverage collection: " + name)
    artifacts = {}
    if isinstance(manifest, dict):
        for field, filename in [("coverage_sha256", "coverage.lcov"), ("test_spans_sha256", "test-spans.json")]:
            if field in manifest:
                artifact = Path(path).parent / filename
                if hashlib.sha256(artifact.read_bytes()).hexdigest() != manifest[field]:
                    raise ValueError("Coverage artifact changed after collection: " + str(artifact))
                artifacts[filename] = manifest[field]
    return {"path": str(path), "sha256": hashlib.sha256(Path(path).read_bytes()).hexdigest(),
            "verified_sources": sorted(sources), "verified_artifacts": artifacts}


def build_report(root, rust_lcov, rust_spans, swift_lcov, python_json):
    languages = {"rust": rust_report(root, rust_lcov, rust_spans), "swift": swift_report(root, swift_lcov), "python": python_report(root, python_json)}
    total = summarize([item for language in languages.values() for item in language["files"]])
    total.pop("files")
    return {"metric": "Unique physical executable production source lines; tests, dependencies and generated code excluded. No production adapters or error paths excluded.",
            "scope": "All src Rust on the measured target, all macOS production Swift, four distributed Python scripts. Test and development infrastructure is outside application scope.",
            "rust_branch_coverage": "not measured; stable LLVM line instrumentation does not collect Rust branches",
            "inputs": {"rust_lcov": str(rust_lcov), "rust_test_spans": str(rust_spans), "swift_lcov": str(swift_lcov), "python_json": list(map(str, python_json))},
            "total": total, "languages": languages}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--rust-lcov", type=Path, required=True)
    parser.add_argument("--rust-test-spans", type=Path, required=True)
    parser.add_argument("--swift-lcov", type=Path, required=True)
    parser.add_argument("--python-json", type=Path, action="append", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-manifest", type=Path, action="append", default=[],
                        help="Verify frozen Rust or native source hashes; repeat for each input run")
    parser.add_argument("--min-line", type=float, default=100, help="Minimum for EACH language, default 100; incomplete mappings always fail")
    args = parser.parse_args(argv)
    if not 0 <= args.min_line <= 100:
        parser.error("--min-line must be between 0 and 100")
    root = args.root.resolve()
    manifests = [verify_source_manifest(root, path) for path in args.source_manifest]
    span_hashes = {entry["verified_artifacts"]["test-spans.json"] for entry in manifests
                   if "test-spans.json" in entry["verified_artifacts"]}
    if span_hashes and hashlib.sha256(args.rust_test_spans.read_bytes()).hexdigest() not in span_hashes:
        raise ValueError("Rust test spans do not match any verified collection")
    report = build_report(root, args.rust_lcov, args.rust_test_spans, args.swift_lcov, args.python_json)
    report["verified_source_manifests"] = manifests
    verified = {name for manifest in manifests for name in manifest["verified_sources"]}
    report["unverified_sources"] = [item["file"] for language in report["languages"].values()
                                    for item in language["files"] if item["file"] not in verified]
    report["source_provenance_complete"] = not report["unverified_sources"]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    failed = []
    for name, data in report["languages"].items():
        print(f"{name}: {data['covered']}/{data['lines']} executable lines; percent={data['percent']}; complete={data['scope_complete']}")
        if data["percent"] is None or data["percent"] < args.min_line:
            failed.append(name)
    if args.min_line == 100 and report["unverified_sources"]:
        failed.append("unverified source identities")
    if failed:
        print(f"Coverage gate FAILED ({args.min_line}% per language): {', '.join(failed)}", file=sys.stderr)
        return 1
    print("Coverage gate PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
