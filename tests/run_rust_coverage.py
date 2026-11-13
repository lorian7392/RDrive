#!/usr/bin/env python3
"""Measure Rust with exact Cargo artifacts, fresh profiles and frozen sources.

This is development infrastructure. It never activates the application, mounts
storage, reads Keychain credentials, or changes a user's service. The optional
Samba fixture requires Docker and uses disposable synthetic shares.
"""
import argparse
import datetime
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
MAX_DIAGNOSTIC_PROFILES = 8
MAX_DIAGNOSTIC_FILE_BYTES = 16 * 1024 * 1024
MAX_DIAGNOSTIC_TOTAL_BYTES = 32 * 1024 * 1024
MAX_DIAGNOSTIC_LINES = 16
MAX_DIAGNOSTIC_LINE_CHARS = 2048


def phase_label(value):
    """Keep fixed phase names filename-safe without accepting LLVM escapes."""
    return re.sub(r"[^a-z0-9_-]+", "-", value.lower()).strip("-_")[:64] or "phase"


def phase_environment(environment, profiles, phase):
    return dict(environment, LLVM_PROFILE_FILE=str(
        profiles / ("rdrive-" + phase_label(phase) + "-%p-%16m.profraw")))


def timestamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


class PhaseLog:
    def __init__(self, path):
        self.path = path
        self.records = []
        self.save()

    def save(self):
        temporary = self.path.with_suffix(".tmp")
        temporary.write_text(json.dumps({"schema": 1, "phases": self.records}, indent=2) + "\n")
        temporary.replace(self.path)

    def start(self, phase, command, environment, log):
        record = {"phase": phase_label(phase), "command": list(map(str, command)),
                  "profile_pattern": environment.get("LLVM_PROFILE_FILE"),
                  "log": str(log), "started": timestamp(), "ended": None,
                  "exit_code": None, "status": "running"}
        self.records.append(record)
        self.save()
        return record

    def finish(self, record, status, code, elapsed, error=None):
        record.update(ended=timestamp(), status=status, exit_code=code, elapsed_seconds=elapsed)
        if error is not None:
            record["error_type"] = type(error).__name__
        self.save()


def profile_phases(name, records):
    matches = []
    for phase in records:
        pattern = phase.get("profile_pattern")
        if pattern and fnmatch.fnmatchcase(name, re.sub(r"%\d*[a-zA-Z]", "*", Path(pattern).name)):
            matches.append(phase["phase"])
    return matches


def profile_evidence(path, directory, remaining):
    """Hash the reported regular file; cap copied evidence, never merge inputs."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise ValueError("Reported profile is not a regular file")
        complete = before.st_size <= MAX_DIAGNOSTIC_FILE_BYTES and before.st_size <= remaining
        copy_limit = before.st_size if complete else min(128, remaining)
        name = path.name if complete else path.name + ".header"
        destination = directory / name
        checksum, header, copied, hashed = hashlib.sha256(), b"", 0, 0
        with destination.open("xb") as preserved:
            while block := source.read(1024 * 1024):
                checksum.update(block)
                header = (header + block[:128])[:128]
                hashed += len(block)
                selected = block[:max(0, copy_limit - copied)]
                preserved.write(selected)
                copied += len(selected)
        after = os.fstat(source.fileno())
    stable = (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns)
    complete = complete and stable and hashed == before.st_size and copied == hashed
    return {"path": str(path), "size": before.st_size, "mtime_ns": before.st_mtime_ns,
            "sha256": checksum.hexdigest(), "header_hex": header.hex(), "bytes_hashed": hashed,
            "stable_during_read": stable,
            "preserved_file": name, "preserved_bytes": copied, "full_copy": complete,
            "preservation_status": "changed_during_read" if not stable else (
                "complete" if complete else "header_only_limit" if copied else "metadata_only_limit")}, copied


def preserve_merge_failure(output, raw, phases, error):
    """Preserve only known raw inputs explicitly named in LLVM diagnostics."""
    directory = output / "profile-merge-diagnostics"
    directory.mkdir()
    known = {path.name: path for path in raw}
    reported, lines, omitted_lines = [], [], 0
    log = output / "logs/profile-merge.log"
    if log.exists():
        with log.open(errors="replace") as source:
            for line in source:
                if not re.search(
                        r"warning:|error:|malformed|corrupt|invalid|truncated|unsupported|cannot|unable|failed|bad magic", line, re.I):
                    continue
                if len(lines) < MAX_DIAGNOSTIC_LINES:
                    lines.append(line.rstrip()[:MAX_DIAGNOSTIC_LINE_CHARS])
                else:
                    omitted_lines += 1
                names = set(re.findall(r"[^\s/\\'\"():]+\.profraw", line))
                for name, path in known.items():
                    if (name in names or str(path) in line) and name not in reported:
                        reported.append(name)
    remaining = MAX_DIAGNOSTIC_TOTAL_BYTES
    files = []
    for name in reported[:MAX_DIAGNOSTIC_PROFILES]:
        path = known[name]
        try:
            evidence, used = profile_evidence(path, directory, remaining)
            remaining -= used
            evidence["phases"] = profile_phases(name, phases.records)
            files.append(evidence)
        except (OSError, ValueError) as failure:
            files.append({"path": str(path), "error_type": type(failure).__name__,
                          "error": str(failure)[:512], "phases": profile_phases(name, phases.records),
                          "full_copy": False, "preserved_bytes": 0, "preservation_status": "unavailable"})
    report = {"schema": 1, "merge_error_type": type(error).__name__,
              "merge_exit_code": getattr(error, "returncode", None), "raw_input_count": len(raw),
              "reported_profile_count": len(reported), "omitted_profiles": max(0, len(reported) - len(files)),
              "diagnostic_lines": lines, "omitted_diagnostic_lines": omitted_lines, "profiles": files,
              "limits": {"profiles": MAX_DIAGNOSTIC_PROFILES, "full_file_bytes": MAX_DIAGNOSTIC_FILE_BYTES,
                         "total_preserved_bytes": MAX_DIAGNOSTIC_TOTAL_BYTES},
              "merge_retried": False, "merge_inputs_changed": False}
    (directory / "diagnostics.json").write_text(json.dumps(report, indent=2) + "\n")


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
            command.extend(["smb::", "status::"])
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
            ("mcp", "--report"), ("mcp_startup", "--report"), ("index", "--output"),
            ("search", "--report"), ("policy", "--output"),
            ("service", "--output"), ("profile_admin", "--report"),
            ("state_directory", "--report"), ("history", "--report"),
            ("checkpoint", "--report"),
            ("status", "--report"),
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


def run(command, *, root, environment, log, capture=False, timeout=1800,
        phases=None, phase=None, profiles=None):
    if profiles is not None:
        environment = phase_environment(environment, profiles, phase)
    record = phases.start(phase, command, environment, log) if phases else None
    started = time.monotonic()
    print("RUN " + shlex.join(map(str, command)), flush=True)
    result = None
    try:
        result = subprocess.run(list(map(str, command)), cwd=root, env=environment,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, timeout=timeout)
        try:
            log.write_text(result.stdout + "\n--- stderr ---\n" + result.stderr)
        except OSError as failure:
            if not result.returncode:
                raise
            print("Could not save failed command log: " + str(failure), file=sys.stderr)
        if result.returncode:
            print((result.stdout + result.stderr)[-12000:], file=sys.stderr)
            raise subprocess.CalledProcessError(result.returncode, command)
    except BaseException as error:
        if isinstance(error, subprocess.TimeoutExpired):
            text = lambda value: value.decode(errors="replace") if isinstance(value, bytes) else value or ""
            try:
                log.write_text(text(error.stdout) + "\n--- stderr ---\n" + text(error.stderr))
            except OSError as failure:
                print("Could not save timed-out command log: " + str(failure), file=sys.stderr)
        if phases:
            try:
                phases.finish(record, "failed", result.returncode if result else None,
                              time.monotonic() - started, error)
            except OSError as failure:
                print("Could not finish coverage phase evidence: " + str(failure), file=sys.stderr)
        raise
    if phases:
        phases.finish(record, "passed", result.returncode, time.monotonic() - started)
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
    phases = PhaseLog(output / "phases.json")
    target = options.target_dir.resolve() if options.target_dir else output / "build"
    frozen = sources(root)
    environment = dict(os.environ, CARGO_LLVM_COV_TARGET_DIR=str(target), CARGO_TARGET_DIR=str(target))
    def execute(phase, command, *, log=None, capture=False, environment_override=None):
        return run(command, root=root,
                   environment=environment if environment_override is None else environment_override,
                   log=log or logs / (phase + ".log"), capture=capture,
                   phases=phases, phase=phase, profiles=profiles)
    show = execute("show-env", [options.cargo_llvm_cov, "llvm-cov", "show-env", "--export-prefix"], capture=True)
    environment.update(parse_environment(show))
    arguments = ["--locked"] + (["--offline"] if options.offline else [])
    objects = {}
    for name, cargo_args in [("cli-build", ["build", "--bin", "rdrive"]),
                             ("test-build", ["test", "--tests", "--no-run"])]:
        output_json = execute(name, [options.cargo, *cargo_args, *arguments, "--message-format=json"], capture=True)
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
        execute(f"test-{index:02d}-" + obj["name"], command)
        executed_tests.append(command)
    jobs = workflows(sys.executable, binary, options.workflows, output, library)
    # Fixture selectors are test controls; ambient leftovers must not turn a
    # full Samba run into a smaller subset while still claiming all workflows.
    for name in ("RDRIVE_SMB_LIFECYCLE", "RDRIVE_SMB_PROOF_ONLY", "RDRIVE_SMB_PROVIDER_ONLY",
                 "RDRIVE_SMB_SMART", "RDRIVE_SMB_NOTIFY_ONLY", "RDRIVE_SMB_REPORT", "RDRIVE_SMB_TEST_RUNNER"):
        environment.pop(name, None)
    for name, command, extra in jobs:
        execute("workflow-" + name, command, log=logs / (name + ".log"),
                environment_override=dict(environment, **extra))
    check_sources(root, frozen)
    for obj in objects.values():
        if digest(Path(obj["path"])) != obj["sha256"]:
            raise RuntimeError("Instrumented object changed during coverage: " + obj["path"])
    raw = sorted(profiles.glob("*.profraw"))
    if not raw:
        raise RuntimeError("No Rust profiles produced")
    sysroot = execute("sysroot", [options.rustc, "--print", "sysroot"], capture=True).strip()
    version = execute("rustc", [options.rustc, "-vV"], capture=True)
    host = next(line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: "))
    llvm = Path(sysroot) / "lib/rustlib" / host / "bin"
    profile_list = output / "profiles.txt"
    profile_list.write_text("\n".join(map(str, raw)) + "\n")
    merged = output / "coverage.profdata"
    try:
        execute("profile-merge", [llvm / "llvm-profdata", "merge", "-sparse", "-f", profile_list, "-o", merged])
    except Exception as error:
        try:
            preserve_merge_failure(output, raw, phases, error)
        except Exception as failure:
            print("Could not preserve merge failure evidence: " + str(failure), file=sys.stderr)
        raise
    explicit_objects = [item["path"] for item in sorted(objects.values(), key=lambda obj: obj["path"])]
    command = [llvm / "llvm-cov", "export", "--format=lcov", "--instr-profile=" + str(merged), explicit_objects[0]]
    command += ["--object=" + path for path in explicit_objects[1:]]
    command += [str(root / name) for name in frozen]
    lcov = execute("llvm-export", command, capture=True)
    (output / "coverage.lcov").write_text(lcov)
    scope_environment = dict(os.environ)
    scope_environment["CARGO_TARGET_DIR"] = str(output / "scope-build")
    spans = execute("scope", [options.cargo, "run", *arguments, "--quiet", "--manifest-path",
                    root / "tools/coverage-scope/Cargo.toml", "--", root],
                    environment_override=scope_environment, capture=True)
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
                "phase_manifest": "phases.json", "phase_manifest_sha256": digest(phases.path),
                "profile_files": len(raw), "status": "passed"}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("Rust collection passed: " + str(output / "manifest.json"), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True, help="New isolated report/profile directory; must not exist")
    parser.add_argument("--target-dir", type=Path, help="Optional reusable build cache; exact Cargo JSON objects are still frozen and verified")
    parser.add_argument("--workflows", choices=["all", "dav", "smb", "none"], default="all")
    parser.add_argument("--test-suite", choices=["all", "portable-smb"], default="all",
                        help="all runs every regular Rust test; portable-smb runs SMB and portable status library tests and transfer operations before Samba fixtures")
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
