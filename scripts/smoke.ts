// End-to-end smoke test for the muster daemon. Run through scripts/smoke.sh, which builds the
// binary, makes a temp dir and passes scoped Deno permissions:
//   deno run --allow-net=127.0.0.1 --allow-read=<tmp>,<bin> --allow-write=<tmp> \
//            --allow-run=<bin>,git,python3,unshare,openssl scripts/smoke.ts <tmp> <bin>
// The panel proxy gets an ephemeral loopback port. Python reads the fixture SQLite DB and
// lists command lines (no push endpoint or signed token may be on one); openssl makes the
// phone-alerts fake push service's certificate.
// Never uses port 7433 or the default data dirs.

const [BASE, BIN, PORT_ARG] = Deno.args;
if (!BASE || !BIN) throw new Error("usage: smoke.ts <tmpdir> <muster-binary> [port]");
// A port of its own lets two worktrees run the smoke test at once (scripts/smoke.sh passes
// MUSTER_SMOKE_PORT).
const PORT = Number(PORT_ARG || 17433);
if (!Number.isInteger(PORT) || PORT < 1024 || PORT > 65535 || PORT === 7433) {
  throw new Error(`bad smoke port ${PORT_ARG}: use 1024-65535, never 7433`);
}
const DATA = `${BASE}/data`;
const RUNTIME = `${BASE}/run`;
const REPO = `${BASE}/repo`;
// Sessions run $SHELL; the test types bash syntax.
const ENV = {
  MUSTER_DATA_DIR: DATA, MUSTER_RUNTIME_DIR: RUNTIME, MUSTER_PORT: String(PORT), SHELL: "/bin/bash",
  HOME: `${BASE}/home`, MUSTER_URL: "", MUSTER_TOKEN: "", MUSTER_BIN: "",
  MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "", MUSTER_RUN: "",
  PATH: `${BASE}/bin:/usr/bin:/bin`, MUSTER_CODEX_HOOKS: "0",
  // No desktop notifications from the test daemon (schedule alerts would reach the real desktop).
  MUSTER_NO_NOTIFY: "1",
};

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

async function startDaemon(extraEnv: Record<string, string> = {}): Promise<Daemon> {
  const proc = new Deno.Command(BIN, {
    args: ["daemon", "--foreground"],
    // Never the owner's real key: unkeyed unless a check gives it a fake one.
    env: { ...ENV, OPENROUTER_API_KEY: "", ...extraEnv },
    stdin: "null",
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const log: string[] = [];
  current = { proc, log, token: "" };
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

  /** `origin`: the Origin header to send (Muster's own UI sends its origin; a script none). */
  constructor(token: string, origin?: string) {
    const url = `ws://127.0.0.1:${PORT}/ws?token=${token}`;
    // Deno's WebSocket takes headers as its second argument.
    // deno-lint-ignore no-explicit-any
    this.ws = origin ? new (WebSocket as any)(url, { headers: { Origin: origin } }) : new WebSocket(url);
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

/** A fake CLI's launch: its arguments on the first line, then the provider variables it got
 * (only those that are set), in `<BASE>/launch-<cli>-<session id>`. */
const LAUNCH_VARS =
  "ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN ANTHROPIC_API_KEY ANTHROPIC_DEFAULT_SONNET_MODEL OPENROUTER_API_KEY MUSTER_PROVIDER CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST CLAUDE_CODE_SUBPROCESS_ENV_SCRUB";
const LAUNCH_DUMP = (cli: string) =>
  `{ printf 'ARGS'; printf ' %s' "$@"; printf '\\n'; for v in ${LAUNCH_VARS}; do if [ -n "\${!v+set}" ]; then printf '%s=%s\\n' "$v" "\${!v}"; fi; done; } > '${BASE}/launch-${cli}-'"$MUSTER_SESSION_ID"`;

async function main() {
  // Stand-in only: renders an empty Codex composer, records input, never starts an agent. Asked
  // to resume the thread "stale-thread", it fails the way Codex does for a thread it doesn't
  // have (guess: exit 1 before its TUI and hooks start).
  await Deno.mkdir(`${BASE}/bin`, { recursive: true });
  await Deno.writeTextFile(`${BASE}/bin/codex`, `#!/bin/bash
printf '%s' "$MUSTER_TOKEN" > '${BASE}/fake-token'
${LAUNCH_DUMP("codex")}
if [ "$1" = resume ] && [ "\${@: -1}" = stale-thread ]; then echo 'Error: No saved session found with ID stale-thread. Run \`codex resume\` without an ID to choose from existing sessions.'; exit 1; fi
if [ "$1" = resume ] && [ -e '${BASE}/codex-broken' ]; then echo 'Error: failed to initialize state runtime: database is locked'; exit 1; fi
stty -echo -icanon min 1 time 0
printf '\\033[2J\\033[H› \\r\\n\\r\\n  fake idle agent\\r\\n'
exec cat > '${BASE}/fake-input'
`);
  await Deno.chmod(`${BASE}/bin/codex`, 0o700);
  // Stand-in Claude: records how it was launched, then idles (never starts a real agent).
  await Deno.writeTextFile(`${BASE}/bin/claude`, `#!/bin/bash
${LAUNCH_DUMP("claude")}
printf '%s' "$MUSTER_TOKEN" > '${BASE}/claude-token-'"$MUSTER_SESSION_ID"
exec cat > /dev/null
`);
  await Deno.chmod(`${BASE}/bin/claude`, 0o700);
  await Deno.mkdir(REPO, { recursive: true });
  await git(REPO, "init", "-q", "-b", "main");
  await Deno.writeTextFile(`${REPO}/README.md`, "smoke\n");
  await git(REPO, "add", ".");
  await git(REPO, "commit", "-q", "-m", "init");

  // A home left in the old bots folder moves to <data>/agents when the daemon starts.
  await Deno.mkdir(`${DATA}/bots/stray/memory`, { recursive: true });
  await Deno.writeTextFile(`${DATA}/bots/stray/memory/note.md`, "kept\n");

  console.log("start daemon");
  let d = await startDaemon();
  ok(
    !(await exists(`${DATA}/bots`)) && (await Deno.readTextFile(`${DATA}/agents/stray/memory/note.md`)) === "kept\n",
    "daemon start moves <data>/bots to <data>/agents",
  );
  const st = await Deno.stat(`${RUNTIME}/daemon.json`);
  ok((st.mode! & 0o777) === 0o600, "daemon.json is mode 0600");

  // auth
  // Without a token the request is taken as a device-cookie call, which needs `X-Muster: 1`.
  const noTok = await fetch(`http://127.0.0.1:${PORT}/api/rpc`, { method: "POST", body: "{}" });
  ok(noTok.status === 403 && (await noTok.text()).includes("missing X-Muster header"), "rpc without token -> 403");
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
  // Shell tabs default to the account's login shell (fish for some): pin a POSIX one for the
  // `$((...))` below, through the same preference the UI sets.
  const pinned = await c.call("prefs.set", { shell: "bash" });
  ok(typeof pinned.shell === "string" && pinned.shell.endsWith("/bash"), "prefs.set shell resolves a name on PATH");
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
  ok(sess.window_id === ch.window_id, "channel.create's first session is in the channel's window");
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
  c.input(sid, "echo shell:$SHELL; cd /\r");
  await c.waitFor("shell output", () => c.terminal(sid).includes(`shell:${pinned.shell}`));
  ok(true, "the shell runs the chosen shell, with SHELL set to it");
  await c.waitFor("live folder", () =>
    c.events.some((e) => e.event === "session_upserted" && e.data.id === sid && e.data.live_cwd === "/"));
  ok(true, "cd moves the session's live_cwd");
  ok(true, `live 0x01 output contains ${expect}`);

  // Panels: real MCP auth, persistence, separate proxy, waits and idle prompt delivery.
  c.input(sid, `printf '%s' "$MUSTER_TOKEN" > '${BASE}/shell-token'\r`);
  for (let n = 0; n < 100 && !(await exists(`${BASE}/shell-token`)); n++) await sleep(50);
  const shellToken = await Deno.readTextFile(`${BASE}/shell-token`);
  let mcpId = 0;
  const mcp = async (token: string, name: string, args: Json = {}): Promise<any> => {
    const response = await fetch(`http://127.0.0.1:${PORT}/mcp`, {
      method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++mcpId, method: "tools/call", params: { name, arguments: args } }),
    });
    const result = await response.json();
    if (!response.ok || result.error || result.result?.isError) throw new Error(`MCP ${name}: ${JSON.stringify(result)}`);
    return JSON.parse(result.result.content[0].text);
  };
  const pid = (await mcp(shellToken, "panel_open", { title: "Smoke review", app: "markdown", data: { markdown: "# Hello" } })).panel_id;
  const panelDir = `${DATA}/panels/${pid}`;
  const panelRow = async () => {
    const out = await new Deno.Command("python3", { args: ["-c",
      "import sqlite3,sys; c=sqlite3.connect('file:'+sys.argv[1]+'?mode=ro',uri=True); r=c.execute('SELECT record FROM panels WHERE id=?',(sys.argv[2],)).fetchone(); print(r[0] if r else 'null')",
      `${DATA}/muster.db`, String(pid)], stdout: "piped", stderr: "piped" }).output();
    if (!out.success) throw new Error(dec.decode(out.stderr));
    return JSON.parse(dec.decode(out.stdout));
  };
  ok((await panelRow()).channel_id === ch.id && (await exists(`${panelDir}/app-1.html`)), "MCP panel_open persists its row and app file");
  ok(((await Deno.stat(`${panelDir}/data-1.json`)).mode! & 0o777) === 0o600, "panel input is private (0600)");
  await mcp(shellToken, "panel_update", { panel_id: pid, data: { markdown: "# Revised" } });
  ok((await panelRow()).revision === 2 && JSON.parse(await Deno.readTextFile(`${panelDir}/data-2.json`)).markdown === "# Revised", "panel_update commits revision and input");
  // Built-in image app: Muster reads a worktree file into a data URI and refuses escapes.
  await Deno.writeFile(`${ch.cwd}/smoke-shot.png`, new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]));
  const imagePid = (await mcp(shellToken, "panel_open", { title: "Smoke image", app: "image", data: { path: "smoke-shot.png", alt: "x" } })).panel_id;
  const imageData = JSON.parse(await Deno.readTextFile(`${DATA}/panels/${imagePid}/data-1.json`));
  ok(imageData.src === "data:image/png;base64,iVBORw0KGgoAAAAN" && imageData.bytes === 12 && (await Deno.readTextFile(`${DATA}/panels/${imagePid}/app-1.html`)).includes("Muster image"), "image panel_open reads a worktree file into a data URI");
  ok(await mcp(shellToken, "panel_open", { title: "Escape", app: "image", data: { path: "/etc/passwd" } }).then(() => false, (e) => String(e).includes("inside the task worktree")), "image paths cannot leave the worktree");
  await mcp(shellToken, "panel_close", { panel_id: imagePid });
  await Deno.remove(`${ch.cwd}/smoke-shot.png`);
  const proxyOrigin = hello.panel_origin;
  ok(proxyOrigin === `http://127.0.0.1:${JSON.parse(await Deno.readTextFile(`${RUNTIME}/daemon.json`)).panel_port}` && !proxyOrigin.endsWith(`:${PORT}`), "discovery and hello agree on a separate panel origin");
  const proxy = await fetch(`${proxyOrigin}/proxy`);
  const proxyHtml = await proxy.text();
  ok(proxy.status === 200 && proxy.headers.get("content-security-policy")?.includes("connect-src 'none'") && !proxyHtml.includes(d.token) && !proxy.headers.has("set-cookie"), "proxy has CSP and no client token or cookies");
  const deniedRoutes = await Promise.all(["/", "/api/rpc", "/api/health", "/ws", "/mcp", "/hook/Stop"].map(async (path) => {
    const response = await fetch(proxyOrigin + path); await response.body?.cancel(); return response.status;
  }));
  ok(deniedRoutes.every((status) => status === 404), "panel origin serves only /proxy");
  // web_preview: a local dev server, checked by the backend and framed through /preview.
  const devServer = Deno.serve({ port: 0, hostname: "127.0.0.1", onListen() {} }, () => new Response("<h1>dev</h1>", { headers: { "content-type": "text/html" } }));
  const devUrl = `http://127.0.0.1:${devServer.addr.port}/`;
  const preview = await mcp(shellToken, "panel_open", { title: "Smoke preview", app: "web_preview", data: { url: devUrl } });
  ok(typeof preview.panel_id === "number" && preview.note === undefined, `web_preview opens on a running dev server without a note: ${JSON.stringify(preview)}`);
  ok((await c.call("panel.preview_check", { id: preview.panel_id })).status === 200, "panel.preview_check reads the dev server");
  const framed = await fetch(`${proxyOrigin}/preview?url=${encodeURIComponent(devUrl)}`);
  const framedHtml = await framed.text();
  ok(framed.status === 200 && framed.headers.get("content-security-policy")?.includes(`frame-src http://127.0.0.1:${devServer.addr.port};`)
    && framedHtml.includes(`src="${devUrl}"`) && !framedHtml.includes("<script") && !framedHtml.includes(d.token), "/preview frames only the dev server's origin, with no script or token");
  for (const bad of ["https://example.com/", `http://127.0.0.1:${PORT}/`, `${proxyOrigin}/proxy`]) {
    const r = await fetch(`${proxyOrigin}/preview?url=${encodeURIComponent(bad)}`);
    await r.body?.cancel();
    ok(r.status === 400, `/preview refuses ${bad}`);
    ok(await mcp(shellToken, "panel_open", { title: "Bad", app: "web_preview", data: { url: bad } }).then(() => false, (e) => /system browser|Muster's own/.test(String(e))), `web_preview panel_open refuses ${bad}`);
  }
  // The user's address bar: completed, checked like the agent's URL, and reported to the agent.
  const typed = await c.call("panel.navigate", { id: preview.panel_id, url: `127.0.0.1:${devServer.addr.port}/x` });
  ok(typed.url === `${devUrl}x` && typed.changed === true, `panel.navigate completes a typed address: ${JSON.stringify(typed)}`);
  ok(await c.call("panel.navigate", { id: preview.panel_id, url: `localhost:${PORT}` }).then(() => false, (e: unknown) => /Muster's own/.test(String(e))), "panel.navigate refuses Muster's own port");
  const navEvents = (await mcp(shellToken, "wait_for_panel_event", { panel_id: preview.panel_id, timeout_seconds: 0 })).events;
  ok(navEvents.length === 1 && navEvents[0].kind === "navigated" && navEvents[0].payload.url === `${devUrl}x` && navEvents[0].payload.previous === devUrl,
    `the agent gets one navigated event: ${JSON.stringify(navEvents)}`);
  await devServer.shutdown();
  const down = await mcp(shellToken, "panel_update", { panel_id: preview.panel_id, data: { url: devUrl } });
  ok(String(down.note).includes("Nothing answers"), `a stopped dev server gives the agent a note: ${JSON.stringify(down)}`);
  await mcp(shellToken, "panel_close", { panel_id: preview.panel_id });
  const page = await fetch(`http://127.0.0.1:${PORT}/`);
  await page.body?.cancel();
  ok(page.headers.get("x-frame-options") === "DENY" && page.headers.get("content-security-policy") === "frame-ancestors 'none'", "Muster's own page refuses to be framed");
  await c.call("panel.context", { panel_id: pid, context: { structuredContent: { selection: 2 } } });
  const waiting = mcp(shellToken, "wait_for_panel_event", { panel_id: pid, timeout_seconds: 5 });
  await sleep(150);
  await c.call("panel.event", { panel_id: pid, kind: "approve", payload: { accepted: true } });
  const received = await waiting;
  ok(received.events.length === 1 && received.events[0].kind === "approve" && received.events[0].context.structuredContent.selection === 2, "UI event and latest context reach MCP wait");
  ok((await mcp(shellToken, "wait_for_panel_event", { panel_id: pid, timeout_seconds: 0 })).timed_out, "wait consumes the event once");
  const cliPanels = await new Deno.Command(BIN, { args: ["panel", "list"], env: { ...ENV, MUSTER_SESSION_ID: String(sid) }, stdout: "piped", stderr: "piped" }).output();
  ok(cliPanels.success && dec.decode(cliPanels.stdout).includes("Smoke review"), "panel CLI uses the same task-scoped backend");
  const fake = await c.call("session.spawn", { channel_id: ch.id, kind: "codex" });
  await c.waitFor("fake idle agent", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === fake.id && e.data.state === "idle"));
  const fakeToken = await Deno.readTextFile(`${BASE}/fake-token`);
  // Cold prompt cache (src/cache.rs): a turn's end starts the clock, the lifetime comes from the
  // shared prefs (Codex on its plan: 30 min), and changing it moves every session's cold time.
  const fakeInfo = async () => (await c.call("hello")).sessions.find((x: Json) => x.id === fake.id);
  ok((await fakeInfo()).cache_cold_at === null, "cache: no cold time before the agent's first turn");
  const notified = await fetch(`http://127.0.0.1:${PORT}/hook/notify`, {
    method: "POST", headers: { Authorization: `Bearer ${fakeToken}` },
    body: JSON.stringify({ type: "agent-turn-complete", "last-assistant-message": "done" }),
  });
  ok(notified.ok, "cache: Codex's turn-complete notify accepted");
  await c.waitFor("a turn's end", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === fake.id && e.data.last_turn_at));
  const turned = await fakeInfo();
  ok(turned.cache_cold_at === turned.last_turn_at + 30 * 60_000, "cache: Codex on its plan goes cold 30 min after its last turn");
  const shellInfo = (await c.call("hello")).sessions.find((x: Json) => x.kind === "shell");
  ok(shellInfo && shellInfo.cache_cold_at === null && shellInfo.last_turn_at === null, "cache: shells never get a cold time");
  await c.call("prefs.set", { cache_ttl: { codex: 1 } });
  ok((await fakeInfo()).cache_cold_at === turned.last_turn_at + 60_000, "cache: a 1-minute lifetime moves its cold time");
  await c.call("prefs.set", { cache_warn: false });
  ok((await fakeInfo()).cache_cold_at === null, "cache: the warning off clears it");
  await c.call("prefs.set", { cache_warn: true, cache_ttl: { codex: 30 } });
  const fakePanel = (await mcp(fakeToken, "panel_open", { title: "Idle delivery", app: "markdown" })).panel_id;
  // Registering a wait suppresses automatic injection even for an idle opener.
  const idleWait = mcp(fakeToken, "wait_for_panel_event", { panel_id: fakePanel, timeout_seconds: 5 });
  await sleep(150);
  await c.call("panel.event", { panel_id: fakePanel, kind: "waited", payload: {} });
  ok((await idleWait).events[0].kind === "waited", "idle agent wait takes precedence over injection");
  await sleep(600);
  ok(!(await Deno.readTextFile(`${BASE}/fake-input`)).includes("waited"), "waited event is never injected");
  await c.call("panel.event", { panel_id: fakePanel, kind: "approve", payload: { message: "approved from panel" } });
  let injected = "";
  for (let n = 0; n < 100; n++) { injected = await Deno.readTextFile(`${BASE}/fake-input`); if (injected.includes("approved from panel")) break; await sleep(100); }
  ok(injected.includes('[panel "Idle delivery" #') && injected.includes("approved from panel"), "idle agent stand-in receives the marked panel prompt");
  ok((await mcp(fakeToken, "wait_for_panel_event", { panel_id: fakePanel, timeout_seconds: 0 })).timed_out, "injected event is not returned again by wait");
  // The user closing a panel is only a notice: no prompt of its own, it rides with the next one.
  const dismissed = (await mcp(fakeToken, "panel_open", { title: "Dismissed", app: "markdown" })).panel_id;
  await c.call("panel.dismiss", { id: dismissed });
  await sleep(1200);
  ok(!(await Deno.readTextFile(`${BASE}/fake-input`)).includes('"kind":"closed"'), "closing a panel does not prompt the idle agent");
  await c.call("panel.event", { panel_id: fakePanel, kind: "approve", payload: { message: "after the close" } });
  for (let n = 0; n < 100; n++) { injected = await Deno.readTextFile(`${BASE}/fake-input`); if (injected.includes("after the close")) break; await sleep(100); }
  ok(injected.includes('[panel "Dismissed" #') && injected.includes('"kind":"closed"') && injected.includes("after the close"), "the close notice comes with the next panel prompt");
  // Three ~9 KiB comments exceed one 24 KiB prompt: each arrives whole, over two prompts.
  const tails = [0, 1, 2].map((n) => `END-OF-COMMENT-${n}`);
  for (const tail of tails) await c.call("panel.event", { panel_id: fakePanel, kind: "comment", payload: { text: "x".repeat(9000) + tail } });
  for (let n = 0; n < 300; n++) { injected = await Deno.readTextFile(`${BASE}/fake-input`); if (tails.every((t) => injected.includes(t))) break; await sleep(100); }
  ok(tails.every((t) => injected.includes(`x${t}`)) && !injected.includes("truncated"), "long panel events reach the idle agent whole, none cut");
  // Typed is not yet submitted: let the last prompt finish before moving its opener.
  for (let n = 0; n < 200; n++) { if ((await c.call("hello")).sessions.find((x: Json) => x.id === fake.id).queued === 0) break; await sleep(100); }
  // A wait is bound to its caller's task: moving the caller ends it, and the click stays queued.
  const movedWait = mcp(fakeToken, "wait_for_panel_event", { panel_id: fakePanel, timeout_seconds: 10 }).then(() => "returned", (e) => String(e));
  await sleep(150);
  await c.call("session.move", { id: fake.id, channel_id: ch2.id });
  ok((await movedWait).includes("moved to another task"), "moving the caller cancels its panel wait");
  await c.call("panel.event", { panel_id: fakePanel, kind: "after-move", payload: {} });
  await sleep(1200);
  ok(!(await Deno.readTextFile(`${BASE}/fake-input`)).includes("after-move"), "a moved opener gets no prompt for its old task's panel");
  const collected = await mcp(shellToken, "wait_for_panel_event", { panel_id: fakePanel, timeout_seconds: 0 });
  ok(collected.events[0]?.kind === "after-move", `the old task can still collect that event (${JSON.stringify(collected).slice(0, 300)})`);
  const closing = (await mcp(shellToken, "panel_open", { title: "Closing", app: "markdown" })).panel_id;
  await mcp(shellToken, "panel_close", { panel_id: closing });
  ok(!(await exists(`${DATA}/panels/${closing}`)), "closing a panel deletes its app and data files");
  // Notes: the human (UI RPC), an agent (MCP, the fake Codex session) and the CLI share them.
  const cliEnv0 = () => ({ ...ENV, MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "" });
  const human = await c.call("note.create", { title: "Smoke note", body: "line one\r\n" });
  ok(human.rev === 1 && human.body === "line one\n" && human.updated_by.kind === "human", "note.create: plain text, the human's");
  const nw1 = await c.call("note.write", { id: human.id, body: "line one\nline two\n", base_rev: 1 });
  ok(nw1.rev === 2, "note.write with the current revision");
  ok(await c.call("note.write", { id: human.id, body: "stale", base_rev: 1 }).then(() => false, (e) => /changed since revision 1/.test(String(e))),
    "a write from an old revision is refused");
  const listed = await mcp(fakeToken, "note_list");
  ok(listed.notes.some((n: Json) => n.id === human.id && n.rev === 2 && n.text === undefined), "MCP note_list (no text)");
  const read = await mcp(fakeToken, "note_read", { note_id: human.id });
  ok(read.text === "line one\nline two\n" && read.rev === 2, "MCP note_read");
  const app = await mcp(fakeToken, "note_append", { note_id: human.id, text: "agent line" });
  ok(app.rev === 3, "MCP note_append needs no revision");
  ok(await mcp(fakeToken, "note_write", { note_id: human.id, text: "x" }).then(() => false, (e) => String(e).includes("rev is required")),
    "MCP note_write needs rev to replace a note");
  ok(await mcp(fakeToken, "note_write", { note_id: human.id, text: "x", rev: 2 }).then(() => false, (e) => String(e).includes("changed since revision 2")),
    "MCP note_write from a stale rev is refused");
  const rewritten = await mcp(fakeToken, "note_write", { note_id: human.id, text: "rewritten\n", rev: 3 });
  ok(rewritten.rev === 4, "MCP note_write with the current rev");
  const made = await mcp(fakeToken, "note_write", { title: "Agent findings", text: "- a\n" });
  const agentNote = await c.call("note.get", { id: made.note_id });
  ok(agentNote.updated_by.kind === "agent" && /^Codex/.test(agentNote.updated_by.name), `the agent's note names it (${agentNote.updated_by.name})`);
  ok(c.events.some((e) => e.event === "note_upserted" && e.data.id === human.id && e.data.rev === 4 && e.data.body === "rewritten\n"),
    "note_upserted carries each change to clients");
  const big = "x".repeat(1024 * 1024);
  const bigNote = await c.call("note.create", { title: "Big", body: big });
  ok(await c.call("note.append", { id: bigNote.id, text: "y" }).then(() => false, (e) => /1 MiB/.test(String(e))), "notes are capped at 1 MiB");
  ok(await c.call("note.create", { title: "Too big", body: big + "y" }).then(() => false, () => true), "an oversized note is refused");
  await c.call("note.delete", { id: bigNote.id });
  // A full 1 MiB note whose every character is JSON-escaped still fits a request (CLI and MCP).
  const quoted = "\"".repeat(1024 * 1024);
  const viaHttp = await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
    method: "POST", headers: { Authorization: `Bearer ${d.token}`, "Content-Type": "application/json" },
    body: JSON.stringify({ id: 1, method: "note.create", params: { title: "Quotes", body: quoted } }),
  });
  const quotedNote = await viaHttp.json();
  ok(viaHttp.status === 200 && quotedNote.result?.body?.length === quoted.length, `a 1 MiB note fits POST /api/rpc (${viaHttp.status})`);
  const viaMcp = await mcp(fakeToken, "note_write", { note_id: quotedNote.result.id, text: quoted.slice(1), rev: 1 });
  ok(viaMcp.rev === 2, "and POST /mcp");
  await c.call("note.delete", { id: quotedNote.result.id });
  ok(await c.call("note.delete", { id: made.note_id, from_session: fake.id }).then(() => false, (e) => /only the human/.test(String(e))),
    "an agent's session can't delete a note");
  const opened = await mcp(fakeToken, "note_open", { note_id: made.note_id });
  ok(opened.window_id === 1 && (await c.call("note.get", { id: made.note_id })).beside === fake.id, "MCP note_open shows it beside the agent");
  const cliNote = await new Deno.Command(BIN, { args: ["note", "append", "Smoke note", "from", "the", "cli"], env: cliEnv0(), stdout: "piped", stderr: "piped" }).output();
  ok(cliNote.success && dec.decode(cliNote.stdout).includes("rev 5"), `muster note append (${dec.decode(cliNote.stderr)})`);
  const cliRead = await new Deno.Command(BIN, { args: ["note", "read", String(human.id)], env: cliEnv0(), stdout: "piped" }).output();
  ok(dec.decode(cliRead.stdout) === "rewritten\nfrom the cli", "muster note read prints the text");
  const cliLs = await new Deno.Command(BIN, { args: ["note", "ls"], env: cliEnv0(), stdout: "piped" }).output();
  ok(dec.decode(cliLs.stdout).includes("Agent findings"), "muster note ls");
  const noteRev = 5;

  await c.call("session.remove", { id: fake.id });
  await c.call("panel.event", { panel_id: pid, kind: "durable", payload: { persisted: true } });

  // GET /api/events: NDJSON, client token only, JSON events without terminal output. The CLI
  // (`muster events --json`) reads the same stream; it keeps running until the SIGKILL below.
  const eventsUrl = `http://127.0.0.1:${PORT}/api/events`;
  const noAuth = await fetch(eventsUrl);
  await noAuth.body?.cancel();
  ok(noAuth.status === 401, "/api/events needs the client token");
  const foreign = await fetch(eventsUrl, { headers: { Authorization: `Bearer ${d.token}`, Origin: "http://evil.test" } });
  await foreign.body?.cancel();
  ok(foreign.status === 403, "/api/events refuses a foreign Origin");
  const evAbort = new AbortController();
  const evResp = await fetch(eventsUrl, { headers: { Authorization: `Bearer ${d.token}` }, signal: evAbort.signal });
  ok(evResp.status === 200 && evResp.headers.get("content-type") === "application/x-ndjson", "/api/events streams NDJSON");
  const cliEnv = { ...ENV, MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "" };
  const evCli = new Deno.Command(BIN, { args: ["events", "--json"], env: cliEnv, stdout: "piped", stderr: "piped" }).spawn();
  const routeLines: string[] = [];
  const cliLines: string[] = [];
  const cliErr: string[] = [];
  const collect = async (s: ReadableStream<Uint8Array>, into: string[]) => {
    let buf = "";
    try {
      for await (const chunk of s.pipeThrough(new TextDecoderStream())) {
        buf += chunk;
        const parts = buf.split("\n");
        buf = parts.pop()!;
        into.push(...parts);
      }
    } catch {
      // aborted
    }
  };
  collect(evResp.body!, routeLines);
  collect(evCli.stdout, cliLines);
  collect(evCli.stderr, cliErr);
  const evCliDone = evCli.status;
  await sleep(1000); // let the CLI connect
  c.input(sid, "echo while-streaming\r");
  await c.call("session.rename", { id: sid, name: "evt-smoke" });
  const sawRename = (lines: string[]) =>
    lines.some((l) => {
      const e = JSON.parse(l);
      return e.event === "session_upserted" && e.data.id === sid && e.data.name === "evt-smoke";
    });
  await c.waitFor("rename event on /api/events", () => sawRename(routeLines));
  await c.waitFor("rename event from `muster events --json`", () => sawRename(cliLines));
  ok(true, "a state change arrives on /api/events and from `muster events --json`");
  ok(
    [...routeLines, ...cliLines].every((l) => /^\{"event":"(session|channel|window)_[a-z]+","data":/.test(l)),
    "the event stream carries only session/channel/window events (no output, notices or log entries)",
  );
  evAbort.abort();
  // `muster new -- ARGS`: arguments go to the agent (here the shell, after its -l).
  const newOut = await new Deno.Command(BIN, {
    args: ["new", REPO, "--agent", "shell", "--name", "extra", "--", "-c", "echo extra-$((40+2)); sleep 30"],
    env: cliEnv,
    stdout: "piped",
    stderr: "piped",
  }).output();
  const extraSid = Number(dec.decode(newOut.stdout).trim());
  ok(newOut.success && extraSid > 0, "muster new ... -- ARGS prints the new session id");
  let extraRan = false;
  for (let i = 0; i < 100 && !extraRan; i++, await sleep(100)) {
    const out = await c.call("session.output", { id: extraSid, lines: 20 });
    extraRan = (out.lines as string[]).some((l) => l.includes("extra-42"));
  }
  ok(extraRan, "the arguments after -- reached the agent command");
  const extraCh = (await c.call("hello")).sessions.find((x: Json) => x.id === extraSid).channel_id;
  const noneOut = await new Deno.Command(BIN, {
    args: ["new", REPO, "--kind", "none", "--", "-x"],
    env: cliEnv,
    stdout: "null",
    stderr: "piped",
  }).output();
  ok(!noneOut.success && dec.decode(noneOut.stderr).includes("need an agent"), "muster new --kind none -- ARGS is refused");
  await c.call("channel.archive", { id: extraCh, remove_worktree: false });

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

  // Phone replays (docs/PROTOCOL.md `attach_info`): a screen, then only the bytes it missed.
  const infos = (cl: Client) => cl.events.filter((e) => e.event === "attach_info").map((e) => e.data);
  const phone = new Client(d.token);
  await phone.open();
  await phone.call("session.attach", { id: sid, replay: "screen", replay_bytes: 65536 });
  await phone.waitFor("screen replay", () => phone.frames.some((f) => f.id === sid && f.type === 0x03));
  const shown = infos(phone)[0];
  ok(shown?.id === sid && shown.kind === "screen" && Number.isInteger(shown.offset) &&
    Number.isInteger(shown.generation) && typeof shown.epoch === "string", "a screen attach is announced by attach_info");
  c.input(sid, "echo third$((1+2))\r");
  await phone.waitFor("live output on the phone", () => phone.terminal(sid).includes("third3"));
  const reached = shown.offset +
    phone.frames.filter((f) => f.id === sid && f.type === 0x01).reduce((n, f) => n + f.data.length, 0);
  phone.close();
  c.input(sid, "echo missed$((2+2))\r");
  await c.waitFor("output while the phone is away", () => c.terminal(sid).includes("missed4"));
  await sleep(300);
  const back = new Client(d.token);
  await back.open();
  ok(Number.isInteger(shown.size_rev), "attach_info carries the terminal size revision");
  const since = { offset: reached, generation: shown.generation, epoch: shown.epoch, size_rev: shown.size_rev };
  await back.call("session.attach", { id: sid, replay: { since } });
  await back.waitFor("delta replay", () => back.frames.some((f) => f.id === sid && f.type === 0x03));
  const delta = back.frames.find((f) => f.id === sid && f.type === 0x03)!.data;
  const resumed = infos(back)[0];
  ok(resumed?.kind === "delta" && dec.decode(delta).includes("missed4") && !dec.decode(delta).includes("third3") &&
    resumed.offset === reached + delta.length, "a returning phone gets only the bytes it missed (delta)");
  await back.call("session.attach", { id: sid, replay: { since: { ...since, epoch: "another-daemon-run" } } });
  await back.waitFor("second attach_info", () => infos(back).length === 2);
  ok(infos(back)[1].kind === "screen", "an offset from another daemon run gets a screen");
  // A resize while away (here shrunk and restored, with nothing printed) can't be caught up by
  // appending: the host clipped what the narrower terminal couldn't hold.
  const position = (cl: Client) => {
    const i = infos(cl).at(-1);
    const last = cl.frames.findLastIndex((g) => g.id === sid && g.type === 0x03);
    const live = cl.frames.slice(last + 1).filter((f) => f.id === sid && f.type === 0x01);
    return { offset: i.offset + live.reduce((n, f) => n + f.data.length, 0), generation: i.generation, epoch: i.epoch,
      size_rev: i.size_rev };
  };
  const away = async (since: Json, n: number) => {
    await back.call("session.detach", { id: sid });
    await sleep(200);
    await back.call("session.attach", { id: sid, replay: { since } });
    await back.waitFor(`attach_info ${n}`, () => infos(back).length === n);
    return infos(back)[n - 1];
  };
  ok((await away(position(back), 3)).kind === "delta", "a phone that was away without a resize gets a delta");
  // A phone still on the bundle from before size_rev existed: attaches, gets a screen.
  const { size_rev: _unknown, ...oldShape } = position(back);
  ok((await away(oldShape, 4)).kind === "screen", "an old phone's since without size_rev gets a screen");
  const preResize = position(back);
  await c.call("session.resize", { id: sid, cols: 60, rows: 30 });
  await c.call("session.resize", { id: sid, cols: 100, rows: 30 });
  const afterResize = await away(preResize, 5);
  ok(afterResize.kind === "screen" && afterResize.size_rev === preResize.size_rev + 4,
    "a phone that was away during a resize gets a screen");
  // A run that ended while the phone was away replays as ended (a full-screen view moves into
  // history): never a delta, even with nothing printed since.
  const exitCh = await c.call("channel.create", { workspace_id: ws.id, name: "replay exit", agent: "shell" });
  await c.waitFor("exit-test shell", () => c.events.some((e) => e.event === "session_upserted" &&
    e.data.channel_id === exitCh.id && e.data.pid > 0));
  const exitSid = c.events.find((e) => e.event === "session_upserted" && e.data.channel_id === exitCh.id)!.data.id;
  const leaver = new Client(d.token);
  await leaver.open();
  await leaver.call("session.attach", { id: exitSid, replay: "screen" });
  await leaver.waitFor("exit-test screen", () => infos(leaver).length === 1 &&
    leaver.frames.some((f) => f.id === exitSid && f.type === 0x03));
  await sleep(500);
  const gone = infos(leaver)[0];
  const lastReplay = leaver.frames.findLastIndex((g) => g.id === exitSid && g.type === 0x03);
  const exitSince = { offset: gone.offset + leaver.frames.slice(lastReplay + 1).filter((f) => f.id === exitSid &&
    f.type === 0x01).reduce((n, f) => n + f.data.length, 0), generation: gone.generation, epoch: gone.epoch,
    size_rev: gone.size_rev };
  leaver.close();
  await c.call("session.kill", { id: exitSid });
  await c.waitFor("exit-test exit", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === exitSid &&
    e.data.state === "exited"));
  const returner = new Client(d.token);
  await returner.open();
  await returner.call("session.attach", { id: exitSid, replay: { since: exitSince } });
  await returner.waitFor("exit-test attach_info", () => infos(returner).length === 1);
  ok(infos(returner)[0].kind === "screen", "a phone that was away while the run ended gets a screen");
  returner.close();
  await c.call("channel.archive", { id: exitCh.id, remove_worktree: false });
  ok(JSON.stringify(await back.call("ping")) === "{}", "ping answers");
  back.close();

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
  // session.move: the shell leaves "My Feature" for a fresh auto-named tab, whose name then
  // comes from the shell's title. Same channel is a no-op; archived/unknown channels are refused.
  const dst = await c.call("channel.create", { workspace_id: ws.id, window_id: 1 });
  ok(dst.auto_named && dst.name === "repo", "empty auto-named tab for session.move");
  c.events = [];
  const moved = await c.call("session.move", { id: bs.id, channel_id: dst.id });
  ok(moved.id === bs.id && moved.channel_id === dst.id && moved.pid === bs.pid, "session.move keeps the process");
  ok(c.events.some((e) => e.event === "session_upserted" && e.data.id === bs.id && e.data.channel_id === dst.id), "session_upserted with the new channel_id");
  await c.waitFor("moved title names the tab", () =>
    c.events.some((e) => e.event === "channel_upserted" && e.data.id === dst.id && e.data.name === "other title"));
  ok(true, "the moved shell's title names its new tab");
  ok((await c.call("hello")).sessions.find((x: Json) => x.id === bs.id).channel_id === dst.id, "session.move persisted (hello)");
  ok((await c.call("session.move", { id: bs.id, channel_id: dst.id })).channel_id === dst.id, "session.move to its own channel is a no-op");
  await c.call("session.move", { id: bs.id, channel_id: plain.id }).then(
    () => ok(false, "session.move refuses an archived channel"),
    (e) => ok(String(e).includes("archived"), "session.move refuses an archived channel"),
  );
  await c.call("session.move", { id: bs.id, channel_id: 99999 }).then(
    () => ok(false, "session.move refuses an unknown channel"),
    (e) => ok(String(e).includes("no such channel"), "session.move refuses an unknown channel"),
  );
  await c.call("session.move", { id: bs.id, channel_id: bare.id });
  await c.call("channel.archive", { id: dst.id, remove_worktree: false });
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

  // ----- groups layout (v2): a tab is a session in a window -----
  const layout = { type: "pane", id: "g1", tabs: [sid], active: sid };
  // JSON with sorted keys: the daemon keeps the layout's content, not its key order.
  const canon = (v: unknown): string =>
    JSON.stringify(v, (_, x) => x && typeof x === "object" && !Array.isArray(x) ? Object.fromEntries(Object.entries(x).sort()) : x);
  c.events = [];
  ok(JSON.stringify(await c.call("window.set_layout", { id: 1, layout })) === "{}", "window.set_layout returns {}");
  ok(canon((await c.call("hello")).windows.find((w: Json) => w.id === 1).layout) === canon(layout), "hello carries the window layout");
  ok(!c.events.some((e) => e.event === "window_upserted"), "window.set_layout has no event echo");
  await c.call("window.set_layout", { id: 99, layout }).then(
    () => ok(false, "window.set_layout rejects an unknown window"),
    (e) => ok(String(e).includes("no such window"), "window.set_layout rejects an unknown window"),
  );
  const task = await c.call("channel.create", { workspace_id: ws.id, agent: "shell", window_id: 1 });
  await c.waitFor("task session", () => c.events.some((e) => e.event === "session_upserted" && e.data.channel_id === task.id));
  const t1 = c.events.find((e) => e.event === "session_upserted" && e.data.channel_id === task.id)!.data;
  const t2 = await c.call("session.spawn", { channel_id: task.id, kind: "shell" });
  ok(t1.window_id === 1 && t2.window_id === 1, "sessions start in their task's window");
  const side = await c.call("window.create", {});
  c.events = [];
  const sw = await c.call("session.set_window", { id: t1.id, window_id: side.id, from_window_id: 1 });
  ok(sw.window_id === side.id && sw.channel_id === task.id && sw.pid === t1.pid, "session.set_window moves the tab, same task and process");
  ok(c.events.some((e) => e.event === "session_upserted" && e.data.id === t1.id && e.data.window_id === side.id), "session.set_window emits session_upserted");
  ok((await c.call("hello")).channels.find((x: Json) => x.id === task.id).window_id === 1, "the task stays while one of its tabs is left behind");
  await c.call("session.set_window", { id: t1.id, window_id: 1, from_window_id: 1 }).then(
    () => ok(false, "session.set_window refuses a stale from_window_id"),
    (e) => ok(String(e).includes("no longer in window 1"), "session.set_window refuses a stale from_window_id"),
  );
  await c.call("session.set_window", { id: t1.id, window_id: 99 }).then(
    () => ok(false, "session.set_window rejects an unknown window"),
    (e) => ok(String(e).includes("no such window"), "session.set_window rejects an unknown window"),
  );
  // `muster move`: the other tab follows; now all of the task's tabs are there, so is the task.
  const mvOut = await new Deno.Command(BIN, { args: ["move", String(t2.id), "--window", String(side.id)], env: cliEnv, stdout: "piped", stderr: "piped" }).output();
  ok(mvOut.success && dec.decode(mvOut.stdout).includes(`window 1 -> ${side.id}`), "muster move --window");
  await c.waitFor("task follows its tabs", () =>
    c.events.some((e) => e.event === "channel_upserted" && e.data.id === task.id && e.data.window_id === side.id));
  ok(true, "the task follows once all its tabs are in the new window");
  const lsJson = await new Deno.Command(BIN, { args: ["ls", "--json"], env: cliEnv, stdout: "piped" }).output();
  ok(JSON.parse(dec.decode(lsJson.stdout)).find((x: Json) => x.id === t2.id)?.window_id === side.id, "muster ls --json shows the session's window");
  const t3 = await c.call("session.spawn", { channel_id: task.id, kind: "shell", window_id: 1 });
  ok(t3.window_id === 1, "session.spawn with window_id");
  // A task closes with its last session; one created without an agent stays until then.
  const empty = await c.call("channel.create", { workspace_id: ws.id, window_id: 1 });
  c.events = [];
  for (const x of [t1, t2]) await c.call("session.remove", { id: x.id });
  ok(!c.events.some((e) => e.event === "channel_removed" && e.data.id === task.id), "the task stays while it has a session");
  await c.waitFor(`window ${side.id} removed with its last tab`, () => c.events.some((e) => e.event === "window_removed" && e.data.id === side.id));
  ok(true, "an unheld window loses its last tab: deleted");
  await c.call("session.remove", { id: t3.id });
  await c.waitFor("task closed", () => c.events.some((e) => e.event === "channel_removed" && e.data.id === task.id));
  ok(true, "removing a task's last session closes the task");
  ok((await c.call("hello")).channels.some((x: Json) => x.id === empty.id), "a task without sessions isn't closed by that");
  const es = await c.call("session.spawn", { channel_id: empty.id, kind: "shell" });
  await c.call("session.remove", { id: es.id });
  await c.waitFor("empty task closed", () => c.events.some((e) => e.event === "channel_removed" && e.data.id === empty.id));
  ok(true, "...only once a session of it is removed");

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

  // --- backend crash: the host keeps the same processes and replay alive
  const beforeCrash = (await c.call("hello")).sessions.find((x: Json) => x.id === sid);
  c.close();
  console.log("SIGKILL daemon, restart");
  await sleep(3500); // let the periodic flush persist output.log
  await stopDaemon(d, "SIGKILL");
  const evStatus = await evCliDone;
  await sleep(100);
  ok(evStatus.code === 0 && cliErr.join("").includes("daemon stopped"), "`muster events` exits 0 when the daemon goes away");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  let h = await c.call("hello");
  let s = h.sessions.find((x: Json) => x.id === sid);
  ok(s.state !== "exited" && s.pid === beforeCrash.pid, "after backend crash: original terminal process survives");
  ok(h.windows.some((w: Json) => w.id === 1 && !w.open && w.channel_ids.includes(ch.id)), "windows persist across restarts");
  ok(h.panels.some((p: Json) => p.id === pid && p.revision === 2), "panel survives backend restart");
  const keptNote = h.notes.find((n: Json) => n.id === human.id);
  ok(keptNote?.rev === noteRev && keptNote.body === undefined && keptNote.preview === "rewritten",
    "hello.notes are summaries (no text)");
  ok((await c.call("note.get", { id: human.id })).body === "rewritten\nfrom the cli" && h.notes.some((n: Json) => n.id === made.note_id && n.window_id === 1),
    "notes (text, revision, window) survive a backend restart");
  ok((await mcp(shellToken, "wait_for_panel_event", { panel_id: pid, timeout_seconds: 0 })).events[0].kind === "durable", "queued shell panel event survives backend restart");
  ok(canon(h.windows.find((w: Json) => w.id === 1).layout) === canon(layout) && s.window_id === 1, "window layouts and session windows persist");
  await c.call("session.attach", { id: sid });
  await c.waitFor("replay after restart", () => c.frames.some((f) => f.id === sid && f.type === 0x03));
  const replayed = c.terminal(sid);
  // 2 MiB of output plus a short terminal-mode prefix.
  ok(replayed.includes(marker) && replayed.length <= 2 * 1024 * 1024 + 256, "surviving session replays host output (capped at 2 MiB)");

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

  // --- clean backend shutdown also preserves terminals
  const beforeCleanStop = (await c.call("hello")).sessions.find((x: Json) => x.id === sid);
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
  ok(s.state !== "exited" && s.pid === beforeCleanStop.pid, "after clean backend stop: original terminal process survives");

  // archive with worktree removal (branch kept)
  await c.call("channel.archive", { id: ch.id, remove_worktree: true });
  ok((await panelRow()) === null && !(await exists(panelDir)), "archive removes panel rows, events and files");
  ok(!(await exists(ch.cwd)), "archive removed the worktree dir");
  ok((await git(REPO, "branch", "--list", "muster/smoke-test")).includes("muster/smoke-test"), "branch kept after archive");
  h = await c.call("hello");
  ok(!h.channels.some((x: Json) => x.id === ch.id) && !h.sessions.some((x: Json) => x.id === sid), "archived channel gone from hello");
  await c.call("workspace.remove", { id: ws.id });
  h = await c.call("hello");
  ok(h.workspaces.length === 0 && h.channels.length === 0, "workspace.remove");

  // agents: records, home dirs and the origin rule (no agent is started)
  const ada = await c.call("agent.create", { name: "Ada", kind: "claude", persona: "Reviews diffs." });
  ok(ada.accepts_origins.join() === "human" && ada.home_dir === `${DATA}/agents/ada`, "agent.create: a claude agent accepts human only");
  ok(await exists(`${DATA}/agents/ada/CLAUDE.md`) && await exists(`${DATA}/agents/ada/memory`), "agent home: CLAUDE.md and memory/");
  ok((await Deno.readTextFile(`${DATA}/agents/ada/CLAUDE.md`)).includes("Reviews diffs."), "persona written to CLAUDE.md");
  const cody = await c.call("agent.create", { name: "cody", kind: "codex" });
  ok(cody.accepts_origins.join() === "human,schedule,webhook" && await exists(`${DATA}/agents/cody/AGENTS.md`), "a codex agent accepts every origin, AGENTS.md");
  await c.call("agent.create", { name: "ADA" }).then(
    () => ok(false, "agent names are unique case-insensitively"),
    (e) => ok(String(e).includes("already exists"), "agent names are unique case-insensitively"),
  );
  // (Asking a real agent would start its agent CLI: only the refusal paths run here.)
  await c.call("agent.ask", { agent: "nobody", text: "hi" }).then(
    () => ok(false, "agent.ask: unknown agent"),
    (e) => ok(String(e).includes("no agent named nobody"), "agent.ask: unknown agent"),
  );
  await c.call("agent.ask", { agent: "ada", text: "hi", from_session: 999999 }).then(
    () => ok(false, "agent.ask: unknown from_session refused"),
    (e) => ok(String(e).includes("no such session"), "agent.ask: unknown from_session refused"),
  );
  ok((await c.call("agent.list")).length === 2, "agent.list");
  // Panel backend: edit, templates, memory, the message log and live events.
  const ed = await c.call("agent.update", {
    name: "ada",
    persona: "Reviews diffs, briefly.",
    autostart: true,
    permissions: { claude_mode: "plan", codex_sandbox: "workspace_write" },
  });
  ok(ed.autostart === true && ed.permissions.claude_mode === "plan", "agent.update: autostart and permissions");
  ok((await Deno.readTextFile(`${DATA}/agents/ada/CLAUDE.md`)).includes("Reviews diffs, briefly."), "agent.update rewrites the persona file");
  await c.waitFor("agent_upserted event", () => c.events.some((e) => e.event === "agent_upserted" && e.data.name === "Ada" && e.data.autostart));
  ok(true, "agent_upserted reaches clients");
  const tpl = await c.call("agent.templates");
  ok(tpl.map((t: Json) => t.id).join() === "blank,orchestrator,researcher,reviewer,pr_reviewer,worker", "agent.templates");
  await c.call("agent.memory.write", { name: "ada", path: "people/me.md", text: "likes short reviews" });
  ok((await Deno.readTextFile(`${DATA}/agents/ada/memory/people/me.md`)) === "likes short reviews", "agent.memory.write");
  ok((await c.call("agent.memory.read", { name: "ada", path: "people/me.md" })).text === "likes short reviews", "agent.memory.read");
  ok((await c.call("agent.memory.list", { name: "ada" })).files.some((f: Json) => f.path === "people/me.md"), "agent.memory.list");
  await c.call("agent.memory.read", { name: "ada", path: "../CLAUDE.md" }).then(
    () => ok(false, "agent.memory.read refuses paths outside memory/"),
    (e) => ok(String(e).includes(".."), "agent.memory.read refuses paths outside memory/"),
  );
  const log = await c.call("agent.messages", { limit: 10 });
  ok(Array.isArray(log.messages) && log.more === false, "agent.messages");
  await c.call("agent.remove", { name: "ada" });
  await c.call("agent.remove", { name: "cody" });
  ok((await c.call("agent.list")).length === 0 && await exists(`${DATA}/agents/ada/CLAUDE.md`), "agent.remove keeps the home dir");

  // A named agent's closed tab is hidden, not stopped (the fake Codex stands in for the agent).
  await c.call("agent.create", { name: "bg", kind: "codex" });
  const started = await c.call("agent.start", { name: "bg" });
  const agentSid: number = started.session.id;
  const agentCh: number = started.session.channel_id;
  const sessOf = async (id: number) => (await c.call("hello")).sessions.find((x: Json) => x.id === id);
  await c.waitFor("fake agent idle", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === agentSid && e.data.state === "idle"));

  // Codex resume: a finished turn's thread is reopened as `codex resume <flags> -- <thread>` on
  // the next start, for a plain Codex tab and a named Codex agent alike; a thread Codex no
  // longer has ends that run, says so, and the next start is a new conversation.
  const agentToken = await Deno.readTextFile(`${BASE}/fake-token`);
  /** The launch `act` makes of session `id`, its token, and the events from `act` on. */
  const codexLaunch = async (id: number, act: () => Promise<unknown>) => {
    await Deno.remove(`${BASE}/launch-codex-${id}`).catch(() => {});
    const mark = c.events.length;
    await act();
    const args = (await launchOf("codex", id)).args;
    const since = () => c.events.slice(mark).filter((e) => e.event === "session_upserted" && e.data.id === id).map((e) => e.data);
    return { args, token: await Deno.readTextFile(`${BASE}/fake-token`), since };
  };
  const turnDone = async (id: number, token: string, thread: string) => {
    const r = await fetch(`http://127.0.0.1:${PORT}/hook/notify`, {
      method: "POST", headers: { Authorization: `Bearer ${token}` },
      body: JSON.stringify({ type: "agent-turn-complete", "thread-id": thread, "input-messages": ["hi"], "last-assistant-message": `done ${thread}` }),
    });
    ok(r.ok, `codex resume: notify for ${thread} accepted`);
    await c.waitFor(`notify ${thread}`, () => c.events.some((e) => e.event === "session_upserted" && e.data.id === id && e.data.state_detail === `done ${thread}`));
  };
  const codexTab: number = (await c.call("session.spawn", { channel_id: agentCh, kind: "codex", extra_args: ["-m", "gpt-smoke"] })).id;
  const firstArgs = (await launchOf("codex", codexTab)).args;
  ok(!firstArgs.startsWith("ARGS resume") && !firstArgs.includes(" -- "), `codex resume: a new tab starts a new conversation: ${firstArgs.slice(0, 80)}`);
  await c.waitFor("codex tab idle", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === codexTab && e.data.state === "idle"));
  await turnDone(codexTab, await Deno.readTextFile(`${BASE}/fake-token`), "thread-tab-1");
  const tab = await codexLaunch(codexTab, () => c.call("session.restart", { id: codexTab }));
  ok(
    tab.args.startsWith("ARGS resume -c notify=") && tab.args.endsWith(" -- thread-tab-1") && tab.args.includes(" --sandbox ") &&
      tab.args.includes(" -m gpt-smoke ") && !tab.args.includes("--resume"),
    `codex resume: a restarted tab reopens its thread with its flags: ${tab.args.slice(0, 40)}…${tab.args.slice(-60)}`,
  );
  await c.call("session.remove", { id: codexTab });
  // The agent: its thread is on its record, so a restart (and agent.start) reopens it.
  await turnDone(agentSid, agentToken, "thread-agent-1");
  ok((await c.call("agent.list")).find((a: Json) => a.name === "bg").resumable, "codex resume: the agent keeps its thread");
  let ag = await codexLaunch(agentSid, () => c.call("session.restart", { id: agentSid }));
  ok(ag.args.startsWith("ARGS resume ") && ag.args.endsWith(" -- thread-agent-1"), `codex resume: a restarted agent reopens its thread: ${ag.args.slice(-60)}`);
  await c.waitFor("resumed agent idle", () => ag.since().some((s) => s.state === "idle"));
  // A sub-agent's turn (its own thread) never replaces the agent's.
  const child = await fetch(`http://127.0.0.1:${PORT}/hook/notify`, {
    method: "POST", headers: { Authorization: `Bearer ${ag.token}` },
    body: JSON.stringify({ type: "agent-turn-complete", "thread-id": "thread-child", "input-messages": ["sub task"], "last-assistant-message": "child done" }),
  });
  ok(child.ok, "codex resume: a sub-agent's notify accepted");
  ag = await codexLaunch(agentSid, () => c.call("session.restart", { id: agentSid }));
  ok(ag.args.endsWith(" -- thread-agent-1"), `codex resume: a sub-agent's turn didn't replace the thread: ${ag.args.slice(-40)}`);
  // Any other failure (here: Codex's database can't open) keeps the thread.
  await Deno.writeTextFile(`${BASE}/codex-broken`, "");
  ag = await codexLaunch(agentSid, () => c.call("session.restart", { id: agentSid }));
  await c.waitFor("broken run ends", () => ag.since().some((s) => s.state === "exited" && s.exit_code === 1));
  await Deno.remove(`${BASE}/codex-broken`);
  ok(!(await sessOf(agentSid)).state_detail?.includes("couldn't reopen"), "codex resume: an unrelated exit 1 says nothing about the conversation");
  ok((await c.call("agent.list")).find((a: Json) => a.name === "bg").resumable, "codex resume: an unrelated exit 1 keeps the thread");
  // Start fresh, then a thread Codex no longer has: that run ends, the tab says so, the next
  // start is new.
  ag = await codexLaunch(agentSid, () => c.call("agent.start", { name: "bg", fresh: true }));
  ok(!ag.args.startsWith("ARGS resume"), `codex resume: Start fresh starts a new conversation: ${ag.args.slice(0, 40)}`);
  await c.waitFor("fresh agent idle", () => ag.since().some((s) => s.state === "idle"));
  await turnDone(agentSid, ag.token, "stale-thread");
  ag = await codexLaunch(agentSid, () => c.call("session.restart", { id: agentSid }));
  ok(ag.args.endsWith(" -- stale-thread"), "codex resume: the restart tries the stale thread");
  await c.waitFor("stale resume ends", () => ag.since().some((s) => s.state === "exited" && s.exit_code === 1));
  const failed = await sessOf(agentSid);
  ok(failed.state_detail?.includes("couldn't reopen its conversation"), `codex resume: the tab says the conversation couldn't be reopened: ${failed.state_detail}`);
  ok(!(await c.call("agent.list")).find((a: Json) => a.name === "bg").resumable, "codex resume: the agent forgets the stale thread");
  ag = await codexLaunch(agentSid, () => c.call("agent.start", { name: "bg" }));
  ok(!ag.args.startsWith("ARGS resume") && !ag.args.includes("stale-thread"), `codex resume: the next start is a new conversation: ${ag.args.slice(0, 60)}`);
  await c.waitFor("fresh agent idle", () => ag.since().some((s) => s.state === "idle"));

  const plainTab = await c.call("session.spawn", { channel_id: agentCh, kind: "shell" });
  await c.call("session.hide", { id: plainTab.id }).then(
    () => ok(false, "session.hide refuses a tab that isn't an agent's"),
    (e) => ok(String(e).includes("not a named agent"), "session.hide refuses a tab that isn't an agent's"),
  );
  await c.call("session.remove", { id: plainTab.id });
  ok(!(await sessOf(plainTab.id)), "an ordinary tab still closes for real");
  const pidBefore = (await sessOf(agentSid)).pid;
  const hid = await c.call("session.hide", { id: agentSid });
  h = await c.call("hello");
  s = h.sessions.find((x: Json) => x.id === agentSid);
  ok(hid.hidden && s.hidden && s.window_id === null && s.state !== "exited" && s.pid === pidBefore, "a hidden agent tab keeps running");
  ok(h.channels.some((x: Json) => x.id === agentCh && x.window_id === null) && !h.windows.some((w: Json) => w.channel_ids.includes(agentCh)), "its task is in no window");
  await c.call("session.send", { id: agentSid, text: "hello while hidden" });
  let got = "";
  for (let n = 0; n < 100; n++) { got = await Deno.readTextFile(`${BASE}/fake-input`); if (got.includes("hello while hidden")) break; await sleep(100); }
  ok(got.includes("hello while hidden"), "a hidden agent still gets messages");
  const startAgain = await c.call("agent.start", { name: "bg" });
  ok(startAgain.started === false && startAgain.session.id === agentSid && startAgain.session.hidden, "agent.start reports the hidden agent running, leaves it hidden");
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  s = await sessOf(agentSid);
  ok(s.hidden && s.window_id === null && s.pid === pidBefore, "still hidden and running after a backend restart");
  const win = await c.call("window.create", {});
  s = await c.call("session.set_window", { id: agentSid, window_id: win.id });
  ok(!s.hidden && s.window_id === win.id && (await c.call("hello")).channels.find((x: Json) => x.id === agentCh).window_id === win.id, "session.set_window shows it again, with its task");
  const removed = await c.call("session.remove", { id: agentSid });
  s = await sessOf(agentSid);
  ok(removed.hidden && removed.session.hidden && s.hidden && s.state !== "exited" && s.pid === pidBefore,
    "session.remove of an agent's tab hides it, never stops it");
  await c.call("session.kill", { id: agentSid });
  await c.waitFor("agent stopped", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === agentSid && e.data.state === "exited"));
  ok(!!(await sessOf(agentSid)), "Stop (session.kill) stops it; its tab stays");
  await c.call("agent.remove", { name: "bg" });

  ({ c, d } = await schedules(c, d));
  ({ c, d } = await triggers(c, d));
  ({ c, d } = await agentSchedules(c, d));
  ({ c, d } = await phoneAlerts(c, d));
  await openRouter(c, d);
  console.log(`\nPASS (${passed} checks)`);
}

/** The fake CLI's launch record for session `id` (waits until it is written). */
async function launchOf(cli: string, id: number): Promise<{ args: string; env: Record<string, string> }> {
  const path = `${BASE}/launch-${cli}-${id}`;
  for (let n = 0; n < 100 && !(await exists(path)); n++) await sleep(100);
  await sleep(100);
  const [first, ...rest] = (await Deno.readTextFile(path)).split("\n");
  const env: Record<string, string> = {};
  for (const line of rest.filter((l) => l)) env[line.slice(0, line.indexOf("="))] = line.slice(line.indexOf("=") + 1);
  return { args: first, env };
}

/** Every file under `dir` (recursively) that contains `needle`. */
async function filesContaining(dir: string, needle: string): Promise<string[]> {
  const out: string[] = [];
  const bytes = enc.encode(needle);
  const has = (hay: Uint8Array) => {
    outer: for (let i = 0; i + bytes.length <= hay.length; i++) {
      for (let j = 0; j < bytes.length; j++) if (hay[i + j] !== bytes[j]) continue outer;
      return true;
    }
    return false;
  };
  for await (const e of Deno.readDir(dir)) {
    const p = `${dir}/${e.name}`;
    if (e.isDirectory) out.push(...await filesContaining(p, needle));
    else if (e.isFile && has(await Deno.readFile(p))) out.push(p);
  }
  return out;
}

const cli = (args: string[], env: Record<string, string>) =>
  new Deno.Command(BIN, { args, env, stdin: "null", stdout: "piped", stderr: "piped" }).output();

/** Schedules (src/schedules/): made and listed through the CLI, a run is an agent message with
 * origin schedule, refusals, pause and skip, the busy rule, the reply in a note, and the missed-run
 * rule across a backend restart (a debug-build clock offset stands in for two days off). Returns
 * the client and backend it ends with. */
async function schedules(c: Client, d: Daemon): Promise<{ c: Client; d: Daemon }> {
  const env = { ...ENV, MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "" };
  const sch = async (...args: string[]) => {
    const o = await cli(["schedule", ...args], env);
    return { ok: o.success, out: dec.decode(o.stdout), err: dec.decode(o.stderr) };
  };
  const input = () => Deno.readTextFile(`${BASE}/fake-input`).catch(() => "");
  const until = async (what: string, cond: () => Promise<boolean>, ms = 15000) => {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      if (await cond()) return;
      await sleep(100);
    }
    throw new Error(`timeout waiting for ${what}`);
  };
  const runs = async (id: number) => (await c.call("schedule.runs", { id, limit: 50 })).runs as Json[];

  // A Claude agent that doesn't take scheduled work is refused, through the CLI too.
  await c.call("agent.create", { name: "nosched", kind: "claude" });
  const refused = await sch("add", "nosched", "--daily", "09:00", "hello");
  ok(!refused.ok && refused.err.includes("doesn't take scheduled work") && refused.err.includes("Codex or OpenRouter"),
    `schedule: a Claude agent without the Schedule origin is refused (${refused.err.trim()})`);
  await c.call("agent.create", { name: "sched", kind: "codex" });
  const fast = await sch("add", "sched", "--cron", "*/5 * * * *", "too often");
  ok(!fast.ok && fast.err.includes("every 15 minutes"), "schedule: faster than every 15 minutes is refused");
  const add = await sch("add", "sched", "--weekdays", "09:00", "--tz", "Europe/London", "--note", "Sched replies",
    "Summary for {date}, since {last_run}");
  ok(add.ok && /schedule \d+: At 09:00, Monday to Friday \(Europe\/London\); next /.test(add.out), `muster schedule add (${add.out.trim()}${add.err})`);
  const id = Number(add.out.match(/schedule (\d+)/)![1]);
  const ls = JSON.parse((await sch("ls", "--json")).out);
  const mine = ls.schedules.find((s: Json) => s.id === id);
  ok(mine && mine.agent === "sched" && mine.status === "active" && mine.next_run > Date.now() && mine.quiet_hours === false &&
    mine.expires_at === null && mine.note_title === "Sched replies", "muster schedule ls --json: the schedule, Codex: no expiry, quiet hours off");
  ok((await sch("ls")).out.includes("Summary for {date}"), "muster schedule ls: a table row");
  await c.waitFor("schedule_upserted", () => c.events.some((e) => e.event === "schedule_upserted" && e.data.id === id));
  ok(true, "schedule_upserted reaches clients");

  // Run now: an agent message from schedule:<id>, origin schedule, hop 0, prompt filled in.
  const ran = await sch("run", String(id));
  ok(ran.ok && ran.out.includes("started"), `muster schedule run (${ran.out.trim()}${ran.err})`);
  await until("the run's prompt", async () => (await input()).includes(`from schedule:${id} origin=schedule hop=0]`));
  const typed = await input();
  ok(typed.includes("since never (this is the first run)") && /Summary for \d{4}-\d\d-\d\d/.test(typed),
    "the run reaches the agent as an envelope with origin schedule, placeholders filled");
  const token = await Deno.readTextFile(`${BASE}/fake-token`);
  const agentSession: number = (await c.call("agent.list")).find((a: Json) => a.name === "sched").session_id;
  let r = (await runs(id))[0];
  ok(r.kind === "manual" && r.outcome === "pending" && r.message?.origin === "schedule" && r.message.hop === 0 &&
    r.message.from === `schedule:${id}`, "history: a pending manual run with its message");

  // Busy: another run while that one waits for its reply is skipped.
  await c.call("schedule.run", { id });
  await until("the busy skip", async () => (await runs(id))[0].outcome === "skipped");
  ok((await runs(id))[0].reason.includes("previous run is still going"), "busy: a run is skipped while the previous one is going");

  // The reply (Stop hook): the run is replied, and the reply lands in the note. The prompt is
  // pasted before Enter is pressed (and the message attributed): give it a moment.
  await sleep(1500);
  const hook = await fetch(`http://127.0.0.1:${PORT}/hook/Stop`, {
    method: "POST", headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify({ last_assistant_message: "3 PRs open" }),
  });
  ok(hook.ok, "Stop hook accepted");
  await until("the replied run", async () => (await runs(id)).some((x) => x.outcome === "replied"));
  r = (await runs(id)).find((x) => x.outcome === "replied")!;
  ok(r.message.reply === "3 PRs open" && r.finished_at >= r.started_at, "history: replied, with the reply");
  const notes = await c.call("note.list");
  const note = await c.call("note.get", { id: notes.find((n: Json) => n.title === "Sched replies").id });
  ok(/^## \w{3} \d+ \w{3} \d{4} \d\d:\d\d · Summary for \{date\}, since \{last_run\}\n\n3 PRs open\n$/.test(note.body) &&
    note.updated_by.name === "sched", `the reply is appended to the note under a date heading (${JSON.stringify(note.body)})`);
  const hist = await sch("history", String(id));
  ok(hist.ok && hist.out.includes("replied") && hist.out.includes("skipped") && hist.out.includes("previous run is still going"),
    "muster schedule history");
  await c.waitFor("schedule_run event", () => c.events.some((e) => e.event === "schedule_run" && e.data.outcome === "replied"));
  ok(true, "schedule_run events reach clients");

  // The second run's {last_run} names the first.
  await c.call("schedule.run", { id });
  await until("the second prompt", async () => /since \d{4}-\d\d-\d\d \d\d:\d\d/.test(await input()));
  ok(true, "{last_run} names the previous run");
  await sleep(1500);
  await fetch(`http://127.0.0.1:${PORT}/hook/Stop`, {
    method: "POST", headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify({ last_assistant_message: "still 3" }),
  });
  await until("the second reply", async () => (await runs(id))[0].outcome === "replied");

  // A shell tab an agent spawned (or typed into) carries the agent's chain: the CLI there (which
  // sends that shell's session id) can't make or change schedules; the agent itself can't either.
  const spawnRes = await fetch(`http://127.0.0.1:${PORT}/mcp`, {
    method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "spawn_session", arguments: { kind: "shell" } } }),
  });
  const spawned = await spawnRes.json();
  ok(spawnRes.ok && !spawned.result?.isError, `the agent spawns a shell (${JSON.stringify(spawned).slice(0, 200)})`);
  const spawnOut = JSON.parse(spawned.result.content[0].text);
  const sh = { id: Number(spawnOut.session_id ?? spawnOut.session?.id ?? spawnOut.id) };
  const fromShell = await cli(["schedule", "add", "sched", "--daily", "09:00", "more work"], { ...env, MUSTER_SESSION_ID: String(sh.id) });
  ok(!fromShell.success && dec.decode(fromShell.stderr).includes("a shell an agent has typed into"),
    "schedule changes from a shell an agent typed into are refused");
  const fromAgent = await cli(["schedule", "resume", "--all"], { ...env, MUSTER_SESSION_ID: String(agentSession) });
  ok(!fromAgent.success && dec.decode(fromAgent.stderr).includes("agent's session"), "and from the agent's own session");
  ok((await sch("ls")).ok, "reading schedules is still fine");
  await c.call("session.kill", { id: sh.id });

  // Pause, resume, skip next.
  ok((await sch("pause", String(id))).out.includes("paused"), "muster schedule pause");
  ok((await c.call("schedule.list")).schedules.find((s: Json) => s.id === id).paused, "paused shows in schedule.list");
  ok((await sch("resume", String(id))).out.includes("active"), "muster schedule resume");
  ok((await sch("skip", String(id))).out.includes("skipping the run at"), "muster schedule skip");
  ok((await c.call("schedule.list")).schedules.find((s: Json) => s.id === id).skip_next === true, "skip_next is set");
  await sch("skip", String(id), "--undo");
  ok((await c.call("schedule.list")).schedules.find((s: Json) => s.id === id).skip_next === false, "skip --undo");
  ok((await sch("pause", "--all")).ok && (await c.call("schedule.settings.get")).paused === true, "muster schedule pause --all");
  await sch("resume", "--all");

  // The unattended cap: two schedule messages already today; at a cap of 2 the next is refused.
  await c.call("schedule.settings.set", { unattended_cap: 2 });
  await c.call("schedule.run", { id });
  await until("the refused run", async () => (await runs(id))[0].outcome === "refused");
  ok((await runs(id))[0].reason.includes("unattended"), "a run past the unattended cap is refused, with the reason");
  const refusedMsgs = (await c.call("agent.messages", { agent: "sched" })).messages.filter((m: Json) => m.refused);
  ok(refusedMsgs.some((m: Json) => m.refused.includes("unattended") && m.origin === "schedule"), "the refusal is in the agent's message log");
  await c.call("schedule.settings.set", { unattended_cap: 24 });
  await c.call("agent.remove", { name: "nosched" });

  // Missed runs across a restart: Muster is "off" for two days (a test clock offset), landing at
  // half past an hour. The daily schedule runs once as a catch-up; the hourly one skips.
  await sch("rm", String(id));
  const made = async (...args: string[]) => {
    const o = await sch("add", "sched", ...args);
    ok(o.ok, `muster schedule add ${args[0]} (${o.out.trim()}${o.err.trim()})`);
    return Number(o.out.match(/schedule (\d+)/)![1]);
  };
  const daily = await made("--daily", "09:00", "daily catch-up check");
  const hourly = await made("--hourly", "hourly check");
  const minute = new Date().getMinutes();
  const offset = 2 * 86_400_000 + ((90 - minute) % 60) * 60_000;
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon({ MUSTER_TEST_CLOCK_OFFSET_MS: String(offset), MUSTER_TEST_CATCHUP_GAP_MS: "300" });
  c = new Client(d.token);
  await c.open();
  await until("the catch-up", async () => (await runs(daily)).some((x) => x.kind === "catch_up" && x.msg_id));
  const cu = (await runs(daily))[0];
  ok(cu.kind === "catch_up" && cu.reason.startsWith("catch-up") && (await input()).includes("daily catch-up check"),
    "after downtime a daily schedule runs once, as a catch-up");
  ok((await runs(daily)).length === 1, "once, not once per missed day");
  await until("the hourly skip", async () => (await runs(hourly)).length > 0);
  const hs = await runs(hourly);
  ok(hs.length === 1 && hs[0].outcome === "skipped" && hs[0].reason.startsWith("missed"), `an hourly schedule skips missed runs (${hs[0].reason})`);
  ok((await c.call("schedule.list")).schedules.every((s: Json) => s.next_run > Date.now() + offset - 60_000),
    "next runs move past the downtime");
  await c.call("agent.remove", { name: "sched" });
  ok((await c.call("schedule.list")).schedules.length === 0, "removing an agent removes its schedules");
  return { c, d };
}

/** Scheduling Phase 2 with fakes: a note trigger fires only once the human allows triggers on
 * the note (and never on its own agent's edit), a branch moving in a local repo, a PR opened on
 * GitHub (a fake `gh` on PATH), a trigger URL with its secret and size cap and `muster trigger`,
 * a "Then…" step one hop further, and the OpenRouter budget refusal (a fake key endpoint). Debug
 * hooks shorten the waits; the backend is restarted for them, and again without them (and
 * without a key) at the end. */
async function triggers(c: Client, d: Daemon): Promise<{ c: Client; d: Daemon }> {
  const env = { ...ENV, MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "" };
  const KEY = "sk-or-smoke-fake-key-trig-4d1e";
  const run = async (args: string[], extra: Record<string, string> = {}) => {
    const o = await cli(args, { ...env, ...extra });
    return { ok: o.success, out: dec.decode(o.stdout), err: dec.decode(o.stderr) };
  };
  const until = async (what: string, cond: () => Promise<boolean>, ms = 20000) => {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      if (await cond()) return;
      await sleep(150);
    }
    throw new Error(`timeout waiting for ${what}`);
  };
  const runs = async (id: number) => (await c.call("schedule.runs", { id, limit: 50 })).runs as Json[];
  const sched = async (id: number) => (await c.call("schedule.list")).schedules.find((s: Json) => s.id === id);
  const idOf = (out: string) => Number(out.match(/schedule (\d+)/)![1]);
  const msgs = async (agent: string) => (await c.call("agent.messages", { agent, limit: 50 })).messages as Json[];

  // A fake OpenRouter key endpoint: what the key has spent today ($6, over the $5 default).
  const seen: string[] = [];
  const keyServer = Deno.serve({ port: 0, hostname: "127.0.0.1", onListen: () => {} }, (req) => {
    seen.push(req.headers.get("authorization") ?? "");
    return Response.json({ data: { label: "smoke", usage: 106.5, usage_daily: 6.0, limit: null } });
  });
  // A fake gh on PATH: prints the body file (PRs), or says it isn't signed in.
  await Deno.writeTextFile(`${BASE}/gh-body.json`, JSON.stringify([{ number: 1, title: "old", user: { login: "a" } }]));
  await Deno.writeTextFile(`${BASE}/bin/gh`, `#!/bin/bash
echo "$@" >> '${BASE}/gh-args'
if [ -e '${BASE}/gh-signed-out' ]; then echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4; fi
cat '${BASE}/gh-body.json'
`);
  await Deno.chmod(`${BASE}/bin/gh`, 0o700);
  // A local repo whose master will move.
  const GREPO = `${BASE}/trig-repo`;
  await Deno.mkdir(GREPO, { recursive: true });
  await git(GREPO, "init", "-q", "-b", "master");
  await git(GREPO, "commit", "-q", "--allow-empty", "-m", "first");

  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon({
    MUSTER_TEST_TRIGGER_DELAY_MS: "1000",
    MUSTER_TEST_WATCH_EVERY_MS: "700",
    OPENROUTER_API_KEY: KEY,
    MUSTER_TEST_OPENROUTER_KEY_URL: `http://127.0.0.1:${keyServer.addr.port}/key`,
  });
  c = new Client(d.token);
  await c.open();

  // Two Codex agents (they take triggered work by default); their tokens answer the Stop hook.
  await c.call("agent.create", { name: "trig", kind: "codex" });
  await c.call("agent.create", { name: "trig2", kind: "codex" });
  await Deno.remove(`${BASE}/fake-token`).catch(() => {}); // an earlier agent's
  const sTrig = (await c.call("agent.start", { name: "trig" })).session.id;
  await until("trig's token", async () => (await exists(`${BASE}/fake-token`)) && (await Deno.readTextFile(`${BASE}/fake-token`)).length > 10);
  const tTrig = await Deno.readTextFile(`${BASE}/fake-token`);
  await Deno.remove(`${BASE}/fake-token`);
  await c.call("agent.start", { name: "trig2" });
  await until("trig2's token", async () => (await exists(`${BASE}/fake-token`)) && (await Deno.readTextFile(`${BASE}/fake-token`)).length > 10);
  const tTrig2 = await Deno.readTextFile(`${BASE}/fake-token`);
  await c.waitFor("both idle", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === sTrig && e.data.state === "idle"));
  const reply = async (token: string, text: string) => {
    await sleep(1500); // the prompt is pasted before Enter (and the message attributed)
    const r = await fetch(`http://127.0.0.1:${PORT}/hook/Stop`, {
      method: "POST", headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify({ last_assistant_message: text }),
    });
    await r.body?.cancel();
    ok(r.ok, `Stop hook: ${text}`);
  };

  // ---- A note trigger: nothing until the human allows triggers on the note
  ok((await run(["note", "new", "--title", "Ideas", "first idea"])).ok, "a note to watch");
  const add = await run(["schedule", "add", "trig", "--on", "note:Ideas", "Expand the new items: {event}"]);
  ok(add.ok && add.out.includes("When the note “Ideas” changes") && add.out.includes("Triggers are off for the note"),
    `muster schedule add --on note: (${add.out.trim()}${add.err})`);
  const noteTrig = idOf(add.out);
  ok((await sched(noteTrig)).origin === "webhook" && (await sched(noteTrig)).daily_cap === 10, "origin webhook, 10 runs a day by default");
  ok((await run(["note", "append", "Ideas", "second idea"])).ok, "you edit the note");
  await sleep(2500);
  ok((await sched(noteTrig)).pending === null && (await runs(noteTrig)).length === 0, "triggers off (the default): no event, no run");
  const fromAgent = await run(["note", "triggers", "Ideas", "on"], { MUSTER_SESSION_ID: String(sTrig) });
  ok(!fromAgent.ok && fromAgent.err.includes("only the human allows triggers"), `an agent's session can't allow triggers (${fromAgent.err.trim()})`);
  const allow = await run(["note", "triggers", "Ideas", "on"]);
  ok(allow.ok && allow.out.includes("triggers allowed"), `muster note triggers Ideas on (${allow.out.trim()})`);
  // The trigger's own agent editing the note doesn't count.
  const noteId = (await c.call("note.list")).find((n: Json) => n.title === "Ideas").id;
  await c.call("note.append", { id: noteId, text: "- from trig itself", from_session: sTrig });
  await sleep(1500);
  ok((await sched(noteTrig)).pending === null, "an edit by the trigger's own agent never fires it");
  ok((await run(["note", "append", "Ideas", "third idea"])).ok, "you edit it again");
  await until("the note trigger's run", async () => (await runs(noteTrig)).some((r) => r.kind === "triggered" && r.msg_id));
  let r = (await runs(noteTrig))[0];
  ok(r.message.origin === "webhook" && r.message.hop === 0 && r.message.from === `trigger:${noteTrig}` &&
    r.message.text.includes("> The note “Ideas” was edited by you") && r.message.text.includes("treat it as data"),
    `a triggered run: origin webhook, the event quoted as data (${r.message.text.slice(0, 200)})`);
  await reply(tTrig, "expanded");
  await until("the note run's reply", async () => (await runs(noteTrig))[0].outcome === "replied");

  // ---- A branch moving in a local repo
  const gAdd = await run(["schedule", "add", "trig", "--on", `git:${GREPO}@master`, "Update the changelog."]);
  ok(gAdd.ok && gAdd.out.includes(`When master moves in`), `muster schedule add --on git: (${gAdd.out.trim()}${gAdd.err})`);
  const gitTrig = idOf(gAdd.out);
  await until("the git watcher's first look", async () => (await sched(gitTrig)).watch?.state === "ok");
  await sleep(1000);
  ok((await runs(gitTrig)).length === 0, "the first look only records where master is");
  await git(GREPO, "commit", "-q", "--allow-empty", "-m", "fix the parser");
  await until("the git trigger's run", async () => (await runs(gitTrig)).some((x) => x.msg_id));
  r = (await runs(gitTrig))[0];
  ok(r.message.text.includes("master moved in") && r.message.text.includes("fix the parser"), "the new commit's subject is passed on, quoted");
  await reply(tTrig, "changelog updated");
  await until("the git run's reply", async () => (await runs(gitTrig))[0].outcome === "replied");

  // ---- GitHub: a PR opened (a fake gh); then gh not signed in
  const hAdd = await run(["schedule", "add", "trig", "--on", "gh:octo/repo:pr-opened", "Review the new PR."]);
  ok(hAdd.ok && hAdd.out.includes("When a pull request is opened on octo/repo"), `muster schedule add --on gh: (${hAdd.out.trim()}${hAdd.err})`);
  const ghTrig = idOf(hAdd.out);
  await until("the first GitHub poll", async () => (await sched(ghTrig)).watch?.state === "ok");
  ok((await Deno.readTextFile(`${BASE}/gh-args`)).includes("repos/octo/repo/pulls?state=all"), "polled with gh api");
  ok((await runs(ghTrig)).length === 0, "the first poll is a baseline");
  await Deno.writeTextFile(`${BASE}/gh-body.json`, JSON.stringify([
    { number: 2, title: "Add the thing", user: { login: "bob" }, html_url: "https://github.com/octo/repo/pull/2" },
    { number: 1, title: "old", user: { login: "a" } },
  ]));
  await until("the GitHub trigger's run", async () => (await runs(ghTrig)).some((x) => x.msg_id), 15000);
  r = (await runs(ghTrig))[0];
  ok(r.message.text.includes("PR #2 opened on octo/repo by bob: Add the thing"), "the new PR is passed on");
  await reply(tTrig, "reviewed");
  await until("the GitHub run's reply", async () => (await runs(ghTrig))[0].outcome === "replied");
  await Deno.writeTextFile(`${BASE}/gh-signed-out`, "");
  await until("gh not signed in", async () => (await sched(ghTrig)).watch?.state === "gh_not_signed_in");
  ok((await sched(ghTrig)).watch.detail.includes("gh auth login"), "the row says gh isn't signed in");
  ok((await run(["schedule", "pause", String(ghTrig)])).ok, "pause it (no more gh calls)");

  // ---- A trigger URL: loopback, its secret, a size cap; `muster trigger`
  const uAdd = await run(["schedule", "add", "trig", "--on", "url:deploy-done", "The deploy finished: {payload}"]);
  ok(uAdd.ok && uAdd.out.includes("/api/trigger/deploy-done"), `muster schedule add --on url: (${uAdd.out.trim()}${uAdd.err})`);
  const urlTrig = idOf(uAdd.out);
  ok(!JSON.stringify(await c.call("schedule.list")).includes("Bearer"), "schedule.list has no secret");
  const shown = await run(["trigger", "--url", "deploy-done"]);
  const secret = shown.out.match(/Secret: (\w+)/)?.[1] ?? "";
  ok(shown.ok && secret.length === 64 && shown.out.includes("curl -fsS"), "muster trigger --url prints the URL, secret and a curl line");
  const fromAgentUrl = await run(["trigger", "--url", "deploy-done"], { MUSTER_SESSION_ID: String(sTrig) });
  ok(!fromAgentUrl.ok && fromAgentUrl.err.includes("only the human"), "an agent's session can't read the secret");
  const post = (headers: Record<string, string>, body: string) =>
    fetch(`http://127.0.0.1:${PORT}/api/trigger/deploy-done`, { method: "POST", headers, body }).then(async (x) => ({ status: x.status, text: await x.text() }));
  ok((await post({}, "x")).status === 401, "no secret: 401");
  ok((await post({ Authorization: "Bearer wrong" }, "x")).status === 403, "a wrong secret: 403");
  ok((await post({ Authorization: `Bearer ${secret}` }, "x".repeat(16 * 1024 + 5))).status === 413, "over 16 KiB: 413");
  ok((await post({ "Tailscale-User-Login": "x", Authorization: `Bearer ${secret}` }, "x")).status === 404, "never through the proxy path");
  const fired = await post({ Authorization: `Bearer ${secret}` }, "build 42 ok\n");
  ok(fired.status === 200 && fired.text.includes('"queued":true'), `the right secret: queued (${fired.text})`);
  await until("the URL trigger's run", async () => (await runs(urlTrig)).some((x) => x.msg_id));
  r = (await runs(urlTrig))[0];
  ok(r.message.text.includes("untrusted data") && r.message.text.includes("> build 42 ok"), "the data reaches the agent quoted as untrusted");
  await reply(tTrig, "site checked");
  await until("the URL run's reply", async () => (await runs(urlTrig))[0].outcome === "replied");
  const again = await run(["trigger", "deploy-done", "second", "call"]);
  ok(again.ok && again.out.includes("it runs at"), `muster trigger: queued behind the 15-minute floor (${again.out.trim()})`);
  ok((await sched(urlTrig)).pending?.count === 1 && (await sched(urlTrig)).pending.payload, "held with its data");
  const agentFire = await run(["trigger", "deploy-done"], { MUSTER_SESSION_ID: String(sTrig) });
  ok(!agentFire.ok && agentFire.err.includes("only the human"), "an agent's session can't fire it");
  const agentMake = await run(["schedule", "add", "trig", "--on", "url:other", "x"], { MUSTER_SESSION_ID: String(sTrig) });
  ok(!agentMake.ok && agentMake.err.includes("agent's session"), "an agent's session can't make a trigger");
  for (const id of [urlTrig, gitTrig]) await run(["schedule", "rm", String(id)]);

  // ---- "Then…": a reply starts the next step, one hop further, quoting the reply
  const cAdd = await run(["schedule", "add", "trig", "--daily", "09:00", "--then", "trig2: The tests said: {reply} ({outcome})",
    "--on-failure", "notify", "Run the tests."]);
  ok(cAdd.ok, `muster schedule add --then (${cAdd.out.trim()}${cAdd.err})`);
  const chained = idOf(cAdd.out);
  const st = await sched(chained);
  ok(st.then.on_success.agent === "trig2" && st.then.on_failure.notify, "its steps");
  await c.call("schedule.run", { id: chained });
  await until("the run's message", async () => (await runs(chained)).some((x) => x.msg_id));
  await reply(tTrig, "2 failed: login, upload");
  await until("the step's message", async () => (await runs(chained)).some((x) => x.kind === "chain" && x.msg_id), 15000);
  const step = (await runs(chained)).find((x) => x.kind === "chain")!;
  const parent = (await runs(chained)).find((x) => x.kind === "manual")!;
  ok(step.agent === "trig2" && step.parent_run === parent.id && step.step === "s" && step.message.hop === 1 &&
    step.message.reply_to === parent.msg_id && step.message.from === `schedule:${chained}/then:${step.id}`,
    `the step: trig2, one hop further, answering the run (${JSON.stringify([step.agent, step.message.hop, step.message.from])})`);
  ok(step.message.text.includes("> 2 failed: login, upload") && step.message.text.includes("(replied)"), "{reply} quoted, {outcome} filled");
  await reply(tTrig2, "filed a note");
  await until("the step's reply", async () => (await runs(chained)).find((x) => x.kind === "chain")?.outcome === "replied");
  ok((await sched(chained)).last_run.kind === "manual", "the row's last run is its own, not the step's");

  // ---- OpenRouter budget: $6 spent today, over the $5 default: unattended work is refused
  await c.call("agent.create", { name: "orw", kind: "codex", provider: "openrouter", model: "openai/gpt-6.1-sol" });
  const oAdd = await run(["schedule", "add", "orw", "--daily", "09:00", "Nightly check."]);
  const orSched = idOf(oAdd.out);
  await c.call("schedule.run", { id: orSched });
  await until("the budget refusal", async () => (await runs(orSched))[0]?.outcome === "refused");
  r = (await runs(orSched))[0];
  ok(r.reason.startsWith("budget: OpenRouter spend today is $6.00 of the $5.00 daily cap"), `refused: budget (${r.reason})`);
  ok(seen.length > 0 && seen.every((h) => h === `Bearer ${KEY}`), "the key went only to the key endpoint, as a header");
  await c.call("schedule.run", { id: orSched });
  await until("the second refusal", async () => (await runs(orSched)).filter((x) => x.outcome === "refused").length === 2);
  const alerts = c.events.filter((e) => e.event === "notice" && String(e.data.text).includes("OpenRouter daily budget reached"));
  ok(alerts.length === 1, `one alert per cap per day (${alerts.length})`);
  const spend = (await c.call("schedule.settings.get")).spend;
  ok(spend.today === 6 && spend.cap === 5 && spend.key_present, `today's spend in the settings: ${JSON.stringify(spend).slice(0, 160)}`);
  // What the human types is never blocked (the ask goes through; the fake agent never answers).
  const asked = await c.call("agent.ask", { agent: "orw", text: "hello", timeout_ms: 1000 }).then((x: Json) => JSON.stringify(x), (e: Error) => String(e));
  ok(!asked.includes("budget"), `a message from you is not refused by the budget (${asked.slice(0, 120)})`);
  // Raised cap: it goes.
  await c.call("schedule.settings.set", { spend_cap: 10 });
  ok(Math.abs((await c.call("schedule.settings.get")).spend.cap - 10) < 1e-9, "the cap can be raised");
  ok(!d.log.join("").includes(KEY), "the daemon log never shows the key");
  // Raw keys from unattended work into the OpenRouter agent go through the caps too: back at $5,
  // `muster keys` from the trig agent's session (its chain is scheduled work) is refused.
  await c.call("schedule.settings.set", { spend_cap: 5 });
  const orwSid: number = (await c.call("agent.list")).find((a: Json) => a.name === "orw").session_id;
  const keys = await run(["keys", String(orwSid), "text:hello", "enter"], { MUSTER_SESSION_ID: String(sTrig) });
  ok(!keys.ok && keys.err.includes("budget:"), `unattended raw keys to an OpenRouter session are refused at the cap (${keys.err.trim()})`);
  await c.call("schedule.settings.set", { spend_cap: 10 });
  // A reload with another key: orw still runs (billing the first key), so its spend can't be
  // read and unattended work for it is refused until it restarts.
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon({
    MUSTER_TEST_TRIGGER_DELAY_MS: "1000",
    MUSTER_TEST_WATCH_EVERY_MS: "700",
    OPENROUTER_API_KEY: KEY + "-other",
    MUSTER_TEST_OPENROUTER_KEY_URL: `http://127.0.0.1:${keyServer.addr.port}/key`,
  });
  c = new Client(d.token);
  await c.open();
  await c.call("schedule.run", { id: orSched });
  await until("the other-key refusal", async () => (await runs(orSched))[0]?.reason?.includes("earlier OpenRouter key") ?? false);
  ok((await runs(orSched))[0].outcome === "refused", `a run on a process launched with another key is refused: ${(await runs(orSched))[0].reason}`);

  for (const a of ["trig", "trig2", "orw"]) await c.call("agent.remove", { name: a });
  await keyServer.shutdown();
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  return { c, d };
}

/** Scheduling Phase 3: an agent proposes a schedule over MCP and it waits for the human's
 * approval (never from the agent's own session); a reminder for itself runs without approval,
 * through `muster schedule remind` in its session; handling scheduled work it can't set another;
 * a file trigger only inside a folder the human chose, firing on a new file. */
async function agentSchedules(c: Client, d: Daemon): Promise<{ c: Client; d: Daemon }> {
  const env = { ...ENV, MUSTER_SESSION_ID: "", MUSTER_CHANNEL_ID: "" };
  const run = async (args: string[], extra: Record<string, string> = {}) => {
    const o = await cli(args, { ...env, ...extra });
    return { ok: o.success, out: dec.decode(o.stdout), err: dec.decode(o.stderr) };
  };
  const until = async (what: string, cond: () => Promise<boolean>, ms = 20000) => {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline) {
      if (await cond()) return;
      await sleep(150);
    }
    throw new Error(`timeout waiting for ${what}`);
  };
  const runs = async (id: number) => (await c.call("schedule.runs", { id, limit: 50 })).runs as Json[];
  const sched = async (id: number) => (await c.call("schedule.list")).schedules.find((s: Json) => s.id === id);
  let mcpId = 0;
  const mcp = async (token: string, name: string, args: Json = {}): Promise<{ ok: boolean; text: string }> => {
    const r = await fetch(`http://127.0.0.1:${PORT}/mcp`, {
      method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++mcpId, method: "tools/call", params: { name, arguments: args } }),
    });
    const j = await r.json();
    return { ok: r.ok && !j.error && !j.result?.isError, text: j.result?.content?.[0]?.text ?? JSON.stringify(j) };
  };

  c.close();
  await stopDaemon(d, "SIGTERM");
  // A file change within the grace after the planner's last turn would be its own: shortened.
  d = await startDaemon({ MUSTER_TEST_TRIGGER_DELAY_MS: "1000", MUSTER_TEST_WATCH_EVERY_MS: "700", MUSTER_TEST_ABSORB_GRACE_MS: "300" });
  c = new Client(d.token);
  await c.open();
  await c.call("agent.create", { name: "planner", kind: "codex" });
  await Deno.remove(`${BASE}/fake-token`).catch(() => {});
  const sPlan = (await c.call("agent.start", { name: "planner" })).session.id;
  await until("planner's token", async () => (await exists(`${BASE}/fake-token`)) && (await Deno.readTextFile(`${BASE}/fake-token`)).length > 10);
  const tPlan = await Deno.readTextFile(`${BASE}/fake-token`);
  await c.waitFor("planner idle", () => c.events.some((e) => e.event === "session_upserted" && e.data.id === sPlan && e.data.state === "idle"));
  const reply = async (text: string) => {
    await sleep(1500);
    const r = await fetch(`http://127.0.0.1:${PORT}/hook/Stop`, {
      method: "POST", headers: { Authorization: `Bearer ${tPlan}` }, body: JSON.stringify({ last_assistant_message: text }),
    });
    await r.body?.cancel();
    ok(r.ok, `Stop hook: ${text}`);
  };

  // ---- A proposal over MCP: shown with what, when, why and its use; it waits for the human
  const prop = await mcp(tPlan, "propose_schedule", {
    prompt: "Summarise the Ideas note", cron: "0 16 * * 5", tz: "Europe/London", reason: "You asked for a weekly summary",
  });
  ok(prop.ok && JSON.parse(prop.text).status === "proposed", `MCP propose_schedule (${prop.text.slice(0, 200)})`);
  const pid = JSON.parse(prop.text).schedule_id;
  let s = await sched(pid);
  ok(s.status === "proposed" && s.next_run === null && s.made_by.agent === "planner" && s.made_by.reason === "You asked for a weekly summary" &&
    s.estimate.text.includes("About 1 run in the coming week") && s.estimate.billing === "codex",
    `schedule.list: a proposal with who, why and its use (${JSON.stringify([s.status, s.estimate?.text])})`);
  ok(c.events.some((e) => e.event === "notice" && String(e.data.text).includes("planner proposes a schedule")), "a notice says it waits for you");
  ok((await run(["schedule", "ls"])).out.includes("proposed"), "muster schedule ls shows it as proposed");
  const selfApprove = await run(["schedule", "approve", String(pid)], { MUSTER_SESSION_ID: String(sPlan) });
  ok(!selfApprove.ok && selfApprove.err.includes("click on Approve"), `the agent can't approve its own proposal (${selfApprove.err.trim()})`);
  const unsetApprove = await run(["schedule", "approve", String(pid)]);
  ok(!unsetApprove.ok && unsetApprove.err.includes("click on Approve"), "nor without its session id (the CLI never approves)");
  const scripted = await c.call("schedule.approve", { id: pid }).then(() => "approved", (e: Error) => String(e));
  ok(scripted.includes("click on Approve"), `nor a WebSocket of a script's that holds no window (${scripted})`);
  const viaHttp = await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
    method: "POST", headers: { Authorization: `Bearer ${d.token}`, "Content-Type": "application/json" },
    body: JSON.stringify({ id: 1, method: "schedule.approve", params: { id: pid } }),
  }).then((r) => r.json());
  ok(String(viaHttp.error?.message).includes("click on Approve"), "nor /api/rpc");
  const widen = await run(["schedule", "add", "planner", "--every", "15m", "more"], { MUSTER_SESSION_ID: String(sPlan) });
  ok(!widen.ok && widen.err.includes("agent's session"), "nor make a schedule directly");
  // The window: Muster's own origin and a claimed window, the connection's process outside every
  // session (this test isn't one of the daemon's sessions).
  const win = new Client(d.token, `http://127.0.0.1:${PORT}`);
  await win.open();
  await win.call("window.claim", {});
  const approved = await win.call("schedule.approve", { id: pid });
  ok(approved.status === "active", `approved with a click in the window (${approved.status})`);
  win.close();
  s = await sched(pid);
  ok(s.status === "active" && s.next_run > Date.now() && s.made_by.approved_at > 0, "approved: active, with a next run");

  // ---- A reminder from the agent's session: no approval, a one-off for itself
  const rem = await run(["schedule", "remind", "--at", "+10m", "Check whether CI on master passed"], { MUSTER_SESSION_ID: String(sPlan) });
  ok(rem.ok && rem.out.includes("reminder at"), `muster schedule remind in the agent's session (${rem.out.trim()}${rem.err})`);
  const rid = Number(rem.out.match(/schedule (\d+)/)![1]);
  s = await sched(rid);
  ok(s.status === "active" && s.made_by.kind === "reminder" && Math.abs(s.next_run - (Date.now() + 10 * 60_000)) < 60_000,
    "the reminder: active, in 10 minutes, made by the agent");
  const tooFar = await mcp(tPlan, "set_reminder", { prompt: "x", in_minutes: 25 * 60 });
  ok(!tooFar.ok && tooFar.text.includes("propose_schedule"), "a reminder past 24 hours is refused");
  // It runs as scheduled work (Run now stands in for its time); while the agent handles it, it
  // can't set another.
  await c.call("schedule.run", { id: rid });
  await until("the reminder's message", async () => (await runs(rid)).some((x) => x.msg_id));
  await until("the reminder typed in", async () =>
    (await Deno.readTextFile(`${BASE}/fake-input`).catch(() => "")).includes(`from schedule:${rid} origin=schedule`));
  await sleep(1000); // submitted (Enter): the session now handles scheduled work
  const chained = await mcp(tPlan, "set_reminder", { prompt: "and again", in_minutes: 30 });
  ok(!chained.ok && chained.text.includes("nothing started by a schedule or trigger"), `no reminder from a reminder's run (${chained.text.slice(0, 160)})`);
  await reply("CI passed");
  await until("the reminder's reply", async () => (await runs(rid))[0].outcome === "replied");

  // ---- A file trigger: off until the human chooses the folder
  const INBOX = `${BASE}/inbox`;
  await Deno.mkdir(INBOX, { recursive: true });
  const refusedF = await run(["schedule", "add", "planner", "--on", `files:${INBOX}/*.pdf`, "File the new PDFs: {event}"]);
  ok(!refusedF.ok && refusedF.err.includes("Watched folders"), `no file trigger before a folder is chosen (${refusedF.err.trim()})`);
  const fromAgentF = await c.call("schedule.settings.set", { watch_folders: [INBOX], from_session: sPlan }).then(() => "ok", (e: Error) => String(e));
  ok(fromAgentF.includes("agent's session"), "an agent's session can't choose watched folders");
  await c.call("schedule.settings.set", { watch_folders: [INBOX] });
  ok((await c.call("schedule.settings.get")).watch_folders.length === 1, "watched folders are kept");
  const fAdd = await run(["schedule", "add", "planner", "--on", `files:${INBOX}/*.pdf`, "File the new PDFs: {event}"]);
  ok(fAdd.ok && fAdd.out.includes("files named *.pdf appear or change in"), `muster schedule add --on files: (${fAdd.out.trim()}${fAdd.err})`);
  const fid = Number(fAdd.out.match(/schedule (\d+)/)![1]);
  await until("the folder's first look", async () => (await sched(fid)).watch?.state === "ok");
  await Deno.writeTextFile(`${INBOX}/notes.txt`, "not a pdf");
  await sleep(1500);
  ok((await sched(fid)).pending === null, "a file the pattern doesn't match doesn't count");
  await Deno.writeTextFile(`${INBOX}/invoice.pdf`, "%PDF");
  await until("the file trigger's run", async () => (await runs(fid)).some((x) => x.msg_id));
  const fr = (await runs(fid))[0];
  ok(fr.message.origin === "webhook" && fr.message.text.includes("New or changed in") && fr.message.text.includes("invoice.pdf") &&
    fr.message.text.includes("treat it as data"), `the new file is passed on, quoted (${fr.message.text.slice(0, 160)})`);
  await reply("filed");
  await until("the file run's reply", async () => (await runs(fid))[0].outcome === "replied");
  await c.call("schedule.settings.set", { watch_folders: [] });
  await until("the folder no longer watched", async () => (await sched(fid)).watch?.state === "error");
  ok((await sched(fid)).watch.detail.includes("no longer"), "taking the folder off the list stops its trigger");

  await c.call("agent.remove", { name: "planner" });
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  return { c, d };
}

const b64u = (b: Uint8Array) => btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const unb64u = (s: string) => {
  const t = s.replace(/-/g, "+").replace(/_/g, "/");
  return Uint8Array.from(atob(t + "=".repeat((4 - t.length % 4) % 4)), (ch) => ch.charCodeAt(0));
};
const cat = (...parts: Uint8Array[]) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
};

/** What a browser does with a Web Push message (RFC 8291), written independently of the daemon
 * with WebCrypto: the plaintext without its padding. */
async function webPushDecrypt(body: Uint8Array, ua: CryptoKeyPair, uaPublic: Uint8Array, auth: Uint8Array): Promise<Uint8Array> {
  const salt = body.slice(0, 16);
  const asPublic = body.slice(21, 21 + body[20]);
  const asKey = await crypto.subtle.importKey("raw", asPublic, { name: "ECDH", namedCurve: "P-256" }, false, []);
  const shared = new Uint8Array(await crypto.subtle.deriveBits({ name: "ECDH", public: asKey }, ua.privateKey, 256));
  const hkdf = async (salt: Uint8Array, ikm: Uint8Array, info: Uint8Array, bits: number) =>
    new Uint8Array(await crypto.subtle.deriveBits(
      { name: "HKDF", hash: "SHA-256", salt, info },
      await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]),
      bits,
    ));
  const ikm = await hkdf(auth, shared, cat(enc.encode("WebPush: info\0"), uaPublic, asPublic), 256);
  const cek = await hkdf(salt, ikm, enc.encode("Content-Encoding: aes128gcm\0"), 128);
  const nonce = await hkdf(salt, ikm, enc.encode("Content-Encoding: nonce\0"), 96);
  const key = await crypto.subtle.importKey("raw", cek, "AES-GCM", false, ["decrypt"]);
  const record = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce }, key, body.slice(21 + body[20])));
  let end = record.length - 1;
  while (end > 0 && record[end] === 0) end--;
  if (record[end] !== 2) throw new Error("no last-record delimiter");
  return record.slice(0, end);
}

/** Phone alerts (src/push.rs, docs/ANDROID.md "Notifications"): a paired phone subscribes, an
 * agent asks for approval, and a local fake push service (https, trusted only by the test
 * daemon's curl, allowed only through MUSTER_PUSH_HOSTS) gets an encrypted, padded, VAPID-signed
 * alert that decrypts to that session. Then redirects, timeouts, 410 and revocation. */
async function phoneAlerts(c: Client, d: Daemon): Promise<{ c: Client; d: Daemon }> {
  console.log("phone alerts");
  const tls = `${BASE}/push-tls`;
  await Deno.mkdir(tls, { recursive: true });
  const made = await new Deno.Command("openssl", {
    args: ["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1",
      "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost",
      "-keyout", `${tls}/key.pem`, "-out", `${tls}/cert.pem`],
    stdout: "null", stderr: "piped",
  }).output();
  if (!made.success) throw new Error(`openssl: ${dec.decode(made.stderr)}`);
  interface Got { path: string; headers: Headers; body: Uint8Array; at: number; end?: number }
  const got: Got[] = [];
  let reply: (path: string) => Promise<Response> = async () => new Response(null, { status: 201 });
  const pushServer = Deno.serve({
    port: 0, hostname: "127.0.0.1", onListen() {},
    cert: await Deno.readTextFile(`${tls}/cert.pem`), key: await Deno.readTextFile(`${tls}/key.pem`),
  }, async (req) => {
    const path = new URL(req.url).pathname;
    const g: Got = { path, headers: new Headers(req.headers), body: new Uint8Array(await req.arrayBuffer()), at: Date.now() };
    got.push(g);
    try {
      return await reply(path);
    } finally {
      g.end = Date.now();
    }
  });
  const pushPort = pushServer.addr.port;
  const pushOrigin = `https://localhost:${pushPort}`;
  // A curl URL glob in the owner's ntfy URL: with globbing off it is one request to that very path.
  const NTFY_PATH = "/ntfy/muster[1-3]";
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon({
    MUSTER_PUSH_HOSTS: `localhost:${pushPort}`,
    CURL_CA_BUNDLE: `${tls}/cert.pem`,
    MUSTER_NTFY_URL: `${pushOrigin}${NTFY_PATH}`,
  });
  c = new Client(d.token);
  await c.open();

  // Pair a phone (as the page at the tailnet origin would) and talk to the daemon as it.
  const ORIGIN = "https://box.tail1.ts.net";
  const pairing = await c.call("device.pair_code", { url: ORIGIN, name: "smoke phone" });
  const paired = await fetch(`http://127.0.0.1:${PORT}/api/device/pair`, {
    method: "POST",
    headers: { Origin: ORIGIN, "X-Muster": "1", "Content-Type": "application/json" },
    body: JSON.stringify({ code: pairing.code }),
  });
  ok(paired.ok, "push: a phone pairs");
  const phoneId = (await paired.json()).device.id;
  const cookie = paired.headers.get("set-cookie")!.split(";")[0];
  const phone = async (method: string, params: Json = {}) => {
    const r = await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
      method: "POST",
      headers: { Cookie: cookie, Origin: ORIGIN, "X-Muster": "1", "Content-Type": "application/json" },
      body: JSON.stringify({ id: 1, method, params }),
    });
    const v = await r.json();
    if (v.error) throw new Error(v.error.message);
    return v.result;
  };
  const refused = async (what: string, p: Promise<unknown>, needle: string) =>
    await p.then(() => ok(false, what), (e) => ok(String(e).includes(needle), `${what}: ${e}`));

  // The browser's side: a P-256 key and an auth secret, as PushManager.subscribe makes them.
  const ua = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]) as CryptoKeyPair;
  const uaPublic = new Uint8Array(await crypto.subtle.exportKey("raw", ua.publicKey));
  const auth = crypto.getRandomValues(new Uint8Array(16));
  const keys = { p256dh: b64u(uaPublic), auth: b64u(auth) };
  const vapidKey = (await phone("push.key")).public_key;
  ok(unb64u(vapidKey).length === 65 && unb64u(vapidKey)[0] === 4, "push: push.key gives the daemon's P-256 public key");
  ok(((await Deno.stat(`${DATA}/push_vapid_key`)).mode! & 0o777) === 0o600, "push: the VAPID key is in the data dir, 0600");
  await refused("push: http endpoint refused", phone("push.subscribe", { endpoint: `http://localhost:${pushPort}/push/a`, ...keys }), "https");
  await refused("push: host off the allowlist refused", phone("push.subscribe", { endpoint: "https://push.example.com/push/a", ...keys }), "not a known push service");
  await refused("push: allowlisted host on another port refused", phone("push.subscribe", { endpoint: "https://localhost/push/a", ...keys }), "not a known push service");
  await refused("push: a curl glob pattern in the endpoint refused", phone("push.subscribe", { endpoint: `${pushOrigin}/push/x[1-10000]`, ...keys }), "subscription refused: push endpoint has characters");
  await refused("push: bad keys refused", phone("push.subscribe", { endpoint: `${pushOrigin}/push/a`, p256dh: b64u(new Uint8Array(65)), auth: keys.auth }), "p256dh");
  await refused("push: the desktop client has no alerts of its own", c.call("push.status"), "paired phone");
  const endpoint = `${pushOrigin}/push/phone-1`;
  let st = await phone("push.subscribe", { endpoint, ...keys });
  ok(st.subscription.endpoint === endpoint && st.subscription.last_ok_at === null, "push: the phone subscribes");
  const listed = (await c.call("device.list")).find((x: Json) => x.id === phoneId);
  ok(listed.push?.endpoint_host === `localhost:${pushPort}` && !JSON.stringify(listed).includes(keys.auth), "push: device.list shows alerts on, no keys");

  // An agent asks for approval: the phone's alert arrives, encrypted to it.
  const ws = (await c.call("hello")).workspaces[0];
  const ch = await c.call("channel.create", { workspace_id: ws.id, name: "alerts", agent: "shell" });
  const claude = await c.call("session.spawn", { channel_id: ch.id, kind: "claude" });
  let token = "";
  for (let n = 0; n < 100 && !token; n++) {
    token = await Deno.readTextFile(`${BASE}/claude-token-${claude.id}`).catch(() => "");
    if (!token) await sleep(100);
  }
  const asked = Date.now();
  const hook = await fetch(`http://127.0.0.1:${PORT}/hook/PermissionRequest`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
    body: JSON.stringify({ tool_name: "Bash", tool_input: { command: "rm -rf build" } }),
  });
  ok(hook.ok, "push: the permission request hook is accepted");
  const waitFor = async (what: string, cond: () => boolean, ms = 10000) => {
    const deadline = Date.now() + ms;
    while (Date.now() < deadline && !cond()) await sleep(50);
    ok(cond(), what);
  };
  await waitFor("push: the alert reaches the push service within 10 s", () => got.some((g) => g.path === "/push/phone-1"));
  const first = got.find((g) => g.path === "/push/phone-1")!;
  console.log(`      (alert took ${first.at - asked} ms)`);
  const h = first.headers;
  ok(h.get("ttl") === "300" && h.get("urgency") === "high" && h.get("content-encoding") === "aes128gcm", "push: TTL 300, Urgency high, aes128gcm");
  ok(h.get("topic") === `muster-s${claude.id}`, "push: the session is the Topic (a newer alert replaces a waiting one)");
  const authz = h.get("authorization") ?? "";
  const m = authz.match(/^vapid t=([^.]+)\.([^.]+)\.([^,]+), k=(\S+)$/);
  ok(m && m[4] === vapidKey, "push: Authorization is vapid t=<JWT>, k=<the daemon's key>");
  const claims = JSON.parse(dec.decode(unb64u(m![2])));
  const verifyKey = await crypto.subtle.importKey("raw", unb64u(vapidKey), { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  const signed = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, verifyKey, unb64u(m![3]), enc.encode(`${m![1]}.${m![2]}`));
  ok(signed && JSON.parse(dec.decode(unb64u(m![1]))).alg === "ES256", "push: the VAPID JWT is ES256 and verifies");
  ok(claims.aud === pushOrigin && claims.exp * 1000 > Date.now() && claims.exp * 1000 <= Date.now() + 24 * 3600_000,
    `push: JWT audience is the push service, expiry within 24 h: ${JSON.stringify(claims)}`);
  ok(claims.sub === "https://muster.invalid/web-push", "push: the VAPID subject is the neutral tool URL, nothing about the owner");
  ok(first.body.length === 86 + 1024 + 16, `push: padded to the fixed size (${first.body.length} bytes)`);
  const alert = JSON.parse(dec.decode(await webPushDecrypt(first.body, ua, uaPublic, auth)));
  ok(alert.v === 1 && alert.kind === "session" && alert.session_id === claude.id && alert.channel_id === ch.id &&
    alert.state === "needs_approval", `push: it decrypts (WebCrypto) to that session's approval: ${JSON.stringify(alert)}`);
  ok(alert.title.includes(claude.name) && alert.body.includes("rm -rf build"), "push: title names the session, body the request");
  for (let n = 0; n < 50 && !(st = await phone("push.status")).subscription.last_ok_at; n++) await sleep(100);
  ok(st.subscription.last_ok_at > 0 && st.subscription.last_error === null, "push: push.status shows the last delivery");
  // The ntfy fallback got the same alert, as plain text with a tap link to the phone page.
  await waitFor("push: the ntfy fallback is posted too", () => got.some((g) => g.path === NTFY_PATH));
  const nt = got.find((g) => g.path === NTFY_PATH)!;
  await sleep(300);
  ok(got.filter((g) => g.path.startsWith("/ntfy/")).length === 1, `push: curl globbing is off (one ntfy request, not three): ${got.map((g) => g.path)}`);
  ok(nt.headers.get("click") === `${ORIGIN}/m/#s=${claude.id}` && dec.decode(nt.body).includes("rm -rf build") &&
    (nt.headers.get("title") ?? "").startsWith("=?UTF-8?B?"), "push: ntfy gets title, text and the tap link");

  // A test alert: same size on the wire, and at most one per 5 s.
  let t = await phone("push.test");
  ok(t.delivered === true && t.error === null, "push: push.test delivers");
  const testMsg = got.filter((g) => g.path === "/push/phone-1").at(-1)!;
  ok(testMsg.body.length === first.body.length, "push: a test alert has the same length as any alert");
  ok(JSON.parse(dec.decode(await webPushDecrypt(testMsg.body, ua, uaPublic, auth))).kind === "test", "push: it decrypts to a test alert");
  await refused("push: test alerts are rate limited", phone("push.test"), "wait 5 seconds");
  let lastTest = Date.now();
  const nextTest = async () => {
    await sleep(Math.max(0, lastTest + 5100 - Date.now()));
    const started = Date.now();
    const r = await phone("push.test");
    lastTest = Date.now();
    return { r, took: lastTest - started };
  };

  // Redirects are not followed.
  reply = async (path) =>
    path === "/push/phone-1" ? new Response(null, { status: 302, headers: { location: `${pushOrigin}/elsewhere` } }) : new Response(null, { status: 201 });
  ({ r: t } = await nextTest());
  ok(t.delivered === false && String(t.error).includes("redirect") && !got.some((g) => g.path === "/elsewhere"), "push: a redirect is refused, not followed");
  st = await phone("push.status");
  ok(String(st.subscription.last_error).includes("redirect") && st.subscription.last_error_at >= st.subscription.last_ok_at, "push: push.status shows the failure");

  // A push service that never answers is given up on after 5 s; curl never sees the endpoint or
  // the signed token on its command line.
  reply = async () => {
    await sleep(9000);
    return new Response(null, { status: 201 });
  };
  const before = got.length;
  const slow = nextTest();
  // Scan while curl waits for the answer (the request has arrived, the reply is held back).
  for (let n = 0; n < 150 && got.length === before; n++) await sleep(50);
  await sleep(300);
  // Deno reads /proc only with every permission, so python lists the command lines.
  const scan = await new Deno.Command("python3", {
    args: ["-I", "-c", `import glob, os
seen = []
for p in glob.glob("/proc/[0-9]*/cmdline"):
    if p == "/proc/%d/cmdline" % os.getpid():
        continue  # this script names what it looks for
    try:
        c = open(p, "rb").read().replace(b"\\0", b" ").decode("utf-8", "replace")
    except OSError:
        continue
    seen.append(c)
print(sum(1 for c in seen if "curl" in c and "-K" in c))
print(sum(1 for c in seen if "vapid" in c or "/push/phone-1" in c))`],
    stdout: "piped", stderr: "null",
  }).output();
  const [curls, leaks] = dec.decode(scan.stdout).trim().split("\n").map(Number);
  ok(curls >= 1 && leaks === 0, `push: curl is running (${curls}) and no command line holds the endpoint or the VAPID token`);
  const { r: timedOut, took } = await slow;
  ok(timedOut.delivered === false && String(timedOut.error).includes("within 5 s") && took < 8500,
    `push: an unanswered push gives up after about 5 s (${took} ms)`);

  // 410: the subscription is gone and its row is deleted.
  reply = async (path) => new Response(null, { status: path === "/push/phone-1" ? 410 : 201 });
  ({ r: t } = await nextTest());
  ok(t.delivered === false && t.subscription === null, "push: 410 deletes the subscription");
  ok((await c.call("device.list")).find((x: Json) => x.id === phoneId).push === null, "push: device.list shows alerts off after 410");

  // Revoking the phone deletes its subscription.
  reply = async () => new Response(null, { status: 201 });
  await phone("push.subscribe", { endpoint, ...keys });
  ok((await c.call("device.list")).find((x: Json) => x.id === phoneId).push !== null, "push: subscribed again");

  // One admission control for every alert request (deliveries, push.test, ntfy): at most 4 at
  // once. And a delivery queued behind slow ones is checked again when its turn comes: a phone
  // revoked, turned off or subscribed elsewhere meanwhile gets nothing.
  const pairPhone = async (name: string, path: string) => {
    const code = (await c.call("device.pair_code", { url: ORIGIN, name })).code;
    const r = await fetch(`http://127.0.0.1:${PORT}/api/device/pair`, {
      method: "POST",
      headers: { Origin: ORIGIN, "X-Muster": "1", "Content-Type": "application/json" },
      body: JSON.stringify({ code }),
    });
    ok(r.ok, `push: ${name} pairs`);
    const id = (await r.json()).device.id;
    const ck = r.headers.get("set-cookie")!.split(";")[0];
    const call = async (method: string, params: Json = {}) => {
      const res = await fetch(`http://127.0.0.1:${PORT}/api/rpc`, {
        method: "POST",
        headers: { Cookie: ck, Origin: ORIGIN, "X-Muster": "1", "Content-Type": "application/json" },
        body: JSON.stringify({ id: 1, method, params }),
      });
      const v = await res.json();
      if (v.error) throw new Error(v.error.message);
      return v.result;
    };
    await call("push.subscribe", { endpoint: `${pushOrigin}${path}`, ...keys });
    return { id, call };
  };
  const [slowA, slowB, slowC, victim] = [
    await pairPhone("slow phone a", "/push/slow-a"),
    await pairPhone("slow phone b", "/push/slow-b"),
    await pairPhone("slow phone c", "/push/slow-c"),
    await pairPhone("revoked phone", "/push/victim"),
  ];
  let release!: () => void;
  const held = new Promise<void>((r) => (release = r));
  const section = Date.now();
  const since = () => got.filter((g) => g.at >= section);
  const count = (path: string) => since().filter((g) => g.path === path).length;
  reply = async (path) => {
    if (path.startsWith("/push/slow-") || path === "/push/phone-1") await held;
    return new Response(null, { status: 201 });
  };
  // Four test alerts hold the four slots (the first phone's after its 5 s gap).
  const tests: Promise<Json>[] = [nextTest().then((x) => x.r)];
  await waitFor("push: the first slow test alert is waiting on the push service", () => count("/push/phone-1") === 1, 8000);
  tests.push(slowA.call("push.test"), slowB.call("push.test"), slowC.call("push.test"));
  await waitFor("push: four test alerts run at once", () => since().filter((g) => g.path.startsWith("/push/")).length === 4);
  // An agent asks again (after working a while: a new approval, so a new alert): its alerts (and
  // ntfy's) have to queue behind the four.
  await fetch(`http://127.0.0.1:${PORT}/hook/PostToolUse`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
    body: JSON.stringify({ tool_name: "Bash", tool_input: {} }),
  }).then((r) => r.text());
  await sleep(200);
  const asked2 = await fetch(`http://127.0.0.1:${PORT}/hook/PermissionRequest`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
    body: JSON.stringify({ tool_name: "Bash", tool_input: { command: "rm -rf dist" } }),
  });
  ok(asked2.ok, "push: a second permission request is accepted");
  await sleep(700);
  ok(since().length === 4, `push: nothing beyond the 4 slots went out while they were busy (ntfy included): ${since().map((g) => g.path)}`);
  // While those alerts wait: one phone is revoked, one turns alerts off, one subscribes elsewhere.
  await c.call("device.revoke", { id: victim.id });
  await slowA.call("push.unsubscribe");
  await slowB.call("push.subscribe", { endpoint: `${pushOrigin}/push/slow-b2`, ...keys });
  release();
  const results = await Promise.all(tests);
  ok(results.every((x) => x.delivered === true), `push: the held test alerts were delivered: ${JSON.stringify(results.map((x) => x.error))}`);
  await waitFor("push: the queued alert reaches the phones still subscribed", () =>
    count("/push/slow-c") === 2 && count("/push/phone-1") === 2 && count(NTFY_PATH) === 1);
  await sleep(500);
  ok(count("/push/victim") === 0, "push: a phone revoked while its alert waited got nothing");
  ok(count("/push/slow-a") === 1, "push: a phone that turned alerts off while its alert waited got nothing more");
  ok(count("/push/slow-b") === 1 && count("/push/slow-b2") === 0, "push: an alert queued for an old subscription is not sent (to either)");
  const ends = since().map((g) => [g.at, g.end ?? Date.now()]);
  const most = Math.max(...ends.map(([at]) => ends.filter(([a, e]) => a <= at && at < e).length));
  ok(most === 4, `push: at most 4 alert requests at once, tests and ntfy included (${most})`);
  reply = async () => new Response(null, { status: 201 });
  await c.call("device.revoke", { id: phoneId });
  ok((await c.call("device.list")).find((x: Json) => x.id === phoneId).push === null, "push: revoking the phone deletes its subscription");
  await refused("push: a revoked phone can't subscribe", phone("push.subscribe", { endpoint, ...keys }), "unauthorized");
  await c.call("session.kill", { id: claude.id });

  await pushServer.shutdown();
  c.close();
  await stopDaemon(d, "SIGTERM");
  d = await startDaemon();
  c = new Client(d.token);
  await c.open();
  return { c, d };
}

/** OpenRouter (src/openrouter.rs): the key comes from the backend's environment and only an
 * OpenRouter agent's session gets it. Ends with the backend stopped. */
async function openRouter(c: Client, d: Daemon): Promise<void> {
  const KEY = "sk-or-smoke-fake-key-7f3a9c";
  ok((await c.call("hello")).openrouter?.key_present === false, "openrouter: no OPENROUTER_API_KEY, hello says so");
  await c.call("agent.create", { name: "orx", provider: "openrouter" }).then(
    () => ok(false, "openrouter: refused without the key"),
    (e) => ok(String(e).includes("OPENROUTER_API_KEY"), "openrouter: an agent on it is refused without the key"),
  );
  c.close();
  await stopDaemon(d, "SIGTERM");
  // A fresh cache: the backend answers from it and never goes to the network here.
  const cached = [
    {
      id: "anthropic/claude-sonnet-5.5",
      name: "Anthropic: Claude Sonnet 5.5",
      vendor: "anthropic",
      context_length: 1000000,
      prompt_per_m: 2,
      completion_per_m: 10,
      created: 1790000000,
    },
    {
      id: "openai/gpt-6.1-sol",
      name: "OpenAI: GPT-6.1 Sol",
      vendor: "openai",
      context_length: 400000,
      prompt_per_m: 2,
      completion_per_m: 12,
      created: 1791000000,
    },
  ];
  await Deno.writeTextFile(`${DATA}/openrouter-models.json`, JSON.stringify({ fetched_at: Date.now(), models: cached }));
  // Also OpenRouter's own shell setup for Claude Code, with the same key: no other session may
  // inherit it (they would all bill OpenRouter, and launch records would hold the key).
  d = await startDaemon({
    OPENROUTER_API_KEY: KEY,
    ANTHROPIC_AUTH_TOKEN: KEY,
    ANTHROPIC_BASE_URL: "https://openrouter.ai/api",
    ANTHROPIC_API_KEY: "",
  });
  c = new Client(d.token);
  await c.open();
  ok((await c.call("hello")).openrouter.key_present === true, "openrouter: the key in the backend's environment is seen");
  const models = await c.call("openrouter.models");
  ok(
    models.key_present && models.models.map((m: Json) => m.id).join() === cached.map((m) => m.id).join() &&
      !models.error,
    `openrouter.models answers from the cache: ${JSON.stringify(models).slice(0, 200)}`,
  );
  await c.call("agent.create", { name: "orc", kind: "codex", provider: "openrouter" }).then(
    () => ok(false, "openrouter: codex needs a model"),
    (e) => ok(String(e).includes("needs a model"), "openrouter: a Codex agent on it needs a model"),
  );
  const orClaude = await c.call("agent.create", {
    name: "or-claude",
    provider: "openrouter",
    model: "anthropic/claude-sonnet-5.5",
  });
  ok(orClaude.provider === "openrouter", "openrouter: agent.create stores the provider");
  await c.call("agent.create", { name: "sub-claude", model: "opus" });
  await c.call("agent.create", { name: "or-codex", kind: "codex", provider: "openrouter", model: "openai/gpt-6.1-sol" });
  const sOr = (await c.call("agent.start", { name: "or-claude" })).session.id;
  const sSub = (await c.call("agent.start", { name: "sub-claude" })).session.id;
  const sCodex = (await c.call("agent.start", { name: "or-codex" })).session.id;
  await c.call("channel.create", {
    workspace_id: (await c.call("hello")).workspaces[0].id,
    agent: "shell",
    prompt: `env > '${BASE}/shell-env'; sleep 30`,
  });
  const or = await launchOf("claude", sOr);
  ok(
    or.env.ANTHROPIC_BASE_URL === "https://openrouter.ai/api" && or.env.ANTHROPIC_AUTH_TOKEN === KEY &&
      or.env.ANTHROPIC_API_KEY === "" && or.env.ANTHROPIC_DEFAULT_SONNET_MODEL === "~anthropic/claude-sonnet-latest[1m]" &&
      or.env.MUSTER_PROVIDER === "openrouter" && !("OPENROUTER_API_KEY" in or.env),
    `openrouter: the Claude agent gets the gateway, the key as ANTHROPIC_AUTH_TOKEN, an empty API key: ${
      JSON.stringify(Object.keys(or.env))
    }`,
  );
  ok(
    or.env.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST === "1" && or.env.CLAUDE_CODE_SUBPROCESS_ENV_SCRUB === "1",
    "openrouter: settings files can't re-route it, and its commands, hooks and MCP servers don't get the key",
  );
  ok(
    (await Deno.readTextFile(`${DATA}/sessions/${sOr}/hook-token`)).length > 10,
    "openrouter: its hooks find the session token in its private folder (the scrub removes MUSTER_TOKEN)",
  );
  ok(or.args.includes("--model anthropic/claude-sonnet-5.5"), `openrouter: --model gets the OpenRouter id: ${or.args.slice(0, 300)}`);
  const sub = await launchOf("claude", sSub);
  ok(
    Object.keys(sub.env).length === 0 && sub.args.includes("--model opus"),
    `openrouter: a subscription agent gets none of it, not even the inherited OpenRouter setup: ${JSON.stringify(sub.env)}`,
  );
  const cx = await launchOf("codex", sCodex);
  ok(
    cx.env.OPENROUTER_API_KEY === KEY && !("ANTHROPIC_BASE_URL" in cx.env) && !("ANTHROPIC_AUTH_TOKEN" in cx.env),
    "openrouter: the Codex agent gets OPENROUTER_API_KEY only",
  );
  ok(
    cx.args.includes(`-c model_provider="muster_openrouter"`) &&
      cx.args.includes(`-c model_providers.muster_openrouter.base_url="https://openrouter.ai/api/v1"`) &&
      cx.args.includes(`-c model_providers.muster_openrouter.env_key="OPENROUTER_API_KEY"`) && cx.args.includes("-m openai/gpt-6.1-sol"),
    `openrouter: Codex gets the provider overrides and the model: ${cx.args.slice(0, 400)}`,
  );
  let shellEnv = "";
  for (let n = 0; n < 100 && !shellEnv.includes("MUSTER_SESSION_ID="); n++) {
    try {
      shellEnv = await Deno.readTextFile(`${BASE}/shell-env`);
    } catch { /* not yet */ }
    await sleep(100);
  }
  ok(shellEnv.includes("MUSTER_SESSION_ID="), "openrouter: the shell tab ran");
  ok(
    !shellEnv.includes(KEY) && !shellEnv.includes("ANTHROPIC_BASE_URL") && !shellEnv.includes("MUSTER_PROVIDER"),
    "openrouter: a shell tab inherits neither the key nor the gateway",
  );
  // The key is nowhere Muster writes or answers.
  const replies = [
    await c.call("hello"),
    await c.call("agent.list"),
    await c.call("openrouter.models"),
    await c.call("agent.models"),
    await c.call("session.output", { id: sOr }),
  ];
  ok(replies.every((r) => !JSON.stringify(r).includes(KEY)), "openrouter: no RPC reply carries the key");
  const ls = await cli(["ls", "--json"], ENV);
  const als = await cli(["agent", "ls", "--json"], ENV);
  ok(
    ls.success && als.success && dec.decode(als.stdout).includes("or-claude") &&
      ![ls, als].some((o) => dec.decode(o.stdout).includes(KEY) || dec.decode(o.stderr).includes(KEY)),
    "openrouter: `muster ls --json` and `muster agent ls --json` don't show the key",
  );
  const inData = await filesContaining(DATA, KEY);
  ok(inData.length === 0, `openrouter: no file in the data dir holds the key (store, launch records, logs): ${inData}`);
  ok((await filesContaining(RUNTIME, KEY)).length === 0, "openrouter: nothing in the runtime dir holds it");
  ok(!d.log.join("").includes(KEY), "openrouter: the daemon log never shows it");
  for (const a of ["or-claude", "sub-claude", "or-codex"]) await c.call("agent.remove", { name: a });
  c.close();

  // `muster reload` from a shell without OPENROUTER_API_KEY (an agent applying a build from its
  // session): the backend restarts itself, so its key carries over.
  const connect = async () => {
    const token = JSON.parse(await Deno.readTextFile(`${RUNTIME}/daemon.json`)).token;
    const k = new Client(token);
    await k.open();
    return k;
  };
  const pidNow = async () => JSON.parse(await Deno.readTextFile(`${RUNTIME}/daemon.json`)).pid;
  const before = await pidNow();
  const handoff = await cli(["reload"], ENV);
  ok(
    handoff.success && dec.decode(handoff.stdout).includes("restarted by the backend itself"),
    `openrouter: reload without the variable hands the key on: ${dec.decode(handoff.stdout).slice(0, 200)} ${
      dec.decode(handoff.stderr).slice(0, 200)
    }`,
  );
  await d.proc.status;
  ok((await pidNow()) !== before, "openrouter: ...a new backend runs");
  c = await connect();
  ok((await c.call("hello")).openrouter.key_present === true, "openrouter: ...and it still has the key");
  c.close();
  // From a command in its own PID namespace (Claude Code's Bash under its credential scrub, in an
  // OpenRouter session) the backend's pid isn't visible: `muster ls` still finds the backend
  // and `muster reload` has the backend restart itself (it can't signal it from there).
  const sandboxed = (args: string[]) =>
    new Deno.Command("unshare", {
      args: ["--user", "--map-current-user", "--pid", "--fork", "--mount-proc", BIN, ...args],
      env: ENV,
      stdin: "null",
      stdout: "piped",
      stderr: "piped",
    }).output().catch(() => null);
  const probe = await sandboxed(["status"]);
  if (probe === null || (!probe.success && dec.decode(probe.stderr).includes("unshare"))) {
    console.log("  skip  openrouter: no unprivileged PID namespaces here (unshare)");
  } else {
    const ls = await sandboxed(["ls", "--json"]);
    ok(ls?.success && Array.isArray(JSON.parse(dec.decode(ls.stdout))), `openrouter: \`muster ls\` from a PID namespace finds the backend: ${
      ls && dec.decode(ls.stderr).slice(0, 200)
    }`);
    const pidBefore = await pidNow();
    const hidden = await sandboxed(["reload"]);
    ok(
      hidden?.success && dec.decode(hidden.stdout).includes("restarted by the backend itself"),
      `openrouter: \`muster reload\` from a PID namespace has the backend restart itself: ${
        hidden && dec.decode(hidden.stdout).slice(0, 200)
      } ${hidden && dec.decode(hidden.stderr).slice(0, 300)}`,
    );
    ok((await pidNow()) !== pidBefore, "openrouter: ...a new backend runs");
    c = await connect();
    ok((await c.call("hello")).openrouter.key_present === true, "openrouter: ...still with the key");
    c.close();
  }
  // Set empty: the command clears it (and says so).
  const cleared = await cli(["reload"], { ...ENV, OPENROUTER_API_KEY: "" });
  ok(
    cleared.success && dec.decode(cleared.stderr).includes("restarted backend won't"),
    `openrouter: reload with the variable set empty warns: ${dec.decode(cleared.stderr).slice(0, 300)}`,
  );
  c = await connect();
  ok((await c.call("hello")).openrouter.key_present === false, "openrouter: ...and the reloaded backend has no key");
  c.close();
  const withKey = await cli(["reload"], { ...ENV, OPENROUTER_API_KEY: KEY });
  ok(withKey.success && !dec.decode(withKey.stderr).includes("OPENROUTER_API_KEY"), "openrouter: reload from a shell with the key");
  c = await connect();
  ok((await c.call("hello")).openrouter.key_present === true, "openrouter: ...gives the reloaded backend the key");
  c.close();
  for (const o of [handoff, cleared, withKey]) {
    ok(!dec.decode(o.stdout).includes(KEY) && !dec.decode(o.stderr).includes(KEY), "openrouter: reload never prints the key");
  }
  ok(
    (await filesContaining(RUNTIME, KEY)).length === 0 && (await filesContaining(DATA, KEY)).length === 0,
    "openrouter: still in no file after the reloads (the detached backend's log included)",
  );

  // "Restart Muster" (`daemon.restart`): every session ends, then a new backend and a new
  // terminal host start by themselves from the launch path, and the API answers again.
  c = await connect();
  const h0 = await c.call("hello");
  const live = await c.call("channel.create", { workspace_id: h0.workspaces[0].id, agent: "shell" });
  let liveSess: Json | undefined;
  for (let n = 0; n < 100 && !(liveSess?.pid > 0); n++) {
    liveSess = (await c.call("hello")).sessions.find((s: Json) => s.channel_id === live.id);
    if (!(liveSess?.pid > 0)) await sleep(50);
  }
  ok(liveSess?.pid > 0 && liveSess.state !== "exited", "restart: a shell runs before it");
  const stale = await c.call("daemon.restart", { backend: "0123456789ab", host: h0.updates.host })
    .then(() => "", (e: Error) => e.message);
  ok(stale.includes("nothing was stopped"), `restart: builds other than the ones shown are refused: ${stale}`);
  const still = await c.call("hello");
  ok(still.pid === h0.pid && still.sessions.find((s: Json) => s.id === liveSess!.id)?.state !== "exited",
    "restart: ...and nothing stopped");
  const restarted = await c.call("daemon.restart", { backend: h0.updates.backend, host: h0.updates.host });
  ok(String(restarted.exe).endsWith("/muster"), `restart: started from the launch path (${restarted.exe})`);
  for (let n = 0; n < 100 && !c.closed && !c.events.some((e) => e.event === "daemon_stopping"); n++) await sleep(20);
  ok(c.events.some((e) => e.event === "daemon_stopping" && e.data.reason === "restart"),
    "restart: windows hear daemon_stopping {reason: restart}");
  let h1: Json | null = null;
  for (const end = Date.now() + 60000; !h1 && Date.now() < end; await sleep(200)) {
    try {
      const disc = JSON.parse(await Deno.readTextFile(`${RUNTIME}/daemon.json`));
      if (disc.pid === h0.pid) continue;
      const k = new Client(disc.token);
      await k.open();
      h1 = await k.call("hello");
      c.close();
      c = k;
    } catch { /* not up yet */ }
  }
  ok(h1 && h1.pid !== h0.pid, `restart: a new backend answers (pid ${h0.pid} -> ${h1?.pid})`);
  ok(
    h1!.terminal_host.instance !== h0.terminal_host.instance && h1!.terminal_host.pid !== h0.terminal_host.pid,
    `restart: ...with a new terminal host (pid ${h0.terminal_host.pid} -> ${h1!.terminal_host.pid})`,
  );
  ok(h1!.sessions.find((s: Json) => s.id === liveSess!.id)?.state === "exited", "restart: the running shell ended");
  ok(typeof h1!.build === "string" && h1!.build === (h0.updates.backend ?? h0.build),
    `restart: the new backend runs the build the restart was for (${h0.build} -> ${h1!.build})`);
  ok(!(await Deno.stat(`${RUNTIME}/restart-failed`).then(() => true, () => false)), "restart: no failure is recorded");
  ok(h1!.updates.host === null && h1!.updates.backend === null, "restart: no update is left waiting at the launch path");
  // It runs the release checked before anything stopped (its read-only snapshot), not the build output.
  // (python3 reads the link: the smoke has no read permission for /proc.)
  const exeOf = async (pid: number) =>
    dec.decode((await new Deno.Command("python3", {
      args: ["-I", "-c", "import os, sys; print(os.readlink(sys.argv[1]))", `/proc/${pid}/exe`],
      stdout: "piped", stderr: "null",
    }).output()).stdout).trim();
  const [appExe, hostExe] = [await exeOf(h1!.pid), await exeOf(h1!.terminal_host.pid)];
  ok(appExe.includes("/application-releases/") && hostExe.includes("/application-releases/") &&
    appExe.split("/").slice(0, -1).join("/") === hostExe.split("/").slice(0, -1).join("/"),
    `restart: the new backend and host run from one checked release (${appExe}, ${hostExe})`);
  ok(h1!.openrouter.key_present === true, "restart: the restarted backend keeps the OpenRouter key");
  const after = await c.call("channel.create", { workspace_id: h1!.workspaces[0].id, agent: "shell" });
  let afterSess: Json | undefined;
  for (let n = 0; n < 100 && !(afterSess?.pid > 0); n++) {
    afterSess = (await c.call("hello")).sessions.find((s: Json) => s.channel_id === after.id);
    if (!(afterSess?.pid > 0)) await sleep(50);
  }
  ok(afterSess?.pid > 0, "restart: the API answers again and a new shell starts on the new host");
  ok(
    (await filesContaining(RUNTIME, KEY)).length === 0 && (await filesContaining(DATA, KEY)).length === 0,
    "restart: the key is still in no file",
  );
  c.close();
  const stop = await cli(["stop"], ENV);
  ok(stop.success, "openrouter: the reloaded backend stops");
  // `current` (the first backend) has exited; the cleanup below finds nothing left to stop.
}

let failed = false;
try {
  await Deno.mkdir(`${BASE}/home`, { recursive: true, mode: 0o700 });
  await main();
} catch (e) {
  failed = true;
  console.error(String(e));
  if (current) console.error("--- daemon log ---\n" + current.log.join("").slice(-4000));
} finally {
  if (current) {
    try {
      current.proc.kill("SIGTERM");
      await current.proc.status;
    } catch { /* already gone: only our child handle is ever signalled */ }
  }
  // The backend child is reaped. Remove its discovery before the explicit host stop,
  // so cleanup never signals a recycled PID from a stale discovery file.
  try { await Deno.remove(`${RUNTIME}/daemon.json`); } catch { /* absent */ }
  const cleanup = await new Deno.Command(BIN, {
    args: ["stop"], env: ENV, stdin: "null", stdout: "piped", stderr: "piped",
  }).output();
  if (!cleanup.success) {
    failed = true;
    console.error("isolated host cleanup failed: " + dec.decode(cleanup.stderr));
  }
}
if (failed) Deno.exit(1);
