#!/usr/bin/env python3
"""Real CLI checks against disposable, capacity-bounded APFS file vaults.

This macOS workflow creates private 512 MiB FILE images only. It never registers
Finder domains, installs launchd jobs, uses real profiles/credentials, formats
physical volumes, or creates/deletes native APFS/Time Machine snapshots. All CLI
calls supply an explicit temporary --home. Snapshot inventories are read-only;
changes made by unrelated system software are recorded, not attributed to RDrive.
"""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import plistlib
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from cli_workflows import Dav

ROOT = Path(__file__).resolve().parents[1]
CAPACITY = 512 * 1024 * 1024


def sha(data):
    return hashlib.sha256(data).hexdigest()


def snapshot_inventory(path):
    device = subprocess.check_output(["/bin/df", "-P", str(path)], text=True, timeout=30).splitlines()[-1].split()[0]
    volume = plistlib.loads(subprocess.check_output(
        ["/usr/sbin/diskutil", "info", "-plist", device], timeout=30))
    values = plistlib.loads(subprocess.check_output(
        ["/usr/sbin/diskutil", "apfs", "listSnapshots", "-plist", volume["DeviceIdentifier"]], timeout=30))
    return {"volume_uuid": volume["VolumeUUID"], "snapshots": {
        row["SnapshotUUID"]: row["SnapshotName"] for row in values.get("Snapshots", [])}}


def owned_image_cleanup(root):
    """Only detach file images proven to live under this private fixture root."""
    if root.stat().st_uid != os.geteuid() or root.stat().st_mode & 0o077:
        raise AssertionError("Unsafe fixture root")
    def images():
        values = plistlib.loads(subprocess.check_output(["/usr/bin/hdiutil", "info", "-plist"], timeout=30))
        return [row for row in values.get("images", [])
                if row.get("image-path") and Path(row["image-path"]).resolve().is_relative_to(root)]
    detached = []
    for row in images():
        image = Path(row["image-path"])
        info = image.lstat()
        assert image.name == "vault.dmg" and not image.is_symlink()
        assert info.st_uid == os.geteuid() and info.st_nlink == 1 and not info.st_mode & 0o077
        assert info.st_size == CAPACITY
        entities = row["system-entities"]
        roots = [v["dev-entry"] for v in entities if re.fullmatch(r"/dev/disk\d+", v.get("dev-entry", ""))]
        assert roots, row
        assert all(not v.get("mount-point") or Path(v["mount-point"]).resolve().is_relative_to(root)
                   for v in entities), "Foreign mount location; refusing fixture cleanup"
        subprocess.run(["/usr/bin/hdiutil", "detach", roots[0]], check=True, capture_output=True, timeout=60)
        detached.append(str(image))
    assert not images(), "Owned fixture image remained attached"
    return detached


class Client:
    def __init__(self, binary, root, url, environment):
        self.binary, self.root, self.environment = binary, root, environment
        root.mkdir(mode=0o700)
        self.home, self.ipc = root / "state", root / "ipc"
        self.ipc.mkdir(mode=0o700)
        self.base = [str(binary), "--home", str(self.home), "--json", "--progress", "off"]
        self.process = None
        self.log = (root / "provider.log").open("w+")
        self.command("profile", "add", "fixture", "--protocol", "webdav", "--url", url,
                     "--allow-http", "--username", "user", "--password-env", "RDRIVE_CHECKPOINT_TEST_PASSWORD", "--timeout", "2")

    def command(self, *args, success=True, timeout=90):
        result = subprocess.run(self.base + list(map(str, args)), env=self.environment,
                                capture_output=True, text=True, timeout=timeout)
        if success:
            assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
            return json.loads(result.stdout)
        assert result.returncode != 0, (args, result.stdout)
        return result

    def config(self, *args):
        return self.command("checkpoint", "config", *args)

    def configure_vault(self, count=2):
        self.config("--storage", "apfs-vault", "--enabled", "true", "--max-bytes", CAPACITY,
                    "--max-records", count, "--auto-prune", "true", "--manual")

    def rows(self):
        value = self.command("checkpoint", "list")
        assert all(row["kind"] == "apfs_file_vault" for row in value["checkpoints"]), value
        return value["checkpoints"]

    def create(self):
        value = self.command("checkpoint", "create")
        assert value["created"] and value.get("confirmed", True) and value["kind"] == "apfs_file_vault", value
        assert str(uuid.UUID(value["id"])) == value["id"]
        return value["id"]

    def status(self):
        return self.command("checkpoint", "status")

    def image(self):
        path = self.home / "checkpoints/vault/vault.dmg"
        assert path.stat().st_size == CAPACITY
        return path

    def rpc(self, **message):
        endpoint = json.loads((self.ipc / "bridge-endpoint.json").read_bytes())
        message.update(v=1, auth_token=endpoint["token"], request_id=uuid.uuid4().hex)
        with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=15) as stream:
            stream.sendall(json.dumps(message).encode() + b"\n")
            with stream.makefile("rb") as source:
                for line in source:
                    result = json.loads(line)
                    if "progress" not in result:
                        assert result.get("ok"), result
                        return result
        raise AssertionError("Fixture broker disconnected")

    def start(self):
        assert self.process is None
        self.process = subprocess.Popen(self.base + ["provider", "serve", "--socket", str(self.ipc / "bridge.sock"),
            "--staging", str(self.ipc / "Transfers"), "--development-tcp"], env=self.environment,
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.log, start_new_session=True)
        def ready():
            assert self.process.poll() is None, "Fixture broker exited"
            try:
                return self.rpc(op="domains")
            except (OSError, json.JSONDecodeError):
                return None
        self.wait(ready, "broker startup", 30)

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGINT)
                try:
                    self.process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    # Only this fixture's known process group is signalled.
                    os.killpg(self.process.pid, signal.SIGTERM)
                    self.process.wait(timeout=15)
            assert self.process.poll() is not None
            self.process = None

    def wait(self, predicate, label, seconds=45):
        deadline = time.monotonic() + seconds
        while True:
            if self.process is not None:
                assert self.process.poll() is None, "Fixture broker exited while " + label
            value = predicate()
            if value:
                return value
            assert time.monotonic() < deadline, label
            time.sleep(.1)

    def scheduler(self):
        path = self.home / "checkpoint-scheduler/state.json"
        return json.loads(path.read_bytes()) if path.exists() else None

    def replace_scheduler(self, transform):
        assert self.process is None, "Only mutate a stopped disposable fixture's scheduler state"
        path = self.home / "checkpoint-scheduler/state.json"
        value = json.loads(path.read_bytes())
        transform(value)
        temporary = path.with_suffix(".fixture-tmp")
        temporary.write_text(json.dumps(value))
        temporary.chmod(0o600)
        temporary.replace(path)

    @contextmanager
    def inherited_host_operation(self):
        """Retain this fixture's real operation lease through Apple lockf.

        The gated child performs no image operations. Closing our own copy
        without LOCK_UN models an RDrive parent disappearing while lockf waits.
        """
        assert self.process is None
        lock = self.home / "checkpoints/vault/host-operation.lock"
        descriptor = json.loads(lock.with_name("descriptor.json").read_bytes())
        meta = lock.lstat()
        assert not lock.is_symlink() and meta.st_uid == os.geteuid()
        assert meta.st_nlink == 1 and meta.st_size == 0 and not meta.st_mode & 0o077
        assert descriptor["host_lock"] == {"device": meta.st_dev, "inode": meta.st_ino}
        ready, release = self.root / "host-tool-ready", self.root / "host-tool-release"
        script = self.root / "host-tool.py"
        script.write_text("""import pathlib,sys,time
ready,release=map(pathlib.Path,sys.argv[1:])
ready.touch(mode=0o600)
deadline=time.monotonic()+90
while not release.exists():
    if time.monotonic()>deadline: raise TimeoutError('fixture gate was not released')
    time.sleep(.02)
""")
        script.chmod(0o600)
        lease = lock.open("r+b")
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        original_flags = fcntl.fcntl(lease, fcntl.F_GETFD)
        wrapper = None
        try:
            wrapper = subprocess.Popen(["/usr/bin/lockf", "-k", "-s", "-t", "0",
                f"/dev/fd/{lease.fileno()}", sys.executable, str(script), str(ready), str(release)],
                pass_fds=(lease.fileno(),), stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            deadline = time.monotonic() + 15
            while not ready.exists():
                assert wrapper.poll() is None, "Owned lockf tool exited before becoming ready"
                assert time.monotonic() < deadline, "Owned lockf tool startup timed out"
                time.sleep(.02)
            assert fcntl.fcntl(lease, fcntl.F_GETFD) == original_flags
            lease.close()  # Never LOCK_UN while the inherited tool is alive.
            with lock.open("r+b") as probe:
                try:
                    fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    raise AssertionError("Inherited operation lease was prematurely released")
            yield
        finally:
            lease.close()
            release.touch(mode=0o600)
            if wrapper is not None:
                try:
                    stdout, stderr = wrapper.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(wrapper.pid, signal.SIGKILL)
                    stdout, stderr = wrapper.communicate(timeout=10)
                assert wrapper.returncode == 0, (wrapper.returncode, stdout, stderr)

    def prepare_interrupted_layout(self):
        """Reconstruct post-System/pre-Core initialization in this fixture only."""
        import shutil
        assert self.process is None
        image = self.image()
        descriptor_path = image.with_name("descriptor.json")
        descriptor = json.loads(descriptor_path.read_bytes())
        meta = image.lstat()
        assert not image.is_symlink() and meta.st_uid == os.geteuid()
        assert meta.st_nlink == 1 and not meta.st_mode & 0o077
        assert descriptor["image"] == {"device": meta.st_dev, "inode": meta.st_ino}
        mount = image.with_name("mount")
        values = plistlib.loads(subprocess.check_output(["/usr/bin/hdiutil", "attach", "-plist",
            "-nobrowse", "-noautoopen", "-owners", "on", "-mountpoint", str(mount), str(image)], timeout=60))
        entities = values["system-entities"]
        whole = next(row["dev-entry"] for row in entities if re.fullmatch(r"/dev/disk\d+", row.get("dev-entry", "")))
        try:
            mounted = [row for row in entities if row.get("mount-point")]
            assert len(mounted) == 1 and Path(mounted[0]["mount-point"]).resolve() == mount
            volume = plistlib.loads(subprocess.check_output(
                ["/usr/sbin/diskutil", "info", "-plist", mounted[0]["dev-entry"]], timeout=30))
            assert volume["VolumeUUID"] == descriptor["volume_uuid"]
            assert volume["FilesystemType"] == "apfs" and not volume.get("APFSSnapshot", False)
            assert volume["VolumeName"] == "RDriveVault-" + descriptor["instance_id"]
            assert mount.stat().st_uid == os.geteuid() and not mount.stat().st_mode & 0o077
            assert json.loads((mount / ".rdrive-vault.json").read_bytes()) == descriptor
            for name in ["blobs", "checkpoints", "staging"]:
                path = mount / name
                assert path.is_dir() and not path.is_symlink() and path.stat().st_dev == mount.stat().st_dev
                for item in [path, *path.rglob("*")]:
                    owned = item.lstat()
                    assert not item.is_symlink() and owned.st_uid == os.geteuid() and not owned.st_mode & 0o077
                    assert owned.st_dev == mount.stat().st_dev
                shutil.rmtree(path)
            layout = mount / ".rdrive-checkpoint-layout.json"
            assert layout.is_file() and not layout.is_symlink()
            assert json.loads(layout.read_bytes()) == {"version": 1, "home": descriptor["home_binding"],
                "instance_id": descriptor["instance_id"], "volume_uuid": descriptor["volume_uuid"]}
            layout.unlink()
            directory = os.open(mount, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            # The exact owned image device is detached, never a physical volume.
            subprocess.run(["/usr/bin/hdiutil", "detach", whole], check=True, capture_output=True, timeout=60)
        with image.open("rb") as backing:
            os.fsync(backing.fileno())
        descriptor_path.write_text(json.dumps(dict(descriptor, ready=False, volume_uuid=None)))
        return descriptor

    def bounded_quiet(self, checkpoint_id, seconds=2.1):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = self.scheduler()
            assert value and value["last_success"]["checkpoint_id"] == checkpoint_id
            assert value["inflight"] is None
            assert self.process is not None and self.process.poll() is None
            time.sleep(.1)


def run(binary, report_path=None):
    assert sys.platform == "darwin", "The actual APFS image workflow is macOS-only"
    binary = Path(binary).resolve()
    binary_hash = sha(binary.read_bytes())
    source_files = {str(path.relative_to(ROOT)): sha(path.read_bytes())
                    for path in sorted((ROOT / "src").rglob("*.rs"))}
    source_files.update({name: sha((ROOT / name).read_bytes()) for name in ["Cargo.toml", "Cargo.lock"]})
    source_digest = sha(json.dumps(source_files, sort_keys=True).encode())
    workflow_hash = sha(Path(__file__).read_bytes())
    checks, clients, cleanup = [], [], []
    root = Path(tempfile.mkdtemp(prefix="rdrive-checkpoint-workflows-", dir="/private/tmp")).resolve()
    root.chmod(0o700)
    try:
        before_os = snapshot_inventory(root)
    except BaseException:
        root.rmdir()
        raise
    remote = root / "remote"
    remote.mkdir(mode=0o700)
    class Fixture(Dav):
        requests_seen = []
        def reply(self, status, body=b"", **headers):
            if status == 207:
                body = body.replace(b"<d:href>/dav/./</d:href>", b"<d:href>/dav/</d:href>")
            try:
                return super().reply(status, body, **headers)
            except (BrokenPipeError, ConnectionResetError):
                self.close_connection = True
    Fixture.root = remote
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    environment = dict(os.environ, RDRIVE_CHECKPOINT_TEST_PASSWORD="fixture-password")
    environment.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
    environment.pop("RDRIVE_HOME", None)
    url = f"http://127.0.0.1:{server.server_port}/dav/"
    report = None
    def passed(label):
        checks.append(label)
        print("PASS " + label, flush=True)
    try:
        manual = Client(binary, root / "manual", url, environment)
        clients.append(manual)
        manual.configure_vault()
        initial = manual.status()
        assert initial["kind"] == "apfs_file_vault" and initial["storage"] == "apfs_vault" and initial["enabled"]
        assert not initial["initialized"] and not (manual.home / "checkpoints/vault/vault.dmg").exists()
        passed("programmatic vault policy and read-only status do not initialize storage or create native snapshots")

        source = manual.root / "payload.data"
        payload = os.urandom(8 * 1024 * 1024)
        source.write_bytes(payload)
        jobs = [manual.command("put", "fixture", source, name, "--queue") for name in ["a.data", "b.data"]]
        queue_before = manual.command("queue", "list")
        manual.command("history", "import", "fixture", "history.data", source, "--request-id", "checkpoint-fixture-history")
        logical = "spool/" + jobs[0]["snapshot"]
        first = manual.create()
        details = manual.command("checkpoint", "inspect", first)
        assert details["state"] == "confirmed" and not details["restores_state_database"]
        assert details["files"][logical]["sha256"] == sha(payload)
        assert len({value["sha256"] for value in details["files"].values()}) == 1
        exported = manual.root / "exported.data"
        assert manual.command("checkpoint", "export", first, logical, exported)["exported"]
        assert exported.read_bytes() == payload
        manual.command("checkpoint", "export", first, logical, exported, success=False)
        assert exported.read_bytes() == payload
        assert manual.command("queue", "list") == queue_before
        manual.image()
        passed("manual creation publishes verified queued/history payloads and exports without overwriting or changing queued edits")

        first_status = manual.status()
        assert first_status["schedule"]["last_success"]["checkpoint_id"] == first
        manual.config("--interval-secs", "60")
        scheduled = manual.status()
        assert scheduled["schedule"]["next_due_at"] >= first_status["schedule"]["last_success"]["completed_at"] + 60
        assert scheduled["schedule"]["next_due_at"] > int(time.time())
        first_used = scheduled["used_bytes"]
        second = manual.create()
        second_status = manual.status()
        assert second_status["schedule"]["last_success"]["checkpoint_id"] == second
        second_used = second_status["used_bytes"]
        assert second != first and {row["id"] for row in manual.rows()} == {first, second}
        assert second_used - first_used < len(payload) // 2, (first_used, second_used)
        manual.start()
        assert manual.command("checkpoint", "reconcile", first)["confirmed"]
        manual.bounded_quiet(second)
        manual.stop()
        manual.config("--manual")
        assert {row["id"] for row in manual.rows()} == {first, second}
        passed("unchanged payloads share CAS storage; manual completion sets the next deadline and reconciliation remains idempotent across restart")

        third_source = manual.root / "third.data"
        third_source.write_bytes(b"new distinct checkpoint payload")
        manual.command("put", "fixture", third_source, "c.data", "--queue")
        manual_queue_expected = manual.command("queue", "list")
        third = manual.create()
        assert {row["id"] for row in manual.rows()} == {second, third}
        retained = manual.root / "retained.data"
        manual.command("checkpoint", "export", second, logical, retained)
        assert retained.read_bytes() == payload
        manual.config("--max-records", "1", "--auto-prune", "false")
        preview = manual.command("checkpoint", "prune")
        assert not preview["apply"] and preview["candidates"] == 1 and preview["removed"] == 0
        assert {row["id"] for row in manual.rows()} == {second, third}
        applied = manual.command("checkpoint", "prune", "--apply")
        assert applied["removed"] == 1 and not applied["os_snapshots_deleted"]
        assert [row["id"] for row in manual.rows()] == [third]
        manual.command("checkpoint", "create", success=False)
        assert [row["id"] for row in manual.rows()] == [third]
        passed("automatic and explicit count retention remove only old owned records and retain the latest verified recovery")

        config_before = (manual.home / "config.json").read_bytes()
        for options in [("--max-bytes", CAPACITY * 2), ("--max-records", "0"), ("--interval-secs", "1")]:
            manual.command("checkpoint", "config", *options, success=False)
            assert (manual.home / "config.json").read_bytes() == config_before
        descriptor = manual.home / "checkpoints/vault/descriptor.json"
        original = descriptor.read_bytes()
        try:
            foreign = json.loads(original)
            foreign["home_binding"] = "0" * 64
            descriptor.write_text(json.dumps(foreign))
            manual.command("checkpoint", "status", success=False)
        finally:
            descriptor.write_bytes(original)
        manual.config("--enabled", "false")
        manual.command("checkpoint", "create", success=False)
        disabled_export = manual.root / "disabled-export.data"
        manual.command("checkpoint", "export", third, logical, disabled_export)
        assert disabled_export.read_bytes() == payload
        assert manual.command("checkpoint", "inspect", third)["state"] == "confirmed"
        manual.image()
        passed("capacity and policy failures preserve configuration; foreign binding fails closed and disabling retains recoverable content")

        manual.config("--enabled", "true", "--max-records", "4", "--auto-prune", "true")
        blob = manual.home / "history/blobs" / sha(payload)
        saved_blob = blob.read_bytes()
        before_ids = {row["id"] for row in manual.rows()}
        try:
            blob.write_bytes(b"corrupt verified source")
            error = manual.command("checkpoint", "create", success=False)
            assert "checksum" in error.stderr or "source changed" in error.stderr, error.stderr
            assert {row["id"] for row in manual.rows()} == before_ids
        finally:
            blob.write_bytes(saved_blob)
        valid_after_error = manual.create()
        assert valid_after_error in {row["id"] for row in manual.rows()}
        assert (remote / "a.data").exists() is False
        passed("corrupt source payloads are rejected before publication and a repaired private source can be checkpointed safely")

        manual.config("--interval-secs", "60")
        legacy_ids = {row["id"] for row in manual.rows()}
        manual.config("--storage", "system-snapshot", "--enabled", "false")
        legacy_policy = json.loads((manual.home / "config.json").read_bytes())["checkpoints"]
        assert legacy_policy.get("storage", "system_snapshot") == "system_snapshot" and not legacy_policy["enabled"]
        assert legacy_policy.get("interval_secs") is None and legacy_policy.get("max_bytes") is None
        assert not legacy_policy.get("auto_prune", False)
        manual.command("checkpoint", "create", success=False)
        assert {row["id"] for row in manual.rows()} == legacy_ids
        assert manual.command("checkpoint", "inspect", valid_after_error)["state"] == "confirmed"
        switched_export = manual.root / "legacy-mode-vault-export.data"
        manual.command("checkpoint", "export", valid_after_error, logical, switched_export)
        assert switched_export.read_bytes() == payload
        manual.config("--storage", "apfs-vault", "--max-bytes", CAPACITY, "--enabled", "false",
                      "--auto-prune", "true", "--manual")
        assert {row["id"] for row in manual.rows()} == legacy_ids
        passed("switching to disabled legacy mode resets scheduling and capacity policy while existing vault checkpoints remain recoverable by identity")

        automatic = Client(binary, root / "automatic", url, environment)
        clients.append(automatic)
        small = automatic.root / "retained.data"
        small.write_bytes(b"unsent automatic checkpoint content")
        job = automatic.command("put", "fixture", small, "automatic.data", "--queue")
        automatic.configure_vault(count=4)
        automatic.config("--interval-secs", "60")
        automatic.start()
        def success_id():
            state = automatic.scheduler()
            return state["last_success"]["checkpoint_id"] if state and state.get("last_success") else None
        first_auto = automatic.wait(success_id, "first due scheduled checkpoint", 60)
        initial_success = automatic.scheduler()["last_success"].copy()
        automatic.bounded_quiet(first_auto)
        state = automatic.status()
        assert state["schedule"]["last_success"]["checkpoint_id"] == first_auto
        assert state["schedule"]["next_due_at"] >= initial_success["completed_at"] + 60
        assert [row["id"] for row in automatic.rows()] == [first_auto]
        automatic.stop()
        automatic.start()
        automatic.bounded_quiet(first_auto)
        assert automatic.scheduler()["last_success"] == initial_success
        automatic.stop()
        passed("resident scheduling creates the first due checkpoint promptly, avoids bursts and retains its deadline across restart")

        automatic.config("--enabled", "false")
        automatic.replace_scheduler(lambda value: value["last_success"].update(completed_at=int(time.time()) - 120))
        automatic.start()
        automatic.bounded_quiet(first_auto)
        assert automatic.status()["schedule"]["operation"] == "disabled"
        automatic.config("--enabled", "true")
        second_auto = automatic.wait(lambda: (value if (value := success_id()) != first_auto else None), "re-enabled overdue checkpoint", 60)
        automatic.bounded_quiet(second_auto)
        automatic.stop()
        ids_before = {row["id"] for row in automatic.rows()}
        second_success = automatic.scheduler()["last_success"].copy()
        def uncertain(value):
            value["inflight"] = {"id": second_auto, "backend_key": second_success["backend_key"],
                                 "started_at": second_success["completed_at"]}
            value["last_success"] = initial_success
            value.update(failure_count=0, retry_at=None, last_error=None, last_deferred=None)
        automatic.replace_scheduler(uncertain)
        automatic.start()
        automatic.wait(lambda: (value if (value := automatic.scheduler()) and value["inflight"] is None
                                and value["last_success"]["checkpoint_id"] == second_auto else None),
                       "retained inflight publication recovery", 60)
        assert {row["id"] for row in automatic.rows()} == ids_before
        automatic.bounded_quiet(second_auto)
        automatic.config("--enabled", "false")
        automatic.stop()
        auto_export = automatic.root / "automatic-export.data"
        automatic.command("checkpoint", "export", second_auto, "spool/" + job["snapshot"], auto_export)
        assert auto_export.read_bytes() == small.read_bytes()
        automatic.image()
        passed("disabled overdue work stays idle; re-enable and retained inflight recovery preserve checkpoint identity without duplicate publication")

        automatic.config("--enabled", "true")
        descriptor_path = automatic.home / "checkpoints/vault/descriptor.json"
        descriptor = json.loads(descriptor_path.read_bytes())
        image_identity = (automatic.image().stat().st_dev, automatic.image().stat().st_ino)
        # A completed image and interior identity can precede the final outer
        # publication. Preserve all authority fields and only rewind that flag.
        prepared = dict(descriptor, ready=False, volume_uuid=None)
        descriptor_path.write_text(json.dumps(prepared))
        automatic.replace_scheduler(uncertain)
        with automatic.inherited_host_operation():
            automatic.start()
            def deferred():
                value = automatic.scheduler()
                assert value["inflight"]["id"] == second_auto, value
                assert value["failure_count"] == 0 and value["last_error"] is None, value
                return value if value["last_deferred"] else None
            busy = automatic.wait(deferred, "inherited creator lease defers checkpoint recovery", 30)
            assert busy["retry_at"] is not None
            assert automatic.status()["busy"], "Busy checkpoint status must remain available"
            assert json.loads(descriptor_path.read_bytes()) == prepared
        automatic.wait(lambda: (value if (value := automatic.scheduler()) and value["inflight"] is None
                               and value["last_success"]["checkpoint_id"] == second_auto else None),
                       "prepared image recovery after inherited creator finishes", 45)
        automatic.bounded_quiet(second_auto)
        automatic.stop()
        recovered = json.loads(descriptor_path.read_bytes())
        assert recovered == descriptor
        assert (automatic.image().stat().st_dev, automatic.image().stat().st_ino) == image_identity
        assert {row["id"] for row in automatic.rows()} == ids_before
        recovery_export = automatic.root / "initialization-recovery-export.data"
        automatic.command("checkpoint", "export", second_auto, "spool/" + job["snapshot"], recovery_export)
        assert recovery_export.read_bytes() == small.read_bytes()
        automatic.config("--enabled", "false")
        passed("inherited host lease defers prepared-image recovery without failure inflation; writable reconciliation preserves identity and verified content without duplicates")

        interrupted = Client(binary, root / "interrupted-initialization", url, environment)
        clients.append(interrupted)
        interrupted_source = interrupted.root / "pending.data"
        interrupted_source.write_bytes(b"retained source survives an interrupted first vault initialization")
        interrupted_job = interrupted.command("put", "fixture", interrupted_source, "pending.data", "--queue")
        interrupted.configure_vault(count=4)
        first_id = interrupted.create()
        original_success = interrupted.scheduler()["last_success"].copy()
        original_descriptor = interrupted.prepare_interrupted_layout()
        def first_attempt(value):
            value.update(inflight={"id": first_id, "backend_key": original_success["backend_key"],
                                   "started_at": original_success["completed_at"]},
                         last_success=None, failure_count=0, retry_at=None, last_error=None, last_deferred=None)
        interrupted.replace_scheduler(first_attempt)
        interrupted.command("checkpoint", "status", success=False)
        interrupted.config("--interval-secs", "60")
        interrupted.start()
        interrupted.wait(lambda: (value if (value := interrupted.scheduler()) and value["inflight"] is None
                                 and (value.get("last_success") or {}).get("checkpoint_id") == first_id else None),
                         "first initialization resumes the retained planned checkpoint", 60)
        interrupted.bounded_quiet(first_id)
        interrupted.stop()
        assert [row["id"] for row in interrupted.rows()] == [first_id]
        assert json.loads((interrupted.home / "checkpoints/vault/descriptor.json").read_bytes()) == original_descriptor
        interrupted_export = interrupted.root / "recovered-first-payload.data"
        interrupted.command("checkpoint", "export", first_id, "spool/" + interrupted_job["snapshot"], interrupted_export)
        assert interrupted_export.read_bytes() == interrupted_source.read_bytes()
        assert interrupted.command("queue", "list") == [interrupted_job]
        interrupted.config("--enabled", "false")
        passed("interrupted first initialization completes only the owned empty layout and publishes the retained planned checkpoint with its queued payload intact")

        assert manual.command("queue", "list") == manual_queue_expected
        assert automatic.command("queue", "list") == [job]
        for client in clients:
            assert client.image().stat().st_size == CAPACITY
        assert not any(method in {"PUT", "DELETE", "MOVE", "COPY"} for method, _ in Fixture.requests_seen)
        assert sha(binary.read_bytes()) == binary_hash
        assert sha(Path(__file__).read_bytes()) == workflow_hash, "Workflow changed during execution"
        assert all(sha((ROOT / name).read_bytes()) == digest for name, digest in source_files.items()), "Rust sources changed during execution"
        after_os = snapshot_inventory(root)
        version = subprocess.check_output([str(binary), "--home", str(root / "version-home"), "--version"],
                                          text=True, env=environment, timeout=15).strip()
        report = {"binary": binary.name, "version": version, "binary_sha256": binary_hash, "passed": len(checks), "checks": checks,
                  "image_capacity_bytes": CAPACITY, "private_image_count": len(clients),
                  "workflow_sha256": workflow_hash, "rust_source_inventory_sha256": source_digest,
                  "rust_source_files_sha256": source_files,
                  "os_snapshot_inventory_before": before_os, "os_snapshot_inventory_after": after_os,
                  "os_snapshot_inventory_unchanged": before_os == after_os,
                  "os_snapshot_delta_attribution": "External mutable OS state; no native snapshot create/delete API is used by this storage path",
                  "scope": "actual CLI, disposable authenticated DAV, private APFS file images and resident provider; no real profiles, native OS snapshot mutation or physical-volume format"}
    except Exception:
        failure = {"fixture": str(root), "passed": checks, "provider_logs": {}, "state": {}}
        for client in clients:
            client.log.flush()
            client.log.seek(0)
            failure["provider_logs"][client.root.name] = client.log.read()[-12000:]
            state = {}
            for name in ["checkpoint-scheduler/state.json", "checkpoints/vault/descriptor.json", "config.json"]:
                path = client.home / name
                if path.exists():
                    try:
                        value = json.loads(path.read_bytes())
                        state[name] = value.get("checkpoints") if name == "config.json" else value
                    except (OSError, ValueError) as error:
                        state[name] = {"read_error": str(error)}
            failure["state"][client.root.name] = state
        output = ROOT / "target" / ("checkpoint-workflow-failure-" + root.name + ".json")
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(failure, indent=2) + "\n")
        raise
    finally:
        try:
            for client in reversed(clients):
                client.stop()
            cleanup = owned_image_cleanup(root)
            for client in clients:
                client.log.close()
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=5)
        import shutil
        shutil.rmtree(root)
    assert report is not None
    report["owned_images_cleaned"] = True
    report["emergency_owned_image_detaches"] = cleanup
    output = Path(report_path or ROOT / "target/checkpoint-workflows.json").resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--report", type=Path)
    options = parser.parse_args()
    run(options.binary.resolve(), options.report)
