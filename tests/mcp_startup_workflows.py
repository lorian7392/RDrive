#!/usr/bin/env python3
"""Verify real MCP process startup while existing sessions and CPU work compete.

Only temporary local directories and fixture-owned children are used. No profile,
network connection, native service, or configured application state is opened.
The ordinary MCP client's 35-second response bound is unchanged.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from mcp_workflows import Client


CPU_WORK = """import pathlib, sys, time
pathlib.Path(sys.argv[1]).write_text('ready')
deadline = time.monotonic() + 60
value = 1
while time.monotonic() < deadline:
    value = (value * 48271) % 2147483647
"""


def initialize(binary, home, allowed, environment, case, *, keep=False):
    started = time.monotonic()
    result = dict(case, ok=False)
    client = None
    try:
        client = Client(binary, home, environment, write=not keep, roots=[allowed])
        result["pid"] = client.process.pid
        client.initialize()
        result["ok"] = True
    except Exception as error:
        result["failure"] = str(error)
    finally:
        result["seconds"] = time.monotonic() - started
        if client is not None and not (keep and result["ok"]):
            try:
                client.close()
            except Exception as error:
                result["ok"] = False
                result["cleanup_failure"] = str(error)
    return result, client if keep and result["ok"] else None


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def exercise(binary, starts, report):
    with tempfile.TemporaryDirectory(prefix="rdrive-mcp-startup-") as temporary:
        root = Path(temporary).resolve()
        for workers in (1, 2):
            home = root / f"workers-{workers}" / "state"
            home.mkdir(parents=True, mode=0o700)
            allowed = home.parent / "allowed"
            allowed.mkdir(mode=0o700)
            # Retain the caller's coverage output destination so every real
            # process contributes to the same fresh, frozen collection.
            environment = dict(os.environ, TOKIO_WORKER_THREADS=str(workers))
            idle, load = [], []
            try:
                for index in range(4):
                    case, client = initialize(binary, home, allowed, environment,
                                             {"workers": workers, "kind": "idle", "index": index},
                                             keep=True)
                    report["cases"].append(case)
                    assert case["ok"], case.get("failure", case)
                    idle.append(client)
                ready = [home.parent / f"cpu-{index}.ready" for index in range(4)]
                for path in ready:
                    load.append(subprocess.Popen([sys.executable, "-c", CPU_WORK, str(path)],
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
                deadline = time.monotonic() + 10
                while not all(path.is_file() for path in ready):
                    assert all(process.poll() is None for process in load), "CPU fixture exited before readiness"
                    assert time.monotonic() < deadline, "CPU fixture readiness timed out"
                    time.sleep(0.01)
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                    futures = [executor.submit(initialize, binary, home, allowed, environment,
                               {"workers": workers, "kind": "startup", "index": index})
                               for index in range(starts)]
                    report["cases"].extend(future.result()[0] for future in futures)
                # Existing sessions must still respond after competing launches.
                for client in idle:
                    assert client.request("ping") == {}
            finally:
                errors = []
                for process in load:
                    try:
                        stop(process)
                    except Exception as error:
                        errors.append(str(error))
                for client in idle:
                    try:
                        client.close()
                    except Exception as error:
                        errors.append(str(error))
                assert not errors, "\n".join(errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/rdrive")
    parser.add_argument("--starts", type=int, default=128, help="New processes per worker-count case (default: 128)")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if args.starts < 1:
        parser.error("--starts must be positive")
    binary = Path(args.binary).resolve(strict=True)
    report = {"schema": 1, "validated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "parameters": {"starts_per_worker_count": args.starts, "worker_counts": [1, 2],
                             "concurrent_starts": 4, "idle_sessions": 4, "cpu_workers": 4}, "cases": []}
    try:
        exercise(binary, args.starts, report)
    except Exception as error:
        report["failure"] = str(error)
    report["passed"] = sum(case["ok"] for case in report["cases"])
    report["total"] = len(report["cases"])
    report["status"] = "passed" if ("failure" not in report
        and report["passed"] == report["total"] == 2 * (args.starts + 4)) else "failed"
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: report[key] for key in ("status", "passed", "total", "binary_sha256")}))
    failures = [case.get("failure", case.get("cleanup_failure", ""))
                for case in report["cases"] if not case["ok"]]
    assert report["status"] == "passed", report.get("failure", "\n".join(failures))


if __name__ == "__main__":
    main()
