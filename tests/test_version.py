"""Check version/tag consistency with disposable executables and App plists."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import plistlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("check_version", Path(__file__).with_name("check_version.py"))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class VersionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="rdrive-version-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.manifest = self.root / "Cargo.toml"
        self.manifest.write_text('[package]\nname="rdrive"\nversion="0.8.0"\n')
        self.binary = self.executable(self.root / "rdrive")
        self.app = self.root / "Fixture.app"
        for relative, identifier in check.BUNDLES.items():
            self.plist(relative, CFBundleIdentifier=identifier,
                       CFBundleShortVersionString="0.8.0", CFBundleVersion="10")
        self.executable(self.app / "Contents/Resources/rdrive")

    def executable(self, path, version="0.8.0", stderr=False, status=0):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/usr/bin/env python3\nimport sys\nassert sys.argv[1:] == ['--version']\n" +
                        f"print('rdrive {version}')\n" +
                        ("print('unexpected diagnostics', file=sys.stderr)\n" if stderr else "") +
                        f"sys.exit({status})\n")
        path.chmod(0o700)
        return path

    def plist(self, relative, **values):
        path = self.app / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(plistlib.dumps(values))

    def test_exact_version_tag_and_all_native_bundles_agree(self):
        value = check.validate(self.manifest, self.binary, self.app, "v0.8.0")
        self.assertEqual(value["tag"], "v0.8.0")
        self.assertEqual(value["embedded_cli_version"], "rdrive 0.8.0")
        self.assertEqual(len(value["bundle_versions"]), 4)
        self.assertEqual(len(value["cli_sha256"]), 64)
        self.assertEqual(check.validate(self.manifest, self.binary)["bundle_versions"], {})

    def test_malformed_or_different_tags_are_rejected_before_cli_execution(self):
        self.binary.unlink()
        for tag in ["0.8.0", "v0.8", "v0.8.1", "v00.8.0", "v0.8.0-extra", "v0.8.0\n", "v$(touch nope)"]:
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "exactly v0.8.0"):
                check.validate(self.manifest, self.binary, tag=tag)
        self.assertFalse((self.root / "nope").exists())

    def test_semantic_prerelease_and_build_metadata_tags_are_supported(self):
        version = "1.2.3-rc.1+build.4"
        self.manifest.write_text(f'[package]\nversion="{version}"\n')
        self.executable(self.binary, version)
        self.assertEqual(check.validate(self.manifest, self.binary, tag="v" + version)["version"], version)
        for invalid in ["01.2.3", "1.2", "1.2.3-01", "1.2.3-", "1.2.3+", "1.2.3\n"]:
            self.manifest.write_text('[package]\nversion=' + json.dumps(invalid) + '\n')
            with self.subTest(version=invalid), self.assertRaisesRegex(ValueError, "semantic version"):
                check.validate(self.manifest, self.binary)

    def test_cli_or_embedded_cli_mismatch_and_stderr_fail(self):
        self.executable(self.binary, "0.7.0")
        with self.assertRaisesRegex(ValueError, "CLI version differs"):
            check.validate(self.manifest, self.binary)
        self.executable(self.binary, stderr=True)
        with self.assertRaises(ValueError):
            check.validate(self.manifest, self.binary)
        self.executable(self.binary)
        self.executable(self.app / "Contents/Resources/rdrive", "0.7.0")
        with self.assertRaisesRegex(ValueError, "CLI version differs"):
            check.validate(self.manifest, self.binary, self.app)

    def test_every_bundle_requires_matching_identity_version_and_build(self):
        for relative, identifier in check.BUNDLES.items():
            for key, value in [("CFBundleIdentifier", "wrong"), ("CFBundleShortVersionString", "0.7.0"), ("CFBundleVersion", "")]:
                values = dict(CFBundleIdentifier=identifier, CFBundleShortVersionString="0.8.0", CFBundleVersion="10")
                values[key] = value
                self.plist(relative, **values)
                with self.subTest(bundle=relative, field=key), self.assertRaises(ValueError):
                    check.validate(self.manifest, self.binary, self.app)
                self.plist(relative, CFBundleIdentifier=identifier, CFBundleShortVersionString="0.8.0", CFBundleVersion="10")
        relative, identifier = list(check.BUNDLES.items())[-1]
        self.plist(relative, CFBundleIdentifier=identifier, CFBundleShortVersionString="0.8.0", CFBundleVersion="11")
        with self.assertRaisesRegex(ValueError, "build numbers differ"):
            check.validate(self.manifest, self.binary, self.app)

    def test_main_writes_report_only_after_successful_validation(self):
        output = self.root / "report/version.json"
        arguments = ["--manifest", str(self.manifest), "--binary", str(self.binary), "--app", str(self.app), "--tag", "v0.8.0", "--output", str(output)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(check.main(arguments), 0)
        self.assertEqual(json.loads(output.read_text())["version"], "0.8.0")
        output.unlink()
        self.executable(self.binary, status=2)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
            check.main(arguments)
        self.assertEqual(failure.exception.code, 1)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
