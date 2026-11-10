import contextlib
import importlib.util
import io
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("coverage_report", Path(__file__).with_name("report_coverage.py"))
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
manifest_spec = importlib.util.spec_from_file_location("source_manifest", Path(__file__).with_name("source_manifest.py"))
source_manifest = importlib.util.module_from_spec(manifest_spec)
manifest_spec.loader.exec_module(source_manifest)


class CoverageReportTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="rdrive-coverage-report-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        for name in ["src/lib.rs", "src/main.rs", "src/test_module.rs", "macos/Shared/One.swift", "macos/tests/Test.swift", "macos/build/Ignored.swift", *report.PYTHON_SOURCES]:
            self.write(name, "line one\nline two\nline three\n")

    def write(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return path

    def inputs(self):
        rust = self.write("rust.lcov", "SF:src/main.rs\nDA:1,0\nDA:2,0\nDA:3,9\nend_of_record\nSF:" + str(self.root / "src/main.rs") + "\nDA:1,2\nDA:2,0\nend_of_record\nSF:/outside/dependency.rs\nDA:1,1\nend_of_record\n")
        spans = self.write("spans.json", json.dumps({"files": {"src/lib.rs": [], "src/main.rs": [{"start": 3, "end": 3}], "src/test_module.rs": [{"start": 1, "end": 3}]}, "production_function_items": {"src/lib.rs": 0, "src/main.rs": 1}}))
        swift = self.write("swift.lcov", "SF:macos/Shared/One.swift\nDA:1,2\nend_of_record\n")
        python = self.write("python.json", json.dumps({"files": {name: {"executed_lines": [1], "missing_lines": [2], "executed_branches": [[1, 2]], "missing_branches": [[1, 3]]} for name in report.PYTHON_SOURCES}}))
        return rust, spans, swift, python

    def test_union_uses_actual_hits_and_preserves_uncovered_production(self):
        rust, spans, swift, python = self.inputs()
        value = report.build_report(self.root, rust, spans, swift, [python])
        self.assertEqual(value["languages"]["rust"]["lines"], 2)
        self.assertEqual(value["languages"]["rust"]["covered"], 1)
        self.assertEqual(value["languages"]["rust"]["files"][1]["missing"], [2])
        self.assertEqual(value["languages"]["rust"]["files"][0]["status"], "no_executable_items")
        self.assertEqual(value["languages"]["swift"]["lines"], 1)
        self.assertEqual(value["total"]["lines"], 11)
        self.assertEqual(len(value["languages"]["python"]["files"][0]["sha256"]), 64)
        self.assertTrue(value["total"]["scope_complete"])

    def test_missing_mappings_and_ast_sources_are_not_silently_dropped(self):
        rust, spans, swift, python = self.inputs()
        rust.write_text("")
        value = report.rust_report(self.root, rust, spans)
        self.assertFalse(value["scope_complete"])
        self.assertIsNone(value["percent"])
        self.assertEqual(value["not_instrumented"], ["src/main.rs"])
        self.write("src/new.rs", "fn new() {}")
        with self.assertRaisesRegex(ValueError, "scope missing source"):
            report.rust_report(self.root, rust, spans)
        swift.write_text("")
        self.assertEqual(report.swift_report(self.root, swift)["not_instrumented"], ["macos/Shared/One.swift"])
        missing = self.write("none.json", '{"files": {}}')
        self.assertFalse(report.python_report(self.root, [missing])["scope_complete"])

    def test_python_merges_separate_runs_and_branches_without_double_counting(self):
        _, _, _, python = self.inputs()
        other = self.write("other.json", json.dumps({"files": {str(self.root / report.PYTHON_SOURCES[0]): {"executed_lines": [2], "missing_lines": [1], "executed_branches": [[1, 3]], "missing_branches": [[1, 2]]}, "/outside/tool.py": {"executed_lines": [1], "missing_lines": []}}}))
        result = report.python_report(self.root, [python, other])["files"][0]
        self.assertEqual((result["lines"], result["covered"], result["missing"]), (2, 2, []))
        self.assertEqual(result["branches"], {"count": 2, "covered": 2})

    def test_lcov_rejects_bad_lines_and_ignores_records_without_a_source(self):
        path = self.write("bad.lcov", "DA:7,1\nSF:src/main.rs\nDA:0,1\n")
        with self.assertRaises(ValueError):
            report.lcov(self.root, path)
        path.write_text("SF:src/main.rs\nDA:4,1\n")
        with self.assertRaisesRegex(ValueError, "outside current source"):
            report.lcov(self.root, path)

    def test_empty_source_records_cannot_satisfy_instrumentation(self):
        rust, spans, swift, python = self.inputs()
        rust.write_text("SF:src/main.rs\nend_of_record\n")
        swift.write_text("SF:macos/Shared/One.swift\nend_of_record\n")
        self.assertEqual(report.rust_report(self.root, rust, spans)["not_instrumented"], ["src/main.rs"])
        self.assertEqual(report.swift_report(self.root, swift)["not_instrumented"], ["macos/Shared/One.swift"])
        python.write_text(json.dumps({"files": {name: {"executed_lines": [], "missing_lines": []} for name in report.PYTHON_SOURCES}}))
        self.assertFalse(report.python_report(self.root, [python])["scope_complete"])
        python.write_text(json.dumps({"files": {report.PYTHON_SOURCES[0]: {"executed_lines": [4], "missing_lines": []}}}))
        with self.assertRaisesRegex(ValueError, "outside current source"):
            report.python_report(self.root, [python])

    def test_frozen_manifests_reject_changed_missing_and_outside_sources(self):
        name = "src/main.rs"
        digest = hashlib.sha256((self.root / name).read_bytes()).hexdigest()
        path = self.write("manifest.json", json.dumps({"sources": {name: digest}}))
        self.assertEqual(report.verify_source_manifest(self.root, path)["verified_sources"], [name])
        path.write_text(json.dumps([{"source": name, "sha256": digest}]))
        self.assertEqual(report.verify_source_manifest(self.root, path)["verified_sources"], [name])
        path.write_text(json.dumps({"sources": {name: "0" * 64}}))
        with self.assertRaisesRegex(ValueError, "Source changed"):
            report.verify_source_manifest(self.root, path)
        for sources in [{}, {"src/missing.rs": digest}, {"../outside.rs": digest}]:
            path.write_text(json.dumps({"sources": sources}))
            with self.assertRaises(ValueError):
                report.verify_source_manifest(self.root, path)
        artifact = self.write("coverage.lcov", "Frozen coverage\n")
        path.write_text(json.dumps({"sources": {name: digest}, "coverage_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest()}))
        self.assertIn("coverage.lcov", report.verify_source_manifest(self.root, path)["verified_artifacts"])
        artifact.write_text("Substituted coverage\n")
        with self.assertRaisesRegex(ValueError, "artifact changed"):
            report.verify_source_manifest(self.root, path)
        path.write_text("SF:src/main.rs\nDA:1,-1\n")
        with self.assertRaises(ValueError):
            report.lcov(self.root, path)

    def test_production_python_exclusions_fail_instead_of_shrinking_denominator(self):
        path = self.write("excluded.json", json.dumps({"files": {report.PYTHON_SOURCES[0]: {"excluded_lines": [2], "executed_lines": [1], "missing_lines": []}}}))
        with self.assertRaisesRegex(ValueError, "exclusions are not allowed"):
            report.python_report(self.root, [path])

    def test_gate_applies_to_every_language_and_writes_failure_evidence(self):
        rust, spans, swift, python = self.inputs()
        output = self.root / "report/coverage.json"
        args = ["--root", str(self.root), "--rust-lcov", str(rust), "--rust-test-spans", str(spans), "--swift-lcov", str(swift), "--python-json", str(python), "--output", str(output)]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(report.main(args), 1)
            self.assertTrue(output.exists())
            self.assertEqual(report.main(args + ["--min-line", "50"]), 0)
            with self.assertRaises(SystemExit):
                report.main(args + ["--min-line", "101"])
        self.assertEqual(json.loads(output.read_text())["languages"]["rust"]["percent"], 50)

    def test_full_gate_requires_complete_verified_source_provenance(self):
        rust, spans, swift, python = self.inputs()
        rust.write_text("SF:src/main.rs\nDA:1,1\nDA:2,1\nend_of_record\n")
        python.write_text(json.dumps({"files": {name: {"executed_lines": [1, 2], "missing_lines": []} for name in report.PYTHON_SOURCES}}))
        output = self.root / "gate.json"
        args = ["--root", str(self.root), "--rust-lcov", str(rust), "--rust-test-spans", str(spans), "--swift-lcov", str(swift), "--python-json", str(python), "--output", str(output)]
        manifest = self.write("frozen.json", json.dumps({"sources": source_manifest.snapshot(self.root)}))
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(report.main(args), 1)
            self.assertFalse(json.loads(output.read_text())["source_provenance_complete"])
            self.assertEqual(report.main(args + ["--source-manifest", str(manifest)]), 0)
            sources = json.loads(manifest.read_text())
            del sources["sources"][report.PYTHON_SOURCES[0]]
            manifest.write_text(json.dumps(sources))
            self.assertEqual(report.main(args + ["--source-manifest", str(manifest)]), 1)

    def test_snapshot_is_exclusive_and_detects_hash_or_inventory_changes(self):
        output = self.root / "snapshots/sources.json"
        args = ["--root", str(self.root), "--output", str(output)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(source_manifest.main(args), 0)
            self.assertEqual(source_manifest.main(args + ["--check"]), 0)
            with self.assertRaises(FileExistsError):
                source_manifest.main(args)
            frozen = json.loads(output.read_text())["sources"]
            self.assertNotIn("macos/tests/Test.swift", frozen)
            self.assertNotIn("macos/build/Ignored.swift", frozen)
            self.write("src/new.rs", "fn additional() {}\n")
            with self.assertRaisesRegex(ValueError, "inventory or hashes changed"):
                source_manifest.main(args + ["--check"])
            (self.root / "src/new.rs").unlink()
            self.write("src/main.rs", "changed\n")
            with self.assertRaises(ValueError):
                source_manifest.main(args + ["--check"])


if __name__ == "__main__":
    unittest.main()
