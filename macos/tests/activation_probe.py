#!/usr/bin/env python3
"""Reversibly probe ONLY this development app's native registration/loadability.

Requires explicit invocation. Does not install an app, enable unrelated plugins,
or change File Provider testing mode. Refuses pre-existing RDrive registrations.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import uuid

LSREGISTER = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"


def command(*args, timeout=40):
    result = subprocess.run([str(x) for x in args], capture_output=True, text=True, timeout=timeout)
    return {"args": [str(x) for x in args], "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}


def payload(result):
    try:
        return json.loads(result["stdout"].strip().splitlines()[-1])
    except (ValueError, IndexError):
        return {}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", type=Path, default=Path(__file__).resolve().parents[1] / "build/RDrive.app")
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[1] / "build/activation-report.json")
    parser.add_argument("--probe-finder", action="store_true", help="temporarily request only RDrive's own Finder extension be enabled")
    options = parser.parse_args()
    app = options.app.resolve()
    control = app / "Contents/MacOS/RDriveCompanion"
    plugins = [app / "Contents/PlugIns/RDriveProvider.appex", app / "Contents/PlugIns/RDriveFinder.appex", app / "Contents/PlugIns/RDriveStatus.appex"]
    identifiers = ["io.rdrive.native.provider", "io.rdrive.native.finder", "io.rdrive.native.status"]
    report = {"before": [], "operations": [], "cleanup": [], "after": []}
    for identifier in identifiers:
        before = command("pluginkit", "-m", "-i", identifier, "-v")
        report["before"].append(before)
        if "no matches" not in before["stdout"] + before["stderr"]:
            raise SystemExit("Refusing to alter a pre-existing RDrive extension registration.")
    domain = "rdrive-native-selftest-" + uuid.uuid4().hex[:12]
    domain_added = False
    finder_requested = False
    try:
        report["operations"].append(command(LSREGISTER, "-f", app))
        for plugin in plugins:
            report["operations"].append(command("pluginkit", "-a", plugin))
        for identifier in identifiers:
            report["operations"].append(command("pluginkit", "-m", "-i", identifier, "-v"))
        report["operations"].append(command(control, "--list-domains"))
        added = command(control, "--register-domain", domain, "--display-name", "RDrive Isolated Activation Test")
        report["operations"].append(added)
        domain_added = payload(added).get("ok") is True
        if domain_added:
            report["operations"].append(command(control, "--list-domains"))
        if options.probe_finder:
            report["operations"].append(command("pluginkit", "-e", "use", "-i", identifiers[1]))
            finder_requested = True
        report["operations"].append(command(control, "--diagnose"))
    finally:
        if domain_added:
            removed = command(control, "--remove-domain", domain)
            report["cleanup"].append(removed)
            preserved = payload(removed).get("preserved_location")
            if preserved:
                path = Path(preserved)
                if path.parent == Path.home() / "Library/CloudStorage" and path.name.startswith("RDrive-RDriveIsolatedActivationTest"):
                    try:
                        os.rmdir(path)  # Empty test directories only; never recursive.
                        report["empty_preserved_directory_removed"] = str(path)
                    except OSError:
                        report["preserved_directory_kept"] = str(path)
        if finder_requested:
            report["cleanup"].append(command("pluginkit", "-e", "ignore", "-i", identifiers[1]))
        for plugin in plugins:
            report["cleanup"].append(command("pluginkit", "-r", plugin))
        report["cleanup"].append(command(LSREGISTER, "-u", app))
        for identifier in identifiers:
            report["after"].append(command("pluginkit", "-m", "-i", identifier, "-v"))
        report["after"].append(command(control, "--diagnose"))
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, indent=2) + "\n")
    for result in report["operations"]:
        print(json.dumps({"operation": result["args"][1:], "exit": result["exit"], "result": payload(result) or result["stdout"].strip(), "stderr": result["stderr"].strip()}))
    print("Activation evidence: " + str(options.output))


if __name__ == "__main__":
    main()
