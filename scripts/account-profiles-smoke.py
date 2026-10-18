#!/usr/bin/env python3
"""Local profiles through the real backend/host, with a fake Claude executable.

Run after cargo build --workspace:
  bash scripts/isolated.sh python3 scripts/account-profiles-smoke.py [--browser]
Never uses the installed executable, real HOME, login files, or running daemon.
"""
from http.client import HTTPConnection
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

PROJECT = Path(__file__).resolve().parents[1]
BIN = PROJECT / "target/debug/muster"
assert BIN.is_file() and BIN.with_name("muster-terminal-host").is_file(), "cargo build --workspace first"
BASE = Path(tempfile.mkdtemp(prefix="muster-profiles-smoke-"))
RUNTIME, DATA, FIXTURE_HOME, FAKEBIN, REPO = (BASE / name for name in ("run", "data", "home", "bin", "repo"))
for directory in (RUNTIME, DATA, FIXTURE_HOME, FAKEBIN, REPO):
    directory.mkdir(mode=0o700)
with socket.socket() as reserve:
    reserve.bind(("127.0.0.1", 0))
    PORT = reserve.getsockname()[1]
ENV = {
    "PATH": f"{FAKEBIN}:/usr/bin:/bin", "HOME": str(FIXTURE_HOME), "SHELL": "/bin/bash", "LANG": "C.UTF-8",
    "MUSTER_RUNTIME_DIR": str(RUNTIME), "MUSTER_DATA_DIR": str(DATA), "MUSTER_PORT": str(PORT),
    "CLAUDE_CONFIG_DIR": str(BASE / "default"), "PROFILE_LAUNCH_LOG": str(BASE / "launches.jsonl"),
}
if "--browser" in sys.argv:
    assert (PROJECT / "web/dist/app.js").is_file(), "deno task web first"
    ENV["MUSTER_WEB_DIR"] = str(PROJECT / "web/dist")

# The stand-in logs only known fixture values, never a real environment or credential.
(FAKEBIN / "claude").write_text('''#!/usr/bin/python3
import json, os, sys, time
row = {"id": int(os.environ["MUSTER_SESSION_ID"]), "pid": os.getpid(),
       "config_dir": os.environ.get("CLAUDE_CONFIG_DIR"), "args": sys.argv[1:]}
fd = os.open(os.environ["PROFILE_LAUNCH_LOG"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, (json.dumps(row) + "\\n").encode())
os.close(fd)
print("FAKE_CLAUDE " + json.dumps(row), flush=True)
while True: time.sleep(60)
''')
(FAKEBIN / "claude").chmod(0o700)
for name in ("personal", "work", "default", "ui"):
    (BASE / name).mkdir(mode=0o700)
    # Deliberately not an authentication document. Registering and launching must leave it alone.
    (BASE / name / ".credentials.json").write_text("fixture: this is not a credential\n")
sentinels = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in BASE.glob("*/.credentials.json")}
current, token = None, None
logs = []
checks = 0


def check(condition, message):
    global checks
    assert condition, message
    checks += 1
    print(f"  ok {message}", flush=True)


def until(message, predicate):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out: {message}; logs: {BASE}")


def http(path, body=None, credential=None):
    conn = HTTPConnection("127.0.0.1", PORT, timeout=10)
    try:
        conn.request("POST" if body is not None else "GET", path, json.dumps(body) if body is not None else None,
                     {"Authorization": f"Bearer {credential or token}", "Content-Type": "application/json"})
        response = conn.getresponse()
        raw = response.read()
        return response.status, json.loads(raw) if raw else None
    finally:
        conn.close()


def rpc(method, params=None, error=None):
    status, response = http("/api/rpc", {"id": 1, "method": method, "params": params or {}})
    assert status == 200, (method, response)
    if error:
        assert error in json.dumps(response.get("error")), response
        return response
    assert "error" not in response, (method, response)
    return response["result"]


def cli(*args):
    return subprocess.run([str(BIN), *args], env=ENV, cwd=REPO, text=True,
                          capture_output=True, check=True, timeout=20).stdout


def start():
    global current, token
    log = open(BASE / f"backend-{len(logs)}.log", "wb")
    logs.append(log)
    current = subprocess.Popen([str(BIN), "daemon", "--foreground"], env=ENV, cwd=REPO,
                               stdin=subprocess.DEVNULL, stdout=log, stderr=log)

    def ready():
        global token
        assert current.poll() is None, f"backend exited: {BASE}"
        try:
            discovery = json.loads((RUNTIME / "daemon.json").read_text())
            if discovery["pid"] != current.pid:
                return False
            token = discovery["token"]
            return http("/api/health")[0] == 200
        except (OSError, ValueError):
            return False
    until("fixture backend startup", ready)


def session(sid):
    return next(s for s in rpc("hello")["sessions"] if s["id"] == sid)


def launched(sid, pid=None):
    def find():
        try:
            rows = [json.loads(line) for line in (BASE / "launches.jsonl").read_text().splitlines()]
        except FileNotFoundError:
            return None
        return next((r for r in reversed(rows) if r["id"] == sid and (pid is None or r["pid"] == pid)), None)
    return until(f"fake Claude launch {sid}", find)


def spawn(channel, **params):
    result = rpc("session.spawn", {"channel_id": channel, "kind": "claude", **params})
    launched(result["id"], result["pid"])
    return result


print(f"Isolated profile smoke files: {BASE}", flush=True)
try:
    start()
    cli("profile", "add", "Personal", str(BASE / "personal"))
    cli("profile", "add", "Work", str(BASE / "work"))
    profiles = json.loads(cli("profile", "ls", "--json"))
    personal = next(p["id"] for p in profiles if p["name"] == "Personal")
    work = next(p["id"] for p in profiles if p["name"] == "Work")
    check(len(profiles) == 2, "CLI registers and lists directory references")
    alias = BASE / "alias"
    alias.symlink_to(BASE / "personal", target_is_directory=True)
    rpc("account_profile.create", {"name": "Duplicate", "config_dir": str(alias)}, error="already registered")
    rpc("account_profile.update", {"id": personal, "config_dir": str(BASE / "work")}, error="unknown field")
    check(True, "directory aliases cannot create duplicates or repoint a profile")
    ws = rpc("workspace.add", {"path": str(REPO)})
    ch = rpc("channel.create", {"workspace_id": ws["id"], "name": "profiles"})["id"]
    a, b, legacy = spawn(ch, account_profile=personal), spawn(ch, account_profile=work), spawn(ch)
    check(launched(a["id"])["config_dir"] == str(BASE / "personal") and launched(b["id"])["config_dir"] == str(BASE / "work"),
          "concurrent Claude processes receive different profile directories")
    check(launched(legacy["id"])["config_dir"] == str(BASE / "default") and legacy["account_profile_id"] is None,
          "legacy/default launch preserves ordinary inherited environment")
    cli("profile", "default", str(REPO), "Work")
    inherited = spawn(ch, parent_id=a["id"])
    default_child = spawn(ch, parent_id=legacy["id"])
    check(inherited["account_profile_id"] == personal and default_child["account_profile_id"] is None,
          "children inherit pinned selection, including Default, before workspace preferences")
    selected = json.loads(cli("new", str(REPO), "--profile", "Personal", "--json"))["session"]
    check(selected["account_profile_id"] == personal, "CLI explicit selection overrides workspace default")
    default = spawn(ch, account_profile="default")
    check(default["account_profile_id"] is None and spawn(ch)["account_profile_id"] == work,
          "explicit Default differs from omitted selection")
    cli("profile", "rename", "Personal", "Home")
    cli("profile", "disable", "Home")
    before_pid = session(a["id"])["pid"]
    rpc("session.restart", {"id": a["id"]}, error="disabled")
    check(session(a["id"])["pid"] == before_pid, "failed restart preflight leaves the original run alive")
    rpc("session.spawn", {"channel_id": ch, "kind": "claude", "account_profile": personal}, error="disabled")
    cli("profile", "enable", "Home")
    restarted = rpc("session.restart", {"id": a["id"]})
    check(launched(a["id"], restarted["pid"])["config_dir"] == str(BASE / "personal"),
          "restart retains profile despite rename and changed workspace default")
    rpc("session.spawn", {"channel_id": ch, "kind": "shell", "account_profile": personal}, error="only supported for Claude")
    cli("agent", "new", "reviewer", "--profile", "Home")
    named = rpc("agent.start", {"name": "reviewer"})["session"]
    check(named["account_profile_id"] == personal, "named-agent creation pins the selected profile")
    # Feed only the existing Muster hook protocol; this is not a provider authentication file.
    mcp = json.loads((DATA / "sessions" / str(named["id"]) / "claude-mcp.json").read_text())
    run_token = mcp["mcpServers"]["muster"]["headers"]["Authorization"].removeprefix("Bearer ")
    status, _ = http("/hook/SessionStart", {"session_id": "fixture-conversation"}, run_token)
    assert status == 200
    resumed = rpc("session.restart", {"id": named["id"]})
    named_run = launched(named["id"], resumed["pid"])
    check(named_run["config_dir"] == str(BASE / "personal") and "fixture-conversation" in named_run["args"],
          "named-agent resume keeps its conversation and profile together")
    cli("profile", "disable", "Home")
    check(rpc("agent.start", {"name": "reviewer"})["started"] is False,
          "disabled profile does not block reuse of a running named agent")
    cli("profile", "enable", "Home")
    old_pids = {s["id"]: s["pid"] for s in rpc("hello")["sessions"]}
    current.terminate()
    current.wait(timeout=10)
    start()
    check(all(session(sid)["pid"] == pid for sid, pid in old_pids.items()), "isolated backend replacement preserves all fixture terminals")
    check(session(a["id"])["account_profile_id"] == personal and session(named["id"])["account_profile_id"] == personal,
          "profile bindings survive backend recovery")
    rpc("session.remove", {"id": named["id"]})
    replacement = rpc("agent.start", {"name": "reviewer"})["session"]
    check(replacement["account_profile_id"] == personal and "fixture-conversation" in launched(replacement["id"])["args"],
          "named-agent replacement session retains profile and resume ID")
    if "--browser" in sys.argv:
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            browser_port = reserve.getsockname()[1]
        subprocess.run([shutil.which("deno"), "run", "--allow-net=127.0.0.1", "--allow-run=chromium",
                        f"--allow-write={BASE}", str(PROJECT / "web/dev/account_profiles.ts"),
                        f"http://127.0.0.1:{PORT}/#token={token}", str(BASE), str(browser_port)],
                       env=ENV, check=True, timeout=90)
        check(True, "rendered profile management and new-session selection")
    check(all((p.read_bytes(), p.stat().st_mtime_ns) == before for p, before in sentinels.items()),
          "all fixture authentication files remain byte-for-byte and timestamp unchanged")
    print(f"Passed {checks} profile integration checks.", flush=True)
finally:
    # This discovery lives exclusively in this test's fresh runtime directory.
    if current is not None and current.poll() is None:
        try:
            rpc("daemon.shutdown", {})
            current.wait(timeout=15)
        finally:
            if current.poll() is None:
                current.terminate()
                current.wait(timeout=10)
    for log in logs:
        log.close()
