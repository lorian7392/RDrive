#!/usr/bin/env python3
"""Real CLI state relocation with disposable DAV, indexes and retained edits.

The fixture has an isolated HOME and no resident-service installation. It never
reads Keychain, touches the user's locator, mounts storage or controls launchd.
Managed-service failure/recovery is exercised through the Rust host seam.
"""
import argparse
import base64
from contextlib import contextmanager
import copy
import ctypes
import datetime
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time

from index_workflows import Broker, IndexedDav, read_provider_journal, report_binary_path, wait


PASSWORD_VARIABLE = "RDRIVE_STATE_DIRECTORY_PASSWORD"
SNAPSHOT_ATTRIBUTE = "user.rdrive-state-migration"
SNAPSHOT_ATTRIBUTE_VALUE = b"selected user metadata retained across state relocation"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def private(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.chmod(0o700)
    return path


def set_attribute(path, name, value):
    if hasattr(os, "setxattr"):
        os.setxattr(path, name, value)
        return
    library = ctypes.CDLL(None, use_errno=True)
    function = library.setxattr
    function.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    function.restype = ctypes.c_int
    buffer = ctypes.create_string_buffer(value)
    if function(os.fsencode(path), name.encode(), buffer, len(value), 0, 0) < 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(path))


def get_attribute(path, name):
    if hasattr(os, "getxattr"):
        return os.getxattr(path, name)
    library = ctypes.CDLL(None, use_errno=True)
    function = library.getxattr
    function.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    function.restype = ctypes.c_ssize_t
    size = function(os.fsencode(path), name.encode(), None, 0, 0, 0)
    if size < 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(path))
    buffer = ctypes.create_string_buffer(size)
    actual = function(os.fsencode(path), name.encode(), buffer, size, 0, 0)
    if actual < 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(path))
    return buffer.raw[:actual]


def tree_bytes(root):
    return {str(path.relative_to(root)): digest(path)
            for path in root.rglob("*") if path.is_file() and not path.is_symlink()}


class StateFixture:
    """One real private connection, cache, SQLite catalog and interrupted upload."""

    def __init__(self, binary, root, *, external_cache=False):
        self.binary, self.root = binary, root
        private(root)
        self.user_home = private(root / "synthetic user home")
        self.standard = private(self.user_home / "Library/Application Support/rdrive")
        self.locator = self.standard / "shared-home.json"
        self.remote = private(root / "remote")
        private(self.remote / "uploads")
        (self.remote / "doc.txt").write_bytes(b"original remote document\n")
        (self.remote / "cached.txt").write_bytes(b"verified cached bytes\n")
        self.queued = b"Q" * (5 * 1024 * 1024) + b"original retained resumable tail\n"
        self.edit = b"durable offline Finder edit\n"

        class Fixture(IndexedDav):
            online = True
            fail_nextcloud = False
            requests_seen = []
            directory_requests = copy.copy(IndexedDav.directory_requests)
            failed = set()
            gates = {}
            locks = {}
            range_requests = []
            interrupt_once = set()

            def handle_dav(handler):
                cls = type(handler)
                # Count even failed/offline requests before the inherited peer
                # can return early. Offline migration must make no request.
                cls.requests_seen.append((handler.command, handler.path))
                if cls.fail_nextcloud and handler.command == "PUT" and "/uploads/rdrive-" in handler.path and handler.path.endswith("/00002"):
                    handler.rfile.read(int(handler.headers.get("Content-Length", 0)))
                    return handler.reply(503)
                if handler.command == "MOVE" and handler.path.endswith("/.file"):
                    handler.rfile.read(int(handler.headers.get("Content-Length", 0)))
                    expected = "Basic " + base64.b64encode(b"user:fixture-password").decode()
                    if handler.headers.get("Authorization") != expected:
                        return handler.reply(401)
                    session = handler.resource().parent
                    destination = handler.resource(handler.headers["Destination"])
                    if not session.is_dir():
                        return handler.reply(404)
                    if destination.exists() and handler.headers.get("Overwrite") != "T":
                        return handler.reply(412)
                    destination.write_bytes(b"".join(path.read_bytes() for path in sorted(session.iterdir())
                                                     if path.is_file() and path.name.isdecimal()))
                    return handler.reply(201, ETag=handler.etag(destination))
                return super(Fixture, handler).handle_dav()

        self.handler = Fixture
        Fixture.root = self.remote
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        self.server.daemon_threads = True
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/dav/"
        self.broker = Broker(binary, root, self.url, domain="migration")
        self.broker.home = root / "original state"
        self.broker.stage = self.broker.home / "SmartTransfers"
        self.broker.env.update(HOME=str(self.user_home), **{PASSWORD_VARIABLE: "fixture-password"})
        self.broker.env.pop("RDRIVE_HOME", None)
        self.broker.env.pop("RDRIVE_INDEX_PASSWORD", None)
        self.broker.base = self.base(self.broker.home)
        self.home = self.broker.home
        self.destination = private(root / "Application Support") / "new state"
        self.external_cache = private(root / "external clean cache") if external_cache else None

    def base(self, home=None):
        arguments = [str(self.binary)]
        if home is not None:
            arguments += ["--home", str(home)]
        return arguments + ["--json", "--progress", "off"]

    def cli(self, *arguments, home=None, success=True, offline=False, seconds=60):
        environment = self.broker.env.copy()
        if offline:
            environment.pop(PASSWORD_VARIABLE, None)
        deadline = time.monotonic() + seconds
        while True:
            result = subprocess.run(self.base(home) + list(map(str, arguments)), env=environment,
                                    capture_output=True, text=True, timeout=seconds)
            if not success:
                assert result.returncode != 0, (arguments, result.stdout, result.stderr)
                return result
            if result.returncode == 0:
                return json.loads(result.stdout)
            if not any(text in result.stderr for text in ("another rdrive process", "already locked", "state directory is in use")):
                raise AssertionError((arguments, result.returncode, result.stdout, result.stderr))
            assert time.monotonic() < deadline, result.stderr
            time.sleep(.05)

    def selected(self, *arguments, **kwargs):
        return self.cli(*arguments, home=self.home, **kwargs)

    def prepare(self):
        self.selected("profile", "add", "fixture", "--protocol", "webdav", "--url", self.url,
                      "--allow-http", "--username", "user", "--password-env", PASSWORD_VARIABLE,
                      "--nextcloud-upload-url", self.url + "uploads/", "--timeout", "3")
        if self.external_cache is not None:
            self.selected("cache", "directory", self.external_cache)
        self.selected("cache", "fetch", "fixture", "cached.txt", "--pin")
        local = self.root / "caller source.txt"
        local.write_bytes(self.queued)
        self.job = self.selected("put", "fixture", local, "queued.txt", "--queue", "--resume")
        local.write_bytes(b"later caller edit; not the upload snapshot\n")
        self.handler.fail_nextcloud = True
        # Queue processing reports individual failures in its result. Either
        # process exit convention is accepted; the durable failed job is checked.
        subprocess.run(self.base(self.home) + ["queue", "run", "fixture"], env=self.broker.env,
                       capture_output=True, timeout=30, check=False)
        self.handler.fail_nextcloud = False
        self.checkpoint = self.home / "spool" / (self.job["id"] + ".upload.json")
        self.checkpoint_before = json.loads(self.checkpoint.read_bytes())
        assert self.checkpoint_before["phase"] == "uploading"
        assert set(self.checkpoint_before["chunks"]) == {"1"}
        self.confirmed_chunk_puts = sum(command == "PUT" and path.endswith("/00001")
                                        for command, path in self.handler.requests_seen)
        assert self.confirmed_chunk_puts
        assert self.checkpoint_before["source_path"] == str(self.home / "spool" / self.job["snapshot"])
        self.selected("index", "config", "--disable")
        self.selected("smart", "setup", "migration", "fixture")
        self.broker.start()
        items = wait(lambda: self.broker.ok(op="list", parent_id="root")["items"], "initial real SQLite root")
        self.doc = next(item for item in items if item["name"] == "doc.txt")
        self.selected("smart", "pause")
        self.handler.online = False
        staged = self.broker.stage / "retained edit"
        staged.write_bytes(self.edit)
        set_attribute(staged, SNAPSHOT_ATTRIBUTE, SNAPSHOT_ATTRIBUTE_VALUE)
        self.broker.rpc(op="modify", id=self.doc["id"], content_path=str(staged),
                        base_content_version=self.doc["content_version"],
                        base_metadata_version=self.doc["metadata_version"])
        staged.unlink()
        self.broker.stop()
        self.index_before = read_provider_journal(self.home, "migration")
        self.queue_path = self.home / "smart" / hashlib.sha256(b"migration").hexdigest() / "queue.json"
        self.queue_before = self.queue_path.read_bytes()
        queue = json.loads(self.queue_before)["mutations"]
        self.pending = queue if isinstance(queue, dict) else {row["id"]: row for row in queue}
        assert len(self.pending) == 1
        self.dirty_before = {path.relative_to(self.home): path.read_bytes()
                             for path in self.queue_path.parent.joinpath("dirty").iterdir() if path.is_file()}
        assert self.edit in self.dirty_before.values()
        self.dirty_attributes = {relative: get_attribute(self.home / relative, SNAPSHOT_ATTRIBUTE)
                                 for relative in self.dirty_before}
        assert SNAPSHOT_ATTRIBUTE_VALUE in self.dirty_attributes.values()
        self.configuration_before = json.loads((self.home / "config.json").read_bytes())
        self.database_before = json.loads((self.home / "state.json").read_bytes())
        self.snapshot_before = (self.home / "spool" / self.job["snapshot"]).read_bytes()
        assert self.snapshot_before == self.queued
        self.search_before = self.selected("index", "search", "migration", "doc", offline=True)
        return self

    def moved(self):
        self.home = self.destination
        self.broker.home = self.home
        self.broker.stage = self.home / "SmartTransfers"
        self.broker.base = self.base(self.home)
        self.queue_path = self.home / "smart" / hashlib.sha256(b"migration").hexdigest() / "queue.json"
        self.checkpoint = self.home / "spool" / (self.job["id"] + ".upload.json")

    def close(self):
        self.broker.close()
        self.server.shutdown()
        self.server.server_close()


@contextmanager
def fixture(binary, root, **options):
    current = StateFixture(binary, root, **options)
    try:
        yield current.prepare()
    finally:
        current.close()


def run(binary):
    binary = Path(binary).resolve()
    original_binary = digest(binary)
    checks = []
    started = time.monotonic()

    def passed(text):
        checks.append(text)
        print("PASS " + text, flush=True)

    with tempfile.TemporaryDirectory(prefix="rd-state-", dir="/private/tmp") as temporary:
        root = private(Path(temporary).resolve())
        with fixture(binary, root / "primary") as current:
            source = current.home
            shown = current.selected("config", "show", offline=True)
            assert Path(shown["home"]) == source and Path(shown["configuration"]) == source / "config.json"
            assert Path(shown["shared_locator"]) == current.locator
            before = tree_bytes(source)
            requests = len(current.handler.requests_seen)
            plan = current.selected("config", "migrate", "--to", current.destination, offline=True)
            assert plan["apply"] is False and Path(plan["source"]) == source and Path(plan["destination"]) == current.destination
            assert tree_bytes(source) == before and not current.destination.exists() and not current.locator.exists()
            assert len(current.handler.requests_seen) == requests
            passed("migration planning is offline and preserves every source byte without creating a destination or shared locator")

            nonempty = private(current.root / "unrelated destination")
            (nonempty / "keep.txt").write_bytes(b"unrelated retained data")
            alias = current.root / "destination alias"
            alias.symlink_to(nonempty, target_is_directory=True)
            for invalid in (source, source / "nested", source.parent, nonempty, alias):
                current.selected("config", "migrate", "--to", invalid, offline=True, success=False)
            assert tree_bytes(source) == before and (nonempty / "keep.txt").read_bytes() == b"unrelated retained data"
            passed("equal, ancestor, nested, nonempty and symlink destinations are rejected without changing source or unrelated files")

            with (source / "client.lock").open("rb+") as lease:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                current.selected("config", "migrate", "--to", current.destination, "--apply", offline=True, success=False)
            assert tree_bytes(source) == before and not current.destination.exists()
            passed("a separate process holding the real Store writer lock prevents apply without freezing or copying the source")

            configuration = (source / "config.json").read_bytes()
            try:
                (source / "config.json").write_bytes(b"invalid retained configuration")
                current.selected("config", "migrate", "--to", current.destination, offline=True, success=False)
            finally:
                (source / "config.json").write_bytes(configuration)
            assert tree_bytes(source) == before and not current.destination.exists()
            passed("corrupt connection configuration is refused before relocation and remains recoverable in place")

            applied = current.selected("config", "migrate", "--to", current.destination, "--apply", offline=True)
            assert applied["migrated"] is True and Path(applied["home"]) == current.destination
            assert Path(applied["source_backup"]).is_dir()
            current.moved()
            assert json.loads((current.home / "config.json").read_bytes()) == current.configuration_before
            assert json.loads((current.home / "state.json").read_bytes()) == current.database_before
            assert (current.home / "spool" / current.job["snapshot"]).read_bytes() == current.snapshot_before
            assert current.queue_path.read_bytes() == current.queue_before
            assert all((current.home / relative).read_bytes() == content for relative, content in current.dirty_before.items())
            assert all(get_attribute(current.home / relative, SNAPSHOT_ATTRIBUTE) == content
                       for relative, content in current.dirty_attributes.items())
            checkpoint = json.loads(current.checkpoint.read_bytes())
            expected = dict(current.checkpoint_before, source_path=str(current.home / "spool" / current.job["snapshot"]))
            assert checkpoint == expected
            assert len(current.handler.requests_seen) == requests
            passed("apply preserves connection references, queue IDs, immutable Smart requests and all retained payloads while rebasing the real Nextcloud checkpoint")

            after_index = read_provider_journal(current.home, "migration")
            assert after_index == current.index_before
            assert current.selected("index", "search", "migration", "doc", offline=True) == current.search_before
            directory = current.selected("cache", "directory", offline=True)
            assert Path(directory["directory"]) == current.home / "cache"
            exported = current.root / "verified offline cache.txt"
            current.selected("cache", "get", "fixture", "cached.txt", exported, offline=True)
            assert exported.read_bytes() == b"verified cached bytes\n"
            passed("the unchanged SQLite namespace/search and pinned clean cache remain readable offline at a destination containing spaces")

            locator = json.loads(current.locator.read_bytes())
            assert Path(locator["home"]) == current.home
            profiles = current.cli("profile", "list", offline=True)
            assert "fixture" in json.dumps(profiles)
            stale = current.cli("profile", "list", home=source, offline=True, success=False)
            assert any(text in stale.stderr.lower() for text in ("migrat", "relocat", "moved")), stale.stderr
            assert not (source / "config.json").exists() and (source / "client.lock").stat().st_mode & 0o222 == 0
            assert len(current.handler.requests_seen) == requests
            passed("default CLI resolves the updated shared locator and the sealed old source refuses stale callers without contacting DAV")

            recovered = current.cli("config", "recover", offline=True)
            assert recovered and current.locator.exists()
            assert json.loads((current.home / "smart/control.json").read_bytes())["paused"] is True
            current.broker.start()
            cached_items = current.broker.ok(op="list", parent_id="root")["items"]
            assert any(item["id"] == current.doc["id"] for item in cached_items)
            assert current.queue_path.read_bytes() == current.queue_before
            assert all(get_attribute(current.home / relative, SNAPSHOT_ATTRIBUTE) == content
                       for relative, content in current.dirty_attributes.items())
            current.handler.online = True
            current.selected("smart", "resume")
            wait(lambda: (current.remote / "doc.txt").read_bytes() == current.edit, "retained edit publication after migration")
            current.broker.stop()
            current.selected("queue", "run", "fixture", "--retry-failed")
            assert (current.remote / "queued.txt").read_bytes() == current.queued
            completed = json.loads(current.checkpoint.read_bytes())
            assert completed["phase"] == "complete" and completed["source_path"] == str(current.home / "spool" / current.job["snapshot"])
            assert sum(command == "PUT" and path.endswith("/00001")
                       for command, path in current.handler.requests_seen) == current.confirmed_chunk_puts
            passed("restart retains original pause policy and cached item IDs; after reconnect the Smart edit and real resumable queued upload both publish their original snapshots")

        with fixture(binary, root / "external", external_cache=True) as current:
            external_before = tree_bytes(current.external_cache)
            current.selected("config", "migrate", "--to", current.destination, "--apply", offline=True)
            current.moved()
            assert json.loads((current.home / "config.json").read_bytes())["cache"]["directory"] == str(current.external_cache)
            # Ownership metadata may rebind to the new state, payloads must not.
            external_after = tree_bytes(current.external_cache)
            assert {k: v for k, v in external_after.items() if k != ".rdrive-cache.json"} == {k: v for k, v in external_before.items() if k != ".rdrive-cache.json"}
            exported = current.root / "external cache export"
            current.selected("cache", "get", "fixture", "cached.txt", exported, offline=True)
            assert exported.read_bytes() == b"verified cached bytes\n"
            assert current.queue_path.read_bytes() == current.queue_before
            passed("an external clean-cache location keeps its configured path and exact payload bytes while its ownership rebind permits offline access from the new state")

        with fixture(binary, root / "crash") as current:
            source = current.home
            private(source / "tmp/copy-proof")
            for number in range(2000):
                (source / "tmp/copy-proof" / f"{number:04}.bin").write_bytes(f"{number:04}".encode() * 1024)
            # A prior locator makes rollback observable to an ordinary CLI,
            # including when source access is temporarily blocked by its guard.
            current.locator.write_text(json.dumps({"version": 1, "home": str(source)}))
            current.locator.chmod(0o600)
            before = tree_bytes(source)
            process = subprocess.Popen(current.base(source) + ["config", "migrate", "--to", str(current.destination), "--apply"],
                                       env=current.broker.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            killed = False
            deadline = time.monotonic() + 45
            try:
                while process.poll() is None and time.monotonic() < deadline:
                    try:
                        intent = json.loads((current.standard / "state-directory-migration.json").read_bytes())
                        stage = Path(intent["stage"])
                        # This real snapshot is copied before the 2,000-file
                        # tail. Its presence proves copy began; the durable
                        # phase must still be before the publication decision.
                        copied = stage / "spool" / current.job["snapshot"]
                        if intent["phase"] == "preparing" and copied.is_file():
                            process.kill()
                            killed = True
                            break
                    except (OSError, ValueError, KeyError):
                        pass
                    time.sleep(.001)
                stdout, stderr = process.communicate(timeout=20)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=15)
            assert killed and process.returncode == -signal.SIGKILL, (process.returncode, stdout, stderr)
            assert (source / "state-directory.json").exists(), "the killed migration did not publish its source guard"
            recovered = current.cli("config", "recover", offline=True)
            assert recovered["recovered"] is True and recovered["committed"] is False and Path(recovered["home"]) == source
            assert tree_bytes(source) == before
            assert Path(json.loads(current.locator.read_bytes())["home"]) == source
            assert "fixture" in json.dumps(current.cli("profile", "list", offline=True))
            assert current.cli("config", "recover", offline=True)["committed"] is False
            assert current.queue_path.read_bytes() == current.queue_before
            assert all((source / relative).read_bytes() == content for relative, content in current.dirty_before.items())
            assert all(get_attribute(source / relative, SNAPSHOT_ATTRIBUTE) == content
                       for relative, content in current.dirty_attributes.items())
            assert any(current.standard.glob("state-incomplete-*")), "the owned partial copy was not retained for inspection"
            assert not current.destination.exists()
            passed("a real SIGKILL during a partially copied migration recovers through the default CLI, restores every original byte and locator, and retains the partial copy without losing queued edits")

    assert digest(binary) == original_binary, "the tested CLI changed during the workflows"
    return {"schema": 1, "generated": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "binary": report_binary_path(binary), "binary_sha256": original_binary,
            "status": "passed", "passed": len(checks), "checks": checks,
            "seconds": round(time.monotonic() - started, 3)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", type=Path, default=Path("target/debug/rdrive"))
    parser.add_argument("--report", type=Path)
    options = parser.parse_args()
    os.umask(0o077)
    report = run(options.binary)
    if options.report:
        options.report.parent.mkdir(parents=True, exist_ok=True)
        options.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "passed": report["passed"], "seconds": report["seconds"]}))


if __name__ == "__main__":
    main()
