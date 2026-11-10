#!/usr/bin/env python3
"""Guarded Smart namespace operations against isolated authenticated local DAV."""
import concurrent.futures
import hashlib
import http.server
import json
from pathlib import Path
import plistlib
import re
import signal
import subprocess
import sys
import tempfile
import threading
from urllib.parse import urlsplit

from index_workflows import Broker, IndexedDav, ROOT, read_provider_journal, report_binary_path, sqlite_items, wait


class NamespaceDav(IndexedDav):
    before_move = None
    move_gate = None
    checked_moves = []

    def handle_dav(self):
        if self.command == "MOVE":
            source = self.resource()
            destination = self.resource(self.headers["Destination"])
            hook, type(self).before_move = type(self).before_move, None
            if hook:
                hook(source, destination)
            condition = self.headers.get("If", "")
            match = re.fullmatch(r'<([^>]+)> \(\[("[^"\r\n]*")\](?: <[^>]+>)?\)', condition)
            if match:
                assert urlsplit(match[1]).path == urlsplit(self.path).path
                assert self.headers.get("Overwrite") == "F"
                type(self).checked_moves.append((self.path, self.headers["Destination"]))
                if not source.exists() or self.etag(source) != match[2]:
                    return self.reply(412)
            elif ".rdrive-repository/" not in self.path:
                raise AssertionError("ordinary provider MOVE lacked a tagged source guard: " + condition)
        return super().handle_dav()

    def reply(self, status, body=b"", **headers):
        gate = type(self).move_gate
        if self.command == "MOVE" and status in (200, 201, 204) and gate and urlsplit(self.headers["Destination"]).path == gate["path"]:
            type(self).move_gate = None
            gate["entered"].set()
            assert gate["release"].wait(40), "MOVE acknowledgement gate timed out"
        return super().reply(status, body, **headers)


def run(binary):
    checks = []
    def passed(message):
        checks.append(message)
        print("PASS " + message, flush=True)
    with tempfile.TemporaryDirectory(prefix="rdrive-ns-", dir="/private/tmp" if Path("/private/tmp").is_dir() else None) as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        for name in ("files", "target", "folder/sub", "race/sub", "erase/sub", "offline/sub", "crash", "filter-delete/sub", "filter-move/sub", "filter-background/sub", "stale-parent/stale/sub"):
            (remote / name).mkdir(parents=True, exist_ok=True)
        for path in ("files/doc.txt", "files/collision.txt", "files/compound.txt", "target/occupied.txt", "folder/sub/child.txt", "race/sub/child.txt", "erase/sub/child.txt", "offline/sub/child.txt", "crash/doc.txt", "filter-delete/sub/child.txt", "filter-move/sub/child.txt", "filter-background/sub/child.txt", "filter-file.txt"):
            (remote / path).write_bytes(("original " + path).encode())
        NamespaceDav.root = remote
        NamespaceDav.online = True
        NamespaceDav.gates = {}
        NamespaceDav.failed = set()
        NamespaceDav.requests_seen = []
        NamespaceDav.checked_moves = []
        NamespaceDav.locks = {}
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), NamespaceDav)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        broker_root = root / "broker"
        broker_root.mkdir()
        broker = Broker(binary, broker_root, f"http://127.0.0.1:{server.server_port}/dav/", "namespaces")
        def item(path):
            return wait(lambda: sqlite_items(broker.home, broker.domain).get(path), path + " indexed", 30)
        def current(identifier):
            return broker.ok(op="item", id=identifier)["item"]
        def modify(value, **extra):
            return dict(op="modify", id=value["id"], base_content_version=value["content_version"], base_metadata_version=value["metadata_version"], **extra)
        def remove(value):
            return dict(op="delete", id=value["id"], recursive=True, base_content_version=value["content_version"], base_metadata_version=value["metadata_version"])
        def mutations():
            return [mutation for domain in broker.cli("smart", "list", broker.domain) for mutation in domain["mutations"]]
        def cancel_conflicts():
            for mutation in mutations():
                if mutation["state"] in ("conflict", "failed", "interrupted", "pending"):
                    broker.cli("smart", "cancel", broker.domain, mutation["id"])
        try:
            broker.configure()
            broker.start()
            item("offline/sub/child.txt")
            doc = item("files/doc.txt")
            broker.ok(op="fetch", id=doc["id"], base_content_version=doc["content_version"])
            tag = plistlib.dumps(["Namespace\n4"], fmt=plistlib.FMT_BINARY).hex()
            doc = broker.ok(**modify(current(doc["id"]), tag_data_hex=tag))["item"]
            broker.ok(op="pin", id=doc["id"], pinned=True)
            doc = current(doc["id"])
            moved = broker.ok(**modify(doc, name="renamed.txt", parent_id=item("target")["id"]))["item"]
            assert moved["id"] == doc["id"] and moved["tag_data_hex"] == tag and moved["pinned"]
            assert not (remote / "files/doc.txt").exists()
            assert (remote / "target/renamed.txt").read_bytes() == b"original files/doc.txt"
            sidecars = [json.loads(path.read_bytes()) for path in (remote / ".rdrive-metadata").rglob("*.json")]
            assert any(value["path"] == "target/renamed.txt" and value["record"]["attributes"].get("com.apple.metadata:_kMDItemUserTags") == tag for value in sidecars)
            passed("guarded file rename/reparent keeps the item ID, tags, pin and content")

            compound = item("files/compound.txt")
            staged = broker.stage / "compound-content"
            staged.write_bytes(b"compound renamed body")
            combined = broker.ok(**modify(compound, name="compound-renamed.txt", parent_id=item("target")["id"], content_path=str(staged), tag_data_hex=tag))["item"]
            assert combined["id"] == compound["id"] and combined["tag_data_hex"] == tag
            assert (remote / "target/compound-renamed.txt").read_bytes() == b"compound renamed body"
            passed("a combined rename/content/metadata request completes its durable follow-up under the same ID")

            folder, child = item("folder"), item("folder/sub/child.txt")
            broker.ok(op="pin", id=folder["id"], pinned=True)
            folder = current(folder["id"])
            result = broker.ok(**modify(folder, name="moved-folder", parent_id=item("target")["id"]))["item"]
            assert result["id"] == folder["id"]
            renamed_child = item("target/moved-folder/sub/child.txt")
            assert renamed_child["id"] == child["id"] and renamed_child["pinned"]
            assert (remote / "target/moved-folder/sub/child.txt").is_file()
            passed("directory moves preserve descendant IDs and inherited pins while updating all canonical paths")

            stale, stale_child = item("stale-parent/stale"), item("stale-parent/stale/sub")
            old_request = modify(stale, name="not-published", parent_id=item("target")["id"])
            old_request["base_content_version"] = "stale-system-snapshot"
            before = len(NamespaceDav.requests_seen)
            assert not broker.rpc(**old_request)["ok"]
            conflict = next(m for m in mutations() if m["request"].get("id") == stale["id"])
            assert conflict["state"] == "conflict" and conflict["error"] == "version_conflict: item changed since the system snapshot"
            projected = current(stale["id"])
            assert projected["name"] == "not-published" and projected["parent_id"] == old_request["parent_id"]
            anchor = broker.ok(op="changes", anchor="current")["anchor"]
            broker.stop()
            resolved = broker.cli("smart", "refresh-conflict", broker.domain, conflict["id"])
            assert resolved["mutation"]["state"] == "cancelled" and resolved["remote_changed"] is False
            assert resolved["directory_present"] and resolved["directory_path"] == "stale-parent/stale"
            assert resolved["refreshed_parent_path"] == "stale-parent"
            retry = broker.cli("smart", "retry", broker.domain, conflict["id"], success=False)
            assert "use the refreshed Finder item" in retry.stderr
            cancelled = broker.cli("smart", "cancel", broker.domain, conflict["id"])
            assert cancelled["error"] == resolved["mutation"]["error"]
            broker.start()
            refreshed = current(stale["id"])
            assert refreshed["name"] == "stale" and refreshed["parent_id"] == item("stale-parent")["id"]
            assert refreshed["sync_state"] != "conflict" and not refreshed["has_unsynced_changes"]
            assert item("stale-parent/stale/sub")["id"] == stale_child["id"]
            assert any(row["id"] == stale["id"] and row["name"] == "stale"
                       for row in broker.ok(op="changes", anchor=anchor)["items"])
            assert not broker.rpc(**old_request)["ok"]
            assert next(m for m in mutations() if m["id"] == conflict["id"])["state"] == "cancelled"
            assert (remote / "stale-parent/stale/sub").is_dir() and not (remote / "target/not-published").exists()
            assert not any(method in ("MOVE", "DELETE", "PUT", "MKCOL") for method, _ in NamespaceDav.requests_seen[before:])
            passed("refresh-conflict restores the canonical Finder location and never replays an abandoned stale rename")

            collision = item("files/collision.txt")
            response = broker.rpc(**modify(collision, name="occupied.txt", parent_id=item("target")["id"]))
            assert not response["ok"] and (remote / "target/occupied.txt").read_bytes() == b"original target/occupied.txt"
            assert (remote / "files/collision.txt").is_file()
            cancel_conflicts()
            passed("occupied destinations are never overwritten and both original files remain")

            collision = current(collision["id"])
            NamespaceDav.before_move = lambda source, destination: destination.write_bytes(b"external destination race")
            response = broker.rpc(**modify(collision, name="late-occupied.txt", parent_id=item("target")["id"]))
            assert not response["ok"] and (remote / "target/late-occupied.txt").read_bytes() == b"external destination race"
            assert (remote / "files/collision.txt").is_file()
            cancel_conflicts()
            passed("a destination created after planning wins the server no-overwrite guard")

            item("race/sub/child.txt")
            race = current(item("race")["id"])
            NamespaceDav.before_move = lambda source, destination: (source / "sub/late.txt").write_bytes(b"external late child")
            response = broker.rpc(**remove(race))
            assert not response["ok"], response
            assert (remote / "race/sub/late.txt").read_bytes() == b"external late child"
            assert (remote / "race/sub/child.txt").is_file()
            assert not any(method == "DELETE" and path.startswith("/dav/race") for method, path in NamespaceDav.requests_seen)
            cancel_conflicts()
            passed("a new nested child racing directory deletion causes safe restore/conflict without recursive DELETE")

            erased, erased_child = item("erase"), item("erase/sub/child.txt")
            response = broker.ok(**remove(erased))
            assert response["recoverable"] and not response["space_reclaimed"]
            recovery = remote / response["recovery_path"]
            assert not (remote / "erase").exists() and (recovery / "sub/child.txt").is_file()
            assert all(value["id"] not in (erased["id"], erased_child["id"]) for value in broker.ok(op="list", parent_id="working_set")["items"])
            passed("recursive deletion removes the visible subtree and retains a durable recoverable tombstone")

            offline = item("offline")
            broker.cli("smart", "pause")
            queued = broker.rpc(**modify(offline, name="offline-renamed", parent_id=item("target")["id"]))
            assert not queued["ok"]
            assert (remote / "offline").is_dir() and not (remote / "target/offline-renamed").exists()
            projected = broker.ok(op="list", parent_id=item("target")["id"])["items"]
            assert any(value["id"] == offline["id"] and value["name"] == "offline-renamed" for value in projected)
            broker.stop()
            NamespaceDav.online = False
            broker.start()
            broker.cli("smart", "resume")
            wait(lambda: any(m["state"] == "interrupted" and m["request"].get("id") == offline["id"] for m in mutations()), "offline move retained after restart", 20)
            assert (remote / "offline/sub/child.txt").is_file()
            NamespaceDav.online = True
            wait(lambda: (remote / "target/offline-renamed/sub/child.txt").is_file(), "queued move replay", 40)
            assert item("target/offline-renamed")["id"] == offline["id"]
            passed("queued moves survive restart and real HTTP outage, preserve their projection and replay on reconnect")

            def blocked_by_filter(request, pattern, source):
                broker.cli("smart", "pause")
                assert not broker.rpc(**request)["ok"]
                broker.cli("filter", "config", "--exclude", pattern)
                before = len(NamespaceDav.requests_seen)
                broker.cli("smart", "resume")
                wait(lambda: any(m["request"].get("id") == request["id"] and m["state"] == "interrupted" and "filters exclude" in (m.get("error") or "") for m in mutations()), "filtered mutation retained", 20)
                assert source.exists()
                assert not any(method in ("MOVE", "DELETE", "PUT") for method, path in NamespaceDav.requests_seen[before:] if path.startswith("/dav/" + source.relative_to(remote).as_posix()))
                broker.stop()
                broker.start()
                assert source.exists()
                broker.cli("filter", "config", "--clear-exclude")
                wait(lambda: not source.exists(), "mutation replay after filter cleared", 35)

            filtered_delete = item("filter-delete")
            item("filter-delete/sub/child.txt")
            blocked_by_filter(remove(filtered_delete), "filter-delete/sub/child.txt", remote / "filter-delete")
            recoveries = broker.cli("smart", "recovery", broker.domain)["recoveries"]
            recovery = next(value for value in recoveries if value["source"] == "filter-delete")
            assert (remote / recovery["recovery_path"] / "sub/child.txt").is_file()
            filtered_move = item("filter-move")
            item("filter-move/sub/child.txt")
            blocked_by_filter(modify(filtered_move, name="filter-moved"), "filter-move/sub/child.txt", remote / "filter-move")
            assert item("filter-moved")["id"] == filtered_move["id"]
            filtered_file = item("filter-file.txt")
            blocked_by_filter(remove(filtered_file), "filter-file.txt", remote / "filter-file.txt")
            passed("new exclusion rules suspend queued file deletion and whole-directory move/delete, survive restart, and replay safely after rules clear")

            background = item("filter-background")
            background_child = item("filter-background/sub/child.txt")
            broker.ok(op="fetch", id=background_child["id"], base_content_version=background_child["content_version"])
            broker.cli("filter", "config", "--exclude", "filter-background/")
            broker.cli("index", "config", "--refresh-seconds", "1", "--active-refresh-seconds", "1")
            before = len(NamespaceDav.requests_seen)
            broker.cli("index", "refresh", broker.domain, "filter-background", "--recursive")
            root_before = sum(method == "PROPFIND" and path.rstrip("/") == "/dav" for method, path in NamespaceDav.requests_seen)
            wait(lambda: sum(method == "PROPFIND" and path.rstrip("/") == "/dav" for method, path in NamespaceDav.requests_seen) > root_before, "allowed background scheduler turn", 20)
            assert not broker.rpc(op="fetch", id=background_child["id"], base_content_version=background_child["content_version"])["ok"]
            assert not any(path.startswith("/dav/filter-background/") for method, path in NamespaceDav.requests_seen[before:])
            assert background["id"] in read_provider_journal(broker.home, broker.domain)["items"]
            broker.cli("filter", "config", "--clear-exclude")
            passed("excluded directories and files keep canonical rows while background traversal and cached-content fetch stop")

            ordinary_domain = broker.domain
            broker.cli("mkdir", "fixture", "managed")
            broker.cli("repo", "init", "fixture", "managed")
            transaction = broker.cli("repo", "begin", "fixture", "managed")["id"]
            broker.cli("repo", "mkdir", transaction, "protected")
            broker.cli("repo", "put", transaction, str(staged), "protected/kept.txt")
            broker.cli("repo", "commit", transaction)
            broker.domain = "namespace-managed"
            broker.cli("smart", "setup", broker.domain, "fixture", "managed", "--repository")
            managed_directory = item("protected")
            broker.cli("smart", "pause")
            assert not broker.rpc(**remove(managed_directory))["ok"]
            transaction = broker.cli("repo", "begin", "fixture", "managed")["id"]
            broker.cli("repo", "put", transaction, str(staged), "protected/excluded.txt")
            broker.cli("repo", "commit", transaction)
            broker.cli("filter", "config", "--exclude", "protected/excluded.txt")
            broker.cli("smart", "resume")
            wait(lambda: any(m["request"].get("id") == managed_directory["id"] and m["state"] == "interrupted" and "filters exclude" in (m.get("error") or "") for m in mutations()), "fresh managed manifest exclusion", 25)
            snapshot = broker.cli("repo", "ls", "fixture", "managed")
            assert "protected/excluded.txt" in snapshot["commit"]["entries"] and "protected/kept.txt" in snapshot["commit"]["entries"]
            broker.cli("filter", "config", "--clear-exclude")
            wait(lambda: any(m["request"].get("id") == managed_directory["id"] and m["state"] == "done" for m in mutations()), "managed delete after filter cleared", 25)
            assert "protected" not in broker.cli("repo", "ls", "fixture", "managed")["commit"]["entries"]
            broker.domain = ordinary_domain
            passed("managed recursive deletion rechecks the fresh manifest and protects an excluded child added after enqueue")

            crash = item("crash/doc.txt")
            gate = dict(path="/dav/target/crash-moved.txt", entered=threading.Event(), release=threading.Event())
            NamespaceDav.move_gate = gate
            request = modify(crash, name="crash-moved.txt", parent_id=item("target")["id"])
            with concurrent.futures.ThreadPoolExecutor() as executor:
                future = executor.submit(broker.rpc, **request)
                assert gate["entered"].wait(25), "guarded MOVE was not reached"
                broker.process.send_signal(signal.SIGKILL)
                broker.process.wait(timeout=10)
                broker.process = None
                gate["release"].set()
                try:
                    future.result(timeout=10)
                except (AssertionError, OSError):
                    pass
            (remote / "crash/doc.txt").write_bytes(b"external replacement after crash")
            broker.start()
            wait(lambda: any(m["state"] == "done" and m["request"].get("id") == crash["id"] for m in mutations()), "lost MOVE acknowledgement reconciliation", 45)
            assert item("target/crash-moved.txt")["id"] == crash["id"]
            assert (remote / "crash/doc.txt").read_bytes() == b"external replacement after crash"
            moves = [source for source, destination in NamespaceDav.checked_moves if urlsplit(destination).path == "/dav/target/crash-moved.txt"]
            assert len(moves) == 1, moves
            passed("SIGKILL after server MOVE reconciles the original identity without repeating MOVE or touching a replacement")
        finally:
            if NamespaceDav.move_gate:
                NamespaceDav.move_gate["release"].set()
            broker.close()
            server.shutdown()
            server.server_close()
    with binary.open("rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    report = dict(binary=report_binary_path(binary), binary_sha256=digest, version=version, passed=len(checks), checks=checks,
                  scope="temporary synthetic authenticated loopback DAV only; no real storage", limitations=["Deleted directories are retained in remote recovery tombstones; storage is not immediately reclaimed.", "DAV guarded moves require strong source ETags and compliant tagged If / Overwrite enforcement."])
    build = ".debug" if binary.parent.name == "debug" else ""
    (ROOT / "docs" / f"NAMESPACE_VALIDATION.{version.removeprefix('rdrive ')}{build}.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    run(Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/rdrive").resolve())
