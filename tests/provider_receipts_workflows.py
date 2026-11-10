#!/usr/bin/env python3
"""Adversarial provider receipts using synthetic authenticated loopback DAV.

Publication gates represent independent writers and actual broker SIGKILLs.
Only temporary fixture data is read or changed; no account or mount is used.
"""
import concurrent.futures
import hashlib
import http.server
import json
import os
from pathlib import Path
import plistlib
import posixpath
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit, urlunsplit
import uuid

from cli_workflows import Dav

ROOT = Path(__file__).resolve().parents[1]
TAGS = "com.apple.metadata:_kMDItemUserTags"


class ReceiptDav(Dav):
    gate_lock = threading.Lock()
    scan_race = None
    publication_gate = None
    deletion_gate = None
    metadata_gate = None

    @classmethod
    def gate(cls, name, path):
        gate = dict(path=path, entered=threading.Event(), release=threading.Event())
        with cls.gate_lock:
            assert getattr(cls, name) is None
            setattr(cls, name, gate)
        return gate

    def resource(self, value=None):
        parts = urlsplit(value or self.path)
        path = posixpath.normpath(parts.path)
        if path == "/dav":
            path += "/"
        return super().resource(urlunsplit((parts.scheme, parts.netloc, path, parts.query, parts.fragment)))

    def reply(self, status, body=b"", **headers):
        gate = None
        with self.gate_lock:
            if self.command == "PUT" and 200 <= status < 300 and self.path.startswith("/dav/.rdrive-metadata/"):
                if self.scan_race is not None:
                    self.scan_race["sidecar"] = self.resource()
                if self.metadata_gate is not None:
                    self.metadata_gate["sidecar"] = self.resource()
                    self.metadata_gate["sidecar_url"] = self.path
            if self.command == "MOVE" and 200 <= status < 300:
                candidate = type(self).publication_gate
                if candidate is not None and urlsplit(self.headers.get("Destination", "")).path == candidate["path"]:
                    gate = candidate
                    type(self).publication_gate = None
            if self.command == "DELETE" and 200 <= status < 300:
                candidate = type(self).deletion_gate
                if candidate is not None and self.path == candidate["path"]:
                    gate = candidate
                    type(self).deletion_gate = None
        if gate is not None:
            # The server has completed the change, but its acknowledgement has
            # not reached this client. Other independent clients remain free.
            gate["entered"].set()
            if not gate["release"].wait(35):
                raise AssertionError("publication acknowledgement gate timed out")
        if status == 207:
            body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
        try:
            return super().reply(status, body, **headers)
        except (BrokenPipeError, ConnectionResetError):
            if gate is None and not getattr(self, "expected_disconnect", False):
                raise
            self.close_connection = True

    def handle_dav(self):
        race = None
        gate = None
        with self.gate_lock:
            candidate = type(self).scan_race
            if self.command == "PROPFIND" and self.headers.get("Depth") == "1" and candidate is not None and candidate.get("sidecar") is not None and self.path == candidate["root"]:
                race = candidate
                type(self).scan_race = None
            candidate = type(self).metadata_gate
            if self.command == "PROPFIND" and self.headers.get("Depth") == "1" and candidate is not None and candidate.get("sidecar") is not None and self.path == candidate["path"]:
                gate = candidate
                type(self).metadata_gate = None
        if gate is not None:
            self.expected_disconnect = True
            gate["entered"].set()
            if not gate["release"].wait(35):
                raise AssertionError("metadata final-refresh gate timed out")
        if race is not None:
            # All upload/sidecar confirmation reads finished. Publish B at the
            # precise boundary before the provider builds its final response.
            if race["kind"] == "body":
                race["target"].write_bytes(race["replacement"])
            else:
                value = json.loads(race["sidecar"].read_text())
                value["record"]["attributes"][TAGS] = race["replacement"]
                race["sidecar"].write_text(json.dumps(value))
            race["fired"].set()
        return super().handle_dav()


def tags(value):
    return plistlib.dumps([value], fmt=plistlib.FMT_BINARY, sort_keys=False).hex()


def run(binary):
    checks = []
    with tempfile.TemporaryDirectory(prefix="rdrive-receipts-") as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        remote.mkdir()
        for name in ("body", "tags", "gone", "replaced", "tag-crash", "chain", "managed"):
            (remote / name).mkdir()
            if name != "managed":
                (remote / name / "doc.txt").write_bytes(b"original")
        ReceiptDav.root = remote
        ReceiptDav.requests_seen = []
        ReceiptDav.locks = {}
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ReceiptDav)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        home = root / "state-a"
        other_home = root / "state-b"
        ipc = root / "ipc"
        ipc.mkdir(mode=0o700)
        stage = ipc / "Transfers"
        socket_path = ipc / "bridge.sock"
        env = dict(os.environ, RDRIVE_RECEIPT_PASSWORD="fixture-password")
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        process = None
        log = (root / "bridge.log").open("w+")

        def cli(*args, other=False):
            deadline = time.monotonic() + 45
            while True:
                result = subprocess.run([str(binary), "--home", str(other_home if other else home), "--json", "--progress", "off", *map(str, args)], env=env, capture_output=True, text=True, timeout=60)
                if result.returncode == 0:
                    return json.loads(result.stdout)
                if not any(text in result.stderr for text in ("another rdrive process", "already locked", "state directory is in use")):
                    raise AssertionError((args, result.stderr, result.stdout))
                assert time.monotonic() < deadline, result.stderr
                time.sleep(.1)

        def rpc(**message):
            descriptor = json.loads((ipc / "bridge-endpoint.json").read_text())
            message.update(v=1, auth_token=descriptor["token"])
            message.setdefault("request_id", uuid.uuid4().hex)
            with socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=45) as client:
                client.sendall(json.dumps(message).encode() + b"\n")
                with client.makefile("rb") as reader:
                    while True:
                        line = reader.readline()
                        assert line, "broker disconnected before reply"
                        value = json.loads(line)
                        if "progress" not in value:
                            return value

        def ok(**message):
            response = rpc(**message)
            assert response.get("ok"), response
            return response

        def wait(check, label, seconds=45):
            deadline = time.monotonic() + seconds
            while True:
                result = check()
                if result:
                    return result
                assert time.monotonic() < deadline, label
                time.sleep(.1)

        def start():
            nonlocal process
            process = subprocess.Popen([str(binary), "--home", str(home), "--json", "--progress", "off", "provider", "serve", "--socket", str(socket_path), "--staging", str(stage), "--development-tcp"], env=env, stdout=subprocess.DEVNULL, stderr=log)
            def ready():
                assert process.poll() is None, "broker exited"
                try:
                    return rpc(op="domains").get("ok")
                except (OSError, json.JSONDecodeError, AssertionError):
                    return False
            wait(ready, "broker startup", 15)

        def stop(crash=False):
            nonlocal process
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGKILL if crash else signal.SIGINT)
                process.wait(timeout=15)
            process = None

        def listed(domain):
            def found():
                items = ok(op="list", domain=domain)["items"]
                return next((item for item in items if item["name"] == "doc.txt"), None)
            return wait(found, "catalog " + domain)

        def mutations(domain):
            path = home / "smart" / hashlib.sha256(domain.encode()).hexdigest() / "queue.json"
            if not path.exists():
                return []
            return sorted(json.loads(path.read_text())["mutations"].values(), key=lambda value: value["sequence"])

        def assert_conflict(domain, response, contents=None):
            assert not response["ok"] and response["error"]["code"] == "version_conflict", response
            mutation = mutations(domain)[-1]
            assert mutation["state"] == "conflict" and mutation["response"] is None, mutation
            if contents is not None:
                snapshot = home / "smart" / hashlib.sha256(domain.encode()).hexdigest() / "dirty" / mutation["snapshot"]["file"]
                assert snapshot.read_bytes() == contents
            journal = json.loads((home / "provider" / (hashlib.sha256(domain.encode()).hexdigest() + ".json")).read_text())
            plan = journal["smart_replays"].get(mutation["id"], mutation["request"])
            identity = "create:" + plan["client_item_id"] if plan["op"] == "create" else plan["request_id"]
            key = hashlib.sha256(domain.encode()).hexdigest() + ":" + identity
            assert journal["requests"].get(key, {}).get("response") is None, "external B was stored as our receipt"

        def passed(name):
            checks.append(name)
            print("PASS " + name, flush=True)

        try:
            for other in (False, True):
                cli("profile", "add", "fixture", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/", "--allow-http", "--username", "user", "--password-env", "RDRIVE_RECEIPT_PASSWORD", other=other)
            for domain in ("body", "tags", "gone", "replaced", "tag-crash", "chain"):
                cli("smart", "setup", domain, "fixture", domain)
            cli("repo", "init", "fixture", "managed")
            initial = root / "initial"
            initial.write_bytes(b"original managed")
            transaction = cli("repo", "begin", "fixture", "managed")["id"]
            cli("repo", "put", transaction, initial, "doc.txt")
            cli("repo", "commit", transaction)
            cli("smart", "setup", "managed", "fixture", "managed", "--repository")
            start()

            item = listed("body")
            source = stage / "raw-a"
            source.write_bytes(b"own raw A")
            race = dict(root="/dav/body/", target=remote / "body/doc.txt", kind="body", replacement=b"external raw B", fired=threading.Event())
            ReceiptDav.scan_race = race
            response = rpc(op="modify", domain="body", id=item["id"], content_path=str(source), base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])
            assert race["fired"].is_set(), "body race did not reach final-refresh boundary"
            assert_conflict("body", response, b"own raw A")
            assert (remote / "body/doc.txt").read_bytes() == b"external raw B"
            passed("raw DAV external body after publication is rejected and local A remains exportable")

            item = listed("tags")
            external_tags = tags("external B\n3")
            race = dict(root="/dav/tags/", target=remote / "tags/doc.txt", kind="metadata", replacement=external_tags, fired=threading.Event())
            ReceiptDav.scan_race = race
            response = rpc(op="modify", domain="tags", id=item["id"], tag_data_hex=tags("own A\n2"), base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])
            assert race["fired"].is_set(), "metadata race did not reach final-refresh boundary"
            assert_conflict("tags", response)
            assert json.loads(race["sidecar"].read_text())["record"]["attributes"][TAGS] == external_tags
            assert (remote / "tags/doc.txt").read_bytes() == b"original"
            passed("raw DAV external tags after publication cannot become our successful metadata receipt")

            item = listed("managed")
            source = stage / "managed-a"
            source.write_bytes(b"own managed A")
            gate = ReceiptDav.gate("publication_gate", "/dav/managed/.rdrive-repository/commits/00000000000000000002.json")
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(rpc, op="modify", domain="managed", id=item["id"], content_path=str(source), base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])
                try:
                    assert gate["entered"].wait(25), "managed publication gate not reached"
                    competitor = root / "managed-b"
                    competitor.write_bytes(b"external managed B")
                    transaction = cli("repo", "begin", "fixture", "managed", other=True)["id"]
                    cli("repo", "put", transaction, competitor, "doc.txt", other=True)
                    assert cli("repo", "commit", transaction, other=True)["commit"]["sequence"] == 3
                finally:
                    gate["release"].set()
                response = pending.result(timeout=45)
            assert_conflict("managed", response, b"own managed A")
            output = root / "managed-result"
            cli("repo", "get", "fixture", "doc.txt", output, "--root", "managed", other=True)
            assert output.read_bytes() == b"external managed B"
            passed("managed competing commit before final response cannot be misattributed as our receipt")

            item = listed("chain")
            source = stage / "chain-edit"
            source.write_bytes(b"own chain A")
            saved = ok(op="modify", domain="chain", id=item["id"], content_path=str(source), base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])["item"]
            original_base = dict(base_content_version=saved["content_version"], base_metadata_version=saved["metadata_version"])
            own_tags = tags("own reachable tags\n2")
            tagged = ok(op="modify", domain="chain", id=item["id"], tag_data_hex=own_tags, **original_base)["item"]
            attributes = {"user.rdrive.receipt#S": b"reachable attribute".hex()}
            added = ok(op="modify", domain="chain", id=item["id"], extended_attributes=attributes, **original_base)["item"]
            assert added["extended_attributes"] == attributes and added["tag_data_hex"] == own_tags
            removed = ok(op="modify", domain="chain", id=item["id"], extended_attributes={}, base_content_version=added["content_version"], base_metadata_version=added["metadata_version"])["item"]
            assert removed["extended_attributes"] == {} and removed["tag_data_hex"] == own_tags
            assert removed["metadata_version"].split(":")[0] == tagged["metadata_version"].split(":")[0]
            source.write_bytes(b"own chain B after tags and attribute removal")
            saved_again = ok(op="modify", domain="chain", id=item["id"], content_path=str(source), **original_base)["item"]
            assert saved_again["tag_data_hex"] == own_tags and saved_again["extended_attributes"] == {}
            assert (remote / "chain/doc.txt").read_bytes() == b"own chain B after tags and attribute removal"
            sidecars = []
            for path in (remote / ".rdrive-metadata").rglob("*.json"):
                value = json.loads(path.read_text())
                if value["path"] == "chain/doc.txt" and value["record"]["content_sha256"] == hashlib.sha256(b"own chain B after tags and attribute removal").hexdigest():
                    sidecars.append((path, value))
            assert len(sidecars) == 1
            sidecar, external = sidecars[0]
            external_tags = tags("external unreachable tags\n3")
            external["record"]["attributes"][TAGS] = external_tags
            sidecar.write_text(json.dumps(external))
            source.write_bytes(b"retained C must not overwrite external metadata")
            response = rpc(op="modify", domain="chain", id=item["id"], content_path=str(source), **original_base)
            assert_conflict("chain", response, b"retained C must not overwrite external metadata")
            assert (remote / "chain/doc.txt").read_bytes() == b"own chain B after tags and attribute removal"
            assert json.loads(sidecar.read_text())["record"]["attributes"][TAGS] == external_tags
            passed("confirmed receipt graph crosses own tags and attribute-removal saves but refuses unrelated external metadata")

            for domain, replacement in (("gone", None), ("replaced", b"same-path replacement B")):
                item = listed(domain)
                path = f"/dav/{domain}/doc.txt"
                gate = ReceiptDav.gate("deletion_gate", path)
                before = ReceiptDav.requests_seen.count(("DELETE", path))
                message = dict(op="delete", domain=domain, id=item["id"], base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])
                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                    pending = pool.submit(rpc, **message)
                    try:
                        assert gate["entered"].wait(20), "delete publication gate not reached"
                        stop(crash=True)
                        if replacement is not None:
                            (remote / domain / "doc.txt").write_bytes(replacement)
                    finally:
                        gate["release"].set()
                    try:
                        pending.result(timeout=10)
                    except (AssertionError, OSError):
                        pass
                start()
                expected_state = "done" if replacement is None else "conflict"
                wait(lambda: any(m["state"] == expected_state for m in mutations(domain)), "delete recovery " + domain)
                assert ReceiptDav.requests_seen.count(("DELETE", path)) == before + 1, "replay issued a second deletion"
                if replacement is None:
                    assert not (remote / domain / "doc.txt").exists()
                    assert ok(**message)["ok"]
                    passed("actual broker SIGKILL after DAV delete recovers absence without issuing a second DELETE")
                else:
                    assert (remote / domain / "doc.txt").read_bytes() == replacement
                    assert not rpc(**message)["ok"]
                    passed("lost DAV delete acknowledgement preserves a same-path replacement after broker restart")

            item = listed("tag-crash")
            desired_tags = tags("committed before crash\n4")
            gate = ReceiptDav.gate("metadata_gate", "/dav/tag-crash/")
            message = dict(op="modify", domain="tag-crash", id=item["id"], tag_data_hex=desired_tags, base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(rpc, **message)
                try:
                    assert gate["entered"].wait(20), "metadata confirmation gate not reached"
                    assert json.loads(gate["sidecar"].read_text())["record"]["attributes"][TAGS] == desired_tags
                    assert mutations("tag-crash")[0]["state"] == "running"
                    stop(crash=True)
                finally:
                    gate["release"].set()
                try:
                    pending.result(timeout=10)
                except (AssertionError, OSError):
                    pass
            before = ReceiptDav.requests_seen.count(("PUT", gate["sidecar_url"]))
            assert before == 1, "metadata publication gate did not follow exactly one write"
            start()
            wait(lambda: any(m["state"] == "done" for m in mutations("tag-crash")), "metadata-only crash recovery")
            receipt = ok(**message)
            assert receipt["item"]["tag_data_hex"] == desired_tags
            assert json.loads(gate["sidecar"].read_text())["record"]["attributes"][TAGS] == desired_tags
            assert ReceiptDav.requests_seen.count(("PUT", gate["sidecar_url"])) == before, "recovery needlessly republished committed tags"
            assert (remote / "tag-crash/doc.txt").read_bytes() == b"original"
            passed("SIGKILL after metadata-only publication reconciles exact own tags without conflict or a second PUT")
        finally:
            for name in ("publication_gate", "deletion_gate", "metadata_gate"):
                gate = getattr(ReceiptDav, name)
                if gate is not None:
                    gate["release"].set()
            stop()
            server.shutdown()
            server.server_close()
            log.close()
    with binary.open("rb") as executable:
        binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
    version = subprocess.run([str(binary), "--version"], capture_output=True, text=True, check=True, timeout=10).stdout.strip()
    report = dict(binary=str(binary), binary_sha256=binary_sha256, version=version, checks=checks, passed=len(checks), scope="synthetic authenticated loopback DAV, independent writer home, actual broker SIGKILL; no real storage")
    destination = ROOT / "docs/PROVIDER_RECEIPTS_VALIDATION.json"
    destination.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    run(Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/release/rdrive").resolve())
