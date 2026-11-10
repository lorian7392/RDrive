import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "publish_release", Path(__file__).resolve().parents[1] / ".github" / "publish_release.py"
)
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)

REPO = "fixture/RDrive"
TAG = "v0.8.2"
COMMIT = "a" * 40


class GitHub:
    """In-memory release API that exposes draft visibility and upload failures."""

    def __init__(self):
        self.release = None
        self.assets = {}
        self.mutations = []
        self.tag_commits = [COMMIT]
        self.annotated = False
        self.corrupt_upload = False

    def __call__(self, *args):
        if args[:3] == ("api", "--paginate", "--slurp"):
            endpoint = args[3]
            if endpoint.endswith("/releases?per_page=100"):
                return json.dumps([[self.release] if self.release else []])
            if endpoint.endswith("/releases/42/assets?per_page=100"):
                return json.dumps([list(self.assets.values())])
        if args[:1] == ("api",) and "/git/ref/tags/" in args[1]:
            if self.annotated:
                return json.dumps({"object": {"type": "tag", "sha": "c" * 40}})
            commit = self.tag_commits.pop(0) if len(self.tag_commits) > 1 else self.tag_commits[0]
            return json.dumps({"object": {"type": "commit", "sha": commit}})
        if args == ("api", f"repos/{REPO}/git/tags/" + "c" * 40):
            return json.dumps({"object": {"type": "commit", "sha": COMMIT}})
        self.mutations.append(args)
        if args[:2] == ("release", "create"):
            assert "--draft" in args and "--verify-tag" in args
            self.release = {"id": 42, "tag_name": args[2], "draft": True,
                            "prerelease": "--prerelease" in args}
            return ""
        if args[:2] == ("release", "upload"):
            assert self.release["draft"] and "--clobber" not in args
            for file in map(Path, args[5:]):
                assert file.name not in self.assets
                self.add_asset(file)
            return ""
        if args == ("api", "--method", "PATCH", f"repos/{REPO}/releases/42", "-F", "draft=false"):
            assert set(self.assets) == set(publisher.ASSETS)
            self.release["draft"] = False
            return json.dumps(self.release)
        raise AssertionError(f"Unexpected GitHub request: {args}")

    def add_asset(self, file):
        digest = "0" * 64 if self.corrupt_upload else publisher.sha256(file)
        self.assets[file.name] = {"name": file.name, "state": "uploaded", "digest": f"sha256:{digest}"}


class ReleasePublicationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.stage = self.root / "assets"
        self.stage.mkdir()
        self.installer = self.root / "install.sh"
        self.installer.write_text("#!/bin/bash\nexit 0\n")
        (self.stage / "install.sh").write_bytes(self.installer.read_bytes())
        binary = self.stage / publisher.BINARY
        binary.write_bytes(b"fixture CLI")
        (self.stage / "SHA256SUMS").write_text(f"{publisher.sha256(binary)}  {binary.name}\n")
        self.github = GitHub()
        patch = mock.patch.object(publisher, "gh", self.github)
        patch.start()
        self.addCleanup(patch.stop)

    def publish(self, tag=TAG):
        with contextlib.redirect_stdout(io.StringIO()):
            publisher.publish(REPO, tag, COMMIT, self.stage, self.installer)

    def test_fresh_release_is_published_only_after_verified_upload_and_can_be_repeated(self):
        self.publish()
        self.assertFalse(self.github.release["draft"])
        self.assertEqual([call[1] for call in self.github.mutations], ["create", "upload", "--method"])
        self.assertEqual(set(self.github.assets), set(publisher.ASSETS))
        self.github.mutations.clear()
        self.publish()
        self.assertEqual(self.github.mutations, [])

    def test_partial_draft_uploads_only_the_missing_assets(self):
        self.github.release = {"id": 42, "tag_name": TAG, "draft": True, "prerelease": False}
        self.github.add_asset(self.stage / publisher.BINARY)
        self.publish()
        upload = self.github.mutations[0]
        self.assertEqual(upload[:2], ("release", "upload"))
        self.assertEqual({Path(file).name for file in upload[5:]}, {"SHA256SUMS", "install.sh"})
        self.assertFalse(self.github.release["draft"])

    def test_mismatching_upload_keeps_draft_private(self):
        self.github.corrupt_upload = True
        with self.assertRaisesRegex(ValueError, "differs from staged bytes"):
            self.publish()
        self.assertTrue(self.github.release["draft"])
        self.assertEqual([call[1] for call in self.github.mutations], ["create", "upload"])

    def test_published_assets_are_never_replaced_or_filled_in(self):
        self.github.release = {"id": 42, "tag_name": TAG, "draft": False, "prerelease": False}
        with self.assertRaisesRegex(ValueError, "Published release is missing required assets"):
            self.publish()
        self.github.corrupt_upload = True
        self.github.add_asset(self.stage / publisher.BINARY)
        with self.assertRaisesRegex(ValueError, "differs from staged bytes"):
            self.publish()
        self.assertEqual(self.github.mutations, [])

    def test_moved_tag_stops_publication_even_after_upload(self):
        self.github.tag_commits = [COMMIT, "b" * 40]
        with self.assertRaisesRegex(ValueError, "no longer points to the verified build commit"):
            self.publish()
        self.assertTrue(self.github.release["draft"])
        self.assertEqual([call[1] for call in self.github.mutations], ["create", "upload"])

    def test_annotated_tag_is_resolved_to_the_verified_commit(self):
        self.github.annotated = True
        self.publish()
        self.assertFalse(self.github.release["draft"])

    def test_prerelease_flag_comes_from_version_suffix(self):
        self.publish("v0.8.2-rc.1+build.7")
        self.assertTrue(self.github.release["prerelease"])
        self.assertFalse(self.github.release["draft"])

    def test_bad_checksum_or_installer_is_rejected_before_remote_mutation(self):
        checksums = self.stage / "SHA256SUMS"
        original = checksums.read_bytes()
        checksums.write_bytes(original + original)
        with self.assertRaisesRegex(ValueError, "canonical executable checksum"):
            self.publish()
        checksums.write_bytes(original)
        (self.stage / "install.sh").write_text("wrong installer")
        with self.assertRaisesRegex(ValueError, "differs from scripts/install.sh"):
            self.publish()
        self.assertEqual(self.github.mutations, [])


if __name__ == "__main__":
    unittest.main()
