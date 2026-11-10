#!/usr/bin/env python3
"""Compile and exercise native SDK objects against an isolated Unix-socket peer."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import uuid

ROOT = Path(__file__).resolve().parents[1]


def main():
    output = ROOT / "build"
    output.mkdir(parents=True, exist_ok=True)
    binary = output / "RDriveNativeTests"
    sdk = subprocess.check_output(["xcrun", "--show-sdk-path"], text=True).strip()
    architecture = subprocess.check_output(["uname", "-m"], text=True).strip()
    subprocess.run(["xcrun", "swiftc", "-swift-version", "5", "-sdk", sdk, "-target", architecture + "-apple-macosx13.0",
                    "-parse-as-library", "-module-cache-path", str(output / "module-cache"), "-module-name", "RDriveNativeTests",
                    str(ROOT / "Shared/Bridge.swift"), str(ROOT / "Shared/TagData.swift"), str(ROOT / "Shared/NativeBudget.swift"), str(ROOT / "FileProvider/ProviderItem.swift"),
                    str(ROOT / "FileProvider/Enumerator.swift"), str(ROOT / "tests/NativeTests.swift"),
                    "-framework", "Foundation", "-framework", "FileProvider", "-framework", "UniformTypeIdentifiers",
                    "-o", str(binary)], check=True)
    errors = []
    calls = []
    with tempfile.TemporaryDirectory(prefix="rdnative-", dir="/private/tmp") as directory:
        group = Path(directory)
        group.chmod(0o700)
        def item(index):
            return {"id": "stable-" + str(index), "parent_id": "root", "name": str(index) + ".txt", "kind": "file", "size": 19,
                    "modified": None, "content_version": "v1", "metadata_version": "m1", "pinned": False, "capabilities": ["read"]}
        def serve():
            while not stopped.is_set():
                try:
                    client, _ = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    return
                try:
                    with client:
                        raw = b""
                        while b"\n" not in raw:
                            part = client.recv(65536)
                            if not part:
                                break
                            raw += part
                        request = json.loads(raw.split(b"\n")[0])
                        calls.append(request)
                        assert request["v"] == 1 and request.get("request_id")
                        if token is not None:
                            assert request.get("auth_token") == token, "Missing private local IPC nonce"
                        if request["op"] == "test.cancel":
                            while client.recv(1024):
                                pass
                            continue
                        if request["op"] == "test.fragmented":
                            frames = [{"progress": {"completed": 19, "total": 19}}, {"ok": True, "value": "fragmented reply"}]
                        elif request["op"] == "test.authentication":
                            frames = [{"ok": False, "error": {"code": "not_authenticated", "message": "Synthetic test failure"}}]
                        elif request["op"] == "list":
                            frames = [{"ok": True, "items": [item(i) for i in range(3)], "anchor": "2"}]
                        elif request["op"] == "changes":
                            if request.get("parent_id") == "folder-one":
                                child = item(4); child["parent_id"] = "folder-one"
                                frames = [{"ok": True, "items": [child, item(5)], "deleted": [], "anchor": "2", "more": False}]
                            else:
                                frames = [{"ok": True, "items": [item(i) for i in range(3)], "deleted": ["old-id"], "anchor": "999" if request.get("anchor") == "current" else "2", "more": False}]
                        else:
                            raise AssertionError("Unexpected native operation: " + str(request.get("op")))
                        payload = b"".join(json.dumps(frame).encode() + b"\n" for frame in frames)
                        for start in range(0, len(payload), 7):
                            client.sendall(payload[start:start + 7])
                except Exception as error:
                    errors.append(error)
        for transport in ["unix", "development-tcp"]:
            errors.clear(); calls.clear()
            token = uuid.uuid4().hex + uuid.uuid4().hex if transport == "development-tcp" else None
            listener = socket.socket(socket.AF_INET if token else socket.AF_UNIX, socket.SOCK_STREAM)
            if token:
                listener.bind(("127.0.0.1", 0))
                endpoint = group / "bridge-endpoint.json"
                endpoint.write_text(json.dumps({"transport": "tcp", "host": "127.0.0.1", "port": listener.getsockname()[1], "token": token}))
                endpoint.chmod(0o600)
            else:
                listener.bind(str(group / "bridge.sock"))
                os.chmod(group / "bridge.sock", 0o600)
            listener.listen(); listener.settimeout(0.2)
            stopped = threading.Event()
            worker = threading.Thread(target=serve, daemon=True)
            worker.start()
            try:
                arguments = [str(binary), str(group)] + (["--development-tcp"] if token else [])
                subprocess.run(arguments, check=True, timeout=30)
                assert not errors, errors
                assert len([c for c in calls if c["op"] == "list"]) == 1, "Paging must reuse one stable snapshot"
            finally:
                stopped.set(); listener.close(); worker.join(timeout=2)


if __name__ == "__main__":
    main()
