#!/usr/bin/env python3
"""Unified status through real CLI processes and disposable authenticated DAV.

Only explicit temporary state homes and synthetic credentials are used. This
workflow does not register Finder domains, install services, mount an APFS image,
read Keychain items, or contact a user's configured server. Fault injections
modify only stopped, disposable fixtures and never a live RDrive state home.
"""
import argparse
import contextlib
import datetime
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import socketserver
import sqlite3
import stat
import struct
import sys
import subprocess
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav
from mcp_workflows import Client as McpClient

ROOT = Path(__file__).resolve().parents[1]
PASSWORD_ENV = "RDRIVE_STATUS_TEST_PASSWORD"
SECTIONS = ("configuration", "service", "connections", "index", "cache", "history",
            "checkpoints", "conflicts", "queue", "transfers")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def private_write(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    raw = value if isinstance(value, bytes) else json.dumps(value).encode()
    with path.open("wb") as target:
        os.fchmod(target.fileno(), 0o600)
        target.write(raw)


def inventory(root):
    if not root.exists():
        return None
    result = {}
    for path in sorted(root.rglob("*")):
        metadata = path.lstat()
        mode = stat.S_IMODE(metadata.st_mode)
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            result[relative] = ("symlink", mode, os.readlink(path))
        elif path.is_file():
            result[relative] = ("file", mode, sha(path.read_bytes()))
        elif path.is_dir():
            result[relative] = ("directory", mode)
        else:
            result[relative] = ("special", mode)
    return result


def envelope(value, name):
    section = value[name]
    assert section["status"] in ("ok", "missing", "error", "partial", "unavailable", "busy"), (name, section)
    assert all(key in section for key in ("scope", "source", "observed_at", "data")), (name, section)
    return section


def validate_report(value):
    assert value["schema_version"] == 1 and value["observed_at"] and value["state_directory"], value
    for name in SECTIONS:
        envelope(value, name)
    return value


def data_rows(value):
    """Find the public per-profile rows without assuming list/map key order."""
    if isinstance(value, dict):
        yield value
        for nested in value.values():
            yield from data_rows(nested)
    elif isinstance(value, list):
        for nested in value:
            yield from data_rows(nested)


def connection(value, profile):
    rows = [row for row in data_rows(envelope(value, "connections")["data"])
            if row.get("profile", row.get("name")) == profile and "state" in row]
    assert len(rows) == 1, (profile, value["connections"])
    return rows[0]


def process_identity(pid):
    result = subprocess.run(["ps", "-p", str(pid), "-o", "pid=,ppid=,pgid=,uid=,lstart=,comm="],
                            capture_output=True, text=True, timeout=5)
    if result.returncode or not result.stdout.strip():
        return None
    parts = result.stdout.strip().split(None, 9)
    assert len(parts) == 10, result.stdout
    executable = parts[9]
    linux_executable = Path(f"/proc/{pid}/exe")
    if linux_executable.is_symlink():
        try:
            executable = str(linux_executable.resolve(strict=True))
        except OSError:
            return None
    return {"pid": int(parts[0]), "parent": int(parts[1]), "group": int(parts[2]), "uid": int(parts[3]),
            "started": " ".join(parts[4:9]), "executable": executable}


def owned_probe_children(parent, binary):
    result = subprocess.run(["pgrep", "-P", str(parent)], capture_output=True, text=True, timeout=5)
    assert result.returncode in (0, 1), result.stderr
    children = []
    for value in result.stdout.split():
        item = process_identity(int(value))
        if item is None:
            continue
        assert item["parent"] == parent and item["uid"] == os.geteuid(), item
        assert Path(item["executable"]).resolve() == binary and item["group"] == item["pid"], item
        children.append(item)
    return children


def same_process(expected):
    actual = process_identity(expected["pid"])
    return actual is not None and all(actual[name] == expected[name]
                                      for name in ["pid", "group", "uid", "started", "executable"])


class StatusDav(Dav):
    guard = threading.Lock()
    observed = []
    response_status = None
    delayed = False
    delay_entered = threading.Event()
    release_delay = threading.Event()
    delay_seconds = 0
    active_requests = 0
    peak_requests = 0
    idle = threading.Event()
    require_pair = False
    pair_seen = threading.Event()
    offline_text = b"Authorization: Bearer synthetic-server-secret-must-never-appear"

    def handle_dav(self):
        with self.guard:
            type(self).observed.append((self.command, self.path, self.headers.get("Depth")))
            delayed, response, delay = self.delayed, self.response_status, self.delay_seconds
            type(self).active_requests += 1
            type(self).idle.clear()
            type(self).peak_requests = max(self.peak_requests, self.active_requests)
            require_pair = self.require_pair
            if require_pair and self.active_requests >= 2:
                type(self).pair_seen.set()
        try:
            if require_pair:
                type(self).pair_seen.wait(10)
            if delayed:
                type(self).delay_entered.set()
                type(self).release_delay.wait(20)
            if delay:
                time.sleep(delay)
            if response is not None:
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                return self.reply(response, self.offline_text)
            return super().handle_dav()
        finally:
            with self.guard:
                type(self).active_requests -= 1
                if not self.active_requests:
                    type(self).idle.set()

    def reply(self, status, body=b"", **headers):
        if status == 207:
            body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
        try:
            return super().reply(status, body, **headers)
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    @classmethod
    def requests(cls):
        with cls.guard:
            return list(cls.observed)


class StatusServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        # The cancellation/timeout cases deliberately close their owned clients.
        if not isinstance(sys.exc_info()[1], (BrokenPipeError, ConnectionResetError)):
            super().handle_error(request, client_address)


class ResetPeer(socketserver.BaseRequestHandler):
    def handle(self):
        # A real TCP reset gives deterministic transport failure even on hosts
        # whose local firewall silently drops connections to closed ports.
        self.request.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))


class ResetServer(socketserver.ThreadingTCPServer):
    daemon_threads = True


class Fixture:
    def __init__(self, binary, root, url):
        self.binary, self.root, self.url = binary, root, url
        self.home, self.ipc = root / "state", root / "ipc"
        self.ipc.mkdir(mode=0o700)
        self.environment = dict(os.environ, **{PASSWORD_ENV: "fixture-password"})
        self.environment.pop("RDRIVE_HOME", None)
        self.environment.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        self.base = [str(binary), "--home", str(self.home), "--progress", "off"]
        self.process = None
        self.log = (root / "provider.log").open("w+")

    def command(self, *arguments, success=True, json_output=True, timeout=30, environment=None):
        args = self.base + (["--json"] if json_output else []) + list(map(str, arguments))
        result = subprocess.run(args, env=environment or self.environment, capture_output=True, text=True, timeout=timeout)
        assert (result.returncode == 0) == success, (arguments, result.returncode, result.stdout, result.stderr)
        if not success:
            return result
        if not json_output:
            return result.stdout
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise AssertionError((arguments, "expected exactly one JSON object", result.stdout, result.stderr)) from error

    def status(self, *args):
        return validate_report(self.command("status", *args))

    def add(self, name, suffix=""):
        return self.command("profile", "add", name, "--protocol", "webdav", "--url", self.url + suffix,
                            "--allow-http", "--username", "user", "--password-env", PASSWORD_ENV, "--timeout", "3")

    def rpc(self, **message):
        endpoint = json.loads((self.ipc / "bridge-endpoint.json").read_bytes())
        message.update(v=1, auth_token=endpoint["token"], request_id=uuid.uuid4().hex)
        message.setdefault("domain", "status-fixture")
        with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=20) as stream:
            stream.sendall(json.dumps(message).encode() + b"\n")
            with stream.makefile("rb") as source:
                for line in source:
                    response = json.loads(line)
                    if "progress" not in response:
                        return response
        raise AssertionError("Fixture broker disconnected")

    def start(self):
        assert self.process is None
        self.process = subprocess.Popen(self.base + ["--json", "provider", "serve", "--socket", str(self.ipc / "bridge.sock"),
            "--staging", str(self.ipc / "Transfers"), "--development-tcp"], env=self.environment,
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.log, start_new_session=True)
        deadline = time.monotonic() + 20
        while True:
            assert self.process.poll() is None, "Fixture provider exited during startup"
            try:
                if self.rpc(op="domains").get("ok"):
                    return
            except (OSError, ValueError):
                pass
            assert time.monotonic() < deadline, "Fixture provider startup timed out"
            time.sleep(.05)

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGINT)
                try:
                    self.process.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    # Only this workflow's known private process group is signalled.
                    os.killpg(self.process.pid, signal.SIGTERM)
                    self.process.wait(timeout=10)
            self.process = None

    def close(self):
        self.stop()
        self.log.close()


def run(binary, report_path=None):
    binary = Path(binary).resolve()
    checks = []
    with tempfile.TemporaryDirectory(prefix="rdrive-unified-status-") as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        remote = root / "remote"
        remote.mkdir(mode=0o700)
        (remote / "other").mkdir(mode=0o700)
        (remote / "doc.txt").write_bytes(b"verified synthetic remote content")
        for number in range(12):
            (remote / f"conflict-{number}.txt").write_bytes(f"unchanged remote {number}".encode())
        StatusDav.root = remote
        StatusDav.observed = []
        StatusDav.requests_seen = []
        StatusDav.response_status = None
        StatusDav.delayed = False
        StatusDav.delay_seconds = 0
        StatusDav.active_requests = 0
        StatusDav.peak_requests = 0
        StatusDav.idle = threading.Event()
        StatusDav.idle.set()
        StatusDav.require_pair = False
        StatusDav.pair_seen = threading.Event()
        StatusDav.delay_entered = threading.Event()
        StatusDav.release_delay = threading.Event()
        server = StatusServer(("127.0.0.1", 0), StatusDav)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        fixture = Fixture(binary, root, f"http://127.0.0.1:{server.server_port}/dav/")
        def passed(label):
            checks.append(label)
            print("PASS " + label, flush=True)
        try:
            missing = fixture.status()
            assert not fixture.home.exists(), "Default status initialized an absent state directory"
            assert not StatusDav.requests()
            human = fixture.command("status", json_output=False)
            assert all(label in human.lower() for label in ["connection", "index", "cache", "history", "checkpoint", "conflict"]), human
            assert not fixture.home.exists()
            passed("empty-state JSON and human overview report all sections without initializing storage or contacting a server")

            fixture.add("fixture")
            fixture.add("other", "other/")
            value = fixture.status()
            assert connection(value, "fixture")["state"] == "unknown"
            assert connection(value, "other")["state"] == "unknown"
            assert not StatusDav.requests()
            passed("configured accounts remain unknown until an actual successful remote observation")

            remote_before = inventory(remote)
            offset = len(StatusDav.requests())
            checked = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "3")
            observed = connection(checked, "fixture")
            assert observed["state"] == "online" and observed.get("last_success_at"), observed
            assert observed.get("checked_at"), observed
            requests = StatusDav.requests()[offset:]
            assert requests and all(method in ("PROPFIND", "OPTIONS", "HEAD") for method, _, _ in requests), requests
            assert all(path.rstrip("/") == "/dav" for _, path, _ in requests), requests
            assert inventory(remote) == remote_before
            first_success = observed["last_success_at"]
            offset = len(StatusDav.requests())
            cached = fixture.status()
            assert connection(cached, "fixture")["state"] == "online"
            assert connection(cached, "other")["state"] == "unknown"
            assert len(StatusDav.requests()) == offset, "Passive overview contacted the server"
            passed("explicit connection check authenticates root metadata only and passive status reuses its timestamp without remote I/O")

            fixture.add("unpersisted")
            health_directory = fixture.home / "connection-health"
            health_directory.chmod(0o500)
            try:
                before = inventory(health_directory)
                offset = len(StatusDav.requests())
                unsaved = fixture.status("--check-connections", "--profile", "unpersisted", "--timeout", "3")
                row = connection(unsaved, "unpersisted")
                assert row["state"] == "online" and row.get("last_success_at") and row.get("observation_error"), row
                assert envelope(unsaved, "connections")["status"] == "partial", unsaved["connections"]
                assert row["source"] == "current_probe_not_persisted", row
                assert len(StatusDav.requests()) > offset and inventory(health_directory) == before
            finally:
                health_directory.chmod(0o700)
            assert connection(fixture.status(), "unpersisted")["state"] == "unknown"
            passed("a fresh authenticated probe remains online when observation persistence fails and reports the storage error separately")

            with StatusDav.guard:
                StatusDav.response_status = 503
            failed = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "3")
            row = connection(failed, "fixture")
            assert row["state"] == "offline" and row.get("last_error"), row
            assert row.get("last_success_at") == first_success
            encoded = json.dumps(failed)
            assert "synthetic-server-secret-must-never-appear" not in encoded and "fixture-password" not in encoded
            with StatusDav.guard:
                StatusDav.response_status = 401
            denied = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "3")
            row = connection(denied, "fixture")
            assert row["state"] == "offline" and row.get("last_error") and row.get("last_success_at") == first_success, row
            with StatusDav.guard:
                StatusDav.response_status = None
                StatusDav.delayed = True
            started = time.monotonic()
            timed_out = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "1")
            assert time.monotonic() - started < 8, "Per-profile probe deadline did not bound a blocked remote"
            row = connection(timed_out, "fixture")
            assert row["state"] == "unknown" and row.get("last_error") and row.get("last_success_at") == first_success, row
            with StatusDav.guard:
                StatusDav.delayed = False
            StatusDav.release_delay.set()
            recovered = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "3")
            assert connection(recovered, "fixture")["state"] == "online"
            missing_credentials = dict(fixture.environment)
            missing_credentials.pop(PASSWORD_ENV)
            last_success = connection(recovered, "fixture")["last_success_at"]
            offset = len(StatusDav.requests())
            unavailable = validate_report(fixture.command("status", "--check-connections", "--profile", "fixture", "--timeout", "3", environment=missing_credentials))
            row = connection(unavailable, "fixture")
            assert row["state"] == "unknown" and row.get("last_error") and row.get("last_success_at") == last_success, row
            assert len(StatusDav.requests()) == offset, "Missing explicit environment credentials caused an unrelated authentication fallback"
            recovered = fixture.status("--check-connections", "--profile", "fixture", "--timeout", "3")
            assert connection(recovered, "fixture")["state"] == "online"
            passed("service failure, authentication rejection and bounded timeout preserve the latest successful observation and redact server text")

            assert StatusDav.idle.wait(3)
            with StatusDav.guard:
                StatusDav.delayed = True
                StatusDav.delay_entered = threading.Event()
                StatusDav.release_delay = threading.Event()
            cancelled = subprocess.Popen(fixture.base + ["--json", "status", "--check-connections", "--profile", "fixture", "--timeout", "10"],
                env=fixture.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            owned_children = []
            try:
                assert StatusDav.delay_entered.wait(8), "Cancellation fixture did not enter authenticated root metadata read"
                owned_children = owned_probe_children(cancelled.pid, binary)
                assert owned_children, "Expected the CLI's isolated probe helper"
                started = time.monotonic()
                cancelled.send_signal(signal.SIGINT)
                stdout, stderr = cancelled.communicate(timeout=5)
                assert cancelled.returncode != 0 and time.monotonic() - started < 5, (cancelled.returncode, stdout, stderr)
                deadline = time.monotonic() + 5
                while any(same_process(item) for item in owned_children) and time.monotonic() < deadline:
                    time.sleep(.025)
                assert not any(same_process(item) for item in owned_children), "Cancelled status left an owned probe helper alive"
                assert "synthetic-server-secret-must-never-appear" not in stdout + stderr and "fixture-password" not in stdout + stderr
            finally:
                if cancelled.poll() is None:
                    cancelled.terminate()
                    cancelled.wait(timeout=5)
                for item in owned_children:
                    if same_process(item):
                        # Exact UID, process birth, executable and private group
                        # prove that only this test's own helper is signalled.
                        os.killpg(item["group"], signal.SIGTERM)
                with StatusDav.guard:
                    StatusDav.delayed = False
                StatusDav.release_delay.set()
            assert StatusDav.idle.wait(3)
            passed("interrupting a live status probe exits promptly and leaves no owned helper or credential output behind")

            for mode in ["timeout", "cancellation"]:
                assert StatusDav.idle.wait(3)
                with StatusDav.guard:
                    StatusDav.delayed = True
                    StatusDav.delay_entered = threading.Event()
                    StatusDav.release_delay = threading.Event()
                client = McpClient(binary, fixture.home, fixture.environment, write=True,
                                   timeout=2 if mode == "timeout" else 30)
                operation_children, probe_children = [], []
                try:
                    client.initialize()
                    client.counter += 1
                    client.send({"jsonrpc":"2.0", "id":client.counter, "method":"tools/call", "params":{
                        "name":"rdrive_status_check_connections", "arguments":{"profile":"fixture", "timeout":10}}})
                    assert StatusDav.delay_entered.wait(8), "MCP status check did not reach the owned metadata barrier"
                    descendants = subprocess.run(["pgrep", "-P", str(client.process.pid)], capture_output=True, text=True, timeout=5)
                    assert descendants.returncode == 0, descendants.stderr
                    for value in descendants.stdout.split():
                        item = process_identity(int(value))
                        if item is None:
                            continue
                        assert item["uid"] == os.geteuid() and item["parent"] == client.process.pid
                        assert Path(item["executable"]).resolve() == binary, item
                        operation_children.append(item)
                        probe_children.extend(owned_probe_children(item["pid"], binary))
                    assert operation_children and probe_children, "The MCP request must own its CLI and isolated probe descendants"
                    if mode == "cancellation":
                        client.send({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{
                            "requestId":client.counter, "reason":"owned status fixture"}})
                    response = client.receive(timeout=7)
                    assert response.get("id") == client.counter and response.get("result", {}).get("isError"), response
                    assert ("timed out" if mode == "timeout" else "cancelled") in json.dumps(response), response
                    deadline = time.monotonic() + 5
                    while any(same_process(item) for item in probe_children + operation_children) and time.monotonic() < deadline:
                        time.sleep(.025)
                    assert not any(same_process(item) for item in probe_children + operation_children), "MCP left an owned status/probe descendant alive"
                    assert "fixture-password" not in json.dumps(response)
                finally:
                    # Both identity chains were observed before cancellation.
                    # Cleanup can signal only this test's exact owned processes.
                    for item in probe_children:
                        if same_process(item):
                            os.killpg(item["group"], signal.SIGTERM)
                    for item in operation_children:
                        if same_process(item):
                            os.kill(item["pid"], signal.SIGTERM)
                    with StatusDav.guard:
                        StatusDav.delayed = False
                    StatusDav.release_delay.set()
                    client.close()
                assert StatusDav.idle.wait(3)
                passed(f"MCP {mode} cooperatively stops the status CLI and its separately grouped live probe descendants")

            with ResetServer(("127.0.0.1", 0), ResetPeer) as disconnected:
                disconnected_thread = threading.Thread(target=disconnected.serve_forever, daemon=True)
                disconnected_thread.start()
                try:
                    fixture.command("profile", "add", "offline", "--protocol", "webdav", "--url",
                        f"http://127.0.0.1:{disconnected.server_address[1]}/dav/", "--allow-http", "--username", "user",
                        "--password-env", PASSWORD_ENV, "--timeout", "3")
                    offline = fixture.status("--check-connections", "--profile", "offline", "--timeout", "10")
                    row = connection(offline, "offline")
                    assert row["state"] == "offline" and row.get("last_error") and not row.get("last_success_at"), row
                finally:
                    disconnected.shutdown()
                    disconnected_thread.join(timeout=3)
            passed("an actual reset transport is reported offline rather than inferred from a cached listing or a running process")

            candidates = []
            for path in (fixture.home / "connection-health").glob("*.json"):
                record = json.loads(path.read_bytes())
                if record.get("name_hash") == sha(b"fixture"):
                    candidates.append((record["last_attempt"]["finished_at_ms"], path, record))
            assert candidates
            _, health_path, record = max(candidates, key=lambda row: row[0])
            health_before = health_path.read_bytes()
            shift = record["last_attempt"]["finished_at_ms"] - (int(time.time() * 1000) - 600_000)
            for name in ["last_attempt", "last_success", "last_failure", "evidence"]:
                if record.get(name) is not None:
                    record[name]["started_at_ms"] -= shift
                    record[name]["finished_at_ms"] -= shift
            private_write(health_path, record)
            try:
                offset = len(StatusDav.requests())
                before = inventory(fixture.home)
                stale = fixture.status()
                row = connection(stale, "fixture")
                assert row["state"] == "stale" and row.get("last_success_at"), row
                assert len(StatusDav.requests()) == offset and inventory(fixture.home) == before
            finally:
                private_write(health_path, health_before)
            passed("old successful evidence becomes stale with its original success time and is never silently refreshed")

            for profile, arguments in [("observed", ("ls",)), ("stat-only", ("stat", "doc.txt")), ("info-only", ("info",))]:
                fixture.add(profile)
                assert connection(fixture.status(), profile)["state"] == "unknown"
                offset = len(StatusDav.requests())
                fixture.command(arguments[0], profile, *arguments[1:])
                assert len(StatusDav.requests()) > offset, (profile, "CLI read did not perform a remote operation")
                row = connection(fixture.status(), profile)
                assert row["state"] == "online" and row.get("last_success_at"), row
            before_missing = connection(fixture.status(), "observed")
            assert fixture.command("stat", "observed", "definitely-missing.txt") is None
            after_missing = connection(fixture.status(), "observed")
            assert after_missing["state"] == "online" and after_missing["last_success_at"] == before_missing["last_success_at"], after_missing
            passed("actual list, stat and info successes record fresh connection evidence while an absent file does not falsely mark a connection offline")

            configured_before_parallel = (fixture.home / "config.json").read_bytes()
            selected = json.loads(configured_before_parallel)
            selected["profiles"].pop("offline")
            private_write(fixture.home / "config.json", selected)
            try:
                assert StatusDav.idle.wait(3), "Previous fixture request did not finish"
                with StatusDav.guard:
                    assert StatusDav.active_requests == 0
                    StatusDav.delay_seconds = .75
                    StatusDav.require_pair = True
                    StatusDav.pair_seen.clear()
                    StatusDav.peak_requests = 0
                offset = len(StatusDav.requests())
                started = time.monotonic()
                all_checked = fixture.status("--check-connections", "--timeout", "3", "--jobs", "2")
                assert time.monotonic() - started < 15, "Bounded parallel checks unexpectedly serialized or stalled"
                for name in selected["profiles"]:
                    assert connection(all_checked, name)["state"] == "online", all_checked["connections"]
                with StatusDav.guard:
                    peak = StatusDav.peak_requests
                assert peak == 2, ("selected probe concurrency", peak)
                requests = StatusDav.requests()[offset:]
                assert len(requests) >= len(selected["profiles"]) and all(method in ("PROPFIND", "OPTIONS", "HEAD") for method, _, _ in requests), requests
            finally:
                with StatusDav.guard:
                    StatusDav.delay_seconds = 0
                    StatusDav.require_pair = False
                    StatusDav.pair_seen.set()
                private_write(fixture.home / "config.json", configured_before_parallel)
            passed("all-profile probes make bounded parallel metadata requests and honor the selected job limit")

            original_config = (fixture.home / "config.json").read_bytes()
            for field, replacement in [("url", fixture.url + "other/"),
                                       ("password_env", "RDRIVE_STATUS_UNUSED_CREDENTIAL_REF"),
                                       ("smb_encryption", False), ("timeout_secs", 4)]:
                changed = json.loads(original_config)
                changed["profiles"]["fixture"][field] = replacement
                private_write(fixture.home / "config.json", changed)
                try:
                    before = inventory(fixture.home)
                    changed_status = fixture.status()
                    assert connection(changed_status, "fixture")["state"] == "unknown", changed_status["connections"]
                    assert inventory(fixture.home) == before
                finally:
                    private_write(fixture.home / "config.json", original_config)
            passed("changed account, credential reference, encryption or timeout settings do not reuse previous connection evidence or perform a credential lookup")

            assert StatusDav.idle.wait(3)
            with StatusDav.guard:
                StatusDav.delayed = True
                StatusDav.delay_entered = threading.Event()
                StatusDav.release_delay = threading.Event()
            changed_during_probe = subprocess.Popen(fixture.base + ["--json", "status", "--check-connections", "--profile", "fixture", "--timeout", "10"],
                env=fixture.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            owned_children = []
            try:
                assert StatusDav.delay_entered.wait(8), "Configuration-change fixture did not enter metadata read"
                owned_children = owned_probe_children(changed_during_probe.pid, binary)
                changed = json.loads(original_config)
                changed["profiles"]["fixture"]["timeout_secs"] = 4
                private_write(fixture.home / "config.json", changed)
                StatusDav.release_delay.set()
                stdout, stderr = changed_during_probe.communicate(timeout=8)
                assert changed_during_probe.returncode == 0, (stdout, stderr)
                current = validate_report(json.loads(stdout))
                row = connection(current, "fixture")
                assert row["state"] == "unknown" and row.get("last_success_at") is None, row
                assert row["probe_error"]["code"] == "configuration_changed", row
                assert envelope(current, "connections")["status"] == "partial", current["connections"]
            finally:
                if changed_during_probe.poll() is None:
                    changed_during_probe.send_signal(signal.SIGINT)
                    changed_during_probe.wait(timeout=5)
                for item in owned_children:
                    if same_process(item):
                        os.killpg(item["group"], signal.SIGTERM)
                private_write(fixture.home / "config.json", original_config)
                with StatusDav.guard:
                    StatusDav.delayed = False
                StatusDav.release_delay.set()
            assert StatusDav.idle.wait(3)
            passed("a configuration change during a live check rejects old evidence and reports an explicit partial-result reason")

            fixture.add("sync-profile")
            sync_root = root / "ordinary-sync"
            sync_root.mkdir(mode=0o700)
            (sync_root / "a.txt").write_bytes(b"common starting version")
            (remote / "synced").mkdir(mode=0o700)
            fixture.command("sync", "sync-profile", sync_root, "synced", "--apply")
            (sync_root / "a.txt").write_bytes(b"local changed version")
            (remote / "synced/a.txt").write_bytes(b"independently changed remote version")
            plan = fixture.command("sync", "sync-profile", sync_root, "synced", success=False)
            assert json.loads(plan.stdout)["conflicts"]
            offset = len(StatusDav.requests())
            ordinary = fixture.status("--details", "--max-items", "2")
            reports = envelope(ordinary, "conflicts")["data"]["ordinary_sync"]["plans"]
            observed_plan = next(row for row in reports if row["profile"] == "sync-profile")
            assert observed_plan["conflict_count"] == 1 and observed_plan.get("observed_at"), observed_plan
            assert observed_plan["conflicts"][0]["path"] == "a.txt", observed_plan
            assert len(StatusDav.requests()) == offset, "Status silently reran an ordinary sync plan"
            preview = fixture.command("conflict", "repair", "sync-profile", sync_root, "synced", "a.txt", "--choose", "local")
            applied = fixture.command("conflict", "repair", "sync-profile", sync_root, "synced", "a.txt", "--choose", "local", "--apply", "--expect", preview["token"])
            assert applied["applied"]
            assert fixture.command("conflict", "list", "sync-profile", sync_root, "synced") == []
            ordinary = fixture.status("--details")
            reports = envelope(ordinary, "conflicts")["data"]["ordinary_sync"]["plans"]
            observed_plan = next(row for row in reports if row["profile"] == "sync-profile")
            assert observed_plan["conflict_count"] == 0, observed_plan
            passed("ordinary sync reports its latest observed conflict plan and refreshes that observation after explicit guarded repair")

            payload = root / "payload.txt"
            payload.write_bytes(b"durable synthetic history and upload contents")
            fixture.command("history", "config", "--profile", "fixture", "--enabled", "true")
            fixture.command("history", "import", "fixture", "doc.txt", payload, "--request-id", "status-history-fixture")
            fixture.command("cache", "config", "--max-bytes", "1048576")
            fixture.command("cache", "fetch", "fixture", "doc.txt", "--pin")
            fixture.command("put", "fixture", payload, "queued-first.txt", "--queue")
            fixture.command("put", "fixture", payload, "queued-second.txt", "--queue")
            fixture.command("checkpoint", "config", "--storage", "apfs-vault", "--enabled", "true", "--manual",
                            "--max-bytes", "536870912", "--max-records", "3", "--auto-prune", "true")
            fixture.command("index", "config", "--disable")
            fixture.command("smart", "setup", "status-fixture", "fixture")
            fixture.start()
            deadline = time.monotonic() + 15
            expected_names = {f"conflict-{number}.txt" for number in range(12)}
            while True:
                listed = fixture.rpc(op="list", parent_id="root")
                assert listed.get("ok"), listed
                items = listed["items"]
                if expected_names.issubset({item["name"] for item in items}):
                    break
                assert time.monotonic() < deadline, ("Synthetic root catalog did not become ready", listed)
                time.sleep(.05)
            for number in range(12):
                item = next(value for value in items if value["name"] == f"conflict-{number}.txt")
                staged = fixture.ipc / "Transfers" / f"conflict-{number}.data"
                staged.write_bytes(f"retained independent edit {number}".encode())
                result = fixture.rpc(op="modify", id=item["id"], content_path=str(staged),
                    base_content_version="obsolete-system-snapshot", base_metadata_version=item["metadata_version"])
                assert not result.get("ok"), result
            fixture.stop()
            paused = fixture.command("smart", "pause")
            assert paused["paused"]
            fixture.start()
            offset = len(StatusDav.requests())
            broker_report = fixture.status("--details", "--max-items", "2")
            assert fixture.process.poll() is None
            assert connection(broker_report, "other")["state"] == "online"
            assert len(StatusDav.requests()) == offset
            conflicts = envelope(broker_report, "conflicts")["data"]
            assert conflicts["needs_attention"] >= 12, conflicts
            assert 0 < len(conflicts["examples"]) <= 2, conflicts
            assert all(row["error"]["code"] == "version_conflict" for row in conflicts["examples"]), conflicts
            assert envelope(broker_report, "index")["data"]["domains"][0]["initialized"]
            assert envelope(broker_report, "history")["data"]["profiles"][0]["entries"] == 1
            assert not (fixture.home / "checkpoints/vault/vault.dmg").exists()
            fixture.stop()
            passed("overview remains available with a real provider and shows bounded retained conflicts, initialized indexes and history without probing or mounting")

            database = fixture.home / "state.json"
            current = json.loads(database.read_bytes())
            current["jobs"][0]["status"] = "running"
            current["jobs"][1]["status"] = "failed"
            current["jobs"][1]["error"] = "version_conflict: synthetic upload changed; Authorization: Bearer synthetic-server-secret-must-never-appear"
            private_write(database, current)
            smart_queue = next((fixture.home / "smart").glob("*/queue.json"))
            queue = json.loads(smart_queue.read_bytes())
            selected = next(iter(queue["mutations"].values()))
            selected["state"] = "running"
            private_write(smart_queue, queue)
            with (fixture.home / "client.lock").open("r+b") as writer:
                fcntl.flock(writer, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fixture.start()
                before = inventory(fixture.home)
                started = time.monotonic()
                held = fixture.status("--details", "--max-items", "2")
                assert time.monotonic() - started < 5, "Overview waited on the Store writer lock"
                assert inventory(fixture.home) == before, "Status initialized, recovered, rewrote or mounted state"
                assert json.loads(smart_queue.read_bytes())["mutations"][selected["id"]]["state"] == "running"
                assert "synthetic-server-secret-must-never-appear" not in json.dumps(held)
                assert "fixture-password" not in json.dumps(held)
                human = fixture.command("status", "--details", "--max-items", "2", json_output=False)
                assert "synthetic-server-secret-must-never-appear" not in human and "fixture-password" not in human
                assert inventory(fixture.home) == before
                fixture.stop()
            passed("held Store writer does not block status; running work, payloads, SQLite sidecars and private file inventory remain unchanged")

            scoped = fixture.status("--profile", "other", "--details", "--max-items", "2")
            assert scoped["profiles"] == 1 and scoped["queued"] == 0 and scoped["cache_files"] == 0, scoped
            assert [row["name"] for row in envelope(scoped, "configuration")["data"]["profiles"]] == ["other"]
            assert [row["profile"] for row in envelope(scoped, "history")["data"]["profiles"]] == ["other"]
            assert not envelope(scoped, "index")["data"]["domains"]
            assert not envelope(scoped, "conflicts")["data"]["domains"]
            assert not envelope(scoped, "conflicts")["data"]["examples"]
            assert not envelope(scoped, "conflicts")["data"]["ordinary_sync"]["plans"]
            assert connection(scoped, "other")["state"] == "online"
            passed("profile scope consistently filters account-owned counters, index, history, conflicts and connection evidence while global components remain labelled")

            history_db = fixture.home / "history/history.sqlite3"
            with contextlib.closing(sqlite3.connect(history_db)) as writer:
                writer.execute("PRAGMA journal_mode=WAL")
                writer.execute("PRAGMA wal_autocheckpoint=0")
                columns = [row[1] for row in writer.execute("PRAGMA table_info(entries)")]
                row = dict(zip(columns, writer.execute("SELECT * FROM entries LIMIT 1").fetchone()))
                archive = json.loads(row["archive"])
                identity = sha(uuid.uuid4().bytes)
                stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
                row.update(id=identity, created=stamp, request_id="status-wal-visible")
                archive["request_id"] = row["request_id"]
                archive["public"].update(id=identity, created=stamp)
                row["archive"] = json.dumps(archive)
                writer.execute("INSERT INTO entries (" + ",".join(columns) + ") VALUES (" + ",".join("?" for _ in columns) + ")", [row[name] for name in columns])
                writer.commit()
                assert history_db.with_name(history_db.name + "-wal").stat().st_size > 0
                before = inventory(fixture.home)
                visible = fixture.status("--profile", "fixture")
                assert envelope(visible, "history")["data"]["profiles"][0]["entries"] == 2, visible["history"]
                assert inventory(fixture.home) == before, "History snapshot changed the live WAL or SHM"
            passed("copied read-only SQL snapshots include committed WAL history without creating or changing live database sidecars")

            index_db = next((fixture.home / "provider").glob("*.sqlite3"))
            with contextlib.closing(sqlite3.connect(index_db)) as writer:
                writer.execute("PRAGMA wal_checkpoint(TRUNCATE)")
                writer.execute("PRAGMA journal_mode=DELETE")
                writer.execute("PRAGMA user_version=1")
                writer.execute("UPDATE meta SET schema_version=1")
                writer.commit()
            before = inventory(fixture.home)
            legacy = fixture.status()
            assert envelope(legacy, "index")["status"] in ("partial", "error"), legacy["index"]
            assert inventory(fixture.home) == before, "Status migrated or locked a legacy provider index"
            with contextlib.closing(sqlite3.connect(index_db.as_uri() + "?mode=ro&immutable=1", uri=True)) as reader:
                assert reader.execute("PRAGMA user_version").fetchone()[0] == 1
            passed("legacy provider SQLite is reported for attention without initialization, schema migration or writer-lock creation")

            original = database.read_bytes()
            private_write(database, b"{")
            try:
                before = inventory(fixture.home)
                partial = fixture.status()
                assert envelope(partial, "configuration")["status"] == "ok"
                assert envelope(partial, "history")["status"] == "ok"
                assert envelope(partial, "cache")["status"] in ("error", "partial")
                assert envelope(partial, "queue")["status"] in ("error", "partial")
                assert inventory(fixture.home) == before
            finally:
                private_write(database, original)
            passed("one corrupt local component becomes a section error while unrelated configuration and history stay visible and untouched")

            offset = len(StatusDav.requests())
            inspected = fixture.status("--check-checkpoints", "--profile", "other")
            assert envelope(inspected, "checkpoints")["data"] is not None
            assert not (fixture.home / "checkpoints/vault/vault.dmg").exists(), "Checkpoint inspection created a vault image"
            assert len(StatusDav.requests()) == offset
            passed("explicit checkpoint inspection reports an uninitialized owned vault without creating a checkpoint or contacting remote storage")

            for args in [("--timeout", "0", "--check-connections"), ("--timeout", "121", "--check-connections"),
                         ("--max-items", "0"), ("--max-items", "101"), ("--profile", "missing")]:
                fixture.command("status", *args, success=False)
            passed("public bounds and unknown-profile errors are rejected without silently inspecting a different account")

            report = {"binary_sha256": sha(binary.read_bytes()), "passed": len(checks), "checks": checks,
                      "scope": "real CLI, disposable authenticated DAV and explicit private state only",
                      "live_state_changed": False, "checkpoint_image_created": (fixture.home / "checkpoints/vault/vault.dmg").exists()}
            if report_path:
                Path(report_path).parent.mkdir(parents=True, exist_ok=True)
                Path(report_path).write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report), flush=True)
            return report
        finally:
            StatusDav.release_delay.set()
            StatusDav.pair_seen.set()
            fixture.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default=str(ROOT / "target/debug/rdrive"))
    parser.add_argument("--report")
    options = parser.parse_args()
    run(options.binary, options.report)
