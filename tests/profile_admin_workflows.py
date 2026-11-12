#!/usr/bin/env python3
"""Profile administration under real provider concurrency, using disposable state.

The CLI, authenticated loopback DAV and Unix provider bridge are real processes.
All profiles, payloads, credential sentinels and coordination files are synthetic;
the user's application, mounted domains and configured accounts are untouched.
"""
import argparse
import concurrent.futures
import copy
import datetime
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import posixpath
import signal
import socket
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit, urlunsplit
import uuid

from cli_workflows import Dav
from index_workflows import read_provider_journal, report_binary_path


SECRET = "synthetic-profile-secret-must-never-be-persisted-or-transferred"
PASSWORD_ENV = "RDRIVE_PROFILE_ADMIN_PASSWORD"


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def wait_for(check, label, seconds=12):
    deadline = time.monotonic() + seconds
    while True:
        value = check()
        if value:
            return value
        assert time.monotonic() < deadline, label
        time.sleep(.025)


def request_id():
    return f"{int(time.time())}:{uuid.uuid4()}"


class ProfileDav(Dav):
    guard = threading.Lock()
    gates = {}
    observed = []
    online = True

    @classmethod
    def gate(cls, remote):
        gate = dict(entered=threading.Event(), release=threading.Event())
        with cls.guard:
            key = "/dav/" + remote.strip("/")
            assert key not in cls.gates
            cls.gates[key] = gate
        return gate

    def resource(self, value=None):
        parts = urlsplit(value or self.path)
        path = posixpath.normpath(parts.path)
        if path == "/dav":
            path += "/"
        return super().resource(urlunsplit((parts.scheme, parts.netloc, path,
                                           parts.query, parts.fragment)))

    def handle_dav(self):
        with self.guard:
            type(self).observed.append((self.command, self.path))
            gate = self.gates.pop(self.path.rstrip("/"), None) if (
                self.command == "PROPFIND" and self.headers.get("Depth") == "1") else None
        if gate is not None:
            gate["entered"].set()
            assert gate["release"].wait(25), "synthetic DAV metadata gate timed out"
        if not self.online:
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
    def __init__(self, binary, root, url):
        self.binary = binary
        self.root = root
        self.home = root / "state"
        self.ipc = root / "ipc"
        self.ipc.mkdir(mode=0o700)
        self.socket = self.ipc / "bridge.sock"
        self.staging = self.ipc / "Transfers"
        self.url = url
        self.environment = dict(os.environ, **{PASSWORD_ENV: "fixture-password",
                                               "RDRIVE_PROFILE_SECRET_SENTINEL": SECRET})
        self.environment.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        self.base = [str(binary), "--home", str(self.home), "--json", "--progress", "off"]
        self.process = None
        self.log = (root / "broker.log").open("w+")

    def command(self, *arguments):
        return self.base + list(map(str, arguments))

    def spawn(self, *arguments):
        return subprocess.Popen(self.command(*arguments), env=self.environment,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def finish(self, process, *, success=True, seconds=15):
        stdout, stderr = process.communicate(timeout=seconds)
        assert (process.returncode == 0) == success, (process.args, process.returncode, stdout, stderr)
        return json.loads(stdout) if success else (stdout, stderr)

    def cli(self, *arguments, success=True, seconds=15):
        return self.finish(self.spawn(*arguments), success=success, seconds=seconds)

    def add(self, name, *, source=None, identity=None):
        arguments = ["profile", "add", name, "--protocol", "webdav", "--url", source or self.url,
                     "--username", "user", "--password-env", PASSWORD_ENV, "--timeout", "10"]
        if (source or self.url).startswith("http:"):
            arguments += ["--allow-http"]
        if identity is not None:
            arguments += ["--request-id", identity]
        return self.cli(*arguments)

    def config(self):
        return json.loads((self.home / "config.json").read_text())

    def descriptor(self):
        return json.loads((self.home / "provider-admin.json").read_text())

    def start(self):
        self.process = self.spawn("provider", "serve", "--socket", self.socket,
                                  "--staging", self.staging, "--development-tcp")

        def ready():
            assert self.process.poll() is None, "provider exited: " + self.process.communicate()[1]
            try:
                descriptor = self.descriptor()
                assert descriptor["version"] == 1
                assert Path(descriptor["home"]) == self.home.resolve()
                assert Path(descriptor["socket"]) == self.socket
                uuid.UUID(descriptor["instance_id"])
                return self.rpc(op="domains").get("ok")
            except (OSError, ValueError, KeyError, AssertionError):
                return False
        wait_for(ready, "provider admin discovery and bridge startup")

    def stop(self, *, crash=False):
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGKILL if crash else signal.SIGINT)
            self.process.communicate(timeout=15)
        self.process = None

    def rpc(self, **message):
        endpoint = json.loads((self.ipc / "bridge-endpoint.json").read_text())
        message.update(v=1, auth_token=endpoint["token"])
        message.setdefault("request_id", str(uuid.uuid4()))
        with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=15) as client:
            client.sendall(json.dumps(message).encode() + b"\n")
            with client.makefile("rb") as reader:
                while True:
                    line = reader.readline()
                    assert line, "provider disconnected before its response"
                    result = json.loads(line)
                    if "progress" not in result:
                        return result

    def admin_message(self, change, identity=None, **overrides):
        descriptor = self.descriptor()
        message = dict(v=1, op="profile_admin", home=descriptor["home"],
                       instance_id=descriptor["instance_id"],
                       command=dict(request_id=identity or request_id(), change=change))
        message.update(overrides)
        assert SECRET not in json.dumps(message)
        return message

    def admin_stream(self, message):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(15)
        client.connect(str(self.socket))
        client.sendall(json.dumps(message).encode() + b"\n")
        return client

    def admin(self, message):
        with self.admin_stream(message) as client:
            with client.makefile("rb") as reader:
                while True:
                    line = reader.readline()
                    assert line, "profile admin disconnected before response"
                    result = json.loads(line)
                    if "progress" not in result:
                        break
        assert result["home"] == str(self.home.resolve()), result
        assert result["instance_id"] == self.descriptor()["instance_id"], result
        assert result["request_id"] == message["command"]["request_id"], result
        return result

    def close(self):
        self.stop()
        self.log.close()


def profile(url="https://profiles.invalid/dav/"):
    return dict(protocol="webdav", url=url, username="user",
                password_env="RDRIVE_PROFILE_SECRET_SENTINEL", allow_http=False,
                timeout_secs=10, smb_encryption=True, keychain_internet=False,
                nextcloud_upload_url=None)


def add_change(name):
    return dict(action="add", name=name, profile=profile())


def stable_config(config):
    value = copy.deepcopy(config)
    value.pop("profile_admin_receipts", None)
    value.pop("profiles", None)
    return value


def protected_files(home):
    """Payload and namespace bytes which profile administration cannot discard."""
    result = {}
    for name in ("cache", "spool", "smart", "transactions", "transfers"):
        directory = home / name
        if directory.exists():
            for path in directory.rglob("*"):
                if path.is_file() and not path.name.endswith((".lock", ".sqlite3-shm", ".sqlite3-wal")):
                    result[path.relative_to(home).as_posix()] = path.read_bytes()
    domain_file = home / "provider-domains.json"
    if domain_file.exists():
        result["provider-domains.json"] = domain_file.read_bytes()
    return result


def run(binary):
    passed = []

    def check(name):
        passed.append(name)
        print("PASS " + name, flush=True)

    with tempfile.TemporaryDirectory(prefix="rdpa-", dir="/private/tmp" if Path("/private/tmp").exists() else None) as temporary:
        root = Path(temporary).resolve()
        remote = root / "dav"
        remote.mkdir(mode=0o700)
        for name in ("foreground", "foreground-cancel", "background", "smart"):
            (remote / name).mkdir()
            (remote / name / "doc.txt").write_bytes(b"fixture namespace and payload\n")
        (remote / "cached.txt").write_bytes(b"cached user bytes\n")
        (remote / "interrupted.bin").write_bytes(b"retained transfer prefix\n" * 8192)
        (remote / "managed").mkdir()
        ProfileDav.root = remote
        ProfileDav.locks = {}
        ProfileDav.gates = {}
        ProfileDav.requests_seen = []
        ProfileDav.interrupt_once = set()
        ProfileDav.range_requests = []
        ProfileDav.observed = []
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ProfileDav)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        broker = Broker(binary, root, f"http://127.0.0.1:{server.server_port}/dav/")
        gates = []
        children = []
        try:
            broker.add("pan123")
            broker.cli("cache", "config", "--max-bytes", "8192")
            broker.cli("filter", "config", "--profile", "pan123", "--exclude", "*.scratch")
            original = broker.config()
            original_pan = original["profiles"]["pan123"]
            requests = len(ProfileDav.observed)
            direct_id = request_id()
            added = broker.add("direct", source="https://profiles.invalid/dav/", identity=direct_id)
            assert added["profile"] == "direct" and added["saved"], added
            assert broker.add("direct", source="https://profiles.invalid/dav/", identity=direct_id) == added
            broker.cli("profile", "remove", "direct")
            assert broker.config()["profiles"] == original["profiles"]
            assert stable_config(broker.config()) == stable_config(original)
            assert len(ProfileDav.observed) == requests, "offline profile administration contacted DAV"
            check("direct add/remove and exact request retry preserve the original home, profile and non-profile settings without network access")

            # Each guard has its own profile, so an earlier upload guard cannot
            # accidentally hide a missing cache or domain guard.
            # An absent domain registry synthesizes a root for every profile.
            # Establish an explicitly empty registry before creating guard-only
            # profiles, so domain-add cannot register those roots implicitly.
            broker.cli("provider", "domain-remove", "pan123")
            assert json.loads((broker.home / "provider-domains.json").read_text()) == []
            guarded_names = ("queued", "cached", "domain-only", "smart-retained", "checkpoint", "transaction")
            for name in guarded_names:
                broker.add(name)
            source = root / "queued-source"
            source.write_bytes(b"durable queued upload\n")
            queued = broker.cli("put", "queued", source, "queued.txt", "--queue")
            source.write_bytes(b"later unrelated edit\n")
            snapshot = broker.home / "spool" / queued["snapshot"]
            broker.cli("cache", "fetch", "cached", "cached.txt", "--pin")
            broker.cli("provider", "domain-add", "foreground", "domain-only", "foreground")
            broker.cli("provider", "domain-add", "foreground-cancel", "domain-only", "foreground-cancel")
            broker.cli("smart", "setup", "retained", "smart-retained", "smart")
            broker.cli("smart", "run", "--staging", root / "initial-staging")
            assert any(record["path"] == "doc.txt" for record in read_provider_journal(broker.home, "retained")["items"].values())
            broker.cli("smart", "pause")
            ProfileDav.interrupt_once.add("/dav/interrupted.bin")
            broker.cli("get", "checkpoint", "interrupted.bin", root / "unfinished-download", "--resume", success=False)
            transfer = next(value for value in broker.cli("transfers", "list") if value.get("profile") == "checkpoint")
            checkpoint = broker.home / "transfers" / (transfer["id"] + ".json")
            partial = Path(json.loads(checkpoint.read_text())["directory"]) / "partial"
            partial_bytes = partial.read_bytes()
            assert partial_bytes and not (root / "unfinished-download").exists()
            broker.cli("repo", "init", "transaction", "managed")
            transaction = broker.cli("repo", "begin", "transaction", "managed")["id"]
            broker.cli("repo", "put", transaction, source, "pending.txt")
            before_guards = protected_files(broker.home)
            guard_messages = {"queued": "unfinished uploads", "cached": "cached files",
                              "domain-only": "Finder domain", "smart-retained": "Finder domain",
                              "checkpoint": "retained transfers", "transaction": "repository transactions"}
            for name in guarded_names:
                _, stderr = broker.cli("profile", "remove", name, success=False)
                assert guard_messages[name] in stderr, (name, stderr)
                assert protected_files(broker.home) == before_guards, name
                assert name in broker.cli("profile", "list")
            assert snapshot.read_bytes() == b"durable queued upload\n"
            assert partial.read_bytes() == partial_bytes
            check("separate upload, cache, domain, download-checkpoint and open-transaction guards retain every payload and profile")

            broker.add("expendable")
            broker.start()
            descriptor = broker.descriptor()
            descriptor_bytes = (broker.home / "provider-admin.json").read_bytes()
            alternate = root / "other-ipc"
            alternate.mkdir(mode=0o700)
            duplicate = broker.spawn("provider", "serve", "--socket", alternate / "bridge.sock",
                                     "--staging", alternate / "Transfers")
            children.append(duplicate)
            _, stderr = broker.finish(duplicate, success=False, seconds=10)
            assert "already administering" in stderr, stderr
            assert (broker.home / "provider-admin.json").read_bytes() == descriptor_bytes
            assert broker.rpc(op="domains")["ok"]
            check("a second broker cannot claim the same state home or replace the active discovery record")

            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                gate = ProfileDav.gate("foreground")
                gates.append(gate)
                listing = pool.submit(broker.rpc, op="list", domain="foreground")
                assert gate["entered"].wait(10), "foreground DAV request did not acquire Store"
                pending = [broker.spawn("profile", "add", name, "--protocol", "webdav", "--url",
                                        "https://profiles.invalid/dav/", "--password-env",
                                        "RDRIVE_PROFILE_SECRET_SENTINEL", "--wait-timeout", "15")
                           for name in ("parallel-a", "parallel-b")]
                pending.append(broker.spawn("profile", "remove", "expendable", "--wait-timeout", "15"))
                children.extend(pending)
                time.sleep(.2)
                assert all(child.poll() is None for child in pending), "admin bypassed the active foreground writer"
                started = time.monotonic()
                listed = broker.cli("profile", "list", seconds=3)
                assert time.monotonic() - started < 3 and "pan123" in listed
                assert not {"parallel-a", "parallel-b"} & set(listed)
                gate["release"].set()
                assert listing.result(timeout=10)["ok"]
                for child in pending:
                    broker.finish(child)
            config = broker.config()
            assert {"parallel-a", "parallel-b", "pan123"} <= set(config["profiles"])
            assert "expendable" not in config["profiles"]
            assert config["profiles"]["pan123"] == original_pan
            assert stable_config(config) == stable_config(original)
            assert protected_files(broker.home) == before_guards
            check("independent concurrent add/add/remove requests queue behind a real foreground DAV writer without losing updates; profile list stays responsive")
            for name in guarded_names:
                _, stderr = broker.cli("profile", "remove", name, "--wait-timeout", "5", success=False)
                assert guard_messages[name] in stderr, (name, stderr)
                assert name in broker.config()["profiles"]
            assert protected_files(broker.home) == before_guards
            assert partial.read_bytes() == partial_bytes
            check("the active broker applies the same independent data-protection guards as direct local administration")

            # A disconnect while waiting must be observed before any config
            # mutation. Releasing an unrelated writer cannot resurrect it.
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                gate = ProfileDav.gate("foreground-cancel")
                gates.append(gate)
                listing = pool.submit(broker.rpc, op="list", domain="foreground-cancel")
                assert gate["entered"].wait(10)
                cancellation_started = time.monotonic()
                assert not listing.done(), listing.result()
                with (broker.home / "client.lock").open("rb") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        pass
                    else:
                        raise AssertionError("gated foreground DAV request does not hold Store")
                cancelled = broker.admin_message(add_change("cancelled-before-commit"))
                client = broker.admin_stream(cancelled)
                client.close()
                timed = broker.spawn("profile", "add", "timed-out-before-commit", "--protocol", "webdav",
                                     "--url", "https://profiles.invalid/dav/", "--wait-timeout", "1")
                children.append(timed)
                timed_stdout, stderr = timed.communicate(timeout=5)
                assert broker.process.poll() is None, (
                    "disconnected admin client terminated the shared broker",
                    broker.process.returncode, broker.process.stderr.read())
                assert timed.returncode != 0, dict(
                    elapsed=time.monotonic() - cancellation_started,
                    stdout=timed_stdout, stderr=stderr,
                    listing_done=listing.done(),
                    listing_result=(repr(listing.exception()) if listing.exception() else listing.result()) if listing.done() else None,
                    broker_exit=broker.process.poll(),
                    broker_stderr=broker.process.stderr.read() if broker.process.poll() is not None else None,
                    domains=json.loads((broker.home / "provider-domains.json").read_text()))
                assert any(word in stderr.lower() for word in ("timeout", "timed out", "deadline")), stderr
                assert not {"cancelled-before-commit", "timed-out-before-commit"} & set(broker.config()["profiles"])
                gate["release"].set()
                assert listing.result(timeout=10)["ok"]
            barrier = broker.admin(broker.admin_message(add_change("after-cancel-barrier")))
            assert barrier["ok"], barrier
            assert not {"cancelled-before-commit", "timed-out-before-commit"} & set(broker.config()["profiles"])
            check("disconnected and timed-out queued operations are cancelled before commit and cannot appear after the writer releases")

            # Observe committed config from another process, then deliberately
            # discard its reply. Retrying the exact identity resolves ambiguity.
            message = broker.admin_message(add_change("reply-lost"))
            client = broker.admin_stream(message)
            wait_for(lambda: "reply-lost" in broker.config()["profiles"], "admin config publication")
            client.close()
            first = broker.admin(message)
            assert first["ok"] and first["result"]["profile"] == "reply-lost", first
            assert broker.admin(message) == first
            conflict = copy.deepcopy(message)
            conflict["command"]["change"]["name"] = "different-body"
            rejected = broker.admin(conflict)
            assert not rejected["ok"] and "different-body" not in broker.config()["profiles"], rejected
            broker.stop(crash=True)
            broker.start()
            replay = broker.admin_message(message["command"]["change"], message["command"]["request_id"])
            replayed = broker.admin(replay)
            assert replayed["ok"] and replayed["result"] == first["result"], replayed
            check("lost acknowledgements replay the same durable result after broker crash/restart, while reused identities with changed arguments fail")

            wrong_home = broker.admin_message(add_change("wrong-home"), home=str(root / "other-home"))
            wrong_instance = broker.admin_message(add_change("wrong-instance"), instance_id=str(uuid.uuid4()))
            for invalid in (wrong_home, wrong_instance):
                result = broker.admin(invalid)
                assert not result["ok"], result
            tcp_request = broker.admin_message(add_change("tcp-forbidden"))
            assert not broker.rpc(**tcp_request)["ok"], "TCP transport accepted profile administration"
            assert not {"wrong-home", "wrong-instance", "tcp-forbidden"} & set(broker.config()["profiles"])
            assert not (root / "other-home").exists()
            check("home and broker-instance bindings reject requests to the wrong state, and profile administration is unavailable over TCP")

            foreign_root = root / "foreign"
            foreign_root.mkdir(mode=0o700)
            foreign = Broker(binary, foreign_root, broker.url)
            discovery_path = broker.home / "provider-admin.json"
            actual_discovery = discovery_path.read_bytes()
            try:
                foreign.add("foreign-original", source="https://profiles.invalid/dav/")
                foreign.start()
                redirected = foreign.descriptor()
                redirected["home"] = str(broker.home.resolve())
                discovery_path.write_text(json.dumps(redirected))
                discovery_path.chmod(0o600)
                unchanged = [(candidate.home / "config.json").read_bytes() for candidate in (broker, foreign)]
                broker.cli("profile", "add", "cross-home", "--protocol", "webdav", "--url",
                           "https://profiles.invalid/dav/", "--wait-timeout", "2", success=False, seconds=5)
                assert [(candidate.home / "config.json").read_bytes() for candidate in (broker, foreign)] == unchanged
            finally:
                discovery_path.write_bytes(actual_discovery)
                discovery_path.chmod(0o600)
                foreign.close()
            check("a descriptor redirected to another live broker cannot mutate either home or silently fall back locally")

            broker.stop()
            # The foreground gate above must queue. Background metadata is
            # different: administration may preempt the read-only attempt.
            broker.cli("smart", "setup", "background", "pan123", "background")
            broker.cli("smart", "resume")
            gate = ProfileDav.gate("background")
            gates.append(gate)
            before_background = protected_files(broker.home)
            broker.start()
            assert gate["entered"].wait(10), "background Smart directory scan did not start"
            namespace_before = read_provider_journal(broker.home, "retained")["items"]
            attempt_before = read_provider_journal(broker.home, "background")["directory_attempts"]["root"]
            started = time.monotonic()
            broker.cli("profile", "add", "during-background", "--protocol", "webdav", "--url",
                       "https://profiles.invalid/dav/", "--wait-timeout", "5", seconds=8)
            assert not gate["release"].is_set() and time.monotonic() - started < 5
            assert broker.config()["profiles"]["pan123"] == original_pan
            assert protected_files(broker.home) == before_background
            assert read_provider_journal(broker.home, "retained")["items"] == namespace_before
            attempt_after = read_provider_journal(broker.home, "background")["directory_attempts"]["root"]
            assert attempt_after["failures"] == attempt_before["failures"]
            gate["release"].set()
            wait_for(lambda: read_provider_journal(broker.home, "background")["directory_scans"].get("root"),
                     "preempted metadata scan retried after admin commit")
            assert broker.rpc(op="list", domain="background")["ok"]
            check("profile add preempts a delayed background metadata scan before DAV releases, preserving existing namespace and durable edits")

            broker.stop()
            # Queue a real retained Smart mutation while synchronization is
            # paused, then remove only its domain registration. An orphan queue
            # still protects the profile and its retained bytes.
            broker.cli("smart", "pause")
            broker.start()
            staged = broker.staging / "retained-content"
            staged.write_bytes(b"retained offline Smart edit\n")
            retained = broker.rpc(op="create", domain="retained", parent_id="root", name="offline.txt",
                                  client_item_id=str(uuid.uuid4()), content_path=str(staged))
            assert not retained["ok"], retained
            broker.stop()
            broker.cli("provider", "domain-remove", "retained")
            retained_before = protected_files(broker.home)
            _, stderr = broker.cli("profile", "remove", "smart-retained", success=False)
            assert "unknown retained Smart queue" in stderr, stderr
            assert protected_files(broker.home) == retained_before
            assert any(b"retained offline Smart edit\n" == data for data in retained_before.values())
            check("a retained Smart edit protects its profile and content even after its explicit domain registration is removed")
            broker.cli("smart", "setup", "retained", "smart-retained", "smart")

            # No reachable broker + external Store owner must fail within the
            # requested deadline without initializing a substitute state home.
            before_lock = {name: (broker.home / name).read_bytes() for name in ("config.json", "state.json")}
            with (broker.home / "client.lock").open("rb") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                started = time.monotonic()
                broker.cli("profile", "add", "external-lock", "--protocol", "webdav", "--url",
                           "https://profiles.invalid/dav/", "--wait-timeout", "1", success=False, seconds=5)
                assert time.monotonic() - started < 5
            assert {name: (broker.home / name).read_bytes() for name in before_lock} == before_lock
            assert "external-lock" not in broker.config()["profiles"]
            check("an unavailable broker and independently held Store lock produce a bounded failure without changing config or user state")

            # A stopped broker leaves either no record or a stale record. Both
            # paths may fall back only after the home administration lock is free.
            stale = descriptor.copy()
            stale["socket"] = str(root / "no-longer-listening.sock")
            descriptor_path = broker.home / "provider-admin.json"
            descriptor_path.write_text(json.dumps(stale))
            descriptor_path.chmod(0o600)
            broker.add("stale-local", source="https://profiles.invalid/dav/")
            broker.cli("profile", "remove", "stale-local")
            descriptor_path.unlink(missing_ok=True)
            broker.add("no-broker", source="https://profiles.invalid/dav/")
            broker.cli("profile", "remove", "no-broker")
            assert broker.config()["profiles"]["pan123"] == original_pan
            check("stale discovery and absent discovery permit safe local administration after the broker has released its home lock")

            # Discovery is not an instruction to mutate another directory.
            invalid = stale.copy()
            invalid["home"] = str(root / "foreign-home")
            descriptor_path.write_text(json.dumps(invalid))
            descriptor_path.chmod(0o600)
            before_invalid = (broker.home / "config.json").read_bytes()
            broker.cli("profile", "add", "invalid-discovery", "--protocol", "webdav", "--url",
                       "https://profiles.invalid/dav/", success=False)
            assert (broker.home / "config.json").read_bytes() == before_invalid
            assert not (root / "foreign-home").exists()
            descriptor_path.unlink()
            check("mismatched discovery fails closed without changing either home")

            for path in broker.home.rglob("*"):
                if path.is_file() and path.stat().st_size < 16 * 1024 * 1024:
                    assert SECRET.encode() not in path.read_bytes(), path
            assert snapshot.read_bytes() == b"durable queued upload\n"
            assert partial.read_bytes() == partial_bytes
            assert broker.config()["profiles"]["pan123"] == original_pan
            assert stable_config(broker.config()) == stable_config(original)
            check("credential values never enter durable profile/admin state, and the existing profile, settings and pending upload remain intact")
        finally:
            for gate in gates:
                gate["release"].set()
            for child in children:
                if child.poll() is None:
                    child.kill()
                    child.communicate(timeout=5)
            broker.close()
            server.shutdown()
            server.server_close()
    return passed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", type=Path, default=Path("target/debug/rdrive"))
    parser.add_argument("--report", type=Path)
    options = parser.parse_args()
    binary = options.binary.resolve()
    before = digest(binary)
    passed = run(binary)
    assert digest(binary) == before, "CLI executable changed during profile administration proof"
    report = dict(schema=1, generated=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  binary=report_binary_path(binary), binary_sha256=before, status="passed",
                  passed=passed, count=len(passed),
                  scope="Disposable state, Unix broker and authenticated loopback DAV only; no application, mounted domain or configured account used")
    if options.report:
        options.report.parent.mkdir(parents=True, exist_ok=True)
        options.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
