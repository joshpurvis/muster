// End-to-end smoke test for the muster daemon. Run through scripts/smoke.sh, which builds the
// binary, makes a temp dir and passes scoped Deno permissions:
//   deno run --allow-net=127.0.0.1:17433 --allow-read=<tmp>,<bin> --allow-write=<tmp> \
//            --allow-run=<bin>,git scripts/smoke.ts <tmp> <bin>
// Never uses port 7433 or the default data dirs.

const [BASE, BIN] = Deno.args;
if (!BASE || !BIN) throw new Error("usage: smoke.ts <tmpdir> <muster-binary>");
const PORT = 17433;
const DATA = `${BASE}/data`;
const RUNTIME = `${BASE}/run`;
const REPO = `${BASE}/repo`;
// Sessions run $SHELL; the test types bash syntax.
const ENV = { MUSTER_DATA_DIR: DATA, MUSTER_RUNTIME_DIR: RUNTIME, MUSTER_PORT: String(PORT), SHELL: "/bin/bash" };

let passed = 0;
function ok(cond: unknown, what: string): void {
  if (!cond) throw new Error(`FAILED: ${what}`);
  passed++;
  console.log(`  ok  ${what}`);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const dec = new TextDecoder();
const enc = new TextEncoder();

async function git(cwd: string, ...args: string[]): Promise<string> {
  const out = await new Deno.Command("git", {
    args: ["-C", cwd, "-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid", ...args],
    stdout: "piped",
    stderr: "piped",
  }).output();
  if (!out.success) throw new Error(`git ${args.join(" ")}: ${dec.decode(out.stderr)}`);
  return dec.decode(out.stdout);
}

async function exists(p: string): Promise<boolean> {
  try {
    await Deno.stat(p);
    return true;
  } catch {
    return false;
  }
}

// ----- daemon process -----

interface Daemon {
  proc: Deno.ChildProcess;
  log: string[];
  token: string;
}

let current: Daemon | null = null;

async function startDaemon(): Promise<Daemon> {
  const proc = new Deno.Command(BIN, {
    args: ["daemon", "--foreground"],
    env: ENV,
    stdin: "null",
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const log: string[] = [];
  for (const s of [proc.stdout, proc.stderr]) {
    (async () => {
      for await (const chunk of s) log.push(dec.decode(chunk));
    })();
  }
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const d = JSON.parse(await Deno.readTextFile(`${RUNTIME}/daemon.json`));
      const r = await fetch(`http://127.0.0.1:${PORT}/api/health`);
      const h = await r.json();
      if (h.ok && d.port === PORT && d.pid === proc.pid) return (current = { proc, log, token: d.token });
    } catch {
      // not up yet
    }
    await sleep(100);
  }
  console.error(log.join(""));
  throw new Error("daemon did not start");
}

async function stopDaemon(d: Daemon, sig: Deno.Signal): Promise<Deno.CommandStatus> {
  d.proc.kill(sig);
  return await d.proc.status;
}

// ----- WS client -----

type Json = Record<string, any>;

class Client {
  ws: WebSocket;
  nextId = 1;
  pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  events: Json[] = [];
  frames: { type: number; id: number; data: Uint8Array }[] = [];
  closed = false;

  constructor(token: string) {
    this.ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?token=${token}`);
    this.ws.binaryType = "arraybuffer";
    this.ws.onmessage = (m) => {
      if (typeof m.data === "string") {
        const v = JSON.parse(m.data);
        if ("id" in v && this.pending.has(v.id)) {
          const p = this.pending.get(v.id)!;
          this.pending.delete(v.id);
          if (v.error) p.reject(new Error(v.error.message));
          else p.resolve(v.result);
        } else if (v.event) {
          this.events.push(v);
        }
      } else {
        const b = new Uint8Array(m.data as ArrayBuffer);
        const id = new DataView(b.buffer).getUint32(1, false);
        this.frames.push({ type: b[0], id, data: b.slice(5) });
      }
    };
    this.ws.onclose = () => (this.closed = true);
  }

  open(): Promise<void> {
    return new Promise((res, rej) => {
      this.ws.onopen = () => res();
      this.ws.onerror = () => rej(new Error("ws error"));
    });
  }

  call(method: string, params: Json = {}): Promise<any> {
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`timeout: ${method}`));
      }, 20000);
    });
  }

  input(id: number, text: string): void {
    const payload = enc.encode(text);
    const b = new Uint8Array(5 + payload.length);
    b[0] = 0x02;
    new DataView(b.buffer).setUint32(1, id, false);
    b.set(payload, 5);
    this.ws.send(b);
  }

  /** All terminal bytes received for a session since the last replay frame. */
  terminal(id: number): string {
    let out = "";
    for (const f of this.frames) {
      if (f.id !== id) continue;
      if (f.type === 0x03) out = dec.decode(f.data);
      else if (f.type === 0x01) out += dec.decode(f.data);
    }
    return out;
  }

  async waitFor(what: string, cond: () => boolean, ms = 10000): Promise<void> {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      if (cond()) return;
      await sleep(50);
    }
    throw new Error(`timeout waiting for ${what}`);
  }

  close(): void {
    this.ws.close();
  }
}

async function rawHttp(req: string): Promise<string> {
  const conn = await Deno.connect({ hostname: "127.0.0.1", port: PORT });
  await conn.write(enc.encode(req));
  const buf = new Uint8Array(4096);
  const n = await conn.read(buf);
  conn.close();
  return dec.decode(buf.subarray(0, n ?? 0));
}

// ----- test -----

async function main() {
  await Deno.mkdir(REPO, { recursive: true });
  await git(REPO, "init", "-q", "-b", "main");
  await Deno.writeTextFile(`${REPO}/README.md`, "smoke\n");
  await git(REPO, "add", ".");
  await git(REPO, "commit", "-q", "-m", "init");

  console.log("start daemon");
  let d = await startDaemon();
  const st = await Deno.stat(`${RUNTIME}/daemon.json`);
  ok((st.mode! & 0o777) === 0o600, "daemon.json is mode 0600");

  // auth
  ok((await fetch(`http://127.0.0.1:${PORT}/api/rpc`, { method: "POST", body: "{}" })).status === 401, "rpc without token -> 401");
  ok(
    (await fetch(`http://127.0.0.1:${PORT}/hook/Stop`, { method: "POST", headers: { Authorization: "Bearer nope" }, body: "{}" })).status === 401,
    "hook with bad session token -> 401",
  );
  const badOrigin = await rawHttp(
    `GET /ws?token=${d.token} HTTP/1.1\r\nHost: 127.0.0.1:${PORT}\r\nOrigin: http://evil.example\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n`,
  );
  ok(badOrigin.startsWith("HTTP/1.1 403"), "ws with foreign Origin -> 403");
  const badTok = await rawHttp(
    `GET /ws?token=wrong HTTP/1.1\r\nHost: 127.0.0.1:${PORT}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n`,
  );
  ok(badTok.startsWith("HTTP/1.1 401"), "ws with bad token -> 401");
  const index = await fetch(`http://127.0.0.1:${PORT}/some/spa/route`);
  ok(index.status === 200 && (await index.text()).includes("<html"), "SPA fallback serves HTML");

  // HTTP rpc
  const rpc = await (
    await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
      method: "POST",
      headers: { Authorization: `Bearer ${d.token}` },
      body: JSON.stringify({ id: 1, method: "hello", params: {} }),
    })
  ).json();
  ok(rpc.result?.version && rpc.result.home, "POST /api/rpc hello works");

  let c = new Client(d.token);
  await c.open();
  const hello = await c.call("hello");
  ok(Array.isArray(hello.sessions) && hello.default_shell, "ws hello");
  await c.call("nonsense.method").then(
    () => ok(false, "unknown method errors"),
    (e) => ok(String(e).includes("unknown method"), "unknown method errors"),
  );
  await c.call("session.resize", { id: "x" }).then(
    () => ok(false, "bad params error"),
    (e) => ok(String(e).includes("invalid params"), "bad params error (no panic)"),
  );
  c.ws.send("not json");
  c.ws.send(new Uint8Array([0x02, 0, 0])); // truncated frame must be ignored

  const ws = await c.call("workspace.add", { path: REPO });
  ok(ws.is_git && ws.name === "repo", "workspace.add on git repo");
  ok(c.events.some((e) => e.event === "workspace_upserted" && e.data.id === ws.id), "workspace_upserted arrives before the result");
  const ws2 = await c.call("workspace.add", { path: REPO + "/" });
  ok(ws2.id === ws.id, "workspace.add is idempotent");

  const ch = await c.call("channel.create", { workspace_id: ws.id, name: "Smoke Test!", agent: "shell" });
  ok(ch.branch === "muster/smoke-test" && ch.owns_worktree, "channel.create made branch muster/smoke-test");
  ok(c.events.some((e) => e.event === "channel_upserted" && e.data.id === ch.id), "channel_upserted arrives before the result");
  ok(await exists(`${ch.cwd}/README.md`), "worktree checked out");
  ok(ch.port_base === 20000 + (ch.id % 1000) * 10, "port_base from channel id");
  const wtList = await git(REPO, "worktree", "list");
  ok(wtList.includes(ch.cwd), "git knows the worktree");
  const ch2 = await c.call("channel.create", { workspace_id: ws.id, name: "smoke test", worktree: true });
  ok(ch2.branch === "muster/smoke-test-2", "branch name deduplicated");

  await c.waitFor("session_upserted", () => c.events.some((e) => e.event === "session_upserted" && e.data.channel_id === ch.id));
  const sess = c.events.find((e) => e.event === "session_upserted" && e.data.channel_id === ch.id)!.data;
  ok(sess.kind === "shell" && sess.pid > 0, "shell session spawned");
  const sid: number = sess.id;

  // attach: result, then replay frame, then live output
  await c.call("session.attach", { id: sid });
  await c.waitFor("replay frame", () => c.frames.some((f) => f.id === sid && f.type === 0x03));
  ok(c.frames.find((f) => f.id === sid)!.type === 0x03, "first frame after attach is 0x03 replay");
  await c.call("session.resize", { id: sid, cols: 100, rows: 30 });
  await sleep(1000); // let the shell draw its prompt
  c.input(sid, "echo mu$((6*7))ster:$MUSTER_SESSION_ID:$PORT:$MUSTER\r");
  const expect = `mu42ster:${sid}:${ch.port_base}:1`;
  await c.waitFor("echo output", () => c.terminal(sid).includes(expect));
  ok(true, `live 0x01 output contains ${expect}`);

  const screen = await c.call("session.screen", { id: sid });
  ok(screen.lines.some((l: string) => l.includes(expect)), "session.screen shows the output");
  ok(c.events.some((e) => e.event === "session_upserted" && e.data.id === sid && e.data.cols === 100), "resize broadcast");

  // replay on re-attach from a second connection
  const c2 = new Client(d.token);
  await c2.open();
  await c2.call("session.attach", { id: sid });
  await c2.waitFor("replay", () => c2.frames.some((f) => f.id === sid && f.type === 0x03));
  ok(c2.terminal(sid).includes(expect), "replay on re-attach contains earlier output");
  c.input(sid, "echo second$((1+1))\r");
  await c2.waitFor("live on 2nd client", () => c2.terminal(sid).includes("second2"));
  ok(true, "both clients receive live output");
  await c2.call("session.detach", { id: sid });
  c2.close();

  // flood: 20 MB of output must not wedge the daemon or lose the tail
  c.input(sid, "head -c 20000000 /dev/zero | tr '\\0' x; echo; echo FLOOD$((40+2))\r");
  await c.waitFor("flood tail", () => c.terminal(sid).includes("FLOOD42"), 60000);
  ok(true, "flood tail delivered (FLOOD42)");
  ok((await c.call("hello")).sessions.length >= 1, "daemon responsive after flood");
  c.frames = [];
  // The flood pushed earlier output out of the 2 MiB ring; leave a fresh marker for replay.
  const marker = "post25flood";
  c.input(sid, "echo post$((5*5))flood\r");
  await c.waitFor("marker", () => c.terminal(sid).includes(marker));

  // timeline + notes
  const note = await c.call("channel.post", { id: ch.id, text: "hello timeline" });
  ok(note.kind === "note", "channel.post returns a note event");
  const tl = await c.call("channel.timeline", { id: ch.id });
  ok(tl.some((e: Json) => e.kind === "session_started") && tl.at(-1).kind === "note", "timeline oldest-first with events");

  // diff
  await Deno.writeTextFile(`${ch.cwd}/README.md`, "smoke\nchanged\n");
  await Deno.writeTextFile(`${ch.cwd}/new.txt`, "x\n");
  const diff = (await c.call("channel.diff", { id: ch.id })).diff as string;
  ok(diff.includes("+changed") && diff.includes("new.txt"), "channel.diff has tracked change + untracked file");

  // fs.complete
  const comp = await c.call("fs.complete", { prefix: `${BASE}/re` });
  ok(comp.entries.includes(`${BASE}/repo/`), "fs.complete");

  // --- hard crash: sessions become exited on restart
  c.close();
  console.log("SIGKILL daemon, restart");
  await sleep(3500); // let the periodic flush persist output.log
  await stopDaemon(d, "SIGKILL");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  let h = await c.call("hello");
  let s = h.sessions.find((x: Json) => x.id === sid);
  ok(s.state === "exited" && s.state_detail === "daemon restarted", "after crash: session exited (daemon restarted)");
  await c.call("session.attach", { id: sid });
  await c.waitFor("replay after restart", () => c.frames.some((f) => f.id === sid && f.type === 0x03));
  const replayed = c.terminal(sid);
  ok(replayed.includes(marker) && replayed.length <= 2 * 1024 * 1024, "exited session replays persisted output.log (capped at 2 MiB)");

  // restart the session: same id, new process, replay cleared
  const rs = await c.call("session.restart", { id: sid });
  ok(rs.id === sid && rs.pid > 0 && rs.state !== "exited", "session.restart spawns a new process");
  await c.waitFor("replay after session restart", () => c.frames.filter((f) => f.id === sid && f.type === 0x03).length >= 2);
  ok(!c.terminal(sid).includes(marker), "restart cleared replay");
  await sleep(800);
  c.input(sid, "echo again$((2+3))\r");
  await c.waitFor("output after restart", () => c.terminal(sid).includes("again5"));
  ok(true, "attached client follows the restarted process");
  const both = await Promise.allSettled([c.call("session.restart", { id: sid }), c.call("session.restart", { id: sid })]);
  ok(
    both.filter((r) => r.status === "fulfilled").length === 1 &&
      both.some((r) => r.status === "rejected" && String(r.reason).includes("already restarting")),
    "concurrent restarts are serialized",
  );
  const live = (await c.call("hello")).sessions.find((x: Json) => x.id === sid);
  ok(live.pid > 0 && live.state !== "exited", "exactly one process after concurrent restarts");

  // kill -> exited with exit code
  await c.call("session.kill", { id: sid });
  await c.waitFor("exit", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === sid && e.data.state === "exited"));
  ok(true, "session.kill -> exited");
  await c.call("session.restart", { id: sid });

  // --- clean shutdown
  c.close();
  console.log("SIGTERM daemon, restart");
  const status = await stopDaemon(d, "SIGTERM");
  ok(status.success, "daemon exits 0 on SIGTERM");
  ok(!(await exists(`${RUNTIME}/daemon.json`)), "daemon.json removed on clean shutdown");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  h = await c.call("hello");
  s = h.sessions.find((x: Json) => x.id === sid);
  ok(s.state === "exited" && s.state_detail === "daemon stopped", "after clean stop: session exited (daemon stopped)");

  // archive with worktree removal (branch kept)
  await c.call("channel.archive", { id: ch.id, remove_worktree: true });
  ok(!(await exists(ch.cwd)), "archive removed the worktree dir");
  ok((await git(REPO, "branch", "--list", "muster/smoke-test")).includes("muster/smoke-test"), "branch kept after archive");
  h = await c.call("hello");
  ok(!h.channels.some((x: Json) => x.id === ch.id) && !h.sessions.some((x: Json) => x.id === sid), "archived channel gone from hello");
  await c.call("workspace.remove", { id: ws.id });
  h = await c.call("hello");
  ok(h.workspaces.length === 0 && h.channels.length === 0, "workspace.remove");

  c.close();
  await stopDaemon(d, "SIGTERM");
  console.log(`\nPASS (${passed} checks)`);
}

try {
  await main();
} catch (e) {
  console.error(String(e));
  if (current) {
    console.error("--- daemon log ---\n" + current.log.join("").slice(-4000));
    try {
      current.proc.kill("SIGTERM");
      await current.proc.status;
    } catch {
      // already gone
    }
  }
  Deno.exit(1);
}
