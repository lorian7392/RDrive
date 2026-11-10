#!/usr/bin/env python3
"""Actual macOS SMB mount roundtrip against an isolated encrypted Samba container.

Synthetic credentials are supplied only through a pseudoterminal. No macOS
sharing service or Keychain item is configured. Requires Docker and native mount
permission. Usage: python3 tests/mount_workflows.py [path/to/rdrive]
"""
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid


def terminal_command(command, password):
    pid, descriptor = pty.fork()
    if pid == 0:
        os.execv(command[0], command)
    output = bytearray()
    sent = False
    status = None
    deadline = time.monotonic() + 60
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([descriptor], [], [], 0.2)
            if readable:
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    chunk = b""
                output.extend(chunk)
                if not sent and b"password" in bytes(output).lower():
                    os.write(descriptor, password.encode() + b"\n")
                    sent = True
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                break
        else:
            # The pty child owns a session; stop its helper descendants as well.
            os.killpg(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
            raise AssertionError("native SMB prompt timed out")
    finally:
        os.close(descriptor)
    text = output.decode(errors="replace").replace(password, "[redacted]")
    if os.waitstatus_to_exitcode(status) != 0:
        raise AssertionError("native mount failed:\n" + text)
    return text


def run_tests(binary):
    docker = os.environ.get("RDRIVE_DOCKER") or shutil.which("docker")
    if not docker:
        raise RuntimeError("Docker is required for this explicit native SMB test")
    image = "alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f"
    container = "rdrive-native-mount-" + uuid.uuid4().hex[:12]
    mounted = False
    started = False
    with tempfile.TemporaryDirectory(prefix="rdrive-native-smb-") as temporary:
        root = Path(temporary).resolve()
        share = root / "share"
        nested = share / "nested"
        nested.mkdir(parents=True)
        share.chmod(0o777)
        nested.chmod(0o777)
        (nested / "hello.txt").write_text("native encrypted SMB bytes\n")
        (share / "outside-root.txt").write_text("must not be exposed")
        point = root / "volume"
        point.mkdir(mode=0o700)
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
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        setup = """set -eu
apk add --no-cache samba-server samba-common-tools
adduser -D test
printf '%s\\n' native-smb-password native-smb-password | smbpasswd -a -s test
exec smbd --foreground --no-process-group --debug-stdout --configfile=/etc/samba/smb.conf
"""
        cli = [str(binary), "--home", str(root / "state"), "--json"]
        try:
            subprocess.run([docker, "run", "--detach", "--name", container,
                            "--publish", f"127.0.0.1:{port}:445", "--volume", f"{share}:/share",
                            "--volume", f"{config}:/etc/samba/smb.conf:ro", image, "sh", "-c", setup],
                           check=True, capture_output=True, text=True, timeout=180)
            started = True
            deadline = time.monotonic() + 180
            while True:
                ready = subprocess.run([docker, "exec", container, "smbcontrol", "smbd", "ping"], capture_output=True, timeout=10)
                if ready.returncode == 0:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError("temporary Samba startup timed out")
                time.sleep(1)
            subprocess.run(cli + ["profile", "add", "native", "--protocol", "smb", "--url",
                                f"smb://127.0.0.1:{port}/test/nested", "--username", "test"],
                           check=True, capture_output=True, timeout=10)
            result = terminal_command(cli + ["mount", "native", str(point), "--read-only"], "native-smb-password")
            mounted = True
            actual = json.loads(subprocess.check_output(cli + ["mounts"], timeout=10))
            volume = next(m for m in actual if m["mountpoint"] == str(point))
            assert volume["filesystem"] == "smbfs" and volume["read_only"]
            assert "/test/nested" in volume["source"], result
            assert (point / "hello.txt").read_text() == "native encrypted SMB bytes\n"
            assert not (point / "outside-root.txt").exists()
            try:
                (point / "forbidden.txt").write_text("must be readonly")
            except OSError:
                pass
            else:
                raise AssertionError("readonly native mount accepted a write")
            stats = subprocess.check_output(["/usr/bin/smbutil", "statshares", "-m", str(point), "-f", "JSON"], timeout=15).decode()
            assert "SMB_CURR_ENCRYPT_ALGORITHM" in stats and "AES" in stats, stats
            subprocess.run(cli + ["unmount", str(point)], check=True, capture_output=True, timeout=15)
            mounted = False
            assert not list(point.iterdir())
            after = json.loads(subprocess.check_output(cli + ["mounts"], timeout=10))
            assert all(m["mountpoint"] != str(point) for m in after)
            print("PASS native encrypted SMB submount, terminal credential prompt, kernel readonly, file read, scope, and verified unmount", flush=True)
            terminal_command(cli + ["mount", "native", str(point)], "native-smb-password")
            mounted = True
            with (point / "written.txt").open("w") as written:
                written.write("native SMB write reached server\n")
                written.flush()
                os.fsync(written.fileno())
            assert (nested / "written.txt").read_text() == "native SMB write reached server\n"
            (point / "written.txt").rename(point / "renamed.txt")
            assert (nested / "renamed.txt").exists()
            (point / "renamed.txt").unlink()
            assert not (nested / "renamed.txt").exists()
            subprocess.run(cli + ["unmount", str(point)], check=True, capture_output=True, timeout=15)
            mounted = False
            assert not list(point.iterdir())
            print("PASS native encrypted SMB writable mount, server-confirmed write, rename, delete, and unmount", flush=True)
        finally:
            # Also check for a mount left by an interrupted prompt/helper.
            observed = subprocess.run(cli + ["mounts"], capture_output=True, text=True, timeout=10)
            if observed.returncode == 0:
                mounted = mounted or any(m["mountpoint"] == str(point) for m in json.loads(observed.stdout))
            if mounted:
                subprocess.run(cli + ["unmount", str(point)], capture_output=True, timeout=15)
            if started:
                subprocess.run([docker, "rm", "--force", container], capture_output=True, timeout=30)


if __name__ == "__main__":
    run_tests(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/rdrive").resolve())
