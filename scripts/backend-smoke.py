#!/usr/bin/env python3
"""Backend/host fault tests. Only fixture-owned processes and private temp paths.
Run: bash scripts/isolated.sh python3 scripts/backend-smoke.py
Requires cargo build --workspace first. No installed Muster executable is used.
"""
from http.client import HTTPConnection
import json
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]
BIN = PROJECT / "target/debug/muster"
assert BIN.is_file() and BIN.with_name("muster-terminal-host").is_file(), "cargo build --workspace first"
BASE = Path(tempfile.mkdtemp(prefix="muster-backend-smoke-"))
print(f"Isolated backend smoke files: {BASE}", flush=True)
RUNTIME, DATA, HOME = (BASE / name for name in ("run", "data", "home"))
for path in (RUNTIME, DATA, HOME):
    path.mkdir(mode=0o700)
with socket.socket() as reserve:
    reserve.bind(("127.0.0.1", 0))
    PORT = reserve.getsockname()[1]
ENV = {"PATH": "/usr/bin:/bin", "HOME": str(HOME), "SHELL": "/bin/bash", "LANG": "C.UTF-8",
       "MUSTER_RUNTIME_DIR": str(RUNTIME), "MUSTER_DATA_DIR": str(DATA), "MUSTER_PORT": str(PORT)}
current = None
logs = []
token = None
passed = 0


def check(condition, message):
    global passed
    assert condition, message
    passed += 1
    print(f"  ok {message}", flush=True)


def until(message, fn):
    end = time.monotonic() + 15
    while time.monotonic() < end:
        result = fn()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"timed out: {message}")


def http(path, body=None, credential=None):
    conn = HTTPConnection("127.0.0.1", PORT, timeout=5)
    try:
        headers = {"Authorization": f"Bearer {credential or token}", "Content-Type": "application/json"}
        conn.request("POST" if body is not None else "GET", path,
                     json.dumps(body) if body is not None else None, headers)
        response = conn.getresponse()
        raw = response.read()
        return response.status, json.loads(raw) if raw else None
    finally:
        conn.close()


def rpc(method, params=None):
    status, response = http("/api/rpc", {"id": 1, "method": method, "params": params or {}})
    assert status == 200 and "error" not in response, (method, response)
    return response["result"]


def start_backend():
    global current, token
    log = open(BASE / f"backend-{len(logs)}.log", "wb")
    logs.append(log)
    current = subprocess.Popen([str(BIN), "daemon", "--foreground"], env=ENV,
                               stdin=subprocess.DEVNULL, stdout=log, stderr=log)

    def ready():
        global token
        assert current.poll() is None, "backend exited; inspect fixture logs"
        try:
            discovery = json.loads((RUNTIME / "daemon.json").read_text())
            if discovery["pid"] != current.pid:
                return False
            token = discovery["token"]
            return http("/api/health")[0] == 200
        except (FileNotFoundError, ConnectionError, OSError):
            return False
    until("backend ready", ready)


def stop_backend(hard=False):
    global current
    if current and current.poll() is None:
        if hard:
            current.kill()
        else:
            current.terminate()
        current.wait(timeout=10)
    current = None


def host_call(method, params=None):
    hello = json.loads((RUNTIME / "terminal.json").read_text())
    assert Path(hello["data_dir"]) == DATA / "terminal"
    request = {"version": 1, "instance": hello["instance"], "command": {"method": method}}
    if params is not None:
        request["command"]["params"] = params
    payload = json.dumps(request).encode()
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(10)
        conn.connect(str(RUNTIME / "terminal.sock"))
        conn.sendall(bytes([0]) + struct.pack(">I", len(payload)) + payload)

        def read(n):
            result = b""
            while len(result) < n:
                part = conn.recv(n - len(result))
                assert part, "host connection ended early"
                result += part
            return result
        header = read(5)
        assert header[0] == 0
        reply = json.loads(read(struct.unpack(">I", header[1:])[0]))
        assert reply["type"] != "error", reply
        return reply.get("data")


def session(sid):
    return next(item for item in rpc("hello")["sessions"] if item["id"] == sid)


def output(sid):
    return "\n".join(rpc("session.output", {"id": sid, "lines": 200})["lines"])


try:
    start_backend()
    host_instance = json.loads((RUNTIME / "terminal.json").read_text())["instance"]
    workspace = rpc("workspace.add", {"path": str(HOME)})
    channel = rpc("channel.create", {"workspace_id": workspace["id"], "agent": "shell"})
    first = next(item for item in rpc("hello")["sessions"] if item["channel_id"] == channel["id"])
    sid, pid = first["id"], first["pid"]
    rpc("session.input", {"id": sid, "data": "for i in $(seq 1 80); do printf 'COUNT:%s\\n' \"$i\"; sleep 0.1; done\r"})
    until("ticker started", lambda: "COUNT:2" in output(sid))
    stop_backend(hard=True)
    time.sleep(0.6)
    inventory = host_call("inventory")
    live = next(item for item in inventory if item["run"]["id"] == sid)
    check(live["alive"] and live["pid"] == pid, "backend SIGKILL preserves the terminal process")
    start_backend()
    check(session(sid)["pid"] == pid and session(sid)["state"] != "exited", "new backend adopts the original process")
    check("COUNT:5" in output(sid), "output produced during backend downtime is available")
    check(json.loads((RUNTIME / "terminal.json").read_text())["instance"] == host_instance,
          "backend restart retains the same terminal host")
    with sqlite3.connect(DATA / "muster.db") as database:
        record = json.loads(database.execute("SELECT record FROM terminal_runs WHERE session_id=?", (sid,)).fetchone()[0])
    check(http("/hook/Stop", {"hook_event_name": "Stop"}, record["token"])[0] == 200,
          "session hook credentials survive backend restart")
    stop_backend()
    check(next(item for item in host_call("inventory") if item["run"]["id"] == sid)["alive"],
          "graceful backend shutdown preserves the terminal")
    start_backend()
    dying = rpc("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
    rpc("session.input", {"id": dying["id"], "data": "sleep 0.4; exit 7\r"})
    stop_backend(hard=True)
    time.sleep(0.7)
    start_backend()
    check(session(dying["id"])["exit_code"] == 7 and session(dying["id"])["state"] == "exited",
          "exit during backend outage is reconciled with its actual status")
    restarted = rpc("session.restart", {"id": sid})
    check(restarted["pid"] != pid and restarted["state"] != "exited", "explicit session restart creates one new process")
    rpc("session.input", {"id": sid, "data": "printf 'AFTER-RESTART\\n'\r"})
    until("replacement terminal works", lambda: "AFTER-RESTART" in output(sid))
    check("COUNT:5" not in output(sid), "replacement run has independent replay")
    full_stop = subprocess.Popen([str(BIN), "stop"], env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if current:
        current.wait(timeout=10)
        current = None
    stdout, stderr = full_stop.communicate(timeout=15)
    check(full_stop.returncode == 0, f"explicit full stop succeeds: {stderr.decode().strip()}")
    check(not (RUNTIME / "terminal.sock").exists(), "full stop terminates the terminal host")
    print(f"Passed {passed} backend survival checks.", flush=True)
finally:
    stop_backend()
    if (RUNTIME / "terminal.sock").exists():
        try:
            host_call("shutdown")
            until("fixture host cleanup", lambda: not (RUNTIME / "terminal.sock").exists())
        except Exception as error:
            print(f"Fixture host cleanup needs inspection at {BASE}: {error}", flush=True)
    for log in logs:
        log.close()
    # Preserve private logs for diagnosis; no PID is read from a file and signalled.
