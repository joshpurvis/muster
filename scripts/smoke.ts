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
  // launch codes: minted with the client token, exchanged once for it
  const base = `http://127.0.0.1:${PORT}`;
  ok((await fetch(`${base}/api/launch-code`, { method: "POST" })).status === 401, "launch-code without token -> 401");
  const lc = await (await fetch(`${base}/api/launch-code`, { method: "POST", headers: { Authorization: `Bearer ${d.token}` } })).json();
  ok(typeof lc.code === "string" && lc.code.length >= 32 && lc.code !== d.token, "launch-code mints a code");
  const exchange = (code: string) =>
    fetch(`${base}/api/exchange`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Origin: base },
      body: JSON.stringify({ code }),
    });
  const lcBody = JSON.stringify({ code: lc.code });
  const evilExchange = await rawHttp(
    `POST /api/exchange HTTP/1.1\r\nHost: 127.0.0.1:${PORT}\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: ${lcBody.length}\r\nConnection: close\r\n\r\n${lcBody}`,
  );
  ok(evilExchange.startsWith("HTTP/1.1 403"), "exchange with foreign Origin -> 403");
  const noOrigin = await rawHttp(
    `POST /api/exchange HTTP/1.1\r\nHost: 127.0.0.1:${PORT}\r\nContent-Type: application/json\r\nContent-Length: ${lcBody.length}\r\nConnection: close\r\n\r\n${lcBody}`,
  );
  ok(noOrigin.startsWith("HTTP/1.1 403"), "exchange without Origin -> 403 (code not consumed)");
  const ex = await exchange(lc.code);
  ok(ex.status === 200 && (await ex.json()).token === d.token, "exchange trades the code for the client token");
  const reuse = await exchange(lc.code);
  ok(reuse.status === 403, "launch code reuse -> 403");
  await reuse.body?.cancel();
  const bogus = await exchange("0".repeat(64));
  ok(bogus.status === 403, "unknown launch code -> 403");
  await bogus.body?.cancel();

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

  const plain = await c.call("channel.create", { workspace_id: ws.id, name: "no tree" });
  ok(!plain.owns_worktree && plain.cwd === ws.path && plain.branch === "main", "worktree is off by default");
  await c.call("channel.archive", { id: plain.id, remove_worktree: false });
  const ch = await c.call("channel.create", { workspace_id: ws.id, name: "Smoke Test!", agent: "shell", worktree: true });
  ok(ch.branch === "muster/smoke-test" && ch.owns_worktree, "channel.create made branch muster/smoke-test");
  ok(ch.cwd.endsWith(`/${ch.id}`), "worktree directory is keyed by the channel id");
  ok(ch.auto_named === false && ch.window_id === 1, "named channel, placed in a new window 1");
  ok(c.events.some((e) => e.event === "window_upserted" && e.data.id === 1 && e.data.channel_ids.includes(ch.id)), "window_upserted with the new tab");
  ok(c.events.some((e) => e.event === "channel_upserted" && e.data.id === ch.id), "channel_upserted arrives before the result");
  ok(await exists(`${ch.cwd}/README.md`), "worktree checked out");
  ok(ch.port_base === 20000 + (ch.id % 1000) * 10, "port_base from channel id");
  const wtList = await git(REPO, "worktree", "list");
  ok(wtList.includes(ch.cwd), "git knows the worktree");
  const ch2 = await c.call("channel.create", { workspace_id: ws.id, name: "smoke test", worktree: true });
  ok(ch2.branch === "muster/smoke-test-2", "branch name deduplicated");

  // Per-channel agent permissions: defaults, create with a profile, set_permissions round trip.
  ok(
    ch.permissions?.codex_sandbox === "workspace_write" && ch.permissions?.claude_mode === "default",
    "new channel gets the default permission profile",
  );
  const netPerms = { codex_sandbox: "workspace_write_network", claude_mode: "accept_edits" };
  const ch3 = await c.call("channel.create", {
    workspace_id: ws.id,
    name: "perms",
    worktree: false,
    permissions: netPerms,
  });
  ok(
    ch3.permissions.codex_sandbox === "workspace_write_network" && ch3.permissions.claude_mode === "accept_edits",
    "channel.create accepts permissions",
  );
  const before = c.events.length;
  const setP = await c.call("channel.set_permissions", {
    id: ch3.id,
    permissions: { codex_sandbox: "read_only", claude_mode: "plan" },
  });
  ok(setP.permissions.codex_sandbox === "read_only" && setP.permissions.claude_mode === "plan", "set_permissions returns the channel");
  await c.waitFor("channel_upserted", () =>
    c.events.slice(before).some((e) => e.event === "channel_upserted" && e.data.id === ch3.id));
  const again = (await c.call("hello")).channels.find((x: { id: number }) => x.id === ch3.id);
  ok(again.permissions.codex_sandbox === "read_only" && again.permissions.claude_mode === "plan", "permissions persisted (hello)");
  let bad = "";
  try {
    await c.call("channel.set_permissions", { id: ch3.id, permissions: { codex_sandbox: "yolo" } });
  } catch (e) {
    bad = String(e);
  }
  ok(bad.includes("unknown variant"), "set_permissions rejects unknown values");

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

  // session.output / session.input / session.wait (the CLI's primitives)
  const out = await c.call("session.output", { id: sid, lines: 20 });
  ok(out.lines.length <= 20 && out.lines.some((l: string) => l.includes(expect)), "session.output returns the tail as plain text");
  ok(!out.lines.join("\n").includes("\x1b["), "session.output has no ANSI escapes");
  await c.call("session.input", { id: sid, data: "echo in$((3*3))put\r" });
  await c.waitFor("session.input output", () => c.terminal(sid).includes("in9put"));
  ok(true, "session.input writes raw bytes");
  const wIdle = await c.call("session.wait", { id: sid, until: ["idle", "working"], timeout_ms: 5000 });
  ok(wIdle.matched && !wIdle.timed_out && wIdle.session.id === sid, "session.wait returns at once when already matching");
  const t0 = Date.now();
  const wNo = await c.call("session.wait", { id: sid, until: ["needs_human"], timeout_ms: 400 });
  ok(!wNo.matched && wNo.timed_out && Date.now() - t0 >= 350, "session.wait times out (needs_human group)");
  await c.call("session.wait", { id: sid, until: ["bogus"] }).then(
    () => ok(false, "session.wait rejects unknown states"),
    (e) => ok(String(e).includes("unknown state"), "session.wait rejects unknown states"),
  );

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

  // ----- windows, tabs, auto naming -----
  const httpRpc = async (method: string, params: Json = {}) =>
    await (
      await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
        method: "POST",
        headers: { Authorization: `Bearer ${d.token}` },
        body: JSON.stringify({ id: 9, method, params }),
      })
    ).json();
  ok((await httpRpc("window.claim")).error?.message.includes("WebSocket"), "window.claim is WebSocket only");
  const w1 = await c.call("window.claim", {});
  ok(w1.id === 1 && w1.open && w1.channel_ids.includes(ch.id), "window.claim takes the lowest window with tabs");
  ok((await c.call("hello")).windows.some((w: Json) => w.id === 1 && w.open), "hello lists windows");
  await c.call("window.focused", { id: 1 });
  const c3 = new Client(d.token);
  await c3.open();
  const w2 = await c3.call("window.claim", {});
  ok(w2.id === 2 && w2.channel_ids.length === 0 && w2.name === "2", "second window.claim gets a new empty window 2");
  // New channels go to the focused open window unless told otherwise.
  const auto = await c.call("channel.create", { workspace_id: ws.id, agent: "shell", prompt: "echo fix the flaky login test now please" });
  ok(auto.auto_named && auto.name === "echo fix the flaky login test", "unnamed channel is named from its first prompt");
  ok(auto.window_id === 1, "new channel goes to the last focused window");
  const bare = await c.call("channel.create", { workspace_id: ws.id, worktree: true, window_id: 2 });
  ok(bare.auto_named && bare.name === "repo" && bare.window_id === 2, "no name, no prompt: folder name, in the requested window");
  ok(bare.branch === `muster/${bare.id}` && bare.cwd.endsWith(`/${bare.id}`), "worktree starts on muster/<id>");
  await c.call("channel.create", { workspace_id: ws.id, window_id: 99 }).then(
    () => ok(false, "channel.create rejects an unknown window"),
    (e) => ok(String(e).includes("no such window"), "channel.create rejects an unknown window"),
  );
  // A shell's OSC title names a channel that has no prompt (priority 3), spinner glyph stripped.
  const bs = await c.call("session.spawn", { channel_id: bare.id, kind: "shell" });
  await sleep(800);
  await c.call("session.input", { id: bs.id, data: "printf '\\033]0;\\342\\234\\263 build the widget\\007'\r" });
  await c.waitFor("title-derived name", () =>
    c.events.some((e) => e.event === "channel_upserted" && e.data.id === bare.id && e.data.name === "build the widget"));
  ok(true, "shell title names the channel (glyph stripped)");
  await c.waitFor("branch renamed after the first derived name", () =>
    c.events.some((e) => e.event === "channel_upserted" && e.data.id === bare.id && e.data.branch === "muster/build-the-widget"));
  ok((await git(REPO, "branch", "--list", "muster/build-the-widget")).includes("muster/build-the-widget"), "git branch renamed to muster/<slug>");
  ok(await exists(bare.cwd), "worktree directory did not move");
  // A prompt outranks a shell title.
  await c.call("session.send", { id: bs.id, text: "echo second opinion" });
  await c.waitFor("prompt-derived name", () =>
    c.events.some((e) => e.event === "channel_upserted" && e.data.id === bare.id && e.data.name === "echo second opinion"));
  ok(true, "first prompt outranks the shell title");
  const renamed = await c.call("channel.rename", { id: bare.id, name: "My Feature" });
  ok(!renamed.auto_named && renamed.name === "My Feature" && renamed.branch === "muster/my-feature", "channel.rename ends auto naming and renames the branch");
  await c.call("session.input", { id: bs.id, data: "printf '\\033]0;other title\\007'\r" });
  await sleep(500);
  ok((await c.call("hello")).channels.find((x: Json) => x.id === bare.id).name === "My Feature", "no auto naming after a rename");
  // Tabs: move, reorder, rename, raise.
  const mv = await c.call("channel.move", { id: auto.id, window_id: 2, index: 0 });
  ok(mv.window_id === 2, "channel.move");
  let wins = (await c.call("hello")).windows;
  ok(JSON.stringify(wins.find((w: Json) => w.id === 2).channel_ids) === JSON.stringify([auto.id, bare.id]), "moved tab inserted at index");
  ok(!wins.find((w: Json) => w.id === 1).channel_ids.includes(auto.id), "moved tab left its old window");
  const ro = await c.call("window.reorder_tabs", { id: 2, channel_ids: [bare.id, auto.id] });
  ok(JSON.stringify(ro.channel_ids) === JSON.stringify([bare.id, auto.id]), "window.reorder_tabs");
  ok((await c.call("window.rename", { id: 2, name: "  side  " })).name === "side", "window.rename");
  const w3 = await c.call("window.create", {});
  ok(w3.id === 3 && !w3.open, "window.create makes an unheld window");
  ok((await c.call("window.raise", { id: 3 })).raised === false, "window.raise is false when no client holds the window");
  // Closing a client releases its window; an empty released window is deleted.
  c.events = [];
  c3.close();
  await c.waitFor("window 2 released", () => c.events.some((e) => e.event === "window_upserted" && e.data.id === 2 && !e.data.open));
  ok(true, "closing a client releases its window (tabs kept)");
  await c.call("channel.move", { id: auto.id, window_id: 1 });
  await c.call("channel.move", { id: bare.id, window_id: 1 });
  await c.waitFor("window 2 removed", () => c.events.some((e) => e.event === "window_removed" && e.data.id === 2));
  ok(true, "a released window without tabs is deleted");
  await c.call("channel.archive", { id: auto.id, remove_worktree: false });
  await c.call("channel.archive", { id: bare.id, remove_worktree: true });
  const c4 = new Client(d.token);
  await c4.open();
  const w4 = await c4.call("window.claim", { id: 3 });
  ok(w4.id === 3 && w4.open, "window.claim with an id takes that window");
  c4.close();
  await c.waitFor("window 3 removed", () => c.events.some((e) => e.event === "window_removed" && e.data.id === 3));
  ok(true, "empty window deleted when its client leaves");

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
  // A shell state change may land after the note, so check order rather than the last entry.
  const iStart = tl.findIndex((e: Json) => e.kind === "session_started");
  const iNote = tl.findIndex((e: Json) => e.kind === "note" && e.id === note.id);
  ok(iStart >= 0 && iNote > iStart, "timeline oldest-first with events");

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
  ok(h.windows.some((w: Json) => w.id === 1 && !w.open && w.channel_ids.includes(ch.id)), "windows persist across restarts");
  await c.call("session.attach", { id: sid });
  await c.waitFor("replay after restart", () => c.frames.some((f) => f.id === sid && f.type === 0x03));
  const replayed = c.terminal(sid);
  // 2 MiB of output plus a short terminal-mode prefix.
  ok(replayed.includes(marker) && replayed.length <= 2 * 1024 * 1024 + 256, "exited session replays persisted output.log (capped at 2 MiB)");

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

  // leftovers: a background job (ignoring SIGHUP) of a shell that exited is reaped on kill
  await sleep(800);
  c.events = [];
  c.input(sid, `(trap '' HUP; exec sleep 7771) & echo $! > ${BASE}/bgpid; exit\r`);
  await c.waitFor("shell exit", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === sid && e.data.state === "exited"));
  const bg = (await Deno.readTextFile(`${BASE}/bgpid`)).trim();
  await c.call("session.kill", { id: sid });
  await sleep(4000); // SIGHUP is ignored: SIGKILL follows after the 3 s grace
  await c.call("session.restart", { id: sid });
  await sleep(800);
  c.input(sid, `kill -0 ${bg} 2>/dev/null && echo BG-ALIVE || echo BG-GONE$((1))\r`);
  await c.waitFor("leftover check", () => c.terminal(sid).includes("BG-GONE1") || c.terminal(sid).includes("BG-ALIVE\r"));
  ok(c.terminal(sid).includes("BG-GONE1"), "background job of an exited shell killed by session.kill");

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
