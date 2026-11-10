#!/usr/bin/env python3
"""Isolated Samba CLI tests; Docker plus network for the default Alpine bootstrap.

RDRIVE_SMB_IMAGE may select a local image with Samba already installed for
offline/repeated runs. The default remains the pinned Alpine image below.
RDRIVE_SMB_NOTIFY_ONLY=1 selects only actual CLI notification/watch tests.

Only a temporary synthetic share is mounted. The container is removed even after failure.
Usage: python3 tests/smb_workflows.py [path/to/rdrive]
"""
import hashlib
import concurrent.futures
import ctypes
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid

from index_workflows import read_provider_journal, wait


def native_xattr(path, name, value=None):
    if sys.platform != "darwin":
        return os.getxattr(path, name) if value is None else os.setxattr(path, name, value)
    library = ctypes.CDLL(None, use_errno=True)
    library.fsetxattr.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    library.fsetxattr.restype = ctypes.c_int
    library.fgetxattr.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
    library.fgetxattr.restype = ctypes.c_ssize_t

    def checked(result):
        if result < 0:
            number = ctypes.get_errno()
            raise OSError(number, os.strerror(number))
        return result

    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        if value is not None:
            buffer = ctypes.create_string_buffer(value)
            checked(library.fsetxattr(descriptor, name.encode(), buffer, len(value), 0, 0))
            return None
        size = checked(library.fgetxattr(descriptor, name.encode(), None, 0, 0, 0))
        buffer = ctypes.create_string_buffer(max(1, size))
        actual = checked(library.fgetxattr(descriptor, name.encode(), buffer, size, 0, 0))
        return buffer.raw[:actual]
    finally:
        os.close(descriptor)


def notification_cli_workflows(binary, root, share, env):
    """Exercise actual CLI stream/watch control flow on an isolated namespace."""
    home = root / "notification-state"
    local = root / "notification-local"
    local.mkdir()
    remote = "cli-notifications"
    (share / remote).mkdir(mode=0o777)

    def cli(*arguments):
        result = subprocess.run([str(binary), "--home", str(home), "--json", "--progress", "off", *map(str, arguments)],
                                env=env, capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, (arguments, result.stdout, result.stderr)
        return result

    cli("profile", "add", "fixture", "--protocol", "smb", "--url", env["RDRIVE_TEST_SMB_URL"],
        "--username", "test", "--password-env", "RDRIVE_TEST_PASSWORD", "--timeout", "15")

    def messages(path):
        data = path.read_text()
        lines = data.splitlines() if data.endswith("\n") else data.splitlines()[:-1]
        return [json.loads(line) for line in lines if line.strip()]

    def stop(process):
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

    source = root / "notification-source"
    source.write_bytes(b"first notification bytes")
    output = root / "notify.jsonl"
    errors = root / "notify.stderr"
    with output.open("w") as stdout, errors.open("w") as stderr:
        process = subprocess.Popen([str(binary), "--home", str(home), "--json", "--progress", "off", "notify", "fixture", remote, "--count", "1"],
                                   env=env, stdout=stdout, stderr=stderr)
        try:
            wait(lambda: any(event.get("kind") == "ready" for event in messages(output)), "CLI notify never registered its watch", seconds=20)
            cli("put", "fixture", source, remote + "/first.txt")
            assert process.wait(timeout=20) == 0, errors.read_text()
            events = messages(output)
            assert events[0]["kind"] == "ready", events
            assert any(event["kind"] != "ready" and event["path"].startswith(remote + "/") for event in events), events
        finally:
            stop(process)
    print("PASS actual CLI notify emits readiness and a server event, then exits after --count 1", flush=True)

    output = root / "watch.jsonl"
    errors = root / "watch.stderr"
    with output.open("w") as stdout, errors.open("w") as stderr:
        process = subprocess.Popen([str(binary), "--home", str(home), "--json", "--progress", "off", "watch", "fixture", str(local), remote,
                                    "--direction", "pull", "--interval", "30"], env=env, stdout=stdout, stderr=stderr)
        try:
            wait(lambda: messages(output), "SMB watch never produced its read-only initial plan", seconds=20)
            before = len(messages(output))
            source.write_bytes(b"second notification bytes")
            cli("put", "fixture", source, remote + "/second.txt")
            wait(lambda: len(messages(output)) > before and any(
                any(action.get("path") == "second.txt" for action in report.get("actions", []))
                for report in messages(output)), "SMB watch did not plan the new file before its 30-second polling interval", seconds=15)
            time.sleep(0.1)
            process.send_signal(signal.SIGINT)
            assert process.wait(timeout=10) == 0, errors.read_text()
            assert all(report["applied"] is False for report in messages(output)), messages(output)
            assert not any(local.iterdir()), "read-only watch changed local files"
        finally:
            stop(process)
    print("PASS actual CLI SMB watch reacts to notifications, preserves read-only plans and exits on SIGINT", flush=True)
    cli("rm", "fixture", remote, "--recursive")


def run_tests(binary):
    docker = os.environ.get("RDRIVE_DOCKER") or shutil.which("docker")
    if not docker:
        raise RuntimeError("Docker executable not found; set RDRIVE_DOCKER")
    image = os.environ.get("RDRIVE_SMB_IMAGE") or "alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f"
    report_path = Path(os.environ["RDRIVE_SMB_REPORT"]).resolve() if os.environ.get("RDRIVE_SMB_REPORT") else None
    report = {"scope": "temporary synthetic Samba storage; no real cloud, NAS or system mount", "status": "starting", "stage": "fixture-startup",
              "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "image": image, "image_id": None, "server_version": None, "checks": []}

    def record(status=None):
        if status is not None:
            report["status"] = status
        if report_path is not None:
            report_path.parent.mkdir(parents=True, exist_ok=True)
            report_path.write_text(json.dumps(report, indent=2) + "\n")

    record()
    container = "rdrive-smb-test-" + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix="rdrive-smb-test-") as temporary:
        root = Path(temporary).resolve()
        share = root / "share"
        share.mkdir()
        share.chmod(0o777)
        config = root / "smb.conf"
        config.write_text("""[global]
server role = standalone server
server min protocol = SMB3_00
server max protocol = SMB3_11
smb encrypt = required
map to guest = Never
load printers = no
disable spoolss = yes
log level = 1
[test]
path = /share
read only = no
valid users = test
force user = root
""")
        if os.environ.get("RDRIVE_SMB_LIFECYCLE") == "1":
            config.write_text(config.read_text().replace("server role =", "kernel oplocks = no\nserver role =")
                              + "durable handles = yes\nkernel share modes = no\nposix locking = no\nstrict sync = yes\n")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        setup = """set -eu
if [ ! -x /usr/sbin/smbd ]; then apk add --no-cache samba-server samba-common-tools; fi
if ! id test >/dev/null 2>&1; then adduser -D test; fi
printf '%s\\n' testpass testpass | smbpasswd -a -s test
exec smbd --foreground --no-process-group --debug-stdout --configfile=/etc/samba/smb.conf
"""
        started = False
        try:
            command = [docker, "run", "--detach", "--name", container,
                       "--publish", f"127.0.0.1:{port}:445", "--volume", f"{share}:/share",
                       "--volume", f"{config}:/etc/samba/smb.conf:ro", image, "sh", "-c", setup]
            subprocess.run(command, check=True, capture_output=True, text=True, timeout=180)
            started = True
            report["image_id"] = subprocess.check_output([docker, "inspect", "--format", "{{.Image}}", container], text=True, timeout=10).strip()
            record()
            deadline = time.monotonic() + 180
            while True:
                ready = subprocess.run([docker, "exec", container, "smbcontrol", "smbd", "ping"],
                                       capture_output=True, text=True, timeout=10)
                if ready.returncode == 0:
                    break
                running = subprocess.run([docker, "inspect", "--format", "{{.State.Running}}", container],
                                         capture_output=True, text=True, timeout=10).stdout.strip()
                if running != "true" or time.monotonic() >= deadline:
                    logs = subprocess.run([docker, "logs", container], capture_output=True, text=True)
                    raise AssertionError("Samba did not start: " + logs.stdout + logs.stderr)
                time.sleep(1)
            version = subprocess.run([docker, "exec", container, "smbd", "--version"],
                                     capture_output=True, text=True, check=True).stdout.strip()
            report["server_version"] = version
            report["stage"] = "workflows"
            record("running")
            print(f"SERVER {version}; IMAGE {image}; IMAGE_ID {report['image_id']}", flush=True)
            home = root / "state"
            env = dict(os.environ, RDRIVE_TEST_PASSWORD="testpass")
            env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
            env["RDRIVE_TEST_SMB_URL"] = f"smb://127.0.0.1:{port}/test"
            env["RDRIVE_TEST_SMB_CONTAINER"] = container
            env["RDRIVE_TEST_DOCKER"] = docker
            if os.environ.get("RDRIVE_SMB_NOTIFY_ONLY") == "1":
                notification_cli_workflows(binary, root, share, env)
                report["checks"].extend(["CLI notify readiness, event and count=1 exit", "CLI read-only watch reacts to a new file and exits on SIGINT"])
                record("passed")
                return
            if runner := os.environ.get("RDRIVE_SMB_TEST_RUNNER"):
                cases = ["guarded_smb_integration", "resumable_smb_integration"]
                if os.environ.get("RDRIVE_SMB_LIFECYCLE") == "1":
                    cases = ["lifecycle_smb_integration"]
                for case in cases:
                    result = subprocess.run([runner, "smb::tests::" + case, "--exact", "--ignored", "--nocapture"],
                                            env=env, capture_output=True, text=True, timeout=300)
                    print(result.stdout, end="", flush=True)
                    if result.returncode != 0 or "1 passed; 0 failed" not in result.stdout:
                        raise AssertionError(f"SMB runtime proof did not pass exactly one test: {case}\n{result.stderr}")
                    report["checks"].append(case)
                    record()
                notification_cli_workflows(binary, root, share, env)
                report["checks"].extend(["CLI notify readiness, event and count=1 exit", "CLI read-only watch reacts to a new file and exits on SIGINT"])
                if os.environ.get("RDRIVE_SMB_PROOF_ONLY") == "1":
                    record("passed")
                    return
            passed = []

            def cli(*args, success=True, state_home=None):
                result = subprocess.run([str(binary), "--home", str(state_home or home), "--json", *args],
                                        env=env, capture_output=True, text=True, timeout=45)
                if success is not None and (result.returncode == 0) != success:
                    raise AssertionError(f"{args}: exit={result.returncode}\n{result.stdout}\n{result.stderr}")
                return result

            def passed_test(name):
                passed.append(name)
                report["checks"].append(name)
                record()
                print("PASS " + name, flush=True)

            cli("profile", "add", "fixture", "--protocol", "smb", "--url", f"smb://127.0.0.1:{port}/test",
                "--username", "test", "--password-env", "RDRIVE_TEST_PASSWORD", "--timeout", "15")
            info = cli("info", "fixture")
            assert "required transport encryption" in info.stdout
            assert "server-transparent POSIX symlinks cannot be identified" in info.stdout
            assert json.loads(info.stdout)["capabilities"]["conditional_write"]
            passed_test("encrypted SMB3 connection and truthful capabilities")

            if os.environ.get("RDRIVE_SMB_PROVIDER_ONLY") == "1":
                cli("mkdir", "fixture", "provider-own-write")
                domain = "smb-own-write"
                smart = os.environ.get("RDRIVE_SMB_SMART") == "1"
                cli("provider", "domain-add", domain, "fixture", "provider-own-write", *(["--smart"] if smart else []))
                # macOS needs a short canonical path for Unix IPC; Linux CI
                # uses its own temporary directory and has no /private/tmp.
                with tempfile.TemporaryDirectory(prefix="rd-smb-fp-", dir="/private/tmp" if sys.platform == "darwin" else None) as bridge_temporary:
                    group = Path(bridge_temporary).resolve()
                    group.chmod(0o700)
                    staging = group / "Transfers"
                    staging.mkdir(mode=0o700)
                    process = subprocess.Popen([str(binary), "--home", str(home), "--progress", "off", "provider", "serve", "--socket", str(group / "bridge.sock"), "--staging", str(staging), "--development-tcp"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    try:
                        descriptor_path = group / "bridge-endpoint.json"
                        deadline = time.monotonic() + 20
                        while not descriptor_path.exists():
                            assert process.poll() is None, "provider bridge exited before endpoint creation"
                            assert time.monotonic() < deadline, "provider endpoint timed out"
                            time.sleep(0.02)
                        descriptor = json.loads(descriptor_path.read_bytes())

                        def rpc(**message):
                            with socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=90) as client:
                                client.sendall(json.dumps(dict(v=1, auth_token=descriptor["token"], domain=domain, **message)).encode() + b"\n")
                                reader = client.makefile("rb")
                                while True:
                                    line = reader.readline(2 * 1024 * 1024)
                                    assert line, "bridge closed without a response"
                                    value = json.loads(line)
                                    if "progress" in value:
                                        continue
                                    return value

                        def success(**message):
                            value = rpc(**message)
                            assert value.get("ok"), value
                            return value

                        def stage(content):
                            path = staging / ("source-" + uuid.uuid4().hex)
                            path.write_bytes(content)
                            path.chmod(0o600)
                            return str(path)

                        def verify_strong(item):
                            journal = read_provider_journal(home, domain)
                            record = journal["items"][item["id"]]
                            assert record["entry"]["etag"].startswith("W/"), "fixture did not exercise weak SMB enumeration"
                            assert record.get("strong") and not record["strong"]["etag"].startswith("W/"), "successful write lost its strong SMB edit guard"
                            own = [pending["confirmed"] for pending in journal["requests"].values() if pending.get("confirmed") and (pending.get("response") or {}).get("item", {}).get("id") == item["id"]]
                            assert any(confirmed["etag"] == record["strong"]["etag"] for confirmed in own), "journal strong revision is not an own-write receipt"

                        tags = plistlib.dumps(["Blue\n4", "研究\n0"], fmt=plistlib.FMT_BINARY, sort_keys=False).hex()
                        attributes = {"user.rdrive.own#S": b"first attribute".hex()}
                        created = success(op="create", parent_id="root", name="file.txt", kind="file", client_item_id="smb-own-create", content_path=stage(b"first body"), tag_data_hex=tags, extended_attributes=attributes)["item"]
                        assert created.get("tag_data_hex") == tags and created.get("extended_attributes") == attributes, "initial SMB success receipt omitted the metadata just published"
                        verify_strong(created)
                        modified = success(op="modify", id=created["id"], request_id=uuid.uuid4().hex, content_path=stage(b"second body without a refetch"), base_content_version=created["content_version"], base_metadata_version=created["metadata_version"])["item"]
                        assert modified.get("tag_data_hex") == tags and modified.get("extended_attributes") == attributes
                        verify_strong(modified)
                        updated_tags = plistlib.dumps(["Green\n2"], fmt=plistlib.FMT_BINARY, sort_keys=False).hex()
                        updated_attributes = {"user.rdrive.own#S": b"second attribute".hex()}
                        tagged = success(op="modify", id=modified["id"], request_id=uuid.uuid4().hex, tag_data_hex=updated_tags, extended_attributes=updated_attributes, base_content_version=modified["content_version"], base_metadata_version=modified["metadata_version"])["item"]
                        assert tagged.get("tag_data_hex") == updated_tags and tagged.get("extended_attributes") == updated_attributes
                        verify_strong(tagged)
                        final_body = b"third body after a metadata-only write"
                        final = success(op="modify", id=tagged["id"], request_id=uuid.uuid4().hex, content_path=stage(final_body), base_content_version=tagged["content_version"], base_metadata_version=tagged["metadata_version"])["item"]
                        assert final["id"] == created["id"] and final.get("tag_data_hex") == updated_tags and final.get("extended_attributes") == updated_attributes
                        verify_strong(final)
                        def bases(item):
                            return dict(id=item["id"], request_id=uuid.uuid4().hex,
                                        base_content_version=item["content_version"], base_metadata_version=item["metadata_version"])

                        def fresh_root_item(identifier):
                            if smart:
                                before = read_provider_journal(home, domain)["directory_scans"].get("root", 0)
                                cli("index", "refresh", domain)
                                wait(lambda: read_provider_journal(home, domain)["directory_scans"].get("root", 0) > before, "SMB source-parent refresh", 30)
                            else:
                                success(op="list", parent_id="root")
                            return success(op="item", id=identifier)["item"]

                        namespace_file = success(op="create", parent_id="root", name="ns-file.txt", kind="file", client_item_id="smb-ns-file", content_path=stage(b"SMB namespace body"), tag_data_hex=tags, extended_attributes=attributes)["item"]
                        renamed = success(op="modify", name="ns-renamed.txt", **bases(namespace_file))["item"]
                        assert renamed["id"] == namespace_file["id"] and renamed["tag_data_hex"] == tags and renamed["extended_attributes"] == attributes
                        assert (share / "provider-own-write/ns-renamed.txt").read_bytes() == b"SMB namespace body"
                        assert not (share / "provider-own-write/ns-file.txt").exists()
                        namespace_directory = success(op="create", parent_id="root", name="ns-directory", kind="directory", client_item_id="smb-ns-directory")["item"]
                        namespace_child = success(op="create", parent_id=namespace_directory["id"], name="child.txt", kind="file", client_item_id="smb-ns-child", content_path=stage(b"SMB nested namespace body"))["item"]
                        namespace_directory = fresh_root_item(namespace_directory["id"])
                        moved_directory = success(op="modify", name="ns-moved-directory", **bases(namespace_directory))["item"]
                        assert moved_directory["id"] == namespace_directory["id"]
                        children = success(op="list", parent_id=moved_directory["id"])["items"]
                        assert len(children) == 1 and children[0]["id"] == namespace_child["id"]
                        assert (share / "provider-own-write/ns-moved-directory/child.txt").read_bytes() == b"SMB nested namespace body"
                        passed_test("SMB guarded file/directory rename retains IDs, child IDs, portable tags and attributes")

                        delete_request = dict(op="delete", **bases(renamed))
                        success(**delete_request)
                        deleted_path = share / "provider-own-write/ns-renamed.txt"
                        assert not deleted_path.exists()
                        deleted_path.write_bytes(b"external replacement after completed delete")
                        success(**delete_request)
                        assert deleted_path.read_bytes() == b"external replacement after completed delete"
                        moved_directory = success(op="item", id=moved_directory["id"])["item"]
                        deleted_directory = success(op="delete", recursive=True, **bases(moved_directory))
                        assert deleted_directory["recoverable"] and not deleted_directory["space_reclaimed"]
                        assert not (share / "provider-own-write/ns-moved-directory").exists()
                        assert (share / "provider-own-write" / deleted_directory["recovery_path"] / "child.txt").read_bytes() == b"SMB nested namespace body"
                        passed_test("SMB guarded file deletion preserves replacements on replay and directory deletion retains recoverable contents")
                        target = share / "provider-own-write/file.txt"
                        assert target.read_bytes() == final_body
                        external = b"E" * len(final_body)
                        external_source = root / "external-edit"
                        external_source.write_bytes(external)
                        external_home = root / "external-state"
                        cli("profile", "add", "fixture", "--protocol", "smb", "--url", f"smb://127.0.0.1:{port}/test", "--username", "test", "--password-env", "RDRIVE_TEST_PASSWORD", "--timeout", "15", state_home=external_home)
                        cli("put", "fixture", str(external_source), "provider-own-write/file.txt", "--overwrite", state_home=external_home)
                        rejected = rpc(op="modify", id=final["id"], request_id=uuid.uuid4().hex, content_path=stage(b"stale overwrite"), base_content_version=final["content_version"], base_metadata_version=final["metadata_version"])
                        assert not rejected["ok"] and rejected["error"]["code"] == "version_conflict", rejected
                        assert target.read_bytes() == external
                    finally:
                        process.send_signal(signal.SIGINT)
                        try:
                            stdout, stderr = process.communicate(timeout=10)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            stdout, stderr = process.communicate()
                        assert b"testpass" not in stdout + stderr
                mode = "Smart" if smart else "raw"
                passed_test(f"{mode} SMB provider create/content/metadata/content receipts retain strong guards, metadata and reject external edits without refetch")
                print(json.dumps({"passed": passed, "domain_mode": {"smart": smart, "repository": False}, "catalog_storage": "SQLite authoritative state"}, indent=2))
                record("passed")
                return

            content = bytes(range(256)) * 9001  # Several MiB, many 64 KiB chunks.
            source = root / "source.bin"
            source.write_bytes(content)
            cli("mkdir", "fixture", "folder")
            cli("put", "fixture", str(source), "folder/original.bin")
            assert (share / "folder/original.bin").read_bytes() == content
            assert "original.bin" in cli("ls", "fixture", "folder").stdout
            assert json.loads(cli("stat", "fixture", "folder/original.bin").stdout)["size"] == len(content)
            downloaded = root / "download.bin"
            cli("get", "fixture", "folder/original.bin", str(downloaded))
            assert hashlib.sha256(downloaded.read_bytes()).digest() == hashlib.sha256(content).digest()
            assert not list((share / "folder").rglob(".rdrive-upload-*"))
            passed_test("multi-chunk upload/list/stat/download, publication and byte equality")

            cli("cp", "fixture", "folder/original.bin", "folder/copy.bin")
            cli("cp", "fixture", "folder/original.bin", "folder/copy.bin", success=False)
            assert (share / "folder/copy.bin").read_bytes() == content
            cli("mv", "fixture", "folder/copy.bin", "folder/renamed.bin")
            cli("mv", "fixture", "folder/renamed.bin", "folder/original.bin", success=False)
            assert (share / "folder/renamed.bin").read_bytes() == content
            passed_test("copy and rename preserve existing destinations")

            source.write_bytes(b"replacement")
            cli("put", "fixture", str(source), "folder/original.bin", success=False)
            assert (share / "folder/original.bin").read_bytes() == content
            # A refused/ambiguous publication deliberately retains its unique stage.
            old_stages = set(share.rglob(".rdrive-upload-*"))
            cli("put", "fixture", str(source), "folder/original.bin", "--overwrite")
            assert (share / "folder/original.bin").read_bytes() == b"replacement"
            cli("cp", "fixture", "folder/original.bin", "folder/renamed.bin", "--overwrite")
            assert (share / "folder/renamed.bin").read_bytes() == b"replacement"
            assert set(share.rglob(".rdrive-upload-*")) == old_stages
            passed_test("explicit replacements publish complete files by rename")

            cli("cp", "fixture", "folder", "tree-copy")
            assert (share / "tree-copy/original.bin").read_bytes() == b"replacement"
            cli("rm", "fixture", "tree-copy", success=False)
            assert (share / "tree-copy/original.bin").exists(), "nonrecursive rm changed nonempty directory"
            cli("rm", "fixture", "tree-copy", "--recursive")
            assert not (share / "tree-copy").exists()
            cli("rm", "fixture", "folder/renamed.bin")
            assert not (share / "folder/renamed.bin").exists()
            passed_test("directory copy, file remove and explicit recursive remove")

            assert json.loads(cli("stat", "fixture", "missing-parent/file").stdout) is None
            for unsafe in ("../outside", "folder:stream", "folder\\original.bin"):
                cli("stat", "fixture", unsafe, success=False)
            cli("cp", "fixture", "folder", "folder/child", success=False)
            passed_test("traversal, ADS, backslash and recursive self-copy rejected")

            push = root / "sync-push"
            push.mkdir()
            (push / "new.txt").write_bytes(b"new SMB sync file")
            cli("mkdir", "fixture", "sync-root")
            push_report = json.loads(cli("sync", "fixture", str(push), "sync-root", "--direction", "push", "--apply").stdout)
            assert push_report["baseline_updated"]
            assert (share / "sync-root/new.txt").read_bytes() == b"new SMB sync file"
            pull = root / "sync-pull"
            pull.mkdir()
            pull_report = json.loads(cli("sync", "fixture", str(pull), "sync-root", "--direction", "pull", "--apply").stdout)
            assert pull_report["baseline_updated"]
            assert (pull / "new.txt").read_bytes() == b"new SMB sync file"
            passed_test("new-file push sync and pull sync establish verified baselines")

            (push / "new.txt").write_bytes(b"updated SMB sync")
            updated = json.loads(cli("sync", "fixture", str(push), "sync-root", "--direction", "push", "--apply").stdout)
            assert updated["baseline_updated"]
            assert (share / "sync-root/new.txt").read_bytes() == b"updated SMB sync"
            assert list((share / "sync-root").glob(".rdrive-backup-*"))
            assert list((share / "sync-root").glob(".rdrive-recovery-done-*.json"))
            clean = json.loads(cli("sync", "fixture", str(push), "sync-root", "--direction", "push").stdout)
            assert not clean["actions"] and not clean["conflicts"]
            passed_test("protected sync replacement retains recovery files and completed receipts stay out of sync")

            resume_source = root / "resume-source.bin"
            resume_source.write_bytes(content)
            cli("put", "fixture", str(resume_source), "resumed.bin", "--resume")
            resumed_download = root / "resumed-download.bin"
            cli("get", "fixture", "resumed.bin", str(resumed_download), "--resume")
            assert resumed_download.read_bytes() == content
            passed_test("CLI resumable upload/download flags preserve complete content")

            cli("mkdir", "fixture", "managed")
            assert json.loads(cli("repo", "init", "fixture", "managed").stdout)["commit"]["sequence"] == 0
            transaction = json.loads(cli("repo", "begin", "fixture", "managed").stdout)["id"]
            cli("repo", "mkdir", transaction, "folder")
            first_content = root / "repository-first.bin"
            first_content.write_bytes(bytes(range(251)) * 1103)
            native_xattr(first_content, "user.rdrive.smb-test", b"SMB repository metadata")
            second_content = root / "repository-second.bin"
            second_content.write_bytes(b"second independent SMB object")
            cli("repo", "put", transaction, str(first_content), "folder/一.txt")
            cli("repo", "put", transaction, str(second_content), "second.bin")
            assert json.loads(cli("repo", "ls", "fixture", "managed").stdout)["commit"]["entries"] == {}
            committed = json.loads(cli("repo", "commit", transaction).stdout)
            assert len(committed["commit"]["entries"]) == 3
            first_item = committed["commit"]["entries"]["folder/一.txt"]
            assert first_item["blob"] == hashlib.sha256(first_content.read_bytes()).hexdigest()
            assert first_item["metadata"]["attributes"]["user.rdrive.smb-test"] == b"SMB repository metadata".hex()
            assert json.loads(cli("repo", "commit", transaction).stdout)["checksum"] == committed["checksum"]
            commits = share / "managed/.rdrive-repository/commits"
            assert len(list(commits.glob("*.json"))) == 1, "transaction replay published another slot"
            repository_output = root / "repository-download.bin"
            cli("repo", "get", "fixture", "folder/一.txt", str(repository_output), "--root", "managed")
            assert repository_output.read_bytes() == first_content.read_bytes()
            assert native_xattr(repository_output, "user.rdrive.smb-test") == b"SMB repository metadata"

            rival_home = root / "rival-state"
            cli("profile", "add", "fixture", "--protocol", "smb", "--url", f"smb://127.0.0.1:{port}/test",
                "--username", "test", "--password-env", "RDRIVE_TEST_PASSWORD", "--timeout", "15", state_home=rival_home)
            writer_a = json.loads(cli("repo", "begin", "fixture", "managed").stdout)["id"]
            writer_b = json.loads(cli("repo", "begin", "fixture", "managed", state_home=rival_home).stdout)["id"]
            cli("repo", "mkdir", writer_a, "writer-A")
            cli("repo", "mkdir", writer_b, "writer-B", state_home=rival_home)
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as writers:
                first = writers.submit(cli, "repo", "commit", writer_a, success=None)
                second = writers.submit(cli, "repo", "commit", writer_b, success=None, state_home=rival_home)
                outcomes = [first.result(), second.result()]
            assert sum(result.returncode == 0 for result in outcomes) == 1, [result.stderr for result in outcomes]
            final = json.loads(cli("repo", "ls", "fixture", "managed").stdout)
            assert final["commit"]["sequence"] == 2
            assert ("writer-A" in final["commit"]["entries"]) != ("writer-B" in final["commit"]["entries"])
            assert len(list(commits.glob("*.json"))) == 2
            assert final["commit"]["entries"]["folder/一.txt"]["id"] == first_item["id"]
            passed_test("managed SMB snapshot publication, content and xattrs, immutable replay and competing homes")

            # Samba's default policy transparently resolves native POSIX symlinks.
            # They are not advertised as SMB reparse points, so record the limitation
            # explicitly instead of claiming client-side rejection.
            (share / "link").symlink_to("folder/original.bin")
            link_entry = json.loads(cli("stat", "fixture", "link").stdout)
            assert link_entry["size"] == len(b"replacement")
            cli("get", "fixture", "link", str(root / "link-download"))
            assert (root / "link-download").read_bytes() == b"replacement"
            print("OBSERVED LIMITATION Samba transparently follows POSIX symlinks; client cannot identify them", flush=True)
            # A separate server-policy check is not a client-side rejection claim.
            config.write_text(config.read_text() + "follow symlinks = no\n")
            subprocess.run([docker, "exec", container, "smbcontrol", "all", "reload-config"],
                           check=True, capture_output=True, text=True, timeout=15)
            assert json.loads(cli("stat", "fixture", "folder/original.bin").stdout)["size"] == len(b"replacement")
            blocked = cli("stat", "fixture", "link", success=None)
            if blocked.returncode == 0:
                assert json.loads(blocked.stdout) is None
            cli("get", "fixture", "link", str(root / "blocked-link-download"), success=False)
            assert not (root / "blocked-link-download").exists()
            print("POLICY CHECK Samba follow symlinks=no prevents access to hidden POSIX symlinks", flush=True)
            print(f"{len(passed)} SMB integration workflows passed; POSIX symlink limitation and server policy verified", flush=True)
            report["checks"].extend(["server-transparent POSIX symlink limitation observed", "server follow-symlinks=no policy verified"])
            record("passed")
        except BaseException as error:
            report["error"] = {"type": type(error).__name__, "message": str(error)}
            record("failed")
            raise
        finally:
            if started:
                subprocess.run([docker, "rm", "--force", container], capture_output=True, timeout=30)


if __name__ == "__main__":
    executable = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/rdrive").resolve()
    run_tests(executable)
