"""Ownership and shutdown boundaries for the disposable Samba fixture."""
from pathlib import Path
import json
import subprocess
import tempfile
import unittest
from unittest import mock

import smb_workflows


class SambaFixtureCleanupTests(unittest.TestCase):
    def test_linux_stops_server_before_repairing_only_its_share_without_following_links(self):
        with mock.patch.object(smb_workflows.sys, "platform", "linux"), \
                mock.patch.object(smb_workflows.os, "getuid", return_value=1001), \
                mock.patch.object(smb_workflows.os, "getgid", return_value=1002), \
                mock.patch.object(smb_workflows.subprocess, "run") as run:
            smb_workflows.cleanup_fixture("/docker", "fixture-unique", "sha256:fixture",
                                          Path("/tmp/fixture-unique/share"))
        self.assertEqual(run.call_args_list, [
            mock.call(["/docker", "rm", "--force", "fixture-unique"],
                      check=True, capture_output=True, text=True, timeout=30),
            mock.call(["/docker", "run", "--rm", "--network", "none", "--user", "0:0",
                       "--volume", "/tmp/fixture-unique/share:/share", "--entrypoint", "chown",
                       "sha256:fixture", "-h", "-R", "1001:1002", "/share"],
                      check=True, capture_output=True, text=True, timeout=30),
        ])

    def test_desktop_cleanup_does_not_change_host_file_ownership(self):
        with mock.patch.object(smb_workflows.sys, "platform", "darwin"), \
                mock.patch.object(smb_workflows.subprocess, "run") as run:
            smb_workflows.cleanup_fixture("/docker", "fixture-unique", "image", Path("/tmp/share"))
        run.assert_called_once_with(["/docker", "rm", "--force", "fixture-unique"],
                                    check=True, capture_output=True, text=True, timeout=30)

    def test_failed_shutdown_prevents_ownership_changes_and_is_not_hidden(self):
        failure = subprocess.CalledProcessError(1, ["/docker", "rm"])
        with mock.patch.object(smb_workflows.sys, "platform", "linux"), \
                mock.patch.object(smb_workflows.subprocess, "run", side_effect=failure) as run:
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                smb_workflows.cleanup_fixture("/docker", "fixture-unique", "image", Path("/tmp/share"))
        self.assertIs(raised.exception, failure)
        self.assertEqual(run.call_count, 1)

    def test_failed_ownership_repair_is_not_hidden(self):
        failure = subprocess.CalledProcessError(1, ["/docker", "run"])
        with mock.patch.object(smb_workflows.sys, "platform", "linux"), \
                mock.patch.object(smb_workflows.subprocess, "run", side_effect=[None, failure]) as run:
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                smb_workflows.cleanup_fixture("/docker", "fixture-unique", "image", Path("/tmp/share"))
        self.assertIs(raised.exception, failure)
        self.assertEqual(run.call_count, 2)


class SambaFixtureMutationTests(unittest.TestCase):
    def test_external_file_creation_uses_only_its_container_and_stdin(self):
        content = b"independent server bytes\0\xff"
        with mock.patch.object(smb_workflows.subprocess, "run") as run:
            smb_workflows.create_fixture_file("/docker", "fixture-unique",
                                             "provider-own-write/ns-renamed.txt", content)
        arguments = run.call_args.args[0]
        self.assertEqual(arguments[:8], ["/docker", "exec", "--interactive", "--user",
                                        "0:0", "fixture-unique", "sh", "-c"])
        self.assertEqual(arguments[9:], ["rdrive-fixture-create", "/share",
                                        "provider-own-write/ns-renamed.txt"])
        self.assertEqual(run.call_args.kwargs, dict(input=content, check=True,
                                                    capture_output=True, timeout=30))
        self.assertNotIn(content, arguments)

    def test_unsafe_paths_are_rejected_before_any_container_operation(self):
        for relative in [None, "", "/absolute", "..", "../escape", "a/../b", ".", "a/./b",
                         "a//b", "a/", "a\\b", "a\0b"]:
            with self.subTest(relative=relative), mock.patch.object(smb_workflows.subprocess, "run") as run:
                with self.assertRaises(ValueError):
                    smb_workflows.create_fixture_file("/docker", "fixture-unique", relative, b"data")
                run.assert_not_called()

    def test_server_file_creation_refuses_existing_files_and_symlink_escapes(self):
        # Execute the actual POSIX shell operation against a temporary share;
        # only the Docker boundary and its fixed /share mount are substituted.
        execute = subprocess.run
        with tempfile.TemporaryDirectory(prefix="rdrive-smb-mutation-") as temporary:
            root = Path(temporary)
            share = root / "share"
            share.mkdir()
            parent = share / "server-created"
            parent.mkdir(mode=0o755)
            outside = root / "outside"
            outside.mkdir()
            (share / "linked-parent").symlink_to(outside, target_is_directory=True)
            protected = outside / "protected"
            protected.write_bytes(b"outside bytes")
            (share / "linked-file").symlink_to(protected)

            def container(arguments, **options):
                self.assertEqual(arguments[10], "/share")
                return execute([*arguments[6:10], str(share), *arguments[11:]], **options)

            with mock.patch.object(smb_workflows.subprocess, "run", side_effect=container):
                smb_workflows.create_fixture_file("/docker", "fixture-unique",
                                                 "server-created/new.bin", b"new\0bytes\xff")
                self.assertEqual((parent / "new.bin").read_bytes(), b"new\0bytes\xff")
                for relative in ["server-created/new.bin", "linked-parent/new.bin", "linked-file"]:
                    with self.subTest(relative=relative), self.assertRaises(subprocess.CalledProcessError):
                        smb_workflows.create_fixture_file("/docker", "fixture-unique", relative, b"overwrite")
            self.assertEqual((parent / "new.bin").read_bytes(), b"new\0bytes\xff")
            self.assertEqual(protected.read_bytes(), b"outside bytes")
            self.assertFalse((outside / "new.bin").exists())

    def test_container_write_failure_is_not_hidden(self):
        failure = subprocess.CalledProcessError(1, ["/docker", "exec"])
        with mock.patch.object(smb_workflows.subprocess, "run", side_effect=failure):
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                smb_workflows.create_fixture_file("/docker", "fixture-unique", "new.bin", b"bytes")
        self.assertIs(raised.exception, failure)


class SambaTimeoutDiagnosticsTests(unittest.TestCase):
    def test_retains_partial_process_output_and_transaction_phases_before_fixture_cleanup(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-smb-timeout-") as temporary:
            root = Path(temporary)
            home = root / "state"
            transactions = home / "transactions"
            transactions.mkdir(parents=True)
            (transactions / "transaction.json").write_text(json.dumps({
                "id": "transaction-id", "phase": "prepared", "base_sequence": 1,
                "commit": {"commit": {"sequence": 2}},
                "snapshots": {"private-payload": "must-not-copy"},
            }))
            share = root / "share"
            slots = share / "managed/.rdrive-repository/commits"
            slots.mkdir(parents=True)
            (slots / "00000000000000000002.json").write_text("{}")
            error = subprocess.TimeoutExpired(["rdrive", "repo", "commit", "transaction-id"],
                                              180, output=b'{"confirmed":true}\n', stderr=b"last phase\xff")
            report = smb_workflows.timeout_diagnostics(error, [home, home], share)
            self.assertEqual(report["stdout"], '{"confirmed":true}\n')
            self.assertEqual(report["stderr"], "last phase\ufffd")
            self.assertEqual(report["timeout"], 180)
            self.assertEqual(report["transactions"], {"state": [{
                "journal": "transaction.json", "id": "transaction-id", "phase": "prepared",
                "base_sequence": 1, "commit_sequence": 2,
            }]})
            self.assertEqual(report["published_slots"], ["00000000000000000002.json"])
            self.assertNotIn("must-not-copy", json.dumps(report))

    def test_bounds_output_and_reports_unreadable_or_invalid_journals(self):
        with tempfile.TemporaryDirectory(prefix="rdrive-smb-timeout-") as temporary:
            root = Path(temporary)
            transactions = root / "state/transactions"
            transactions.mkdir(parents=True)
            outside = root / "outside.json"
            outside.write_text('{"phase":"must-not-follow"}')
            (transactions / "linked.json").symlink_to(outside)
            (transactions / "invalid.json").write_text("not JSON")
            (transactions / "oversized.json").write_bytes(b" " * (1024 * 1024 + 1))
            error = subprocess.TimeoutExpired("fixture-command", 1, output="a" * 70000)
            report = smb_workflows.timeout_diagnostics(error, [root / "state"], root / "share")
            self.assertEqual(report["command"], ["fixture-command"])
            self.assertEqual(len(report["stdout"]), 65536)
            self.assertEqual(report["stderr"], "")
            self.assertEqual(len(report["transactions"]["state"]), 3)
            self.assertTrue(all("diagnostic_error" in value for value in report["transactions"]["state"]))
            self.assertNotIn("must-not-follow", json.dumps(report))


if __name__ == "__main__":
    unittest.main()
