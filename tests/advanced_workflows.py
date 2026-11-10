#!/usr/bin/env python3
"""Black-box managed transactions, metadata, concurrency and provider bridge tests.

Uses only temporary local storage, loopback HTTP/TCP, and the Python standard
library. Run: python3 tests/advanced_workflows.py [path/to/rdrive]
"""
import concurrent.futures
import ctypes
import hashlib
import json
import os
from pathlib import Path
import plistlib
import posixpath
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav
import http.server

TAGS = "com.apple.metadata:_kMDItemUserTags"

# CPython exposes os.*xattr on Linux but not all macOS builds. Call the native
# fd APIs directly on macOS, independently of the Rust implementation under test.
_libc = ctypes.CDLL(None, use_errno=True)
if sys.platform == "darwin":
    _libc.fsetxattr.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    _libc.fsetxattr.restype = ctypes.c_int
    _libc.fgetxattr.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    _libc.fgetxattr.restype = ctypes.c_ssize_t
    _libc.flistxattr.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    _libc.flistxattr.restype = ctypes.c_ssize_t
    _libc.fremovexattr.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
    _libc.fremovexattr.restype = ctypes.c_int


def native_xattr(path, operation):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        return operation(descriptor)
    finally:
        os.close(descriptor)


def checked(result):
    if result < 0:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number))
    return result


def set_xattr(path, name, value):
    if sys.platform != "darwin":
        return os.setxattr(path, name, value)
    buffer = ctypes.create_string_buffer(value)
    return native_xattr(path, lambda fd: checked(_libc.fsetxattr(fd, name.encode(), buffer, len(value), 0, 0)))


def get_xattr(path, name):
    if sys.platform != "darwin":
        return os.getxattr(path, name)
    def read(fd):
        length = checked(_libc.fgetxattr(fd, name.encode(), None, 0, 0, 0))
        buffer = ctypes.create_string_buffer(max(1, length))
        actual = checked(_libc.fgetxattr(fd, name.encode(), buffer, length, 0, 0))
        return buffer.raw[:actual]
    return native_xattr(path, read)


def list_xattrs(path):
    if sys.platform != "darwin":
        return os.listxattr(path)
    def read(fd):
        length = checked(_libc.flistxattr(fd, None, 0, 0))
        buffer = ctypes.create_string_buffer(max(1, length))
        actual = checked(_libc.flistxattr(fd, buffer, length, 0))
        return [value.decode() for value in buffer.raw[:actual].split(b"\0") if value]
    return native_xattr(path, read)


def remove_xattr(path, name):
    if sys.platform != "darwin":
        return os.removexattr(path, name)
    return native_xattr(path, lambda fd: checked(_libc.fremovexattr(fd, name.encode(), 0)))


class AdvancedDav(Dav):
    mutation_lock = threading.RLock()
    counters_lock = threading.Lock()
    active_puts = 0
    peak_puts = 0
    publication_barrier = None
    body_received = {}
    gate_lock = threading.Lock()
    read_gate = None
    ack_gate = None

    @classmethod
    def arm_gate(cls, name, path):
        gate = {"path": path, "entered": threading.Event(), "release": threading.Event()}
        with cls.gate_lock:
            assert getattr(cls, name) is None, "a fixture gate is already armed"
            setattr(cls, name, gate)
        return gate

    def consume_gate(self, name, path):
        with self.gate_lock:
            gate = getattr(type(self), name)
            if gate is None or gate["path"] != path:
                return None
            setattr(type(self), name, None)
        return gate

    def reply(self, status, body=b"", **headers):
        expected_disconnect = False
        if self.command == "MOVE" and 200 <= status < 300:
            gate = self.consume_gate("ack_gate", self.headers.get("Destination", ""))
            if gate:
                # Dav has published the immutable slot; hold only its response.
                gate["entered"].set()
                if not gate["release"].wait(timeout=30):
                    raise AssertionError("commit acknowledgment gate timed out")
                expected_disconnect = True
                self.close_connection = True
        try:
            return super().reply(status, body, **headers)
        except (BrokenPipeError, ConnectionResetError):
            # A test intentionally kills the client after server publication.
            if not expected_disconnect:
                raise
            self.close_connection = True

    def resource(self, value=None):
        value = value or self.path
        from urllib.parse import urlsplit, urlunsplit
        pieces = urlsplit(value)
        path = posixpath.normpath(pieces.path)
        if path == "/dav":
            path += "/"
        return super().resource(urlunsplit((pieces.scheme, pieces.netloc, path, pieces.query, pieces.fragment)))

    def handle_dav(self):
        if self.command == "GET":
            gate = self.consume_gate("read_gate", self.path)
            if gate:
                gate["entered"].set()
                if not gate["release"].wait(timeout=30):
                    raise AssertionError("snapshot read gate timed out")
            path = self.resource()
            expected = self.headers.get("If-Match")
            if expected and (not path.exists() or self.etag(path) != expected):
                return self.reply(412)
        slow = self.command == "PUT" and self.path.startswith("/dav/parallel/")
        if slow:
            with self.counters_lock:
                type(self).active_puts += 1
                type(self).peak_puts = max(type(self).peak_puts, type(self).active_puts)
            try:
                time.sleep(0.3)
                result = super().handle_dav()
                self.body_received[self.path] = time.monotonic()
                return result
            finally:
                with self.counters_lock:
                    type(self).active_puts -= 1
        if self.command == "MOVE" and self.headers.get("Destination", "").endswith("/commits/00000000000000000002.json") and self.publication_barrier:
            try:
                self.publication_barrier.wait(timeout=10)
            except threading.BrokenBarrierError:
                return self.reply(500)
        if self.command in ("PUT", "MOVE", "COPY", "MKCOL", "DELETE"):
            with self.mutation_lock:
                return super().handle_dav()
        return super().handle_dav()


def tag_bytes(*names):
    return plistlib.dumps(list(names), fmt=plistlib.FMT_BINARY, sort_keys=False)


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/rdrive").resolve()
    checks = []
    failures = []
    with tempfile.TemporaryDirectory(prefix="rd-advanced-", dir="/private/tmp") as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        AdvancedDav.root = root / "remote"
        AdvancedDav.root.mkdir()
        AdvancedDav.requests_seen = []
        AdvancedDav.locks = {}
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), AdvancedDav)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environments = {}
        processes = []

        def environment(name="a"):
            if name not in environments:
                environments[name] = dict(os.environ, RDRIVE_HOME=str(root / ("state-" + name)), RDRIVE_TEST_PASSWORD="fixture-password")
                environments[name].pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
            return environments[name]

        def raw_cli(*args, home="a", options=(), timeout=90):
            progress = [] if "--progress" in options else ["--progress", "off"]
            return subprocess.run([str(binary), "--json", *progress, *map(str, options), *map(str, args)], env=environment(home), capture_output=True, timeout=timeout)

        def cli(*args, home="a", options=(), ok=True):
            result = raw_cli(*args, home=home, options=options)
            if not ok:
                assert result.returncode != 0, (args, "unexpected success")
                return result
            assert result.returncode == 0, f"{args}: {result.stderr.decode()}\n{result.stdout.decode()}"
            assert b"fixture-password" not in result.stderr
            return json.loads(result.stdout)

        def group(name, function):
            try:
                function()
                checks.append(name)
            except Exception as error:
                failures.append({"group": name, "error": str(error)})

        def connect_provider(descriptor, message, *, authenticated=True):
            message = dict(v=1, **message)
            if authenticated:
                message["auth_token"] = descriptor["token"]
            else:
                message["auth_token"] = "wrong-token"
            with socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=60) as client:
                client.sendall(json.dumps(message, ensure_ascii=False).encode() + b"\n")
                # Keep the write half open: closing the request stream is the
                # bridge's cancellation signal while an operation is running.
                reader = client.makefile("rb")
                while True:
                    line = reader.readline(2 * 1024 * 1024)
                    assert line, "provider closed without a result"
                    value = json.loads(line)
                    if "progress" in value:
                        continue
                    return value

        def request(descriptor, **message):
            response = connect_provider(descriptor, message)
            assert response.get("ok"), f"provider {message}: {response}"
            return response

        try:
            for home in ("a", "b"):
                cli("profile", "add", "test", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_TEST_PASSWORD", home=home)
            for directory in ("metadata", "metasync", "parallel", "repo", "ordinary"):
                cli("mkdir", "test", directory)
            payload = root / "payload.bin"
            content = bytes(range(256)) * 128
            payload.write_bytes(content)
            original_tags = tag_bytes("Research\n4", "待办\n0")
            set_xattr(payload, TAGS, original_tags)
            set_xattr(payload, "user.rdrive.test", b"\0\1\xffuser-attribute")

            def metadata_workflows():
                cli("put", "test", payload, "metadata/file")
                captured = cli("metadata", "capture", payload)
                assert captured["attributes"][TAGS] == original_tags.hex()
                report = cli("metadata", "push", "test", payload, "metadata/file")
                assert report["changed"]
                copy = root / "metadata-copy"
                copy.write_bytes(content)
                cli("metadata", "pull", "test", "metadata/file", copy)
                assert get_xattr(copy, TAGS) == original_tags
                assert get_xattr(copy, "user.rdrive.test") == b"\0\1\xffuser-attribute"
                local = root / "sync"
                local.mkdir()
                local_file = local / "file"
                local_file.write_bytes(content)
                set_xattr(local_file, TAGS, tag_bytes("Initial\n1"))
                cli("put", "test", local_file, "metasync/file")
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                set_xattr(local_file, TAGS, tag_bytes("Changed locally\n2"))
                plan = cli("sync", "test", local, "metasync", "--metadata")
                assert any(action["direction"] == "push" for action in plan["metadata"]["actions"])
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                witness = root / "witness"
                witness.write_bytes(content)
                cli("metadata", "pull", "test", "metasync/file", witness)
                assert get_xattr(witness, TAGS) == tag_bytes("Changed locally\n2")
                set_xattr(witness, TAGS, tag_bytes("Changed remotely\n5"))
                cli("metadata", "push", "test", witness, "metasync/file")
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                assert get_xattr(local_file, TAGS) == tag_bytes("Changed remotely\n5")
                remove_xattr(local_file, TAGS)
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                cli("metadata", "pull", "test", "metasync/file", witness)
                assert TAGS not in list_xattrs(witness)
                preserved = tag_bytes("Keep across content replacement\n3")
                set_xattr(local_file, TAGS, preserved)
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                new_content = root / "remote-content-edit"
                new_content.write_bytes(content + b"new remote content")
                cli("put", "test", new_content, "metasync/file", "--overwrite")
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                assert local_file.read_bytes() == new_content.read_bytes()
                if TAGS not in list_xattrs(local_file) or get_xattr(local_file, TAGS) != preserved:
                    failures.append({"group": "content-sync metadata preservation", "error": "pulling a content-only remote edit discarded existing local Finder tags"})
                # An absent sidecar means metadata is unknown; an explicit empty
                # snapshot is a remote request to clear tags and takes priority.
                set_xattr(local_file, TAGS, tag_bytes("Prior tags before explicit clear\n6"))
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                explicit_clear = root / "remote-explicit-clear"
                explicit_clear.write_bytes(new_content.read_bytes() + b"v2")
                cli("put", "test", explicit_clear, "metasync/file", "--overwrite")
                cli("metadata", "push", "test", explicit_clear, "metasync/file")
                cli("sync", "test", local, "metasync", "--metadata", "--apply")
                if TAGS in list_xattrs(local_file):
                    failures.append({"group": "remote explicit metadata clear", "error": "preserved local tags overrode an explicit empty metadata snapshot on the new remote revision"})

            group("native Finder tags/user xattrs; metadata push/pull and metadata-only sync/clear", metadata_workflows)

            def parallel_workflows():
                jobs = [cli("put", "test", payload, f"parallel/{number}", "--queue") for number in range(3)]
                with AdvancedDav.counters_lock:
                    AdvancedDav.peak_puts = 0
                result = cli("queue", "run", options=("--jobs", "3"))
                results = result if isinstance(result, list) else result["results"]
                assert all(next(item for item in results if item["id"] == job["id"])["status"] == "done" for job in jobs)
                assert AdvancedDav.peak_puts >= 2, f"uploads did not overlap: peak {AdvancedDav.peak_puts}"
                for number in range(3):
                    assert (AdvancedDav.root / "parallel" / str(number)).read_bytes() == content

            group("queue --jobs3 overlaps independent real HTTP uploads and confirms all snapshots", parallel_workflows)

            def throttle_workflows():
                rate_payload = root / "rate.bin"
                rate_payload.write_bytes(bytes(range(256)) * 256)
                begin = time.monotonic()
                result = raw_cli("put", "test", rate_payload, "rate-limited", options=("--limit", "32768", "--progress", "json"))
                elapsed = time.monotonic() - begin
                assert result.returncode == 0, result.stderr.decode()
                assert elapsed >= 1.7, f"64KiB finished in {elapsed:.3f}s under32KiB/s"
                events = [json.loads(line) for line in result.stderr.splitlines()]
                assert events and all(event["type"] == "transfer_progress" for event in events)
                assert any(event["event"] == "finished" and event["aggregate"]["transferred"] == 65536 for event in events)
                assert b"fixture-password" not in result.stderr
                assert (AdvancedDav.root / "rate-limited").read_bytes() == rate_payload.read_bytes()

            group("aggregate payload limit and JSON progress use measured bytes without exposing credentials", throttle_workflows)

            def repository_workflows():
                assert cli("repo", "init", "test", "repo")["commit"]["sequence"] == 0
                transaction = cli("repo", "begin", "test", "repo")["id"]
                cli("repo", "mkdir", transaction, "folder")
                cli("repo", "put", transaction, payload, "folder/一.txt")
                second = root / "second"
                second.write_bytes(b"second file")
                cli("repo", "put", transaction, second, "second.bin")
                assert cli("repo", "ls", "test", "repo")["commit"]["entries"] == {}
                committed = cli("repo", "commit", transaction)
                assert len(committed["commit"]["entries"]) == 3
                assert cli("repo", "commit", transaction)["checksum"] == committed["checksum"]
                output = root / "repository-download"
                cli("repo", "get", "test", "folder/一.txt", output, "--root", "repo")
                assert output.read_bytes() == content
                assert get_xattr(output, TAGS) == original_tags
                assert get_xattr(output, "user.rdrive.test") == get_xattr(payload, "user.rdrive.test")
                a = cli("repo", "begin", "test", "repo", home="a")["id"]
                b = cli("repo", "begin", "test", "repo", home="b")["id"]
                cli("repo", "mkdir", a, "writer-A", home="a")
                cli("repo", "mkdir", b, "writer-B", home="b")
                AdvancedDav.publication_barrier = threading.Barrier(2)
                try:
                    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                        first = pool.submit(raw_cli, "repo", "commit", a, home="a")
                        second = pool.submit(raw_cli, "repo", "commit", b, home="b")
                        outcomes = [first.result(timeout=90), second.result(timeout=90)]
                finally:
                    AdvancedDav.publication_barrier = None
                assert sum(result.returncode == 0 for result in outcomes) == 1, [result.stderr.decode() for result in outcomes]
                final = cli("repo", "ls", "test", "repo")
                assert final["commit"]["sequence"] == 2
                assert ("writer-A" in final["commit"]["entries"]) != ("writer-B" in final["commit"]["entries"])
                # Kill a separate OS process after publication, before the
                # server acknowledges it. A new process must reconcile the
                # exact prepared transaction without publishing another slot.
                killed_tx = cli("repo", "begin", "test", "repo")["id"]
                cli("repo", "mkdir", killed_tx, "survived-process-kill")
                slot_path = "/dav/repo/.rdrive-repository/commits/00000000000000000003.json"
                destination = f"http://127.0.0.1:{server.server_port}{slot_path}"
                gate = AdvancedDav.arm_gate("ack_gate", destination)
                killed = subprocess.Popen([str(binary), "--json", "--progress", "off", "repo", "commit", killed_tx], env=environment(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                processes.append(killed)
                try:
                    assert gate["entered"].wait(timeout=30), "commit never reached the post-publication response gate"
                    published = json.loads((AdvancedDav.root / slot_path[5:]).read_bytes())
                    assert published["commit"]["transaction"] == killed_tx
                    killed.kill()
                    killed.communicate(timeout=10)
                    assert killed.returncode == -9, f"commit was not terminated by SIGKILL: {killed.returncode}"
                finally:
                    gate["release"].set()
                    with AdvancedDav.gate_lock:
                        AdvancedDav.ack_gate = None
                recovered = cli("repo", "commit", killed_tx)
                assert recovered["checksum"] == published["checksum"] and recovered["commit"]["sequence"] == 3
                after_recovery = cli("repo", "ls", "test", "repo")
                assert after_recovery["checksum"] == published["checksum"]
                assert "survived-process-kill" in after_recovery["commit"]["entries"]
                assert len(list((AdvancedDav.root / "repo/.rdrive-repository/commits").glob("*.json"))) == 3

            group("managed atomic multi-file visibility, replay, xattrs, two-writer conflict and SIGKILL after publication", repository_workflows)

            def provider_workflows():
                cli("provider", "domain-add", "advanced-managed", "test", "repo", "--repository")
                cli("provider", "domain-add", "advanced-raw", "test", "ordinary")
                group_root = root / "g"
                group_root.mkdir(mode=0o700)
                staging = group_root / "stage"
                staging.mkdir(mode=0o700)
                process = subprocess.Popen([str(binary), "--progress", "off", "provider", "serve", "--socket", str(group_root / "b.sock"), "--staging", str(staging), "--development-tcp"], env=environment(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                processes.append(process)
                descriptor_path = group_root / "bridge-endpoint.json"
                deadline = time.monotonic() + 15
                while not descriptor_path.exists():
                    assert process.poll() is None, "provider exited during startup"
                    assert time.monotonic() < deadline, "provider endpoint timed out"
                    time.sleep(0.02)
                descriptor = json.loads(descriptor_path.read_bytes())
                assert stat.S_IMODE(descriptor_path.stat().st_mode) == 0o600
                before = len(AdvancedDav.requests_seen)
                rejected = connect_provider(descriptor, dict(op="domains"), authenticated=False)
                assert rejected["error"]["code"] == "not_authenticated"
                assert len(AdvancedDav.requests_seen) == before
                domains = request(descriptor, op="domains")["domains"]
                assert any(domain["id"] == "advanced-managed" for domain in domains)
                before = sum(method == "GET" and "/objects/" in path for method, path in AdvancedDav.requests_seen)
                items = request(descriptor, op="list", domain="advanced-managed", parent_id="root")["items"]
                folder = next(item for item in items if item["name"] == "folder")
                child = request(descriptor, op="list", domain="advanced-managed", parent_id=folder["id"])["items"][0]
                request(descriptor, op="item", domain="advanced-managed", id=child["id"])
                assert sum(method == "GET" and "/objects/" in path for method, path in AdvancedDav.requests_seen) == before, "enumeration fetched content blobs"
                with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                    callbacks = [pool.submit(request, descriptor, op="list", domain="advanced-managed", parent_id="root") if number % 2 == 0 else pool.submit(request, descriptor, op="item", domain="advanced-managed", id=child["id"]) for number in range(16)]
                    for callback in callbacks:
                        assert callback.result(timeout=90)["ok"], "concurrent callback failed"
                fetched = request(descriptor, op="fetch", domain="advanced-managed", id=child["id"], base_content_version=child["content_version"], base_metadata_version=child["metadata_version"])
                downloaded = Path(fetched["content_path"])
                assert downloaded.parent == staging and downloaded.read_bytes() == content
                assert get_xattr(downloaded, TAGS) == original_tags
                collision_source = staging / ("collision-" + uuid.uuid4().hex)
                collision_source.write_bytes(b"must never replace the existing file")
                collision_source.chmod(0o600)
                before_collision = cli("repo", "ls", "test", "repo")
                collision = connect_provider(descriptor, dict(op="create", domain="advanced-managed", parent_id=folder["id"], name=child["name"], kind="file", client_item_id="fresh-existing-name", content_path=str(collision_source)))
                assert not collision["ok"] and collision["error"]["code"] == "version_conflict", collision
                after_collision = cli("repo", "ls", "test", "repo")
                assert after_collision["checksum"] == before_collision["checksum"], "same-name create mutated repository"
                assert after_collision["commit"]["entries"]["folder/一.txt"]["id"] == child["id"]
                create = dict(op="create", domain="advanced-managed", parent_id="root", name="created.txt", kind="file", client_item_id="advanced-create-1", tag_data_hex=tag_bytes("Provider\n2").hex())
                created = request(descriptor, **create)["item"]
                sequence = cli("repo", "ls", "test", "repo")["commit"]["sequence"]
                assert request(descriptor, **create)["item"]["id"] == created["id"]
                assert cli("repo", "ls", "test", "repo")["commit"]["sequence"] == sequence
                assert bytes.fromhex(created["tag_data_hex"]) == tag_bytes("Provider\n2")
                staged = staging / ("upload-" + uuid.uuid4().hex)
                staged.write_bytes(b"updated through provider")
                staged.chmod(0o600)
                modify = dict(op="modify", domain="advanced-managed", id=created["id"], request_id=uuid.uuid4().hex, content_path=str(staged), base_content_version=created["content_version"], base_metadata_version=created["metadata_version"])
                modified = request(descriptor, **modify)["item"]
                sequence = cli("repo", "ls", "test", "repo")["commit"]["sequence"]
                assert request(descriptor, **modify)["item"]["content_version"] == modified["content_version"]
                assert cli("repo", "ls", "test", "repo")["commit"]["sequence"] == sequence
                # tagData is an independent File Provider field: changing only
                # contents must not clear tags from the existing system item.
                if modified.get("tag_data_hex") != created.get("tag_data_hex"):
                    failures.append({"group": "provider content-only metadata preservation", "error": "a content-only modify cleared existing Finder tags"})
                rename = dict(op="modify", domain="advanced-managed", id=modified["id"], request_id=uuid.uuid4().hex, name="renamed.txt", base_content_version=modified["content_version"], base_metadata_version=modified["metadata_version"])
                renamed = request(descriptor, **rename)["item"]
                assert renamed["id"] == created["id"] and renamed["name"] == "renamed.txt"
                sequence = cli("repo", "ls", "test", "repo")["commit"]["sequence"]
                assert request(descriptor, **rename)["item"]["id"] == renamed["id"]
                assert cli("repo", "ls", "test", "repo")["commit"]["sequence"] == sequence
                clear = dict(op="modify", domain="advanced-managed", id=renamed["id"], request_id=uuid.uuid4().hex, tag_data_hex="", base_content_version=renamed["content_version"], base_metadata_version=renamed["metadata_version"])
                cleared = request(descriptor, **clear)["item"]
                assert cleared.get("tag_data_hex") is None
                deletion = dict(op="delete", domain="advanced-managed", id=cleared["id"], request_id=uuid.uuid4().hex, base_content_version=cleared["content_version"], base_metadata_version=cleared["metadata_version"])
                request(descriptor, **deletion)
                sequence = cli("repo", "ls", "test", "repo")["commit"]["sequence"]
                request(descriptor, **deletion)
                assert cli("repo", "ls", "test", "repo")["commit"]["sequence"] == sequence
                assert "renamed.txt" not in cli("repo", "ls", "test", "repo")["commit"]["entries"]
                # Simulate a crash between the successful immutable commit and
                # response journaling. The old delete must reconcile its own
                # transaction even after a new file appears at the same path.
                journal_path = Path(environment()["RDRIVE_HOME"]) / "provider" / (hashlib.sha256(b"advanced-managed").hexdigest() + ".json")
                journal = json.loads(journal_path.read_bytes())
                pending = [value for key, value in journal["requests"].items() if key.endswith(":" + deletion["request_id"])]
                assert len(pending) == 1 and pending[0]["transaction"] and pending[0]["response"] is not None
                pending[0]["response"] = None
                journal_path.write_text(json.dumps(journal), encoding="utf-8")
                replacement_source = root / "delete-replacement"
                replacement_source.write_bytes(b"a distinct replacement must survive delete replay")
                replacement_tx = cli("repo", "begin", "test", "repo", home="b")["id"]
                cli("repo", "put", replacement_tx, replacement_source, "renamed.txt", home="b")
                replacement_snapshot = cli("repo", "commit", replacement_tx, home="b")
                replacement_entry = replacement_snapshot["commit"]["entries"]["renamed.txt"]
                assert replacement_entry["id"] != cleared["id"]
                request(descriptor, **deletion)
                replayed_snapshot = cli("repo", "ls", "test", "repo")
                assert replayed_snapshot["checksum"] == replacement_snapshot["checksum"], "old delete replay published another mutation"
                assert replayed_snapshot["commit"]["entries"]["renamed.txt"] == replacement_entry
                replacement_output = root / "delete-replacement-output"
                cli("repo", "get", "test", "renamed.txt", replacement_output, "--root", "repo")
                assert replacement_output.read_bytes() == replacement_source.read_bytes()

                def snapshot_interleaving(operation):
                    current = request(descriptor, op="item", domain="advanced-managed", id=child["id"])["item"]
                    captured = cli("repo", "ls", "test", "repo", home="b")
                    old_entry = captured["commit"]["entries"]["folder/一.txt"]
                    old_bytes = (AdvancedDav.root / "repo/.rdrive-repository/objects" / old_entry["blob"][:2] / old_entry["blob"]).read_bytes()
                    competitor = root / ("competing-" + operation)
                    competitor.write_bytes(("competing snapshot B during " + operation).encode())
                    transaction_b = cli("repo", "begin", "test", "repo", home="b")["id"]
                    cli("repo", "put", transaction_b, competitor, "folder/一.txt", home="b")
                    message = dict(op=operation, domain="advanced-managed", id=current["id"], base_content_version=current["content_version"], base_metadata_version=current["metadata_version"])
                    if operation == "modify":
                        stale_source = staging / ("stale-" + uuid.uuid4().hex)
                        stale_source.write_bytes(b"stale snapshot A edit must not overwrite B")
                        stale_source.chmod(0o600)
                        message.update(request_id=uuid.uuid4().hex, content_path=str(stale_source))
                    slot = f"/dav/repo/.rdrive-repository/commits/{captured['commit']['sequence']:020}.json"
                    gate = AdvancedDav.arm_gate("read_gate", slot)
                    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                        result = pool.submit(connect_provider, descriptor, message)
                        try:
                            assert gate["entered"].wait(timeout=30), f"{operation} never reached the pinned snapshot gate"
                            published_b = cli("repo", "commit", transaction_b, home="b")
                        finally:
                            gate["release"].set()
                            with AdvancedDav.gate_lock:
                                AdvancedDav.read_gate = None
                        response = result.result(timeout=90)
                    if operation == "fetch":
                        assert response["ok"], response
                        assert response["item"]["content_version"] == current["content_version"]
                        assert Path(response["content_path"]).read_bytes() == old_bytes, "snapshot A fetch returned snapshot B bytes"
                    else:
                        assert not response["ok"] and response["error"]["code"] == "version_conflict", response
                    observed_b = cli("repo", "ls", "test", "repo")
                    assert observed_b["checksum"] == published_b["checksum"], f"{operation} changed the competing snapshot"
                    assert observed_b["commit"]["entries"]["folder/一.txt"]["blob"] == hashlib.sha256(competitor.read_bytes()).hexdigest()

                snapshot_interleaving("fetch")
                snapshot_interleaving("modify")
                cli("put", "test", payload, "ordinary/a")
                first = request(descriptor, op="list", domain="advanced-raw", parent_id="root")["items"]
                cli("put", "test", payload, "ordinary/b")
                second = request(descriptor, op="list", domain="advanced-raw", parent_id="root")["items"]
                assert len(second) == 2 and len({item["id"] for item in second}) == 2
                assert next(item for item in second if item["name"] == "a")["id"] == first[0]["id"]
                process.terminate()
                stdout, stderr = process.communicate(timeout=10)
                assert b"fixture-password" not in stdout + stderr and descriptor["token"].encode() not in stdout + stderr

            group("authenticated provider IPC: lazy fetch, concurrent callbacks, CRUD/replay, collision/delete recovery and snapshot races", provider_workflows)
            print(json.dumps({"passed": checks, "failures": failures}, ensure_ascii=False, indent=2))
            if failures:
                raise SystemExit(1)
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
