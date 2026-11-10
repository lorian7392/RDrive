#!/usr/bin/env python3
"""Actual macOS File Provider + Rust bridge + authenticated loopback DAV workflow.

Only synthetic files/accounts and one temporary domain are used. Requires an
explicit development-IPC build. No File Provider testing mode or global access
permission is enabled. --hold-for-ui keeps the fixture alive briefly for Finder
menu/badge verification; press Enter to clean it up.
"""
import argparse
import http.server
import importlib.util
import json
from index_workflows import provider_journal_from_db
import os
import plistlib
from pathlib import Path
import select
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from advanced_workflows import set_xattr, get_xattr, remove_xattr

ROOT = Path(__file__).resolve().parents[1]
LSREGISTER = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
DATALESS = getattr(stat, "SF_DATALESS", 0x40000000)


def wait_for(check, label, seconds=45):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (FileNotFoundError, OSError) as error:
            last = error
        time.sleep(0.2)
    raise AssertionError(label + " timed out" + (": " + str(last) if last else ""))


def rpc(endpoint, **request):
    request.update(v=1, request_id=uuid.uuid4().hex)
    descriptor = endpoint.parent / "bridge-endpoint.json"
    settings = json.loads(descriptor.read_text()) if descriptor.exists() else None
    if settings:
        assert settings["host"] == "127.0.0.1" and settings["transport"] == "tcp"
        request["auth_token"] = settings["token"]
    with socket.socket(socket.AF_INET if settings else socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(30)
        connection.connect(("127.0.0.1", settings["port"]) if settings else str(endpoint))
        connection.sendall(json.dumps(request).encode() + b"\n")
        with connection.makefile("rb") as stream:
            for line in stream:
                reply = json.loads(line)
                if "progress" in reply:
                    continue
                if not reply.get("ok"):
                    raise AssertionError("Bridge failed: " + str(reply.get("error")))
                return reply
    raise AssertionError("Bridge disconnected without a final reply")


def run_tests(options):
    app = options.app.resolve()
    binary = options.binary.resolve()
    control = app / "Contents/MacOS/RDriveCompanion"
    ipc = options.ipc.resolve()
    ipc.mkdir(mode=0o700, parents=True, exist_ok=True)
    endpoint = ipc / "bridge.sock"
    transfers = ipc / "Transfers"
    if endpoint.exists() or (ipc / "bridge-endpoint.json").exists():
        try:
            rpc(endpoint, op="domains")
        except (OSError, AssertionError):
            pass  # The Rust service independently verifies and replaces stale sockets.
        else:
            raise RuntimeError("A bridge is already running; refusing to replace another service")
    spec = importlib.util.spec_from_file_location("dav_fixture", ROOT / "tests/cli_workflows.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    domain = "native_fp_" + uuid.uuid4().hex[:12]
    registered = False
    process = None
    host_process = None
    created = None
    passed = []
    report = {"domain": domain, "checks": passed, "development_ipc_root": str(ipc)}
    roots_path = ipc / "finder-roots.json"
    prior_roots = roots_path.read_bytes() if roots_path.exists() else None
    with tempfile.TemporaryDirectory(prefix="rdrive-provider-native-") as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        remote.mkdir()
        content = b"Actual on-demand File Provider bytes\n" * 4096
        (remote / "hello.txt").write_bytes(content)
        second_content = b"Second native cache file\n" * 4096
        offline_content = b"Actual native offline edit survived restart\n"
        if options.extended_smart:
            (remote / "second.txt").write_bytes(second_content)
            (remote / "too-large.bin").write_bytes(b"x" * (len(content) + 4097))
            (remote / "pin-folder").mkdir()
            (remote / "pin-folder/a.txt").write_bytes(b"Pinned child A\n")
            (remote / "pin-folder/b.txt").write_bytes(b"Pinned child B\n")
        class NativeDav(module.Dav):
            requests_seen = []
            offline = False
            def handle_dav(self):
                if self.offline:
                    self.requests_seen.append((self.command, self.path))
                    self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    return self.reply(503)
                return super().handle_dav()
            def reply(self, status, body=b"", **headers):
                # The shared fixture formats its root as /dav/./. Native bridge
                # traversal correctly rejects that; emit the canonical DAV root.
                if status == 207:
                    body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
                return super().reply(status, body, **headers)
        NativeDav.root = remote
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), NativeDav)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        home = root / "state"
        env = dict(os.environ, RDRIVE_NATIVE_TEST_PASSWORD="fixture-password")
        cli = [str(binary), "--home", str(home), "--json"]
        log_path = root / "bridge.stderr"
        log = log_path.open("w+")

        def native(*arguments, success=True):
            result = subprocess.run([str(control), *arguments], capture_output=True, text=True, timeout=40)
            lines = result.stdout.strip().splitlines()
            value = json.loads(lines[-1]) if lines else {}
            if success and (result.returncode != 0 or value.get("ok") is not True):
                raise AssertionError("Native control failed: " + str(arguments) + "\n" + result.stdout + result.stderr)
            return value

        def start_bridge():
            nonlocal process
            process = subprocess.Popen(cli + ["provider", "serve", "--socket", str(endpoint), "--staging", str(transfers), "--development-tcp"],
                                       env=env, stdout=subprocess.DEVNULL, stderr=log)
            def ready():
                if process.poll() is not None:
                    log.flush()
                    raise RuntimeError("Rust bridge exited: " + log_path.read_text())
                try:
                    return rpc(endpoint, op="domains")
                except OSError:
                    return None
            return wait_for(ready, "Rust bridge startup", 15)

        def stop_bridge():
            nonlocal process
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=5)
            process = None

        def passed_test(label):
            passed.append(label)
            print("PASS " + label, flush=True)

        def flags(path):
            result = subprocess.run(["python3", "-c", "import os,sys;print(os.stat(sys.argv[1]).st_flags)", str(path)], timeout=30, capture_output=True, text=True)
            if result.returncode != 0:
                raise OSError(result.stderr.strip())
            return result.stdout.strip()

        try:
            subprocess.run(cli + ["profile", "add", domain, "--protocol", "webdav", "--url",
                                  f"http://127.0.0.1:{server.server_port}/dav/", "--username", "user",
                                  "--password-env", "RDRIVE_NATIVE_TEST_PASSWORD", "--allow-http"],
                           env=env, check=True, capture_output=True, timeout=15)
            if options.smart:
                subprocess.run(cli + ["provider", "domain-remove", domain], env=env, check=True, capture_output=True, timeout=15)
                subprocess.run(cli + ["provider", "domain-add", domain, domain, "", "--smart"], env=env, check=True, capture_output=True, timeout=15)
            if options.cache_bytes is not None:
                subprocess.run(cli + ["cache", "config", "--max-bytes", str(options.cache_bytes)], env=env, check=True, capture_output=True, timeout=15)
            start_bridge()
            def initial_catalog():
                value = rpc(endpoint, op="list", domain=domain, parent_id="root")
                return value if any(item["name"] == "hello.txt" for item in value["items"]) else None
            initial = wait_for(initial_catalog, "initial complete native catalog")
            item = next(item for item in initial["items"] if item["name"] == "hello.txt")
            item_id = item["id"]
            assert not [r for r in NativeDav.requests_seen if r[0] == "GET"], "Metadata enumeration downloaded file contents"
            subprocess.run([LSREGISTER, "-f", str(app)], check=True, capture_output=True, timeout=20)
            for plugin in ["RDriveProvider.appex", "RDriveFinder.appex", "RDriveStatus.appex"]:
                subprocess.run(["pluginkit", "-a", str(app / "Contents/PlugIns" / plugin)], check=True, capture_output=True, timeout=20)
            native("--register-domain", domain, "--display-name", "RDrive Native Fixture " + domain[-6:])
            registered = True
            domain_root = Path(native("--domain-root", domain)["path"])
            report["domain_root"] = str(domain_root)
            host_process = subprocess.Popen([str(control), "--group-directory", str(ipc), "--watch-domains"],
                env=dict(env, RDRIVE_HOME=str(home)), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            report["native_cli_watcher_started"] = True
            hello = domain_root / "hello.txt"
            listing = subprocess.run(["python3", "-c", "import os,sys,json;print(json.dumps(os.listdir(sys.argv[1])))", str(domain_root)], capture_output=True, text=True, timeout=60)
            assert listing.returncode == 0 and "hello.txt" in json.loads(listing.stdout), listing.stdout + listing.stderr
            before = int(flags(hello))
            report["placeholder_flags"] = before
            assert before & DATALESS, "File was not a real macOS dataless placeholder: " + hex(before)
            assert not [r for r in NativeDav.requests_seen if r[0] == "GET"], "Listing triggered a content download"
            passed_test("actual macOS placeholder enumeration without content GET")

            result = subprocess.check_output(["python3", "-c", "import pathlib,sys;sys.stdout.buffer.write(pathlib.Path(sys.argv[1]).read_bytes())", str(hello)], timeout=60)
            assert result == content
            assert not int(flags(hello)) & DATALESS
            assert [r for r in NativeDav.requests_seen if r[0] == "GET" and r[1] == "/dav/hello.txt"]
            passed_test("opening a placeholder hydrated the exact remote bytes through Swift and Rust")

            wait_for(lambda: native("--evict", domain, item_id, success=False).get("ok") is True, "native eviction request after system hydration settled")
            wait_for(lambda: int(flags(hello)) & DATALESS, "native eviction")
            passed_test("native eviction restored a dataless file")

            rpc(endpoint, op="pin", domain=domain, id=item_id, pinned=True)
            native("--signal", domain)
            wait_for(lambda: not int(flags(hello)) & DATALESS, "pin-triggered eager materialization")
            def pinned_eviction_result():
                value = native("--evict", domain, item_id, success=False)
                error = value.get("error", {})
                return value if error.get("domain") == "NSFileProviderErrorDomain" and error.get("code") == -2008 else None
            pinned_eviction = wait_for(pinned_eviction_result, "native non-evictable offline policy")
            report["pinned_eviction"] = pinned_eviction
            assert pinned_eviction.get("ok") is False, "Pinned native file remained evictable"
            passed_test("Keep Offline eagerly hydrated the file and prevented eviction")

            rpc(endpoint, op="pin", domain=domain, id=item_id, pinned=False)
            native("--signal", domain)
            def evicted():
                value = native("--evict", domain, item_id, success=False)
                return value.get("ok") is True and int(flags(hello)) & DATALESS
            wait_for(evicted, "unpin followed by native eviction")
            assert subprocess.check_output(["python3", "-c", "import pathlib,sys;sys.stdout.buffer.write(pathlib.Path(sys.argv[1]).read_bytes())", str(hello)], timeout=60) == content
            passed_test("unpin allowed eviction and subsequent open downloaded the file again")

            if options.view_only:
                rpc(endpoint, op="pin", domain=domain, id=item_id, pinned=True)
                native("--signal", domain)
                if options.hold_for_ui:
                    roots_path.write_text(json.dumps([{"path": str(domain_root), "domain": domain}]))
                    roots_path.chmod(0o600)
                    print("HOLD_FOR_UI " + json.dumps({"domain": domain, "root": str(domain_root), "item": item_id, "socket": str(endpoint)}), flush=True)
                    readable, _, _ = select.select([sys.stdin], [], [], options.hold_seconds)
                    if readable:
                        sys.stdin.readline()
                return

            created = domain_root / "created.txt"
            created.write_text("Native callback created this file\n")
            wait_for(lambda: (remote / "created.txt").exists(), "native create upload")
            wait_for(lambda: (remote / "created.txt").read_text() == "Native callback created this file\n", "native create bytes")
            def created_catalog_item():
                return next((i for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "created.txt"), None)
            created_item = wait_for(created_catalog_item, "created file catalog refresh")
            wait_for(lambda: native("--resolve-file", str(created), success=False).get("id") == created_item["id"], "native provider identity assignment")
            native("--set-tags", str(created), "Blue")
            expected_tags = plistlib.loads(get_xattr(created, "com.apple.metadata:_kMDItemUserTags"))
            assert [name.split("\n")[0] for name in expected_tags] == ["Blue"]
            report["expected_local_tags"] = expected_tags
            def tags_saved():
                value = rpc(endpoint, op="item", domain=domain, id=created_item["id"])["item"].get("tag_data_hex")
                if not value:
                    return False
                report["native_tag_data_hex"] = value
                decoded = plistlib.loads(bytes.fromhex(value))
                report["native_tag_decoded"] = repr(decoded)
                if not isinstance(decoded, list):
                    raise AssertionError("Native tagData uses a different representation: " + repr(decoded)[:1200])
                return decoded == expected_tags
            wait_for(tags_saved, "native Finder tag update")
            set_xattr(created, "user.rdrive.test#S", b"native syncable attribute")
            wait_for(lambda: rpc(endpoint, op="item", domain=domain, id=created_item["id"])["item"].get("extended_attributes", {}).get("user.rdrive.test#S") == b"native syncable attribute".hex(), "native syncable xattr update")
            remove_xattr(created, "user.rdrive.test#S")
            wait_for(lambda: "user.rdrive.test#S" not in rpc(endpoint, op="item", domain=domain, id=created_item["id"])["item"].get("extended_attributes", {}), "native syncable xattr removal")
            assert tags_saved(), "Syncable xattr removal erased Finder tags"
            passed_test("native Finder tags and syncable user xattr changes reached metadata sidecars")
            created.write_text("Native callback modified this file\n")
            wait_for(lambda: (remote / "created.txt").read_text() == "Native callback modified this file\n", "native modify upload")
            created.unlink()
            wait_for(lambda: not (remote / "created.txt").exists(), "native delete")
            passed_test("native create, version-guarded modify and delete reached the DAV fixture")

            if options.extended_smart:
                def trim_to_zero():
                    return native("--trim-cache", "0", success=False).get("ok") is True
                wait_for(trim_to_zero, "initial native cache trim", 60)
                subprocess.run(cli + ["cache", "config", "--max-bytes", str(len(content))], env=env, check=True, capture_output=True, timeout=15)
                assert hello.read_bytes() == content
                second = domain_root / "second.txt"
                assert second.read_bytes() == second_content
                wait_for(lambda: int(flags(hello)) & DATALESS, "LRU native eviction to admit another file")
                native("--pin", domain, next(i["id"] for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "second.txt"))
                wait_for(lambda: rpc(endpoint, op="item", domain=domain, id=next(i["id"] for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "second.txt"))["item"]["pinned"], "second file pin")
                assert native("--trim-cache", "0", success=False).get("ok") is False
                assert second.read_bytes() == second_content and not int(flags(second)) & DATALESS
                large_item = next(i for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "too-large.bin")
                large_preflight = native("--check-cache", domain, large_item["id"], success=False)
                report["oversized_cache_preflight"] = large_preflight
                assert large_preflight.get("ok") is False
                assert not [r for r in NativeDav.requests_seen if r[0] == "GET" and r[1] == "/dav/too-large.bin"]
                native("--unpin", domain, next(i["id"] for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "second.txt"))
                wait_for(trim_to_zero, "unpin and native cache reclaim", 60)
                assert int(flags(second)) & DATALESS
                report["cache_after_reclaim"] = rpc(endpoint, op="cache_budget")
                assert report["cache_after_reclaim"]["used_bytes"] == 0
                passed_test("fixed native cache quota evicted LRU content, protected pins, rejected oversize before GET and reclaimed through OS eviction")

                folder = next(i for i in rpc(endpoint, op="list", domain=domain, parent_id="root")["items"] if i["name"] == "pin-folder")
                native("--pin", domain, folder["id"])
                for child in ["a.txt", "b.txt"]:
                    wait_for(lambda child=child: not int(flags(domain_root / "pin-folder" / child)) & DATALESS, "folder pin eagerly downloaded " + child, 60)
                (remote / "pin-folder/c.txt").write_bytes(b"Future pinned child C\n")
                wait_for(lambda: not int(flags(domain_root / "pin-folder/c.txt")) & DATALESS, "future descendant inherited offline policy", 60)
                def current_child():
                    return next(i for i in rpc(endpoint, op="list", domain=domain, parent_id=folder["id"])["items"] if i["name"] == "c.txt")
                previous_version = current_child()["content_version"]
                previous_gets = len([r for r in NativeDav.requests_seen if r == ("GET", "/dav/pin-folder/c.txt")])
                updated_child = b"Remote pinned child D\n"
                assert len(updated_child) == len(b"Future pinned child C\n")
                (remote / "pin-folder/c.txt").write_bytes(updated_child)
                def refreshed_child():
                    child = current_child()
                    gets = len([r for r in NativeDav.requests_seen if r == ("GET", "/dav/pin-folder/c.txt")])
                    return child["content_version"] != previous_version and child.get("native_materialized") and gets > previous_gets
                wait_for(refreshed_child, "pinned same-size remote update applied", 90)
                before_read_gets = len([r for r in NativeDav.requests_seen if r == ("GET", "/dav/pin-folder/c.txt")])
                assert (domain_root / "pin-folder/c.txt").read_bytes() == updated_child
                assert len([r for r in NativeDav.requests_seen if r == ("GET", "/dav/pin-folder/c.txt")]) == before_read_gets, "Reading the claimed cached version triggered another GET"
                report["pinned_same_size_remote_update_verified"] = True
                assert native("--trim-cache", "0", success=False).get("ok") is False
                native("--unpin", domain, folder["id"])
                wait_for(trim_to_zero, "folder unpin and safe native reclaim", 60)
                passed_test("finite-budget folder pin preflight hydrated known children and a future descendant inherited the policy")

                assert hello.read_bytes() == content
                NativeDav.offline = True
                listed = subprocess.check_output(["python3", "-c", "import os,sys,json;print(json.dumps(os.listdir(sys.argv[1])))", str(domain_root)], timeout=10)
                assert "hello.txt" in json.loads(listed)
                hello.write_bytes(offline_content)
                wait_for(lambda: rpc(endpoint, op="item", domain=domain, id=item_id)["item"].get("has_unsynced_changes"), "durable native offline edit", 60)
                native("--trim-cache", "0", success=False)
                assert hello.read_bytes() == offline_content and not int(flags(hello)) & DATALESS
                stop_bridge(); start_bridge()
                assert hello.read_bytes() == offline_content
                NativeDav.offline = False
                wait_for(lambda: (remote / "hello.txt").read_bytes() == offline_content, "automatic native edit replay after reconnect", 90)
                wait_for(lambda: not rpc(endpoint, op="item", domain=domain, id=item_id)["item"].get("has_unsynced_changes"), "native pending state cleared", 60)
                hello.write_bytes(content)
                wait_for(lambda: (remote / "hello.txt").read_bytes() == content, "restore synthetic content through native replay", 60)
                passed_test("native Smart catalog stayed available offline and an offline edit survived service restart then replayed automatically")

            rpc(endpoint, op="pin", domain=domain, id=item_id, pinned=True)
            native("--signal", domain)
            stop_bridge()
            assert subprocess.check_output(["python3", "-c", "import pathlib,sys;sys.stdout.buffer.write(pathlib.Path(sys.argv[1]).read_bytes())", str(hello)], timeout=15) == content
            passed_test("materialized file remained readable with the backend service stopped")
            if options.hold_for_ui:
                start_bridge()
                roots_path.write_text(json.dumps([{"path": str(domain_root), "domain": domain}]))
                roots_path.chmod(0o600)
                print("HOLD_FOR_UI " + json.dumps({"domain": domain, "root": str(domain_root), "item": item_id, "socket": str(endpoint)}), flush=True)
                readable, _, _ = select.select([sys.stdin], [], [], options.hold_seconds)
                if readable:
                    sys.stdin.readline()
        except BaseException as error:
            report["failure"] = str(error)
            if options.hold_for_ui and options.hold_on_failure and registered and report.get("domain_root"):
                print("HOLD_FOR_UI_FAILURE " + json.dumps({"domain": domain, "root": report["domain_root"], "item": item_id, "error": str(error)}), flush=True)
                readable, _, _ = select.select([sys.stdin], [], [], options.hold_seconds)
                if readable:
                    sys.stdin.readline()
            raise
        finally:
            if host_process is not None and host_process.poll() is None:
                host_process.terminate()
                host_process.wait(timeout=10)
            if process is not None and process.poll() is None:
                try:
                    report["final_index"] = rpc(endpoint, op="list", domain=domain, parent_id="root")
                    if created is not None and created.exists():
                        try:
                            report["local_tags_hex"] = get_xattr(created, "com.apple.metadata:_kMDItemUserTags").hex()
                        except OSError as error:
                            report["local_tags_error"] = str(error)
                    report["metadata_sidecars"] = {str(path.relative_to(remote)): json.loads(path.read_text()) for path in remote.rglob("*.json") if ".rdrive-metadata" in path.parts}
                    if options.smart:
                        summary = subprocess.run(cli + ["smart", "status"], env=env, capture_output=True, text=True, timeout=15)
                        report["smart_status"] = json.loads(summary.stdout) if summary.returncode == 0 else {"stderr": summary.stderr}
                        report["provider_indexes"] = {path.name: provider_journal_from_db(path) for path in (home / "provider").glob("*.sqlite3")}
                except Exception as error:
                    report["diagnostic_error"] = str(error)
            if registered:
                removed = native("--remove-domain", domain, success=False)
                report["removed_domain"] = removed
                preserved = removed.get("preserved_location")
                if preserved:
                    path = Path(preserved)
                    # The named fixture contains only our known synthetic hello.txt.
                    # Preserve any unexpected content instead of recursively deleting it.
                    if path.parent == Path.home() / "Library/CloudStorage" and path.name.startswith("RDrive-RDriveNativeFixture"):
                        try:
                            children = list(path.rglob("*"))
                            allowed = {"hello.txt": [content, offline_content], "created.txt": [b"Native callback created this file\n", b"Native callback modified this file\n"],
                                       "second.txt": [second_content], "pin-folder/a.txt": [b"Pinned child A\n"],
                                       "pin-folder/b.txt": [b"Pinned child B\n"], "pin-folder/c.txt": [b"Future pinned child C\n", b"Remote pinned child D\n"]}
                            def synthetic(child):
                                relative = child.relative_to(path).as_posix()
                                if child.is_symlink():
                                    return False
                                if child.is_dir():
                                    return relative == "pin-folder"
                                return child.is_file() and relative in allowed and child.read_bytes() in allowed[relative]
                            if all(synthetic(child) for child in children):
                                for child in sorted(children, key=lambda item: len(item.parts), reverse=True):
                                    child.rmdir() if child.is_dir() else child.unlink()
                                path.rmdir()
                                report["preserved_synthetic_data_cleaned"] = True
                        except FileNotFoundError:
                            report["preserved_synthetic_data_cleaned"] = True
            stop_bridge()
            if prior_roots is None:
                roots_path.unlink(missing_ok=True)
            else:
                roots_path.write_bytes(prior_roots)
            log.close()
            server.shutdown(); server.server_close(); worker.join(timeout=3)
            report["requests"] = NativeDav.requests_seen
            report["bridge_stderr"] = log_path.read_text()
            options.output.parent.mkdir(parents=True, exist_ok=True)
            options.output.write_text(json.dumps(report, indent=2) + "\n")
    print("Native provider evidence: " + str(options.output), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rdrive")
    parser.add_argument("--app", type=Path, default=ROOT / "macos/build/RDrive.app")
    parser.add_argument("--ipc", type=Path, default=Path("/private/tmp/rdrive-native-ipc-501"))
    parser.add_argument("--output", type=Path, default=ROOT / "macos/build/provider-native-report.json")
    parser.add_argument("--hold-for-ui", action="store_true")
    parser.add_argument("--smart", action="store_true")
    parser.add_argument("--cache-bytes", type=int)
    parser.add_argument("--extended-smart", action="store_true")
    parser.add_argument("--view-only", action="store_true")
    parser.add_argument("--hold-on-failure", action="store_true")
    parser.add_argument("--hold-seconds", type=int, default=300)
    run_tests(parser.parse_args())
