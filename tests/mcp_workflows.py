#!/usr/bin/env python3
"""Exercise the real MCP stdio boundary against an authenticated local DAV server.

Only Python's standard library and the existing DAV fixture are used. All state,
files, and server mutations are isolated in a temporary directory. No configured
real storage account or native mount is contacted.

Run: python3 tests/mcp_workflows.py [path/to/rdrive] [--report path.json]
"""
import argparse
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time

from cli_workflows import Dav


PASSWORD = "fixture-password"


class LocalDav(Dav):
    """A delayed directory lets the test observe timeout and cancellation."""
    slow_started = threading.Event()

    def reply(self, status, body=b"", **headers):
        # Path.relative_to(root).as_posix() is '.' for the root itself. The
        # shared fixture's root href must use the canonical DAV collection URL.
        if status == 207:
            body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
        try:
            return super().reply(status, body, **headers)
        except (BrokenPipeError, ConnectionResetError):
            # Timeout/cancel tests intentionally close their DAV connection.
            self.close_connection = True

    def handle_dav(self):
        if self.path.startswith("/dav/slow"):
            self.slow_started.set()
            time.sleep(3)
        return super().handle_dav()


def sample_stalled_process(process):
    """Sample only the fixture-owned child after an unexpected timeout."""
    if sys.platform != "darwin" or process.poll() is not None:
        return ""
    try:
        result = subprocess.run(["/usr/bin/sample", str(process.pid), "1", "1", "-mayDie"],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, timeout=15)
        return "\nMCP child sample:\n" + result.stdout[:512 * 1024]
    except (OSError, subprocess.TimeoutExpired) as error:
        return "\nMCP child sample unavailable: " + type(error).__name__


class Client:
    def __init__(self, binary, home, env, *, write=False, roots=(), timeout=30):
        args = [str(binary), "--home", str(home), "mcp", "serve", "--timeout", str(timeout)]
        if write:
            args.append("--allow-write")
        for root in roots:
            args.extend(("--local-root", str(root)))
        self.process = subprocess.Popen(args, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.lines = queue.Queue()
        self.stderr = bytearray()
        self.responses = []
        self.counter = 0

        def read_stdout():
            for line in self.process.stdout:
                self.lines.put(line)
            self.lines.put(None)

        def read_stderr():
            while chunk := self.process.stderr.read(4096):
                self.stderr.extend(chunk)

        self.readers = [threading.Thread(target=read_stdout, daemon=True),
                        threading.Thread(target=read_stderr, daemon=True)]
        for reader in self.readers:
            reader.start()

    def send(self, value):
        self.process.stdin.write(json.dumps(value, ensure_ascii=False).encode() + b"\n")
        self.process.stdin.flush()

    def receive(self, timeout=35):
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty:
            stderr = self.stderr.decode(errors="replace").replace(PASSWORD, "<redacted>")[-2000:]
            readers = [reader.is_alive() for reader in self.readers]
            sample = sample_stalled_process(self.process).replace(PASSWORD, "<redacted>")
            raise AssertionError(
                f"MCP server did not return a response (pid={self.process.pid}, "
                f"exit={self.process.poll()!r}, readers={readers}); stderr={stderr or '<empty>'}{sample}"
            ) from None
        if line is None:
            raise AssertionError(f"MCP server exited early: {self.stderr.decode(errors='replace')}")
        try:
            value = json.loads(line)
        except ValueError:
            raise AssertionError(f"stdout contains a non-protocol line: {line[:200]!r}") from None
        assert isinstance(value, dict) and value.get("jsonrpc") == "2.0", value
        self.responses.append(value)
        return value

    def request(self, method, params=None, *, error=None, timeout=35):
        self.counter += 1
        identifier = self.counter
        request = {"jsonrpc": "2.0", "id": identifier, "method": method}
        if params is not None:
            request["params"] = params
        self.send(request)
        response = self.receive(timeout)
        assert response.get("id") == identifier, response
        if error is not None:
            assert response.get("error", {}).get("code") == error, response
            return response["error"]
        assert "error" not in response, response
        return response["result"]

    def initialize(self, version="2025-11-25"):
        result = self.request("initialize", {"protocolVersion": version, "capabilities": {},
                                             "clientInfo": {"name": "rdrive-test", "version": "1"}})
        assert result["protocolVersion"] == version, result
        assert "tools" in result["capabilities"], result
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return result

    def tool(self, name, *, fail=False, **arguments):
        result = self.request("tools/call", {"name": "rdrive_" + name, "arguments": arguments})
        assert isinstance(result.get("content"), list), result
        assert bool(result.get("isError", False)) == fail, result
        if fail:
            return result
        structured = result.get("structuredContent")
        if structured is not None:
            return structured.get("data", structured) if isinstance(structured, dict) else structured
        text = "\n".join(block["text"] for block in result["content"] if block["type"] == "text")
        try:
            return json.loads(text)
        except ValueError:
            return text

    def invalid_tool(self, name, **arguments):
        return self.request("tools/call", {"name": "rdrive_" + name,
                                            "arguments": arguments}, error=-32602)

    def close(self):
        if self.process.stdin and not self.process.stdin.closed:
            try:
                self.process.stdin.close()
            except BrokenPipeError:
                pass
        if self.process.poll() is None:
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
                raise AssertionError("MCP process did not exit after stdin EOF") from None
        for reader in self.readers:
            reader.join(timeout=2)
        assert self.process.returncode == 0, self.stderr.decode(errors="replace")
        # Unexpected unsolicited stdout is also checked at shutdown.
        while not self.lines.empty():
            line = self.lines.get_nowait()
            if line is not None:
                value = json.loads(line)
                assert value.get("jsonrpc") == "2.0", value
        assert PASSWORD.encode() not in self.stderr, "credential leaked to MCP stderr"
        assert PASSWORD not in json.dumps(self.responses), "credential leaked to MCP response"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/rdrive")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary = Path(args.binary).resolve(strict=True)
    passed = []
    with tempfile.TemporaryDirectory(prefix="rdrive-mcp-") as temporary:
        root = Path(temporary).resolve()
        home, allowed = root / "state", root / "allowed"
        allowed.mkdir()
        LocalDav.root = root / "remote"
        LocalDav.root.mkdir()
        LocalDav.requests_seen = []
        LocalDav.locks = {}
        LocalDav.interrupt_once = set()
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), LocalDav)
        server.daemon_threads = True
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        env = dict(os.environ, RDRIVE_TEST_PASSWORD=PASSWORD)
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        clients = []

        def cli(*arguments):
            completed = subprocess.run([str(binary), "--home", str(home), "--json", "--progress", "off",
                                        *map(str, arguments)], env=env, capture_output=True, timeout=30)
            assert completed.returncode == 0, completed.stderr.decode(errors="replace")
            return json.loads(completed.stdout)

        def client(**kwargs):
            result = Client(binary, home, env, **kwargs)
            clients.append(result)
            return result

        try:
            cli("profile", "add", "test", "--protocol", "webdav", "--url",
                f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http",
                "--username", "user", "--password-env", "RDRIVE_TEST_PASSWORD")
            (LocalDav.root / "hello.txt").write_text("你好，MCP\n", encoding="utf-8")
            (LocalDav.root / "binary.bin").write_bytes(b"\xff\x00\xfe")
            (LocalDav.root / "large.txt").write_bytes(b"x" * (1024 * 1024 + 1))
            (LocalDav.root / "slow").mkdir()

            readonly = client(roots=[allowed])
            readonly.request("tools/list", error=-32000)
            initialization = readonly.initialize()
            assert readonly.request("ping") == {}
            catalog = readonly.request("tools/list")["tools"]
            names = {tool["name"] for tool in catalog}
            assert {"rdrive_profiles", "rdrive_list", "rdrive_stat", "rdrive_sync_plan", "rdrive_index_status"} <= names
            assert not {"rdrive_upload", "rdrive_remove", "rdrive_download", "rdrive_sync_apply"} & names
            for tool in catalog:
                assert tool["inputSchema"]["type"] == "object", tool
                assert tool["inputSchema"].get("additionalProperties") is False, tool
            readonly.request("tools/call", {"name": "rdrive_remove",
                                            "arguments": {"profile": "test", "remote": "hello.txt"}}, error=-32602)
            assert (LocalDav.root / "hello.txt").exists()
            assert "test" in readonly.tool("profiles")
            assert readonly.tool("stat", profile="test", remote="hello.txt")["size"] > 0
            listing = readonly.tool("list", profile="test", remote="")
            assert any(item["path"] == "hello.txt" for item in listing), listing
            text = readonly.tool("read_text", profile="test", remote="hello.txt")
            assert "你好，MCP" in str(text), text
            readonly.tool("read_text", profile="test", remote="binary.bin", fail=True)
            large_gets = sum(command == "GET" and path == "/dav/large.txt"
                             for command, path in LocalDav.requests_seen)
            temporary_payloads = set((home / "tmp").iterdir())
            readonly.tool("read_text", profile="test", remote="large.txt", fail=True)
            assert sum(command == "GET" and path == "/dav/large.txt"
                       for command, path in LocalDav.requests_seen) == large_gets, "oversized text was downloaded before rejection"
            assert set((home / "tmp").iterdir()) == temporary_payloads, "oversized text left a temporary payload"
            resources = readonly.request("resources/list")["resources"]
            assert {"rdrive://status", "rdrive://profiles", "rdrive://domains", "rdrive://cache",
                    "rdrive://index", "rdrive://filters", "rdrive://policies", "rdrive://service"} == {resource["uri"] for resource in resources}, resources
            for resource in resources:
                response = readonly.request("resources/read", {"uri": resource["uri"]})
                assert response["contents"][0]["uri"] == resource["uri"], response
                json.loads(response["contents"][0]["text"])
            passed.append("stdio lifecycle, schemas, resources, read-only enforcement, UTF-8 and read-size bounds")

            readonly.request("tools/call", {"name": "rdrive_stat", "arguments": {
                "profile": "test", "remote": "hello.txt", "unknown": True}}, error=-32602)
            readonly.request("tools/call", {"name": "rdrive_list", "arguments": {
                "profile": "test", "recursive": "false"}}, error=-32602)
            readonly.request("tools/call", {"name": "rdrive_stat", "arguments": {
                "profile": "test"}}, error=-32602)
            readonly.request("not/a/method", error=-32601)
            readonly.request("tools/call", {"name": "not_a_tool", "arguments": {}}, error=-32602)
            readonly.process.stdin.write(b"{invalid json}\n")
            readonly.process.stdin.flush()
            malformed = readonly.receive()
            assert malformed["id"] is None and malformed["error"]["code"] == -32700, malformed
            readonly.send({"jsonrpc": "2.0", "method": "notifications/an_unused_notification"})
            assert readonly.request("ping") == {}
            readonly.invalid_tool("stat", profile="test", remote="../escape")
            readonly.invalid_tool("stat", profile="--help", remote="hello.txt")
            readonly.invalid_tool("stat", profile="test", remote="--json")
            readonly.invalid_tool("list", profile="test", contains="--progress=json")
            missing = readonly.tool("stat", profile="missing", remote="hello.txt", fail=True)
            assert missing.get("isError"), missing
            passed.append("unknown methods/tools, strict argument types, required fields, traversal and CLI error conversion")

            writable = client(write=True, roots=[allowed])
            writable.initialize()
            write_names = {tool["name"] for tool in writable.request("tools/list")["tools"]}
            assert {"rdrive_upload", "rdrive_download", "rdrive_sync_apply", "rdrive_repo_commit"} <= write_names
            for gated in write_names - names:
                readonly.request("tools/call", {"name": gated, "arguments": {}}, error=-32602)
            readonly.invalid_tool("cache_config", max_bytes=0)
            readonly.invalid_tool("smart_pause")
            readonly.invalid_tool("smart_resume")
            readonly.invalid_tool("queue_run")
            readonly.invalid_tool("index_config", enable=True)
            readonly.invalid_tool("index_refresh", domain="mcp-index")

            # Configuration goes through the actual CLI process; subsequent
            # calls and the index resource must read the same persisted policy.
            initial_index = readonly.tool("index_status")
            assert initial_index["domains"] == [] and initial_index["policy"]["enabled"], initial_index
            index_requests = len(LocalDav.requests_seen)
            policy = writable.tool("index_config", disable=True, depth=2,
                                   refresh_seconds=91, active_refresh_seconds=17)
            assert policy == {"enabled": False, "prefetch_depth": 2,
                              "refresh_secs": 91, "active_refresh_secs": 17}, policy
            assert readonly.tool("index_status")["policy"] == policy
            assert cli("index", "status")["policy"] == policy
            index_resource = readonly.request("resources/read", {"uri": "rdrive://index"})
            assert json.loads(index_resource["contents"][0]["text"])["policy"] == policy
            saved_config = (home / "config.json").read_bytes()
            for invalid_policy in ({"enable": True, "disable": True}, {"depth": 1, "all": True},
                                   {"refresh_seconds": 0}, {"active_refresh_seconds": 0},
                                   {"refresh_seconds": 31_536_001},
                                   {"active_refresh_seconds": 31_536_001}, {"depth": 2**32},
                                   {"depth": -1}, {"enable": "true"}):
                writable.invalid_tool("index_config", **invalid_policy)
                assert (home / "config.json").read_bytes() == saved_config, invalid_policy
            enabled = writable.tool("index_config", enable=True, all=True)
            assert enabled["enabled"] and enabled["prefetch_depth"] is None, enabled
            root_only = writable.tool("index_config", depth=0)
            assert root_only["prefetch_depth"] == 0 and root_only["refresh_secs"] == 91, root_only
            cli("smart", "setup", "mcp-index", "test")
            domain_index = readonly.tool("index_status", domain="mcp-index")
            assert len(domain_index["domains"]) == 1, domain_index
            assert domain_index["domains"][0]["domain"] == "mcp-index", domain_index
            assert not domain_index["domains"][0]["initialized"], domain_index
            readonly.tool("index_status", domain="missing-domain", fail=True)
            refresh = writable.tool("index_refresh", domain="mcp-index", recursive=True)
            assert refresh["queued"] and not refresh["completed"] and refresh["requires_provider_service"], refresh
            assert refresh["remote"] == "" and refresh["recursive"], refresh
            writable.invalid_tool("index_refresh", domain="mcp-index", remote="../escape")
            writable.tool("index_refresh", domain="mcp-index", remote="unindexed-child", fail=True)
            assert len(LocalDav.requests_seen) == index_requests, "index control operations contacted remote storage"
            passed.append("index policy persists through MCP/CLI/resource, invalid combinations preserve state, targeted refresh queues metadata-only demand")

            target = allowed / "download.txt"
            writable.tool("download", profile="test", remote="hello.txt", local=str(target))
            assert target.read_text() == "你好，MCP\n"
            writable.tool("download", profile="test", remote="hello.txt", local=str(target), fail=True)
            payload = allowed / "input.txt"
            payload.write_text("original payload", encoding="utf-8")
            writable.tool("mkdir", profile="test", remote="files")
            writable.tool("upload", profile="test", local=str(payload), remote="files/a.txt")
            assert (LocalDav.root / "files/a.txt").read_text() == "original payload"
            payload.write_text("new payload")
            writable.tool("upload", profile="test", local=str(payload), remote="files/a.txt", fail=True)
            assert (LocalDav.root / "files/a.txt").read_text() == "original payload"
            writable.tool("upload", profile="test", local=str(payload), remote="files/a.txt", overwrite=True)
            writable.tool("copy", profile="test", **{"from": "files/a.txt", "to": "files/b.txt"})
            writable.tool("move", profile="test", **{"from": "files/b.txt", "to": "files/c.txt"})
            assert not (LocalDav.root / "files/b.txt").exists()
            assert (LocalDav.root / "files/c.txt").read_text() == "new payload"
            writable.tool("remove", profile="test", remote="files/c.txt")
            assert not (LocalDav.root / "files/c.txt").exists()
            passed.append("real DAV downloads/uploads, no-clobber and explicit overwrite, copy/move/remove")

            outside = root / "outside"
            outside.mkdir()
            for escaped in (outside / "escape.txt", allowed / ".." / "outside" / "traversal.txt"):
                writable.invalid_tool("download", profile="test", remote="hello.txt", local=str(escaped))
                assert not escaped.exists()
            link = allowed / "linked"
            link.symlink_to(outside, target_is_directory=True)
            writable.invalid_tool("download", profile="test", remote="hello.txt", local=str(link / "escape.txt"))
            assert not (outside / "escape.txt").exists()
            # A root that contains the private state must still not expose it.
            broad = client(write=True, roots=[root])
            broad.initialize()
            writable.invalid_tool("upload", profile="test", local=str(home / "config.json"), remote="leak")
            broad.invalid_tool("upload", profile="test", local=str(home / "config.json"), remote="leak")
            assert not (LocalDav.root / "leak").exists()
            case_alias = home.with_name(home.name.upper()) / "config.json"
            if case_alias.exists() and case_alias.samefile(home / "config.json"):
                broad.invalid_tool("upload", profile="test", local=str(case_alias), remote="case-leak")
                assert not (LocalDav.root / "case-leak").exists()
            marker = root / "shell-executed"
            malicious = f"name;touch {marker}"
            writable.tool("read_text", profile="test", remote=malicious, fail=True)
            assert not marker.exists()
            no_roots = client(write=True)
            no_roots.initialize()
            no_roots.invalid_tool("download", profile="test", remote="hello.txt", local=str(allowed / "unapproved"))
            assert not (allowed / "unapproved").exists()
            passed.append("explicit local roots, ancestor/symlink escape rejection, protected state and no shell execution")

            syncdir = allowed / "sync"
            syncdir.mkdir()
            (syncdir / "a.txt").write_text("local initial")
            writable.tool("mkdir", profile="test", remote="sync")
            plan = readonly.tool("sync_plan", profile="test", local=str(syncdir), remote="sync")
            assert not plan["applied"] and not (LocalDav.root / "sync/a.txt").exists(), plan
            applied = writable.tool("sync_apply", profile="test", local=str(syncdir), remote="sync")
            assert applied["applied"] and (LocalDav.root / "sync/a.txt").read_text() == "local initial"
            (syncdir / "a.txt").write_text("local concurrent")
            (LocalDav.root / "sync/a.txt").write_text("remote concurrent")
            conflict = writable.tool("sync_apply", profile="test", local=str(syncdir), remote="sync", fail=True)
            assert conflict["structuredContent"]["data"]["conflicts"], conflict
            assert (syncdir / "a.txt").read_text() == "local concurrent"
            assert (LocalDav.root / "sync/a.txt").read_text() == "remote concurrent"
            (syncdir / "a.txt").write_text("merged")
            (LocalDav.root / "sync/a.txt").write_text("merged")
            writable.tool("sync_apply", profile="test", local=str(syncdir), remote="sync")
            (syncdir / "a.txt").unlink()
            readonly.tool("sync_plan", profile="test", local=str(syncdir), remote="sync", delete=True)
            assert (LocalDav.root / "sync/a.txt").exists(), "planning applied a deletion"
            writable.tool("sync_apply", profile="test", local=str(syncdir), remote="sync", delete=True)
            assert not (LocalDav.root / "sync/a.txt").exists(), "explicit baseline deletion was not applied"
            passed.append("sync plan has no publication, conflict data and both copies preserved, explicit baseline-backed deletion")

            writable.tool("cache_config", max_bytes=4096)
            writable.tool("cache_fetch", profile="test", remote="hello.txt", pin=True)
            before = len(LocalDav.requests_seen)
            offline = allowed / "offline.txt"
            writable.tool("cache_get", profile="test", remote="hello.txt", local=str(offline))
            assert offline.read_text() == "你好，MCP\n"
            assert len(LocalDav.requests_seen) == before
            assert readonly.tool("cache_status")
            writable.tool("cache_prune", max_bytes=0)
            assert readonly.tool("cache_list")
            passed.append("persistent cache budget, pinned cache and verified offline read without network")

            queued_file = allowed / "queued-input.txt"
            queued_file.write_text("durable snapshot")
            job = writable.tool("upload", profile="test", local=str(queued_file), remote="queued.txt", queue=True)
            assert not (LocalDav.root / "queued.txt").exists()
            queued_file.write_text("later local edit")
            writable.tool("queue_run", profile="test")
            assert (LocalDav.root / "queued.txt").read_text() == "durable snapshot"
            assert next(item for item in readonly.tool("queue_list") if item["id"] == job["id"])["status"] == "done"
            cancelled_job = writable.tool("upload", profile="test", local=str(queued_file), remote="cancelled.txt", queue=True)
            writable.tool("queue_cancel", id=cancelled_job["id"])
            writable.tool("queue_run", profile="test")
            assert not (LocalDav.root / "cancelled.txt").exists()
            assert writable.tool("smart_pause")["paused"]
            assert readonly.tool("smart_status")["control"]["paused"]
            assert not writable.tool("smart_resume")["paused"]
            passed.append("durable upload snapshots, queue cancellation and persisted Smart pause/resume")

            writable.tool("versions_save", profile="test", remote="files/a.txt")
            versions = readonly.tool("versions_list", profile="test", remote="files/a.txt")
            assert len(versions) == 1, versions
            version_id = versions[0]["id"]
            writable.tool("upload", profile="test", local=str(queued_file), remote="files/a.txt", overwrite=True)
            writable.tool("versions_restore", profile="test", remote="files/a.txt", id=version_id, fail=True)
            assert (LocalDav.root / "files/a.txt").read_text() == "later local edit"
            writable.tool("versions_restore", profile="test", remote="files/a.txt", id=version_id, overwrite=True)
            assert (LocalDav.root / "files/a.txt").read_text() == "new payload"
            metadata = readonly.tool("metadata_capture", local=str(payload))
            assert metadata["content_sha256"] == hashlib.sha256(payload.read_bytes()).hexdigest()
            writable.tool("metadata_push", profile="test", local=str(payload), remote="files/a.txt")
            writable.tool("metadata_pull", profile="test", remote="files/a.txt", local=str(payload))
            assert payload.read_text() == "new payload"
            passed.append("custom version save/no-clobber restore and guarded portable metadata round trip")

            writable.tool("mkdir", profile="test", remote="managed")
            writable.tool("repo_init", profile="test", root="managed")
            transaction = writable.tool("repo_begin", profile="test", root="managed")["id"]
            writable.tool("repo_mkdir", transaction=transaction, path="folder")
            writable.tool("repo_put", transaction=transaction, local=str(payload), path="folder/a.txt")
            before_commit = readonly.tool("repo_list", profile="test", root="managed")
            assert before_commit["commit"]["entries"] == {}, before_commit
            committed = writable.tool("repo_commit", transaction=transaction)
            assert committed["commit"]["entries"]["folder/a.txt"]["kind"] == "file", committed
            assert writable.tool("repo_commit", transaction=transaction)["checksum"] == committed["checksum"]
            recovered = allowed / "repo-download.txt"
            writable.tool("repo_get", profile="test", path="folder/a.txt", local=str(recovered), root="managed")
            assert recovered.read_text() == "new payload"
            assert readonly.tool("repo_status", transaction=transaction)["phase"] == "committed"
            history = readonly.tool("history", limit=2)
            assert isinstance(history, list) and len(history) <= 2, history
            passed.append("managed transaction stays unpublished until commit, idempotent commit and verified get")

            LocalDav.slow_started.clear()
            writable.counter += 1
            cancelled_id = writable.counter
            writable.send({"jsonrpc": "2.0", "id": cancelled_id, "method": "tools/call", "params": {
                "name": "rdrive_list", "arguments": {"profile": "test", "remote": "slow"}}})
            assert LocalDav.slow_started.wait(timeout=3), "delayed DAV request did not start"
            ping_started = time.monotonic()
            assert writable.request("ping", timeout=2) == {}
            assert time.monotonic() - ping_started < 1.5, "ping waited behind a file operation"
            writable.send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {
                "requestId": cancelled_id, "reason": "synthetic test cancellation"}})
            cancelled = writable.receive(timeout=3)
            assert cancelled["id"] == cancelled_id and cancelled["result"]["isError"], cancelled
            assert "cancel" in json.dumps(cancelled).lower(), cancelled
            assert writable.tool("status"), "state lock was retained after cancellation"
            passed.append("ping stays responsive during file operation, cancellation reaps child and releases state lock")

            short = client(timeout=1)
            short.initialize()
            started = time.monotonic()
            short.tool("list", profile="test", remote="slow", fail=True)
            assert time.monotonic() - started < 2.8, "operation timeout was not enforced"
            assert short.request("ping") == {}
            passed.append("timed-out CLI is reaped and MCP remains responsive")

            eof = client()
            eof.initialize()
            LocalDav.slow_started.clear()
            eof.counter += 1
            eof.send({"jsonrpc": "2.0", "id": eof.counter, "method": "tools/call", "params": {
                "name": "rdrive_list", "arguments": {"profile": "test", "remote": "slow"}}})
            assert LocalDav.slow_started.wait(timeout=3)
            eof.close()
            assert readonly.tool("status"), "state lock was retained after EOF"

            oversized = client()
            try:
                oversized.process.stdin.write(b" " * (1024 * 1024 + 1) + b"\n")
                oversized.process.stdin.flush()
            except BrokenPipeError:
                pass
            bounded = oversized.receive(timeout=5)
            assert bounded["error"]["code"] == -32700, bounded
            oversized.process.wait(timeout=5)
            assert oversized.process.returncode == 0
            passed.append("active-operation EOF releases state lock; oversized inbound frame fails within its bound")

            for version in ("2025-03-26", "2025-06-18"):
                legacy = client()
                legacy.initialize(version)
                assert legacy.request("ping") == {}
            passed.append("negotiation of supported older MCP revisions")
        finally:
            try:
                for entry in reversed(clients):
                    entry.close()
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    report = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "server_version": initialization["serverInfo"]["version"],
              "validated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "tool_count": len(write_names), "read_only_tool_count": len(names),
              "protocol": "2025-11-25", "passed": passed,
              "scope": "Temporary authenticated loopback WebDAV fixture; no real remote storage or mounts"}
    if args.report:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
