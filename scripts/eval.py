#!/usr/bin/env python3
"""Repeatable, local-only Muster evals and baseline comparisons. No real agents or credentials.

deno task eval --output /tmp/muster-baseline.json
deno task eval:compare /tmp/muster-baseline.json /tmp/muster-candidate.json
"""
import argparse
import hashlib
from http.client import HTTPConnection
import json
import math
import os
from pathlib import Path
import platform
import selectors
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def stats(values):
    ordered = sorted(values)
    return {"n": len(values), "p50": ordered[math.ceil(len(values)*.5)-1] if values else None,
            "p95": ordered[math.ceil(len(values)*.95)-1] if values else None,
            "max": max(values) if values else None, "samples": values}


def compare(paths):
    baseline, candidate = [json.loads(Path(p).read_text()) for p in paths]
    keys = ["fixture", "renderer", "browser", "build_mode", "machine", "sessions", "keys", "repeats"]
    mismatch = [k for k in keys if baseline.get(k) != candidate.get(k)]
    if mismatch:
        raise SystemExit("Incomparable runs (different " + ", ".join(mismatch) + ")")
    if not baseline.get("ok") or not candidate.get("ok"):
        raise SystemExit("Cannot compare performance: a run failed correctness checks")
    print(f"Baseline {baseline['backend'][:12]} → candidate {candidate['backend'][:12]}")
    print(f"Collection: {baseline['collection']} → {candidate['collection']}")
    old = {r["name"]: r for r in baseline["results"]}
    if set(old) != {r["name"] for r in candidate["results"]}:
        raise SystemExit("Incomparable scenario sets")
    for result in candidate["results"]:
        before = old[result["name"]]
        a, b = before["p95"], result["p95"]
        delta = f"{(b/a-1)*100:+.1f}%" if a is not None and a > 0 and b is not None else "n/a"
        def number(value):
            if value is None:
                return "n/a"
            return f"{value:,.2f}" if result["unit"] == "ms" else f"{value:,.0f}"
        sparse = " [few samples]" if min(before["n"], result["n"]) < 20 else ""
        print(f"{result['name']}: p95 {number(a)} → {number(b)} {result['unit']} ({delta}); n={before['n']}/{result['n']}{sparse}")
    print("Informational: repeat runs on the same idle machine; no automatic performance threshold yet.")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def until(fn, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            result = fn()
            if result:
                return result
        except (OSError, ValueError, StopIteration):
            pass
        time.sleep(.05)
    raise RuntimeError("fixture timed out")


class Fixture:
    def __init__(self, binary, collection):
        self.base = Path(tempfile.mkdtemp(prefix="muster-eval-"))
        self.binary = binary
        self.port = free_port()
        self.backend = self.browser = None
        self.logs = []
        self.token = ""
        for name in ["run", "data", "home"]:
            (self.base / name).mkdir(mode=0o700)
        self.env = {"PATH": "/usr/bin:/bin", "HOME": str(self.base / "home"), "SHELL": "/bin/bash", "LANG": "C.UTF-8",
                    "MUSTER_RUNTIME_DIR": str(self.base / "run"), "MUSTER_DATA_DIR": str(self.base / "data"),
                    "MUSTER_PORT": str(self.port), "MUSTER_ISOLATION": "off", "MUSTER_METRICS": collection,
                    "MUSTER_WEB_DIR": str(ROOT / "web/dist")}
        self.echo = self.base / "echo.py"
        self.echo.write_text("import sys,tty\ntty.setraw(0)\nprint('EVAL-READY',flush=True)\ni=0\nwhile True:\n c=sys.stdin.read(1)\n if not c: break\n i+=1\n print(f'\\r\\nEVAL{i}:{c}',end='\\r\\n',flush=True)\n")
        print(f"Isolated eval files: {self.base}", file=sys.stderr, flush=True)

    def log(self, name):
        out = open(self.base / name, "ab")
        self.logs.append(out)
        return out

    def rpc(self, method, params=None):
        conn = HTTPConnection("127.0.0.1", self.port, timeout=20)
        try:
            conn.request("POST", "/api/rpc", json.dumps({"id": 1, "method": method, "params": params or {}}),
                         {"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
            response = conn.getresponse()
            data = json.loads(response.read())
            if response.status != 200 or "error" in data:
                raise RuntimeError(f"{method}: {data}")
            return data["result"]
        finally:
            conn.close()

    def start(self):
        log = self.log("backend.log")
        self.backend = subprocess.Popen([str(self.binary), "daemon", "--foreground"], env=self.env,
                                        stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        def ready():
            if self.backend.poll() is not None:
                raise RuntimeError("backend exited; inspect fixture log")
            d = json.loads((self.base / "run/daemon.json").read_text())
            if d["pid"] != self.backend.pid:
                return False
            self.token = d["token"]
            return self.rpc("hello")
        return until(ready, 40)

    def browser_call(self, phase=None, **args):
        if phase:
            self.browser.stdin.write(json.dumps({"phase": phase, **args}) + "\n")
            self.browser.stdin.flush()
        with selectors.DefaultSelector() as selector:
            selector.register(self.browser.stdout, selectors.EVENT_READ)
            if not selector.select(60):
                raise RuntimeError("browser timed out; inspect browser.log")
        line = self.browser.stdout.readline()
        if not line:
            raise RuntimeError("browser exited; inspect browser.log")
        return json.loads(line)

    def start_browser(self):
        self.browser = subprocess.Popen([shutil.which("deno"), "run", "--allow-net=127.0.0.1", "--allow-run=chromium",
            str(ROOT / "web/dev/metrics_eval.ts"), f"http://127.0.0.1:{self.port}/#token={self.token}",
            str(self.base), str(free_port())], env=self.env | {"DENO_DIR": str(self.base / "deno")},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log("browser.log"), text=True)
        return self.browser_call()

    def shell(self, workspace, name):
        c = self.rpc("channel.create", {"workspace_id": workspace, "agent": "shell", "name": name})
        s = next(s for s in self.rpc("hello")["sessions"] if s["channel_id"] == c["id"])
        return c, s

    def host_call(self, method, params=None):
        d = json.loads((self.base / "run/terminal.json").read_text())
        assert Path(d["data_dir"]) == self.base / "data/terminal"
        # The version is read from this checkout, never guessed against a live host.
        import re
        version = int(re.search(r"pub const VERSION: u32 = (\d+)", (ROOT / "crates/terminal-protocol/src/lib.rs").read_text())[1])
        payload = json.dumps({"version": version, "instance": d["instance"], "command":
                             {"method": method, **({"params": params} if params is not None else {})}}).encode()
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(10)
            sock.connect(str(self.base / "run/terminal.sock"))
            sock.sendall(bytes([0]) + struct.pack(">I", len(payload)) + payload)
            def read(n):
                out = b""
                while len(out) < n:
                    part = sock.recv(n-len(out))
                    if not part:
                        raise RuntimeError("host closed")
                    out += part
                return out
            header = read(5)
            result = json.loads(read(struct.unpack(">I", header[1:])[0]))
            if result["type"] == "error":
                raise RuntimeError(str(result))
            return result.get("data")

    def stop_backend(self):
        if self.backend and self.backend.poll() is None:
            self.backend.terminate()
            self.backend.wait(timeout=15)

    def close(self):
        try:
            if self.browser and self.browser.poll() is None:
                self.browser.stdin.write('{"phase":"close"}\n')
                self.browser.stdin.flush()
                self.browser.stdin.close()
                try:
                    self.browser.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    self.browser.terminate()
                    self.browser.wait(timeout=5)
                    raise RuntimeError("eval browser did not close cleanly") from None
        finally:
            try:
                self.stop_backend()
            finally:
                if (self.base / "run/terminal.json").exists():
                    self.host_call("shutdown")
                for log in self.logs:
                    log.close()


def run(args):
    binary = (ROOT / args.binary).resolve()
    if not binary.is_file() or not binary.with_name("muster-terminal-host").is_file():
        raise SystemExit("Build the workspace first (deno task build)")
    counts = [int(n) for n in args.sessions.split(",")]
    if not counts or min(counts) < 1 or max(counts) > 32 or not 1 <= args.repeats <= 10 or not 10 <= args.keys <= 100:
        raise SystemExit("sessions must be 1..32, repeats 1..10, keys 10..100")
    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown")
    report = {"fixture": 1, "renderer": "headless-chromium-software", "machine": {"cpu": cpu, "cpus": os.cpu_count(), "kernel": platform.release(), "arch": platform.machine()},
              "backend": hashlib.sha256(binary.read_bytes()).hexdigest(), "host": hashlib.sha256(binary.with_name("muster-terminal-host").read_bytes()).hexdigest(),
              "ui": hashlib.sha256((ROOT / "web/dist/app.js").read_bytes()).hexdigest(), "sessions": counts, "keys": args.keys, "repeats": args.repeats,
              "collection": args.collection, "results": [], "checks": [], "ok": False, "started": time.time()}
    values = {}
    def add(name, unit, samples):
        entry = values.setdefault(name, {"unit": unit, "samples": []})
        entry["samples"].extend(samples)
    try:
        for repeat in range(args.repeats):
            for count in counts:
                f = Fixture(binary, args.collection)
                try:
                    f.start()
                    f.rpc("prefs.set", {"shell": "bash"})
                    w = f.rpc("workspace.add", {"path": str(f.base / "home")})
                    channel, session = f.shell(w["id"], "echo")
                    sid = session["id"]
                    f.rpc("session.input", {"id": sid, "data": f"exec python3 -u {f.echo}\r"})
                    others = [f.shell(w["id"], f"load-{i}")[1] for i in range(count-1)]
                    ready = f.start_browser()
                    report["browser"] = ready["browser"]["product"]
                    for stage in ["quiet", "output"]:
                        if stage == "output":
                            for s in others:
                                f.rpc("session.input", {"id": s["id"], "data": "while true; do printf '%080d\\n' 0; sleep 0.01; done\r"})
                        r = f.browser_call("typing", sid=sid, channel=channel["id"], keys=args.keys,
                                           start=0 if stage == "quiet" else args.keys)
                        report["checks"].append({"repeat": repeat, "sessions": count, "stage": stage, "attempts": r["attempts"], "failures": r["failures"]})
                        if r["failures"]:
                            raise RuntimeError("controlled echo timed out")
                        prefix = f"{count}_sessions.{stage}"
                        add(prefix+".typing", "ms", r["samples"])
                        add(prefix+".first_key", "ms", r["idle"])
                    # A backend outage must preserve the PTY and every controlled marker once.
                    inventory = f.host_call("inventory")
                    run_id = next(s["run"] for s in inventory if s["run"]["id"] == sid)
                    f.browser_call("before_restart")
                    f.stop_backend()
                    f.host_call("input", {"run": run_id, "data": [ord("y")]})
                    f.start()
                    marker = f"EVAL{args.keys*2+1}:y"
                    recovery = f.browser_call("recovered", sid=sid, marker=marker)
                    expected = [f"EVAL{i}:x" for i in range(1, args.keys*2+1)] + [marker]
                    if recovery["lines"] != expected:
                        raise RuntimeError("recovery lost or duplicated output")
                    current = next(s for s in f.rpc("hello")["sessions"] if s["id"] == sid)
                    if current["pid"] != session["pid"]:
                        raise RuntimeError("recovery replaced the terminal")
                    add(f"{count}_sessions.recovery", "ms", [recovery["ms"]])
                    # Repeated create/remove measures retained backend/host RSS, not agent RSS.
                    def rss(pid):
                        return int(Path(f"/proc/{pid}/statm").read_text().split()[1])*os.sysconf("SC_PAGE_SIZE")
                    host_pid = json.loads((f.base / "run/terminal.json").read_text())["pid"]
                    before = {"backend": rss(f.backend.pid), "host": rss(host_pid)}
                    for i in range(5):
                        c, s = f.shell(w["id"], f"churn-{i}")
                        f.rpc("channel.archive", {"id": c["id"], "remove_worktree": False})
                    flushed = f.browser_call("flush")
                    if flushed["errors"]:
                        raise RuntimeError(str(flushed["errors"]))
                    for label, pid in [("backend", f.backend.pid), ("host", host_pid)]:
                        add(f"{count}_sessions.churn_{label}_rss_delta", "bytes", [rss(pid)-before[label]])
                    exported = f.rpc("metrics.export")
                    report["build_mode"] = "debug" if exported["runs"][exported["run"]]["debug"] else "release"
                    (f.base / "metrics.json").write_text(json.dumps(exported, indent=2)+"\n")
                    if args.collection == "on":
                        names = {s["name"] for s in exported["samples"]}
                        if not {"rpc.duration_ms", "process.rss_bytes", "ui.attach_parse_ms"}.issubset(names):
                            raise RuntimeError(f"metrics missing from end-to-end export: {names}")
                        if exported["persistence_errors"]:
                            raise RuntimeError("metrics persistence failed")
                    elif exported["samples"]:
                        raise RuntimeError("disabled collector wrote samples")
                    print(f"Eval {repeat+1}/{args.repeats}: {count} sessions passed", file=sys.stderr, flush=True)
                finally:
                    f.close()
        report["ok"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        report["results"] = [{"name": name, "unit": entry["unit"], **stats(entry["samples"])} for name, entry in values.items()]
        Path(args.output).write_text(json.dumps(report, indent=2)+"\n")
        print(f"Eval report: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "compare":
        if len(sys.argv) != 4:
            raise SystemExit("usage: eval.py compare BASELINE.json CANDIDATE.json")
        compare(sys.argv[2:])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--output", default="/tmp/muster-eval.json")
        parser.add_argument("--binary", default="target/release/muster")
        parser.add_argument("--sessions", default="1,4,12")
        parser.add_argument("--keys", type=int, default=30)
        parser.add_argument("--repeats", type=int, default=3)
        parser.add_argument("--collection", choices=["on", "off"], default="on")
        run(parser.parse_args())
