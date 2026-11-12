#!/usr/bin/env python3
"""Synthetic end-to-end Smart catalog, durable edit, reconnect and quota tests.
No real cloud account, mount, or Finder domain is used by this script.
"""
import http.server
import hashlib
import fcntl
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from cli_workflows import Dav

ROOT = Path(__file__).resolve().parents[1]


def run(binary):
    checks = []
    with tempfile.TemporaryDirectory(prefix="rdrive-smart-") as tmp:
        root = Path(tmp).resolve()
        remote = root / "remote"
        remote.mkdir()
        (remote / "folder").mkdir()
        (remote / "folder" / "child.txt").write_bytes(b"child")
        (remote / "doc.txt").write_bytes(b"original")
        (remote / "four.txt").write_bytes(b"four")
        (remote / "slow" / "level-one" / "level-two").mkdir(parents=True)
        (remote / "slow" / "level-one" / "level-two" / "deep.txt").write_bytes(b"deep metadata only")
        (remote / "preserved" / "nested").mkdir(parents=True)
        (remote / "preserved" / "nested" / "keep.txt").write_bytes(b"cached subtree")
        (remote / "pinned-lazy").mkdir()
        (remote / "Backup").mkdir()
        (remote / "Backup" / "remote.part").write_bytes(b"existing excluded temporary contents")
        (remote / "GH666").mkdir()
        (remote / "GH666" / "keep.txt").write_bytes(b"untouched directory contents")
        (remote / "GH666Legacy").mkdir()
        (remote / "GH666Legacy" / "keep.txt").write_bytes(b"retained legacy directory contents")
        class Fixture(Dav):
            online = True
            requests_seen = []
            request_attempts = []
            uploads = 0
            directory_requests = []
            directory_delays = {"/dav/slow": 2.2, "/dav/slow/level-one": 2.2, "/dav/slow/level-one/level-two": 2.2}
            failed_directories = set()
            failed_directory_requests = []
            directory_lock = threading.Lock()
            def etag(self, path):
                value = super().etag(path)
                return "W/" + value if path.name in ("GH666", "GH666Legacy") and path.is_dir() else value
            def handle_dav(self):
                type(self).request_attempts.append((self.command, self.path))
                path = self.path.rstrip("/")
                delay = 0
                if self.command == "PROPFIND" and self.headers.get("Depth") == "1":
                    with self.directory_lock:
                        self.directory_requests.append((path, time.monotonic()))
                        delay = self.directory_delays.pop(path, 0)
                    if path in self.failed_directories:
                        self.failed_directory_requests.append(path)
                        self.rfile.read(int(self.headers.get("Content-Length", 0)))
                        return self.reply(503)
                if delay:
                    time.sleep(delay)
                if not type(self).online:
                    self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    return self.reply(503)
                if self.command == "PUT" and self.path == "/dav/doc.txt":
                    type(self).uploads += 1
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
        stage = ipc / "Transfers"
        endpoint = ipc / "bridge.sock"
        env = dict(os.environ, RDRIVE_SMART_TEST_PASSWORD="fixture-password")
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        cli_base = [str(binary), "--home", str(home), "--json", "--progress", "off"]
        process = None
        log = (root / "bridge.log").open("w+")
        def command(*args, timeout=50):
            deadline = time.monotonic() + 45
            while True:
                result = subprocess.run(cli_base + list(args), env=env, capture_output=True, text=True, timeout=timeout)
                busy = any(message in result.stderr for message in
                           ("another rdrive process", "already locked", "state directory is in use"))
                if result.returncode == 0 or not busy:
                    return result
                assert time.monotonic() < deadline, (result.returncode, result.stderr)
                time.sleep(.1)
        def cli(*args):
            result = command(*args)
            assert result.returncode == 0, (result.returncode, result.stderr)
            return json.loads(result.stdout)
        def rpc(**message):
            settings = json.loads((ipc / "bridge-endpoint.json").read_text())
            message.update(v=1, auth_token=settings["token"])
            message.setdefault("domain", "smart-fixture")
            message.setdefault("request_id", uuid.uuid4().hex)
            with socket.create_connection(("127.0.0.1", settings["port"]), timeout=40) as client:
                client.sendall(json.dumps(message).encode() + b"\n")
                with client.makefile("rb") as stream:
                    while True:
                        line = stream.readline()
                        assert line, "bridge disconnected"
                        value = json.loads(line)
                        if "progress" not in value:
                            return value
        def ok(**message):
            response = rpc(**message)
            assert response.get("ok"), response
            return response
        def wait(check, label, seconds=50):
            deadline = time.monotonic() + seconds
            while True:
                result = check()
                if result:
                    return result
                assert time.monotonic() < deadline, label
                time.sleep(.2)
        def start(limited=False):
            nonlocal process
            command = cli_base + (["--limit", "65536"] if limited else [])
            process = subprocess.Popen(command + ["provider", "serve", "--socket", str(endpoint), "--staging", str(stage), "--development-tcp"], env=env, stdout=subprocess.DEVNULL, stderr=log)
            def ready():
                assert process.poll() is None, "bridge exited"
                try:
                    return rpc(op="domains").get("ok")
                except (OSError, json.JSONDecodeError):
                    return False
            wait(ready, "startup", 15)
        def stop(crash=False):
            nonlocal process
            if process and process.poll() is None:
                process.send_signal(signal.SIGKILL if crash else signal.SIGINT)
                process.wait(timeout=15)
            process = None
        def passed(text):
            checks.append(text)
            print("PASS " + text, flush=True)
        try:
            cli("profile", "add", "fixture", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_SMART_TEST_PASSWORD")
            cli("index", "config", "--disable", "--refresh-seconds", "3", "--active-refresh-seconds", "3")
            cli("smart", "setup", "smart-fixture", "fixture")
            root_started = time.monotonic()
            start()
            def listed():
                items = ok(op="list")["items"]
                return items if any(i["name"] == "doc.txt" for i in items) else None
            items = wait(listed, "initial root catalog must not recurse into slow children", 4.5)
            assert time.monotonic() - root_started < 5, "root browsing waited for a full remote-tree scan"
            assert not [path for path, _ in Fixture.directory_requests if path.startswith("/dav/slow")]
            initial_working_set = ok(op="list", parent_id="working_set")["items"]
            assert not any(item["name"] in ("child.txt", "level-one", "deep.txt", "keep.txt") for item in initial_working_set), "working_set recursively discovered unrequested children"
            assert not [path for path, _ in Fixture.directory_requests if path not in ("/dav",)]
            passed("root catalog stays responsive with a deep tree whose recursive metadata delays exceed five seconds; working_set stays lazy")
            doc = next(i for i in items if i["name"] == "doc.txt")
            assert not [r for r in Fixture.requests_seen if r[0] == "GET"]
            fetched = ok(op="fetch", id=doc["id"], base_content_version=doc["content_version"], base_metadata_version=doc["metadata_version"])
            assert Path(fetched["content_path"]).read_bytes() == b"original"
            Path(fetched["content_path"]).unlink()
            passed("metadata-only initial catalog and exact on-demand hydration")

            cli("smart", "pause")
            Fixture.online = False
            started = time.monotonic()
            assert ok(op="list")["items"]
            assert ok(op="item", id=doc["id"])["item"]["name"] == "doc.txt"
            assert ok(op="changes", anchor="0")["items"]
            assert time.monotonic() - started < 1.5
            passed("offline browsing, item lookup and change anchors return immediately")

            staged = stage / "local-edit"
            staged.write_bytes(b"durable offline edit")
            mutation = dict(op="modify", id=doc["id"], content_path=str(staged), base_content_version=doc["content_version"], base_metadata_version=doc["metadata_version"])
            response = rpc(**mutation)
            assert not response["ok"], "local retention was falsely acknowledged as a remote upload"
            first_status = cli("smart", "status")
            pending = first_status["domains"][0]["mutations"]
            assert len(pending) == 1, first_status
            staged.unlink()
            stop(crash=True)
            start()
            assert not rpc(op="status", id=doc["id"])["state"] == "online"
            cli("smart", "resume")
            Fixture.online = True
            wait(lambda: (remote / "doc.txt").read_bytes() == b"durable offline edit", "automatic reconnect upload")
            wait(lambda: ok(op="status", id=doc["id"])["state"] not in ("queued", "syncing"), "confirmed upload state")
            confirmed = cli("smart", "status")
            confirmed_domain = confirmed["domains"][0]
            assert all(m["payload_discarded"] for m in confirmed_domain["mutations"] if m["state"] == "done" and m.get("snapshot"))
            assert confirmed_domain["status"]["completed_snapshot_bytes"] == 0
            assert not list((home / "provider").glob("content-*")), "confirmed duplicate provider payload was not reclaimed"
            passed("offline edit survives source deletion and process crash, then uploads automatically on reconnect; confirmed transfer copies are reclaimed")

            staged.write_bytes(b"durable offline edit")
            mutation["content_path"] = str(staged)
            uploads = Fixture.uploads
            assert ok(**mutation)["item"]["id"] == doc["id"]
            assert Fixture.uploads == uploads, "OS retry duplicated the confirmed upload"
            passed("new transport request IDs replay the same confirmed receipt without another upload")

            # Preserve order after an attempted offline write; the next edit may
            # rebase only on the earlier write's exact confirmed receipt.
            base = ok(op="item", id=doc["id"])["item"]
            Fixture.online = False
            staged.write_bytes(b"first attempted offline save")
            ordered = dict(op="modify", id=doc["id"], content_path=str(staged), base_content_version=base["content_version"], base_metadata_version=base["metadata_version"])
            assert not rpc(**ordered)["ok"]
            staged.write_bytes(b"newest ordered offline save")
            assert not rpc(**ordered)["ok"]
            Fixture.online = True
            wait(lambda: (remote / "doc.txt").read_bytes() == b"newest ordered offline save", "ordered own-write chain")
            wait(lambda: ok(op="status", id=doc["id"])["state"] not in ("queued", "syncing"), "ordered receipts")
            assert ok(op="status", id=doc["id"])["state"] != "conflict"
            passed("successive offline saves replay in order using only confirmed own-write version guards")

            cli("smart", "pause")
            staged.write_bytes(b"first unsent create")
            create = dict(op="create", parent_id="root", name="offline-new.txt", kind="file", client_item_id="stable-native-create", content_path=str(staged))
            assert not rpc(**create)["ok"]
            staged.write_bytes(b"newest unsent create")
            assert not rpc(**create)["ok"]
            cli("smart", "resume")
            wait(lambda: (remote / "offline-new.txt").exists() and (remote / "offline-new.txt").read_bytes() == b"newest unsent create", "newest pending create")
            wait(lambda: any(i["name"] == "offline-new.txt" for i in ok(op="list")["items"]), "create receipt catalog")
            staged.write_bytes(b"create saved again after reconnect")
            assert ok(**create)["item"]["name"] == "offline-new.txt"
            assert (remote / "offline-new.txt").read_bytes() == b"create saved again after reconnect"
            passed("repeated saves to an offline new file retain every snapshot and publish the newest version safely")

            updated = ok(op="item", id=doc["id"])["item"]
            cli("smart", "pause")
            staged.write_bytes(b"local conflict copy")
            conflict = rpc(op="modify", id=doc["id"], content_path=str(staged), base_content_version=updated["content_version"], base_metadata_version=updated["metadata_version"])
            assert not conflict["ok"]
            (remote / "doc.txt").write_bytes(b"external writer wins")
            cli("smart", "resume")
            wait(lambda: ok(op="status", id=doc["id"])["state"] == "conflict", "conflict detection")
            assert (remote / "doc.txt").read_bytes() == b"external writer wins"
            status = cli("smart", "status")
            entry = next(m for m in status["domains"][0]["mutations"] if m["state"] == "conflict")
            export = root / "recovered.txt"
            cli("smart", "export", "smart-fixture", entry["id"], str(export))
            assert export.read_bytes() == b"local conflict copy"
            passed("concurrent remote changes stop overwrites and retain an exportable local conflict copy")

            weak_folder = next(item for item in items if item["name"] == "GH666")
            no_op = dict(op="modify", id=weak_folder["id"], name=weak_folder["name"], parent_id="root",
                         request_id="weak-folder-no-op", base_content_version=weak_folder["content_version"],
                         base_metadata_version=weak_folder["metadata_version"])
            request_offset = len(Fixture.request_attempts)
            updated_folder = ok(**no_op)
            assert updated_folder["item"]["id"] == weak_folder["id"]
            assert not updated_folder["item"]["has_unsynced_changes"]
            assert ok(**no_op)["item"]["id"] == weak_folder["id"]
            assert all(method == "PROPFIND" for method, _ in Fixture.request_attempts[request_offset:]), "a directory no-op downloaded or wrote remote contents"
            assert (remote / "GH666/keep.txt").read_bytes() == b"untouched directory contents"
            passed("a weak-identity directory no-op and its exact retry succeed without content hydration or remote writes")

            folder = next(i for i in items if i["name"] == "folder")
            ok(op="pin", id=folder["id"], pinned=True)
            def folder_children():
                children = ok(op="list", parent_id=folder["id"])["items"]
                return children if any(item["name"] == "child.txt" for item in children) else None
            children = wait(folder_children, "first browsed folder metadata")
            assert all(i["pinned"] for i in children)
            (remote / "folder" / "new.txt").write_bytes(b"new")
            wait(lambda: any(i["name"] == "new.txt" and i["pinned"] for i in ok(op="list", parent_id=folder["id"])["items"]), "recursive new-child pin")
            ok(op="pin", id=folder["id"], pinned=False)
            assert all(not i["pinned"] for i in ok(op="list", parent_id=folder["id"])["items"])
            passed("folder pin is inherited by existing and new descendants and reverses on unpin")

            # A stale metadata-only directory callback can contain no actual
            # name, parent, tag, or content change. Repair only that callback
            # while retained descendant edits keep their original guards.
            folder_now = ok(op="item", id=folder["id"])["item"]
            stale = rpc(op="modify", id=folder["id"], name=folder_now["name"],
                        parent_id=folder_now["parent_id"], base_content_version="obsolete-directory-snapshot",
                        base_metadata_version=folder_now["metadata_version"])
            assert not stale["ok"], stale
            stale_entry = next(mutation for mutation in cli("smart", "status")["domains"][0]["mutations"]
                               if mutation["request"].get("id") == folder["id"] and mutation["state"] == "conflict")
            assert stale_entry["error"] == "version_conflict: item changed since the system snapshot", stale_entry
            assert stale_entry.get("snapshot") is None

            cli("smart", "pause")
            child = next(item for item in ok(op="list", parent_id=folder["id"])["items"] if item["name"] == "child.txt")
            staged.write_bytes(b"retained descendant edit during parent repair")
            assert not rpc(op="modify", id=child["id"], content_path=str(staged),
                           base_content_version=child["content_version"], base_metadata_version=child["metadata_version"])["ok"]
            assert not rpc(op="create", parent_id=folder["id"], name="pending-child", kind="directory",
                           client_item_id="pending-parent-repair-child")["ok"]
            staged.unlink()
            cli("index", "config", "--disable", "--refresh-seconds", "3600", "--active-refresh-seconds", "3600")
            smart_root = home / "smart" / hashlib.sha256(b"smart-fixture").hexdigest()
            queue_path = smart_root / "queue.json"
            before = json.loads(queue_path.read_text())["mutations"]
            child_entry = next(mutation for mutation in before.values()
                               if mutation["request"].get("id") == child["id"] and mutation.get("snapshot"))
            snapshot = smart_root / "dirty" / child_entry["snapshot"]["file"]
            child_contents = snapshot.read_bytes()
            Fixture.online = False
            request_offset = len(Fixture.request_attempts)
            with (smart_root / "queue.lock").open("r+b") as queue_lock:
                fcntl.flock(queue_lock, fcntl.LOCK_EX)
                availability = ok(op="smart_repair_status", id=folder["id"])
                assert availability["eligible"] and availability["mutation_id"] == stale_entry["id"], availability
                assert availability["related_pending"] >= 2, availability

            for incomplete in (dict(id=folder["id"]), dict(mutation_id=stale_entry["id"])):
                assert not rpc(op="repair_stale_directory", **incomplete)["ok"]
            wrong = rpc(op="repair_stale_directory", id=folder["id"], mutation_id=child_entry["id"])
            assert not wrong["ok"], wrong
            repaired = ok(op="repair_stale_directory", id=folder["id"], mutation_id=stale_entry["id"])
            assert repaired["repaired"] and repaired["refresh_queued"] and not repaired["remote_changed"], repaired
            repeated = ok(op="repair_stale_directory", id=folder["id"], mutation_id=stale_entry["id"])
            assert repeated["repaired"] and repeated["refresh_queued"] and not repeated["remote_changed"], repeated
            after = json.loads(queue_path.read_text())["mutations"]
            assert after[stale_entry["id"]]["state"] == "cancelled"
            assert {key: value for key, value in after.items() if key != stale_entry["id"]} == {
                key: value for key, value in before.items() if key != stale_entry["id"]
            }, "repair changed another mutation or its original request guards"
            assert snapshot.read_bytes() == child_contents
            assert (remote / "folder/child.txt").read_bytes() == b"child"
            assert not (remote / "folder/pending-child").exists()
            assert Fixture.request_attempts[request_offset:] == [], "offline no-op repair contacted the server"
            acknowledged = ok(**stale_entry["request"])
            assert acknowledged["item"]["id"] == folder["id"]
            assert acknowledged["item"]["sync_state"] != "conflict", acknowledged
            assert json.loads(queue_path.read_text())["mutations"] == after, "acknowledging the obsolete callback changed queue history"
            assert snapshot.read_bytes() == child_contents and not Fixture.request_attempts[request_offset:]
            # A paused broker still publishes local overlays under client.lock.
            # The negative CLI assertion must reach the cancellation guard,
            # even if it first meets a brief background writer lease.
            with (home / "client.lock").open("r+b") as writer_lock:
                fcntl.flock(writer_lock, fcntl.LOCK_EX)
                release = threading.Timer(.5, lambda: fcntl.flock(writer_lock, fcntl.LOCK_UN))
                release.start()
                try:
                    denied_retry = command("smart", "retry", "smart-fixture", stale_entry["id"], timeout=30)
                    assert denied_retry.returncode != 0 and "cancelled" in denied_retry.stderr, (denied_retry.returncode, denied_retry.stderr)
                finally:
                    release.join(timeout=2)
                    fcntl.flock(writer_lock, fcntl.LOCK_UN)
            assert not ok(op="smart_repair_status", id=folder["id"])["eligible"]
            Fixture.online = True
            cli("index", "config", "--refresh-seconds", "3", "--active-refresh-seconds", "3")
            cli("smart", "resume")
            wait(lambda: (remote / "folder/child.txt").read_bytes() == child_contents,
                 "preserved descendant edit replays after parent no-op repair")
            wait(lambda: (remote / "folder/pending-child").is_dir(), "preserved descendant creation")
            passed("offline stale-directory no-op repair stays readable under queue locks and preserves descendant requests, snapshots and write guards")

            cli("smart", "pause")
            current = ok(op="item", id=folder["id"])["item"]
            ok(op="native_error", id=folder["id"], base_content_version=current["content_version"], native_message="Cache budget cannot fit this file")
            assert ok(op="status", id=folder["id"])["state"] == "error"
            ok(op="native_error", id=folder["id"], base_content_version=current["content_version"], native_message=None)
            ok(op="cache_budget", native_accounting_id="fileprovider:smart-fixture", native_cached_versions={folder["id"]:current["content_version"]}, native_cached_versions_reset=True)
            assert ok(op="status", id=folder["id"])["state"] == "cached"
            wait(lambda: ok(op="item", id=folder["id"])["item"]["sync_state"] == "cached", "cached overlay while paused")
            ok(op="cache_budget", native_accounting_id="fileprovider:smart-fixture", native_cached_versions={}, native_cached_versions_reset=True)
            passed("version-bound native errors and cached state update Finder metadata even while uploads are paused")

            cli("cache", "config", "--max-bytes", "10")
            budget = ok(op="cache_budget", native_accounting_id="fileprovider:smart-fixture", native_used_bytes=8, native_reserved_bytes=0, native_pinned_bytes=0)
            assert budget["used_bytes"] == 8
            denied = rpc(op="cache_budget", native_accounting_id="fileprovider:smart-fixture", native_used_bytes=8, native_reserved_bytes=4, native_pinned_bytes=0)
            assert not denied["ok"]
            assert ok(op="cache_budget")["used_bytes"] == 8
            blocked = command("cache", "fetch", "fixture", "four.txt", timeout=30)
            assert blocked.returncode != 0 and "cache capacity is insufficient" in blocked.stderr, (blocked.returncode, blocked.stderr)
            cli("cache", "config", "--max-bytes", "12")
            cli("cache", "fetch", "fixture", "four.txt")
            budget = ok(op="cache_budget")
            assert budget["used_bytes"] == 12 and budget["rust_used_bytes"] == 4
            ok(op="cache_budget", native_accounting_id="fileprovider:smart-fixture", native_used_bytes=0, native_reserved_bytes=0, native_pinned_bytes=0)
            cli("cache", "config", "--max-bytes", "0")
            assert export.read_bytes() == b"local conflict copy"
            assert cli("smart", "status")["domains"][0]["status"]["dirty_bytes"] >= len(b"local conflict copy")
            passed("persistent combined cache cap denies over-admission and never evicts retained dirty edits")

            # A process can die while an atomic demand write is still temporary.
            # An incomplete orphan must never poison subsequent committed scopes.
            demands = home / "provider" / ("demands-" + hashlib.sha256(b"smart-fixture").hexdigest())
            demands.mkdir(mode=0o700, exist_ok=True)
            orphan = demands / (".rdrive-" + str(uuid.uuid4()) + ".tmp")
            descriptor = os.open(orphan, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            try:
                os.write(descriptor, b'{"incomplete":')
            finally:
                os.close(descriptor)
            cli("smart", "resume")
            slow = next(item for item in items if item["name"] == "slow")
            parent = slow
            for relative, child_name in (("slow", "level-one"), ("slow/level-one", "level-two"), ("slow/level-one/level-two", "deep.txt")):
                request_path = "/dav/" + relative
                assert not any(path == request_path for path, _ in Fixture.directory_requests), "a cold descendant was scanned before it was browsed"
                started = time.monotonic()
                cold = ok(op="list", parent_id=parent["id"])["items"]
                assert not cold and time.monotonic() - started < 1.5, "cold directory browsing waited for network metadata"
                wait(lambda: any(path == request_path for path, _ in Fixture.directory_requests), "requested metadata scan " + relative)
                started = time.monotonic()
                assert any(item["name"] == "doc.txt" for item in ok(op="list")["items"])
                assert time.monotonic() - started < 1.5, "cached root was blocked by a delayed child scan"
                parent_id = parent["id"]
                parent = wait(lambda: next((item for item in ok(op="list", parent_id=parent_id)["items"] if item["name"] == child_name), None), "on-demand child " + relative)
            passed("each cold child scans only after explicit browsing and delayed child metadata never blocks cached root reads")

            preserved = next(item for item in items if item["name"] == "preserved")
            nested = wait(lambda: next((item for item in ok(op="list", parent_id=preserved["id"])["items"] if item["name"] == "nested"), None), "preserved subtree parent")
            kept = wait(lambda: next((item for item in ok(op="list", parent_id=nested["id"])["items"] if item["name"] == "keep.txt"), None), "preserved subtree file")
            Fixture.failed_directories.update({"/dav/preserved", "/dav/preserved/nested"})
            (remote / "root-marker.txt").write_bytes(b"forces a successful root refresh")
            wait(lambda: any(item["name"] == "root-marker.txt" for item in ok(op="list")["items"]), "independent root refresh", 90)
            wait(lambda: any(path.startswith("/dav/preserved") for path in Fixture.failed_directory_requests), "actual failed descendant metadata refresh", 90)
            for item in (preserved, nested, kept):
                assert ok(op="item", id=item["id"])["item"]["id"] == item["id"], "an unscanned or failed sibling subtree was treated as deleted"
            known = {item["id"] for item in ok(op="list", parent_id="working_set")["items"]}
            assert {preserved["id"], nested["id"], kept["id"]} <= known
            passed("successful root refresh retains cached descendant metadata when an independent directory scan fails")

            anchor = ok(op="changes", anchor="current")["anchor"]
            shutil.rmtree(remote / "preserved")
            removed_ids = {preserved["id"], nested["id"], kept["id"]}
            wait(lambda: not ({item["id"] for item in ok(op="list", parent_id="working_set")["items"]} & removed_ids), "known directory subtree deletion", 90)
            deleted = set(ok(op="changes", anchor=anchor)["deleted"])
            assert removed_ids <= deleted, "known descendants were not removed after their parent disappeared from a successful listing"
            assert ok(op="item", id=doc["id"])["item"]["id"] == doc["id"]
            passed("successful parent listing removes a remotely deleted known directory and its cached descendants while preserving siblings")

            pinned = next(item for item in items if item["name"] == "pinned-lazy")
            assert not any(path.startswith("/dav/pinned-lazy") for path, _ in Fixture.directory_requests), "working_set browsing implicitly scanned an unrequested unpinned directory"
            ok(op="pin", id=pinned["id"], pinned=True)
            (remote / "pinned-lazy" / "new-parent" / "new-child").mkdir(parents=True)
            (remote / "pinned-lazy" / "new-parent" / "new-child" / "new-deep.txt").write_bytes(b"discovered by pin policy")
            discovered = wait(lambda: next((item for item in ok(op="list", parent_id="working_set")["items"] if item["name"] == "new-deep.txt"), None), "recursive pin discovery without child browsing", 120)
            assert discovered["pinned"]
            assert ok(op="item", id=pinned["id"])["item"]["pinned"]
            passed("pinned directory policy recursively discovers new nested descendants without browsing those children")


            # Network payload progress remains attached to the actual Finder
            # callback even though Smart first retains an immutable local copy.
            stop()
            start(limited=True)
            payload = b"finder-progress-payload" * 4096
            source = stage / "progress-source"
            source.write_bytes(payload)
            def measured(**message):
                settings = json.loads((ipc / "bridge-endpoint.json").read_text())
                message.update(v=1, auth_token=settings["token"], domain="smart-fixture", request_id=uuid.uuid4().hex)
                frames = []
                during = None
                with socket.create_connection(("127.0.0.1", settings["port"]), timeout=40) as client:
                    client.sendall(json.dumps(message).encode() + b"\n")
                    with client.makefile("rb") as stream:
                        for line in stream:
                            value = json.loads(line)
                            if "progress" not in value:
                                assert value.get("ok"), value
                                return value, frames, during
                            progress = value["progress"]
                            frames.append(progress)
                            if during is None and progress.get("completed", 0) > 0:
                                started = time.monotonic()
                                during = ok(op="transfer_status")["transfers"]
                                assert time.monotonic() - started < 1.5, "payload progress query waited for the uploader's writer lock"
                raise AssertionError("payload callback disconnected")
            uploaded, upload_frames, during = measured(op="create", parent_id="root", name="progress.bin", kind="file", client_item_id="progress-local", content_path=str(source))
            numeric = [frame for frame in upload_frames if frame.get("total") == len(payload)]
            assert numeric and any(0 < frame["completed"] < len(payload) for frame in numeric), upload_frames
            assert all(frame["completed"] < len(payload) for frame in numeric), "transport completion was shown as a committed receipt"
            assert any(row["id"] == "progress-local" and row["background"] is False and row["mutation_id"] and row["total"] == len(payload) for row in during), during
            final = next(row for row in ok(op="transfer_status")["transfers"] if row["id"] == "progress-local")
            assert final["state"] == "completed" and final["completed"] == len(payload), final
            assert (remote / "progress.bin").read_bytes() == payload
            passed("foreground Smart upload reports primary byte progress during the original callback; polling bypasses writer locks and completion waits for a durable receipt")
            downloaded, download_frames, _ = measured(op="fetch", id=uploaded["item"]["id"])
            assert Path(downloaded["content_path"]).read_bytes() == payload
            Path(downloaded["content_path"]).unlink()
            assert any(frame.get("total") == len(payload) and 0 < frame["completed"] < len(payload) for frame in download_frames), download_frames
            passed("on-demand download reports bounded primary byte progress without counting metadata or verification traffic")
            cli("smart", "pause")
            background_source = stage / "background-progress-source"
            background_source.write_bytes(payload)
            queued = rpc(op="create", parent_id="root", name="background-progress.bin", kind="file", client_item_id="background-local", content_path=str(background_source))
            assert not queued.get("ok"), queued
            cli("smart", "resume")
            def background_progress():
                return next((row for row in ok(op="transfer_status")["transfers"] if row["id"] == "background-local" and row["background"] is True and row["state"] == "transferring" and row["completed"] > 0), None)
            active = wait(background_progress, "active background payload progress", 30)
            assert active["mutation_id"] and active["total"] == len(payload), active
            completed = wait(lambda: next((row for row in ok(op="transfer_status")["transfers"] if row["transfer_id"] == active["transfer_id"] and row["state"] == "completed"), None), "background durable receipt", 30)
            assert completed["completed"] == len(payload) and (remote / "background-progress.bin").read_bytes() == payload
            assert not ok(op="transfer_status", domain="different-domain")["transfers"]
            passed("background reconnect exposes stable item and mutation identities plus live bytes; terminal success is retained only after receipt publication")

            # Reproduce an index created before temporary-file exclusion was
            # enabled. These are test-only rule changes in the disposable home.
            stop()
            cli("filter", "config", "--include-temporary")
            cli("index", "config", "--disable", "--refresh-seconds", "3600", "--active-refresh-seconds", "3600")
            start()
            backup = next(item for item in ok(op="list")["items"] if item["name"] == "Backup")
            ok(op="refresh_index", id=backup["id"])
            part = wait(lambda: next((item for item in ok(op="list", parent_id=backup["id"])["items"] if item["name"] == "remote.part"), None),
                        "legacy temporary-file catalog")
            database = home / "provider" / (hashlib.sha256(b"smart-fixture").hexdigest() + ".sqlite3")
            def backup_refresh_complete():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as catalog:
                    control = json.loads(catalog.execute("SELECT control_json FROM meta").fetchone()[0])
                return "finder:" + backup["id"] in control.get("completed_demands", {})
            wait(backup_refresh_complete, "legacy directory refresh completion")
            cli("smart", "pause")
            stop()
            cli("filter", "config", "--exclude-temporary")
            start()
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as catalog:
                assert catalog.execute("SELECT COUNT(*) FROM items WHERE path=?", ("Backup/remote.part",)).fetchone()[0] == 1
            cli("smart", "resume")
            backup_now = ok(op="item", id=backup["id"])["item"]
            assert ok(op="modify", id=backup["id"], name=backup_now["name"], parent_id=backup_now["parent_id"],
                      base_content_version=backup_now["content_version"], base_metadata_version=backup_now["metadata_version"])["item"]["id"] == backup["id"]
            retained_bytes = b"genuine user edit to an excluded file"
            filtered_source = stage / "retained-filter-source"
            filtered_source.write_bytes(retained_bytes)
            excluded_modify = rpc(op="modify", id=part["id"], content_path=str(filtered_source),
                                  base_content_version=part["content_version"], base_metadata_version=part["metadata_version"])
            assert not excluded_modify["ok"] and excluded_modify["error"]["code"] == "excluded_by_filter", excluded_modify
            assert ok(op="status", id=part["id"])["state"] == "blocked"
            excluded_create = rpc(op="create", parent_id=backup["id"], name="new.part", kind="file",
                                  client_item_id="genuine-excluded-create", content_path=str(filtered_source))
            assert not excluded_create["ok"] and excluded_create["error"]["code"] == "excluded_by_filter", excluded_create
            assert not ok(op="smart_repair_status", id=part["id"])["eligible"]
            assert not ok(op="smart_repair_status", id="genuine-excluded-create")["eligible"]
            assert (remote / "Backup/remote.part").read_bytes() == b"existing excluded temporary contents"
            assert not (remote / "Backup/new.part").exists()
            passed("an excluded cached descendant does not block a directory no-op; genuine excluded writes return a local rule error, retain their contents and cannot use no-op repair")

            cli("smart", "pause")
            legacy_request = dict(op="modify", id=backup["id"], name=backup["name"], parent_id="root",
                                  base_content_version=backup_now["content_version"], base_metadata_version="legacy-filter-blocked-directory-snapshot")
            assert not rpc(**legacy_request)["ok"]
            stop()
            stored = json.loads(queue_path.read_text())
            selected = next(row for row in stored["mutations"].values()
                            if row["request"].get("id") == backup["id"] and row["state"] == "pending")
            # Preserve the actual enqueued immutable request and fingerprint;
            # only reproduce the legacy local failure state while stopped.
            selected["state"] = "interrupted"
            selected["attempts"] = 150
            selected["error"] = "server_unreachable: synchronization filters exclude Backup/remote.part; local mutation retained until the rules allow it"
            queue_path.write_text(json.dumps(stored))
            before = json.loads(queue_path.read_text())["mutations"]
            snapshots = {row["id"]: (smart_root / "dirty" / row["snapshot"]["file"]).read_bytes()
                         for row in before.values() if row.get("snapshot") and not row.get("payload_discarded")}
            assert any(row["request"].get("smart_origin_id") == "genuine-excluded-create" and row["id"] in snapshots for row in before.values())
            config_before = (home / "config.json").read_bytes()
            Fixture.online = False
            request_offset = len(Fixture.request_attempts)
            start()
            wait(lambda: ok(op="status", id=backup["id"])["state"] == "blocked", "legacy filter block overlay")
            preview = ok(op="smart_repair_status", id=backup["id"])
            assert preview["eligible"] and preview["mutation_id"] == selected["id"], preview
            assert preview["filter_path"] == "Backup/remote.part" and "*.part" in preview["filter_reason"], preview
            repaired = ok(op="repair_stale_directory", id=backup["id"], mutation_id=selected["id"])
            assert repaired["repaired"] and repaired["refresh_queued"] and not repaired["remote_changed"], repaired
            repeated = ok(**legacy_request)
            assert repeated["cancelled_no_op"] and repeated["item"]["id"] == backup["id"] and not repeated["remote_changed"], repeated
            after = json.loads(queue_path.read_text())["mutations"]
            assert after[selected["id"]]["state"] == "cancelled"
            assert {key: value for key, value in before.items() if key != selected["id"]} == {
                key: value for key, value in after.items() if key != selected["id"]}
            for mutation_id, contents in snapshots.items():
                assert (smart_root / "dirty" / after[mutation_id]["snapshot"]["file"]).read_bytes() == contents
            assert (home / "config.json").read_bytes() == config_before
            assert (remote / "Backup/remote.part").read_bytes() == b"existing excluded temporary contents"
            assert not (remote / "Backup/new.part").exists()
            assert Fixture.request_attempts[request_offset:] == [], "local filter-blocked repair or its retry contacted the server"
            assert not ok(op="smart_repair_status", id=backup["id"])["eligible"]
            passed("legacy filter-blocked directory no-op repair and the exact OS retry stay offline and preserve excluded file edits, immutable queue records, snapshots, remote contents and rules")

            legacy_folder = next(item for item in ok(op="list")["items"] if item["name"] == "GH666Legacy")
            weak_now = ok(op="item", id=legacy_folder["id"])["item"]
            legacy_metadata_request = dict(op="modify", id=legacy_folder["id"], name="GH666Legacy", parent_id="root",
                                           request_id="legacy-weak-directory-metadata",
                                           base_content_version=weak_now["content_version"],
                                           base_metadata_version=weak_now["metadata_version"])
            assert not rpc(**legacy_metadata_request)["ok"]
            stop()
            stored = json.loads(queue_path.read_text())
            selected = next(row for row in stored["mutations"].values()
                            if row["request"].get("id") == legacy_folder["id"] and row["state"] == "pending")
            selected.update(state="failed", attempts=2, error="unsupported: fetch this file before editing metadata")
            queue_path.write_text(json.dumps(stored))
            original_request = selected["request"]
            replay = dict(original_request)
            replay["base_metadata_version"] = original_request["base_metadata_version"].split(":")[0] + ":legacy-replay-overlay"
            fingerprint_fields = [replay.get(key) for key in ("op", "domain", "id", "parent_id", "name", "kind",
                                  "base_content_version", "base_metadata_version", "tag_data_hex", "extended_attributes")]
            fingerprint_fields.extend([None, replay.get("recursive", False)])
            fingerprint = hashlib.sha256(json.dumps(fingerprint_fields, separators=(",", ":")).encode()).hexdigest()
            intent = dict(fingerprint=fingerprint, metadata=None, source_fingerprint=None, transaction=None,
                          confirmed=None, response=None, deletion_path=None, deletion_expected=None,
                          metadata_base_fingerprint=None, namespace=None, version=None)
            intent_key = hashlib.sha256(b"smart-fixture").hexdigest() + ":" + replay["request_id"]
            with sqlite3.connect(database) as catalog:
                control = json.loads(catalog.execute("SELECT control_json FROM meta").fetchone()[0])
                control["requests"][intent_key] = intent
                control["smart_replays"][selected["id"]] = replay
                catalog.execute("UPDATE meta SET control_json=?", (json.dumps(control),))
                record = json.loads(catalog.execute("SELECT record_json FROM items WHERE id=?", (legacy_folder["id"],)).fetchone()[0])
                assert record["entry"]["etag"].startswith("W/") and record["strong"] is None
            before = json.loads(queue_path.read_text())["mutations"]
            snapshots = {row["id"]: (smart_root / "dirty" / row["snapshot"]["file"]).read_bytes()
                         for row in before.values() if row.get("snapshot") and not row.get("payload_discarded")}
            remote_before = {path.relative_to(remote).as_posix(): path.read_bytes() for path in remote.rglob("*") if path.is_file()}
            config_before = (home / "config.json").read_bytes()
            request_offset = len(Fixture.request_attempts)
            start()
            wait(lambda: ok(op="status", id=legacy_folder["id"])["state"] == "error", "legacy directory metadata failure overlay")
            preview = ok(op="smart_repair_status", id=legacy_folder["id"])
            assert preview["eligible"] and preview["mutation_id"] == selected["id"], preview
            repaired = ok(op="repair_stale_directory", id=legacy_folder["id"], mutation_id=selected["id"])
            assert repaired["repaired"] and repaired["refresh_queued"] and not repaired["remote_changed"], repaired
            repeated = ok(op="repair_stale_directory", id=legacy_folder["id"], mutation_id=selected["id"])
            assert repeated["repaired"] and not repeated["remote_changed"], repeated
            acknowledged = ok(**legacy_metadata_request)
            assert acknowledged["cancelled_no_op"] and acknowledged["item"]["sync_state"] != "error", acknowledged
            after = json.loads(queue_path.read_text())["mutations"]
            assert after[selected["id"]]["state"] == "cancelled"
            assert {key: value for key, value in before.items() if key != selected["id"]} == {
                key: value for key, value in after.items() if key != selected["id"]}
            for mutation_id, contents in snapshots.items():
                assert (smart_root / "dirty" / after[mutation_id]["snapshot"]["file"]).read_bytes() == contents
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as catalog:
                control = json.loads(catalog.execute("SELECT control_json FROM meta").fetchone()[0])
                assert control["requests"][intent_key] == intent, "repair altered the original unpublished provider intent"
                assert control["smart_replays"][selected["id"]]["base_metadata_version"] == replay["base_metadata_version"]
            assert (home / "config.json").read_bytes() == config_before
            assert {path.relative_to(remote).as_posix(): path.read_bytes() for path in remote.rglob("*") if path.is_file()} == remote_before
            assert Fixture.request_attempts[request_offset:] == [], "legacy metadata no-op repair or its retry contacted the server"
            assert not ok(op="smart_repair_status", id=legacy_folder["id"])["eligible"]
            passed("legacy failed weak-directory no-op repair validates an empty intent with its saved overlay revision, works offline, and preserves other requests, snapshots, remote contents and configuration")
        finally:
            stop()
            server.shutdown()
            server.server_close()
            log.close()
    report = {"binary":str(binary),"version":subprocess.check_output([str(binary),"--version"],text=True).strip(),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"checks":checks,"passed":len(checks),"scope":"synthetic loopback DAV; no real remote mutations"}
    version = report["version"].split()[-1]
    suffix = ".debug" if "debug" in binary.parts else ""
    report["binary"] = str(binary.relative_to(ROOT)) if binary.is_relative_to(ROOT) else binary.name
    report_path = Path(os.environ["RDRIVE_SMART_REPORT"]).resolve() if os.environ.get("RDRIVE_SMART_REPORT") else ROOT / f"docs/SMART_VALIDATION_{version}{suffix}.json"
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps(report,ensure_ascii=False))


if __name__ == "__main__":
    run(Path(sys.argv[1] if len(sys.argv)>1 else ROOT/"target/release/rdrive").resolve())
