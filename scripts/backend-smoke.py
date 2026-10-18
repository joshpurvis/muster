#!/usr/bin/env python3
"""Backend/host fault tests. Only fixture-owned processes and private temp paths.
Run: bash scripts/isolated.sh python3 scripts/backend-smoke.py
Requires cargo build --workspace first. No installed Muster executable is used.
"""
from http.client import HTTPConnection
import json
import re
import shutil
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
sys.dont_write_bytecode = True
from terminal_fault_proxy import SpawnGate

PROJECT = Path(__file__).resolve().parents[1]
BIN = PROJECT / "target/debug/muster"
assert BIN.is_file() and BIN.with_name("muster-terminal-host").is_file(), "cargo build --workspace first"
BASE = Path(tempfile.mkdtemp(prefix="muster-backend-smoke-"))
print(f"Isolated backend smoke files: {BASE}", flush=True)
BUILD = BASE / "disposable-build"
BUILD.mkdir(mode=0o700)
shutil.copy2(BIN, BUILD / "muster")
shutil.copy2(BIN.with_name("muster-terminal-host"), BUILD / "muster-terminal-host")
BIN = BUILD / "muster"
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
detached_reload = False


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


def http(path, body=None, credential=None, timeout=5):
    conn = HTTPConnection("127.0.0.1", PORT, timeout=timeout)
    try:
        headers = {"Authorization": f"Bearer {credential or token}", "Content-Type": "application/json"}
        conn.request("POST" if body is not None else "GET", path,
                     json.dumps(body) if body is not None else None, headers)
        response = conn.getresponse()
        raw = response.read()
        return response.status, json.loads(raw) if raw else None
    finally:
        conn.close()


def asset(path):
    conn = HTTPConnection("127.0.0.1", PORT, timeout=5)
    try:
        conn.request("GET", path)
        response = conn.getresponse()
        return response.status, response.read().decode()
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
    with sqlite3.connect(DATA / "muster.db") as database:
        record = json.loads(database.execute("SELECT record FROM terminal_runs WHERE session_id=?", (sid,)).fetchone()[0])
    stable_bin = Path(record["spec"]["env"]["MUSTER_BIN"])
    check(stable_bin == DATA / "bin/muster" and stable_bin.is_file(),
          "terminals receive a stable application command path")
    # Remove only this fixture's build, never the worktree or installed executables.
    # Subsequent callbacks, backend restarts and reloads run through the saved path.
    shutil.rmtree(BUILD)
    BIN = stable_bin
    helper = subprocess.run([str(BIN), "ls", "--json"], env=ENV,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
    check(helper.returncode == 0 and any(item["id"] == sid for item in json.loads(helper.stdout)),
          "saved command works after its original build directory is deleted")
    def mcp_post():
        status, response = http("/mcp", {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "post_to_channel", "arguments": {"text": "MCP restart-limit fixture"}}}, record["token"])
        assert status == 200 and "result" in response, response
        return response["result"]
    for _ in range(30):
        assert not mcp_post().get("isError", False)
    rpc("session.input", {"id": sid, "data": "for i in $(seq 1 80); do printf 'COUNT:%s\\n' \"$i\"; sleep 0.1; done\r"})
    until("ticker started", lambda: "COUNT:2" in output(sid))
    stop_backend(hard=True)
    time.sleep(0.6)
    inventory = host_call("inventory")
    live = next(item for item in inventory if item["run"]["id"] == sid)
    check(live["alive"] and live["pid"] == pid, "backend SIGKILL preserves the terminal process")
    start_backend()
    check(session(sid)["pid"] == pid and session(sid)["state"] != "exited", "new backend adopts the original process")
    limited = mcp_post()
    check(limited.get("isError", False) and "rate limit" in str(limited),
          "MCP rate limits remain exhausted after backend replacement")
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
    hook_env = ENV | {"MUSTER_URL": f"http://127.0.0.1:{PORT}", "MUSTER_TOKEN": record["token"],
                      "MUSTER_HOOK_DB": str(DATA / "hook-inbox.sqlite")}
    callback = subprocess.run([str(BIN), "hook", "shell", "SmokeDurableCallback", '{"saved":"during outage"}'],
                              env=hook_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
    check(callback.returncode == 0 and not callback.stdout and not callback.stderr,
          "hook helper durably accepts a callback while the backend is absent")
    with sqlite3.connect(DATA / "hook-inbox.sqlite") as database:
        saved_callback = database.execute("SELECT id,token,event,payload FROM hooks ORDER BY id DESC LIMIT 1").fetchone()
    assert saved_callback is not None
    def callback_count():
        with sqlite3.connect(DATA / "muster.db") as database:
            return database.execute("SELECT COUNT(*) FROM events WHERE kind='hook' AND json_extract(data,'$.event')='SmokeDurableCallback'").fetchone()[0]
    start_backend()
    check(callback_count() == 1, "replacement backend drains the durable callback before serving clients")
    # Simulate an acknowledgement lost after application commit: restore the same inbox row.
    stop_backend(hard=True)
    with sqlite3.connect(DATA / "hook-inbox.sqlite") as database:
        database.execute("INSERT INTO hooks(id,token,event,payload) VALUES (?,?,?,?)", saved_callback)
    start_backend()
    check(callback_count() == 1, "a callback with a committed checkpoint is not applied twice")
    dying = rpc("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
    rpc("session.input", {"id": dying["id"], "data": "sleep 0.4; exit 7\r"})
    stop_backend(hard=True)
    time.sleep(0.7)
    start_backend()
    check(session(dying["id"])["exit_code"] == 7 and session(dying["id"])["state"] == "exited",
          "exit during backend outage is reconciled with its actual status")
    restarted = rpc("session.restart", {"id": sid})
    check(restarted["pid"] != pid and restarted["state"] != "exited", "explicit session restart creates one new process")
    callback = subprocess.run([str(BIN), "hook", "shell", "SmokeDurableCallback", '{}'], env=hook_env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
    assert callback.returncode == 0
    def inbox_empty():
        with sqlite3.connect(DATA / "hook-inbox.sqlite") as database:
            return database.execute("SELECT COUNT(*) FROM hooks").fetchone()[0] == 0
    until("obsolete callback drained", inbox_empty)
    check(callback_count() == 1, "old run callbacks cannot affect a restarted session")
    rpc("session.input", {"id": sid, "data": "printf 'AFTER-RESTART\\n'\r"})
    until("replacement terminal works", lambda: "AFTER-RESTART" in output(sid))
    check("COUNT:5" not in output(sid), "replacement run has independent replay")

    # Model a backend crash after removal is recorded but before kill/drain or
    # Forget finishes. Only this fixture's database is edited, with its backend off.
    retiring = rpc("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
    rpc("session.input", {"id": retiring["id"], "data": "trap '' HUP; printf 'RETIRE-READY\\n'; sleep 100\r"})
    until("retirement fixture ready", lambda: "RETIRE-READY" in output(retiring["id"]) and session(retiring["id"])["state"] == "working")
    stop_backend(hard=True)
    with sqlite3.connect(DATA / "muster.db") as database:
        retired = json.loads(database.execute("SELECT record FROM terminal_runs WHERE session_id=?", (retiring["id"],)).fetchone()[0])
        retired["retired"] = True
        database.execute("UPDATE terminal_runs SET record=? WHERE session_id=?", (json.dumps(retired), retiring["id"]))
        database.execute("DELETE FROM sessions WHERE id=?", (retiring["id"],))
    start_backend()
    def retirement_finished():
        inventory = host_call("inventory")
        with sqlite3.connect(DATA / "muster.db") as database:
            remaining = database.execute("SELECT COUNT(*) FROM terminal_runs WHERE session_id=?", (retiring["id"],)).fetchone()[0]
        return not remaining and all(item["run"] != retired["spec"]["run"] for item in inventory)
    until("retired run and record removed", retirement_finished)
    check(not (DATA / "terminal/runs" / f'{retiring["id"]}-{retired["spec"]["run"]["generation"]}').exists(),
          "retirement interrupted by a backend crash finishes after process exit and drain")
    check(session(sid)["pid"] == restarted["pid"], "retirement recovery preserves unrelated terminal processes")

    def launch_through_gate():
        result = []
        def request():
            try:
                result.append(http("/api/rpc", {"id": 1, "method": "session.spawn",
                    "params": {"channel_id": channel["id"], "kind": "shell"}}, timeout=20))
            except (ConnectionError, OSError) as error:
                result.append(error)
        sender = threading.Thread(target=request)
        sender.start()
        return sender, result

    def no_launch_record(run):
        with sqlite3.connect(DATA / "muster.db") as database:
            return database.execute("SELECT COUNT(*) FROM terminal_runs WHERE session_id=?", (run["id"],)).fetchone()[0] == 0

    for phase in ("before", "after"):
        with SpawnGate(RUNTIME, phase) as gate:
            sender, result = launch_through_gate()
            try:
                assert gate.entered.wait(5), f"spawn did not reach {phase} gate: {gate.errors}"
                run = gate.request["run"]
                if phase == "after":
                    assert gate.response["type"] == "status", gate.response
                rpc("session.remove", {"id": run["id"]})
                if phase == "before":
                    until("unspawned launch durably cancelled", lambda: no_launch_record(run))
                gate.release.set()
                sender.join(timeout=10)
                assert not sender.is_alive() and len(result) == 1, result
                assert isinstance(result[0], tuple) and "error" in result[0][1], result
                until("removed in-flight launch forgotten", lambda: no_launch_record(run)
                      and all(item["run"] != run for item in host_call("inventory")))
                check(all(item["id"] != run["id"] for item in rpc("hello")["sessions"]),
                      f"removal wins against a spawn delayed {phase} host creation")
            finally:
                gate.release.set()
                sender.join(timeout=12)

    with SpawnGate(RUNTIME, "after") as gate:
        sender, result = launch_through_gate()
        try:
            assert gate.entered.wait(5), f"spawn acknowledgement was not captured: {gate.errors}"
            assert gate.response["type"] == "status", gate.response
            run = gate.request["run"]
            unacknowledged_pid = gate.response["data"]["pid"]
            stop_backend(hard=True)
        finally:
            gate.release.set()
            sender.join(timeout=12)
    start_backend()
    check(session(run["id"])["pid"] == unacknowledged_pid
          and sum(item["run"] == run for item in host_call("inventory")) == 1,
          "backend crash before spawn acknowledgement adopts exactly the original process")
    rpc("session.remove", {"id": run["id"]})
    until("unacknowledged fixture run retired", lambda: no_launch_record(run))
    check(session(sid)["pid"] == restarted["pid"]
          and rpc("hello")["terminal_host"]["instance"] == host_instance,
          "delayed launch and removal faults preserve unrelated terminals and the host")

    # A prompt accepted into a busy terminal's queue must survive application replacement.
    waiting = BASE / "queued-command-runs"
    rpc("session.input", {"id": sid, "data": "sleep 4\r"})
    until("shell busy before enqueue", lambda: session(sid)["state"] == "working")
    sent = rpc("session.send", {"id": sid, "text": f"printf 'once\\n' >> {waiting}"})
    check(not sent["delivered"] and sent["queued"] == 1, "busy terminal accepts a durable waiting prompt")
    stop_backend(hard=True)
    start_backend()
    check(rpc("session.queue", {"id": sid})["queued"], "waiting prompt survives backend SIGKILL")
    until("restored queue delivered", lambda: waiting.exists() and waiting.read_text() == "once\n")
    until("restored queue finished", lambda: rpc("session.queue", {"id": sid})["stage"] is None)
    stop_backend(hard=True)
    start_backend()
    time.sleep(1)
    check(waiting.read_text() == "once\n", "completed prompt is not replayed after another restart")

    # Crash after the durable Typing checkpoint. The command may have reached the terminal;
    # replacement must pause for review, never make a second automatic attempt.
    uncertain = BASE / "uncertain-command-runs"
    until("shell ready before interrupted delivery", lambda: session(sid)["state"] == "idle")
    request_errors = []
    def send_uncertain():
        try:
            rpc("session.send", {"id": sid, "text": f"printf 'once\\n' >> {uncertain}; # " + "padding " * 300})
        except (ConnectionError, OSError, AssertionError) as error:
            request_errors.append(error)
    sender = threading.Thread(target=send_uncertain)
    sender.start()
    def typing_checkpoint():
        with sqlite3.connect(DATA / "muster.db") as database:
            row = database.execute("SELECT record FROM runtime_state WHERE key='prompt-queues'").fetchone()
        queue = json.loads(row[0]).get(str(sid), {}) if row else {}
        return (queue.get("active") or {}).get("stage") == "typing"
    until("delivery typing checkpoint", typing_checkpoint)
    stop_backend(hard=True)
    sender.join(timeout=10)
    assert not sender.is_alive(), "interrupted request thread did not finish"
    start_backend()
    review = rpc("session.queue", {"id": sid})
    check(review["paused"] and review["uncertain"] and review["stage"] == "typing",
          "interrupted prompt delivery is retained for review")
    before = uncertain.read_text() if uncertain.exists() else ""
    time.sleep(1.5)
    check((uncertain.read_text() if uncertain.exists() else "") == before,
          "ambiguous input is never automatically replayed")
    check(sid in rpc("hello")["paused_deliveries"], "reconnecting UI is told about paused delivery")
    rpc("session.clear_queue", {"id": sid})
    check(not rpc("session.queue", {"id": sid})["paused"], "explicit queue clearing resolves recovery pause")
    rpc("session.input", {"id": sid, "data": "\x03"})

    # UI installation changes only release files. Old HTML keeps loading its own bundle.
    ui_source = BASE / "ui-source"
    ui_source.mkdir()
    (ui_source / "muster-ui.json").write_text('{"protocol":1}')
    (ui_source / "index.html").write_text('<html>release-one<link href="app.css"><script src="app.js"></script></html>')
    (ui_source / "app.js").write_text("release-one")
    (ui_source / "app.css").write_text("body{}")
    def install_ui():
        return subprocess.run([str(BIN), "ui-install", str(ui_source)], env=ENV,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    check(install_ui().returncode == 0, "independent UI release installs without a service restart")
    code, page = asset("/")
    old_asset = re.search(r'src="([^"]+)"', page).group(1)
    check(code == 200 and asset(old_asset) == (200, "release-one"), "HTML pins its JavaScript to one UI release")
    (ui_source / "index.html").write_text('<html>release-two<link href="app.css"><script src="app.js"></script></html>')
    (ui_source / "app.js").write_text("release-two")
    check(install_ui().returncode == 0 and "release-two" in asset("/")[1], "UI updates are visible immediately on refresh")
    check(asset(old_asset) == (200, "release-one"), "a previously loaded page retains access to its original bundle")
    (ui_source / "muster-ui.json").write_text('{"protocol":999}')
    check(install_ui().returncode != 0 and "release-two" in asset("/")[1], "incompatible UI installation preserves the active release")
    check(current.poll() is None and session(sid)["pid"] == restarted["pid"],
          "UI updates preserve backend and terminal processes")

    # Exercise the user-facing reload command after fault tests. The replacement
    # detaches itself, so cleanup for this one uses the CLI with the same private paths.
    old_backend = current
    reload_process = subprocess.Popen([str(BIN), "reload"], env=ENV,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    detached_reload = True
    old_backend.wait(timeout=15)
    current = None
    stdout, stderr = reload_process.communicate(timeout=20)
    check(reload_process.returncode == 0, f"backend-only reload succeeds: {stderr.decode().strip()}")
    discovery = json.loads((RUNTIME / "daemon.json").read_text())
    token = discovery["token"]
    check(discovery["pid"] != old_backend.pid and session(sid)["pid"] == restarted["pid"],
          "reload replaces the backend while preserving the terminal PID")
    check(rpc("hello")["terminal_host"]["instance"] == host_instance,
          "reload confirms the same terminal host identity")
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
    if detached_reload and (RUNTIME / "daemon.json").exists():
        subprocess.run([str(BIN), "stop"], env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=25, check=True)
    if (RUNTIME / "terminal.sock").exists():
        try:
            host_call("shutdown")
            until("fixture host cleanup", lambda: not (RUNTIME / "terminal.sock").exists())
        except Exception as error:
            print(f"Fixture host cleanup needs inspection at {BASE}: {error}", flush=True)
    for log in logs:
        log.close()
    # Preserve private logs for diagnosis; no PID is read from a file and signalled.
