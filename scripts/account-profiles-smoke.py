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
    "STATUSLINE_DIR": str(BASE / "statusline"),
}
(BASE / "statusline").mkdir(mode=0o700)
if "--browser" in sys.argv:
    assert (PROJECT / "web/dist/app.js").is_file(), "deno task web first"
    ENV["MUSTER_WEB_DIR"] = str(PROJECT / "web/dist")

# The stand-in logs only known fixture values, never a real environment or credential. Like
# Claude Code, it runs the status-line command from its --settings file through /bin/sh with a
# JSON payload on stdin: here whenever the test drops <STATUSLINE_DIR>/<session>.json.
(FAKEBIN / "claude").write_text('''#!/usr/bin/python3
import json, os, subprocess, sys, time
row = {"id": int(os.environ["MUSTER_SESSION_ID"]), "pid": os.getpid(),
       "config_dir": os.environ.get("CLAUDE_CONFIG_DIR"), "args": sys.argv[1:]}
fd = os.open(os.environ["PROFILE_LAUNCH_LOG"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, (json.dumps(row) + "\\n").encode())
os.close(fd)
print("FAKE_CLAUDE " + json.dumps(row), flush=True)
args = sys.argv[1:]
settings = args[args.index("--settings") + 1] if "--settings" in args else None
trigger = os.path.join(os.environ["STATUSLINE_DIR"], f"{row['id']}.json")
while True:
    if settings and os.path.exists(trigger):
        data = open(trigger, "rb").read()
        os.unlink(trigger)
        command = json.load(open(settings))["statusLine"]["command"]
        p = subprocess.run(["/bin/sh", "-c", command], input=data, capture_output=True, timeout=10)
        with open(trigger[:-5] + ".out", "a") as out:
            out.write(json.dumps({"stdout": p.stdout.decode(), "code": p.returncode}) + "\\n")
    time.sleep(0.05)
''')
(FAKEBIN / "claude").chmod(0o700)
for name in ("personal", "work", "default", "ui"):
    (BASE / name).mkdir(mode=0o700)
    # Deliberately not an authentication document. Registering and launching must leave it alone.
    (BASE / name / ".credentials.json").write_text("fixture: this is not a credential\n")
# Each profile's own status line, which Muster's reporter must keep showing (exit code included).
(BASE / "personal" / "settings.json").write_text(json.dumps(
    {"statusLine": {"type": "command", "command": "cat > /dev/null; echo HOME-USER-LINE; exit 3"}}))
(BASE / "work" / "settings.json").write_text(json.dumps(
    {"env": {"FIXTURE": "not read"}, "statusLine": {"type": "command", "command": "cat > /dev/null; echo WORK-USER-LINE", "padding": 1}}))
sentinels = {p: (p.read_bytes(), p.stat().st_mtime_ns)
             for p in [*BASE.glob("*/.credentials.json"), *BASE.glob("*/settings.json")]}
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


def status_line(sid, payload):
    """Have session `sid`'s fake Claude run its status line once with `payload`."""
    out = BASE / "statusline" / f"{sid}.out"
    before = len(out.read_text().splitlines()) if out.exists() else 0
    tmp = BASE / "statusline" / f"{sid}.tmp"
    tmp.write_text(json.dumps(payload))
    tmp.rename(BASE / "statusline" / f"{sid}.json")

    def done():
        lines = out.read_text().splitlines() if out.exists() else []
        return json.loads(lines[-1]) if len(lines) > before else None
    return until(f"status line of session {sid}", done)


def usage_row(name):
    return next(r for r in rpc("account_usage.get")["rows"] if r["name"] == name)


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

    # ----- usage display and placement suggestions (docs/ACCOUNT_PROFILES.md steps 2-3) -----
    now = int(time.time())

    def limits(five, seven, api, five_reset=None):
        return {"session_id": "fixture-conversation", "cwd": str(REPO), "transcript_path": "/fixture/t.jsonl",
                "account": "Work", "cost": {"total_api_duration_ms": api, "total_cost_usd": 0},
                "rate_limits": {"five_hour": {"used_percentage": five, "resets_at": five_reset or now + 3 * 3600},
                                "seven_day": {"used_percentage": seven, "resets_at": now + 4 * 86400}}}
    work_line = json.loads((DATA / "sessions" / str(b["id"]) / "claude-settings.json").read_text())["statusLine"]
    check(work_line["padding"] == 1 and "statusline claude" in work_line["command"]
          and "WORK-USER-LINE" in work_line["command"] and "FIXTURE" not in json.dumps(work_line),
          "a session's status line wraps its profile's own one, options kept, nothing else copied")
    check(rpc("account_usage.get")["no_suggestion"] and usage_row("Work")["five_hour"]["status"] == "unknown",
          "before any report every account is unknown and nothing is suggested")
    check(status_line(b["id"], limits(25, 40, 100)) == {"stdout": "WORK-USER-LINE\n", "code": 0},
          "the user's status line output is shown unchanged")
    check(status_line(a["id"], limits(92, 70, 100)) == {"stdout": "HOME-USER-LINE\n", "code": 3},
          "the user's status line exit code passes through")
    check(status_line(legacy["id"], limits(50, 50, 10, five_reset=now + 2)) == {"stdout": "", "code": 0},
          "without a status line of its own a session shows an empty one")
    work_row, home_row = usage_row("Work"), usage_row("Home")
    check(work_row["five_hour"]["used_percentage"] == 25 and work_row["five_hour"]["status"] == "current"
          and home_row["seven_day"]["used_percentage"] == 70 and usage_row("Default")["seven_day"]["status"] == "current",
          "reports are attributed to each session's pinned profile, not a label in the payload")
    usage = rpc("account_usage.get")
    check(usage["suggestion"]["name"] == "Work" and "Work used 25%" in usage["suggestion"]["reason"],
          "the suggestion names the account with the most known room and why")
    seen = home_row["five_hour"]["observed_at"]
    status_line(a["id"], limits(92, 70, 100))
    check(usage_row("Home")["five_hour"]["observed_at"] == seen, "a redraw of the same numbers does not renew freshness")
    check(http("/usage/claude", limits(1, 1, 1))[0] == 401 and http("/usage/claude", limits(1, 1, 1), "forged")[0] == 401,
          "only a session token can report usage")
    cli("profile", "login-changed", "Home")
    check(usage_row("Home")["five_hour"]["status"] == "unknown", "a login change forgets the profile's usage")
    status_line(a["id"], limits(92, 70, 100))
    check(usage_row("Home")["five_hour"]["status"] == "unconfirmed",
          "an old redraw seen afterwards is kept as unconfirmed, not as fresh")
    until("Default's 5-hour window to reset", lambda: usage_row("Default")["five_hour"]["status"] == "reset")
    check(True, "a passed reset reads as unknown, not as zero")
    text = cli("profile", "usage")
    check("Suggested for a new task: Work" in text and "unknown (reset" in text and "not confirmed fresh" in text,
          "muster profile usage prints the table and the suggestion")
    cli("profile", "same-account", "Home", "Work")
    home_row = usage_row("Home")
    check(home_row["same_account_as"] == ["Work"] and home_row["five_hour"]["reported_by"] == "Work"
          and rpc("account_usage.get")["suggestion"]["choices"] == [personal, work],
          "profiles declared to be one account share one set of numbers")
    cli("profile", "same-account", "Home")
    check(usage_row("Home")["same_account_as"] == [], "a profile can be its own account again")
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
