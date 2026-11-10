#!/usr/bin/env python3
"""Measure Rust with exact Cargo artifacts, fresh profiles and frozen sources.

This is development infrastructure. It never activates the application, mounts
storage, reads Keychain credentials, or changes a user's service. The optional
Samba fixture requires Docker and uses disposable synthetic shares.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sources(root):
    return {path.relative_to(root).as_posix(): digest(path)
            for path in sorted((root / "src").rglob("*.rs"))}


def check_sources(root, expected):
    if sources(root) != expected:
        raise RuntimeError("Rust source changed during coverage collection; discard this measurement")


def parse_environment(output):
    environment = {}
    for line in output.splitlines():
        if not line.strip():
            continue
        words = shlex.split(line.removeprefix("export "))
        if len(words) != 1 or "=" not in words[0]:
            raise ValueError("Unexpected cargo-llvm-cov show-env output")
        name, value = words[0].split("=", 1)
        if not name or not name.replace("_", "a").isalnum():
            raise ValueError("Invalid environment name from cargo-llvm-cov")
        environment[name] = value
    if "RUSTC_WRAPPER" not in environment or "CARGO_LLVM_COV" not in environment:
        raise ValueError("Incomplete cargo-llvm-cov instrumentation environment")
    return environment


def artifacts(output, root):
    found = {}
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        entry = json.loads(line)
        if entry.get("reason") != "compiler-artifact" or not entry.get("executable"):
            continue
        source = Path(entry["target"]["src_path"]).resolve()
        if not source.is_relative_to(root.resolve()):
            continue
        path = Path(entry["executable"]).resolve()
        found[str(path)] = {"path": str(path), "name": entry["target"]["name"],
                            "kind": entry["target"]["kind"],
                            "test": entry.get("profile", {}).get("test", False)}
    return list(found.values())


def test_command(obj, library, suite):
    command = [obj["path"]]
    if obj["path"] == str(library):
        if suite == "portable-smb":
            command.append("smb::")
        # Library fixtures exercise process-global signals and file locks.
        # Isolate unrelated fixtures while preserving concurrency inside each.
        command.append("--test-threads=1")
    return command


def workflows(python, binary, group, output, library):
    commands = []
    if group in ("dav", "all"):
        for name, flag in [
            ("cli", None), ("advanced", None), ("smart", None),
            ("namespace", None), ("provider_receipts", None),
            ("coverage_cli", None), ("cache_directory", "--report"),
            ("mcp", "--report"), ("index", "--output"),
            ("search", "--report"), ("policy", "--output"),
            ("service", "--output"),
        ]:
            command = [python, str(ROOT / "tests" / (name + "_workflows.py")), str(binary)]
            if flag:
                command += [flag, str(output / (name + ".json"))]
            commands.append((name, command, {}))
    if group in ("smb", "all"):
        for name, extra in [
            ("smb", {}),
            ("smb-lifecycle", {"RDRIVE_SMB_LIFECYCLE": "1", "RDRIVE_SMB_PROOF_ONLY": "1"}),
            ("smb-provider", {"RDRIVE_SMB_PROVIDER_ONLY": "1"}),
            ("smb-smart", {"RDRIVE_SMB_PROVIDER_ONLY": "1", "RDRIVE_SMB_SMART": "1"}),
        ]:
            extra = dict(extra, RDRIVE_SMB_TEST_RUNNER=str(library),
                         RDRIVE_SMB_REPORT=str(output / (name + ".json")))
            commands.append((name, [python, str(ROOT / "tests/smb_workflows.py"), str(binary)], extra))
    return commands


def run(command, *, root, environment, log, capture=False, timeout=1800):
    print("RUN " + shlex.join(map(str, command)), flush=True)
    result = subprocess.run(list(map(str, command)), cwd=root, env=environment,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, timeout=timeout)
    log.write_text(result.stdout + "\n--- stderr ---\n" + result.stderr)
    if result.returncode:
        print((result.stdout + result.stderr)[-12000:], file=sys.stderr)
        raise subprocess.CalledProcessError(result.returncode, command)
    return result.stdout if capture else None


def collect(options):
    root = ROOT.resolve()
    output = options.output_dir.resolve()
    # A new report directory guarantees no profile from an earlier source
    # revision can accidentally enter this measurement. Cached build artifacts
    # are allowed: Cargo JSON identifies the exact current executable objects.
    output.mkdir(parents=True, exist_ok=False)
    profiles = output / "profiles"
    profiles.mkdir()
    logs = output / "logs"
    logs.mkdir()
    target = options.target_dir.resolve() if options.target_dir else output / "build"
    frozen = sources(root)
    environment = dict(os.environ, CARGO_LLVM_COV_TARGET_DIR=str(target), CARGO_TARGET_DIR=str(target))
    show = run([options.cargo_llvm_cov, "llvm-cov", "show-env", "--export-prefix"],
               root=root, environment=environment, log=logs / "show-env.log", capture=True)
    environment.update(parse_environment(show))
    environment["LLVM_PROFILE_FILE"] = str(profiles / "rdrive-%p-%16m.profraw")
    arguments = ["--locked"] + (["--offline"] if options.offline else [])
    objects = {}
    for name, cargo_args in [("cli-build", ["build", "--bin", "rdrive"]),
                             ("test-build", ["test", "--tests", "--no-run"])]:
        output_json = run([options.cargo, *cargo_args, *arguments, "--message-format=json"],
                          root=root, environment=environment, log=logs / (name + ".log"), capture=True)
        objects.update({obj["path"]: obj for obj in artifacts(output_json, root)})
    executable = [obj for obj in objects.values() if not obj["test"] and obj["kind"] == ["bin"] and obj["name"] == "rdrive"]
    library = [obj for obj in objects.values() if obj["test"] and "lib" in obj["kind"] and obj["name"] == "rdrive"]
    tests = sorted((obj for obj in objects.values() if obj["test"]), key=lambda obj: obj["path"])
    if len(executable) != 1 or len(library) != 1 or not tests:
        raise RuntimeError("Cargo did not return one current CLI, one library test and test artifacts")
    binary, library = Path(executable[0]["path"]), Path(library[0]["path"])
    for obj in objects.values():
        obj["sha256"] = digest(Path(obj["path"]))
    check_sources(root, frozen)
    executed_tests = []
    for index, obj in enumerate(tests):
        if options.test_suite == "portable-smb" and obj["path"] != str(library) and obj["name"] != "coverage_operations":
            continue
        command = test_command(obj, library, options.test_suite)
        run(command, root=root, environment=environment,
            log=logs / (f"test-{index:02d}-" + obj["name"] + ".log"))
        executed_tests.append(command)
    jobs = workflows(sys.executable, binary, options.workflows, output, library)
    # Fixture selectors are test controls; ambient leftovers must not turn a
    # full Samba run into a smaller subset while still claiming all workflows.
    for name in ("RDRIVE_SMB_LIFECYCLE", "RDRIVE_SMB_PROOF_ONLY", "RDRIVE_SMB_PROVIDER_ONLY",
                 "RDRIVE_SMB_SMART", "RDRIVE_SMB_NOTIFY_ONLY", "RDRIVE_SMB_REPORT", "RDRIVE_SMB_TEST_RUNNER"):
        environment.pop(name, None)
    for name, command, extra in jobs:
        run(command, root=root, environment=dict(environment, **extra), log=logs / (name + ".log"))
    check_sources(root, frozen)
    for obj in objects.values():
        if digest(Path(obj["path"])) != obj["sha256"]:
            raise RuntimeError("Instrumented object changed during coverage: " + obj["path"])
    raw = sorted(profiles.glob("*.profraw"))
    if not raw:
        raise RuntimeError("No Rust profiles produced")
    sysroot = run([options.rustc, "--print", "sysroot"], root=root, environment=environment,
                  log=logs / "sysroot.log", capture=True).strip()
    version = run([options.rustc, "-vV"], root=root, environment=environment,
                  log=logs / "rustc.log", capture=True)
    host = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
    llvm = Path(sysroot) / "lib/rustlib" / host / "bin"
    profile_list = output / "profiles.txt"
    profile_list.write_text("\n".join(map(str, raw)) + "\n")
    merged = output / "coverage.profdata"
    run([llvm / "llvm-profdata", "merge", "-sparse", "-f", profile_list, "-o", merged],
        root=root, environment=environment, log=logs / "profile-merge.log")
    explicit_objects = [item["path"] for item in sorted(objects.values(), key=lambda obj: obj["path"])]
    command = [llvm / "llvm-cov", "export", "--format=lcov", "--instr-profile=" + str(merged), explicit_objects[0]]
    command += ["--object=" + path for path in explicit_objects[1:]]
    command += [str(root / name) for name in frozen]
    lcov = run(command, root=root, environment=environment, log=logs / "llvm-export.log", capture=True)
    (output / "coverage.lcov").write_text(lcov)
    scope_environment = dict(os.environ)
    scope_environment["CARGO_TARGET_DIR"] = str(output / "scope-build")
    spans = run([options.cargo, "run", *arguments, "--quiet", "--manifest-path",
                 root / "tools/coverage-scope/Cargo.toml", "--", root],
                root=root, environment=scope_environment, log=logs / "scope.log", capture=True)
    json.loads(spans)
    (output / "test-spans.json").write_text(spans)
    check_sources(root, frozen)
    git = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True, check=True).stdout.strip()
    manifest = {"schema": 1, "root": str(root), "sources": frozen, "objects": list(objects.values()),
                "workflows": options.workflows, "executed_workflows": [name for name, _, _ in jobs],
                "test_suite": options.test_suite, "test_executables": len(executed_tests),
                "executed_test_commands": executed_tests, "compiled_test_executables": len(tests),
                "git_head": git, "rustc": version,
                "coverage_lcov": "coverage.lcov", "coverage_sha256": digest(output / "coverage.lcov"),
                "test_spans_sha256": digest(output / "test-spans.json"), "source_hashes_verified": True,
                "profile_files": len(raw), "status": "passed"}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("Rust collection passed: " + str(output / "manifest.json"), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True, help="New isolated report/profile directory; must not exist")
    parser.add_argument("--target-dir", type=Path, help="Optional reusable build cache; exact Cargo JSON objects are still frozen and verified")
    parser.add_argument("--workflows", choices=["all", "dav", "smb", "none"], default="all")
    parser.add_argument("--test-suite", choices=["all", "portable-smb"], default="all",
                        help="all runs every regular Rust test; portable-smb runs only SMB library tests and transfer operations before Samba fixtures")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--cargo-llvm-cov", default="cargo-llvm-cov")
    options = parser.parse_args(argv)
    if options.test_suite == "portable-smb" and options.workflows != "smb":
        parser.error("portable-smb requires --workflows smb; it supplements the macOS all-test collection")
    collect(options)
    return 0


if __name__ == "__main__":
    sys.exit(main())
