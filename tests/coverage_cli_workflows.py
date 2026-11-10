#!/usr/bin/env python3
"""Additional real CLI branches against disposable DAV, editor and companion peers."""
import argparse
import http.server
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import threading
import time

from mcp_workflows import LocalDav


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="rdrive-cli-coverage-", dir="/private/tmp") as temporary:
        root = Path(temporary).resolve()
        remote = root / "remote"
        remote.mkdir()
        LocalDav.root = remote
        LocalDav.requests_seen = []
        LocalDav.locks = {}
        LocalDav.interrupt_once = set()
        LocalDav.range_requests = []
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), LocalDav)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = dict(os.environ, RDRIVE_HOME=str(root / "state"), RDRIVE_CLI_COVERAGE_PASSWORD="fixture-password")
        environment.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        def cli(*arguments, ok=True, raw=False, extra_env=None):
            env = environment.copy()
            if extra_env:
                env.update(extra_env)
            result = subprocess.run([str(binary), "--json", "--progress", "off", *map(str, arguments)],
                                    env=env, capture_output=True, timeout=20)
            assert (result.returncode == 0) == ok, (arguments, result.returncode, result.stdout, result.stderr)
            if not ok:
                return result
            return result.stdout if raw else json.loads(result.stdout)
        def editor(body):
            path = root / "synthetic-editor"
            path.write_text("#!/bin/sh\n" + body + "\n")
            path.chmod(0o700)
            return {"EDITOR": str(path)}
        def broken_output(*arguments, extra_env=None):
            reader, writer = os.pipe()
            os.close(reader)
            env = environment.copy()
            if extra_env:
                env.update(extra_env)
            try:
                result = subprocess.run([str(binary), "--json", "--progress", "off", *map(str, arguments)],
                                        env=env, stdout=writer, stderr=subprocess.PIPE, timeout=20)
            finally:
                os.close(writer)
            assert result.returncode == 1 and b"cannot write command output" in result.stderr, (arguments, result.returncode, result.stderr)
            assert b"panicked" not in result.stderr
        def stop_watch(arguments, stream, expected):
            process = subprocess.Popen([str(binary), *arguments], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                selected = process.stdout if stream == "stdout" else process.stderr
                assert select.select([selected], [], [], 15)[0], "Watcher did not produce its first real result"
                line = selected.readline()
                assert expected in line, line
                time.sleep(0.1)
                process.send_signal(signal.SIGINT)
                stdout, stderr = process.communicate(timeout=5)
                assert process.returncode == 0, (line, stdout, stderr)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=5)
        try:
            cli("profile", "add", "dav", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/",
                "--allow-http", "--username", "user", "--password-env", "RDRIVE_CLI_COVERAGE_PASSWORD")
            source = root / "source.txt"
            source.write_bytes(b"original")
            cli("mkdir", "dav", "documents")
            cli("put", "dav", source, "documents/edit.txt")
            assert cli("edit", "dav", "documents/edit.txt", extra_env=editor("exit 0"))["changed"] is False
            cli("edit", "dav", "documents/edit.txt", extra_env=editor("printf 'edited' > \"$1\""))
            assert (remote / "documents/edit.txt").read_bytes() == b"edited"
            failed = cli("edit", "dav", "documents/edit.txt", extra_env=editor("printf 'retained' > \"$1\"; exit 1"), ok=False)
            assert b"edited file retained" in failed.stderr
            cli("edit", "dav", "documents/edit.txt", extra_env={"EDITOR": str(root / "missing-editor")}, ok=False)
            assert cli("url", "dav", "documents/edit.txt")["public_link"] is False
            cli("ls", "dav", "documents", "--recursive", "--contains", "absent")
            cli("cat", "dav", "documents/edit.txt", raw=True)
            cli("cache", "fetch", "dav", "documents", ok=False)
            cli("cache", "fetch", "dav", "documents", "--recursive", "--pin")
            assert cli("cache", "unpin", "dav", "documents", "--recursive")["unpinned"] == 1
            cli("cache", "list", "dav")
            cli("cache", "prune")
            first = cli("versions", "save", "dav", "documents/edit.txt")
            second = cli("versions", "save", "dav", "documents/edit.txt")
            versions = cli("versions", "list", "dav", "documents/edit.txt")
            assert len(versions) == 2 and first["id"] != second["id"]
            preview = cli("versions", "prune", "dav", "documents/edit.txt", "--keep", "1")
            assert preview["applied"] is False and len(preview["versions"]) == 1
            cli("versions", "prune", "dav", "documents/edit.txt", "--keep", "1", "--apply")
            assert len(cli("versions", "list", "dav", "documents/edit.txt")) == 1
            cli("versions", "restore", "dav", "documents/edit.txt", second["id"], ok=False)
            cli("versions", "restore", "dav", "documents/edit.txt", second["id"], "--overwrite")
            cli("notify", "dav", "documents", "--count", "1", ok=False)
            cli("put", "dav", source, "unsupported-resume", "--resume", ok=False)
            cli("put", "dav", root / "missing-source", "missing", ok=False)
            cli("rm", "dav", "", ok=False)
            cli("versions", "restore", "dav", "documents/edit.txt", "../invalid", ok=False)
            cli("mkdir", "dav", "managed")
            cli("repo", "init", "dav", "managed")
            transaction = cli("repo", "begin", "dav", "managed")["id"]
            cli("repo", "mkdir", transaction, "folder")
            cli("repo", "put", transaction, source, "folder/file")
            cli("repo", "mv", transaction, "folder", "renamed")
            cli("repo", "rm", transaction, "renamed", "--recursive")
            cli("repo", "abort", transaction)
            cli("repo", "transactions")
            cli("smart", "run", "--staging", root / "smart-staging")
            empty = root / "empty-mountpoint"
            empty.mkdir(mode=0o700)
            plan = cli("mount", "dav", empty, "--read-only", "--browse", "--no-prompt", "--dry-run")
            assert plan["filesystem"] == "webdav" and plan["read_only"]
            (empty / "do-not-hide").write_bytes(b"protected")
            cli("mount", "dav", empty, "--no-prompt", ok=False)
            assert (empty / "do-not-hide").read_bytes() == b"protected"
            local = root / "sync"
            local.mkdir()
            cli("mkdir", "dav", "sync")
            symlink = root / "sync-link"
            symlink.symlink_to(local, target_is_directory=True)
            assert b"symbolic link" in cli("sync", "dav", symlink, "sync", ok=False).stderr
            stop_watch(["--json", "--progress", "off", "watch", "dav", str(local), "sync", "--interval", "1"], "stdout", b"applied")
            stop_watch(["--json", "watch", "missing", str(local), "--interval", "1"], "stderr", b"watch_error")
            stop_watch(["watch", "missing", str(local), "--interval", "1"], "stderr", b"watch:")
            ordinary = subprocess.run([str(binary), "status"], env=environment, capture_output=True, timeout=20)
            assert ordinary.returncode == 0 and b"\n  " in ordinary.stdout
            failure = subprocess.run([str(binary), "stat", "missing", "file"], env=environment, capture_output=True, timeout=20)
            assert failure.returncode == 1 and b"error:" in failure.stderr, (failure.returncode, failure.stderr)
            for arguments in [
                ("info", "dav"),
                ("get", "dav", "documents/edit.txt", root / "resume-output", "--resume"),
                ("cp", "dav", "documents/edit.txt", "documents/output-copy"),
                ("url", "dav", "documents/edit.txt"),
                ("cache", "clear"), ("cache", "status"), ("cache", "list"),
                ("queue", "run", "dav"), ("transfers", "list"),
                ("versions", "save", "dav", "documents/edit.txt"),
                ("versions", "list", "dav", "documents/edit.txt"),
                ("versions", "prune", "dav", "documents/edit.txt", "--keep", "1"),
                ("status",), ("history",),
            ]:
                broken_output(*arguments)
            pending = cli("put", "dav", source, "output-cancel", "--queue")
            broken_output("queue", "cancel", pending["id"])
            cli("queue", "cancel", pending["id"])
            assert not (root / "state/spool" / pending["snapshot"]).exists()
            LocalDav.interrupt_once.add("/dav/documents/edit.txt")
            abandoned = root / "discarded-download"
            cli("get", "dav", "documents/edit.txt", abandoned, "--resume", ok=False)
            checkpoint = next(item for item in cli("transfers", "list") if item.get("remote") == "documents/edit.txt" and item.get("direction") == "download")
            broken_output("transfers", "discard", checkpoint["id"])
            assert not abandoned.exists()
            assert not (root / "state/transfers" / (checkpoint["id"] + ".json")).exists()
            broken_output("lock", "dav", "documents/edit.txt")
            broken_output("unlock", "dav", "documents/edit.txt", extra_env={"RDRIVE_WEBDAV_LOCK_TOKEN": "opaquelocktoken:fixture-lock"})
            print("PASS disposable DAV editor/versions/cache/mount-preflight/watch CLI workflows")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)


if __name__ == "__main__":
    main()
