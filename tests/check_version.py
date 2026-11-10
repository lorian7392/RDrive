#!/usr/bin/env python3
"""Validate a compiled RDrive version and an optional source version tag.

Development infrastructure only: --version does not open profiles or storage.
The optional native App is inspected; it is never launched or activated.
"""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CORE = r"(?:0|[1-9][0-9]*)"
PRERELEASE = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
SEMVER = re.compile(rf"{CORE}\.{CORE}\.{CORE}(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?")
BUNDLES = {
    "Contents/Info.plist": "io.rdrive.native",
    "Contents/PlugIns/RDriveFinder.appex/Contents/Info.plist": "io.rdrive.native.finder",
    "Contents/PlugIns/RDriveProvider.appex/Contents/Info.plist": "io.rdrive.native.provider",
    "Contents/PlugIns/RDriveStatus.appex/Contents/Info.plist": "io.rdrive.native.status",
}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cli_version(binary, expected):
    result = subprocess.run([str(binary.resolve()), "--version"], check=True,
                            capture_output=True, text=True, timeout=10)
    actual = result.stdout.strip()
    if actual != "rdrive " + expected or result.stderr.strip():
        raise ValueError(f"CLI version differs from Cargo.toml: expected 'rdrive {expected}', got {actual!r}")
    return actual


def validate(manifest, binary, app=None, tag=None):
    version = tomllib.loads(manifest.read_text())["package"]["version"]
    if not isinstance(version, str) or not SEMVER.fullmatch(version):
        raise ValueError("Cargo.toml must declare an explicit semantic version")
    if tag is not None and tag != "v" + version:
        raise ValueError(f"Version tag must be exactly v{version}; got {tag!r}")
    report = {"version": version, "tag": tag,
              "cli_version": cli_version(binary, version), "cli_sha256": digest(binary),
              "bundle_versions": {}}
    if app is not None:
        builds = set()
        for relative, identifier in BUNDLES.items():
            path = app / relative
            info = plistlib.loads(path.read_bytes())
            if info.get("CFBundleIdentifier") != identifier or info.get("CFBundleShortVersionString") != version:
                raise ValueError("Native bundle identity/version differs from Cargo.toml: " + relative)
            build = info.get("CFBundleVersion")
            if not isinstance(build, str) or not build:
                raise ValueError("Native bundle is missing its build number: " + relative)
            builds.add(build)
            report["bundle_versions"][relative] = {"version": version, "build": build,
                                                   "plist_sha256": digest(path)}
        if len(builds) != 1:
            raise ValueError("Native host and extension build numbers differ")
        embedded = app / "Contents/Resources/rdrive"
        report["embedded_cli_version"] = cli_version(embedded, version)
        report["embedded_cli_sha256"] = digest(embedded)
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=ROOT / "Cargo.toml")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--app", type=Path)
    parser.add_argument("--tag", help="Tag name, without refs/tags/; must exactly equal v<Cargo version>")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        report = validate(args.manifest, args.binary, args.app, args.tag)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f"Version validation failed: {error}\n")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print("Version verified: " + report["version"] + (" (" + args.tag + ")" if args.tag else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
