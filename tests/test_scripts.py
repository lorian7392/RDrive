"""Exercise production script entry points with temporary files and OS peers.

No compiler, signer, CLI, SDK lookup, installation, or native activation is run.
The build peer records requests and creates only the requested temporary binary
files. Assertions about those files concern packaging logic, not Apple tooling.
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import plistlib
import runpy
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
MCP = ROOT / "scripts/mcp-config.py"
INSTALL = ROOT / "scripts/install-skills.py"
BUILD = ROOT / "macos/build.py"


def load(path):
    spec = importlib.util.spec_from_file_location("rdrive_script_" + path.stem.replace("-", "_"), path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def invoke(module, arguments, *, entry=False):
    output, errors = io.StringIO(), io.StringIO()
    with mock.patch.object(sys, "argv", [module.__file__, *map(str, arguments)]), \
            contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
        if entry:
            result = runpy.run_path(module.__file__, run_name="__main__")
        else:
            result = module.main()
    return result, output.getvalue(), errors.getvalue()


class TemporaryFiles(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rdrive-script-tests-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()

    def write(self, relative, content=b"fixture"):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path


class MCPConfigurationTests(TemporaryFiles):
    def setUp(self):
        super().setUp()
        self.module = load(MCP)
        self.binary = self.write("bin/rdrive")
        self.home = self.root / "state"
        self.arguments = ["--binary", self.binary, "--home", self.home]

    def test_json_output_is_read_only_and_expands_local_arguments(self):
        first = self.root / "first"; first.mkdir()
        second = self.root / "第二个"; second.mkdir()
        _, output, errors = invoke(self.module, self.arguments + ["--allow-write", "--timeout", "9", "--local-root", first, "--local-root", second], entry=True)
        value = json.loads(output)["mcpServers"]["rdrive"]
        self.assertEqual(value["command"], str(self.binary))
        self.assertEqual(value["args"], ["--home", str(self.home), "mcp", "serve", "--timeout", "9", "--allow-write", "--local-root", str(first), "--local-root", str(second)])
        self.assertEqual(errors, "")
        self.assertFalse(self.home.exists())
        self.assertEqual(self.binary.read_bytes(), b"fixture")

    def test_toml_output_and_default_timeout(self):
        _, output, _ = invoke(self.module, self.arguments + ["--format", "toml"])
        lines = output.splitlines()
        self.assertEqual(lines[0], "[mcp_servers.rdrive]")
        self.assertEqual(json.loads(lines[1].split(" = ", 1)[1]), str(self.binary))
        self.assertEqual(json.loads(lines[2].split(" = ", 1)[1]), ["--home", str(self.home), "mcp", "serve", "--timeout", "300"])

    def test_invalid_executable_local_root_and_timeout_exit_before_writing(self):
        for arguments in [
            ["--home", self.home, "--binary", self.root / "missing"],
            self.arguments + ["--local-root", self.root / "missing"],
            self.arguments + ["--local-root", self.binary],
            self.arguments + ["--timeout", "0"],
            self.arguments + ["--timeout", "-1"],
            self.arguments + ["--timeout", "invalid"],
        ]:
            with self.subTest(arguments=arguments), self.assertRaises(SystemExit) as failure:
                invoke(self.module, arguments)
            self.assertEqual(failure.exception.code, 2)
            self.assertFalse(self.home.exists())


class SkillInstallationTests(TemporaryFiles):
    def setUp(self):
        super().setUp()
        self.module = load(INSTALL)
        self.module.__file__ = str(self.root / "fixture/scripts/install-skills.py")
        self.source = self.root / "fixture/skills"
        for name in self.module.SKILLS:
            self.write("fixture/skills/" + name + "/SKILL.md", (name + "\n").encode())
            self.write("fixture/skills/" + name + "/references/guide.txt", b"nested guide")
        self.target = self.root / "target"

    def assert_error(self, arguments, message):
        result, output, errors = invoke(self.module, arguments)
        self.assertEqual(result, 1)
        self.assertEqual(output, "")
        self.assertIn(message, json.loads(errors)["error"])

    def test_install_copies_files_and_nested_content_preserving_existing_permissions(self):
        self.target.mkdir(mode=0o750)
        sentinel = self.write("target/unrelated.txt", b"preserved")
        before_mode = stat.S_IMODE(self.target.stat().st_mode)
        result, output, errors = invoke(self.module, ["--target", self.target])
        self.assertEqual(result, 0)
        self.assertTrue(json.loads(output)["installed"])
        self.assertEqual(errors, "")
        for name in self.module.SKILLS:
            self.assertEqual((self.target / name / "SKILL.md").read_bytes(), (name + "\n").encode())
            self.assertEqual((self.target / name / "references/guide.txt").read_bytes(), b"nested guide")
        self.assertEqual(sentinel.read_bytes(), b"preserved")
        self.assertEqual(stat.S_IMODE(self.target.stat().st_mode), before_mode)

    def test_dry_run_deduplicates_and_uses_codex_home_without_creating_directories(self):
        codex = self.root / "codex-home"
        with mock.patch.dict(os.environ, {"CODEX_HOME": str(codex)}):
            result, output, _ = invoke(self.module, ["--dry-run", "--skill", "rdrive-files", "--skill", "rdrive-files"])
        self.assertEqual(result, 0)
        self.assertEqual(json.loads(output), {"dry_run": True, "target": str(codex / "skills"), "skills": ["rdrive-files"]})
        self.assertFalse(codex.exists())

    def test_default_home_and_real_script_entrypoint_are_confined_to_temp(self):
        with mock.patch.dict(os.environ, {"CODEX_HOME": ""}), mock.patch.object(Path, "home", return_value=self.root):
            result, output, _ = invoke(self.module, ["--dry-run"])
        self.assertEqual(result, 0)
        self.assertEqual(json.loads(output)["target"], str(self.root / ".codex/skills"))
        actual = load(INSTALL)
        with self.assertRaises(SystemExit) as complete:
            invoke(actual, ["--target", self.target, "--skill", "rdrive-transactions"], entry=True)
        self.assertEqual(complete.exception.code, 0)
        self.assertEqual((self.target / "rdrive-transactions/SKILL.md").read_bytes(), (ROOT / "skills/rdrive-transactions/SKILL.md").read_bytes())
        self.assertEqual(list(self.target.iterdir()), [self.target / "rdrive-transactions"])

    def test_missing_source_and_source_links_fail_without_creating_target(self):
        (self.source / "rdrive-files/SKILL.md").unlink()
        self.assert_error(["--target", self.target], "Bundled skill is missing")
        self.assertFalse(self.target.exists())
        (self.source / "rdrive-files/SKILL.md").write_text("restored")
        (self.source / "rdrive-files/references/link").symlink_to(self.source / "rdrive-smart-sync/SKILL.md")
        self.assert_error(["--target", self.target], "symbolic link")
        self.assertFalse(self.target.exists())

    def test_target_link_file_and_existing_entries_are_never_replaced(self):
        actual = self.root / "actual"; actual.mkdir(mode=0o700)
        self.target.symlink_to(actual, target_is_directory=True)
        self.assert_error(["--target", self.target], "real directory")
        self.assertTrue(self.target.is_symlink()); self.assertEqual(list(actual.iterdir()), [])
        self.target.unlink(); self.target.write_bytes(b"original target file")
        self.assert_error(["--target", self.target], "real directory")
        self.assertEqual(self.target.read_bytes(), b"original target file")
        self.target.unlink(); self.target.mkdir()
        existing = self.target / "rdrive-files"; existing.symlink_to(self.root / "not-present")
        self.assert_error(["--target", self.target], "not be overwritten")
        self.assertTrue(existing.is_symlink())
        self.assertEqual(os.readlink(existing), str(self.root / "not-present"))

    def test_copy_failure_rolls_back_only_owned_destinations(self):
        self.target.mkdir(mode=0o750)
        sentinel = self.write("target/unrelated.txt", b"preserved")
        before_mode = stat.S_IMODE(self.target.stat().st_mode)
        actual_copy = self.module.shutil.copy2
        def fail_second(source, destination, *args, **kwargs):
            actual_copy(source, destination, *args, **kwargs)
            if Path(source).parent.name == "rdrive-smart-sync":
                raise OSError("injected copy failure after bytes")
        with mock.patch.object(self.module.shutil, "copy2", side_effect=fail_second):
            self.assert_error(["--target", self.target], "injected copy failure")
        self.assertEqual(list(self.target.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_bytes(), b"preserved")
        self.assertEqual(stat.S_IMODE(self.target.stat().st_mode), before_mode)

    def test_nested_copy_and_target_creation_failures_leave_no_skill(self):
        with mock.patch.object(self.module.shutil, "copytree", side_effect=OSError("injected nested copy")):
            self.assert_error(["--target", self.target], "injected nested copy")
        self.assertEqual(list(self.target.iterdir()), [])
        original_mkdir = Path.mkdir
        def fail_target(path, *args, **kwargs):
            if path == self.target:
                raise PermissionError("injected target permissions")
            return original_mkdir(path, *args, **kwargs)
        with mock.patch.object(Path, "mkdir", new=fail_target):
            self.assert_error(["--target", self.target], "injected target permissions")
        self.assertEqual(list(self.target.iterdir()), [])

    def test_concurrent_destination_is_preserved_while_prior_copy_is_rolled_back(self):
        original_mkdir = Path.mkdir
        concurrent = self.target / "rdrive-smart-sync"
        def race(path, *args, **kwargs):
            if path == concurrent:
                original_mkdir(path)
                (path / "concurrent-user-file").write_bytes(b"other install")
                raise FileExistsError("injected concurrent install")
            return original_mkdir(path, *args, **kwargs)
        with mock.patch.object(Path, "mkdir", new=race):
            self.assert_error(["--target", self.target], "injected concurrent install")
        self.assertEqual(list(self.target.iterdir()), [concurrent])
        self.assertEqual((concurrent / "concurrent-user-file").read_bytes(), b"other install")

    def test_failed_rollback_is_reported_and_partial_directory_remains_visible(self):
        with mock.patch.object(self.module.shutil, "copy2", side_effect=OSError("copy failed")), \
                mock.patch.object(self.module.shutil, "rmtree", side_effect=PermissionError("rollback denied")):
            self.assert_error(["--target", self.target], "rollback denied")
        self.assertTrue((self.target / "rdrive-files").is_dir())
        self.assertFalse((self.target / "rdrive-smart-sync").exists())
        self.assertFalse((self.target / "rdrive-transactions").exists())


class BuildPeer:
    def __init__(self, cli, *, fail_run=None, fail_query=None, namespace_policy=True):
        self.cli = cli
        self.calls = []
        self.queries = []
        self.fail_run = fail_run
        self.fail_query = fail_query
        self.namespace_policy = namespace_policy

    def query(self, arguments, *, text):
        self.queries.append(arguments)
        if self.fail_query and self.fail_query(arguments):
            raise subprocess.CalledProcessError(1, arguments, stderr="injected SDK lookup failure")
        if arguments == ["xcrun", "--show-sdk-path"]:
            return "/fixture/SDK\n"
        if arguments == ["uname", "-m"]:
            return "arm64\n"
        if arguments == [str(self.cli), "--version"]:
            return "rdrive 0.8.3\n"
        raise AssertionError("unexpected external query: " + repr(arguments))

    def run(self, arguments, *, check, **options):
        self.calls.append(arguments)
        if self.fail_run and self.fail_run(arguments):
            raise subprocess.CalledProcessError(1, arguments, stderr="injected build failure")
        if "-typecheck" in arguments:
            assert check is False and options["capture_output"] and options["text"]
            assert options["timeout"] == 120 and arguments[-1] == "-"
            supported = self.namespace_policy or "NSFileProviderNamespacePolicy" not in options["input"]
            return subprocess.CompletedProcess(arguments, 0 if supported else 1, "", "" if supported else "unsupported namespace policy")
        if arguments[:2] == ["xcrun", "swiftc"]:
            binary = Path(arguments[arguments.index("-o") + 1])
            binary.write_bytes(("synthetic compiled " + binary.name).encode())
        elif arguments[0] != "codesign":
            raise AssertionError("unexpected external command: " + repr(arguments))
        if not check:
            raise AssertionError("build command must check errors")


class NativeBuildScriptTests(TemporaryFiles):
    def setUp(self):
        super().setUp()
        self.module = load(BUILD)
        self.cli = self.write("input/rdrive", b"isolated CLI payload")
        self.output = self.root / "output"
        self.arguments = ["--cli", self.cli, "--output", self.output]

    @contextlib.contextmanager
    def peer(self, **kwargs):
        peer = BuildPeer(self.cli, **kwargs)
        with mock.patch.object(self.module.subprocess, "check_output", side_effect=peer.query), \
                mock.patch.object(self.module.subprocess, "run", side_effect=peer.run):
            yield peer

    def report(self):
        return json.loads((self.output / "build-report.json").read_text())

    def read_plist(self, relative):
        return plistlib.loads((self.output / relative).read_bytes())

    def previous_app(self):
        return self.write("output/RDrive.app/previous-build", b"previous binary")

    def assert_previous_preserved(self, previous):
        self.assertEqual(previous.read_bytes(), b"previous binary")
        self.assertFalse((self.output / "build-report.json").exists())

    def test_adhoc_entrypoint_builds_verified_temporary_bundles_and_truthful_report(self):
        previous = self.previous_app()
        with self.peer() as peer:
            _, output, _ = invoke(self.module, self.arguments, entry=True)
        report = self.report()
        self.assertEqual(json.loads(output), report)
        self.assertFalse(previous.exists())
        self.assertEqual(list(self.output.glob(".building-*")), [])
        self.assertEqual((self.output / "RDrive.app/Contents/Resources/rdrive").read_bytes(), self.cli.read_bytes())
        self.assertEqual(report["source_cli_sha256"], hashlib.sha256(self.cli.read_bytes()).hexdigest())
        self.assertEqual(report["embedded_cli_sha256"], report["source_cli_sha256"])
        self.assertEqual(report["cli_version"], "rdrive 0.8.3")
        self.assertEqual(report["version"], "0.8.3")
        self.assertEqual(report["bundle_build"], "19")
        self.assertEqual(report["app_group"], "group.io.rdrive.shared")
        self.assertIsNone(report["development_ipc_root"])
        self.assertFalse(report["distribution_ready"])
        self.assertFalse(report["installed"]); self.assertFalse(report["activated"])
        self.assertEqual(report["signing_status"], "ad-hoc review build; no Developer Team identity")
        self.assertEqual(len([call for call in peer.calls if call[:2] == ["xcrun", "swiftc"] and "-typecheck" not in call]), 4)
        self.assertTrue(report["sdk_namespace_policy"])
        self.assertTrue(all("RDRIVE_HAS_NAMESPACE_POLICY" in call for call in peer.calls if "-typecheck" not in call and call[:2] == ["xcrun", "swiftc"]))
        self.assertEqual(peer.calls[-1][:5], ["codesign", "--verify", "--deep", "--strict", "--verbose=2"])
        provider = self.read_plist("RDrive.app/Contents/PlugIns/RDriveProvider.appex/Contents/Info.plist")["NSExtension"]
        self.assertEqual(provider["NSExtensionFileProviderDocumentGroup"], "group.io.rdrive.shared")
        self.assertEqual(len(provider["NSFileProviderDecorations"]), 15)
        blocked = next(item for item in provider["NSFileProviderDecorations"]
                       if item["Identifier"] == "io.rdrive.native.provider.status.blocked")
        self.assertEqual(blocked["Label"], "Excluded by sync rules")
        self.assertEqual([action["NSExtensionFileProviderActionIdentifier"]
                          for action in provider["NSExtensionFileProviderActions"]],
                         ["rdrive.refresh", "rdrive.pin", "rdrive.unpin"])
        refresh = provider["NSExtensionFileProviderActions"][0]
        self.assertEqual(refresh["NSExtensionFileProviderActionName"], "Refresh")
        self.assertEqual(refresh["NSExtensionFileProviderActionActivationRule"],
                         "fileproviderItems.@count == 1 AND SUBQUERY(fileproviderItems, $item, "
                         "$item.userInfo.rdriveSmart == YES AND $item.contentType.identifier == 'public.folder').@count == 1")
        finder = self.read_plist("RDrive.app/Contents/PlugIns/RDriveFinder.appex/Contents/Info.plist")["NSExtension"]
        self.assertEqual(finder["NSExtensionAttributes"], {})
        status_ui = self.read_plist("RDrive.app/Contents/PlugIns/RDriveStatus.appex/Contents/Info.plist")["NSExtension"]
        self.assertEqual(status_ui["NSExtensionFileProviderActions"][0]["NSExtensionFileProviderActionIdentifier"], "rdrive.status")
        repair = status_ui["NSExtensionFileProviderActions"][1]
        self.assertEqual(repair["NSExtensionFileProviderActionIdentifier"], "rdrive.repair")
        self.assertEqual(repair["NSExtensionFileProviderActionName"], "Repair Sync Issue…")
        self.assertEqual(repair["NSExtensionFileProviderActionActivationRule"],
                         "fileproviderItems.@count == 1 AND SUBQUERY(fileproviderItems, $item, "
                         "$item.userInfo.rdriveSmart == YES AND $item.userInfo.rdriveState IN {'conflict', 'blocked', 'queued'} "
                         "AND $item.contentType.identifier == 'public.folder').@count == 1")
        self.assertTrue(self.read_plist("RDriveProvider.entitlements")["com.apple.security.app-sandbox"])

    def test_signing_team_and_explicit_app_group_are_reported(self):
        with self.peer() as peer:
            invoke(self.module, self.arguments + ["--identity", "Fixture Identity", "--team-id", "FIXTURETEAM"])
        report = self.report()
        self.assertEqual(report["app_group"], "FIXTURETEAM.io.rdrive.shared")
        self.assertTrue(report["distribution_ready"])
        self.assertEqual(report["signing_status"], "signed with requested identity")
        self.assertTrue(all("Fixture Identity" in call for call in peer.calls if "--sign" in call))
        with self.peer():
            invoke(self.module, self.arguments + ["--app-group", "group.fixture.explicit"])
        self.assertEqual(self.report()["app_group"], "group.fixture.explicit")

    def test_older_sdk_omits_only_unavailable_namespace_policy(self):
        with self.peer(namespace_policy=False) as peer:
            invoke(self.module, self.arguments)
        self.assertFalse(self.report()["sdk_namespace_policy"])
        compiles = [call for call in peer.calls if call[:2] == ["xcrun", "swiftc"] and "-typecheck" not in call]
        self.assertEqual(len(compiles), 4)
        self.assertTrue(all("RDRIVE_HAS_NAMESPACE_POLICY" not in call for call in compiles))
        probes = [call for call in peer.calls if "-typecheck" in call]
        self.assertEqual(len(probes), 2)
        self.assertEqual(probes[0], probes[1])
        self.assertFalse(self.report()["installed"])
        self.assertFalse(self.report()["activated"])

    def test_sdk_probe_failures_preserve_previous_app_before_staging(self):
        previous = self.previous_app()
        with self.peer(fail_run=lambda args: "-typecheck" in args), self.assertRaises(subprocess.CalledProcessError):
            invoke(self.module, self.arguments)
        self.assert_previous_preserved(previous)
        self.assertEqual(list(self.output.glob(".building-*")), [])

    def test_sdk_capability_probe_rejects_broken_imports_and_compiler_crashes(self):
        compiler = ["xcrun", "swiftc", "-sdk", "/fixture/SDK", "-target", "arm64-apple-macosx13.0"]
        success = subprocess.CompletedProcess(compiler, 0, "", "")
        failed = subprocess.CompletedProcess(compiler, 1, "", "broken FileProvider import")
        with mock.patch.object(self.module.subprocess, "run", return_value=failed) as run, self.assertRaises(subprocess.CalledProcessError) as error:
            self.module.namespace_policy_supported(compiler)
        self.assertEqual(error.exception.stderr, "broken FileProvider import")
        self.assertEqual(run.call_count, 1, "a broken SDK must not become an unavailable optional feature")
        crashed = subprocess.CompletedProcess(compiler, -11, "", "compiler crashed")
        with mock.patch.object(self.module.subprocess, "run", side_effect=[success, crashed]), self.assertRaises(subprocess.CalledProcessError) as error:
            self.module.namespace_policy_supported(compiler)
        self.assertEqual(error.exception.returncode, -11)
        for failure in [FileNotFoundError("missing compiler"), subprocess.TimeoutExpired(compiler, 120)]:
            with self.subTest(failure=type(failure).__name__), mock.patch.object(self.module.subprocess, "run", side_effect=failure), self.assertRaises(type(failure)):
                self.module.namespace_policy_supported(compiler)

    def test_development_build_uses_narrow_exception_and_removes_app_groups(self):
        development = self.root / "private-ipc"
        with self.peer() as peer:
            invoke(self.module, self.arguments + ["--identity", "Fixture Identity", "--development-ipc-root", development])
        self.assertEqual(stat.S_IMODE(development.stat().st_mode), 0o700)
        report = self.report()
        self.assertEqual(report["development_ipc_root"], str(development))
        self.assertIsNone(report["app_group"])
        self.assertFalse(report["distribution_ready"])
        entitlements = self.read_plist("Companion.entitlements")
        self.assertEqual(entitlements, {"com.apple.security.temporary-exception.files.absolute-path.read-write": [str(development) + "/"]})
        for path in (self.output / "RDrive.app").rglob("Info.plist"):
            bundle = plistlib.loads(path.read_bytes())
            self.assertEqual(bundle["RDriveDevelopmentIPCRoot"], str(development))
            self.assertNotIn("RDriveAppGroup", bundle)
            self.assertNotIn("NSExtensionFileProviderDocumentGroup", bundle.get("NSExtension", {}))
        for call in peer.calls:
            if call[:2] == ["xcrun", "swiftc"]:
                self.assertIn("RDRIVE_DEVELOPMENT", call)

    def test_system_and_relative_development_roots_fail_before_external_commands(self):
        for development in ["relative", "/", "/private/tmp", str(self.root / "fixture-home")]:
            with self.subTest(root=development), self.peer() as peer, \
                    mock.patch.object(Path, "home", return_value=self.root / "fixture-home"), \
                    self.assertRaisesRegex(SystemExit, "dedicated absolute"):
                invoke(self.module, self.arguments + ["--development-ipc-root", development])
            self.assertEqual(peer.calls, []); self.assertEqual(peer.queries, [])
        self.assertFalse((self.output / "RDrive.app").exists())

    def test_public_and_foreign_development_directories_are_rejected_without_chmod(self):
        development = self.root / "private-ipc"; development.mkdir(mode=0o755)
        with self.peer() as peer, self.assertRaisesRegex(SystemExit, "0700"):
            invoke(self.module, self.arguments + ["--development-ipc-root", development])
        self.assertEqual(stat.S_IMODE(development.stat().st_mode), 0o755)
        self.assertEqual(peer.queries, [])
        development.chmod(0o700)
        original_stat = Path.stat
        metadata = development.stat()
        def foreign(path, *args, **kwargs):
            if path == development:
                return SimpleNamespace(st_uid=os.getuid() + 1, st_mode=metadata.st_mode)
            return original_stat(path, *args, **kwargs)
        with self.peer() as peer, mock.patch.object(Path, "stat", new=foreign), self.assertRaisesRegex(SystemExit, "owned by this user"):
            invoke(self.module, self.arguments + ["--development-ipc-root", development])
        self.assertEqual(stat.S_IMODE(development.stat().st_mode), 0o700)
        self.assertEqual(peer.queries, [])

    def test_sdk_failure_preserves_previous_bundle_and_propagates(self):
        previous = self.previous_app()
        with self.peer(fail_query=lambda args: args[0] == "xcrun"), self.assertRaises(subprocess.CalledProcessError):
            invoke(self.module, self.arguments)
        self.assert_previous_preserved(previous)
        self.assertEqual(list(self.output.glob(".building-*")), [])

    def test_compiler_signer_and_verifier_failures_preserve_previous_app(self):
        for phase in ["compiler", "signer", "verify"]:
            with self.subTest(phase=phase):
                previous = self.previous_app()
                failure = lambda args: ((phase == "compiler" and args[:2] == ["xcrun", "swiftc"] and "-typecheck" not in args) or (phase == "signer" and "--sign" in args) or (phase == "verify" and "--verify" in args))
                before = set(self.output.glob(".building-*"))
                with self.peer(fail_run=failure), self.assertRaises(subprocess.CalledProcessError):
                    invoke(self.module, self.arguments)
                self.assert_previous_preserved(previous)
                created = set(self.output.glob(".building-*")) - before
                self.assertEqual(len(created), 1)
                self.assertTrue((created.pop() / "RDrive.app/Contents/Info.plist").is_file())

    def test_missing_cli_is_rejected_without_replacing_existing_bundle(self):
        previous = self.previous_app()
        self.cli.unlink()
        with self.peer(), self.assertRaisesRegex(SystemExit, "Build the Rust CLI"):
            invoke(self.module, self.arguments)
        self.assert_previous_preserved(previous)
        self.assertEqual(len(list(self.output.glob(".building-*"))), 1)

    def test_cli_copy_failure_preserves_source_and_previous_bundle(self):
        previous = self.previous_app()
        with self.peer(), mock.patch.object(self.module.shutil, "copy2", side_effect=PermissionError("injected copy failure")), self.assertRaisesRegex(PermissionError, "injected copy failure"):
            invoke(self.module, self.arguments)
        self.assert_previous_preserved(previous)
        self.assertEqual(self.cli.read_bytes(), b"isolated CLI payload")

    def test_previous_bundle_removal_failure_retains_old_and_staged_bundles(self):
        previous = self.previous_app()
        with self.peer(), mock.patch.object(self.module.shutil, "rmtree", side_effect=PermissionError("injected removal failure")), self.assertRaisesRegex(PermissionError, "injected removal failure"):
            invoke(self.module, self.arguments)
        self.assert_previous_preserved(previous)
        self.assertEqual(len(list(self.output.glob(".building-*/RDrive.app"))), 1)

    def test_staging_rmdir_failure_occurs_after_publication_without_false_report(self):
        previous = self.previous_app()
        original_rmdir = Path.rmdir
        def fail_staging(path, *args, **kwargs):
            if path.name.startswith(".building-"):
                raise PermissionError("injected staging rmdir failure")
            return original_rmdir(path, *args, **kwargs)
        with self.peer(), mock.patch.object(Path, "rmdir", new=fail_staging), self.assertRaisesRegex(PermissionError, "injected staging rmdir failure"):
            invoke(self.module, self.arguments)
        self.assertFalse(previous.exists())
        self.assertEqual((self.output / "RDrive.app/Contents/Resources/rdrive").read_bytes(), self.cli.read_bytes())
        self.assertFalse((self.output / "build-report.json").exists())
        remnants = list(self.output.glob(".building-*"))
        self.assertEqual(len(remnants), 1)
        self.assertEqual(list(remnants[0].iterdir()), [])


if __name__ == "__main__":
    unittest.main()
