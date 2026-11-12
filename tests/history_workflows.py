#!/usr/bin/env python3
"""Account-bound history and native bridge workflows on disposable loopback DAV.

No real storage, File Provider domain, mount, credential or OS snapshot is used.
Checkpoint status is read-only; creation, mounting and authorization are never
requested. APFS mutation boundaries are covered by the Rust Host mocks.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav

ROOT = Path(__file__).resolve().parents[1]


def run(binary, report_path=None):
    checks = []
    with tempfile.TemporaryDirectory(prefix="rdrive-history-") as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        remote.mkdir()
        (remote / "doc.txt").write_bytes(b"original remote bytes")
        (remote / "rename-source.txt").write_bytes(b"renamed prior version")
        (remote / "copy-source.txt").write_bytes(b"copy source bytes")
        (remote / "copy-target.txt").write_bytes(b"copy target prior")
        (remote / "move-source.txt").write_bytes(b"move source bytes")
        (remote / "move-target.txt").write_bytes(b"move target prior")
        (remote / "tree" / "empty").mkdir(parents=True)
        (remote / "tree" / "child.txt").write_bytes(b"nested remote bytes")
        (remote / "other-account").mkdir()
        (remote / "synced").mkdir()

        class Fixture(Dav):
            online = True
            attempts = []
            requests_seen = []
            locks = {}
            def handle_dav(self):
                type(self).attempts.append((self.command, self.path))
                if not type(self).online:
                    self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    return self.reply(503)
                return super().handle_dav()
            def reply(self, status, body=b"", **headers):
                if status == 207:
                    body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
                try:
                    return super().reply(status, body, **headers)
                except (BrokenPipeError, ConnectionResetError):
                    self.close_connection = True
        Fixture.root = remote
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        home = root / "state"
        ipc = root / "ipc"
        ipc.mkdir(mode=0o700)
        staging = ipc / "Transfers"
        environment = dict(os.environ, RDRIVE_HISTORY_TEST_PASSWORD="fixture-password")
        environment.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        base = [str(binary), "--home", str(home), "--json", "--progress", "off"]
        process = None
        log = (root / "bridge.log").open("w+")
        def command(*args, success=True, timeout=60):
            result = subprocess.run(base + list(map(str, args)), env=environment, capture_output=True, text=True, timeout=timeout)
            assert (result.returncode == 0) == success, (args, result.returncode, result.stdout, result.stderr)
            if not success:
                return result
            try:
                return json.loads(result.stdout)
            except json.JSONDecodeError as error:
                raise AssertionError((args, "expected one JSON response", result.stdout, result.stderr)) from error
        def wait(predicate, label, seconds=30):
            deadline = time.monotonic() + seconds
            while True:
                value = predicate()
                if value:
                    return value
                assert time.monotonic() < deadline, label
                time.sleep(.1)
        def rpc(**request):
            endpoint = json.loads((ipc / "bridge-endpoint.json").read_text())
            request.update(v=1, auth_token=endpoint["token"], request_id=uuid.uuid4().hex)
            request.setdefault("domain", "history-fixture")
            with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=45) as client:
                client.sendall(json.dumps(request).encode() + b"\n")
                with client.makefile("rb") as stream:
                    while True:
                        line = stream.readline()
                        assert line, "History bridge disconnected"
                        response = json.loads(line)
                        if "progress" not in response:
                            return response
        def ok(**request):
            response = rpc(**request)
            assert response.get("ok"), response
            return response
        def start():
            nonlocal process
            process = subprocess.Popen(base + ["provider", "serve", "--socket", str(ipc / "bridge.sock"), "--staging", str(staging), "--development-tcp"], env=environment, stdout=subprocess.DEVNULL, stderr=log)
            def ready():
                assert process.poll() is None, "history broker exited"
                try:
                    return rpc(op="domains").get("ok")
                except (OSError, json.JSONDecodeError):
                    return False
            wait(ready, "history broker startup", 15)
        def stop():
            nonlocal process
            if process and process.poll() is None:
                process.send_signal(signal.SIGINT)
                process.wait(timeout=20)
            process = None
        def passed(label):
            checks.append(label)
            print("PASS " + label, flush=True)
        def entries(path=None, **options):
            args = ["history", "files", "fixture"] + ([path] if path is not None else [])
            for key, value in options.items():
                args.extend(["--" + key.replace("_", "-"), str(value)])
            value = command(*args)
            return value["entries"] if isinstance(value, dict) else value
        def identity(value):
            return value["entry"]["id"] if "entry" in value else value["id"]
        def snapshot():
            return {file.relative_to(home).as_posix(): file.read_bytes()
                    for file in (home / "smart").rglob("*")
                    if file.is_file() and (file.name == "queue.json" or "dirty" in file.parts)}
        def metadata_path(path, entry):
            key = lambda text: hashlib.sha256(text.encode()).hexdigest()
            revision = json.dumps([entry["etag"], entry["size"]], separators=(",", ":"))
            return remote / ".rdrive-metadata" / key(path) / (key(revision) + ".json")
        def attach_metadata(path, attributes):
            entry = command("stat", "fixture", path)
            record = {"version": 1, "content_sha256": hashlib.sha256((remote / path).read_bytes()).hexdigest(),
                      "attributes": attributes, "selection": ["com.apple.FinderInfo", "com.apple.metadata:*", "user.*"],
                      "excluded": []}
            sidecar = metadata_path(path, entry)
            sidecar.parent.mkdir(parents=True, exist_ok=True)
            sidecar.write_text(json.dumps({"version": 1, "path": path, "entry": entry, "record": record}))
            return entry, sidecar
        try:
            for name, suffix in [("fixture", ""), ("other", "other-account/")]:
                command("profile", "add", name, "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/{suffix}", "--allow-http", "--username", "user", "--password-env", "RDRIVE_HISTORY_TEST_PASSWORD")
            command("history", "config", "--enabled", "false")
            command("history", "config", "--profile", "fixture", "--unlimited")
            command("history", "config", "--profile", "fixture", "--enabled", "true", "--keep", "20", "--max-age-days", "30", "--max-bytes", "5368709120")
            status = command("history", "status", "fixture")
            assert status["policy"] == {"enabled": True, "keep": 20, "max_age_days": 30, "max_bytes": 5368709120}, status
            assert status["separate_from_cache"] and not entries()
            assert command("history", "--limit", "1") is not None
            config_before = (home / "config.json").read_bytes()
            for args in [("history", "config", "--profile", "fixture", "--keep", "0"),
                         ("history", "config", "--profile", "fixture", "--max-bytes", "0"),
                         ("history", "files", "fixture", "--page-size", "0"),
                         ("checkpoint", "config", "--max-records", "0")]:
                command(*args, success=False)
            assert (home / "config.json").read_bytes() == config_before
            passed("shared per-connection history policy and legacy completion-event command coexist")

            local = root / "recovered.txt"
            local.write_bytes(b"verified recovered payload")
            request_id = "history-workflow-import"
            imported = command("history", "import", "fixture", "doc.txt", local, "--request-id", request_id)
            version = identity(imported)
            before = {p.relative_to(remote).as_posix(): p.read_bytes() for p in remote.rglob("*") if p.is_file()}
            assert identity(command("history", "import", "fixture", "doc.txt", local, "--request-id", request_id)) == version
            local.write_bytes(b"different recovered payload")
            command("history", "import", "fixture", "doc.txt", local, "--request-id", request_id, success=False)
            local.write_bytes(b"verified recovered payload")
            assert {p.relative_to(remote).as_posix(): p.read_bytes() for p in remote.rglob("*") if p.is_file()} == before
            assert command("history", "files", "other")["entries"] == []
            assert entries("doc.txt")[0]["id"] == version
            passed("checkpoint payload import is idempotent, account-bound and never writes the remote tree")

            destination = root / "export.txt"
            exported = command("history", "export", "fixture", version, destination)
            assert exported["verified"] and destination.read_bytes() == local.read_bytes()
            command("history", "export", "fixture", version, destination, success=False)
            assert destination.read_bytes() == local.read_bytes()
            corrupt_blob = home / "history" / "blobs" / hashlib.sha256(local.read_bytes()).hexdigest()
            original_blob = corrupt_blob.read_bytes()
            corrupt_blob.write_bytes(b"corrupted")
            invalid_destination = root / "invalid-export.txt"
            command("history", "export", "fixture", version, invalid_destination, success=False)
            assert not invalid_destination.exists()
            corrupt_blob.write_bytes(original_blob)
            passed("export verifies immutable SHA-256 content, refuses corruption and preserves existing destinations")

            copy_plan = command("history", "restore", "fixture", version, "--to", "restored-copy.txt")
            assert copy_plan["mode"] == "copy" and copy_plan["target"] == "restored-copy.txt"
            assert not (remote / "restored-copy.txt").exists()
            restored = command("history", "restore", "fixture", version, "--plan", copy_plan["plan_id"], "--apply")
            assert restored["restored"] and (remote / "restored-copy.txt").read_bytes() == local.read_bytes()
            assert (remote / "doc.txt").read_bytes() == b"original remote bytes"
            repeated = command("history", "restore", "fixture", version, "--plan", copy_plan["plan_id"], "--apply")
            assert repeated["restored"] and repeated["path"] == restored["path"]
            passed("default restore previews a separate copy and replays its durable receipt without duplicate publication")

            stale_plan = command("history", "restore", "fixture", version, "--replace")
            (remote / "doc.txt").write_bytes(b"concurrent writer")
            command("history", "restore", "fixture", version, "--plan", stale_plan["plan_id"], "--apply", success=False)
            assert (remote / "doc.txt").read_bytes() == b"concurrent writer"
            replace_plan = command("history", "restore", "fixture", version, "--replace")
            command("history", "restore", "fixture", version, "--plan", replace_plan["plan_id"], "--apply")
            assert (remote / "doc.txt").read_bytes() == local.read_bytes()
            captured = [entry for entry in entries("doc.txt") if entry["reason"] == "restore"]
            assert captured, entries("doc.txt")
            recovery = root / "prior-current.txt"
            command("history", "export", "fixture", captured[0]["id"], recovery)
            assert recovery.read_bytes() == b"concurrent writer"
            passed("stale replacement plans preserve concurrent writes; confirmed replacement retains the exact prior current version")

            tagged_path = "metadata-history-tagged.txt"
            historical_bytes = b"historical tagged content BBBB"
            current_bytes = b"current tagged content AAAA"
            historical_tags = {"user.history-regression": b"historical tags".hex()}
            current_tags = {"user.history-regression": b"current tags".hex()}
            (remote / tagged_path).write_bytes(historical_bytes)
            attach_metadata(tagged_path, historical_tags)
            metadata_upload = root / "metadata-upload.txt"
            metadata_upload.write_bytes(current_bytes)
            command("put", "fixture", metadata_upload, tagged_path, "--overwrite")
            historical_version = next(entry["id"] for entry in entries(tagged_path) if entry["reason"] == "overwrite")
            current_entry, old_sidecar = attach_metadata(tagged_path, current_tags)
            metadata_plan = command("history", "restore", "fixture", historical_version, "--replace")
            metadata_restore = command("history", "restore", "fixture", historical_version, "--plan", metadata_plan["plan_id"], "--apply")
            restored_entry = command("stat", "fixture", tagged_path)
            restored_sidecar = metadata_path(tagged_path, restored_entry)
            assert metadata_restore["restored"] and metadata_restore["metadata_restored"]
            assert current_entry["etag"] != restored_entry["etag"] and old_sidecar != restored_sidecar
            assert (remote / tagged_path).read_bytes() == historical_bytes and old_sidecar.exists()
            assert json.loads(restored_sidecar.read_text())["record"]["attributes"] == historical_tags
            tagged_before_image = root / "metadata-tagged-before-image.txt"
            command("history", "export", "fixture", metadata_restore["current_version_preserved"], tagged_before_image)
            assert tagged_before_image.read_bytes() == current_bytes
            assert command("metadata", "capture", tagged_before_image)["attributes"] == current_tags
            passed("different-content tagged replacement publishes historical tags on the new revision and independently preserves current bytes and tags")

            untagged_path = "metadata-history-without-sidecar.txt"
            untagged_bytes = b"historical content with no portable metadata"
            (remote / untagged_path).write_bytes(untagged_bytes)
            command("put", "fixture", metadata_upload, untagged_path, "--overwrite")
            untagged_version = next(entry["id"] for entry in entries(untagged_path) if entry["reason"] == "overwrite")
            current_entry, old_sidecar = attach_metadata(untagged_path, current_tags)
            untagged_plan = command("history", "restore", "fixture", untagged_version, "--replace")
            untagged_restore = command("history", "restore", "fixture", untagged_version, "--plan", untagged_plan["plan_id"], "--apply")
            restored_entry = command("stat", "fixture", untagged_path)
            assert untagged_restore["restored"] and not untagged_restore["metadata_restored"]
            assert current_entry["etag"] != restored_entry["etag"]
            assert (remote / untagged_path).read_bytes() == untagged_bytes
            assert old_sidecar.exists() and not metadata_path(untagged_path, restored_entry).exists()
            untagged_before_image = root / "metadata-untagged-before-image.txt"
            command("history", "export", "fixture", untagged_restore["current_version_preserved"], untagged_before_image)
            assert untagged_before_image.read_bytes() == current_bytes
            assert command("metadata", "capture", untagged_before_image)["attributes"] == current_tags
            passed("historical content without portable metadata restores successfully while the previous revision's sidecar stays retained and does not attach to new contents")

            tree_source = root / "recovered-tree"
            (tree_source / "empty").mkdir(parents=True)
            (tree_source / "nested").mkdir()
            (tree_source / "nested" / "child.txt").write_bytes(b"complete historical tree")
            tree_entry = command("history", "import", "fixture", "tree", tree_source, "--request-id", "history-tree-import")
            tree_version = identity(tree_entry)
            tree_export = root / "exported-tree"
            command("history", "export", "fixture", tree_version, tree_export)
            assert (tree_export / "empty").is_dir() and (tree_export / "nested" / "child.txt").read_bytes() == b"complete historical tree"
            command("history", "restore", "fixture", tree_version, "--replace", success=False)
            tree_plan = command("history", "restore", "fixture", tree_version, "--to", "tree-restored")
            command("history", "restore", "fixture", tree_version, "--plan", tree_plan["plan_id"], "--apply")
            assert (remote / "tree-restored" / "empty").is_dir() and (remote / "tree-restored" / "nested" / "child.txt").read_bytes() == b"complete historical tree"
            passed("directory history retains empty directories and every nested payload; restore creates a full copy and refuses replacement")

            upload = root / "upload.txt"
            upload.write_bytes(b"ordinary overwrite")
            command("put", "fixture", upload, "doc.txt", "--overwrite")
            assert any(entry["reason"] == "overwrite" for entry in entries("doc.txt"))
            command("rm", "fixture", "doc.txt")
            deletion = next(entry for entry in entries("doc.txt") if entry["reason"] == "delete")
            assert not (remote / "doc.txt").exists()
            deleted_export = root / "deleted-copy.txt"
            command("history", "export", "fixture", deletion["id"], deleted_export)
            assert deleted_export.read_bytes() == b"ordinary overwrite"
            passed("ordinary CLI overwrite and deletion capture recoverable before-images before changing the server")

            for verb, source, target, prior in [
                ("cp", "copy-source.txt", "copy-target.txt", b"copy target prior"),
                ("mv", "move-source.txt", "move-target.txt", b"move target prior"),
            ]:
                source_bytes = (remote / source).read_bytes()
                command(verb, "fixture", source, target, "--overwrite")
                assert (remote / target).read_bytes() == source_bytes
                captured_target = next(entry for entry in entries(target) if entry["reason"] == "overwrite")
                target_export = root / (verb + "-overwritten-prior.txt")
                command("history", "export", "fixture", captured_target["id"], target_export)
                assert target_export.read_bytes() == prior
                assert (remote / source).exists() == (verb == "cp")
            passed("copy/move overwrite retains the destination before-image and applies guarded source removal only for move")

            for verb in ("cp", "mv"):
                source, target = verb + "-absent-source.txt", verb + "-observed-absent.txt"
                source_bytes = (verb + " source for an absent destination").encode()
                (remote / source).write_bytes(source_bytes)
                assert not (remote / target).exists()
                command(verb, "fixture", source, target, "--overwrite")
                assert (remote / target).read_bytes() == source_bytes
                assert (remote / source).exists() == (verb == "cp")
            passed("history-enabled copy and move with explicit overwrite publish to initially absent destinations without replacing an unarchived target")

            command("rm", "fixture", "tree", "--recursive")
            recycled_tree = next(entry for entry in entries("tree") if entry["reason"] == "delete")
            assert recycled_tree["kind"] == "directory" and not (remote / "tree").exists()
            deleted_tree = root / "deleted-tree"
            command("history", "export", "fixture", recycled_tree["id"], deleted_tree)
            assert (deleted_tree / "empty").is_dir() and (deleted_tree / "child.txt").read_bytes() == b"nested remote bytes"
            passed("recursive directory deletion retains a verified full before-image including empty descendants")

            command("put", "fixture", upload, "rename-source.txt", "--overwrite")
            renamed_before = entries("rename-source.txt")
            command("mv", "fixture", "rename-source.txt", "renamed.txt")
            renamed_after = entries("renamed.txt")
            assert {entry["id"] for entry in renamed_before}.issubset({entry["id"] for entry in renamed_after})
            assert renamed_before[0]["item_id"] in {entry["item_id"] for entry in renamed_after}
            passed("recorded rename aliases keep the stable item's earlier versions discoverable at its new path")

            sync_local = root / "sync-local"
            sync_local.mkdir()
            sync_file = sync_local / "tracked.txt"
            sync_file.write_bytes(b"sync initial remote revision")
            command("sync", "fixture", sync_local, "synced", "--direction", "push", "--apply")
            sync_file.write_bytes(b"sync replacement revision")
            command("sync", "fixture", sync_local, "synced", "--direction", "push", "--apply")
            sync_versions = entries("synced/tracked.txt")
            sync_prior = next(entry for entry in sync_versions if entry["reason"] == "overwrite")
            sync_export = root / "sync-before-overwrite.txt"
            command("history", "export", "fixture", sync_prior["id"], sync_export)
            assert sync_export.read_bytes() == b"sync initial remote revision"
            sync_file.unlink()
            command("sync", "fixture", sync_local, "synced", "--direction", "push", "--delete", "--apply")
            assert not (remote / "synced" / "tracked.txt").exists()
            sync_deleted = next(entry for entry in entries("synced/tracked.txt") if entry["reason"] == "delete")
            sync_deleted_export = root / "sync-before-delete.txt"
            command("history", "export", "fixture", sync_deleted["id"], sync_deleted_export)
            assert sync_deleted_export.read_bytes() == b"sync replacement revision"
            dry_run = command("sync", "fixture", sync_local, "synced", "--direction", "push", "--delete")
            assert not dry_run["actions"] and not dry_run["conflicts"], dry_run
            passed("ordinary sync guards and retains remote overwrite/deletion before-images while internal recovery names stay out of the baseline")

            # Seed a retained offline edit, then exercise authenticated Finder
            # history operations while the worker's Store writer is resident.
            command("index", "config", "--disable")
            command("smart", "setup", "history-fixture", "fixture")
            start()
            items = wait(lambda: ok(op="list").get("items"), "initial Smart listing")
            selected = next(item for item in items if item["name"] == "restored-copy.txt")
            command("smart", "pause")
            Fixture.online = False
            staged = staging / "retained-offline-edit"
            staged.write_bytes(b"must retain this offline edit")
            retained = rpc(op="modify", id=selected["id"], content_path=str(staged), base_content_version=selected["content_version"], base_metadata_version=selected["metadata_version"])
            assert not retained["ok"], retained
            staged.unlink()
            retained_before = snapshot()
            assert any("dirty/" in name for name in retained_before), retained_before
            request_count = len(Fixture.attempts)
            listed = ok(op="history_list", id="root", offset=0, page_size=100)
            assert listed["entries"] and listed["policy"]["enabled"]
            viewed = ok(op="history_list", id=selected["id"], offset=0, page_size=100)
            assert viewed["item_id"] == selected["id"]
            command("history", "status", "fixture")
            command("history", "files", "fixture", "--offset", "0", "--page-size", "1")
            cli_export = root / "resident-broker-export.txt"
            command("history", "export", "fixture", version, cli_export)
            assert cli_export.read_bytes() == local.read_bytes()
            rejected = rpc(op="history_cli")
            assert not rejected["ok"] and "local Unix bridge" in json.dumps(rejected)
            saved = ok(op="history_config", history_policy={"enabled": True, "keep": 19, "max_age_days": 29, "max_bytes": None})
            assert saved["saved"] and saved["policy"]["max_bytes"] is None
            command("history", "config", "--profile", "fixture", "--keep", "20")
            assert snapshot() == retained_before, "History reads or shared settings changed retained edits"
            assert len(Fixture.attempts) == request_count, "Offline history reads/settings contacted the server"
            passed("Finder history and CLI shared settings work offline beside a resident broker without changing queued edits or snapshots")

            # Export is bound to the selected folder/item. The root contains all
            # account history, including the deleted doc.txt entry.
            exported = ok(op="history_export", id="root", version_id=tree_version)
            payload = Path(exported["content_path"])
            assert exported["remote_changed"] is False and exported["verified"]
            assert payload.is_relative_to(staging) and payload.is_dir()
            members = exported["manifest"]["entries"]
            assert all(member["path"] and not member["path"].startswith("/") for member in members)
            assert {member["path"] for member in members} == {"empty", "nested", "nested/child.txt"}
            assert (payload / "nested" / "child.txt").read_bytes() == b"complete historical tree"
            assert snapshot() == retained_before
            bad_selection = rpc(op="history_export", id=selected["id"], version_id=tree_version)
            assert not bad_selection["ok"], bad_selection
            passed("authenticated Finder export verifies a complete subtree and rejects a history entry outside the selected item's scope")

            Fixture.online = True
            plan = ok(op="history_restore_prepare", id="root", version_id=version, mode="copy")
            assert plan["can_restore"] and plan["mode"] == "copy"
            receipt = ok(op="history_restore", id="root", version_id=version, plan_id=plan["plan_id"])
            assert receipt["restored"] and receipt["remote_changed"]
            assert (remote / receipt["path"]).read_bytes() == local.read_bytes()
            assert snapshot() == retained_before
            stop()
            start()
            replayed = ok(op="history_restore", id="root", version_id=version, plan_id=plan["plan_id"])
            assert replayed["path"] == receipt["path"] and snapshot() == retained_before
            passed("Finder guarded restore survives broker restart and preserves every unrelated pending edit and immutable snapshot")
            stop()

            command("history", "config", "--profile", "fixture", "--keep", "1", "--max-age-days", "1", "--max-bytes", "1")
            before_prune = entries()
            preview = command("history", "prune", "fixture")
            assert preview["removed"] == 0 and entries() == before_prune
            command("history", "prune", "fixture", "--apply")
            command("history", "clear", "fixture")
            assert snapshot() == retained_before
            passed("retention pruning previews without deletion and never removes retained Smart queue payloads")

            # A separate state home keeps the earlier offline edit untouched
            # while a real broker encounters and then recovers a history quota.
            quota_remote = remote / "quota"
            quota_remote.mkdir()
            quota_before = b"before-image larger than the one-byte history limit"
            (quota_remote / "doc.txt").write_bytes(quota_before)
            quota_home, quota_ipc = root / "quota-state", root / "quota-ipc"
            quota_ipc.mkdir(mode=0o700)
            quota_stage = quota_ipc / "Transfers"
            quota_base = [str(binary), "--home", str(quota_home), "--json", "--progress", "off"]
            quota_process = None
            def quota_command(*args):
                result = subprocess.run(quota_base + list(map(str, args)), env=environment, capture_output=True, text=True, timeout=40)
                assert result.returncode == 0, (args, result.stdout, result.stderr)
                return json.loads(result.stdout)
            def quota_rpc(**request):
                endpoint = json.loads((quota_ipc / "bridge-endpoint.json").read_text())
                request.update(v=1, domain="quota-fixture", auth_token=endpoint["token"], request_id=uuid.uuid4().hex)
                with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=20) as client:
                    client.sendall(json.dumps(request).encode() + b"\n")
                    with client.makefile("rb") as stream:
                        while True:
                            line = stream.readline()
                            assert line, "quota bridge disconnected"
                            reply = json.loads(line)
                            if "progress" not in reply:
                                return reply
            def quota_mutation():
                path = quota_home / "smart" / hashlib.sha256(b"quota-fixture").hexdigest() / "queue.json"
                records = json.loads(path.read_text())["mutations"]
                selected_edits = [row for row in records.values() if row["request"]["op"] == "modify" and row["request"]["id"] == selected_quota["id"]]
                assert len(selected_edits) == 1, records
                return path, selected_edits[0]
            def quota_start():
                nonlocal quota_process
                quota_process = subprocess.Popen(quota_base + ["provider", "serve", "--socket", str(quota_ipc / "bridge.sock"), "--staging", str(quota_stage), "--development-tcp"], env=environment, stdout=subprocess.DEVNULL, stderr=log)
                def ready():
                    assert quota_process.poll() is None, "quota broker exited"
                    try:
                        return quota_rpc(op="domains").get("ok")
                    except (OSError, json.JSONDecodeError):
                        return False
                wait(ready, "quota broker startup", 15)
            def quota_stop():
                nonlocal quota_process
                if quota_process and quota_process.poll() is None:
                    quota_process.send_signal(signal.SIGINT)
                    quota_process.wait(timeout=20)
                quota_process = None
            try:
                quota_command("profile", "add", "quota", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/quota/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_HISTORY_TEST_PASSWORD")
                quota_command("history", "config", "--profile", "quota", "--enabled", "true", "--max-bytes", "1")
                assert json.loads((quota_home / "config.json").read_bytes())["history"]["profiles"]["quota"]["max_bytes"] == 1
                quota_command("index", "config", "--disable")
                quota_command("smart", "setup", "quota-fixture", "quota")
                quota_start()
                selected_quota = next(item for item in wait(lambda: quota_rpc(op="list", parent_id="root").get("items"), "quota catalog") if item["name"] == "doc.txt")
                fetched = quota_rpc(op="fetch", id=selected_quota["id"], base_content_version=selected_quota["content_version"], base_metadata_version=selected_quota["metadata_version"])
                assert fetched["ok"], fetched
                selected_quota = fetched["item"]
                quota_source = quota_stage / "retained-quota-edit"
                quota_source.write_bytes(b"the exact guarded local payload must survive quota recovery")
                quota_source.chmod(0o600)
                requests_before_quota = len(Fixture.attempts)
                rejected = quota_rpc(op="modify", id=selected_quota["id"], content_path=str(quota_source), base_content_version=selected_quota["content_version"], base_metadata_version=selected_quota["metadata_version"])
                assert not rejected["ok"] and rejected["error"]["code"] == "insufficient_quota", rejected
                assert rejected["error"]["message"].startswith("insufficient_quota: history capacity"), rejected
                quota_queue, failed = quota_mutation()
                assert failed["state"] == "failed" and failed["attempts"] == 1, failed
                quota_snapshot = quota_queue.parent / "dirty" / failed["snapshot"]["file"]
                retained_quota = quota_snapshot.read_bytes()
                assert retained_quota == quota_source.read_bytes() and (quota_remote / "doc.txt").read_bytes() == quota_before
                time.sleep(2.2)
                assert quota_mutation()[1] == failed, "quota failure was retried automatically"
                assert not [(method, path) for method, path in Fixture.attempts[requests_before_quota:] if path.startswith("/dav/quota/") and method in ("PUT", "DELETE", "MOVE", "COPY", "MKCOL")]
                # A stopped quota failure must not monopolize the domain.
                # Creating an absent item requires no history before-image.
                independent_source = quota_stage / "independent-create"
                independent_source.write_bytes(b"unrelated create remains available")
                independent_source.chmod(0o600)
                independent = quota_rpc(op="create", parent_id="root", name="independent.txt", kind="file", client_item_id=uuid.uuid4().hex, content_path=str(independent_source))
                assert independent["ok"] and (quota_remote / "independent.txt").read_bytes() == independent_source.read_bytes(), independent
                assert quota_mutation()[1] == failed and quota_snapshot.read_bytes() == retained_quota
                # Shared configuration remains programmatic with the broker
                # resident. A larger budget alone must not restart failed work.
                quota_command("history", "config", "--profile", "quota", "--max-bytes", "1024")
                quota_policy = json.loads((quota_home / "config.json").read_bytes())["history"]["profiles"]["quota"]
                assert quota_policy["enabled"] and quota_policy["max_bytes"] == 1024
                time.sleep(1.1)
                assert quota_mutation()[1] == failed and quota_snapshot.read_bytes() == retained_quota
                quota_stop()
                retry = quota_command("smart", "retry", "quota-fixture", failed["id"])
                pending = retry["retried"][0]
                assert pending["state"] == "pending" and pending["request"] == failed["request"] and pending["fingerprint"] == failed["fingerprint"]
                assert pending["snapshot"] == failed["snapshot"] and quota_snapshot.read_bytes() == retained_quota
                quota_start()
                wait(lambda: quota_mutation()[1]["state"] == "done", "explicit quota retry publication")
                assert quota_mutation()[1]["request"] == failed["request"] and (quota_remote / "doc.txt").read_bytes() == retained_quota
                retained_history = quota_command("history", "files", "quota", "doc.txt")["entries"]
                prior = next(entry for entry in retained_history if entry["reason"] == "overwrite")
                prior_copy = root / "quota-prior-remote.txt"
                quota_command("history", "export", "quota", prior["id"], prior_copy)
                assert prior_copy.read_bytes() == quota_before
                assert snapshot() == retained_before
                passed("history quota stops automatic replay without blocking unrelated creates and preserves the original guarded edit until programmatic capacity change and explicit retry")
            finally:
                quota_stop()

            checkpoint = command("checkpoint", "status")
            assert "available" in checkpoint and checkpoint["system_managed"]
            listing = command("checkpoint", "list")
            assert listing["checkpoints"] == [] and listing["system_managed"]
            command("checkpoint", "config", "--enabled", "false", "--max-records", "32")
            command("checkpoint", "create", success=False)
            missing = str(uuid.uuid4())
            command("checkpoint", "inspect", missing, success=False)
            command("checkpoint", "export", missing, "cache/missing", root / "never-exported", success=False)
            command("checkpoint", "reconcile", missing, success=False)
            command("checkpoint", "forget", missing, success=False)
            forgotten = command("checkpoint", "forget", missing, "--apply")
            assert forgotten["forgotten"] and not forgotten["os_snapshot_deleted"]
            assert not (root / "never-exported").exists()
            assert snapshot() == retained_before
            assert not any("mount" in method.lower() for method, _ in Fixture.attempts)
            assert b"fixture-password" not in (home / "config.json").read_bytes()
            passed("read-only checkpoint capability/status and disabled creation leave OS snapshots, remote data and retained edits untouched")
        except Exception:
            diagnostic = ROOT / "target/history-workflow-failure.json"
            diagnostic.parent.mkdir(parents=True, exist_ok=True)
            log.flush()
            log.seek(0)
            diagnostic.write_text(json.dumps({"requests": Fixture.attempts[-100:], "remote_files": [p.relative_to(remote).as_posix() for p in remote.rglob("*")], "broker_log": log.read()[-8000:]}, indent=2) + "\n")
            raise
        finally:
            stop()
            server.shutdown()
            server.server_close()
            log.close()
    report = {"binary": binary.name, "version": subprocess.check_output([str(binary), "--version"], text=True).strip(), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "passed": len(checks), "checks": checks, "scope": "disposable authenticated loopback DAV, native broker IPC, read-only APFS status; no real storage, domains or OS snapshot mutation"}
    output = Path(report_path or os.environ.get("RDRIVE_HISTORY_REPORT", ROOT / "target/history-workflows.json")).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report", type=Path)
    options = parser.parse_args()
    run(options.binary.resolve(), options.report)
