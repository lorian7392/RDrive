#!/usr/bin/env python3
"""SQLite provider-index black-box regressions and measured query comparison.

All state, HTTP service data and legacy migration inputs are synthetic and live
in temporary directories. This module also supplies read-only SQLite diagnostic
helpers for the provider receipt and native workflow tests.
"""
import argparse
from collections import Counter
import concurrent.futures
from contextlib import closing
import hashlib
import http.server
import json
import os
from pathlib import Path
import posixpath
import signal
import socket
import sqlite3
import statistics
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit, urlunsplit
import uuid

from cli_workflows import Dav

ROOT = Path(__file__).resolve().parents[1]


def report_binary_path(binary):
    """Keep repository builds portable while identifying external executables."""
    resolved = Path(binary).resolve()
    try:
        return resolved.relative_to(ROOT).as_posix()
    except ValueError:
        return str(resolved)


def state_key(value):
    return hashlib.sha256(value.encode()).hexdigest()


def provider_database(home, domain):
    return Path(home) / "provider" / (state_key(domain) + ".sqlite3")


def sqlite_read(path):
    connection = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True, timeout=2)
    connection.execute("PRAGMA query_only=ON")
    return connection


def provider_journal_from_db(path):
    """Read one consistent authoritative SQL snapshot, never legacy JSON."""
    with closing(sqlite_read(path)) as connection:
        connection.execute("BEGIN")
        row = connection.execute("SELECT binding, anchor, control_json, pins_json FROM meta WHERE id=1").fetchone()
        assert row is not None, "provider SQLite metadata is missing"
        journal = json.loads(row[2])
        journal["binding"] = row[0]
        journal["anchor"] = row[1]
        journal["pins"] = json.loads(row[3])
        journal["items"] = {item[0]: json.loads(item[1]) for item in connection.execute("SELECT id, record_json FROM items")}
        journal["events"] = [dict(anchor=anchor, item=json.loads(item) if item is not None else None, deleted=deleted)
                             for anchor, item, deleted in connection.execute("SELECT anchor, item_json, deleted FROM events ORDER BY anchor")]
        journal["directory_scans"] = dict(connection.execute("SELECT id, scanned_at FROM directory_scans"))
        connection.commit()
        return journal


def read_provider_journal(home, domain):
    return provider_journal_from_db(provider_database(home, domain))


def sqlite_items(home, domain):
    with closing(sqlite_read(provider_database(home, domain))) as connection:
        return {path: json.loads(item) for path, item in connection.execute("SELECT path, item_json FROM items")}


class IndexedDav(Dav):
    online = True
    directory_requests = Counter()
    failed = set()
    gates = {}
    guard = threading.Lock()

    def resource(self, value=None):
        parts = urlsplit(value or self.path)
        path = posixpath.normpath(parts.path)
        if path == "/dav":
            path += "/"
        return super().resource(urlunsplit((parts.scheme, parts.netloc, path, parts.query, parts.fragment)))

    def handle_dav(self):
        path = self.path.rstrip("/")
        gate = None
        if self.command == "PROPFIND" and self.headers.get("Depth") == "1":
            with self.guard:
                self.directory_requests[path] += 1
                gate = self.gates.pop(path, None)
            if gate is not None:
                gate["entered"].set()
                if not gate["release"].wait(25):
                    raise AssertionError("fixture metadata gate timed out")
        if not self.online or path in self.failed:
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


class Broker:
    def __init__(self, binary, root, url, domain="indexed", remote=""):
        self.binary = binary
        self.root = root
        self.home = root / "state"
        self.ipc = root / "ipc"
        self.ipc.mkdir(mode=0o700)
        self.stage = self.ipc / "Transfers"
        self.domain = domain
        self.remote = remote
        self.url = url
        self.env = dict(os.environ, RDRIVE_INDEX_PASSWORD="fixture-password")
        self.env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        self.base = [str(binary), "--home", str(self.home), "--json", "--progress", "off"]
        self.process = None
        self.log = (root / "broker.log").open("w+")

    def cli(self, *args, success=True):
        deadline = time.monotonic() + 45
        while True:
            result = subprocess.run(self.base + list(map(str, args)), env=self.env, capture_output=True, text=True, timeout=60)
            if not success:
                assert result.returncode != 0, (args, result.stdout)
                return result
            if result.returncode == 0:
                return json.loads(result.stdout)
            if not any(message in result.stderr for message in ("another rdrive process", "already locked", "state directory is in use")):
                raise AssertionError((args, result.stderr, result.stdout))
            assert time.monotonic() < deadline, result.stderr
            time.sleep(.1)

    def configure(self):
        self.cli("profile", "add", "fixture", "--protocol", "webdav", "--url", self.url,
                 "--allow-http", "--username", "user", "--password-env", "RDRIVE_INDEX_PASSWORD", "--timeout", "30")
        self.cli("smart", "setup", self.domain, "fixture", self.remote)

    def rpc(self, **message):
        descriptor = json.loads((self.ipc / "bridge-endpoint.json").read_text())
        message.update(v=1, auth_token=descriptor["token"])
        message.setdefault("domain", self.domain)
        message.setdefault("request_id", uuid.uuid4().hex)
        with socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=40) as client:
            client.sendall(json.dumps(message).encode() + b"\n")
            with client.makefile("rb") as reader:
                while True:
                    line = reader.readline()
                    assert line, "provider disconnected"
                    value = json.loads(line)
                    if "progress" not in value:
                        return value

    def ok(self, **message):
        value = self.rpc(**message)
        assert value.get("ok"), value
        return value

    def start(self):
        self.process = subprocess.Popen(self.base + ["provider", "serve", "--socket", str(self.ipc / "bridge.sock"),
                                                   "--staging", str(self.stage), "--development-tcp"],
                                        env=self.env, stdout=subprocess.DEVNULL, stderr=self.log)
        def ready():
            assert self.process.poll() is None, "provider exited during startup: " + (self.root / "broker.log").read_text()
            try:
                return self.rpc(op="domains").get("ok")
            except (OSError, json.JSONDecodeError, AssertionError):
                return False
        wait(ready, "provider startup", 15)

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            self.process.wait(timeout=15)
        self.process = None

    def close(self):
        self.stop()
        self.log.close()


def wait(check, label, seconds=75):
    deadline = time.monotonic() + seconds
    while True:
        try:
            result = check()
        except sqlite3.OperationalError as error:
            if "unable to open database" not in str(error) and "no such table" not in str(error):
                raise
            result = None
        if result:
            return result
        assert time.monotonic() < deadline, label
        time.sleep(.2)


def legacy_journal(domain, remote, url, buckets, files_per_bucket, *, migration_receipt=False):
    binding_input = json.dumps([domain, "fixture", "webdav", url, "user", remote, False], separators=(",", ":"), ensure_ascii=False)
    binding = state_key(binding_input + ":smart-v1")
    records = {}
    for bucket in range(buckets):
        parent_id = f"bucket-{bucket:03}"
        folder = f"bucket-{bucket:03}"
        for name, item_id, parent, directory, size in [(folder, parent_id, "root", True, 0)] + [
                (f"{folder}/file-{number:05}.txt", f"file-{bucket:03}-{number:05}", parent_id, False, 1)
                for number in range(files_per_bucket)]:
            entry = dict(path=f"{remote}/{name}" if remote else name, is_dir=directory, size=size,
                         modified=None, etag=None if directory else '"' + state_key(item_id) + '"')
            item = dict(id=item_id, parent_id=parent, name=name.rsplit("/", 1)[-1], kind="directory" if directory else "file",
                        size=size, modified=None, content_version=state_key(json.dumps([directory, size, entry["etag"], None], separators=(",", ":"))),
                        metadata_version=state_key(json.dumps([name, None, {}, False], separators=(",", ":"))), pinned=False,
                        capabilities=["read", "add"] if directory else ["read", "write", "delete"], tag_data_hex=None,
                        extended_attributes={}, smart=True, sync_state="online", sync_message=None,
                        has_unsynced_changes=False, native_materialized=False)
            records[item_id] = dict(path=name, item=item, entry=entry, strong=None if directory else entry,
                                    client_id=None, metadata_known=False, metadata_checked=0)
    journal = dict(binding=binding, items=records, anchor=0, events=[], requests={}, pins=[], pins_migrated=True,
                   native_errors={}, smart_replays={}, directory_scans={}, directory_attempts={}, metadata_attempts={}, directory_turn=0)
    if migration_receipt:
        first = records["file-000-00000"]
        journal["pins"] = ["bucket-000"]
        key = state_key(domain) + ":saved-fixture-key"
        journal["requests"][key] = dict(fingerprint=state_key("saved fixture request"), metadata=None, source_fingerprint=None,
                                       transaction=None, confirmed=first["entry"], response={"item": first["item"]},
                                       deletion_path=None, deletion_expected=None, metadata_base_fingerprint=None)
        journal["anchor"] = 2
        journal["events"] = [dict(anchor=1, item=first["item"], deleted=None), dict(anchor=2, item=None, deleted="legacy-deleted")]
    return journal


def save_legacy(broker, journal):
    provider = broker.home / "provider"
    provider.mkdir(mode=0o700, exist_ok=True)
    path = provider / (state_key(broker.domain) + ".json")
    payload = json.dumps(journal, separators=(",", ":"), ensure_ascii=False).encode()
    path.write_bytes(payload)
    path.chmod(0o600)
    return path, payload


def measure(function, arguments, rounds=30):
    elapsed = []
    for index in range(rounds):
        start = time.perf_counter_ns()
        function(arguments[index % len(arguments)])
        elapsed.append((time.perf_counter_ns() - start) / 1_000_000)
    return dict(samples=rounds, median_ms=statistics.median(elapsed), p95_ms=sorted(elapsed)[max(0, int(.95 * len(elapsed)) - 1)], min_ms=min(elapsed))


def run(binary, output=None):
    checks = []
    metrics = {}
    def passed(name):
        checks.append(name)
        print("PASS " + name, flush=True)
    # The provider also owns a Unix socket, even when the fixture uses its
    # authenticated TCP bridge. macOS's default temp parent can exceed SUN_LEN.
    short_tmp = "/private/tmp" if Path("/private/tmp").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="rdrive-index-", dir=short_tmp) as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        for directory in ("folder/nested", "slow/deep", "sibling"):
            (remote / directory).mkdir(parents=True, exist_ok=True)
        for path, data in {"fast.txt": b"fast", "folder/stable.txt": b"stable A", "folder/remove.txt": b"remove",
                           "folder/nested/kept.txt": b"preserve", "slow/deep/leaf.txt": b"deep", "sibling/other.txt": b"other"}.items():
            (remote / path).write_bytes(data)
        expected = {path.relative_to(remote).as_posix() for path in remote.rglob("*")}
        IndexedDav.root = remote
        IndexedDav.requests_seen = []
        IndexedDav.directory_requests = Counter()
        IndexedDav.failed = set()
        IndexedDav.online = True
        gate = dict(entered=threading.Event(), release=threading.Event())
        IndexedDav.gates = {"/dav/slow": gate}
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), IndexedDav)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        brokers = []
        def broker(name, domain="indexed", remote_path=""):
            location = root / name
            location.mkdir()
            value = Broker(binary, location, f"http://127.0.0.1:{server.server_port}/dav/", domain, remote_path)
            brokers.append(value)
            return value
        try:
            main = broker("main")
            main.configure()
            defaults = json.loads((main.home / "config.json").read_text())["index"]
            assert defaults == dict(enabled=True, prefetch_depth=None, refresh_secs=300, active_refresh_secs=30), defaults
            main.cli("index", "config", "--disable")
            assert not json.loads((main.home / "config.json").read_text())["index"]["enabled"]
            main.cli("index", "config", "--enable", "--depth", "1")
            changed_config = json.loads((main.home / "config.json").read_text())["index"]
            assert changed_config["enabled"] and changed_config["prefetch_depth"] == 1
            main.cli("index", "config", "--all")
            main.start()
            def root_listing():
                rows = main.ok(op="list", parent_id="root")["items"]
                return rows if any(item["name"] == "fast.txt" for item in rows) else None
            root_rows = wait(root_listing, "root publication before deep indexing", 8)
            assert gate["entered"].wait(30), "background preindex did not traverse an unopened child"
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                start = time.monotonic()
                current = pool.submit(main.ok, op="list", parent_id="root").result(timeout=2)
                metrics["blocked_network_root_query_ms"] = (time.monotonic() - start) * 1000
            assert current["items"] and not gate["release"].is_set()
            with closing(sqlite_read(provider_database(main.home, main.domain))) as connection:
                assert connection.execute("SELECT COUNT(*) FROM items WHERE parent_id='root'").fetchone()[0] == len(root_rows)
            gate["release"].set()
            passed("root publishes before a blocked deep scan and concurrent SQLite-backed reads remain available")

            def full_index():
                rows = sqlite_items(main.home, main.domain)
                return rows if expected <= set(rows) else None
            indexed = wait(full_index, "progressive background full-tree metadata indexing", 90)
            assert {"/dav/folder/nested", "/dav/slow/deep", "/dav/sibling"} <= set(IndexedDav.directory_requests)
            assert not [path for method, path in IndexedDav.requests_seen if method == "GET"], "preindex fetched user contents"
            metrics["index_status"] = main.cli("index", "status", main.domain)
            for domain_status in metrics["index_status"]["domains"]:
                domain_status["database"] = Path(domain_status["database"]).relative_to(root).as_posix()
            passed("default background preindex discovers unopened children and deep descendants without content GET")

            main.cli("index", "config", "--refresh-seconds", "120", "--active-refresh-seconds", "60")
            folder = indexed["folder"]
            before = IndexedDav.directory_requests.copy()
            deadline = time.monotonic() + 6.5
            while time.monotonic() < deadline:
                assert main.ok(op="list", parent_id=folder["id"])["items"]
                time.sleep(.25)
            assert IndexedDav.directory_requests["/dav/folder"] == before["/dav/folder"], "fresh active scope was listed again before its TTL"
            passed("refresh TTL avoids repeatedly listing a fresh directory despite repeated local queries")

            anchor = main.ok(op="changes", anchor="current")["anchor"]
            old = sqlite_items(main.home, main.domain)
            (remote / "folder/stable.txt").write_bytes(b"stable B changed")
            (remote / "folder/remove.txt").unlink()
            (remote / "folder/new.txt").write_bytes(b"new")
            (remote / "sibling/unrequested.txt").write_bytes(b"unrelated scope")
            main.cli("index", "refresh", main.domain, "folder")
            def changed():
                rows = sqlite_items(main.home, main.domain)
                return rows if "folder/new.txt" in rows and "folder/remove.txt" not in rows and rows["folder/stable.txt"]["content_version"] != old["folder/stable.txt"]["content_version"] else None
            changed_rows = wait(changed, "manual targeted incremental refresh", 30)
            assert changed_rows["folder/stable.txt"]["id"] == old["folder/stable.txt"]["id"]
            assert changed_rows["folder/nested/kept.txt"]["id"] == old["folder/nested/kept.txt"]["id"]
            assert changed_rows["sibling/other.txt"]["id"] == old["sibling/other.txt"]["id"]
            assert "sibling/unrequested.txt" not in changed_rows
            assert old["folder/remove.txt"]["id"] in main.ok(op="changes", anchor=anchor)["deleted"]
            passed("manual targeted refresh applies create/delete/modify atomically and preserves stable sibling identities")

            kept_id = changed_rows["folder/nested/kept.txt"]["id"]
            IndexedDav.failed.add("/dav/folder/nested")
            failures_before = IndexedDav.directory_requests["/dav/folder/nested"]
            (remote / "folder/nested/kept.txt").unlink()
            main.cli("index", "refresh", main.domain, "folder/nested")
            wait(lambda: IndexedDav.directory_requests["/dav/folder/nested"] > failures_before, "errored directory scan", 25)
            assert sqlite_items(main.home, main.domain)["folder/nested/kept.txt"]["id"] == kept_id
            IndexedDav.failed.remove("/dav/folder/nested")
            main.cli("index", "refresh", main.domain, "folder/nested")
            wait(lambda: "folder/nested/kept.txt" not in sqlite_items(main.home, main.domain), "successful empty directory refresh", 30)
            assert sqlite_items(main.home, main.domain)["sibling/other.txt"]["id"] == old["sibling/other.txt"]["id"]
            passed("errored listings preserve known descendants; only a later successful empty scope removes them")

            main.cli("index", "config", "--depth", "0")
            (remote / "manual-recursive/one").mkdir(parents=True)
            (remote / "manual-recursive/one/leaf.txt").write_bytes(b"manual subtree")
            main.cli("index", "refresh", main.domain)
            wait(lambda: "manual-recursive" in sqlite_items(main.home, main.domain), "depth-zero root update", 25)
            time.sleep(3.5)
            assert "/dav/manual-recursive" not in IndexedDav.directory_requests, "depth-zero preindex entered a child"
            main.cli("index", "refresh", main.domain, "manual-recursive", "--recursive")
            wait(lambda: "manual-recursive/one/leaf.txt" in sqlite_items(main.home, main.domain), "manual recursive subtree refresh", 45)
            manual_id = sqlite_items(main.home, main.domain)["manual-recursive"]["id"]
            wait(lambda: manual_id in read_provider_journal(main.home, main.domain).get("completed_demands", {}), "durable completion of one-shot recursive demand", 15)
            main.cli("index", "config", "--active-refresh-seconds", "1")
            (remote / "manual-recursive/late/deep").mkdir(parents=True)
            (remote / "manual-recursive/late/deep/future.txt").write_bytes(b"future subtree")
            def future_directory():
                main.ok(op="list", parent_id=manual_id)
                return "manual-recursive/late" in sqlite_items(main.home, main.domain)
            wait(future_directory, "ordinary parent browse discovers future child", 20)
            time.sleep(3.5)
            assert "/dav/manual-recursive/late" not in IndexedDav.directory_requests, "a completed one-shot hint became a permanent recursive watch"
            assert "manual-recursive/late/deep/future.txt" not in sqlite_items(main.home, main.domain)
            main.cli("index", "config", "--active-refresh-seconds", "60")
            main.cli("index", "config", "--all")
            assert json.loads((main.home / "config.json").read_text())["index"]["prefetch_depth"] is None
            passed("depth limits hold while manual recursive refresh crosses its subtree once and does not follow future children")

            snapshot = sqlite_items(main.home, main.domain)
            main.stop()
            IndexedDav.online = False
            main.start()
            restart_rows = main.ok(op="list", parent_id=folder["id"])["items"]
            assert {item["id"] for item in restart_rows} == {item["id"] for path, item in snapshot.items() if item["parent_id"] == folder["id"]}
            assert main.ok(op="item", id=snapshot["slow/deep/leaf.txt"]["id"])["item"]["id"] == snapshot["slow/deep/leaf.txt"]["id"]
            main.stop()
            IndexedDav.online = True
            passed("process restart reuses persisted SQLite metadata while the service is offline")

            migration = broker("migration", "legacy", "legacy")
            migration.configure()
            migration.cli("index", "config", "--disable")
            migration.cli("smart", "pause")
            legacy = legacy_journal(migration.domain, migration.remote, migration.url, 1, 2, migration_receipt=True)
            legacy_path, legacy_bytes = save_legacy(migration, legacy)
            migration.start()
            wait(lambda: provider_database(migration.home, migration.domain).exists(), "legacy migration")
            migrated = read_provider_journal(migration.home, migration.domain)
            assert migrated["binding"] == legacy["binding"]
            assert set(migrated["requests"]) == set(legacy["requests"])
            for identity, receipt in legacy["requests"].items():
                actual=migrated["requests"][identity]
                assert all(actual.get(key)==value for key,value in receipt.items())
                assert all(value is None for key,value in actual.items() if key not in receipt), "migration changed retained receipt semantics"
            assert set(migrated["pins"]) == set(legacy["pins"])
            assert any(event["deleted"] == "legacy-deleted" for event in migrated["events"])
            assert legacy_path.read_bytes() == legacy_bytes, "migration modified its retained legacy evidence"
            legacy_path.write_bytes(b"{deliberately invalid retained legacy journal")
            assert len(migration.ok(op="list", parent_id="bucket-000")["items"]) == 2
            migration.stop()
            migration.start()
            assert len(migration.ok(op="list", parent_id="bucket-000")["items"]) == 2
            passed("atomic legacy migration retains receipts/pins/events and SQLite stays authoritative after legacy JSON corruption")

            migration.stop()
            config_path = migration.home / "config.json"
            config = json.loads(config_path.read_text())
            config["profiles"]["fixture"]["url"] += "changed-root/"
            config_path.write_text(json.dumps(config))
            migration.start()
            rebound = migration.rpc(op="list", parent_id="root")
            assert not rebound["ok"] and rebound["error"]["code"] == "version_conflict", rebound
            assert read_provider_journal(migration.home, migration.domain)["binding"] == legacy["binding"]
            unsafe = migration.cli("index", "refresh", migration.domain, "../escape", success=False)
            assert "traversal" in unsafe.stderr, unsafe.stderr
            migration.stop()
            passed("backend binding changes and unsafe refresh paths fail closed without reusing another scope")

            perf = broker("performance", "performance", "performance")
            perf.configure()
            perf.cli("index", "config", "--disable")
            perf.cli("smart", "pause")
            large = legacy_journal(perf.domain, perf.remote, perf.url, 100, 100)
            large_path, large_bytes = save_legacy(perf, large)
            perf.start()
            wait(lambda: provider_database(perf.home, perf.domain).exists(), "10k SQLite migration", 30)
            assert len(sqlite_items(perf.home, perf.domain)) == 10100
            large_path.write_bytes(b"{retained JSON is not a query source")
            # Corrupt only an unrelated full Record projection, keeping its
            # public Item projection intact. A full-Journal load would now fail;
            # an actual indexed folder query must not deserialize that record.
            with sqlite3.connect(provider_database(perf.home, perf.domain)) as connection:
                connection.execute("UPDATE items SET record_json=? WHERE id=?", ("{invalid unrelated record", "file-099-00099"))
            assert len(perf.ok(op="list", parent_id="bucket-000")["items"]) == 100
            parents = [f"bucket-{number:03}" for number in range(100)]
            with closing(sqlite_read(provider_database(perf.home, perf.domain))) as connection:
                plan = [row[3] for row in connection.execute("EXPLAIN QUERY PLAN SELECT item_json FROM items WHERE parent_id=? ORDER BY id", (parents[0],))]
                assert any("items_parent_id" in row for row in plan), plan
                def sql_query(parent):
                    return [json.loads(row[0]) for row in connection.execute("SELECT item_json FROM items WHERE parent_id=? ORDER BY id", (parent,))]
                def legacy_query(parent):
                    return [record["item"] for record in json.loads(large_bytes)["items"].values() if record["item"]["parent_id"] == parent]
                for parent in parents[:3]:
                    assert {item["id"] for item in sql_query(parent)} == {item["id"] for item in legacy_query(parent)}
                metrics["folder_queries"] = dict(records=10100, children_per_folder=100, legacy_bytes=len(large_bytes),
                    method="Warm actual SQLite child query plus Item JSON decoding versus json.loads of retained legacy bytes plus parent filtering; actual provider socket RPC measured separately. These are local fixture measurements, not a before/after Rust benchmark.",
                    sqlite_query_decode=measure(sql_query, parents), legacy_full_parse_scan=measure(legacy_query, parents),
                    provider_rpc=measure(lambda parent: perf.ok(op="list", parent_id=parent)["items"], parents), query_plan=plan)
            metrics["folder_queries"]["observed_parse_to_sql_median_ratio"] = metrics["folder_queries"]["legacy_full_parse_scan"]["median_ms"] / metrics["folder_queries"]["sqlite_query_decode"]["median_ms"]
            passed("10,100-row folder queries use the parent index despite an unrelated corrupt Record; SQL/RPC and JSON timings have no hard speed threshold")
        finally:
            gate["release"].set()
            for value in brokers:
                value.close()
            server.shutdown()
            server.server_close()
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    with binary.open("rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    report = dict(binary=report_binary_path(binary), version=version, binary_sha256=digest, checks=checks, passed=len(checks), metrics=metrics,
                  path_format=dict(binary="repository-relative when inside the repository; otherwise absolute",
                                   index_database="relative to the removed temporary fixture root"),
                  scope="synthetic loopback WebDAV, temporary SQLite/legacy state, deterministic 10,100-row comparison; no real storage")
    output = output or ROOT / "docs" / ("INDEX_VALIDATION.json" if binary.parent.name == "release" else "INDEX_VALIDATION.debug.json")
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", type=Path, default=ROOT / "target/release/rdrive")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    run(arguments.binary.resolve(), arguments.output)
