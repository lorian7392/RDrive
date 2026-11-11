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
UPLOAD = f"https://uploads.github.com/repos/{REPO}/releases/42/assets"


class GitHub:
    """In-memory release API that exposes draft visibility and upload failures."""

    def __init__(self):
        self.release = None
        self.assets = {}
        self.mutations = []
        self.tag_commits = [COMMIT]
        self.annotated = False
        self.corrupt_upload = False
        self.hide_draft_listing = False
        self.upload_url = UPLOAD + "{?name,label}"

    def set_release(self, tag=TAG, draft=True, prerelease=False):
        self.release = {"id": 42, "tag_name": tag, "draft": draft, "prerelease": prerelease,
                        "upload_url": self.upload_url, "assets": []}

    def __call__(self, *args):
        if args[:3] == ("api", "--paginate", "--slurp"):
            endpoint = args[3]
            if endpoint.endswith("/releases?per_page=100"):
                visible = self.release and not (self.release["draft"] and self.hide_draft_listing)
                return json.dumps([[self.release] if visible else []])
            if endpoint.endswith("/releases/42/assets?per_page=100"):
                return json.dumps([list(self.assets.values())])
        if args[:1] == ("api",) and "/git/ref/tags/" in args[1]:
            if self.annotated:
                return json.dumps({"object": {"type": "tag", "sha": "c" * 40}})
            commit = self.tag_commits.pop(0) if len(self.tag_commits) > 1 else self.tag_commits[0]
            return json.dumps({"object": {"type": "commit", "sha": commit}})
        if args == ("api", f"repos/{REPO}/git/tags/" + "c" * 40):
            return json.dumps({"object": {"type": "commit", "sha": COMMIT}})
        if args[:4] == ("api", "--method", "POST", f"repos/{REPO}/releases"):
            fields = dict(value.split("=", 1) for value in args[5::2])
            assert fields["draft"] == "true" and fields["generate_release_notes"] == "true"
            self.set_release(tag=fields["tag_name"], prerelease=fields["prerelease"] == "true")
            self.mutations.append(("create", fields["tag_name"]))
            return json.dumps(self.release)
        if args[:3] == ("api", "--method", "POST") and args[3].startswith(UPLOAD + "?name="):
            assert self.release["draft"]
            assert args[4:9] == ("--hostname", "github.com", "--header",
                                 "Content-Type: application/octet-stream", "--input")
            file = Path(args[9])
            assert args[3] == UPLOAD + "?name=" + file.name
            assert file.name not in self.assets
            self.add_asset(file)
            self.mutations.append(("upload", file.name))
            return json.dumps(self.assets[file.name])
        if args == ("api", "--method", "PATCH", f"repos/{REPO}/releases/42", "-F", "draft=false"):
            assert set(self.assets) == set(publisher.ASSETS)
            self.release["draft"] = False
            self.mutations.append(("publish", self.release["tag_name"]))
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
        self.assertEqual(self.github.mutations,
                         [("create", TAG), *(("upload", name) for name in publisher.ASSETS),
                          ("publish", TAG)])
        self.assertEqual(set(self.github.assets), set(publisher.ASSETS))
        self.github.mutations.clear()
        self.publish()
        self.assertEqual(self.github.mutations, [])

    def test_partial_draft_uploads_only_the_missing_assets(self):
        self.github.set_release()
        self.github.add_asset(self.stage / publisher.BINARY)
        self.publish()
        self.assertEqual(self.github.mutations,
                         [("upload", "SHA256SUMS"), ("upload", "install.sh"), ("publish", TAG)])
        self.assertFalse(self.github.release["draft"])

    def test_fresh_draft_is_uploaded_and_published_when_tag_listing_is_stale(self):
        self.github.hide_draft_listing = True
        self.publish()
        self.assertFalse(self.github.release["draft"])
        self.assertEqual(set(self.github.assets), set(publisher.ASSETS))

    def test_upload_url_must_belong_to_the_returned_release(self):
        self.github.upload_url = UPLOAD.replace("/42/", "/43/") + "{?name,label}"
        with self.assertRaisesRegex(ValueError, "unexpected release asset upload URL"):
            self.publish()
        self.assertEqual(self.github.mutations, [("create", TAG)])

    def test_mismatching_upload_keeps_draft_private(self):
        self.github.corrupt_upload = True
        with self.assertRaisesRegex(ValueError, "differs from staged bytes"):
            self.publish()
        self.assertTrue(self.github.release["draft"])
        self.assertEqual(self.github.mutations,
                         [("create", TAG), *(("upload", name) for name in publisher.ASSETS)])

    def test_published_assets_are_never_replaced_or_filled_in(self):
        self.github.set_release(draft=False)
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
        self.assertEqual(self.github.mutations,
                         [("create", TAG), *(("upload", name) for name in publisher.ASSETS)])

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
