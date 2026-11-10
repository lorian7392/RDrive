#!/usr/bin/env python3
"""Real CLI/MCP cache-directory and clear workflows with synthetic state only.

Uses an authenticated loopback DAV fixture and private temporary directories.
No configured storage account, native mount, application, or runtime is used.
Run: python3 tests/cache_directory_workflows.py [path/to/rdrive] [--report path.json]
"""
import argparse
import datetime
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav
from mcp_workflows import Client


REPOSITORY = Path(__file__).resolve().parents[1]


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def payloads(directory):
    """Record names and bytes, so previews cannot quietly copy or delete data."""
    return {path.name: sha256(path) for path in directory.iterdir()
            if path.is_file() and path.name != ".rdrive-cache.json"}


def private(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.chmod(0o700)
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", type=Path, default=Path("target/debug/rdrive"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    binary_hash = sha256(binary)
    passed = []
    skipped = []

    with tempfile.TemporaryDirectory(prefix="rdrive-cache-directory-") as temporary:
        root = Path(temporary).resolve()
        home = root / "state"
        remote = private(root / "remote")
        contents = {
            "clean.txt": b"ordinary clean cache\n",
            "pinned.txt": b"retained offline pin\n",
            "active.txt": b"an active verified reader\n",
            "after-move.txt": b"new cache writes use the selected directory\n",
        }
        for name, content in contents.items():
            (remote / name).write_bytes(content)

        class Fixture(Dav):
            online = True
            requests_seen = []
            locks = {}
            interrupt_once = set()
            range_requests = []

            def handle_dav(self):
                if not type(self).online:
                    type(self).requests_seen.append((self.command, self.path))
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
        server.daemon_threads = True
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        env = dict(os.environ, RDRIVE_HOME=str(home), RDRIVE_TEST_PASSWORD="fixture-password")
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        clients = []
        output_counter = 0

        def cli(*arguments, ok=True, offline=False):
            actual_env = env.copy()
            if offline:
                actual_env.pop("RDRIVE_TEST_PASSWORD", None)
            result = subprocess.run(
                [str(binary), "--home", str(home), "--json", "--progress", "off", *map(str, arguments)],
                env=actual_env, capture_output=True, timeout=30,
            )
            if not ok:
                assert result.returncode != 0, f"{arguments} unexpectedly succeeded"
                return result
            assert result.returncode == 0, (arguments, result.stderr.decode(errors="replace"),
                                            result.stdout.decode(errors="replace"))
            return json.loads(result.stdout)

        def database():
            return json.loads((home / "state.json").read_text())

        def cached():
            return {item["remote"]: item for item in database()["cache"].values()}

        def durable_bytes():
            return {name: (home / name).read_bytes() for name in ("config.json", "state.json")}

        def assert_directory(directory, *, custom):
            result = cli("cache", "directory", offline=True)
            assert Path(result["directory"]) == directory, result
            assert Path(result["default_directory"]) == home / "cache", result
            assert result["custom_directory"] is custom, result
            return result

        def offline_get(name):
            nonlocal output_counter
            output_counter += 1
            destination = root / f"offline-copy-{output_counter}"
            before = len(Fixture.requests_seen)
            cli("cache", "get", "test", name, destination, offline=True)
            assert destination.read_bytes() == contents[name]
            assert len(Fixture.requests_seen) == before, "offline cache read contacted DAV"

        def new_client(*, write=False, roots=()):
            result = Client(binary, home, env, write=write, roots=roots)
            clients.append(result)
            result.initialize()
            return result

        try:
            cli("profile", "add", "test", "--protocol", "webdav", "--url",
                f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http",
                "--username", "user", "--password-env", "RDRIVE_TEST_PASSWORD")
            assert_directory(home / "cache", custom=False)
            cli("cache", "config", "--max-bytes", "4096")
            for name in ("clean.txt", "pinned.txt", "active.txt"):
                cli("cache", "fetch", "test", name, *( ["--pin"] if name == "pinned.txt" else []))
            initial = cached()
            assert len(initial) == 3
            original_payloads = payloads(home / "cache")
            passed.append("default cache directory and backward-compatible cache limit persist across CLI processes")

            queued_input = root / "queued-input.txt"
            queued_content = b"queued data is durable user content\n"
            queued_input.write_bytes(queued_content)
            job = cli("put", "test", queued_input, "queued.txt", "--queue")
            queued_input.write_bytes(b"later caller edit")
            snapshot = home / "spool" / job["snapshot"]
            # Synthetic sentinels cover the separate durable/native directory
            # scope; the upload snapshot itself was created through the CLI.
            dirty = private(home / "smart" / "synthetic-domain" / "dirty") / "retained-edit"
            dirty.write_bytes(b"retained offline edit\n")
            native = private(home / "provider" / "synthetic-native") / "materialized"
            native.write_bytes(b"OS-managed materialized content\n")
            ledger = home / "native-cache-usage.json"
            ledger.write_text(json.dumps({"version": 1, "reports": {"synthetic-native": {
                "used": len(native.read_bytes()), "reserved": 0, "pinned": 0, "updated": int(time.time()),
            }}}))
            protected = {path: path.read_bytes() for path in (snapshot, dirty, native, ledger)}

            custom = root / "custom-cache"
            before = durable_bytes()
            preview = cli("cache", "directory", custom, "--dry-run", offline=True)
            assert not preview["apply"] and preview["changed"], preview
            assert Path(preview["directory"]) == custom, preview
            assert preview["files"] == 3 and preview["bytes"] == sum(map(len, list(contents.values())[:3])), preview
            assert durable_bytes() == before and payloads(home / "cache") == original_payloads
            assert not custom.exists(), "dry-run created its destination"
            passed.append("directory dry-run reports existing payload bytes without changing configuration or creating a target")

            active_object = initial["active.txt"]["object"]
            with (home / "cache" / active_object).open("rb") as lease:
                fcntl.flock(lease, fcntl.LOCK_SH | fcntl.LOCK_NB)
                cli("cache", "directory", custom, ok=False, offline=True)
                assert durable_bytes() == before and payloads(home / "cache") == original_payloads
                assert not custom.exists(), "rejected active migration left a target directory"
            passed.append("a separate process holding a real shared payload lock prevents migration without changing source data")

            private(custom)
            Fixture.online = False
            before_requests = len(Fixture.requests_seen)
            moved = cli("cache", "directory", custom, offline=True)
            assert moved["apply"] and moved["changed"], moved
            assert payloads(custom) == original_payloads and not payloads(home / "cache")
            assert_directory(custom, custom=True)
            assert cli("cache", "status", offline=True)["max_bytes"] == 4096
            assert cached() == initial, "migration changed cache identities or pin flags"
            for name in initial:
                offline_get(name)
            assert len(Fixture.requests_seen) == before_requests, "offline migration contacted DAV"
            assert all(path.read_bytes() == data for path, data in protected.items())
            passed.append("offline directory migration physically moves all clean payloads and preserves pins, identities, budget and restart reads")

            Fixture.online = True
            cli("cache", "fetch", "test", "after-move.txt")
            after_object = cached()["after-move.txt"]["object"]
            assert (custom / after_object).read_bytes() == contents["after-move.txt"]
            assert not (home / "cache" / after_object).exists()
            Fixture.online = False
            before_requests = len(Fixture.requests_seen)
            restored = cli("cache", "directory", "--default", offline=True)
            assert restored["apply"] and restored["changed"], restored
            assert_directory(home / "cache", custom=False)
            assert not payloads(custom)
            for name in contents:
                offline_get(name)
            assert len(Fixture.requests_seen) == before_requests
            passed.append("new admissions use the selected directory; --default physically migrates back and remains readable after restart")

            nonempty = private(root / "nonempty")
            sentinel = nonempty / "keep.txt"
            sentinel.write_bytes(b"unrelated user data")
            public = private(root / "public")
            public.chmod(0o755)
            link = root / "linked-cache"
            link.symlink_to(custom, target_is_directory=True)
            regular = root / "regular-file"
            regular.write_bytes(b"not a directory")
            invalid = ("relative-cache", nonempty, public, link, regular, home,
                       home / "spool", home / "cache" / "nested")
            for destination in invalid:
                for options in ((), ("--dry-run",)):
                    before = durable_bytes()
                    before_payloads = payloads(home / "cache")
                    cli("cache", "directory", destination, *options, ok=False, offline=True)
                    assert durable_bytes() == before and payloads(home / "cache") == before_payloads
                    assert sentinel.read_bytes() == b"unrelated user data"
                    assert stat.S_IMODE(public.stat().st_mode) == 0o755
                    assert link.is_symlink() and regular.read_bytes() == b"not a directory"
            assert not (home / "cache" / "nested").exists()
            before = durable_bytes()
            cli("cache", "directory", custom, "--default", ok=False, offline=True)
            assert durable_bytes() == before
            passed.append("relative, nonempty, public, symlink, regular-file and overlapping paths are rejected for apply and dry-run without data or permission changes")

            # UUID-shaped unindexed data must survive ordinary Store recovery
            # too, rather than being mistaken for an abandoned download.
            unmanaged = home / "cache" / str(uuid.uuid4())
            unmanaged.write_bytes(b"unindexed data must never be cleared")
            before = durable_bytes()
            before_payloads = payloads(home / "cache")
            with (home / "cache" / active_object).open("rb") as lease:
                fcntl.flock(lease, fcntl.LOCK_SH | fcntl.LOCK_NB)
                preview = cli("cache", "clear", offline=True)
                expected_bytes = len(contents["clean.txt"]) + len(contents["after-move.txt"])
                assert not preview["apply"] and preview["evicted"] == 0 and preview["freed_bytes"] == 0, preview
                assert preview["candidates"] == 2 and preview["candidate_bytes"] == expected_bytes, preview
                assert preview["skipped_pinned"] == 1 and preview["skipped_active"] == 1, preview
                assert durable_bytes() == before and payloads(home / "cache") == before_payloads
                cleared = cli("cache", "clear", "--apply", offline=True)
                assert cleared["apply"] and cleared["evicted"] == 2 and cleared["freed_bytes"] == expected_bytes, cleared
                assert cleared["skipped_pinned"] == 1 and cleared["skipped_active"] == 1, cleared
                assert set(cached()) == {"active.txt", "pinned.txt"}
                assert (home / "cache" / active_object).read_bytes() == contents["active.txt"]
                assert unmanaged.read_bytes() == b"unindexed data must never be cleared"
                assert all(path.read_bytes() == data for path, data in protected.items())
                assert lease.read() == contents["active.txt"], "clear invalidated an active reader"
            offline_get("pinned.txt")
            offline_get("active.txt")
            cli("cache", "get", "test", "clean.txt", root / "missing-clean", ok=False, offline=True)
            assert not (root / "missing-clean").exists()
            cleared = cli("cache", "clear", "--apply", offline=True)
            assert cleared["evicted"] == 1 and cleared["freed_bytes"] == len(contents["active.txt"]), cleared
            assert set(cached()) == {"pinned.txt"}
            again = cli("cache", "clear", "--apply", offline=True)
            assert again["evicted"] == 0 and again["freed_bytes"] == 0 and again["candidates"] == 0, again
            assert all(path.read_bytes() == data for path, data in protected.items())
            before = durable_bytes()
            cli("cache", "directory", custom, ok=False, offline=True)
            assert durable_bytes() == before and unmanaged.read_bytes() == b"unindexed data must never be cleared"
            unmanaged.unlink()
            passed.append("clear defaults to a mutation-free preview; apply removes only recorded unpinned inactive payloads, preserves locks/pins/unmanaged data, and is restart-idempotent")
            passed.append("clear and migration retain real queued snapshots plus separate dirty/native fixture payloads and native accounting")

            Fixture.online = True
            cli("queue", "run")
            assert (remote / "queued.txt").read_bytes() == queued_content
            assert next(item for item in cli("queue", "list") if item["id"] == job["id"])["status"] == "done"
            passed.append("a queued upload still publishes its original durable snapshot after cache cleanup and both migrations")

            allowed = private(root / "allowed")
            outside = private(root / "outside")
            mcp_cache = allowed / "selected-cache"
            readonly = new_client(roots=[allowed])
            read_catalog = readonly.request("tools/list")["tools"]
            read_names = {tool["name"] for tool in read_catalog}
            assert "rdrive_cache_directory" in read_names
            assert not {"rdrive_cache_set_directory", "rdrive_cache_clear"} & read_names
            assert next(tool for tool in read_catalog if tool["name"] == "rdrive_cache_directory")["annotations"]["readOnlyHint"]
            before = durable_bytes()
            assert Path(readonly.tool("cache_directory")["directory"]) == home / "cache"
            readonly.invalid_tool("cache_set_directory", directory=str(mcp_cache))
            readonly.invalid_tool("cache_clear", apply=True)
            readonly.invalid_tool("cache_directory", directory=str(mcp_cache))
            assert durable_bytes() == before and not mcp_cache.exists()

            writable = new_client(write=True, roots=[allowed])
            write_catalog = writable.request("tools/list")["tools"]
            for name in ("rdrive_cache_set_directory", "rdrive_cache_clear"):
                assert not next(tool for tool in write_catalog if tool["name"] == name)["annotations"]["readOnlyHint"]
            writable.invalid_tool("cache_set_directory")
            writable.invalid_tool("cache_set_directory", directory=str(mcp_cache), default=True)
            writable.invalid_tool("cache_set_directory", directory=str(mcp_cache), dry_run="true")
            writable.invalid_tool("cache_clear", apply="true")
            writable.invalid_tool("cache_set_directory", directory=str(outside / "cache"))
            writable.invalid_tool("cache_set_directory", directory=str(allowed / ".." / "outside" / "cache"))
            escaped = allowed / "escaped"
            escaped.symlink_to(outside, target_is_directory=True)
            writable.invalid_tool("cache_set_directory", directory=str(escaped / "cache"))
            no_roots = new_client(write=True)
            no_roots.invalid_tool("cache_set_directory", directory=str(mcp_cache))
            assert durable_bytes() == before and not mcp_cache.exists() and not (outside / "cache").exists()
            plan = writable.tool("cache_set_directory", directory=str(mcp_cache), dry_run=True)
            assert not plan["apply"] and not mcp_cache.exists() and durable_bytes() == before
            migrated = writable.tool("cache_set_directory", directory=str(mcp_cache))
            assert migrated["apply"] and Path(readonly.tool("cache_directory")["directory"]) == mcp_cache
            pin_object = cached()["pinned.txt"]["object"]
            assert (mcp_cache / pin_object).read_bytes() == contents["pinned.txt"]
            # The selected cache is private state even when it sits below an
            # otherwise approved data root. Generic file tools cannot export,
            # overwrite or synchronize it.
            writable.invalid_tool("upload", profile="test", local=str(mcp_cache / pin_object), remote="cache-leak")
            writable.invalid_tool("download", profile="test", remote="pinned.txt", local=str(mcp_cache / "overwrite"))
            readonly.invalid_tool("sync_plan", profile="test", local=str(mcp_cache), remote="")
            readonly.invalid_tool("sync_plan", profile="test", local=str(allowed), remote="")
            broad = new_client(write=True, roots=[root])
            broad.invalid_tool("upload", profile="test", local=str(mcp_cache / pin_object), remote="broad-cache-leak")
            broad.invalid_tool("upload", profile="test", local=str(home / "config.json"), remote="state-leak")
            broad.invalid_tool("cache_set_directory", directory=str(home / "spool"))
            assert not (remote / "cache-leak").exists() and not (remote / "broad-cache-leak").exists()
            assert not (remote / "state-leak").exists() and not (mcp_cache / "overwrite").exists()

            # macOS commonly preserves a caller's spelling through resolve()
            # while differently cased paths still address the same inode.
            # Exercise that filesystem behavior, without pretending a
            # case-sensitive filesystem tested the equivalent boundary.
            originals = {"state": home, "cache": mcp_cache, "cache_parent": allowed, "ancestor": root}
            aliases = {name: path.with_name(path.name.swapcase()) for name, path in originals.items()}
            alias_probe = {name: alias != originals[name] and alias.exists() and alias.samefile(originals[name])
                           for name, alias in aliases.items()}
            if all(alias_probe.values()):
                known_sync = private(root / "known-sync")
                (remote / "known-sync").mkdir()
                cli("sync", "test", known_sync, "known-sync", "--apply")
                known_alias = known_sync.with_name(known_sync.name.swapcase())
                assert known_alias.exists() and known_alias.samefile(known_sync)
                before = durable_bytes()
                before_payloads = payloads(mcp_cache)
                before_requests = len(Fixture.requests_seen)
                state_alias = aliases["state"]
                blocked_locations = (state_alias, state_alias / "spool", state_alias / "spool" / "case-cache",
                                     aliases["ancestor"], known_alias, known_alias / "cache-child")
                for destination in blocked_locations:
                    for options in ((), ("--dry-run",)):
                        cli("cache", "directory", destination, *options, ok=False)
                        assert durable_bytes() == before and payloads(mcp_cache) == before_payloads
                assert not (home / "spool" / "case-cache").exists()
                assert not (known_sync / "cache-child").exists()
                assert not list(known_sync.iterdir()), "refused migration adopted a known synchronization tree"
                for source in (aliases["cache"], aliases["cache_parent"]):
                    for options in ((), ("--apply",)):
                        rejected = cli("sync", "test", source, "", *options, ok=False)
                        detail = rejected.stderr.decode(errors="replace").lower()
                        assert "cache" in detail and "overlap" in detail, detail
                writable.invalid_tool("upload", profile="test", local=str(aliases["cache"] / pin_object),
                                      remote="alias-cache-leak")
                broad.invalid_tool("upload", profile="test", local=str(aliases["cache_parent"] / mcp_cache.name / pin_object),
                                   remote="alias-parent-cache-leak")
                writable.invalid_tool("download", profile="test", remote="pinned.txt",
                                      local=str(aliases["cache"] / "alias-overwrite"))
                readonly.invalid_tool("sync_plan", profile="test", local=str(aliases["cache"]), remote="")
                readonly.invalid_tool("sync_plan", profile="test", local=str(aliases["cache_parent"]), remote="")
                broad.invalid_tool("cache_set_directory", directory=str(state_alias / "spool" / "mcp-case-cache"))
                broad.invalid_tool("upload", profile="test", local=str(state_alias / "config.json"), remote="alias-state-leak")
                assert durable_bytes() == before and payloads(mcp_cache) == before_payloads
                assert len(Fixture.requests_seen) == before_requests, "rejected aliases contacted remote storage"
                assert not (mcp_cache / "alias-overwrite").exists() and not (home / "spool" / "mcp-case-cache").exists()
                for name in ("alias-cache-leak", "alias-parent-cache-leak", "alias-state-leak"):
                    assert not (remote / name).exists(), f"a protected alias was exported: {name}"
                passed.append("real same-inode case aliases cannot bypass state/sync-tree migration guards, CLI sync overlap checks, or MCP cache export/overwrite/local-root protection")
            else:
                skipped.append({"check": "same-inode case-alias protection", "reason":
                                "Temporary filesystem did not resolve every differing-case directory name to the same inode",
                                "observed_aliases": alias_probe})

            Fixture.online = False
            before_requests = len(Fixture.requests_seen)
            mcp_output = allowed / "offline-mcp.txt"
            writable.tool("cache_get", profile="test", remote="pinned.txt", local=str(mcp_output))
            assert mcp_output.read_bytes() == contents["pinned.txt"]
            preview = writable.tool("cache_clear")
            assert not preview["apply"] and preview["skipped_pinned"] == 1 and preview["evicted"] == 0, preview
            applied = writable.tool("cache_clear", apply=True)
            assert applied["apply"] and applied["evicted"] == 0, applied
            returned = writable.tool("cache_set_directory", default=True)
            assert returned["apply"] and Path(readonly.tool("cache_directory")["directory"]) == home / "cache"
            offline_get("pinned.txt")
            assert len(Fixture.requests_seen) == before_requests
            passed.append("MCP exposes a read-only directory query and write-gated clear/migration with strict schemas and explicit local-root authorization")
            passed.append("MCP blocks custom-cache export, overwrite and overlapping sync even under broad roots; offline cache reads and return-to-default remain available")
        finally:
            try:
                for client in reversed(clients):
                    client.close()
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    assert sha256(binary) == binary_hash, "binary changed while the workflow was running"
    try:
        binary_label = binary.relative_to(REPOSITORY).as_posix()
    except ValueError:
        binary_label = binary.name
    report = {
        "binary": binary_label,
        "binary_sha256": binary_hash,
        "test": Path(__file__).resolve().relative_to(REPOSITORY).as_posix(),
        "validated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": "Private temporary state and authenticated loopback WebDAV only; no real accounts, mounts or running application state",
        "passed": passed,
        "skipped": skipped,
    }
    if args.report:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
