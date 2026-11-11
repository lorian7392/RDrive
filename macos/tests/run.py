#!/usr/bin/env python3
"""Compile and exercise native SDK objects against an isolated Unix-socket peer."""
import argparse
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import uuid

ROOT = Path(__file__).resolve().parents[1]
BUILD_SPEC = importlib.util.spec_from_file_location("rdrive_native_build", ROOT / "build.py")
BUILD_MODULE = importlib.util.module_from_spec(BUILD_SPEC)
BUILD_SPEC.loader.exec_module(BUILD_MODULE)


PRODUCTION_SOURCES = [
    "Shared/Bridge.swift", "Shared/TagData.swift", "Shared/NativeBudget.swift",
    "FileProvider/ProviderItem.swift", "FileProvider/Enumerator.swift", "FileProvider/Provider.swift",
    "Companion/DomainWatcher.swift", "Companion/main.swift", "FinderSync/FinderSync.swift",
    "FileProviderUI/StatusViewController.swift",
]
TESTED_SOURCES = [source for source in PRODUCTION_SOURCES if source != "Companion/main.swift"]


def export_coverage(output, binary, objects, profiles, tested_sources, safe_control_tests=False, namespace_policy=False):
    """Use every production mapping, including zero-hit compile-only components."""
    raw = sorted(profiles.glob("*.profraw"))
    if not raw:
        raise RuntimeError("No Swift profiles were produced by the isolated native tests")
    profile = output / "coverage.profdata"
    subprocess.run(["xcrun", "llvm-profdata", "merge", "-sparse", *map(str, raw), "-o", str(profile)], check=True)
    arguments = [str(binary), "-instr-profile=" + str(profile)]
    for obj in objects:
        arguments += ["-object=" + str(obj)]
    sources = [str(ROOT / name) for name in PRODUCTION_SOURCES]
    arguments += sources
    with (output / "coverage.json").open("w") as result:
        subprocess.run(["xcrun", "llvm-cov", "export", *arguments], stdout=result, check=True)
    with (output / "coverage.lcov").open("w") as result:
        subprocess.run(["xcrun", "llvm-cov", "export", "-format=lcov", *arguments], stdout=result, check=True)
    report = json.loads((output / "coverage.json").read_text())
    indexed = {Path(file["filename"]).resolve(): file for unit in report["data"] for file in unit["files"]}
    # Swift closure/generic instantiations can count a physical source line
    # multiple times in LLVM's LF/summary counters. DA records identify the
    # executable source lines directly; merge duplicate mappings by max hits.
    unique_lines = {}
    current = None
    for record in (output / "coverage.lcov").read_text().splitlines():
        if record.startswith("SF:"):
            current = Path(record[3:]).resolve()
            unique_lines.setdefault(current, {})
        elif current is not None and record.startswith("DA:"):
            line, hits, *_ = record[3:].split(",")
            lines = unique_lines[current]
            lines[int(line)] = max(lines.get(int(line), 0), int(hits))
        elif record == "end_of_record":
            current = None
    manifest = []
    for name in PRODUCTION_SOURCES:
        source = ROOT / name
        covered = indexed.get(source.resolve())
        manifest.append({
            "source": "macos/" + name,
            "sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "status": "instrumented" if covered is not None else "not_instrumented",
            "test_execution": "isolated_host_tests" if name in tested_sources else ("isolated_controller_and_safe_control_tests" if safe_control_tests and name == "Companion/main.swift" else "compile_only_zero_hit_mapping"),
            "llvm_summary": covered["summary"] if covered is not None else None,
            "unique_lines": {
                "count": len(unique_lines.get(source.resolve(), {})),
                "covered": sum(hits > 0 for hits in unique_lines.get(source.resolve(), {}).values()),
                "uncovered": [line for line, hits in sorted(unique_lines.get(source.resolve(), {}).items()) if hits == 0],
            } if covered is not None else None,
        })
    missing = [entry["source"] for entry in manifest if entry["status"] != "instrumented"]
    lines = [entry["unique_lines"] for entry in manifest if entry["unique_lines"] is not None]
    count = sum(value["count"] for value in lines)
    covered = sum(value["covered"] for value in lines)
    summary = {
        "scope": "All ten first-party native Swift production sources; tests are excluded",
        "production_sources": len(PRODUCTION_SOURCES),
        "instrumented_sources": len(PRODUCTION_SOURCES) - len(missing),
        "not_instrumented": missing,
        "scope_complete": not missing,
        "sdk_namespace_policy": namespace_policy,
        "executable_lines": count,
        "covered_lines": covered,
        "line_percent": 100 * covered / count if count and not missing else None,
        "line_metric": "Unique (production source file, executable line) from LLVM LCOV DA records; duplicate instantiations use max hits. Original LLVM counters are preserved separately.",
        "note": "Compile-only components remain in the denominator with zero hits. No domains were registered, no native UI was opened, and no OS cache eviction was requested.",
        "sources": manifest,
    }
    (output / "coverage-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (output / "coverage-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: summary[key] for key in ["scope_complete", "instrumented_sources", "production_sources", "covered_lines", "executable_lines", "line_percent"]}))
    if missing:
        raise RuntimeError("Swift coverage mappings are incomplete: " + ", ".join(missing))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--coverage", action="store_true", help="Collect actual LLVM coverage with all ten production Swift files in scope")
    parser.add_argument("--output-dir", type=Path, default=ROOT.parent / "target/native-tests", help="Isolated build/report directory (default: target/native-tests)")
    options = parser.parse_args()
    output = options.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    binary = output / "RDriveNativeTests"
    sdk = subprocess.check_output(["xcrun", "--show-sdk-path"], text=True).strip()
    architecture = subprocess.check_output(["uname", "-m"], text=True).strip()
    compiler = ["xcrun", "swiftc", "-swift-version", "5", "-sdk", sdk, "-target", architecture + "-apple-macosx13.0",
                "-module-cache-path", str(output / "module-cache")]
    if options.coverage:
        compiler += ["-profile-generate", "-profile-coverage-mapping", "-D", "RDRIVE_DEVELOPMENT"]
    namespace_policy = BUILD_MODULE.namespace_policy_supported(compiler)
    if namespace_policy:
        compiler += ["-D", "RDRIVE_HAS_NAMESPACE_POLICY"]
    frameworks = ["-framework", "Foundation", "-framework", "FileProvider", "-framework", "UniformTypeIdentifiers",
                  "-framework", "AppKit", "-framework", "FinderSync", "-framework", "FileProviderUI"]
    # Compile the actual Companion source as a testable module, renaming only
    # its generated entry symbol. Importing it cannot execute the App bootstrap;
    # the bootstrap remains mapped and is exercised separately by safe controls.
    companion_sources = [*PRODUCTION_SOURCES[:3], "Companion/DomainWatcher.swift", "Companion/main.swift"]
    companion_object = output / "CompanionCoverage.o"
    companion_library = output / "libRDriveCompanionCoverage.dylib"
    subprocess.run([*compiler, "-emit-object", "-whole-module-optimization", "-emit-module", "-enable-testing",
                    "-emit-module-path", str(output / "RDriveCompanionCoverage.swiftmodule"),
                    "-module-name", "RDriveCompanionCoverage", "-Xfrontend", "-entry-point-function-name",
                    "-Xfrontend", "rdrive_test_companion_entry", *[str(ROOT / source) for source in companion_sources],
                    *frameworks, "-o", str(companion_object)], check=True)
    subprocess.run([*compiler, "-emit-library", str(companion_object), *frameworks,
                    "-o", str(companion_library)], check=True)
    linking = ["-I", str(output), "-L", str(output), "-lRDriveCompanionCoverage",
               "-Xlinker", "-rpath", "-Xlinker", str(output)]
    subprocess.run([*compiler, "-parse-as-library", "-module-name", "RDriveNativeTests",
                    *[str(ROOT / source) for source in TESTED_SOURCES], str(ROOT / "tests/NativeTests.swift"), *map(str, sorted((ROOT / "tests").glob("coverage_*.swift"))),
                    *frameworks, *linking, "-o", str(binary)], check=True)
    objects = [companion_library]
    profiles = output / ("profiles-" + uuid.uuid4().hex)
    if options.coverage:
        profiles.mkdir(mode=0o700)
        # Compile every remaining production source for its real executable-line
        # mapping. These binaries/libraries are deliberately never run or loaded.
        components = {
            "Companion": [*PRODUCTION_SOURCES[:3], "Companion/DomainWatcher.swift", "Companion/main.swift"],
        }
        for name, sources in components.items():
            obj = output / ("RDriveCoverage" + name + ("" if name == "Companion" else ".dylib"))
            library = [] if name == "Companion" else ["-parse-as-library", "-emit-library"]
            subprocess.run([*compiler, *library, "-module-name", "RDriveCoverage" + name,
                            *[str(ROOT / source) for source in sources], *frameworks, "-o", str(obj)], check=True)
            objects.append(obj)
    errors = []
    calls = []
    with tempfile.TemporaryDirectory(prefix="rdnative-", dir="/private/tmp") as directory:
        group = Path(directory)
        group.chmod(0o700)
        def item(index):
            return {"id": "stable-" + str(index), "parent_id": "root", "name": str(index) + ".txt", "kind": "file", "size": 19,
                    "modified": None, "content_version": "v1", "metadata_version": "m1", "pinned": False, "capabilities": ["read"]}
        def serve():
            while not stopped.is_set():
                try:
                    client, _ = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    return
                try:
                    with client:
                        raw = b""
                        while b"\n" not in raw:
                            part = client.recv(65536)
                            if not part:
                                break
                            raw += part
                        if not raw:
                            continue
                        request = json.loads(raw.split(b"\n")[0])
                        calls.append(request)
                        assert request["v"] == 1 and request.get("request_id")
                        if token is not None:
                            assert request.get("auth_token") == token, "Missing private local IPC nonce"
                        if request["op"] == "test.cancel":
                            while client.recv(1024):
                                pass
                            continue
                        if request["op"] == "test.fragmented":
                            frames = [{"progress": {"completed": 19, "total": 19}}, {"ok": True, "value": "fragmented reply"}]
                        elif request["op"] == "test.indeterminate-progress":
                            frames = [{"progress": {"completed": 3, "total": None, "bytes_per_second": 7, "eta_seconds": None}}, {"progress": {}}, {"ok": True}]
                        elif request["op"] == "test.authentication":
                            frames = [{"ok": False, "error": {"code": "not_authenticated", "message": "Synthetic test failure"}}]
                        elif request["op"] == "test.oversized":
                            client.sendall(b"x" * (64 * 1024 * 1024 + 65536)); continue
                        elif request["op"] == "test.nonobject":
                            client.sendall(b"[]\n"); continue
                        elif request["op"] == "test.malformed":
                            client.sendall(b"{\n"); continue
                        elif request["op"] == "test.disconnected":
                            continue
                        elif request["op"] == "test.error":
                            frames = [{"ok": False}]
                        elif request["op"] == "test.missing-item":
                            frames = [{"ok": True}]
                        elif request.get("domain") == "provider-error":
                            frames = [{"ok": False, "error": {"code": "version_conflict", "message": "Synthetic provider conflict"}}]
                        elif request.get("domain") == "provider-cancel":
                            while client.recv(1024):
                                pass
                            continue
                        elif request["op"] == "item":
                            frames = [{"progress": {"completed": 999, "total": 1000}}, {"ok": True, "item": item(0)}]
                        elif request["op"] in ["create", "modify"]:
                            if "content_path" in request:
                                assert Path(request["content_path"]).read_bytes() == b"provider-create"
                            row = item(0)
                            row["parent_id"] = request.get("parent_id", "root")
                            row["name"] = request.get("name", "new.txt")
                            frames = ([{"progress": {"completed": 7, "total": len(b"provider-create")}}] if "content_path" in request else [])
                            frames += [{"ok": True, "item": row}]
                        elif request["op"] == "fetch":
                            transfers = group / "Transfers"
                            transfers.mkdir(exist_ok=True)
                            staged = transfers / ("provider-" + uuid.uuid4().hex)
                            outside = group / "provider-outside-content"
                            outside.write_bytes(b"provider-content")
                            domain = request.get("domain")
                            if domain == "provider-outside":
                                staged = outside
                            elif domain == "provider-link":
                                staged.symlink_to(outside)
                            elif domain == "provider-directory":
                                staged.mkdir()
                            else:
                                staged.write_bytes(b"provider-content")
                            frames = [{"progress": {"completed": len(b"provider-content"), "total": len(b"provider-content")}}, {"ok": True}] if domain == "provider-fetch-missing" else [
                                {"progress": {"completed": len(b"provider-content"), "total": len(b"provider-content")}}, {"ok": True, "item": item(0), "content_path": str(staged)}]
                        elif request["op"] in ["delete", "pin", "native_error"]:
                            frames = [{"ok": False, "error": {"code": "version_conflict"}}] if request.get("domain") == "provider-pin-error" else [{"ok": True}]
                        elif request["op"] == "refresh_index":
                            assert set(request) - {"auth_token", "request_id", "v"} == {"op", "domain", "id"}
                            assert request["domain"] in ("provider-tests", "provider-refresh-no-ack")
                            if request["id"] == "stable-0":
                                frames = [{"ok": False, "error": {"code": "unsupported", "message": "Select a directory"}}]
                            else:
                                assert request["id"] in ("folder-one", "root")
                                frames = [{"ok": True, "queued": request["domain"] == "provider-tests", "completed": False}]
                        elif request["op"] == "cache_budget":
                            frames = [{"ok": True, "max_bytes": 1024, "used_bytes": 0, "reserved_bytes": 0}]
                        elif request["op"] == "domains":
                            fixture = group / "transfer-progress-fixture.json"
                            value = json.loads(fixture.read_bytes()) if fixture.exists() else {}
                            frames = [{"ok": True, "domains": value.get("domains", [])}]
                        elif request["op"] == "transfer_status":
                            fixture = group / "transfer-progress-fixture.json"
                            value = json.loads(fixture.read_bytes()) if fixture.exists() else {}
                            frames = [value.get("reply", {"ok": True, "transfers": value.get("transfers", [])})]
                        elif request["op"] == "list":
                            domain = request.get("domain")
                            if domain == "missing-list":
                                frames = [{"ok": True, "anchor": "2"}]
                            elif domain == "missing-list-anchor":
                                frames = [{"ok": True, "items": []}]
                            elif domain == "invalid-list-item":
                                frames = [{"ok": True, "items": [{"name": "invalid"}], "anchor": "2"}]
                            else:
                                frames = [{"ok": True, "items": [item(i) for i in range(3)], "anchor": "2"}]
                        elif request["op"] == "changes":
                            if request.get("domain") == "missing-changes":
                                frames = [{"ok": True, "items": [], "anchor": "2"}]
                            elif request.get("domain") == "missing-changes-anchor":
                                frames = [{"ok": True, "items": [], "deleted": []}]
                            elif request.get("parent_id") == "folder-one":
                                child = item(4); child["parent_id"] = "folder-one"
                                frames = [{"ok": True, "items": [child, item(5)], "deleted": [], "anchor": "2", "more": False}]
                            else:
                                frames = [{"ok": True, "items": [item(i) for i in range(3)], "deleted": ["old-id"], "anchor": "999" if request.get("anchor") == "current" else "2", "more": False}]
                        else:
                            raise AssertionError("Unexpected native operation: " + str(request.get("op")))
                        payload = b"".join(json.dumps(frame).encode() + b"\n" for frame in frames)
                        for start in range(0, len(payload), 7):
                            client.sendall(payload[start:start + 7])
                except (BrokenPipeError, ConnectionResetError):
                    # A cancelled request may close while a fragmented reply is
                    # being delivered; production callback assertions still run.
                    pass
                except OSError as error:
                    if error.errno != errno.ENOTCONN:
                        errors.append(error)
                except Exception as error:
                    errors.append(error)
        for transport in ["unix", "development-tcp"]:
            errors.clear(); calls.clear()
            token = uuid.uuid4().hex + uuid.uuid4().hex if transport == "development-tcp" else None
            listener = socket.socket(socket.AF_INET if token else socket.AF_UNIX, socket.SOCK_STREAM)
            if token:
                listener.bind(("127.0.0.1", 0))
                endpoint = group / "bridge-endpoint.json"
                endpoint.write_text(json.dumps({"transport": "tcp", "host": "127.0.0.1", "port": listener.getsockname()[1], "token": token}))
                endpoint.chmod(0o600)
            else:
                listener.bind(str(group / "bridge.sock"))
                os.chmod(group / "bridge.sock", 0o600)
            listener.listen(); listener.settimeout(0.2)
            stopped = threading.Event()
            worker = threading.Thread(target=serve, daemon=True)
            worker.start()
            try:
                arguments = [str(binary), str(group)] + (["--development-tcp"] if token else [])
                environment = os.environ.copy()
                if options.coverage:
                    environment["LLVM_PROFILE_FILE"] = str(profiles / (transport + "-%p-%m.profraw"))
                subprocess.run(arguments, check=True, timeout=60, env=environment)
                if options.coverage:
                    # Exercise only control branches that exit before every OS
                    # domain-manager/UI operation. Never launch the App run loop.
                    companion = output / "RDriveCoverageCompanion"
                    control_cases = [
                        (["--unexpected-test-option"], 2),
                        (["--pin", "synthetic"], 2),
                        (["--unpin", "synthetic"], 2),
                        (["--check-cache", "synthetic"], 2),
                        (["--trim-cache", "not-a-number"], 2),
                        (["--trim-cache", "-1"], 2),
                        (["--check-cache", "synthetic", "stable-0", "--group-directory", "relative"], 1),
                        (["--check-cache", "synthetic", "stable-0", "--group-directory", str(group / "missing")], 1),
                        (["--check-cache", "synthetic", "stable-0", "--group-directory", str(group / "public-test")], 1),
                    ]
                    synthetic_domain = "coverage-" + uuid.uuid4().hex
                    synthetic_file = group / "control-file.txt"
                    synthetic_file.write_text("Private control fixture")
                    # Read-only SDK failures for an unregistered random domain
                    # and an ordinary temporary file do not register/change it.
                    control_cases.extend([
                        (["--domain-root", synthetic_domain], 1),
                        (["--resolve-file", str(synthetic_file)], 1),
                    ])
                    if token:
                        # The returned item is non-Smart, so reserve returns nil
                        # and --check-cache exits before any manager invocation.
                        control_cases.append((["--check-cache", "synthetic", "stable-0", "--group-directory", str(group)], 0))
                    for index, (arguments, expected_status) in enumerate(control_cases):
                        environment["LLVM_PROFILE_FILE"] = str(profiles / (transport + "-control-" + str(index) + "-%p-%m.profraw"))
                        result = subprocess.run([str(companion), *arguments], env=environment,
                                                capture_output=True, text=True, timeout=10)
                        assert result.returncode == expected_status, (arguments, result.returncode, result.stdout, result.stderr)
                        message = json.loads(result.stdout)
                        assert message["ok"] == (expected_status == 0), (arguments, message)
                assert not errors, errors
                assert len([c for c in calls if c["op"] == "list" and c.get("domain") == "fixture"]) == 1, "Paging must reuse one stable snapshot"
                assert [c["id"] for c in calls if c["op"] == "refresh_index"] == [
                    "folder-one", "root", "root", "stable-0", "root"]
            finally:
                stopped.set(); listener.close(); worker.join(timeout=2)

    if options.coverage:
        export_coverage(output, binary, objects, profiles, TESTED_SOURCES,
                        safe_control_tests=True, namespace_policy=namespace_policy)


if __name__ == "__main__":
    main()
