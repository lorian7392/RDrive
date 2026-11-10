#!/usr/bin/env python3
"""Black-box CLI workflows against a temporary, authenticated WebDAV fixture.

Uses only Python's standard library. No real remote storage is contacted.
Run: python3 tests/cli_workflows.py [path/to/rdrive]
"""
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
from urllib.parse import quote, unquote, urlsplit
from xml.sax.saxutils import escape


class Dav(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    root = None
    locks = {}
    requests_seen = []
    interrupt_once = set()
    range_requests = []

    def log_message(self, *_):
        pass

    def reply(self, status, body=b"", **headers):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        for key, value in headers.items():
            self.send_header(key.replace("_", "-"), value)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def resource(self, value=None):
        path = unquote(urlsplit(value or self.path).path)
        if not path.startswith("/dav/"):
            raise ValueError("outside fixture root")
        result = self.root / path[5:]
        if not result.resolve().is_relative_to(self.root):
            raise ValueError("traversal")
        return result

    def etag(self, path):
        data = "\n".join(sorted(x.name for x in path.iterdir())).encode() if path.is_dir() else path.read_bytes()
        return '"' + hashlib.sha256(data).hexdigest() + '"'

    def handle_dav(self):
        self.requests_seen.append((self.command, self.path))
        size = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(size)
        auth = "Basic " + base64.b64encode(b"user:fixture-password").decode()
        if self.headers.get("Authorization") != auth:
            return self.reply(401)
        p = self.resource()
        exists = p.exists()
        if self.command in ("PUT", "DELETE", "MOVE", "COPY"):
            if self.headers.get("If-None-Match") == "*" and exists:
                return self.reply(412)
            match = self.headers.get("If-Match")
            if match and (not exists or self.etag(p) != match):
                return self.reply(412)
            if str(p) in self.locks and self.locks[str(p)] not in self.headers.get("If", ""):
                return self.reply(423)
        if self.command == "PROPFIND":
            if not exists:
                return self.reply(404)
            paths = [p]
            if p.is_dir() and self.headers.get("Depth") == "1":
                paths += sorted(p.iterdir())
            rows = []
            for item in paths:
                href = "/dav/" + quote(item.relative_to(self.root).as_posix())
                if item.is_dir() and not href.endswith("/"):
                    href += "/"
                kind = "<d:collection/>" if item.is_dir() else ""
                length = 0 if item.is_dir() else item.stat().st_size
                rows.append(f"<d:response><d:href>{escape(href)}</d:href><d:propstat><d:prop>"
                            f"<d:resourcetype>{kind}</d:resourcetype><d:getcontentlength>{length}</d:getcontentlength>"
                            f"<d:getetag>{escape(self.etag(item))}</d:getetag>"
                            "<d:getlastmodified>Mon, 09 Nov 2026 08:00:00 GMT</d:getlastmodified>"
                            "</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
            xml = ('<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">' + "".join(rows) + "</d:multistatus>").encode()
            return self.reply(207, xml, Content_Type="application/xml")
        if self.command == "GET":
            if not exists or not p.is_file():
                return self.reply(404)
            data = p.read_bytes()
            if self.headers.get("If-Match") and self.headers["If-Match"] != self.etag(p):
                return self.reply(412)
            requested = self.headers.get("Range")
            if requested:
                assert requested.startswith("bytes=") and requested.endswith("-")
                offset = int(requested[6:-1])
                self.range_requests.append((self.path, offset))
                if offset >= len(data):
                    return self.reply(416)
                remaining = data[offset:]
                if self.path in self.interrupt_once:
                    self.interrupt_once.remove(self.path)
                    self.send_response(206)
                    self.send_header("Content-Length", str(len(remaining)))
                    self.send_header("Content-Range", f"bytes {offset}-{len(data)-1}/{len(data)}")
                    self.send_header("ETag", self.etag(p))
                    self.end_headers()
                    self.wfile.write(remaining[:len(remaining)//2])
                    self.wfile.flush()
                    self.close_connection = True
                    self.connection.shutdown(socket.SHUT_RDWR)
                    return
                return self.reply(206, remaining, Content_Range=f"bytes {offset}-{len(data)-1}/{len(data)}", ETag=self.etag(p))
            return self.reply(200, data, ETag=self.etag(p))
        if self.command == "PUT":
            if not p.parent.is_dir():
                return self.reply(409)
            p.write_bytes(body)
            return self.reply(204 if exists else 201, ETag=self.etag(p))
        if self.command == "MKCOL":
            if exists:
                return self.reply(405)
            if not p.parent.is_dir():
                return self.reply(409)
            p.mkdir()
            return self.reply(201)
        if self.command in ("MOVE", "COPY"):
            if not exists:
                return self.reply(404)
            dest = self.resource(self.headers["Destination"])
            if dest.exists() and self.headers.get("Overwrite") != "T":
                return self.reply(412)
            if not dest.parent.is_dir():
                return self.reply(409)
            if dest.exists() and dest.is_dir():
                shutil.rmtree(dest)
            if self.command == "MOVE":
                p.replace(dest)
            elif p.is_dir():
                shutil.copytree(p, dest)
            else:
                shutil.copyfile(p, dest)
            return self.reply(201)
        if self.command == "DELETE":
            if not exists:
                return self.reply(404)
            shutil.rmtree(p) if p.is_dir() else p.unlink()
            return self.reply(204)
        if self.command == "LOCK":
            token = "opaquelocktoken:fixture-lock"
            self.locks[str(p)] = token
            return self.reply(200, Lock_Token=f"<{token}>")
        if self.command == "UNLOCK":
            token = self.headers.get("Lock-Token", "").strip("<>")
            if self.locks.get(str(p)) != token:
                return self.reply(409)
            del self.locks[str(p)]
            return self.reply(204)
        self.reply(405)

    def do_GET(self):
        try:
            self.handle_dav()
        except (ValueError, OSError):
            self.reply(400)

    do_PROPFIND = do_PUT = do_DELETE = do_MKCOL = do_MOVE = do_COPY = do_LOCK = do_UNLOCK = do_GET


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/rdrive").resolve()
    with tempfile.TemporaryDirectory(prefix="rdrive-e2e-") as temporary:
        root = Path(temporary).resolve()
        Dav.root = root / "remote"
        Dav.root.mkdir()
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Dav)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        env = dict(os.environ, RDRIVE_HOME=str(root / "state"), RDRIVE_TEST_PASSWORD="fixture-password")
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        checks = []

        def cli(*args, ok=True, offline=False, raw=False):
            actual_env = env.copy()
            if offline:
                actual_env.pop("RDRIVE_TEST_PASSWORD", None)
            result = subprocess.run([str(binary), "--json", *map(str, args)], env=actual_env, capture_output=True, timeout=30)
            if ok and result.returncode != 0:
                raise AssertionError(f"{args}: {result.stderr.decode()}\n{result.stdout.decode()}")
            if not ok:
                assert result.returncode != 0, f"{args} unexpectedly succeeded"
                return result
            return result.stdout if raw else json.loads(result.stdout)

        try:
            cli("profile", "add", "test", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_TEST_PASSWORD")
            payload = root / "input"
            content = bytes(range(256)) * 8192
            payload.write_bytes(content)
            cli("mkdir", "test", "folder/sub", "--parents")
            cli("put", "test", payload, "folder/sub/中文 & spaces.bin")
            output = root / "download"
            cli("get", "test", "folder/sub/中文 & spaces.bin", output)
            assert output.read_bytes() == content
            cli("get", "test", "folder/sub/中文 & spaces.bin", output, ok=False)
            assert cli("cat", "test", "folder/sub/中文 & spaces.bin", raw=True) == content
            checks.append("2 MiB binary/Unicode upload, download, cat, no-clobber")

            cli("put", "test", payload, "resume.bin")
            Dav.interrupt_once.add("/dav/resume.bin")
            resumed_target = root / "resumed.bin"
            cli("get", "test", "resume.bin", resumed_target, "--resume", ok=False)
            transfer = next(t for t in cli("transfers", "list") if t["remote"] == "resume.bin")
            assert transfer["bytes"] == len(content)//2 and not resumed_target.exists(), {"transfer": transfer, "expected_prefix_bytes": len(content)//2, "destination_exists": resumed_target.exists()}
            resumed = cli("get", "test", "resume.bin", resumed_target, "--resume")
            assert resumed["resumed_from"] == len(content)//2
            assert resumed["transferred"] == len(content)//2
            assert resumed_target.read_bytes() == content
            assert [offset for request, offset in Dav.range_requests if request == "/dav/resume.bin"] == [0, len(content)//2]
            assert not cli("transfers", "list")
            Dav.interrupt_once.add("/dav/resume.bin")
            changed_target = root / "changed.bin"
            cli("get", "test", "resume.bin", changed_target, "--resume", ok=False)
            (Dav.root / "resume.bin").write_bytes(b"x" * len(content))
            previous_ranges = len(Dav.range_requests)
            cli("get", "test", "resume.bin", changed_target, "--resume", ok=False)
            assert len(Dav.range_requests) == previous_ranges and not changed_target.exists()
            saved = next(t for t in cli("transfers", "list") if t["remote"] == "resume.bin")
            cli("transfers", "discard", saved["id"])
            checks.append("real interrupted HTTP resumes only remaining bytes across CLI processes; revision change preserves destination")

            job = cli("put", "test", payload, "queued", "--queue")
            payload.write_bytes(b"later local edit")
            cli("queue", "run")
            assert (Dav.root / "queued").read_bytes() == content
            assert next(j for j in cli("queue", "list") if j["id"] == job["id"])["status"] == "done"
            cli("put", "test", payload, "queued", ok=False)
            assert (Dav.root / "queued").read_bytes() == content
            failed = [j for j in cli("queue", "list") if j["status"] == "failed"]
            assert failed
            assert (root / "state" / "spool" / failed[0]["snapshot"]).is_file()
            cli("queue", "cancel", failed[0]["id"])
            checks.append("durable queue uses snapshot; conflict retains failed upload")

            interrupted = cli("put", "test", payload, "queued", "--queue", "--overwrite")
            state_path = root / "state/state.json"
            database = json.loads(state_path.read_text())
            next(j for j in database["jobs"] if j["id"] == interrupted["id"])["status"] = "running"
            state_path.write_text(json.dumps(database))
            (Dav.root / "queued").write_bytes(b"newer remote writer")
            result = cli("queue", "run")
            assert any(j.get("status") == "skipped" and j["id"] == interrupted["id"] for j in result)
            assert (Dav.root / "queued").read_bytes() == b"newer remote writer"
            cli("queue", "run", "--retry-ambiguous-overwrite")
            assert (Dav.root / "queued").read_bytes() == b"later local edit"
            (Dav.root / "queued").write_bytes(content)
            checks.append("restart does not automatically repeat an ambiguous overwrite")

            cli("cache", "fetch", "test", "queued", "--pin")
            before = len(Dav.requests_seen)
            cached = root / "cached"
            cli("cache", "get", "test", "queued", cached, offline=True)
            assert cached.read_bytes() == content
            assert len(Dav.requests_seen) == before
            result = cli("cache", "prune")
            assert result["evicted"] == 0
            cli("cache", "unpin", "test", "queued")
            assert cli("cache", "prune")["evicted"] == 1
            checks.append("offline cache never contacts server; pin/unpin/LRU pruning")

            cli("put", "test", payload, "queued", "--overwrite", "--version")
            versions = cli("versions", "list", "test", "queued")
            assert len(versions) == 1
            cli("versions", "restore", "test", "queued", versions[0]["id"], "--overwrite")
            assert (Dav.root / "queued").read_bytes() == content
            version_content = Dav.root / ".rdrive-versions" / hashlib.sha256(b"queued").hexdigest() / versions[0]["id"] / "content"
            version_content.write_bytes(b"tampered archive")
            cli("versions", "restore", "test", "queued", versions[0]["id"], "--overwrite", ok=False)
            assert (Dav.root / "queued").read_bytes() == content
            cli("cp", "test", "queued", "copy")
            cli("mv", "test", "copy", "moved")
            cli("rm", "test", "moved")
            checks.append("remote version save/list/restore/checksum rejection; copy/move/delete")

            token = cli("lock", "test", "queued")["token"]
            cli("put", "test", payload, "queued", "--overwrite", ok=False)
            env["RDRIVE_WEBDAV_LOCK_TOKEN"] = token
            cli("put", "test", payload, "queued", "--overwrite")
            cli("unlock", "test", "queued")
            env.pop("RDRIVE_WEBDAV_LOCK_TOKEN")
            checks.append("WebDAV lock blocks writes without token and permits token owner")

            local = root / "sync"
            local.mkdir()
            (local / "a.txt").write_bytes(b"initial local")
            cli("mkdir", "test", "sync")
            plan = cli("sync", "test", local, "sync")
            assert not plan["applied"] and not (Dav.root / "sync/a.txt").exists()
            cli("sync", "test", local, "sync", "--apply")
            assert (Dav.root / "sync/a.txt").read_bytes() == b"initial local"
            (Dav.root / "sync/a.txt").write_bytes(b"remote edit")
            cli("sync", "test", local, "sync", "--apply")
            assert (local / "a.txt").read_bytes() == b"remote edit"
            (local / "a.txt").write_bytes(b"local edit")
            (Dav.root / "sync/a.txt").write_bytes(b"remote edit concurrently")
            cli("sync", "test", local, "sync", "--apply", ok=False)
            assert (local / "a.txt").read_bytes() == b"local edit"
            assert (Dav.root / "sync/a.txt").read_bytes() == b"remote edit concurrently"
            checks.append("sync dry-run, confirmed baseline, pull changes, preserve both conflict copies")

            conflicts = cli("conflict", "list", "test", local, "sync")
            assert [conflict["path"] for conflict in conflicts] == ["a.txt"]
            repair = ("conflict", "repair", "test", local, "sync", "a.txt", "--choose")
            preview = cli(*repair, "local")
            assert not preview["applied"] and preview["recovery_path"] is None
            cli(*repair, "local", "--apply", ok=False)
            (Dav.root / "sync/a.txt").write_bytes(b"new remote revision")
            stale = cli(*repair, "local", "--apply", "--expect", preview["token"], ok=False)
            assert b"preview this repair again" in stale.stderr
            assert (local / "a.txt").read_bytes() == b"local edit"
            assert (Dav.root / "sync/a.txt").read_bytes() == b"new remote revision"

            preview = cli(*repair, "local")
            result = cli(*repair, "local", "--apply", "--expect", preview["token"])
            assert result["applied"] and result["baseline_updated"]
            assert (local / result["recovery_path"]).read_bytes() == b"new remote revision"
            assert (Dav.root / "sync/a.txt").read_bytes() == b"local edit"
            assert cli("conflict", "list", "test", local, "sync") == []

            (local / "a.txt").write_bytes(b"another local edit")
            (Dav.root / "sync/a.txt").write_bytes(b"another remote edit")
            preview = cli(*repair, "remote")
            result = cli(*repair, "remote", "--apply", "--expect", preview["token"])
            assert result["applied"] and result["baseline_updated"]
            assert (local / result["recovery_path"]).read_bytes() == b"another local edit"
            assert (local / "a.txt").read_bytes() == b"another remote edit"
            assert cli("conflict", "list", "test", local, "sync") == []
            checks.append("preview and repair one sync conflict from either side with stale-token rejection and recovery copies")

            cli("stat", "test", "../escape", ok=False)
            cli("profile", "add", "bad", "--protocol", "webdav", "--url", "https://user:secret@example.com/", ok=False)
            assert b"fixture-password" not in (root / "state/config.json").read_bytes()
            assert b"fixture-password" not in (root / "state/state.json").read_bytes()
            checks.append("traversal rejected; passwords absent from persisted config/state")
            print(json.dumps({"passed": checks}, ensure_ascii=False, indent=2))
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
