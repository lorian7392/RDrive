#!/usr/bin/env python3
"""Version/conflict policy proof with disposable authenticated loopback DAV only."""
import argparse
import concurrent.futures
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav
from index_workflows import report_binary_path


def run(binary, output=None):
    checks = []
    with tempfile.TemporaryDirectory(prefix="rdrive-policy-") as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        remote.mkdir()
        for name, content in {"doc.txt": b"v0", "skip.bin": b"bin0", "both.txt": b"both0", "replace.txt": b"replace0", "remote.txt": b"remote0", "crash.txt": b"crash0", "compound.txt": b"compound0", "blocked.txt": b"blocked0"}.items():
            (remote / name).write_bytes(content)

        class Fixture(Dav):
            requests_seen = []
            reject_versions = False
            race_replace = False
            version_gate = None

            def handle_dav(self):
                if self.command == "PUT" and "/.rdrive-versions/" in self.path and self.path.endswith("/manifest.json") and type(self).reject_versions:
                    self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    return self.reply(503)
                if self.command == "PUT" and self.path == "/dav/replace.txt" and type(self).race_replace:
                    type(self).race_replace = False
                    (remote / "replace.txt").write_bytes(b"third-party-after-plan")
                return super().handle_dav()

            def reply(self, status, body=b"", **headers):
                if status == 207:
                    body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
                gate = type(self).version_gate
                if gate is not None and self.command == "PUT" and 200 <= status < 300 and "/.rdrive-versions/" in self.path and self.path.endswith("/manifest.json"):
                    type(self).version_gate = None
                    gate["entered"].set()
                    gate["release"].wait(30)
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
        env = dict(os.environ, RDRIVE_POLICY_TEST_PASSWORD="fixture-password")
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        base = [str(binary), "--home", str(home), "--json", "--progress", "off"]
        process = None
        log = (root / "bridge.log").open("w+")

        def wait(check, label, seconds=45):
            until = time.monotonic() + seconds
            while True:
                result = check()
                if result:
                    return result
                assert time.monotonic() < until, label
                time.sleep(.15)

        def cli(*args):
            until = time.monotonic() + 45
            while True:
                result = subprocess.run(base + list(args), env=env, capture_output=True, text=True, timeout=60)
                if result.returncode == 0:
                    return json.loads(result.stdout)
                if "another rdrive process" not in result.stderr and "active local worker" not in result.stderr:
                    raise AssertionError(result.stderr)
                assert time.monotonic() < until, result.stderr
                time.sleep(.1)

        def cli_failure(*args):
            until = time.monotonic() + 30
            while True:
                result = subprocess.run(base + list(args), env=env, capture_output=True, text=True, timeout=45)
                assert result.returncode != 0, result.stdout
                if "another rdrive process" not in result.stderr and "active local worker" not in result.stderr:
                    return result.stderr
                assert time.monotonic() < until, result.stderr
                time.sleep(.1)

        def rpc(**request):
            settings = json.loads((ipc / "bridge-endpoint.json").read_text())
            request.update(v=1, domain="policy", auth_token=settings["token"])
            request.setdefault("request_id", uuid.uuid4().hex)
            with socket.create_connection(("127.0.0.1", settings["port"]), timeout=60) as client:
                client.sendall(json.dumps(request).encode() + b"\n")
                with client.makefile("rb") as stream:
                    for line in stream:
                        response = json.loads(line)
                        if "progress" not in response:
                            return response
            raise ConnectionError("provider disconnected")

        def ok(**request):
            response = rpc(**request)
            assert response.get("ok"), response
            return response

        def start():
            nonlocal process
            process = subprocess.Popen(base + ["provider", "serve", "--socket", str(endpoint), "--staging", str(stage), "--development-tcp"], env=env, stdout=subprocess.DEVNULL, stderr=log)
            def ready():
                assert process.poll() is None, "provider exited"
                try:
                    return rpc(op="domains").get("ok")
                except (OSError, ValueError, FileNotFoundError):
                    return False
            wait(ready, "provider startup", 20)

        def stop(crash=False):
            nonlocal process
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGKILL if crash else signal.SIGINT)
                process.wait(timeout=15)
            process = None

        def item(name):
            return next(i for i in ok(op="list", parent_id="root")["items"] if i["name"] == name)

        def mutations():
            return next(row["mutations"] for row in cli("smart", "list", "policy") if row["domain"] == "policy")

        def mutation_for(request_id):
            rows = mutations()
            return next((row for row in rows if row["request"].get("request_id") == request_id), None)

        def modify(name, content, stale=None, new_name=None):
            current = stale or item(name)
            source = stage / (uuid.uuid4().hex + ".data")
            source.write_bytes(content)
            identity = uuid.uuid4().hex
            request = dict(op="modify", id=current["id"], name=new_name or name, parent_id="root", request_id=identity,
                           base_content_version=current["content_version"], base_metadata_version=current["metadata_version"], content_path=str(source))
            response = rpc(**request)
            return response, request

        def manifests(name):
            folder = remote / ".rdrive-versions" / hashlib.sha256(name.encode()).hexdigest()
            return {p.parent.name: json.loads(p.read_text()) for p in folder.glob("*/manifest.json")}

        def saved_contents(name):
            folder = remote / ".rdrive-versions" / hashlib.sha256(name.encode()).hexdigest()
            return [(folder / identity / "content").read_bytes() for identity in manifests(name)]

        def passed(text):
            checks.append(text)
            print("PASS " + text, flush=True)

        try:
            cli("profile", "add", "fixture", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_POLICY_TEST_PASSWORD")
            cli("index", "config", "--disable", "--refresh-seconds", "1", "--active-refresh-seconds", "1")
            cli("smart", "setup", "policy", "fixture")
            cli("policy", "config", "--enable-versions", "--keep", "2", "--include", "*.txt")
            start()
            wait(lambda: len(ok(op="list", parent_id="root")["items"]) >= 8, "initial catalog")
            for number in range(1, 4):
                response, _ = modify("doc.txt", f"v{number}".encode())
                assert response.get("ok"), response
            wait(lambda: len(manifests("doc.txt")) == 2, "automatic retention")
            assert set(saved_contents("doc.txt")) == {b"v1", b"v2"}
            assert (remote / "doc.txt").read_bytes() == b"v3"
            passed("Smart overwrites archived prior revisions and retained exactly the configured two newest backups")

            response, _ = modify("skip.bin", b"bin1")
            assert response.get("ok") and not manifests("skip.bin"), response
            cli("policy", "config", "--profile", "fixture", "--disable-versions")
            response, _ = modify("doc.txt", b"v4")
            assert response.get("ok") and set(saved_contents("doc.txt")) == {b"v1", b"v2"}, response
            passed("version filename filters and per-profile disable override left excluded files and old history unchanged")

            stale = item("both.txt")
            (remote / "both.txt").write_bytes(b"external-both")
            cli("policy", "config", "--profile", "fixture", "--conflicts", "keep-both")
            response, request = modify("both.txt", b"local-both", stale)
            assert not response.get("ok"), response
            wait(lambda: any(p.read_bytes() == b"local-both" for p in remote.glob("both (conflict-*).txt")), "automatic keep-both copy")
            copies = list(remote.glob("both (conflict-*).txt"))
            assert len(copies) == 1 and (remote / "both.txt").read_bytes() == b"external-both"
            stop(); start()
            repeated = rpc(**request)
            assert repeated.get("ok") and len(list(remote.glob("both (conflict-*).txt"))) == 1, repeated
            passed("automatic keep-both published one deterministic CreateNew copy and reused its receipt after restart")

            stale = item("replace.txt")
            (remote / "replace.txt").write_bytes(b"external-before-plan")
            cli("policy", "config", "--profile", "fixture", "--conflicts", "replace")
            Fixture.race_replace = True
            response, request = modify("replace.txt", b"local-replacement", stale)
            assert not response.get("ok"), response
            def stopped_resolution():
                return next((row for row in mutations() if row.get("resolution") and row["resolution"]["target_path"] == "replace.txt" and row["state"] == "conflict"), None)
            conflict = wait(stopped_resolution, "second external revision stopped replacement", 15)
            assert (remote / "replace.txt").read_bytes() == b"third-party-after-plan"
            assert b"external-before-plan" in saved_contents("replace.txt")
            time.sleep(1.1)
            assert (remote / "replace.txt").read_bytes() == b"third-party-after-plan"
            stop(); start()
            assert (remote / "replace.txt").read_bytes() == b"third-party-after-plan"
            cli("smart", "resolve", "policy", conflict["id"], "--strategy", "replace")
            wait(lambda: (remote / "replace.txt").read_bytes() == b"local-replacement", "explicit guarded replacement")
            assert b"third-party-after-plan" in saved_contents("replace.txt")
            passed("replace archived the observed remote version, refused another external edit and required a new explicit decision")

            cli("policy", "config", "--profile", "fixture", "--conflicts", "stop")
            stale = item("remote.txt")
            (remote / "remote.txt").write_bytes(b"remote-wins")
            response, _ = modify("remote.txt", b"retained-local", stale)
            assert not response.get("ok"), response
            conflict = wait(lambda: next((row for row in mutations() if row["request"].get("id") == stale["id"] and row["state"] == "conflict"), None), "stop conflict")
            result = cli("smart", "resolve", "policy", conflict["id"], "--strategy", "remote")
            recovered = root / "recovered.txt"
            cli("smart", "export", "policy", conflict["id"], str(recovered))
            assert result["local_snapshot_retained"] and recovered.read_bytes() == b"retained-local" and (remote / "remote.txt").read_bytes() == b"remote-wins"
            passed("remote resolution cancelled publication while retaining an exportable original local snapshot")

            cli("policy", "reset", "fixture")
            gate = {"entered": threading.Event(), "release": threading.Event()}
            Fixture.version_gate = gate
            with concurrent.futures.ThreadPoolExecutor() as workers:
                future = workers.submit(modify, "crash.txt", b"survived-version-crash")
                assert gate["entered"].wait(25), "version publication gate was not reached"
                stop(crash=True)
                gate["release"].set()
                try:
                    future.result(timeout=10)
                except (ConnectionError, OSError):
                    pass
            start()
            wait(lambda: (remote / "crash.txt").read_bytes() == b"survived-version-crash", "resume after committed backup acknowledgement loss")
            assert len(manifests("crash.txt")) == 1 and saved_contents("crash.txt") == [b"crash0"]
            passed("SIGKILL after backup publication reused the same durable version slot before replaying the guarded edit")

            pending_id = "policy-" + hashlib.sha256(b"independent-unconfirmed-writer").hexdigest()
            pending_dir = remote / ".rdrive-versions" / hashlib.sha256(b"doc.txt").hexdigest() / pending_id
            pending_dir.mkdir()
            pending_bytes = b"another writer's unconfirmed recovery copy"
            (pending_dir / "content").write_bytes(pending_bytes)
            (pending_dir / "manifest.json").write_text(json.dumps({"id": pending_id, "path": "doc.txt", "saved": "2099-01-01T00:00:00+00:00",
                "sha256": hashlib.sha256(pending_bytes).hexdigest(), "original": {"path": "doc.txt", "is_dir": False, "size": len(pending_bytes), "modified": None, "etag": '"pending-version"'}}))
            response, _ = modify("doc.txt", b"v5")
            assert response.get("ok"), response
            wait(lambda: len(manifests("doc.txt")) == 3, "confirmed retention with protected pending backup")
            assert (pending_dir / "content").read_bytes() == pending_bytes
            assert set(saved_contents("doc.txt")) == {pending_bytes, b"v4", b"v2"}
            passed("automatic retention protected another writer's unconfirmed backup while limiting completed history")

            before = manifests("doc.txt")
            Fixture.reject_versions = True
            response, _ = modify("doc.txt", b"must-remain-local")
            assert not response.get("ok"), response
            stop()
            assert manifests("doc.txt") == before and (remote / "doc.txt").read_bytes() == b"v5"
            retained = next(row for row in mutations() if row.get("snapshot", {}).get("sha256") == hashlib.sha256(b"must-remain-local").hexdigest())
            recovered = root / "failed-backup-local.txt"
            cli("smart", "export", "policy", retained["id"], str(recovered))
            assert recovered.read_bytes() == b"must-remain-local"
            cli("smart", "cancel", "policy", retained["id"])
            passed("failed backup preparation neither replaced the remote file nor pruned valid history and retained the local edit")

            Fixture.reject_versions = False
            cli("policy", "config", "--profile", "fixture", "--conflicts", "replace")
            start()
            stale = item("compound.txt")
            (remote / "compound.txt").write_bytes(b"external-compound")
            response, _ = modify("compound.txt", b"compound-local-body", stale, "renamed.txt")
            assert not response.get("ok"), response
            compound = wait(lambda: next((row for row in mutations() if row["state"] == "conflict" and row["request"].get("name") == "renamed.txt" and "pending rename or move" in (row.get("error") or "")), None), "compound intent preserved with visible automatic-preparation error")
            assert (remote / "compound.txt").read_bytes() == b"external-compound" and not (remote / "renamed.txt").exists()
            assert "pending rename or move" in cli_failure("smart", "resolve", "policy", compound["id"], "--strategy", "replace")
            planned = cli("smart", "resolve", "policy", compound["id"], "--strategy", "keep-both")
            recovered_path = remote / planned["path"]
            wait(lambda: recovered_path.exists() and recovered_path.read_bytes() == b"compound-local-body", "compound content recovered separately")
            assert (remote / "compound.txt").read_bytes() == b"external-compound" and not (remote / "renamed.txt").exists()
            passed("replace refused to discard rename intent and keep-both recovered the compound edit body without changing either namespace target")

            stale = item("blocked.txt")
            (remote / "blocked.txt").write_bytes(b"external-filtered")
            cli("filter", "config", "--profile", "fixture", "--include", "blocked.txt")
            cli("policy", "config", "--profile", "fixture", "--conflicts", "keep-both")
            response, _ = modify("blocked.txt", b"filtered-local-body", stale)
            assert not response.get("ok"), response
            blocked = wait(lambda: next((row for row in mutations() if row["state"] == "conflict" and row["request"].get("name") == "blocked.txt" and "exclude" in (row.get("error") or "")), None), "filtered conflict destination remained stopped")
            assert not list(remote.glob("blocked (conflict-*).txt")) and (remote / "blocked.txt").read_bytes() == b"external-filtered"
            before = manifests("blocked.txt")
            cli("filter", "config", "--profile", "fixture", "--clear-include", "--exclude", "blocked.txt")
            failure = cli_failure("smart", "resolve", "policy", blocked["id"], "--strategy", "replace")
            assert "exclude" in failure and manifests("blocked.txt") == before
            recovered = root / "filtered-local.txt"
            cli("smart", "export", "policy", blocked["id"], str(recovered))
            assert recovered.read_bytes() == b"filtered-local-body"
            passed("source and generated conflict-name filters prevented publication or backup transfer while retaining an exportable edit")
        finally:
            if Fixture.version_gate is not None:
                Fixture.version_gate["release"].set()
            stop()
            diagnostic_mutations = cli("smart", "list", "policy")
            server.shutdown(); server.server_close()
            log.close()
            report = {"checks": checks, "count": len(checks), "passed":len(checks), "binary": report_binary_path(binary),
                      "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "validation_scope":"synthetic authenticated loopback DAV only; no real storage",
                      "cleanup":"broker and HTTP server stopped; temporary state removed by context manager"}
            if len(checks)!=10:
                report.update(mutations=diagnostic_mutations,
                    remote_files={str(p.relative_to(remote)):p.read_bytes().hex() for p in remote.rglob("*") if p.is_file()},
                    bridge_stderr=(root/"bridge.log").read_text())
            if output:
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_text(json.dumps(report, indent=2) + "\n")
    return checks


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    run(options.binary.resolve(), options.output)
