#!/usr/bin/env python3
"""Exercise the release installer with a private temporary home and fake gh."""

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "scripts" / "install.sh"
REPO = "github.com/lorian7392/RDrive"
ASSET = "rdrive-macos-arm64"

FAKE_GH = r'''import hashlib
import json
import os
from pathlib import Path
import stat
import sys

args = sys.argv[1:]
mode = os.environ["INSTALL_TEST_MODE"]
record = {"args": args}
if args[:2] == ["release", "download"]:
    download_dir = Path(args[args.index("--dir") + 1])
    record["mode"] = stat.S_IMODE(download_dir.stat().st_mode)
with Path(os.environ["INSTALL_TEST_LOG"]).open("a") as log:
    log.write(json.dumps(record) + "\n")

if args[:2] == ["release", "view"]:
    if mode == "view-error":
        sys.exit(1)
    if mode != "prerelease":
        print(args[2] if args[2] != "--repo" else "v0.8.1")
elif args[:2] == ["release", "download"]:
    if mode == "download-error":
        sys.exit(1)
    asset = "rdrive-macos-arm64"
    version = args[2][1:]
    payload = f'#!/bin/bash\nprintf "rdrive {version}\\n"\n'.encode()
    binary = download_dir / asset
    if mode == "symlink-asset":
        binary.symlink_to(Path(os.environ["INSTALL_TEST_LOG"]))
    else:
        binary.write_bytes(payload)
    checksum = "0" * 64 if mode == "bad" else hashlib.sha256(payload).hexdigest()
    filename = f"./{asset}" if mode == "wrong-name" else asset
    row = f"{checksum}  {filename}\n"
    (download_dir / "SHA256SUMS").write_text(row * (2 if mode == "duplicate" else 1))
else:
    sys.exit(2)
'''


class InstallTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="rdrive-installer-test-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.home = self.root / "home"
        self.downloads = self.root / "downloads"
        self.downloads.mkdir()
        self.mock_bin = self.root / "tools"
        self.mock_bin.mkdir()
        self.log = self.root / "gh.jsonl"
        gh = self.mock_bin / "gh"
        gh.write_text("#!/usr/bin/env python3\n" + FAKE_GH)
        gh.chmod(0o755)
        uname = self.mock_bin / "uname"
        uname.write_text('#!/bin/bash\ncase "$1" in -s) printf "Darwin\\n" ;; -m) printf "arm64\\n" ;; *) exit 1 ;; esac\n')
        uname.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.home), TMPDIR=str(self.downloads),
                        INSTALL_TEST_LOG=str(self.log), GH_HOST="example.invalid",
                        PATH=os.pathsep.join((str(self.mock_bin), str(Path(sys.executable).parent),
                                              "/usr/bin", "/bin", "/usr/sbin", "/sbin")))

    def run_installer(self, *args, mode="valid", success=True, embedded=False):
        env = dict(self.env, INSTALL_TEST_MODE=mode)
        invocation = ["/bin/bash", str(INSTALLER)]
        if embedded:
            invocation = ["/bin/bash", "-c", INSTALLER.read_text(), "rdrive-install"]
        result = subprocess.run([*invocation, *map(str, args)], env=env, text=True,
                                capture_output=True, timeout=20)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(list(self.downloads.iterdir()), [])
        return result

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def install_incumbent(self, version="0.8.0"):
        prefix = self.root / "existing prefix"
        executable = prefix / "bin" / "rdrive"
        executable.parent.mkdir(parents=True)
        executable.write_text(f'#!/bin/bash\nprintf "rdrive {version}\\n"\n')
        executable.chmod(0o755)
        return prefix, executable

    def assert_install(self, executable, version):
        result = subprocess.run([str(executable), "--version"], text=True,
                                capture_output=True, timeout=5, check=True)
        self.assertEqual(result.stdout, f"rdrive {version}\n")
        self.assertEqual(stat.S_IMODE(executable.stat().st_mode), 0o755)
        self.assertEqual(list(executable.parent.iterdir()), [executable])

    def test_default_install_selects_latest_stable_release(self):
        result = self.run_installer()
        executable = self.home / ".local" / "bin" / "rdrive"
        self.assert_install(executable, "0.8.1")
        self.assertIn(str(executable), result.stdout)
        view, download = self.calls()
        self.assertEqual(view["args"][:6], ["release", "view", "--repo", REPO, "--json",
                                             "tagName,isDraft,isPrerelease"])
        self.assertIn(".isDraft", view["args"][-1])
        self.assertIn(".isPrerelease", view["args"][-1])
        self.assertEqual(download["args"][:-1], ["release", "download", "v0.8.1", "--repo", REPO,
                                                  "--pattern", ASSET, "--pattern", "SHA256SUMS", "--dir"])
        self.assertEqual(download["mode"], 0o700)

    def test_explicit_release_atomically_replaces_regular_executable(self):
        prefix, executable = self.install_incumbent()
        with executable.open("rb") as old:
            self.run_installer("--prefix", prefix, "--version", "v0.8.2",
                               "--replace-version", "v0.8.0", embedded=True)
            self.assertIn(b"0.8.0", old.read())
        self.assert_install(executable, "0.8.2")
        self.assertEqual(self.calls()[0]["args"][:5], ["release", "view", "v0.8.2", "--repo", REPO])

    def test_failed_release_and_checksum_preserve_incumbent(self):
        prefix, executable = self.install_incumbent()
        old = (executable.read_bytes(), executable.stat())
        failures = {"bad": "checksum mismatch", "wrong-name": "no exact entry",
                    "duplicate": "duplicate entries", "symlink-asset": "regular CLI asset",
                    "download-error": "cannot download", "view-error": "cannot read the release",
                    "prerelease": "published stable release"}
        for mode, error in failures.items():
            with self.subTest(mode=mode):
                result = self.run_installer("--prefix", prefix, "--version", "v0.8.2",
                                            mode=mode, success=False)
                self.assertIn(error, result.stderr)
                self.assertEqual(executable.read_bytes(), old[0])
                self.assertEqual(executable.stat().st_ino, old[1].st_ino)
                self.assertEqual(list(executable.parent.iterdir()), [executable])

    def test_stale_update_and_active_lock_leave_installed_binary_intact(self):
        prefix, executable = self.install_incumbent("0.8.2")
        result = self.run_installer("--prefix", prefix, "--version", "v0.8.1",
                                    "--replace-version", "v0.8.0", embedded=True, success=False)
        self.assertIn("changed since the update check", result.stderr)
        self.assert_install(executable, "0.8.2")
        lock = executable.parent / ".rdrive-install.lock"
        lock.mkdir()
        result = self.run_installer("--prefix", prefix, "--version", "v0.8.3", success=False)
        self.assertIn("cannot acquire the install lock", result.stderr)
        lock.rmdir()
        self.assert_install(executable, "0.8.2")

    def test_symlink_app_and_source_destinations_are_rejected_before_download(self):
        prefix = self.root / "symlink-prefix"
        executable = prefix / "bin" / "rdrive"
        executable.parent.mkdir(parents=True)
        victim = self.root / "victim"
        victim.write_bytes(b"keep this file")
        executable.symlink_to(victim)
        app_prefix = self.root / "RDrive.ApP" / "Contents" / "Resources"
        for destination, error in ((prefix, "symbolic link"), (app_prefix, "outside app bundles"),
                                   (INSTALLER.parents[1], "outside the RDrive source checkout")):
            with self.subTest(destination=destination):
                result = self.run_installer("--prefix", destination, success=False)
                self.assertIn(error, result.stderr)
        self.assertTrue(executable.is_symlink())
        self.assertEqual(victim.read_bytes(), b"keep this file")
        self.assertFalse(app_prefix.exists())
        self.assertEqual(self.calls(), [])

    def test_help_and_invalid_cli_inputs_do_not_touch_release(self):
        self.assertIn("Usage:", self.run_installer("--help").stdout)
        for args in (("--prefix", "relative"), ("--prefix", "/"), ("--version", "v0.8.0-rc.1"),
                     ("--version",), ("--unknown",)):
            with self.subTest(args=args):
                self.run_installer(*args, success=False)
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.home.exists())


if __name__ == "__main__":
    unittest.main()
