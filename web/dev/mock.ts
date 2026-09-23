// Dev-only mock daemon: serves web/dist and speaks the client protocol with in-memory state.
// Shell sessions are real bash processes behind `script` (for a PTY); "claude"/"codex"
// sessions are bash too, with a fake state machine so approvals/inbox can be exercised.
//
//   deno task build:dev && deno task mock      -> http://127.0.0.1:17744/#token=dev
//
// Not part of the production bundle.

const PORT = Number(Deno.env.get("MOCK_PORT") ?? 17744);
const TOKEN = "dev";
const DIST = new URL("../dist/", import.meta.url);
const HOME = Deno.env.get("HOME") ?? "/tmp";

type Kind = "claude" | "codex" | "shell";
interface Session {
  id: number;
  channel_id: number;
  kind: Kind;
  name: string;
  cwd: string;
  state: string;
  state_detail: string | null;
  state_since: number;
  title: string | null;
  parent_id: number | null;
  pid: number | null;
  exit_code: number | null;
  cols: number;
  rows: number;
  queued: number;
  created_at: number;
  /** The window this session is a tab in (docs/PROTOCOL.md "Groups layout (v2)"). */
  window_id: number | null;
}

const now = () => Date.now();
let seq = 1;
const workspaces: any[] = [];
const channels: any[] = [];
const sessions = new Map<number, Session>();
const events: any[] = [];
const procs = new Map<
  number,
  { child: Deno.ChildProcess; writer: WritableStreamDefaultWriter<Uint8Array> }
>();
const scrollback = new Map<number, Uint8Array[]>();
const clients = new Set<WebSocket>();
const attached = new Map<WebSocket, Set<number>>();

// ---------- windows (docs/PROTOCOL.md "Windows, tabs and naming", "Groups layout (v2)") ----------
// A tab is a session (`Session.window_id`); `channel_ids` are the tasks whose window it is.

interface Win {
  id: number;
  name: string;
  channel_ids: number[];
  open: boolean;
  last_focused_at: number;
  created_at: number;
  /** Opaque split layout from the frontend (`window.set_layout`). */
  layout: unknown;
}
const windows: Win[] = [];
/** Which window each connection holds. */
const holding = new Map<WebSocket, number>();

function newWindow(id?: number, name?: string): Win {
  const wid = id ?? (windows.reduce((m, w) => Math.max(m, w.id), 0) + 1);
  const w: Win = {
    id: wid,
    name: name || String(wid),
    channel_ids: [],
    open: false,
    last_focused_at: 0,
    created_at: now(),
    layout: null,
  };
  windows.push(w);
  windows.sort((a, b) => a.id - b.id);
  return w;
}

const liveTasks = (w: Win) => w.channel_ids.filter((id) => channels.some((c) => c.id === id && !c.archived));
const hasTabs = (w: Win) => [...sessions.values()].some((s) => s.window_id === w.id);
const winById = (id: number | null | undefined) => windows.find((x) => x.id === id);

function upsertWindow(w: Win) {
  broadcast("window_upserted", w);
}

/** Delete a window nobody holds once no tab (session) or task refers to it. */
function removeWindowIfEmpty(w: Win | undefined): boolean {
  if (!w || w.open || hasTabs(w) || liveTasks(w).length || !windows.includes(w)) return false;
  windows.splice(windows.indexOf(w), 1);
  broadcast("window_removed", { id: w.id });
  return true;
}

/** A task's window follows its tabs: to the one window they are all in, else (none left in its
 *  window) to `prefer` if one of them is there, else the oldest session's window. */
function taskFollows(c: any, prefer?: number) {
  if (!c || c.archived) return;
  const wins = [...sessions.values()]
    .filter((s) => s.channel_id === c.id && winById(s.window_id))
    .sort((a, b) => a.id - b.id)
    .map((s) => s.window_id as number);
  if (!wins.length) return;
  let target: number;
  if (wins.every((w) => w === wins[0])) target = wins[0];
  else if (!wins.includes(c.window_id)) target = prefer !== undefined && wins.includes(prefer) ? prefer : wins[0];
  else return;
  if (target === c.window_id) return;
  const from = winById(c.window_id);
  placeChannel(c, winById(target)!);
  removeWindowIfEmpty(from);
}

/** The window a new session goes to: `requested` (must exist), else the parent's, else the
 *  task's, else where a new task would go. */
function sessionWindow(requested: number | undefined, parent: number | undefined, ch: any): Win {
  if (requested != null) {
    const w = winById(requested);
    if (!w) throw new Error(`no such window: ${requested}`);
    return w;
  }
  return winById(sessions.get(parent as number)?.window_id) ?? winById(ch.window_id) ?? defaultWindow();
}

/** Put channel `c` in window `w` at `index` (default end), taking it out of any other window. */
function placeChannel(c: any, w: Win, index?: number) {
  for (const o of windows) {
    if (o === w) continue;
    const i = o.channel_ids.indexOf(c.id);
    if (i >= 0) {
      o.channel_ids.splice(i, 1);
      upsertWindow(o);
    }
  }
  w.channel_ids = w.channel_ids.filter((x) => x !== c.id);
  w.channel_ids.splice(
    index === undefined ? w.channel_ids.length : Math.max(0, Math.min(index, w.channel_ids.length)),
    0,
    c.id,
  );
  c.window_id = w.id;
  broadcast("channel_upserted", c);
  upsertWindow(w);
}

/** Where new channels go: the open window focused last, else the lowest, else a new one. */
function defaultWindow(): Win {
  const open = windows.filter((w) => w.open).sort((a, b) => b.last_focused_at - a.last_focused_at);
  return open[0] ?? windows[0] ?? newWindow();
}

function release(ws: WebSocket) {
  const id = holding.get(ws);
  holding.delete(ws);
  const w = windows.find((x) => x.id === id);
  if (!w || [...holding.values()].includes(w.id)) return;
  w.open = false;
  upsertWindow(w);
  removeWindowIfEmpty(w);
}

function claim(ws: WebSocket, id?: number): Win {
  const held = windows.find((x) => x.id === holding.get(ws));
  if (held && (id === undefined || id === held.id)) return held;
  release(ws);
  let w: Win | undefined;
  if (id !== undefined) w = windows.find((x) => x.id === id) ?? newWindow(id);
  else {
    w = windows.find((x) => !x.open && hasTabs(x)) ?? newWindow();
  }
  w.open = true;
  holding.set(ws, w.id);
  // Adopt channels without a (live) window.
  for (const c of channels) {
    if (c.archived) continue;
    if (c.window_id == null || !windows.some((x) => x.id === c.window_id)) placeChannel(c, w);
  }
  for (const s of sessions.values()) {
    if (!winById(s.window_id)) {
      s.window_id = w.id;
      upsert(s);
    }
  }
  upsertWindow(w);
  return w;
}

/** First prompt: first line, first 6 words, at most 40 chars. */
function nameFromPrompt(text: string): string {
  return text.split("\n")[0].trim().split(/\s+/).slice(0, 6).join(" ").slice(0, 40);
}

function maybeAutoName(channelId: number, prompt: string) {
  const c = channels.find((x) => x.id === channelId);
  if (!c || !c.auto_named || c.derived) return;
  const n = nameFromPrompt(prompt);
  if (!n) return;
  c.name = n;
  c.derived = true;
  broadcast("channel_upserted", c);
}

function broadcast(event: string, data: unknown) {
  const msg = JSON.stringify({ event, data });
  for (const ws of clients) if (ws.readyState === WebSocket.OPEN) ws.send(msg);
}

function logEvent(channel_id: number | null, session_id: number | null, kind: string, data: any) {
  const e = { id: seq++, channel_id, session_id, kind, data, ts: now() };
  events.push(e);
  broadcast("event", e);
  return e;
}

function upsert(s: Session) {
  broadcast("session_upserted", s);
}

function setState(s: Session, state: string, detail: string | null = null) {
  if (s.state === state && s.state_detail === detail) return;
  const from = s.state;
  s.state = state;
  s.state_detail = detail;
  s.state_since = now();
  upsert(s);
  logEvent(s.channel_id, s.id, "state_change", { session_id: s.id, from, to: state, detail });
}

function frame(type: number, sid: number, payload: Uint8Array) {
  const b = new Uint8Array(5 + payload.length);
  b[0] = type;
  new DataView(b.buffer).setUint32(1, sid);
  b.set(payload, 5);
  return b;
}

function pushOutput(sid: number, chunk: Uint8Array) {
  const sb = scrollback.get(sid) ?? [];
  sb.push(chunk);
  let total = sb.reduce((n, c) => n + c.length, 0);
  while (total > 512 * 1024 && sb.length > 1) total -= sb.shift()!.length;
  scrollback.set(sid, sb);
  const f = frame(1, sid, chunk);
  for (const [ws, set] of attached) if (set.has(sid) && ws.readyState === WebSocket.OPEN) ws.send(f);
}

const enc = new TextEncoder();
const dec = new TextDecoder();

function startProcess(s: Session) {
  const env: Record<string, string> = {
    TERM: "xterm-256color",
    COLORTERM: "truecolor",
    HOME,
    PATH: Deno.env.get("PATH") ?? "/usr/bin:/bin",
    LANG: Deno.env.get("LANG") ?? "C.UTF-8",
    PS1: "\\[\\e[38;5;141m\\]\\W\\[\\e[0m\\] \\[\\e[38;5;244m\\]$\\[\\e[0m\\] ",
    MUSTER: "1",
    MUSTER_SESSION_ID: String(s.id),
  };
  const intro = s.kind === "shell"
    ? ""
    : `printf '\\e[1;38;5;173m✻ Fake ${s.kind}\\e[0m  \\e[2m(mock agent: type anything; "ask" triggers an approval)\\e[0m\\n\\n'; `;
  const cmd = new Deno.Command("script", {
    args: [
      "-qfec",
      `stty cols ${s.cols} rows ${s.rows} 2>/dev/null; ${intro}exec bash --norc --noprofile -i`,
      "/dev/null",
    ],
    cwd: s.cwd,
    env,
    clearEnv: true,
    stdin: "piped",
    stdout: "piped",
    stderr: "piped",
  });
  const child = cmd.spawn();
  const writer = child.stdin.getWriter();
  procs.set(s.id, { child, writer });
  s.pid = child.pid;
  s.exit_code = null;
  (async () => {
    for await (const chunk of child.stdout) pushOutput(s.id, chunk);
  })();
  (async () => {
    for await (const chunk of child.stderr) pushOutput(s.id, chunk);
  })();
  child.status.then((st) => {
    procs.delete(s.id);
    if (!sessions.has(s.id)) return;
    s.exit_code = st.code;
    s.pid = null;
    setState(s, "exited");
    logEvent(s.channel_id, s.id, "session_exited", { session_id: s.id, exit_code: st.code });
  });
  if (s.kind === "shell") setTimeout(() => setState(s, "idle"), 300);
  else fakeAgent(s);
}

function fakeAgent(s: Session) {
  setTimeout(() => setState(s, "working", "Reading the codebase"), 500);
  setTimeout(() => {
    if (s.state === "working") setState(s, "idle");
  }, 3500);
}

function spawn(req: any): Session {
  const ch = channels.find((c) => c.id === req.channel_id);
  if (!ch) throw new Error("no such channel");
  const kind = req.kind as Kind;
  if (ch.archived) throw new Error(`channel ${ch.id} is archived`);
  const win = sessionWindow(req.window_id, req.parent_id, ch);
  const count = [...sessions.values()].filter((x) => x.channel_id === ch.id && x.kind === kind).length;
  const s: Session = {
    id: seq++,
    channel_id: ch.id,
    kind,
    name: req.name ?? (count ? `${kind}-${count + 1}` : kind),
    cwd: req.cwd ?? ch.cwd,
    state: "starting",
    state_detail: null,
    state_since: now(),
    title: null,
    parent_id: null,
    pid: null,
    exit_code: null,
    cols: req.cols ?? 120,
    rows: req.rows ?? 32,
    queued: 0,
    created_at: now(),
    window_id: win.id,
  };
  sessions.set(s.id, s);
  upsert(s);
  logEvent(ch.id, s.id, "session_started", { session_id: s.id, kind, name: s.name });
  startProcess(s);
  if (req.prompt) {
    logEvent(ch.id, s.id, "prompt", { text: req.prompt, session_id: s.id, from: "human", queued: false });
    setTimeout(() => write(s.id, `# ${req.prompt}\r`), 700);
  }
  return s;
}

function deliver(s: Session, text: string) {
  if (s.kind !== "shell" && /ask|approve/i.test(text)) {
    setState(s, "working", "Thinking");
    setTimeout(() => setState(s, "needs_approval", "Bash: cargo test --workspace"), 1200);
  } else if (s.kind !== "shell") {
    setState(s, "working", "Editing files");
    setTimeout(() => s.state === "working" && setState(s, "idle"), 2500);
    setTimeout(
      () => logEvent(s.channel_id, s.id, "agent_post", { session_id: s.id, text: `Done: ${text}` }),
      2600,
    );
  }
  write(s.id, s.kind === "shell" ? text + "\r" : `# ${text}\r`);
}

function write(sid: number, data: string | Uint8Array) {
  const p = procs.get(sid);
  if (!p) return;
  p.writer.write(typeof data === "string" ? enc.encode(data) : data).catch(() => {});
}

function kill(sid: number) {
  const p = procs.get(sid);
  if (!p) return;
  try {
    p.child.kill("SIGHUP");
  } catch { /* gone */ }
  setTimeout(() => {
    try {
      p.child.kill("SIGKILL");
    } catch { /* gone */ }
  }, 1500);
}

function stripAnsi(s: string) {
  // deno-lint-ignore no-control-regex
  return s.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "").replace(/\x1b\][^\x07]*\x07/g, "").replace(/\r/g, "");
}

async function handle(method: string, p: any, ws?: WebSocket): Promise<unknown> {
  switch (method) {
    case "hello":
      return {
        version: "0.0.0-mock",
        workspaces,
        channels: channels.filter((c) => !c.archived),
        sessions: [...sessions.values()],
        windows,
        home: HOME,
        default_shell: "/bin/bash",
      };
    case "window.claim": {
      if (!ws) throw new Error("window.claim is WebSocket only");
      return claim(ws, typeof p.id === "number" ? p.id : undefined);
    }
    case "window.create": {
      const w = newWindow(undefined, p.name);
      upsertWindow(w);
      return w;
    }
    case "window.focused": {
      const w = windows.find((x) => x.id === p.id);
      if (w) w.last_focused_at = now();
      return {};
    }
    case "window.rename": {
      const w = windows.find((x) => x.id === p.id);
      if (!w) throw new Error(`no such window: ${p.id}`);
      w.name = String(p.name).trim() || String(w.id);
      upsertWindow(w);
      return w;
    }
    case "window.reorder_tabs": {
      const w = windows.find((x) => x.id === p.id);
      if (!w) throw new Error(`no such window: ${p.id}`);
      const want = (p.channel_ids as number[]).filter((id) => w.channel_ids.includes(id));
      w.channel_ids = [...want, ...w.channel_ids.filter((id) => !want.includes(id))];
      upsertWindow(w);
      return w;
    }
    case "window.set_layout": {
      const w = winById(p.id);
      if (!w) throw new Error(`no such window: ${p.id}`);
      w.layout = p.layout ?? null; // no event echo
      return {};
    }
    case "session.set_window": {
      const s = sessions.get(p.id);
      const w = winById(p.window_id);
      if (!w) throw new Error(`no such window: ${p.window_id}`);
      if (!s) throw new Error(`no such session: ${p.id}`);
      if (p.from_window_id != null && p.from_window_id !== s.window_id) {
        throw new Error(`session ${s.id} is no longer in window ${p.from_window_id} (now in ${s.window_id ?? "none"})`);
      }
      if (s.window_id === w.id) return s;
      const from = winById(s.window_id);
      s.window_id = w.id;
      upsert(s);
      taskFollows(channels.find((c) => c.id === s.channel_id), w.id);
      removeWindowIfEmpty(from);
      return s;
    }
    case "window.raise":
      // No window manager here: the client pulls the tab instead.
      return { raised: false };
    case "channel.move": {
      const c = channels.find((x) => x.id === p.id && !x.archived);
      if (!c) throw new Error(`no such channel: ${p.id}`);
      const w = windows.find((x) => x.id === p.window_id);
      if (!w) throw new Error(`no such window: ${p.window_id}`);
      const from = windows.find((x) => x.id === c.window_id);
      placeChannel(c, w, p.index);
      // The task's sessions (tabs) go with it.
      const left = new Set<Win | undefined>([from]);
      for (const s of sessions.values()) {
        if (s.channel_id !== c.id || s.window_id === w.id) continue;
        left.add(winById(s.window_id));
        s.window_id = w.id;
        upsert(s);
      }
      for (const o of left) if (o !== w) removeWindowIfEmpty(o);
      return c;
    }
    case "workspace.add": {
      let path = String(p.path).replace(/^~(?=\/|$)/, HOME).replace(/\/+$/, "");
      if (!path) path = "/";
      const st = await Deno.stat(path).catch(() => null);
      if (!st?.isDirectory) throw new Error(`not a directory: ${path}`);
      const existing = workspaces.find((w) => w.path === path);
      if (existing) return existing;
      const is_git = !!(await Deno.stat(path + "/.git").catch(() => null));
      const w = { id: seq++, name: p.name || path.split("/").pop() || path, path, is_git, created_at: now() };
      workspaces.push(w);
      broadcast("workspace_upserted", w);
      return w;
    }
    case "workspace.remove": {
      const i = workspaces.findIndex((w) => w.id === p.id);
      if (i >= 0) workspaces.splice(i, 1);
      for (const c of channels.filter((c) => c.workspace_id === p.id)) {
        await handle("channel.archive", { id: c.id });
      }
      broadcast("workspace_removed", { id: p.id });
      return {};
    }
    case "channel.create": {
      const w = workspaces.find((w) => w.id === p.workspace_id);
      if (!w) throw new Error("no such workspace");
      const given = String(p.name ?? "").trim();
      const wt = w.is_git && p.worktree === true;
      const id = seq++;
      const c: any = {
        id,
        workspace_id: w.id,
        name: given || w.path.split("/").pop() || w.path,
        auto_named: !given,
        window_id: null,
        cwd: w.path, // the mock doesn't create worktrees
        branch: wt ? `muster/${id}` : null,
        base_branch: wt ? (p.base || "main") : null,
        owns_worktree: wt,
        port_base: 20000 + channels.length * 10,
        archived: false,
        layout: null,
        created_at: now(),
        last_read_at: now(),
        sort_order: channels.length,
        permissions: p.permissions ?? { codex_sandbox: "workspace_write", claude_mode: "default" },
      };
      channels.push(c);
      if (p.window_id != null && !winById(p.window_id)) throw new Error(`no such window: ${p.window_id}`);
      const target = winById(p.window_id) ?? defaultWindow();
      placeChannel(c, target);
      logEvent(c.id, null, "system", {
        text: `Channel created${wt ? ` on ${c.branch}` : ""}`,
        level: "info",
      });
      if (p.agent) spawn({ channel_id: c.id, kind: p.agent, prompt: p.prompt, cols: p.cols, rows: p.rows });
      if (p.prompt) maybeAutoName(c.id, p.prompt);
      return c;
    }
    case "channel.set_permissions": {
      const c = channels.find((c) => c.id === p.id);
      if (!c) throw new Error(`no such channel: ${p.id}`);
      c.permissions = p.permissions;
      broadcast("channel_upserted", c);
      return c;
    }
    case "channel.rename": {
      const c = channels.find((c) => c.id === p.id);
      const n = String(p.name).trim();
      if (!n) throw new Error("channel name is empty");
      c.name = n;
      c.auto_named = false;
      broadcast("channel_upserted", c);
      return c;
    }
    case "channel.archive": {
      const c = channels.find((c) => c.id === p.id);
      if (!c) return {};
      c.archived = true;
      const left = new Set<Win | undefined>();
      for (const s of [...sessions.values()].filter((s) => s.channel_id === c.id)) {
        kill(s.id);
        sessions.delete(s.id);
        left.add(winById(s.window_id));
        broadcast("session_removed", { id: s.id });
      }
      broadcast("channel_removed", { id: c.id });
      const w = windows.find((x) => x.id === c.window_id);
      if (w) {
        w.channel_ids = w.channel_ids.filter((id) => id !== c.id);
        upsertWindow(w);
        removeWindowIfEmpty(w);
      }
      for (const o of left) removeWindowIfEmpty(o);
      return {};
    }
    case "channel.set_layout": {
      const c = channels.find((c) => c.id === p.id);
      if (c) c.layout = p.layout;
      return {};
    }
    case "channel.mark_read": {
      const c = channels.find((c) => c.id === p.id);
      if (c) {
        c.last_read_at = now();
        broadcast("channel_upserted", c);
      }
      return {};
    }
    case "channel.reorder": {
      (p.ids as number[]).forEach((id, i) => {
        const c = channels.find((c) => c.id === id);
        if (c) {
          c.sort_order = i;
          broadcast("channel_upserted", c);
        }
      });
      return {};
    }
    case "channel.post":
      return logEvent(p.id, null, "note", { text: p.text });
    case "channel.timeline": {
      let list = events.filter((e) => e.channel_id === p.id);
      if (p.before) list = list.filter((e) => e.id < p.before);
      return list.slice(-(p.limit ?? 100));
    }
    case "channel.diff": {
      const c = channels.find((c) => c.id === p.id);
      try {
        const out = await new Deno.Command("git", {
          args: ["diff", "HEAD"],
          cwd: c.cwd,
          stdout: "piped",
          stderr: "null",
        })
          .output();
        return { diff: dec.decode(out.stdout) };
      } catch {
        return { diff: "" };
      }
    }
    case "session.spawn":
      return spawn(p);
    case "session.attach": {
      return { __attach: p.id };
    }
    case "session.detach":
      if (ws) attached.get(ws)?.delete(p.id);
      return {};
    case "session.resize": {
      const s = sessions.get(p.id);
      if (s) {
        s.cols = p.cols;
        s.rows = p.rows;
        // `script` can't resize its pty; tell bash via stty so line wrapping is right.
        write(p.id, "");
      }
      return {};
    }
    case "session.kill":
      kill(p.id);
      return {};
    case "session.restart": {
      const s = sessions.get(p.id);
      if (!s) throw new Error("no such session");
      kill(s.id);
      scrollback.set(s.id, []);
      await new Promise((r) => setTimeout(r, 200));
      s.state = "starting";
      s.state_since = now();
      upsert(s);
      startProcess(s);
      upsert(s);
      return s;
    }
    case "session.remove": {
      const s = sessions.get(p.id);
      if (!s) throw new Error(`no such session: ${p.id}`);
      kill(p.id);
      sessions.delete(p.id);
      broadcast("session_removed", { id: p.id });
      // A task closes with its last session (worktree kept); else its window follows its tabs.
      const c = channels.find((x) => x.id === s.channel_id);
      const closed = (await handle("channel.close_if_empty", { id: s.channel_id }) as any).closed;
      if (!closed) taskFollows(c);
      removeWindowIfEmpty(winById(s.window_id));
      return {};
    }
    case "session.move": {
      const s = sessions.get(p.id);
      const c = channels.find((c) => c.id === p.channel_id);
      if (!s) throw new Error(`no such session: ${p.id}`);
      if (!c) throw new Error(`no such channel: ${p.channel_id}`);
      if (c.archived) throw new Error(`channel ${c.id} is archived`);
      const from = s.channel_id;
      if (p.from_channel_id != null && p.from_channel_id !== from) {
        throw new Error(`session ${s.id} is no longer in channel ${p.from_channel_id} (now in ${from})`);
      }
      if (from !== c.id) {
        s.channel_id = c.id;
        upsert(s);
        if (p.close_empty_source) await handle("channel.close_if_empty", { id: from });
      }
      return s;
    }
    case "channel.close_if_empty": {
      const c = channels.find((c) => c.id === p.id);
      if (!c || c.archived || [...sessions.values()].some((s) => s.channel_id === c.id)) return { closed: false };
      await handle("channel.archive", { id: c.id });
      return { closed: true };
    }
    case "session.rename": {
      const s = sessions.get(p.id)!;
      s.name = p.name;
      upsert(s);
      return s;
    }
    case "session.send": {
      const s = sessions.get(p.id);
      if (!s) throw new Error("no such session");
      const busy = s.state === "working" && p.mode !== "interrupt";
      logEvent(s.channel_id, s.id, "prompt", { text: p.text, session_id: s.id, from: "human", queued: busy });
      maybeAutoName(s.channel_id, p.text);
      if (busy) {
        s.queued++;
        upsert(s);
        const wait = () => {
          if (s.state === "working") return setTimeout(wait, 400);
          s.queued = Math.max(0, s.queued - 1);
          upsert(s);
          deliver(s, p.text);
        };
        setTimeout(wait, 400);
        return { delivered: false, queued: s.queued };
      }
      deliver(s, p.text);
      return { delivered: true, queued: s.queued };
    }
    case "session.approve": {
      const s = sessions.get(p.id);
      if (!s) throw new Error("no such session");
      logEvent(s.channel_id, s.id, "approval", { session_id: s.id, decision: p.decision });
      if (p.decision === "deny") setState(s, "idle");
      else {
        setState(s, "working", "Running cargo test");
        setTimeout(() => s.state === "working" && setState(s, "idle"), 2000);
      }
      return {};
    }
    case "session.interrupt":
      write(p.id, "\x03");
      return {};
    case "session.clear_queue": {
      const s = sessions.get(p.id);
      if (s) {
        s.queued = 0;
        upsert(s);
      }
      return {};
    }
    case "session.screen": {
      const sb = scrollback.get(p.id) ?? [];
      const text = stripAnsi(dec.decode(concat(sb.slice(-20))));
      return { lines: text.split("\n").slice(-30) };
    }
    case "fs.complete": {
      const prefix = String(p.prefix || "~/");
      const expanded = prefix.replace(/^~(?=\/|$)/, HOME);
      const dir = expanded.endsWith("/") ? expanded : expanded.slice(0, expanded.lastIndexOf("/") + 1);
      const base = expanded.slice(dir.length);
      const out: string[] = [];
      try {
        for await (const e of Deno.readDir(dir || "/")) {
          if (!e.isDirectory || (e.name.startsWith(".") && !base.startsWith("."))) continue;
          if (!e.name.startsWith(base)) continue;
          const full = dir + e.name + "/";
          out.push(prefix.startsWith("~") ? full.replace(HOME, "~") : full);
        }
      } catch { /* unreadable */ }
      return { entries: out.sort().slice(0, 100) };
    }
  }
  throw new Error(`unknown method ${method}`);
}

function concat(parts: Uint8Array[]) {
  const n = parts.reduce((a, b) => a + b.length, 0);
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function onSocket(ws: WebSocket) {
  ws.binaryType = "arraybuffer";
  clients.add(ws);
  attached.set(ws, new Set());
  ws.onclose = () => {
    clients.delete(ws);
    attached.delete(ws);
    release(ws);
  };
  ws.onmessage = async (e) => {
    if (typeof e.data !== "string") {
      const b = new Uint8Array(e.data as ArrayBuffer);
      if (b[0] !== 2) return;
      const sid = new DataView(b.buffer).getUint32(1);
      write(sid, b.slice(5));
      return;
    }
    const msg = JSON.parse(e.data);
    try {
      const result: any = await handle(msg.method, msg.params ?? {}, ws);
      if (result && result.__attach !== undefined) {
        const sid = result.__attach;
        ws.send(JSON.stringify({ id: msg.id, result: {} }));
        attached.get(ws)?.add(sid);
        ws.send(frame(3, sid, concat(scrollback.get(sid) ?? [])));
        return;
      }
      ws.send(JSON.stringify({ id: msg.id, result }));
    } catch (err) {
      ws.send(
        JSON.stringify({ id: msg.id, error: { message: err instanceof Error ? err.message : String(err) } }),
      );
    }
  };
}

const TYPES: Record<string, string> = {
  html: "text/html; charset=utf-8",
  js: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8",
  map: "application/json",
};

Deno.serve({ port: PORT, hostname: "127.0.0.1" }, async (req) => {
  const url = new URL(req.url);
  if (url.pathname === "/ws") {
    if (url.searchParams.get("token") !== TOKEN) return new Response("unauthorized", { status: 401 });
    const { socket, response } = Deno.upgradeWebSocket(req);
    onSocket(socket);
    return response;
  }
  if (url.pathname === "/api/health") return Response.json({ ok: true, version: "mock" });
  const path = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
  if (path.includes("..")) return new Response("no", { status: 400 });
  try {
    const body = await Deno.readFile(new URL(path, DIST));
    const ext = path.split(".").pop() ?? "";
    return new Response(body, {
      headers: { "content-type": TYPES[ext] ?? "application/octet-stream", "cache-control": "no-store" },
    });
  } catch {
    return new Response("not found", { status: 404 });
  }
});

console.log(`mock daemon: http://127.0.0.1:${PORT}/#token=${TOKEN}`);
