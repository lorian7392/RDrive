import importlib.util
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runner = load("rust_coverage_runner", "run_rust_coverage.py")
merger = load("rust_coverage_merger", "merge_rust_coverage.py")


class RustCoverageToolsTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="rdrive-coverage-tools-")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {\n    println!(\"hello\");\n}\n")

    def test_environment_is_parsed_without_shell_evaluation(self):
        text = "export RUSTC_WRAPPER='/a path/wrapper'\nCARGO_LLVM_COV=1\nFLAGS='literal $(touch nope) `whoami`'\nRUSTFLAGS='-C\x1finstrument-coverage'\n"
        values = runner.parse_environment(text)
        self.assertEqual(values["RUSTC_WRAPPER"], "/a path/wrapper")
        self.assertEqual(values["FLAGS"], "literal $(touch nope) `whoami`")
        self.assertEqual(values["RUSTFLAGS"], "-C\x1finstrument-coverage")
        for invalid in ["CARGO_LLVM_COV=1", "RUSTC_WRAPPER=x\nCARGO_LLVM_COV=1 extra", "RUSTC_WRAPPER=x\nCARGO_LLVM_COV=1\nbad-name=x"]:
            with self.assertRaises(ValueError):
                runner.parse_environment(invalid)

    def test_only_current_cargo_executables_are_selected(self):
        def item(path, source, **extra):
            return dict(reason="compiler-artifact", executable=path,
                        target={"src_path": source, "kind": ["lib"], "name": "rdrive"},
                        profile={"test": True}, **extra)
        entries = [item("/tmp/exact-object", str(self.root / "src/main.rs")),
                   item("/tmp/dependency", "/outside/dep.rs"),
                   {"reason": "build-finished", "success": True}]
        found = runner.artifacts("plain message\n" + "\n".join(map(json.dumps, entries)), self.root)
        self.assertEqual([obj["path"] for obj in found], ["/private/tmp/exact-object"] if str(Path('/tmp').resolve()) == '/private/tmp' else ["/tmp/exact-object"])
        self.assertTrue(found[0]["test"])

    def test_workflow_inventory_includes_fault_and_all_smb_modes(self):
        jobs = runner.workflows("python", Path("/binary"), "all", self.root, Path("/lib-tests"))
        self.assertEqual(len(jobs), 22)
        by_name = {name: (command, extra) for name, command, extra in jobs}
        self.assertIn("coverage_cli", by_name)
        self.assertEqual(by_name["mcp_startup"][0][-2:], ["--report", str(self.root / "mcp_startup.json")])
        self.assertEqual(by_name["profile_admin"][0][-2:], ["--report", str(self.root / "profile_admin.json")])
        self.assertEqual(by_name["state_directory"][0][-2:], ["--report", str(self.root / "state_directory.json")])
        self.assertEqual(by_name["checkpoint"][0][-2:], ["--report", str(self.root / "checkpoint.json")])
        self.assertEqual(by_name["status"][0][-2:], ["--report", str(self.root / "status.json")])
        self.assertEqual(by_name["smb"][1]["RDRIVE_SMB_TEST_RUNNER"], "/lib-tests")
        self.assertNotIn("RDRIVE_SMB_PROVIDER_ONLY", by_name["smb"][1])
        self.assertEqual(by_name["smb-lifecycle"][1]["RDRIVE_SMB_PROOF_ONLY"], "1")
        self.assertEqual(by_name["smb-smart"][1]["RDRIVE_SMB_SMART"], "1")
        self.assertEqual(runner.workflows("python", Path("/binary"), "none", self.root, Path("/lib-tests")), [])
        self.assertEqual(len(runner.workflows("python", Path("/binary"), "dav", self.root, Path("/lib-tests"))), 18)

    def test_library_fixtures_are_isolated_without_filtering_the_full_suite(self):
        library = Path("/current/lib-tests")
        obj = {"path": str(library)}
        self.assertEqual(runner.test_command(obj, library, "all"),
                         [str(library), "--test-threads=1"])
        self.assertEqual(runner.test_command(obj, library, "portable-smb"),
                         [str(library), "smb::", "status::", "--test-threads=1"])
        external = {"path": "/current/coverage_operations"}
        for suite in ["all", "portable-smb"]:
            self.assertEqual(runner.test_command(external, library, suite),
                             ["/current/coverage_operations"])
        self.assertEqual(obj, {"path": str(library)})

    def test_frozen_inventory_rejects_changed_added_and_removed_sources(self):
        frozen = runner.sources(self.root)
        runner.check_sources(self.root, frozen)
        source = self.root / "src/main.rs"
        original = source.read_text()
        source.write_text(original + "// changed\n")
        with self.assertRaisesRegex(RuntimeError, "source changed"):
            runner.check_sources(self.root, frozen)
        source.write_text(original)
        (self.root / "src/new.rs").write_text("fn new() {}")
        with self.assertRaises(RuntimeError):
            runner.check_sources(self.root, frozen)
        (self.root / "src/new.rs").unlink()
        source.unlink()
        with self.assertRaises(RuntimeError):
            runner.check_sources(self.root, frozen)

    def test_run_preserves_failure_output_and_never_masks_nonzero_exit(self):
        log = self.root / "command.log"
        completed = subprocess.CompletedProcess(["fake"], 17, "synthetic stdout", "synthetic stderr")
        with mock.patch.object(runner.subprocess, "run", return_value=completed):
            with self.assertRaises(subprocess.CalledProcessError) as failure:
                runner.run(["fake"], root=self.root, environment={}, log=log)
        self.assertEqual(failure.exception.returncode, 17)
        self.assertIn("synthetic stderr", log.read_text())

    def test_phase_profile_names_are_sanitized_without_changing_runtime_tokens(self):
        values = {"workflow Smart/../../%p ☃": "workflow-smart-p", "___ / %m": "m", "???": "phase"}
        original = {"LLVM_PROFILE_FILE": "ambient", "KEEP": "unchanged"}
        for label, expected in values.items():
            environment = runner.phase_environment(original, self.root, label)
            self.assertEqual(Path(environment["LLVM_PROFILE_FILE"]).name, f"rdrive-{expected}-%p-%16m.profraw")
            self.assertEqual(environment["KEEP"], "unchanged")
        self.assertEqual(original["LLVM_PROFILE_FILE"], "ambient")
        self.assertEqual(len(runner.phase_label("a" * 100)), 64)

    def test_phase_manifest_records_success_failure_timeout_and_exact_command(self):
        phases = runner.PhaseLog(self.root / "phases.json")
        command = ["literal $(not-a-shell)", "argument with spaces"]
        outcomes = [subprocess.CompletedProcess(command, 0, "passed", ""),
                    subprocess.CompletedProcess(command, 17, "partial", "failure")]
        for index, outcome in enumerate(outcomes):
            with mock.patch.object(runner.subprocess, "run", return_value=outcome) as execute:
                if index:
                    with self.assertRaises(subprocess.CalledProcessError):
                        runner.run(command, root=self.root, environment={}, log=self.root / f"{index}.log",
                                   phases=phases, phase=f"TEST/{index}", profiles=self.root)
                else:
                    runner.run(command, root=self.root, environment={}, log=self.root / f"{index}.log",
                               phases=phases, phase=f"TEST/{index}", profiles=self.root)
                self.assertEqual(execute.call_args.args[0], command)
                self.assertIn(f"rdrive-test-{index}-%p-%16m", execute.call_args.kwargs["env"]["LLVM_PROFILE_FILE"])
        timeout = subprocess.TimeoutExpired(command, 3, output=b"before timeout", stderr=b"partial diagnostic")
        with mock.patch.object(runner.subprocess, "run", side_effect=timeout), self.assertRaises(subprocess.TimeoutExpired) as failure:
            runner.run(command, root=self.root, environment={}, log=self.root / "timeout.log",
                       phases=phases, phase="timeout", profiles=self.root)
        self.assertIs(failure.exception, timeout)
        self.assertIn("partial diagnostic", (self.root / "timeout.log").read_text())
        records = json.loads(phases.path.read_text())["phases"]
        self.assertEqual([record["exit_code"] for record in records], [0, 17, None])
        self.assertEqual([record["status"] for record in records], ["passed", "failed", "failed"])
        self.assertEqual(records[-1]["error_type"], "TimeoutExpired")
        for record in records:
            self.assertEqual(record["command"], command)
            self.assertIsNotNone(record["started"])
            self.assertIsNotNone(record["ended"])
            self.assertGreaterEqual(record["elapsed_seconds"], 0)

    def test_spawn_failure_keeps_original_exception_and_finishes_phase(self):
        phases = runner.PhaseLog(self.root / "phases.json")
        original = FileNotFoundError("missing tool")
        with mock.patch.object(runner.subprocess, "run", side_effect=original), self.assertRaises(FileNotFoundError) as failure:
            runner.run(["missing"], root=self.root, environment={}, log=self.root / "missing.log",
                       phases=phases, phase="missing", profiles=self.root)
        self.assertIs(failure.exception, original)
        self.assertEqual(json.loads(phases.path.read_text())["phases"][0]["error_type"], "FileNotFoundError")

    def merge_fixture(self, names):
        output = self.root / "output with spaces"
        (output / "profiles").mkdir(parents=True)
        (output / "logs").mkdir()
        paths = []
        for name, content in names:
            path = output / "profiles" / name
            path.write_bytes(content)
            paths.append(path)
        phases = runner.PhaseLog(output / "phases.json")
        phases.records = [{"phase": "workflow-smart", "profile_pattern": str(output / "profiles/rdrive-workflow-smart-%p-%16m.profraw")}]
        return output, paths, phases

    def test_actual_corrupt_header_message_preserves_only_reported_input(self):
        bad_name = "rdrive-workflow-smart-9684-5469779597299527299_4.profraw"
        output, raw, phases = self.merge_fixture([(bad_name, b"corrupt header\0\1"), ("valid.profraw", b"valid bytes")])
        log = f"warning: {raw[0]}: invalid instrumentation profile data (file header is corrupt)\nerror: no profile can be merged\nRead {raw[1]} successfully\n"
        (output / "logs/profile-merge.log").write_text(log)
        original = subprocess.CalledProcessError(1, ["llvm-profdata", "merge"])
        runner.preserve_merge_failure(output, raw, phases, original)
        directory = output / "profile-merge-diagnostics"
        report = json.loads((directory / "diagnostics.json").read_text())
        self.assertEqual(report["reported_profile_count"], 1)
        self.assertIn("error: no profile can be merged", report["diagnostic_lines"])
        self.assertFalse(report["merge_retried"] or report["merge_inputs_changed"])
        evidence = report["profiles"][0]
        self.assertEqual(evidence["size"], raw[0].stat().st_size)
        self.assertEqual(evidence["mtime_ns"], raw[0].stat().st_mtime_ns)
        self.assertEqual(evidence["sha256"], hashlib.sha256(raw[0].read_bytes()).hexdigest())
        self.assertEqual(evidence["header_hex"], raw[0].read_bytes().hex())
        self.assertEqual(evidence["phases"], ["workflow-smart"])
        self.assertEqual(evidence["preservation_status"], "complete")
        self.assertEqual((directory / evidence["preserved_file"]).read_bytes(), raw[0].read_bytes())
        self.assertFalse((directory / "valid.profraw").exists())

    def test_evidence_file_byte_and_diagnostic_bounds_are_explicit(self):
        output, raw, phases = self.merge_fixture([(f"bad-{index}.profraw", b"x" * size) for index, size in enumerate([4, 12, 16])])
        lines = [f"warning: {path}: file header is corrupt " + "x" * 4000 for path in raw]
        (output / "logs/profile-merge.log").write_text("\n".join(lines * 10))
        with mock.patch.object(runner, "MAX_DIAGNOSTIC_PROFILES", 2), mock.patch.object(runner, "MAX_DIAGNOSTIC_FILE_BYTES", 8), mock.patch.object(runner, "MAX_DIAGNOSTIC_TOTAL_BYTES", 8):
            runner.preserve_merge_failure(output, raw, phases, subprocess.CalledProcessError(1, "merge"))
        report = json.loads((output / "profile-merge-diagnostics/diagnostics.json").read_text())
        self.assertEqual(len(report["profiles"]), 2)
        self.assertEqual(report["omitted_profiles"], 1)
        self.assertLessEqual(sum(profile["preserved_bytes"] for profile in report["profiles"]), 8)
        self.assertEqual(report["profiles"][1]["preservation_status"], "header_only_limit")
        self.assertFalse(report["profiles"][1]["full_copy"])
        self.assertEqual(report["profiles"][1]["sha256"], hashlib.sha256(raw[1].read_bytes()).hexdigest())
        self.assertEqual(len(report["diagnostic_lines"]), runner.MAX_DIAGNOSTIC_LINES)
        self.assertTrue(all(len(line) <= runner.MAX_DIAGNOSTIC_LINE_CHARS for line in report["diagnostic_lines"]))
        self.assertGreater(report["omitted_diagnostic_lines"], 0)
        self.assertLess((output / "profile-merge-diagnostics/diagnostics.json").stat().st_size, 40000)

    def test_evidence_never_follows_reported_symlink(self):
        output, raw, phases = self.merge_fixture([])
        outside = self.root / "outside.txt"
        outside.write_text("must not be copied")
        link = output / "profiles/linked.profraw"
        link.symlink_to(outside)
        (output / "logs/profile-merge.log").write_text(f"warning: {link}: invalid instrumentation profile\n")
        runner.preserve_merge_failure(output, [link], phases, subprocess.CalledProcessError(1, "merge"))
        report = json.loads((output / "profile-merge-diagnostics/diagnostics.json").read_text())
        self.assertEqual(report["profiles"][0]["preservation_status"], "unavailable")
        self.assertEqual(list((output / "profile-merge-diagnostics").glob("*.profraw")), [])

    def fake_collection(self, *, merge_exit=0):
        objects = self.root / "objects"
        objects.mkdir()
        binary, library = objects / "rdrive", objects / "rdrive-tests"
        binary.write_bytes(b"compiled CLI")
        library.write_bytes(b"compiled tests")
        output = self.root / "collection"
        options = mock.Mock(output_dir=output, target_dir=None, cargo="cargo", rustc="rustc",
                            cargo_llvm_cov="cargo-llvm-cov", offline=True, workflows="none", test_suite="all")
        def artifact(path, kind, is_test):
            return json.dumps({"reason": "compiler-artifact", "executable": str(path),
                               "target": {"src_path": str(self.root / "src/main.rs"), "name": "rdrive", "kind": [kind]},
                               "profile": {"test": is_test}})
        def execute(command, **kwargs):
            first = Path(command[0]).name
            stdout, stderr, code = "", "", 0
            if first == "cargo-llvm-cov":
                stdout = "RUSTC_WRAPPER=wrapper\nCARGO_LLVM_COV=1\n"
            elif first == "cargo" and command[1] == "build":
                stdout = artifact(binary, "bin", False)
            elif first == "cargo" and command[1] == "test":
                stdout = artifact(library, "lib", True)
            elif first == library.name:
                path = Path(kwargs["env"]["LLVM_PROFILE_FILE"].replace("%p", "9684").replace("%16m", "5469779597299527299_4"))
                path.write_bytes(b"synthetic raw header")
            elif first == "rustc":
                stdout = str(self.root / "toolchain") + "\n" if "--print" in command else "host: aarch64-apple-darwin\n"
            elif first == "llvm-profdata":
                code = merge_exit
                raw = next((output / "profiles").glob("*.profraw"))
                stderr = f"warning: {raw}: invalid instrumentation profile data (file header is corrupt)\nerror: no profile can be merged\n" if code else ""
            elif first == "llvm-cov":
                stdout = "SF:" + str(self.root / "src/main.rs") + "\nDA:1,1\nend_of_record\n"
            elif first == "cargo" and command[1] == "run":
                stdout = '{"files":{"src/main.rs":[]}}'
            elif first == "git":
                stdout = "fixture-commit\n"
            else:
                self.fail(f"unexpected tool command: {command}")
            return subprocess.CompletedProcess(command, code, stdout, stderr)
        return output, options, execute

    def test_collection_binds_phase_manifest_and_keeps_merge_flags(self):
        output, options, execute = self.fake_collection()
        with mock.patch.object(runner, "ROOT", self.root), mock.patch.object(runner.subprocess, "run", side_effect=execute):
            runner.collect(options)
        manifest = json.loads((output / "manifest.json").read_text())
        self.assertEqual(manifest["phase_manifest_sha256"], runner.digest(output / "phases.json"))
        phases = json.loads((output / "phases.json").read_text())["phases"]
        self.assertTrue(all(phase["status"] == "passed" for phase in phases))
        self.assertTrue(all("-%p-%16m.profraw" in phase["profile_pattern"] for phase in phases))
        merge = next(phase for phase in phases if phase["phase"] == "profile-merge")
        self.assertEqual(merge["command"][1:4], ["merge", "-sparse", "-f"])
        self.assertEqual(len(list((output / "profiles").glob("*.profraw"))), 1)

    def test_merge_failure_preserves_provenance_and_original_exception_even_if_evidence_fails(self):
        for fail_evidence in [False, True]:
            with self.subTest(fail_evidence=fail_evidence), tempfile.TemporaryDirectory() as directory:
                previous = self.root
                self.root = Path(directory)
                (self.root / "src").mkdir()
                (self.root / "src/main.rs").write_text("fn main() {}\n")
                try:
                    output, options, execute = self.fake_collection(merge_exit=17)
                    errors = []
                    actual = runner.run
                    def remember(*args, **kwargs):
                        try:
                            return actual(*args, **kwargs)
                        except subprocess.CalledProcessError as error:
                            errors.append(error)
                            raise
                    with mock.patch.object(runner, "ROOT", self.root), mock.patch.object(runner.subprocess, "run", side_effect=execute), mock.patch.object(runner, "run", side_effect=remember):
                        if fail_evidence:
                            with mock.patch.object(runner, "preserve_merge_failure", side_effect=OSError("evidence unavailable")), self.assertRaises(subprocess.CalledProcessError) as failure:
                                runner.collect(options)
                        else:
                            with self.assertRaises(subprocess.CalledProcessError) as failure:
                                runner.collect(options)
                    self.assertIs(failure.exception, errors[0])
                    self.assertEqual(failure.exception.returncode, 17)
                    phases = json.loads((output / "phases.json").read_text())["phases"]
                    self.assertEqual(phases[-1]["phase"], "profile-merge")
                    self.assertEqual(phases[-1]["exit_code"], 17)
                    self.assertFalse((output / "manifest.json").exists())
                    if not fail_evidence:
                        self.assertTrue((output / "profile-merge-diagnostics/diagnostics.json").exists())
                finally:
                    self.root = previous

    def test_always_uploads_include_bounded_diagnostics_not_entire_profile_sets(self):
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/checks.yml").read_text()
        for platform in ["macos", "linux"]:
            self.assertIn(f"target/coverage/rust-{platform}/phases.json", workflow)
            self.assertIn(f"target/coverage/rust-{platform}/profile-merge-diagnostics/", workflow)
            self.assertNotIn(f"target/coverage/rust-{platform}/profiles/", workflow)
        self.assertGreaterEqual(workflow.count("if: always()"), 3)

    def collection(self, name, origin, hits, **changes):
        directory = self.root / name
        directory.mkdir()
        manifest = {"schema": 1, "root": origin, "sources": merger.source_hashes(self.root),
                    "status": "passed", "source_hashes_verified": True,
                    "git_head": "same-commit", "workflows": "dav", "test_suite": "all",
                    "rustc": "rustc 1.90.0\nhost: aarch64-apple-darwin\nLLVM version: 20.1.8\n"}
        manifest.update(changes)
        (directory / "coverage.lcov").write_text("SF:" + origin + "/src/main.rs\n" +
            "\n".join(f"DA:{line},{value}" for line, value in hits.items()) +
            "\nend_of_record\nSF:/dependency/out.rs\nDA:1,1\nend_of_record\n")
        manifest["coverage_sha256"] = hashlib.sha256((directory / "coverage.lcov").read_bytes()).hexdigest()
        (directory / "test-spans.json").write_text(json.dumps({"files": {"src/main.rs": []}}))
        manifest["test_spans_sha256"] = hashlib.sha256((directory / "test-spans.json").read_bytes()).hexdigest()
        (directory / "manifest.json").write_text(json.dumps(manifest))
        return directory

    def test_cross_host_union_counts_source_lines_once_and_keeps_zero_hits(self):
        mac = self.collection("mac", "/Users/runner/repo", {1: 1, 2: 0, 3: 0})
        linux = self.collection("linux", "/home/runner/repo", {1: 8, 2: 3, 3: 0})
        output = self.root / "merged/coverage.lcov"
        evidence = merger.merge(self.root, [mac, linux], output)
        self.assertEqual(evidence["sources"], merger.source_hashes(self.root))
        self.assertEqual(len(evidence["inputs"]), 2)
        text = output.read_text()
        self.assertEqual(text.count("DA:1,"), 1)
        self.assertIn("DA:1,8\nDA:2,3\nDA:3,0", text)
        self.assertIn("LF:3\nLH:2", text)
        self.assertNotIn("dependency", text)
        self.assertTrue(output.with_suffix(".manifest.json").exists())

    def test_merge_rejects_incomplete_different_or_unverified_sources(self):
        cases = [{"status": "failed"}, {"source_hashes_verified": False}, {"schema": 2},
                 {"sources": {}}, {"root": "relative/repo"}]
        for index, changes in enumerate(cases):
            directory = self.collection(str(index), "/same-root", {1: 1}, **changes)
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                merger.merge(self.root, [directory], self.root / "merged.lcov")
        first = self.collection("first", "/root1", {1: 1})
        second = self.collection("second", "/root2", {1: 1}, git_head="other-commit")
        with self.assertRaisesRegex(ValueError, "different Git"):
            merger.merge(self.root, [first, second], self.root / "merged.lcov")

    def test_first_inventory_defines_macos_target_and_cannot_omit_regular_tests(self):
        mac = self.collection("mac", "/mac", {1: 1, 2: 0})
        linux = self.collection("linux", "/linux", {1: 1, 2: 2, 3: 7},
                                rustc="host: x86_64-unknown-linux-gnu", test_suite="portable-smb", workflows="smb")
        output = self.root / "merged.lcov"
        evidence = merger.merge(self.root, [mac, linux], output)
        self.assertNotIn("DA:3,", output.read_text())
        self.assertIn("DA:2,2", output.read_text())
        self.assertEqual(evidence["inputs"][1]["lines_outside_macos_target_inventory"], 1)
        with self.assertRaisesRegex(ValueError, "First collection must be a macOS"):
            merger.merge(self.root, [linux, mac], output)
        partial = self.collection("partial", "/mac", {1: 1}, test_suite="portable-smb")
        with self.assertRaisesRegex(ValueError, "all regular tests"):
            merger.merge(self.root, [partial], output)

    def test_portable_subset_cannot_be_requested_without_samba_fixtures(self):
        with mock.patch.object(runner, "collect") as collect:
            with self.assertRaises(SystemExit):
                runner.main(["--output-dir", str(self.root / "new"), "--test-suite", "portable-smb", "--workflows", "none"])
        collect.assert_not_called()

    def test_report_replacement_is_detected_even_when_sources_still_match(self):
        directory = self.collection("run", "/mac", {1: 1, 2: 0})
        report = directory / "coverage.lcov"
        report.write_text(report.read_text().replace("DA:2,0", "DA:2,1"))
        with self.assertRaisesRegex(ValueError, "report changed"):
            merger.merge(self.root, [directory], self.root / "merged.lcov")

    def test_scope_replacement_is_detected_even_when_coverage_and_sources_match(self):
        directory = self.collection("run", "/mac", {1: 1, 2: 0})
        (directory / "test-spans.json").write_text('{"files": {"src/main.rs": [{"start": 1, "end": 3}]}}')
        with self.assertRaisesRegex(ValueError, "Test scope report changed"):
            merger.merge(self.root, [directory], self.root / "merged.lcov")

    def test_merge_rejects_outdated_bounds_negative_hits_and_empty_mappings(self):
        for index, hits in enumerate([{0: 1}, {4: 1}, {1: -1}, {}]):
            directory = self.collection(str(index), "/origin", hits)
            with self.subTest(hits=hits), self.assertRaises(ValueError):
                merger.merge(self.root, [directory], self.root / "merged.lcov")

    def test_existing_output_is_refused_before_running_any_command(self):
        options = mock.Mock(output_dir=self.root, target_dir=None)
        with mock.patch.object(runner, "run") as command:
            with self.assertRaises(FileExistsError):
                runner.collect(options)
        command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
