"""Ownership and shutdown boundaries for the disposable Samba fixture."""
from pathlib import Path
import subprocess
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


if __name__ == "__main__":
    unittest.main()
