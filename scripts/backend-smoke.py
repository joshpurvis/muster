#!/usr/bin/env python3
"""Backend/host fault tests. Only fixture-owned processes and private temp paths.
Run: bash scripts/isolated.sh python3 scripts/backend-smoke.py
Add --browser for rendered desktop/mobile recovery checks (requires Chromium and web/dist).
Requires cargo build --workspace first. No installed Muster executable is used.
"""
from http.client import HTTPConnection
import fcntl
import json
import re
import selectors
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
# A stand-in `claude` so a named agent can be started without the real CLI.
FAKEBIN = BASE / "fakebin"
FAKEBIN.mkdir(mode=0o700)
(FAKEBIN / "claude").write_text("#!/bin/sh\nexec sleep 600\n")
(FAKEBIN / "claude").chmod(0o700)
ENV = {"PATH": f"{FAKEBIN}:/usr/bin:/bin", "HOME": str(HOME), "SHELL": "/bin/bash", "LANG": "C.UTF-8",
       "MUSTER_RUNTIME_DIR": str(RUNTIME), "MUSTER_DATA_DIR": str(DATA), "MUSTER_PORT": str(PORT)}
current = None
logs = []
token = None
passed = 0
detached_reload = False
browser = None


def browser_phase(phase=None):
    if browser is None:
        return None
    if phase is not None:
        browser.stdin.write(json.dumps({"phase": phase}) + "\n")
        browser.stdin.flush()
    with selectors.DefaultSelector() as selector:
        selector.register(browser.stdout, selectors.EVENT_READ)
        assert selector.select(timeout=35), f"browser phase timed out: {phase}; inspect {BASE / 'browser.log'}"
    line = browser.stdout.readline()
    assert line, f"browser exited during {phase}; inspect {BASE / 'browser.log'}"
    result = json.loads(line)
    assert result["phase"] == (phase or "ready"), result
    return result


def close_browser():
    global browser
    if browser is not None:
        if browser.poll() is None and not browser.stdin.closed:
            browser.stdin.write('{"phase":"close"}\n')
            browser.stdin.flush()
        # EOF also releases the driver's stream read, which can otherwise keep
        # Deno alive after Chromium itself has already exited.
        if not browser.stdin.closed:
            browser.stdin.close()
        try:
            browser.wait(timeout=35)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"browser cleanup timed out; inspect {BASE / 'browser.log'}") from None
        browser.stdout.close()
        assert browser.returncode == 0, f"browser recovery failed; inspect {BASE / 'browser.log'}"
        browser = None


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


def start_browser(sid):
    global browser
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        browser_port = reserve.getsockname()[1]
    browser_log = open(BASE / "browser.log", "a")
    logs.append(browser_log)
    browser = subprocess.Popen([shutil.which("deno"), "run", "--allow-net=127.0.0.1",
        "--allow-run=chromium", f"--allow-write={BASE / 'shots'}",
        str(PROJECT / "web/dev/recovery.ts"), f"http://127.0.0.1:{PORT}/#token={token}",
        str(sid), str(BASE), str(browser_port)], env=ENV | {"DENO_DIR": str(BASE / "deno")},
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=browser_log, text=True)
    browser_phase()


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
    if "--browser" in sys.argv:
        start_browser(sid)
    rpc("session.input", {"id": sid, "data": "for i in $(seq 1 80); do printf 'COUNT:%s\\n' \"$i\"; sleep 0.1; done\r"})
    until("ticker started", lambda: "COUNT:2" in output(sid))
    browser_phase("before")
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
    if browser is not None:
        check(browser_phase("after")["pid"] == pid,
              "rendered UI reconnects, replays downtime output and accepts keyboard input with the same PID")
    with sqlite3.connect(DATA / "muster.db") as database:
        record = json.loads(database.execute("SELECT record FROM terminal_runs WHERE session_id=?", (sid,)).fetchone()[0])
    check(http("/hook/Stop", {"hook_event_name": "Stop"}, record["token"])[0] == 200,
          "session hook credentials survive backend restart")
    browser_phase("mobile")
    stop_backend()
    if browser is not None:
        host_call("input", {"run": record["spec"]["run"], "data": list(b"printf 'MOBILE-DOWNTIME-OK\\n'\r")})
        browser_phase("mobile_offline")
    check(next(item for item in host_call("inventory") if item["run"]["id"] == sid)["alive"],
          "graceful backend shutdown preserves the terminal")
    def legacy_daemon_lock_free():
        # What an older, monolithic daemon takes to start (released immediately here).
        with open(RUNTIME / "daemon.lock", "a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(lock, fcntl.LOCK_UN)
            return True
    check(not legacy_daemon_lock_free(), "a live terminal host keeps older daemons from starting beside it")
    hook_env = ENV | {"MUSTER_URL": f"http://127.0.0.1:{PORT}", "MUSTER_TOKEN": record["token"],
                      "MUSTER_HOOK_DB": str(DATA / "hook-inbox.sqlite")}
    callback = subprocess.run([str(BIN), "hook", "shell", "SmokeDurableCallback", '{"saved":"during outage"}'],
                              env=hook_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
    check(callback.returncode == 0 and not callback.stdout and not callback.stderr,
          "hook helper durably accepts a callback while the backend is absent")
    callback = subprocess.run([str(BIN), "hook", "shell", "PreToolUse", '{"tool_name":"Agent"}'],
                              env=hook_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
    assert callback.returncode == 0, callback.stderr
    with sqlite3.connect(DATA / "hook-inbox.sqlite") as database:
        saved_callback = database.execute("SELECT id,token,event,payload FROM hooks ORDER BY id DESC LIMIT 1").fetchone()
    assert saved_callback is not None
    def callback_count():
        with sqlite3.connect(DATA / "muster.db") as database:
            return database.execute("SELECT COUNT(*) FROM events WHERE kind='hook' AND json_extract(data,'$.event')='SmokeDurableCallback'").fetchone()[0]
    start_backend()
    check(callback_count() == 1, "replacement backend drains the durable callback before serving clients")
    check(session(sid)["subagents"] == 1, "replacement backend restores a queued subagent-start callback")
    if browser is not None:
        check(browser_phase("mobile_after")["pid"] == pid,
              "mobile UI preserves pairing and draft, disables offline send and replays downtime output")
        close_browser()
    # Simulate an acknowledgement lost after application commit: restore the same inbox row.
    stop_backend(hard=True)
    with sqlite3.connect(DATA / "hook-inbox.sqlite") as database:
        database.execute("INSERT INTO hooks(id,token,event,payload) VALUES (?,?,?,?)", saved_callback)
    start_backend()
    check(callback_count() == 1, "a callback with a committed checkpoint is not applied twice")
    check(session(sid)["subagents"] == 1, "subagent count survives backend replacement without duplicate increments")
    dying = rpc("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
    rpc("session.input", {"id": dying["id"], "data": "sleep 0.4; exit 7\r"})
    stop_backend(hard=True)
    time.sleep(0.7)
    start_backend()
    check(session(dying["id"])["exit_code"] == 7 and session(dying["id"])["state"] == "exited",
          "exit during backend outage is reconciled with its actual status")
    restarted = rpc("session.restart", {"id": sid})
    check(restarted["pid"] != pid and restarted["state"] != "exited", "explicit session restart creates one new process")
    check(restarted["subagents"] == 0, "a new terminal generation discards the previous run's subagent count")
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

    def call_through_gate(method, params):
        result = []
        def request():
            try:
                result.append(http("/api/rpc", {"id": 1, "method": method, "params": params}, timeout=25))
            except (ConnectionError, OSError) as error:
                result.append(error)
        sender = threading.Thread(target=request)
        sender.start()
        return sender, result

    def launch_record(session_id):
        with sqlite3.connect(DATA / "muster.db") as database:
            row = database.execute("SELECT record FROM terminal_runs WHERE session_id=?", (session_id,)).fetchone()
        return json.loads(row[0]) if row else None

    def crash_before_acknowledgement(method, params):
        """Hold the Spawn response, SIGKILL the backend, then start a replacement."""
        with SpawnGate(RUNTIME, "after") as gate:
            sender, _ = call_through_gate(method, params)
            try:
                assert gate.entered.wait(10), f"spawn acknowledgement was not captured: {gate.errors}"
                assert gate.response["type"] == "status", gate.response
                held = (gate.request["run"], gate.response["data"]["pid"])
                stop_backend(hard=True)
            finally:
                gate.release.set()
                sender.join(timeout=30)
        start_backend()
        return held

    # A callback from a launch whose acknowledgement is still outstanding must not hold
    # up other sessions' callbacks or keystrokes.
    with SpawnGate(RUNTIME, "after") as gate:
        sender, result = call_through_gate("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
        try:
            assert gate.entered.wait(10), f"spawn acknowledgement was not captured: {gate.errors}"
            held_run = gate.request["run"]
            pending = launch_record(held_run["id"])
            assert pending and pending["pending"], pending
            for hook_token, event, payload in ((pending["token"], "PreToolUse", '{"tool_name":"Agent"}'),
                                               (launch_record(sid)["token"], "SmokeDurableCallback", "{}")):
                callback = subprocess.run([str(BIN), "hook", "shell", event, payload],
                    env=hook_env | {"MUSTER_TOKEN": hook_token}, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=4)
                assert callback.returncode == 0, callback.stderr
            until("other session's callback processed during the held launch", lambda: callback_count() == 2)
            rpc("session.input", {"id": sid, "data": "printf 'INPUT-DURING-PENDING\\n'\r"})
            until("keystrokes reach another terminal", lambda: "INPUT-DURING-PENDING" in output(sid))
            check(True, "an unacknowledged launch's callback blocks neither other callbacks nor keystrokes")
        finally:
            gate.release.set()
            sender.join(timeout=30)
    assert len(result) == 1 and isinstance(result[0], tuple) and "result" in result[0][1], result
    check(session(held_run["id"])["subagents"] == 1,
          "a callback applied before the spawn acknowledgement is not reset by it")
    rpc("session.remove", {"id": held_run["id"]})

    # Restart: the backend dies after the host starts the replacement but before the
    # acknowledgement. The survivor must come back running, not marked exited.
    restarting = rpc("session.spawn", {"channel_id": channel["id"], "kind": "shell"})
    until("restart fixture running", lambda: session(restarting["id"])["state"] != "starting")
    run, new_pid = crash_before_acknowledgement("session.restart", {"id": restarting["id"]})
    check(session(restarting["id"])["pid"] == new_pid and session(restarting["id"])["state"] != "exited"
          and sum(item["run"] == run for item in host_call("inventory")) == 1,
          "a restart interrupted before its acknowledgement recovers the live replacement")
    rpc("session.input", {"id": restarting["id"], "data": "printf 'RESTART-SURVIVOR\\n'\r"})
    until("recovered replacement accepts input", lambda: "RESTART-SURVIVOR" in output(restarting["id"]))
    rpc("session.remove", {"id": restarting["id"]})

    # A shell's initial command is saved before its launch, so a crash before the
    # acknowledgement neither loses it nor runs it twice.
    initial = BASE / "initial-command-runs"
    run, _ = crash_before_acknowledgement("session.spawn", {"channel_id": channel["id"], "kind": "shell",
        "prompt": f"printf 'once\\n' >> {initial}"})
    until("initial command delivered after recovery", lambda: initial.exists() and initial.read_text() == "once\n")
    stop_backend(hard=True)
    start_backend()
    time.sleep(1.5)
    check(initial.read_text() == "once\n", "a shell's initial command survives a crash before acknowledgement, once")
    rpc("session.remove", {"id": run["id"]})

    # A named agent is linked to its session before the process starts.
    rpc("agent.create", {"name": "smoke-agent", "kind": "claude"})
    run, agent_pid = crash_before_acknowledgement("agent.start", {"name": "smoke-agent"})
    agent = next(item for item in rpc("agent.list") if item["name"] == "smoke-agent")
    check(agent["session_id"] == run["id"], "a named agent keeps its session across a crash before acknowledgement")
    again = rpc("agent.start", {"name": "smoke-agent"})
    agent_runs = [item for item in host_call("inventory") if item["alive"] and item["run"]["id"] == run["id"]]
    check(not again["started"] and again["session"]["id"] == run["id"] and len(agent_runs) == 1
          and agent_runs[0]["pid"] == agent_pid, "starting the agent again reuses the survivor instead of a duplicate")
    rpc("session.remove", {"id": run["id"]})
    rpc("agent.remove", {"name": "smoke-agent"})

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
    def interrupted_delivery(name):
        uncertain = BASE / f"{name}-uncertain-runs"
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
        return uncertain

    uncertain = interrupted_delivery("first")
    review = rpc("session.queue", {"id": sid})
    check(review["paused"] and review["uncertain"] and review["stage"] == "typing",
          "interrupted prompt delivery is retained for review")
    before = uncertain.read_text() if uncertain.exists() else ""
    time.sleep(1.5)
    check((uncertain.read_text() if uncertain.exists() else "") == before,
          "ambiguous input is never automatically replayed")
    check(sid in rpc("hello")["paused_deliveries"], "reconnecting UI is told about paused delivery")
    resumed = BASE / "resumed-after-review"
    for text in ("kept", "kept-too"):
        rpc("session.send", {"id": sid, "text": f"printf '{text}\\n' >> {resumed}"})
    if browser is None and "--browser" in sys.argv:
        start_browser(sid)
    if browser is not None:
        # The desktop review dialog's Resume button, clicked in the rendered UI.
        browser_phase("review_desktop")
    else:
        rpc("session.resume_queue", {"id": sid})
    review = rpc("session.queue", {"id": sid})
    check(not review["paused"] and not review["uncertain"], "resuming forgets only the reviewed uncertain record")
    until("waiting prompts delivered after review", lambda: resumed.exists() and resumed.read_text() == "kept\nkept-too\n")
    check(resumed.read_text() == "kept\nkept-too\n", "every waiting prompt survives the resume and is delivered once")
    check((uncertain.read_text() if uncertain.exists() else "") == before,
          "resuming never resends the uncertain prompt")
    if browser is not None:
        # The same through the mobile composer's Resume button.
        uncertain = interrupted_delivery("mobile")
        before = uncertain.read_text() if uncertain.exists() else ""
        resumed = BASE / "resumed-from-mobile"
        for text in ("phone-kept", "phone-kept-too"):
            rpc("session.send", {"id": sid, "text": f"printf '{text}\\n' >> {resumed}"})
        assert rpc("session.queue", {"id": sid})["paused"]
        browser_phase("review_mobile")
        until("waiting prompts delivered after mobile review",
              lambda: resumed.exists() and resumed.read_text() == "phone-kept\nphone-kept-too\n")
        check(not rpc("session.queue", {"id": sid})["paused"]
              and (uncertain.read_text() if uncertain.exists() else "") == before,
              "mobile Resume keeps the waiting prompts and never resends the uncertain one")
        close_browser()
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
    check(until("host released the fence lock", legacy_daemon_lock_free), "after a full stop an older daemon may start")
    # A legacy (monolithic) daemon holding daemon.lock, without a usable daemon.json: this
    # backend must refuse before opening the shared database, and leave no host behind.
    (RUNTIME / "daemon.json").unlink(missing_ok=True)
    with open(RUNTIME / "daemon.lock", "a") as legacy_lock:
        fcntl.flock(legacy_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        log = open(BASE / "backend-legacy-lock.log", "wb")
        logs.append(log)
        refused = subprocess.Popen([str(BIN), "daemon", "--foreground"], env=ENV,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            refused.wait(timeout=20)
        finally:
            if refused.poll() is None:
                refused.kill()
                refused.wait()
        log.flush()
        message = (BASE / "backend-legacy-lock.log").read_text()
        check(refused.returncode != 0 and "older Muster daemon" in message
              and not (RUNTIME / "terminal.sock").exists() and not (RUNTIME / "daemon.json").exists(),
              "a backend refuses to start beside a legacy daemon holding daemon.lock")
    print(f"Passed {passed} backend survival checks.", flush=True)
finally:
    try:
        close_browser()
    except Exception as error:
        print(f"Browser fixture cleanup needs inspection at {BASE}: {error}", flush=True)
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
