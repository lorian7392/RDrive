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
        self.assertEqual(len(jobs), 20)
        by_name = {name: (command, extra) for name, command, extra in jobs}
        self.assertIn("coverage_cli", by_name)
        self.assertEqual(by_name["mcp_startup"][0][-2:], ["--report", str(self.root / "mcp_startup.json")])
        self.assertEqual(by_name["profile_admin"][0][-2:], ["--report", str(self.root / "profile_admin.json")])
        self.assertEqual(by_name["state_directory"][0][-2:], ["--report", str(self.root / "state_directory.json")])
        self.assertEqual(by_name["smb"][1]["RDRIVE_SMB_TEST_RUNNER"], "/lib-tests")
        self.assertNotIn("RDRIVE_SMB_PROVIDER_ONLY", by_name["smb"][1])
        self.assertEqual(by_name["smb-lifecycle"][1]["RDRIVE_SMB_PROOF_ONLY"], "1")
        self.assertEqual(by_name["smb-smart"][1]["RDRIVE_SMB_SMART"], "1")
        self.assertEqual(runner.workflows("python", Path("/binary"), "none", self.root, Path("/lib-tests")), [])
        self.assertEqual(len(runner.workflows("python", Path("/binary"), "dav", self.root, Path("/lib-tests"))), 16)

    def test_library_fixtures_are_isolated_without_filtering_the_full_suite(self):
        library = Path("/current/lib-tests")
        obj = {"path": str(library)}
        self.assertEqual(runner.test_command(obj, library, "all"),
                         [str(library), "--test-threads=1"])
        self.assertEqual(runner.test_command(obj, library, "portable-smb"),
                         [str(library), "smb::", "--test-threads=1"])
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
