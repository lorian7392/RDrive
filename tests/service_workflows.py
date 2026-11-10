#!/usr/bin/env python3
"""Isolated resident-service proofs; never installs a user's default LaunchAgent.

The temporary plist has a home-derived unique label, an explicit temporary agent
directory, and a synthetic WebDAV profile. Only that label and its own processes
are stopped. Pass --launchd to additionally test the real per-user launchd job.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import plistlib
import signal
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import uuid

from index_workflows import IndexedDav, read_provider_journal, wait

ROOT = Path(__file__).resolve().parents[1]


def run(binary, launchd=False):
    binary = Path(binary).resolve()
    before_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    checks = []
    child_pids = set()
    supervisor = None
    installed = False
    service_label = None
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="rdsvc-", dir="/private/tmp") as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        home, ipc, agents = root / "state", root / "ipc", root / "agents"
        remote = root / "remote"
        remote.mkdir()
        (remote / "example.txt").write_bytes(b"synthetic service file")
        class Fixture(IndexedDav):
            pass
        Fixture.root = remote
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        variable = "RDRIVE_SERVICE_TEST_" + uuid.uuid4().hex.upper()
        env = dict(os.environ, **{variable: "fixture-password"})
        env.pop("RDRIVE_WEBDAV_LOCK_TOKEN", None)
        base = [str(binary), "--home", str(home), "--json", "--progress", "off"]
        log = (root / "supervisor.log").open("w+")

        def cli(*args, success=True, environment=None, executable=None):
            command = [str(executable), *base[1:]] if executable else base
            result = subprocess.run(command + list(args), cwd=ROOT, env=environment or env,
                                    capture_output=True, text=True, timeout=45)
            if success:
                assert result.returncode == 0, (args, result.stdout, result.stderr)
                return json.loads(result.stdout)
            assert result.returncode != 0, (args, result.stdout)
            return result

        def state():
            value = cli("service", "status")
            runtime = value.get("runtime") or {}
            for row in runtime.get("children", {}).values():
                if row.get("pid"):
                    child_pids.add(row["pid"])
            return value

        def rpc(**message):
            descriptor = json.loads((ipc / "bridge-endpoint.json").read_text())
            message.update(v=1, auth_token=descriptor["token"], domain="resident")
            with socket.create_connection(("127.0.0.1", descriptor["port"]), timeout=10) as client:
                client.sendall(json.dumps(message).encode() + b"\n")
                with client.makefile("rb") as reader:
                    while line := reader.readline():
                        value = json.loads(line)
                        if "progress" not in value:
                            assert value.get("ok"), value
                            return value
            raise AssertionError("fixture broker disconnected")

        def broker_pid():
            value = state()
            if value.get("supervisor_running"):
                return (value.get("runtime") or {}).get("children", {}).get("broker", {}).get("pid")
            return None

        def record(name):
            checks.append(name)
            print("PASS " + name, flush=True)

        try:
            cli("profile", "add", "fixture", "--protocol", "webdav", "--url", f"http://127.0.0.1:{server.server_port}/dav/",
                "--allow-http", "--username", "user", "--password-env", variable, "--timeout", "2")
            cli("smart", "setup", "resident", "fixture")
            options = ("--ipc-root", str(ipc), "--development-tcp", "--launch-agents-dir", str(agents))
            plan = cli("service", "install", *options, "--dry-run")
            parsed = plistlib.loads(plan["plist"].encode())
            assert parsed["KeepAlive"] is True and parsed["RunAtLoad"] is True
            assert parsed["ProgramArguments"] == [str(binary), "--home", str(home), "--progress", "off", "service", "run"]
            assert "EnvironmentVariables" not in parsed and "fixture-password" not in plan["plist"]
            assert not (home / "service").exists() and not agents.exists() and not ipc.exists()
            record("install dry-run renders a fixed-argv KeepAlive LaunchAgent without creating service files or persisting credentials")

            installed_state = cli("service", "install", *options)
            installed = True
            service_label = installed_state["label"]
            plist_path = Path(installed_state["plist_path"])
            first = plist_path.read_bytes()
            assert plist_path.parent == agents
            assert plist_path.stat().st_mode & 0o777 == 0o600
            assert not installed_state["launchd"]["loaded"] and not installed_state["supervisor_running"]
            cli("service", "install", *options)
            assert first == plist_path.read_bytes()
            for operation in ("start", "stop", "restart", "uninstall"):
                assert cli("service", operation, "--dry-run")["dry_run"] is True
            record("installation is private, idempotent, isolated from the default LaunchAgents directory, and all mutations have dry-run plans")

            supervisor = subprocess.Popen(base + ["service", "run"], env=env, stdout=log, stderr=log)
            first_pid = wait(broker_pid, "foreground supervised broker", 20)
            wait(lambda: rpc(op="domains")["domains"], "broker bridge", 15)
            wait(lambda: rpc(op="list", parent_id="root")["items"], "restored configured Smart catalog", 20)
            known = read_provider_journal(home, "resident")["items"]
            assert any(row["path"] == "example.txt" for row in known.values())
            assert not state()["launchd"]["loaded"]
            cli("service", "run", success=False)
            cli("service", "start", success=False)
            assert supervisor.poll() is None and broker_pid() == first_pid
            record("foreground supervisor restores configured Smart domains and rejects duplicate owners without disturbing the running broker")

            os.kill(first_pid, signal.SIGKILL)
            replacement = wait(lambda: (pid if (pid := broker_pid()) and pid != first_pid else None), "broker restart after SIGKILL", 20)
            assert replacement != first_pid
            wait(lambda: rpc(op="list", parent_id="root")["items"], "catalog after broker crash", 20)
            assert state()["runtime"]["children"]["broker"]["restarts"] >= 1
            (remote / "new.txt").write_bytes(b"remote metadata after restart")
            cli("index", "refresh", "resident")
            wait(lambda: any(row["name"] == "new.txt" for row in rpc(op="list", parent_id="root")["items"]), "background refresh after restart", 20)
            record("a killed broker restarts with backoff and retains SQLite namespace while background indexing resumes")

            environment = dict(env)
            environment.pop(variable)
            diagnostics = cli("service", "status", environment=environment)
            assert diagnostics["credentials"]["environment_profiles"][0]["available_in_this_process"] is False
            assert diagnostics["runtime"]["credentials"]["environment_profiles"][0]["available_in_this_process"] is True
            for file in [plist_path, *(home / "service").iterdir()]:
                if file.is_file():
                    assert b"fixture-password" not in file.read_bytes(), file.name
            record("credential diagnostics distinguish caller and resident environment while configuration, plist, status and logs contain no password values")

            stopped = cli("service", "stop")
            supervisor.wait(timeout=15)
            assert not stopped["supervisor_running"] and stopped["runtime"]["state"] == "stopped"
            assert (home / "provider").exists()
            record("stop gracefully terminates only owned children and preserves persistent Smart state")

            if launchd:
                cli("service", "start")
                def launched():
                    value = state()
                    return value if value["launchd"]["loaded"] and value["launchd"]["managed"] and value["launchd"]["running"] and value["supervisor_running"] and broker_pid() else None
                launched_state = wait(launched, "isolated launchd service start", 30)
                launchd_pid = launched_state["launchd"]["pid"]
                assert launched_state["runtime"]["supervisor_pid"] == launchd_pid
                assert launched_state["runtime"]["credentials"]["environment_profiles"][0]["available_in_this_process"] is False
                record("real per-user launchd starts only the isolated temporary label; terminal credentials are not silently inherited")

                os.kill(launchd_pid, signal.SIGKILL)
                def relaunched():
                    value = launched()
                    return value if value and value["launchd"]["pid"] != launchd_pid else None
                wait(relaunched, "launchd KeepAlive after supervisor SIGKILL", 50)
                cli("service", "restart")
                wait(launched, "explicit isolated launchd restart", 35)
                record("launchd KeepAlive recovers a killed supervisor and explicit restart restores the managed process tree")

            removed = cli("service", "uninstall")
            installed = False
            assert not removed["installed"] and not plist_path.exists()
            assert read_provider_journal(home, "resident")["items"]
            assert not (home / "service/installation.json").exists()
            record("uninstall bootouts only the owned temporary label and preserves cached catalog and retained user data")

            copied_binary = root / "installed-rdrive"
            shutil.copy2(binary, copied_binary)
            cli("service", "install", *options, executable=copied_binary)
            installed = True
            if launchd:
                cli("service", "start")
                wait(launched, "copied executable launchd start", 30)
            else:
                supervisor = subprocess.Popen([str(copied_binary), *base[1:], "service", "run"],
                                              env=env, stdout=log, stderr=log)
            copied_pid = wait(broker_pid, "copied executable broker", 20)
            copied_binary.rename(root / "moved-rdrive")
            diagnostics = state()
            assert diagnostics["installed"] and diagnostics["supervisor_running"]
            if launchd:
                assert diagnostics["launchd"]["loaded"] and diagnostics["launchd"]["managed"]
            cli("service", "start", success=False)
            cli("service", "restart", success=False)
            assert broker_pid() == copied_pid, "failed restart disturbed an existing owned process"
            stopped = cli("service", "stop")
            if supervisor is not None and supervisor.poll() is None:
                supervisor.wait(timeout=15)
            assert not stopped["supervisor_running"] and not stopped["launchd"]["loaded"]
            repaired = cli("service", "install", *options)
            repaired_plist = plistlib.loads(Path(repaired["plist_path"]).read_bytes())
            assert repaired_plist["ProgramArguments"][0] == str(binary)
            assert not cli("service", "uninstall")["installed"]
            installed = False
            record("a moved service executable remains inspectable and stoppable; restart fails without disrupting its owner and reinstall repairs the old path")

            obsolete_binary = root / "obsolete-rdrive"
            shutil.copy2(binary, obsolete_binary)
            cli("service", "install", *options, executable=obsolete_binary)
            installed = True
            obsolete_binary.unlink()
            assert state()["installed"]
            assert cli("service", "uninstall", "--dry-run")["dry_run"]
            assert not cli("service", "uninstall")["installed"]
            installed = False
            assert read_provider_journal(home, "resident")["items"]
            record("an installation whose old executable was removed can be uninstalled while preserving indexed user data")
        finally:
            if installed:
                subprocess.run(base + ["service", "stop"], env=env, capture_output=True, timeout=30)
                subprocess.run(base + ["service", "uninstall"], env=env, capture_output=True, timeout=30)
            if supervisor is not None and supervisor.poll() is None:
                supervisor.terminate()
                supervisor.wait(timeout=15)
            server.shutdown()
            server.server_close()
            log.close()
            if service_label:
                left = subprocess.run(["/bin/launchctl", "print", f"gui/{os.getuid()}/{service_label}"], capture_output=True, text=True, timeout=10)
                assert left.returncode != 0, "owned isolated launchd label remains loaded"
            for pid in child_pids:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue
                raise AssertionError(f"owned fixture child remains running: {pid}")
    after_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert before_hash == after_hash, "binary changed during fixture run"
    return dict(binary_sha256=before_hash, checks=checks, passed=len(checks), launchd_exercised=launchd,
                cleanup_verified=True, elapsed_seconds=round(time.monotonic()-started,2),
                scope="Temporary synthetic WebDAV profile, private service and IPC directories, and a unique temporary LaunchAgent; real mount and existing services unchanged.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/rdrive")
    parser.add_argument("--launchd", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = run(args.binary, args.launchd)
    if args.output:
        args.output.write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps(result,ensure_ascii=False))
