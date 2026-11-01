#!/usr/bin/env python3
"""Real user-manager isolation, against one private daemon with fake agents only.
Run after cargo build --workspace: python3 scripts/isolation-smoke.py
Every Muster CLI call has private runtime/data paths and port 0. No real daemon is used.
"""
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/muster"
BASE = Path(tempfile.mkdtemp(prefix="muster-isolation-"))
print(f"Isolation smoke files: {BASE}", flush=True)
for name in ("run", "data", "home", "bin"):
    (BASE / name).mkdir(mode=0o700)
# The entire session process tree is disposable. No credentials or real agent CLI.
(BASE / "bin/claude").write_text("#!/bin/sh\nexec sleep 600\n")
(BASE / "bin/claude").chmod(0o700)
ENV = {"PATH": f"{BASE / 'bin'}:/usr/bin:/bin", "HOME": str(BASE / "home"),
       "SHELL": "/bin/bash", "LANG": "C.UTF-8", "MUSTER_PORT": "0",
       "MUSTER_RUNTIME_DIR": str(BASE / "run"), "MUSTER_DATA_DIR": str(BASE / "data"),
       "MUSTER_ISOLATION": "required", "MUSTER_TEST_SENTINEL": "literal $HOME %n newline\nvalue"}
for key in ("XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"):
    if key in os.environ:
        ENV[key] = os.environ[key]


def cli(*args, **kwargs):
    return subprocess.run([str(BIN), *args], env=ENV, text=True, capture_output=True, timeout=35, **kwargs)


def discovery():
    return json.loads((BASE / "run/daemon.json").read_text())


def rpc(method, params=None):
    d = discovery()
    conn = HTTPConnection("127.0.0.1", d["port"], timeout=10)
    try:
        conn.request("POST", "/api/rpc", json.dumps({"id": 1, "method": method, "params": params or {}}),
                     {"Authorization": f"Bearer {d['token']}", "Content-Type": "application/json"})
        response = conn.getresponse()
        data = json.loads(response.read())
        assert response.status == 200 and "error" not in data, data
        return data["result"]
    finally:
        conn.close()


def until(fn, why):
    end = time.monotonic() + 15
    while time.monotonic() < end:
        result = fn()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(why)


def cgroup(pid):
    return Path(f"/proc/{pid}/cgroup").read_text().strip().removeprefix("0::")


def check(value, why):
    assert value, why
    print(f"  ok {why}", flush=True)


try:
    started = cli("daemon")
    check(started.returncode == 0, f"private backend started: {started.stderr}")
    hello = rpc("hello")
    backend = discovery()["pid"]
    host = hello["terminal_host"]["pid"]
    backend_group, host_group = cgroup(backend), cgroup(host)
    check("muster-core.slice/muster-backend-" in backend_group and backend_group.endswith(".service"), "backend is a dedicated user service")
    check("muster-core.slice/muster-terminal-host-" in host_group and host_group.endswith(".service"), "terminal host is a separate user service")
    check("kitty" not in backend_group + host_group, "neither durable process belongs to Kitty")
    # Only inspect the fixture's harmless sentinel; never dump process environments.
    environment = dict(item.split(b"=", 1) for item in Path(f"/proc/{backend}/environ").read_bytes().split(b"\0") if b"=" in item)
    check(environment[b"MUSTER_TEST_SENTINEL"].decode() == ENV["MUSTER_TEST_SENTINEL"], "service preserves literal environment bytes")
    unit = backend_group.rsplit("/", 1)[-1]
    properties = subprocess.run(["systemctl", "--user", "show", unit, "--property=Environment", "--property=ExecStart"],
                                env=ENV, capture_output=True, text=True, check=True).stdout
    check("MUSTER_TEST_SENTINEL" not in properties and "literal $HOME" not in properties, "environment values are absent from unit properties and argv")
    workspace = rpc("workspace.add", {"path": str(BASE)})
    channel = rpc("channel.create", {"workspace_id": workspace["id"], "name": "Isolation", "agent": "shell"})
    first = rpc("hello")["sessions"][0]
    second = rpc("session.spawn", {"channel_id": channel["id"], "kind": "claude"})
    for session in (first, second):
        until(lambda: "muster-sessions.slice/muster-session-" in cgroup(session["pid"]), "session entered its own scope")
    groups = [cgroup(s["pid"]) for s in (first, second)]
    check(groups[0] != groups[1] and all(g.endswith(".scope") for g in groups), "sessions have independent scopes")

    reloaded = cli("reload")
    check(reloaded.returncode == 0, f"reload succeeds under systemd: {reloaded.stderr}")
    check(discovery()["pid"] != backend and rpc("hello")["terminal_host"]["pid"] == host, "reload replaces only the backend")
    check(all(cgroup(s["pid"]) == g for s, g in zip((first, second), groups)), "reload preserves session processes and scopes")

    # Exercise the backend-spawned helper, which must survive its parent service stopping.
    backend = discovery()["pid"]
    rpc("daemon.reload_handoff", {"exe": str(BIN)})
    until(lambda: (BASE / "run/daemon.json").exists() and discovery()["pid"] != backend, "handoff publishes a replacement")
    check(rpc("hello")["terminal_host"]["pid"] == host, "reload handoff survives the old service ending")

    # Local cgroup OOM only: at most 64 MiB RAM, no swap; never pressure the whole machine.
    limited_env = {**ENV, "MUSTER_JOB_ISOLATION": "required", "MUSTER_JOB_MEMORY_HIGH": "32M",
                   "MUSTER_JOB_MEMORY_MAX": "64M", "MUSTER_JOB_MEMORY_SWAP_MAX": "0"}
    oom = subprocess.run(["bash", str(ROOT / "scripts/job.sh"), "browser", "python3", "-c",
                          "x = bytearray(128 * 1024 * 1024); print(len(x))"], env=limited_env,
                         capture_output=True, text=True, timeout=25)
    check(oom.returncode != 0, "over-budget disposable job is stopped within its own scope")
    check(rpc("hello")["terminal_host"]["pid"] == host and all(Path(f"/proc/{s['pid']}").exists() for s in (first, second)),
          "backend, terminal host and both sessions survive the job's OOM")

    # Killing one scope is another independent failure boundary (only our fake agent).
    scope = groups[1].rsplit("/", 1)[-1]
    subprocess.run(["systemctl", "--user", "kill", "--signal=KILL", scope], env=ENV, check=True)
    until(lambda: next(s for s in rpc("hello")["sessions"] if s["id"] == second["id"])["state"] == "exited", "isolated session exit reported")
    check(Path(f"/proc/{first['pid']}").exists() and rpc("hello")["terminal_host"]["pid"] == host, "one session's failure leaves the other session and host running")
finally:
    stopped = cli("stop")
    if stopped.returncode:
        print(stopped.stderr)
    assert stopped.returncode == 0, "private fixture cleanup failed"

print("Isolation smoke passed", flush=True)
