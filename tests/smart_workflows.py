#!/usr/bin/env python3
"""Synthetic end-to-end Smart catalog, durable edit, reconnect and quota tests.
No real cloud account, mount, or Finder domain is used by this script.
"""
import http.server
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import socket
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
        class Fixture(Dav):
            online = True
            requests_seen = []
            uploads = 0
            directory_requests = []
            directory_delays = {"/dav/slow": 2.2, "/dav/slow/level-one": 2.2, "/dav/slow/level-one/level-two": 2.2}
            failed_directories = set()
            failed_directory_requests = []
            directory_lock = threading.Lock()
            def handle_dav(self):
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
        def cli(*args):
            deadline = time.monotonic() + 45
            while True:
                result = subprocess.run(cli_base + list(args), env=env, capture_output=True, text=True, timeout=50)
                if result.returncode == 0:
                    return json.loads(result.stdout)
                if "another rdrive process" not in result.stderr and "already locked" not in result.stderr and "state directory is in use" not in result.stderr:
                    raise AssertionError(result.stderr)
                assert time.monotonic() < deadline, result.stderr
                time.sleep(.1)
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
        def start():
            nonlocal process
            process = subprocess.Popen(cli_base + ["provider", "serve", "--socket", str(endpoint), "--staging", str(stage), "--development-tcp"], env=env, stdout=subprocess.DEVNULL, stderr=log)
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
            blocked = subprocess.run(cli_base + ["cache", "fetch", "fixture", "four.txt"],env=env,capture_output=True,text=True,timeout=30)
            assert blocked.returncode != 0, blocked.stdout
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
        finally:
            stop()
            server.shutdown()
            server.server_close()
            log.close()
    report = {"binary":str(binary),"version":subprocess.check_output([str(binary),"--version"],text=True).strip(),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"checks":checks,"passed":len(checks),"scope":"synthetic loopback DAV; no real remote mutations"}
    version = report["version"].split()[-1]
    suffix = ".debug" if "debug" in binary.parts else ""
    report["binary"] = str(binary.relative_to(ROOT)) if binary.is_relative_to(ROOT) else binary.name
    report_path = ROOT / f"docs/SMART_VALIDATION_{version}{suffix}.json"
    report_path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps(report,ensure_ascii=False))


if __name__ == "__main__":
    run(Path(sys.argv[1] if len(sys.argv)>1 else ROOT/"target/release/rdrive").resolve())
