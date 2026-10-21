// Dev-only mock daemon: serves web/dist and speaks the client protocol with in-memory state.
// Shell sessions are real bash processes behind `script` (for a PTY); "claude"/"codex"
// sessions are bash too, with a fake state machine so approvals/inbox can be exercised.
//
//   deno task build:dev && deno task mock      -> http://127.0.0.1:17744/#token=dev
//
// Not part of the production bundle.

import type { Note, NoteAuthor, NoteSummary, Panel, PanelResource } from "../src/types.ts";
import { isPreviewUrl } from "../src/panels/preview.ts";
import { hiddenCharsOrSpaces, memoryFileNameError } from "../src/agents.ts";
import { servePanelHtml } from "./build_panels.ts";
import { scheduleMock } from "./mock_schedules.ts";

const PORT = Number(Deno.env.get("MOCK_PORT") ?? 17744);
const TOKEN = "dev";
const DIST = new URL("../dist/", import.meta.url);
const HOME = Deno.env.get("HOME") ?? "/tmp";
const panelProxy = await Deno.readTextFile(new URL("../panels/proxy.html", import.meta.url));
const builtinApps: Record<string, string> = {};
for (const app of ["markdown", "image", "chart", "table", "pr_decision", "pr_overview", "board"]) {
  builtinApps[app] = await Deno.readTextFile(new URL(`../panels/${app}.html`, import.meta.url));
}
// Drawn by the UI around the panel server's /preview page (src/web_preview.rs).
builtinApps.web_preview = "<!doctype html><!-- web_preview -->";
const previewPage = await Deno.readTextFile(new URL("../panels/preview.html", import.meta.url));
/** Like src/web_preview.rs `parse`, loosely: a loopback http(s) URL, never one of the mock's ports. */
function mockPreviewUrl(url: unknown): URL {
  if (!isPreviewUrl(url)) throw new Error("web_preview frames only local dev servers; open other pages in the user's system browser instead");
  const u = new URL(url);
  const port = Number(u.port || (u.protocol === "https:" ? 443 : 80));
  if (port === PORT || port === panelServer.addr.port) throw new Error(`port ${port} is Muster's own`);
  return u;
}
/** Like src/web_preview.rs `check`, loosely: is it up, does it refuse framing, does it redirect away. */
async function mockPreviewCheck(url: string) {
  let res: Response;
  try {
    res = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(3000) });
  } catch {
    return { reachable: false };
  }
  await res.body?.cancel();
  const xfo = (res.headers.get("x-frame-options") ?? "").toLowerCase();
  const ancestors = /frame-ancestors([^;,]*)/i.exec(res.headers.get("content-security-policy") ?? "")?.[1]?.trim();
  const refused = ancestors !== undefined
    ? (/\*|http:|127\.0\.0\.1:\*/.test(ancestors) ? null : `Content-Security-Policy frame-ancestors ${ancestors}`)
    : xfo.includes("deny") ? "X-Frame-Options: DENY" : xfo.includes("sameorigin") ? "X-Frame-Options: SAMEORIGIN" : null;
  const location = res.status >= 300 && res.status < 400 ? res.headers.get("location") : null;
  const away = location && /^[a-z]+:\/\//i.test(location) && new URL(location).origin !== new URL(url).origin ? location : null;
  return { reachable: true, answered: true, status: res.status, refused, leaves_to: away };
}
/** Like src/panel_apps.rs: an image `path` becomes a data URI; a `url` origin joins resourceDomains.
 * Each picture in a pr_decision panel's `media` is read the same way, from a path only. */
async function mockPanelData(app: string, cwd: string, data: any, csp: any, opening: boolean) {
  if (app === "pr_decision" && Array.isArray(data?.media)) {
    const media = [];
    for (const m of data.media) {
      if (m?.kind !== "image") { media.push(m); continue; }
      if (m.url !== undefined) throw new Error("pr_decision media pictures take a path only");
      const { kind: _, ...rest } = m;
      media.push({ kind: "image", ...(await mockImage(cwd, rest, csp, opening)) });
    }
    return { ...data, media };
  }
  if (app === "web_preview") {
    if (Object.keys(data ?? {}).some((k) => k !== "url")) throw new Error("web_preview takes only `url`");
    return { url: mockPreviewUrl(data?.url).href };
  }
  if (app !== "image") return data;
  return await mockImage(cwd, data, csp, opening);
}
async function mockImage(cwd: string, data: any, csp: any, opening: boolean) {
  const out = { ...data };
  if (typeof data.path === "string" && data.url === undefined) {
    const root = await Deno.realPath(cwd);
    const file = await Deno.realPath(`${root}/${data.path}`);
    if (!file.startsWith(root + "/")) throw new Error("image path must stay inside the task worktree");
    const info = await Deno.stat(file);
    if (!info.isFile || info.size > 8 * 1024 * 1024) throw new Error("image path must name a regular file of at most 8 MiB");
    const bytes = await Deno.readFile(file);
    const mime = bytes[0] === 0x89 && bytes[1] === 0x50 ? "image/png" : bytes[0] === 0xff && bytes[1] === 0xd8 ? "image/jpeg"
      : /\.svg$/i.test(file) ? "image/svg+xml" : null;
    if (!mime) throw new Error("unsupported image type");
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    Object.assign(out, { src: `data:${mime};base64,${btoa(bin)}`, mime, bytes: bytes.length });
  } else if (typeof data.url === "string" && data.path === undefined) {
    const u = new URL(data.url);
    if (u.protocol !== "https:" || u.username || u.password) throw new Error("image url must be https");
    csp.resourceDomains ??= [];
    if (!csp.resourceDomains.includes(u.origin)) {
      if (!opening) throw new Error("the image URL's origin is not in this panel's resourceDomains");
      csp.resourceDomains.push(u.origin);
    }
  } else throw new Error("image data needs path or url");
  return out;
}
const panels = new Map<number, PanelResource>();
const panelEvents: any[] = [];
let panelSeq = 0;
let panelResourceDelay = 0;
const panelServer = Deno.serve({ port: 0, hostname: "127.0.0.1" }, (req) => {
  const u = new URL(req.url);
  if (u.pathname === "/preview" && ["GET", "HEAD"].includes(req.method)) {
    let target: URL;
    try { target = mockPreviewUrl(u.searchParams.get("url")); } catch (e) { return new Response(String(e), { status: 400 }); }
    const frameSrc = target.origin;
    const attr = target.href.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/'/g, "&#39;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    return new Response(previewPage.replace("__MUSTER_PREVIEW_URL__", attr), {
      headers: {
        "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
        "Content-Security-Policy": `default-src 'none'; style-src 'unsafe-inline'; frame-src ${frameSrc}; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors http://127.0.0.1:${PORT} http://localhost:${PORT}`,
      },
    });
  }
  if (u.pathname !== "/proxy" || !["GET", "HEAD"].includes(req.method)) return new Response("Not found", { status: 404 });
  const csp = JSON.parse(u.searchParams.get("csp") || "{}");
  const resources = (csp.resourceDomains ?? []).filter((s: unknown) => typeof s === "string" && /^https?:\/\/[\w.*:[\]-]+\/?$/.test(s)).join(" ");
  return new Response(panelProxy.replace("__MUSTER_UI_ORIGINS__", JSON.stringify([`http://127.0.0.1:${PORT}`, `http://localhost:${PORT}`])), {
    headers: {
      "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
      "Content-Security-Policy": `default-src 'none'; script-src 'self' 'unsafe-inline' ${resources}; style-src 'self' 'unsafe-inline' ${resources}; img-src 'self' data: ${resources}; font-src 'self' data: ${resources}; media-src 'self' data: ${resources}; connect-src 'none'; frame-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors http://127.0.0.1:${PORT} http://localhost:${PORT}`,
    },
  });
});
const panelOrigin = `http://127.0.0.1:${panelServer.addr.port}`;

// The test runner owns this mock and its fake terminals; never leave them running on exit.
for (const signal of ["SIGTERM", "SIGINT"] as const) Deno.addSignalListener(signal, () => {
  for (const id of procs.keys()) kill(id);
  setTimeout(() => Deno.exit(0), 500);
});

type Kind = "claude" | "codex" | "shell";
interface Session {
  id: number;
  channel_id: number;
  kind: Kind;
  /** What the run bills to (a named agent's provider when it started). */
  provider?: "subscription" | "openrouter";
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
  size_owner: string | null;
  queued: number;
  created_at: number;
  /** The window this session is a tab in (docs/PROTOCOL.md "Groups layout (v2)"). */
  window_id: number | null;
  /** A named agent's closed tab, still running (`session.hide`). */
  hidden?: boolean;
  /** Last model request (src/cache.rs): entering or leaving `working`. */
  last_turn_at?: number | null;
  /** `last_turn_at` + the provider's cache lifetime (`prefs.cache_ttl`). */
  cache_cold_at?: number | null;
  cache_ttl_min?: number | null;
}

const now = () => Date.now();
let seq = 1;
/** `hello.updates` (docs/PROTOCOL.md "Updates"); `mock.updates` changes it. */
const updates = { ui_release: null as string | null, backend: null as string | null, host: null as string | null };
/** Shared window preferences (`prefs.get` / `prefs.set`). */
const windowPrefs = {
  decorations: true,
  cache_warn: true,
  cache_ttl: { claude: 60, claude_openrouter: 5, codex: 30, codex_openrouter: 10 } as Record<string, number>,
  revision: 0,
};

/** src/cache.rs `CacheRules::cold_at` (and `refresh`'s `cache_ttl_min`). */
function coldAt(s: Session): number | null {
  const key = s.kind + (s.provider === "openrouter" ? "_openrouter" : "");
  s.cache_ttl_min = windowPrefs.cache_warn && s.kind !== "shell" ? windowPrefs.cache_ttl[key] : null;
  if (s.cache_ttl_min == null || s.last_turn_at == null) return null;
  return s.last_turn_at + s.cache_ttl_min * 60_000;
}
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
const deviceClients = new Set<WebSocket>();
const pairAttempts: number[] = [];
const device = {
  id: 1,
  name: "Android phone",
  scopes: ["view", "interact"],
  created_at: now(),
  last_seen_at: now(),
  expires_at: now() + 30 * 86400000,
};

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
const hasTabs = (w: Win) => [...sessions.values()].some((s) => s.window_id === w.id) || [...panels.values()].some((r) => !r.panel.closed && r.panel.window_id === w.id);
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
  if (!wins.length) {
    // Every tab it has left is a hidden agent's: the task leaves its window too.
    const tabs = [...sessions.values()].filter((s) => s.channel_id === c.id);
    const from = winById(c.window_id);
    if (from && tabs.length && tabs.every((s) => s.hidden)) {
      from.channel_ids = from.channel_ids.filter((x) => x !== c.id);
      c.window_id = null;
      broadcast("channel_upserted", c);
      if (!removeWindowIfEmpty(from)) upsertWindow(from);
    }
    return;
  }
  let target: number;
  if (wins.every((w) => w === wins[0])) target = wins[0];
  else if (!wins.includes(c.window_id)) {
    target = prefer !== undefined && wins.includes(prefer) ? prefer : wins[0];
  } else return;
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
  // Adopt channels without a (live) window, except a hidden agent's task.
  for (const c of channels) {
    if (c.archived) continue;
    const tabs = [...sessions.values()].filter((s) => s.channel_id === c.id);
    if (tabs.length && tabs.every((s) => s.hidden)) continue;
    if (c.window_id == null || !windows.some((x) => x.id === c.window_id)) placeChannel(c, w);
  }
  for (const s of sessions.values()) {
    if (!s.hidden && !winById(s.window_id)) {
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
  if (s.kind !== "shell" && (from === "working" || state === "working")) {
    s.last_turn_at = now();
    s.cache_cold_at = coldAt(s);
  }
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
    // A restart's old process may exit after its replacement started.
    if (procs.get(s.id)?.child !== child) return;
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
    size_owner: null,
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

// ---------- named agents (docs/PROTOCOL.md "Agents"; src/agents.rs) ----------
// Records, the message log and memory/ files live here; an agent's session is a fake agent like
// any other, in its own tab of an "agents" workspace (the mock runs it in $HOME).

interface MockAgent {
  id: number;
  name: string;
  kind: "claude" | "codex";
  provider: "subscription" | "openrouter";
  model: string | null;
  effort: string | null;
  persona: string;
  permissions: { codex_sandbox: string; claude_mode: string };
  accepts_origins: string[];
  autostart: boolean;
  home_dir: string;
  channel_id: number | null;
  session_id: number | null;
  resumable: boolean;
  created_at: number;
}

const DEFAULT_PERMS = { codex_sandbox: "workspace_write", claude_mode: "default" };
// A short copy of `agent.models` (the real lists live in src/agents.rs `model_catalog`).
const AGENT_MODELS = {
  claude: {
    models: [
      { id: "fable", note: "latest Fable", efforts: null },
      { id: "opus", note: "latest Opus", efforts: null },
      { id: "sonnet", note: "latest Sonnet", efforts: null },
      { id: "haiku", note: "latest Haiku", efforts: null },
    ],
    efforts: ["low", "medium", "high", "xhigh", "max"],
  },
  codex: {
    models: [
      { id: "gpt-6.1-sol", note: null, efforts: null },
      { id: "gpt-6-luna", note: null, efforts: ["low", "medium", "high", "xhigh", "max"] },
    ],
    efforts: ["low", "medium", "high", "xhigh", "max", "ultra"],
  },
};
const AGENT_TEMPLATES = [
  {
    id: "blank",
    label: "Blank",
    about: "Nothing filled in: a Claude agent that takes human-started work.",
    name: "",
    kind: "claude",
    model: null,
    effort: null,
    persona: "",
    permissions: DEFAULT_PERMS,
    accepts: ["human"],
    autostart: false,
  },
  {
    id: "orchestrator",
    label: "Orchestrator",
    about: "Splits work across the other agents and sums up their replies for you.",
    name: "orchestrator",
    kind: "claude",
    model: null,
    effort: null,
    persona:
      "You coordinate the other muster agents for the human. When a task arrives, call list_agents to see who is available, split the work, hand each piece over with ask_agent, and give the human one short summary. Never do large work yourself. Keep notes in memory/.",
    permissions: DEFAULT_PERMS,
    accepts: ["human"],
    autostart: false,
  },
  {
    id: "researcher",
    label: "Researcher",
    about: "Reads and searches the web, answers with sources. Plan mode: it can't change files.",
    name: "researcher",
    kind: "claude",
    model: null,
    effort: null,
    persona:
      'You answer questions by reading files and searching the web. Every claim carries its source. You run in plan mode: end each answer with a short "For memory/" list.',
    permissions: { codex_sandbox: "workspace_write", claude_mode: "plan" },
    accepts: ["human"],
    autostart: false,
  },
  {
    id: "reviewer",
    label: "Reviewer",
    about: "Codex in a read-only sandbox: reviews the diffs or files you point it at.",
    name: "reviewer",
    kind: "codex",
    model: null,
    effort: null,
    persona:
      "You review the diffs or files you are asked about. Report findings ranked by severity (P1, P2, P3), each with the file and line and the smallest fix.",
    permissions: { codex_sandbox: "read_only", claude_mode: "default" },
    accepts: ["human"],
    autostart: false,
  },
  {
    id: "pr_reviewer",
    label: "PR reviewer",
    about: "Reads a GitHub PR, shows what you need to decide in a panel, merges only on your click.",
    name: "pr-reviewer",
    kind: "claude",
    model: null,
    effort: null,
    persona:
      "You help the human decide on GitHub pull requests fast, with a Muster pr_decision panel. Gather with gh, open the panel, wait_for_panel_event, and merge only on a merge event with --match-head-commit <head_sha>.",
    permissions: DEFAULT_PERMS,
    accepts: ["human"],
    autostart: false,
  },
  {
    id: "worker",
    label: "Background worker",
    about: "Codex for routine jobs, also on schedules and webhooks; starts with the daemon.",
    name: "worker",
    kind: "codex",
    model: null,
    effort: null,
    persona:
      "You run routine jobs when asked, by a person, a schedule or a webhook. Post a short result to your channel with post_to_channel and keep a log in memory/log.md.",
    permissions: { codex_sandbox: "workspace_write", claude_mode: "default" },
    accepts: ["human", "schedule", "webhook"],
    autostart: true,
  },
];

const agentList: MockAgent[] = [];
const agentMsgs: any[] = [];
const memory = new Map<string, Map<string, string>>();

function newAgent(p: any): MockAgent {
  const name = String(p.name ?? "").trim();
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]{0,39}$/.test(name)) throw new Error(`bad agent name \`${name}\``);
  if (agentList.some((a) => a.name.toLowerCase() === name.toLowerCase())) {
    throw new Error(`an agent named ${name} already exists`);
  }
  const kind = p.kind === "codex" ? "codex" : "claude";
  const provider = p.provider === "openrouter" ? "openrouter" : "subscription";
  checkProvider(kind, provider, p.model ?? null, true);
  const a: MockAgent = {
    id: seq++,
    name,
    kind,
    provider,
    model: p.model || null,
    effort: p.effort || null,
    persona: p.persona ?? "",
    permissions: p.permissions ?? { ...DEFAULT_PERMS },
    accepts_origins: p.accepts ?? (kind === "claude" ? ["human"] : ["human", "schedule", "webhook"]),
    autostart: !!p.autostart,
    home_dir: `${HOME}/.local/share/muster/agents/${name.toLowerCase()}`,
    channel_id: null,
    session_id: null,
    resumable: false,
    created_at: now(),
  };
  agentList.push(a);
  memory.set(a.name, new Map());
  return a;
}

/** OpenRouter (src/openrouter.rs): whether the mock backend "has" OPENROUTER_API_KEY
 * (`mock.openrouter` sets it), and a short copy of the filtered model list. */
let orKey = false;
const OR_MODELS = [
  ["anthropic/claude-opus-5.5", "Anthropic: Claude Opus 5.5", 1000000, 5, 25],
  ["anthropic/claude-sonnet-5.5", "Anthropic: Claude Sonnet 5.5", 1000000, 2, 10],
  ["anthropic/claude-haiku-5.5", "Anthropic: Claude Haiku 5.5", 200000, 0.8, 4],
  ["~anthropic/claude-opus-latest", "Anthropic: Claude Opus Latest", 1000000, 5, 25],
  ["deepseek/deepseek-v4-pro", "DeepSeek: DeepSeek V4 Pro", 1000000, 0.55, 2.2],
  ["google/gemini-3.8-pro", "Google: Gemini 3.8 Pro", 2000000, 2.5, 15],
  ["google/gemini-3.8-flash", "Google: Gemini 3.8 Flash", 1000000, 0.33, 2.75],
  ["minimax/minimax-m3.1-flash:free", "MiniMax: MiniMax M3.1 Flash (free)", 1000000, 0, 0],
  ["moonshotai/kimi-k3", "MoonshotAI: Kimi K3", 256000, 0.6, 2.5],
  ["openai/gpt-6.1-sol", "OpenAI: GPT-6.1 Sol", 400000, 2, 12],
  ["openai/gpt-6-luna", "OpenAI: GPT-6 Luna", 400000, 0.25, 2],
  ["qwen/qwen3.5-coder", "Qwen: Qwen3.5 Coder", 1000000, 0.3, 1.2],
  ["z-ai/glm-5.2", "Z.ai: GLM 5.2", 200000, 0.6, 2.2],
].map(([id, name, context_length, prompt_per_m, completion_per_m], i) => ({
  id,
  name,
  vendor: String(id).replace(/^~/, "").split("/")[0],
  context_length,
  prompt_per_m,
  completion_per_m,
  created: 1792000000 - i,
  alias_of: String(id).startsWith("~") ? "anthropic/claude-opus-5.5" : null,
}));

function checkProvider(kind: string, provider: string, model: string | null, needKey: boolean) {
  if (provider !== "openrouter") return;
  if (kind === "codex" && !model?.trim()) {
    throw new Error("a Codex agent on OpenRouter needs a model, with its vendor (e.g. openai/gpt-6.1-sol)");
  }
  if (needKey && !orKey) {
    throw new Error("OpenRouter needs the OPENROUTER_API_KEY environment variable on Muster's backend, which doesn't have it.");
  }
}

function agentJson(a: MockAgent) {
  const s = a.session_id !== null ? sessions.get(a.session_id) : undefined;
  return { ...a, state: s?.state ?? null, detail: s?.state_detail ?? null };
}

function agentNamed(name: string): MockAgent {
  const a = agentList.find((x) => x.name.toLowerCase() === String(name ?? "").trim().toLowerCase());
  if (!a) throw new Error(`no agent named ${name}`);
  return a;
}

function pushAgentMessage(m: any) {
  const msg = {
    id: seq++,
    from_session: null,
    to_session: null,
    reply_to: null,
    created_at: now(),
    reply: null,
    replied_at: null,
    refused: null,
    ...m,
  };
  agentMsgs.push(msg);
  broadcast("agent_message", msg);
  return msg;
}

// ---------- schedules (dev/mock_schedules.ts) ----------

const sched = scheduleMock({
  now,
  nextId: () => seq++,
  broadcast,
  agents: () => agentList,
  sendMessage: (a, from, text, refused = null) =>
    pushAgentMessage({
      from,
      to: a.name,
      to_agent: a.id,
      origin: from.startsWith("trigger:") ? "webhook" : "schedule",
      hop: from.includes("/then:") ? 1 : 0,
      text,
      refused,
    }),
  reply: (m, text) => {
    m.reply = text;
    m.replied_at = now();
    broadcast("agent_message", m);
  },
  note: (c) => {
    if (c.id !== undefined) {
      const n = getNote(c.id);
      return { id: n.id, title: n.title };
    }
    const t = String(c.title ?? "").trim();
    const hit = [...notes.values()].find((n) => n.title.toLowerCase() === t.toLowerCase());
    const n = hit ?? (noteMethod("note.create", { title: t, body: "" }) as Note);
    return { id: n.id, title: n.title };
  },
  noteTitle: (id) => notes.get(id)?.title ?? null,
  noteTriggers: (id) => (notes.has(id) ? !!notes.get(id)!.triggers : null),
});

// Seed (before the conversation below, so these older messages sort under it): a Codex worker
// with a weekday summary (mixed history) and a paused hourly check, and a Claude subscription
// agent that opted in to scheduled work, whose schedule expires tomorrow.
{
  const worker = newAgent({ ...AGENT_TEMPLATES[5], autostart: false });
  const digest = newAgent({
    name: "digest",
    kind: "claude",
    persona: "You write a short morning digest of what changed overnight.",
    accepts: ["human", "schedule"],
  });
  worker.created_at = digest.created_at = now() - 8 * 24 * 3600_000;
  const day = (n: number) => sched.weekdayAt(n);
  sched.seed(
    {
      agent: worker.name,
      name: "Daily PR summary",
      prompt: "Summarise the pull requests opened since {last_run} and post the summary to your channel.",
      when: { kind: "cron", cron: "0 9 * * 1-5" },
    },
    9 * 24 * 3600_000,
    [
      { slot: day(4), outcome: "replied", mins: 4, reply: "3 PRs opened: #812 (auth retry), #815 (docs), #816 (deps). None blocked." },
      { slot: day(3), outcome: "skipped", reason: "busy: the agent was busy for an hour" },
      {
        slot: day(2),
        outcome: "refused",
        reason: "agent worker already got 24 unattended (scheduled or triggered) messages in the last 24 hours",
      },
      {
        slot: day(1) + 2 * 3600_000,
        kind: "catch_up",
        outcome: "replied",
        mins: 6,
        reason: "catch-up: missed while Muster was off or asleep",
        reply: "1 PR opened: #820 (schedules UI). CI is green.",
      },
      { slot: day(0), outcome: "replied", mins: 4, reply: "2 PRs opened: #821 (mock), #822 (e2e). #821 waits for review." },
    ],
  );
  const ci = sched.seed(
    {
      agent: worker.name,
      name: "CI check",
      prompt: "Check CI on master and say if anything is red.",
      when: { kind: "cron", cron: "0 * * * *" },
      paused: true,
    },
    3 * 24 * 3600_000,
    [{ slot: now() - 26 * 3600_000, outcome: "blocked", mins: 2, reason: "stopped on an approval prompt: gh run list" }],
  );
  void ci;
  sched.seed(
    {
      agent: digest.name,
      name: "Morning digest",
      prompt: "Write this morning's digest of what changed overnight ({date}).",
      when: { kind: "cron", cron: "30 8 * * *" },
    },
    6.5 * 24 * 3600_000,
    [{ slot: day(0) - 30 * 60_000, outcome: "replied", mins: 3, reply: "Overnight: 2 merges, no failed builds." }],
  );
}

// Seed: two agents and a short conversation, so the panel has something to show.
{
  const orch = newAgent({ ...AGENT_TEMPLATES[1], accepts: ["human"] });
  const rev = newAgent({ ...AGENT_TEMPLATES[3] });
  orch.created_at = rev.created_at = now() - 3 * 3600_000;
  memory.get(orch.name)!.set(
    "notes.md",
    "# Notes\n\n- reviewer is good at Rust diffs\n- release notes due Friday\n",
  );
  memory.get(orch.name)!.set("people/josh.md", "Prefers short summaries.\n");
  const t0 = now() - 40 * 60_000;
  agentMsgs.push(
    {
      id: seq++,
      from: "human",
      from_session: null,
      to: orch.name,
      to_agent: orch.id,
      to_session: null,
      origin: "human",
      hop: 0,
      reply_to: null,
      created_at: t0,
      text: "Plan the release notes for v0.4 and get the auth diff reviewed.",
      reply: "Done: notes drafted in memory/release.md; reviewer found one P2 in the auth diff.",
      replied_at: t0 + 9 * 60_000,
      refused: null,
    },
    {
      id: seq++,
      from: orch.name,
      from_session: null,
      to: rev.name,
      to_agent: rev.id,
      to_session: null,
      origin: "human",
      hop: 1,
      reply_to: null,
      created_at: t0 + 2 * 60_000,
      text: "Review the diff in fix-auth: is the token refresh safe?\nFocus on the retry loop.",
      reply: "P2: the retry loop re-sends the old refresh token after a 401 (src/auth.rs:212).",
      replied_at: t0 + 7 * 60_000,
      refused: null,
    },
    {
      id: seq++,
      from: "session:41",
      from_session: 41,
      to: orch.name,
      to_agent: orch.id,
      to_session: null,
      origin: "schedule",
      hop: 2,
      reply_to: null,
      created_at: t0 + 20 * 60_000,
      text: "Nightly: summarise yesterday's merged PRs.",
      reply: null,
      replied_at: null,
      refused:
        "orchestrator accepts only human-started messages, and this one started from a schedule (origin=schedule)",
    },
  );
}

// ---------- notes (like src/notes.rs) ----------

const notes = new Map<number, Note>();
let noteSeq = 0;
const NOTE_MAX = 1024 * 1024;
const isCtl = (c: string) => { const k = c.codePointAt(0)!; return k < 32 || (k >= 127 && k < 160); };
const cleanNote = (s: string) => [...s.replace(/\r\n?/g, "\n")].filter((c) => c === "\n" || c === "\t" || !isCtl(c)).join("");

function noteAuthor(from: number | undefined): NoteAuthor {
  const s = from !== undefined ? sessions.get(from) : undefined;
  if (from !== undefined && !s) throw new Error("calling session no longer exists");
  if (!s || s.kind === "shell") return { kind: "human", name: null, session_id: s?.id ?? null };
  const c = channels.find((x) => x.id === s.channel_id);
  const kind = s.kind === "claude" ? "Claude" : "Codex";
  return { kind: "agent", name: c ? `${kind} (${c.name})` : kind, session_id: s.id };
}

function getNote(id: number): Note {
  const n = notes.get(id);
  if (!n) throw new Error(`no note with id ${id} (see note_list)`);
  if (n.window_id !== null && !winById(n.window_id)) {
    n.window_id = n.beside = null;
    n.version++;
  }
  return n;
}

function saveNote(n: Note): Note {
  n.version++;
  broadcast("note_upserted", n);
  return n;
}

/** Like src/store.rs `load_note_summaries`: no text. */
function noteSummary(n: Note): NoteSummary {
  const { body, ...rest } = n;
  return {
    ...rest, open: n.window_id !== null, bytes: new TextEncoder().encode(body).length,
    lines: body === "" ? 0 : body.split("\n").length - (body.endsWith("\n") ? 1 : 0),
    preview: (body.split("\n").map((l) => l.trim()).find((l) => l) ?? "").slice(0, 120),
  };
}

function noteMethod(method: string, p: any): unknown {
  switch (method) {
    case "note.list":
      return [...notes.keys()].map(getNote).map(noteSummary);
    case "note.search": {
      const q = String(p.query ?? "").trim().toLowerCase();
      if (q.length < 2) return [];
      return [...notes.values()].sort((a, b) => b.updated_at - a.updated_at)
        .filter((n) => n.body.toLowerCase().includes(q)).slice(0, 20)
        .map((n) => ({ id: n.id, line: n.body.split("\n").find((l) => l.toLowerCase().includes(q))!.trim().slice(0, 120) }));
    }
    case "note.get":
      return getNote(p.id);
    case "note.create": {
      const body = cleanNote(p.body ?? "");
      if (body.length > NOTE_MAX) throw new Error("note text would be too long; the limit is 1 MiB");
      if (p.window_id != null && !winById(p.window_id)) throw new Error(`no window with id ${p.window_id}`);
      const id = ++noteSeq;
      const n: Note = { id, title: String(p.title ?? "").trim() || `Note ${id}`, body, rev: 1, created_at: now(), updated_at: now(),
        updated_by: noteAuthor(p.from_session), window_id: p.window_id ?? null, beside: null, version: 0, triggers: false };
      notes.set(id, n);
      return saveNote(n);
    }
    case "note.write": {
      const n = getNote(p.id);
      if (n.rev !== p.base_rev) {
        throw new Error(`note ${n.id} changed since revision ${p.base_rev}: it is at revision ${n.rev}. Read it again (note_read) and write from that revision`);
      }
      const body = cleanNote(String(p.body));
      if (new TextEncoder().encode(body).length > NOTE_MAX) throw new Error("note text would be too long; the limit is 1 MiB (1048576 bytes)");
      if (body === n.body && p.title === undefined) return n;
      if (p.title !== undefined) n.title = String(p.title).trim();
      const changed = body !== n.body;
      if (changed) Object.assign(n, { body, rev: n.rev + 1, updated_at: now(), updated_by: noteAuthor(p.from_session) });
      saveNote(n);
      if (changed && n.updated_by.kind === "human") sched.noteChanged(n.id, n.title);
      return n;
    }
    case "note.set_triggers": {
      // The human's switch only (src/rpc.rs refuses it from an agent's session).
      if (noteAuthor(p.from_session).kind !== "human") throw new Error("only the human allows triggers on a note");
      const n = getNote(p.id);
      n.triggers = !!p.on;
      saveNote(n);
      sched.noteTriggersChanged(n.id);
      return n;
    }
    case "note.append": {
      const n = getNote(p.id);
      const text = cleanNote(String(p.text));
      if (!text) throw new Error("nothing to append");
      const sep = n.body === "" || n.body.endsWith("\n") ? "" : "\n";
      if (new TextEncoder().encode(n.body + sep + text).length > NOTE_MAX) throw new Error("note text would be too long; the limit is 1 MiB");
      Object.assign(n, { body: n.body + sep + text, rev: n.rev + 1, updated_at: now(), updated_by: noteAuthor(p.from_session) });
      saveNote(n);
      if (n.updated_by.kind === "human") sched.noteChanged(n.id, n.title);
      return n;
    }
    case "note.rename": {
      const n = getNote(p.id);
      const t = String(p.name ?? "").trim();
      if (!t || t.length > 200 || [...t].some(isCtl)) throw new Error("a note title must be 1–200 bytes of text on one line");
      n.title = t;
      return saveNote(n);
    }
    case "note.open": {
      const n = getNote(p.id);
      const w = p.window_id ?? (p.from_session !== undefined ? sessions.get(p.from_session)?.window_id : undefined);
      if (!winById(w)) throw new Error(`no window with id ${w}`);
      n.window_id = w;
      n.beside = p.window_id == null && p.from_session !== undefined ? p.from_session : (p.beside ?? null);
      return saveNote(n);
    }
    case "note.close": {
      const n = getNote(p.id);
      const prev = winById(n.window_id);
      n.window_id = n.beside = null;
      saveNote(n);
      removeWindowIfEmpty(prev);
      return n;
    }
    case "note.delete": {
      if (noteAuthor(p.from_session).kind !== "human") throw new Error("only the human deletes notes");
      getNote(p.id);
      notes.delete(p.id);
      broadcast("note_removed", { id: p.id });
      return {};
    }
  }
  throw new Error(`unknown method: ${method}`);
}

async function agentMethod(method: string, p: any): Promise<unknown> {
  switch (method) {
    case "agent.list":
      return agentList.map(agentJson);
    case "agent.templates":
      return AGENT_TEMPLATES;
    case "agent.models":
      return AGENT_MODELS;
    case "agent.create": {
      const a = newAgent(p);
      broadcast("agent_upserted", agentJson(a));
      return agentJson(a);
    }
    case "agent.update": {
      const a = agentNamed(p.name);
      const provider = p.provider === undefined ? a.provider : p.provider === "openrouter" ? "openrouter" : "subscription";
      const model = "model" in p ? String(p.model).trim() || null : a.model;
      checkProvider(a.kind, provider, model, provider !== a.provider);
      a.provider = provider;
      for (const k of ["model", "effort"] as const) if (k in p) a[k] = String(p[k]).trim() || null;
      if ("persona" in p) a.persona = p.persona;
      if ("autostart" in p) a.autostart = !!p.autostart;
      if ("permissions" in p) a.permissions = p.permissions;
      if ("accepts" in p) {
        if (!p.accepts.length) {
          throw new Error("accepts needs at least one origin (human, schedule, webhook)");
        }
        a.accepts_origins = p.accepts;
      }
      broadcast("agent_upserted", agentJson(a));
      return agentJson(a);
    }
    case "agent.remove": {
      const a = agentNamed(p.name);
      if (a.session_id !== null && sessions.has(a.session_id)) {
        await handle("session.remove_now", { id: a.session_id });
      }
      agentList.splice(agentList.indexOf(a), 1);
      broadcast("agent_removed", { id: a.id, name: a.name });
      return { removed: agentJson(a) };
    }
    case "agent.start": {
      const a = agentNamed(p.name);
      const cur = a.session_id !== null ? sessions.get(a.session_id) : undefined;
      if (cur && cur.state !== "exited" && !p.fresh) {
        const pending = (cur.provider ?? "subscription") !== a.provider;
        return { agent: agentJson(a), session: cur, started: false, provider_pending: pending };
      }
      if (cur) {
        // The run bills to what the agent says now (src/hub.rs start_process).
        cur.provider = a.provider;
        const s = await handle("session.restart", { id: cur.id });
        broadcast("agent_upserted", agentJson(a));
        return { agent: agentJson(a), session: s, started: true };
      }
      let ws = workspaces.find((w) => w.name === "agents" && w.path === HOME);
      if (!ws) {
        ws = { id: seq++, name: "agents", path: HOME, is_git: false, created_at: now() };
        workspaces.push(ws);
        broadcast("workspace_upserted", ws);
      }
      let c = channels.find((x) => x.id === a.channel_id && !x.archived);
      if (!c) {
        c = await handle("channel.create", {
          workspace_id: ws.id,
          name: a.name,
          window_id: p.window_id,
          permissions: a.permissions,
        });
        a.channel_id = c.id;
      }
      const s = spawn({ channel_id: c.id, kind: a.kind, name: a.name, window_id: p.window_id, cwd: HOME });
      s.provider = a.provider;
      broadcast("session_upserted", s);
      a.session_id = s.id;
      broadcast("agent_upserted", agentJson(a));
      return { agent: agentJson(a), session: s, started: true };
    }
    case "agent.messages": {
      const to = p.agent ? agentNamed(p.agent).id : null;
      const limit = Math.min(200, Math.max(1, p.limit ?? 50));
      const all = agentMsgs
        .filter((m) => (to === null || m.to_agent === to) && (p.before == null || m.id < p.before))
        .sort((x, y) => y.id - x.id);
      return { messages: all.slice(0, limit), more: all.length > limit };
    }
    case "agent.memory.list": {
      const a = agentNamed(p.name);
      const files = [...(memory.get(a.name) ?? new Map()).entries()]
        .map(([path, text]) => ({ path, size: text.length, modified: now() }))
        .sort((x, y) => x.path.localeCompare(y.path));
      return { dir: `${a.home_dir}/memory`, files };
    }
    // Same rules as the daemon (`agents::memory_add`): one visible name, never replaces (a
    // taken name gets -2, -3, ...), text only, at most 256 KB.
    case "agent.memory.add": {
      const a = agentNamed(p.name);
      const name = String(p.file_name ?? "");
      const bad = memoryFileNameError(name);
      if (bad) throw new Error(`can't add \`${name}\`: ${bad}`);
      if (typeof p.text !== "string") throw new Error("text is required");
      if (new TextEncoder().encode(p.text).length > 256 * 1024) throw new Error(`${name} is larger than 256 KB`);
      if (p.text.includes("\0")) throw new Error(`${name} isn't a text file`);
      const files = memory.get(a.name) ?? new Map<string, string>();
      memory.set(a.name, files);
      const dot = name.lastIndexOf(".");
      const nth = (i: number) =>
        i === 1 ? name : dot > 0 ? `${name.slice(0, dot)}-${i}${name.slice(dot)}` : `${name}-${i}`;
      // A folder (any `name/...` file) takes the name too.
      const taken = (n: string) => files.has(n) || [...files.keys()].some((k) => k.startsWith(n + "/"));
      let i = 1;
      while (i < 100 && taken(nth(i))) i++;
      if (i === 100) throw new Error(`memory/ already has ${name} and 98 more like it`);
      files.set(nth(i), p.text);
      return { path: nth(i), size: p.text.length, renamed: i > 1 };
    }
    case "agent.memory.delete": {
      const a = agentNamed(p.name);
      const path = String(p.path ?? "");
      const files = memory.get(a.name) ?? new Map<string, string>();
      if (!files.has(path)) throw new Error(`memory/${path}: No such file or directory`);
      files.delete(path);
      return { path };
    }
    case "agent.memory.read":
    case "agent.memory.write": {
      const a = agentNamed(p.name);
      const path = String(p.path ?? "");
      if (!path || path.startsWith("/") || path.split("/").some((x) => !x || x === "." || x === "..")) {
        throw new Error(`memory paths can't contain \`..\`, \`.\` or empty parts (got \`${path}\`)`);
      }
      const files = memory.get(a.name) ?? new Map<string, string>();
      memory.set(a.name, files);
      if (method === "agent.memory.read") {
        if (!files.has(path)) throw new Error(`reading memory/${path}: No such file or directory`);
        return { path, text: files.get(path), size: files.get(path)!.length };
      }
      if (typeof p.text !== "string") throw new Error("text is required");
      if (p.text.length > 256 * 1024) throw new Error("memory files are at most 256 KB");
      // A new file's name reads as what it is; an existing one keeps its name (agents::memory_write).
      if (!files.has(path) && path.split("/").some(hiddenCharsOrSpaces)) {
        throw new Error(`\`${path}\` has control or invisible characters, or spaces around a part`);
      }
      files.set(path, p.text);
      return { path, size: p.text.length };
    }
    // Dev only: a message arriving from an agent (e2e checks the log updates live).
    case "mock.agent_message": {
      const a = agentNamed(p.to);
      return pushAgentMessage({
        from: p.from ?? "human",
        to: a.name,
        to_agent: a.id,
        origin: p.origin ?? "human",
        hop: p.hop ?? 0,
        text: p.text ?? "",
        refused: p.refused ?? null,
      });
    }
  }
  throw new Error(`unknown method ${method}`);
}

function stripAnsi(s: string) {
  // deno-lint-ignore no-control-regex
  return s.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "").replace(/\x1b\][^\x07]*\x07/g, "").replace(/\r/g, "");
}

async function handle(method: string, p: any, ws?: WebSocket): Promise<unknown> {
  if (
    ws && deviceClients.has(ws) && ![
      "hello",
      "session.attach",
      "session.detach",
      "session.resize",
      "session.send",
      "session.input",
      "session.approve",
      "session.interrupt",
      "channel.mark_read",
    ].includes(method)
  ) throw new Error("This device does not have permission for " + method);
  // Panel loading under a flaky connection: delay resource replies, drop every client.
  if (method === "mock.panel_delay") {
    panelResourceDelay = Number(p.ms) || 0;
    return {};
  }
  if (method === "mock.drop_clients") {
    setTimeout(() => { for (const c of clients) c.close(); }, 150);
    return {};
  }
  if (method === "mock.updates") {
    // Pretend a build or UI release is waiting: {ui_release?, backend?, host?}.
    Object.assign(updates, p);
    broadcast("updates_changed", updates);
    return updates;
  }
  if (method === "mock.cache_idle") {
    // Pretend session `id`'s last turn ended `idle_ms` ago (the cold-cache bar, docs/UI.md).
    const s = sessions.get(p.id);
    if (!s) throw new Error("no such session");
    s.last_turn_at = now() - Number(p.idle_ms);
    s.state_since = s.last_turn_at;
    s.cache_cold_at = coldAt(s);
    if (s.state === "working" || s.state === "starting") s.state = "idle";
    upsert(s);
    return s;
  }
  if (method === "mock.openrouter") {
    // Pretend the backend was (re)started with or without OPENROUTER_API_KEY: {key_present}.
    orKey = !!p.key_present;
    return { key_present: orKey };
  }
  if (method === "openrouter.models") {
    return { key_present: orKey, models: OR_MODELS, fetched_at: now() - 20 * 60_000, stale: false, error: null };
  }
  if (method.startsWith("schedule.") || method.startsWith("trigger.")) return sched.handle(method, p);
  if (method.startsWith("agent.") || method.startsWith("mock.")) return agentMethod(method, p);
  if (method.startsWith("note.")) return noteMethod(method, p);
  switch (method) {
    case "panel.open": {
      const s = sessions.get(p.from_session);
      if (!s) throw new Error("calling session not found");
      const args = p.arguments;
      if (args.app !== "custom" && !builtinApps[args.app]) throw new Error("unknown app");
      // Like src/panels.rs check_kit: the built-ins already draw with the kit.
      if (args.kit !== undefined && args.app !== "custom") throw new Error("kit applies to custom apps only (the built-in apps already use it)");
      const csp = structuredClone(args.csp ?? {});
      const data = await mockPanelData(args.app, channels.find((c) => c.id === s.channel_id)?.cwd ?? HOME, args.data ?? {}, csp, true);
      const id = ++panelSeq;
      const panel: Panel = { id, title: args.title, app: args.app, channel_id: s.channel_id, session_id: s.id,
        window_id: s.window_id, csp, revision: 1, data_revision: 1, html_revision: 1, kit: args.kit === true, created_at: now(), updated_at: now(), closed: false };
      panels.set(id, { panel, html: args.app === "custom" ? args.html : builtinApps[args.app], data });
      broadcast("panel_upserted", panel);
      return { panel_id: id };
    }
    case "panel.resource": {
      if (panelResourceDelay) await new Promise((r) => setTimeout(r, panelResourceDelay));
      const resource = panels.get(p.id);
      if (!resource || resource.panel.closed) throw new Error("panel not found");
      // Like src/panel_kit.rs: fonts for a built-in, the kit in front of a kit panel's HTML.
      return { ...resource, html: await servePanelHtml(resource.panel.app, !!resource.panel.kit, resource.html) };
    }
    case "panel.update": {
      const args = p.arguments;
      const resource = panels.get(args.panel_id);
      if (!resource || resource.panel.closed) throw new Error("panel not found");
      if (resource.panel.channel_id !== sessions.get(p.from_session)?.channel_id) throw new Error("panel belongs to another task");
      if (args.kit !== undefined && resource.panel.app !== "custom") throw new Error("kit applies to custom apps only (the built-in apps already use it)");
      resource.panel.revision++;
      if (args.html !== undefined && args.html !== resource.html) { resource.html = args.html; resource.panel.html_revision = resource.panel.revision; }
      // Switching the kit reloads the app with the same HTML.
      if (args.kit !== undefined && args.kit !== !!resource.panel.kit) { resource.panel.kit = args.kit; resource.panel.html_revision = resource.panel.revision; }
      if (args.data !== undefined) {
        const cwd = channels.find((c) => c.id === resource.panel.channel_id)?.cwd ?? HOME;
        resource.data = await mockPanelData(resource.panel.app, cwd, args.data, structuredClone(resource.panel.csp), false);
        resource.panel.data_revision = resource.panel.revision;
      }
      if (args.title !== undefined) resource.panel.title = args.title;
      broadcast("panel_upserted", resource.panel);
      return {};
    }
    case "panel.dismiss":
    case "panel.close": {
      const id = p.id ?? p.arguments?.panel_id;
      const resource = panels.get(id);
      if (!resource) throw new Error("panel not found");
      if (!resource.panel.closed && method === "panel.dismiss") panelEvents.push({ panel_id: id, kind: "closed", payload: {}, at: now() });
      resource.panel.closed = true;
      broadcast("panel_upserted", resource.panel);
      return {};
    }
    case "panel.move": {
      const r = panels.get(p.id);
      if (!r || r.panel.closed || !winById(p.window_id)) throw new Error("panel or window not found");
      const prev = winById(r.panel.window_id);
      r.panel.window_id = p.window_id;
      broadcast("panel_upserted", r.panel);
      removeWindowIfEmpty(prev);
      return {};
    }
    case "panel.navigate": {
      // Like src/panels.rs `navigate`: complete what was typed, check it, store it, tell the agent.
      const r = panels.get(p.id);
      if (!r || r.panel.closed || r.panel.app !== "web_preview") throw new Error("only a web preview has an address bar");
      const t = String(p.url ?? "").trim();
      const typed = /^\d{1,5}$/.test(t) ? `http://localhost:${t}/` : /^[a-z][a-z0-9+.-]*:\/\//i.test(t) || !t ? t : `http://${t}`;
      let url: string;
      try { url = mockPreviewUrl(typed).href; } catch (e) {
        throw new Error(String((e as Error).message).replace(/web_preview frames only local dev servers.*/, "The preview shows only local dev servers (localhost or 127.0.0.1 with a port); open other pages in your browser."));
      }
      const previous = String(r.data.url);
      if (url === previous) return { url, changed: false };
      r.panel.revision++;
      r.panel.data_revision = r.panel.revision;
      r.data = { url };
      broadcast("panel_upserted", r.panel);
      panelEvents.push({ panel_id: p.id, kind: "navigated", payload: { url, previous }, at: now() });
      return { url, changed: true };
    }
    case "panel.preview_check": {
      const r = panels.get(p.id);
      if (!r || r.panel.closed || r.panel.app !== "web_preview") throw new Error("panel is not a web_preview");
      return await mockPreviewCheck(String(r.data.url));
    }
    case "panel.event":
      if (!panels.has(p.panel_id)) throw new Error("panel not found");
      panelEvents.push({ ...p, at: now() });
      return {};
    case "panel.context": return {};
    case "panel.tool":
      if (p.name !== "get_diff" || p.arguments?.channel_id !== undefined) throw new Error("tool not allowed");
      return { content: [{ type: "text", text: "Mock diff" }], isError: false };
    case "panel.list": return { panels: [...panels.values()].map((r) => r.panel).filter((x) => !x.closed && x.channel_id === sessions.get(p.from_session)?.channel_id) };
    case "panel.wait": {
      const wanted = (e: any) => panels.get(e.panel_id)?.panel.channel_id === sessions.get(p.from_session)?.channel_id && (p.arguments?.panel_id === undefined || p.arguments.panel_id === e.panel_id);
      const found = panelEvents.filter(wanted);
      for (let i = panelEvents.length - 1; i >= 0; i--) if (wanted(panelEvents[i])) panelEvents.splice(i, 1);
      return { events: found, timed_out: !found.length };
    }
    case "hello":
      return {
        version: "0.0.0-mock",
        workspaces,
        channels: channels.filter((c) => !c.archived),
        sessions: [...sessions.values()],
        panels: [...panels.values()].map((r) => r.panel).filter((p) => !p.closed),
        panel_origin: panelOrigin,
        notes: [...notes.keys()].map(getNote).map(noteSummary),
        windows,
        home: HOME,
        default_shell: "/bin/bash",
        updates,
        prefs: windowPrefs,
        openrouter: { key_present: orKey },
      };
    case "prefs.get":
      return windowPrefs;
    case "prefs.set":
      for (const [k, v] of Object.entries(p.cache_ttl ?? {})) {
        if (!(k in windowPrefs.cache_ttl)) throw new Error(`unknown field \`${k}\``);
        if (!Number.isInteger(v) || (v as number) < 1 || (v as number) > 1440) {
          throw new Error(`cache_ttl.${k}: 1 to 1440 minutes`);
        }
      }
      if (typeof p.decorations === "boolean") windowPrefs.decorations = p.decorations;
      if (typeof p.cache_warn === "boolean") windowPrefs.cache_warn = p.cache_warn;
      Object.assign(windowPrefs.cache_ttl, p.cache_ttl ?? {});
      for (const s of sessions.values()) {
        const ttlBefore = s.cache_ttl_min ?? null;
        const at = coldAt(s);
        if (at !== (s.cache_cold_at ?? null) || ttlBefore !== s.cache_ttl_min) {
          s.cache_cold_at = at;
          upsert(s);
        }
      }
      windowPrefs.revision++;
      broadcast("prefs_changed", windowPrefs);
      return windowPrefs;
    case "daemon.reload":
      // Only the offered build, as the backend checks.
      if (!updates.backend) throw new Error("the backend already runs the build at /mock/target/release/muster");
      if (p.expect !== updates.backend) throw new Error("the build changed since the update was offered");
      // A new backend: every connection drops and reconnects (terminals keep running).
      updates.backend = null;
      setTimeout(() => {
        for (const c of clients) c.close();
      }, 150);
      return { exe: "/mock/target/release/muster" };
    case "daemon.shutdown":
      broadcast("daemon_stopping", { reason: "shutdown" });
      for (const s of sessions.values()) if (s.state !== "exited") kill(s.id);
      return {};
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
        throw new Error(
          `session ${s.id} is no longer in window ${p.from_window_id} (now in ${s.window_id ?? "none"})`,
        );
      }
      if (s.window_id === w.id && !s.hidden) return s;
      const from = winById(s.window_id);
      s.window_id = w.id;
      s.hidden = false;
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
      for (const [id, r] of panels) if (r.panel.channel_id === c.id) {
        panels.delete(id);
        broadcast("panel_removed", { id });
      }
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
      if (!sessions.has(p.id)) throw new Error("no such session");
      return { __attach: p.id, replay_bytes: p.replay_bytes };
    }
    case "session.detach":
      if (ws) attached.get(ws)?.delete(p.id);
      return {};
    case "session.resize": {
      const s = sessions.get(p.id);
      if (s) {
        s.cols = p.cols;
        s.rows = p.rows;
        s.size_owner = ws && deviceClients.has(ws) ? `device:${device.id}` : "local";
        upsert(s);
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
      // A new conversation: nothing cached to lose (src/hub.rs start_process; no --resume here).
      s.last_turn_at = null;
      s.cache_cold_at = null;
      upsert(s);
      startProcess(s);
      upsert(s);
      return s;
    }
    case "session.remove":
    case "session.remove_now": {
      const s = sessions.get(p.id);
      if (!s) throw new Error(`no such session: ${p.id}`);
      // As the daemon: closing a named agent's tab hides it (`agent.remove` removes it for real,
      // through the mock-only "session.remove_now").
      if (method === "session.remove" && agentList.some((a) => a.session_id === s.id)) {
        return { hidden: true, session: await handle("session.hide", { id: s.id }) };
      }
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
    case "session.hide": {
      // A named agent's tab closed: it runs on in no window (the real daemon: hub::hide_session).
      const s = sessions.get(p.id);
      if (!s) throw new Error(`no such session: ${p.id}`);
      if (!agentList.some((a) => a.session_id === s.id)) {
        throw new Error(`session ${s.id} is not a named agent's; only an agent's tab can be hidden`);
      }
      if (s.hidden) return s;
      const from = winById(s.window_id);
      s.window_id = null;
      s.hidden = true;
      upsert(s);
      taskFollows(channels.find((c) => c.id === s.channel_id));
      removeWindowIfEmpty(from);
      return s;
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
      if (!c || c.archived || [...sessions.values()].some((s) => s.channel_id === c.id)) {
        return { closed: false };
      }
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
      if (p.state_since !== undefined && (p.state_since !== s.state_since || s.state !== "needs_approval")) {
        throw new Error("Approval is stale: the prompt changed");
      }
      logEvent(s.channel_id, s.id, "approval", { session_id: s.id, decision: p.decision });
      if (p.decision === "deny") setState(s, "idle");
      else {
        setState(s, "working", "Running cargo test");
        setTimeout(() => s.state === "working" && setState(s, "idle"), 2000);
      }
      return {};
    }
    case "session.input":
      write(p.id, String(p.data));
      return {};
    case "session.interrupt":
      write(p.id, sessions.get(p.id)?.kind === "shell" ? "\x03" : "\x1b");
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

function onSocket(ws: WebSocket, isDevice = false) {
  ws.binaryType = "arraybuffer";
  clients.add(ws);
  if (isDevice) deviceClients.add(ws);
  attached.set(ws, new Set());
  ws.onclose = () => {
    clients.delete(ws);
    deviceClients.delete(ws);
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
        const replay = concat(scrollback.get(sid) ?? []);
        const limit = typeof result.replay_bytes === "number"
          ? Math.max(0, result.replay_bytes)
          : replay.length;
        ws.send(frame(3, sid, replay.subarray(Math.max(0, replay.length - limit))));
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
  webmanifest: "application/manifest+json",
  svg: "image/svg+xml",
  png: "image/png",
};

Deno.serve({ port: PORT, hostname: "127.0.0.1" }, async (req) => {
  const url = new URL(req.url);
  const cookie = /(?:^|;\s*)muster_device=mock-device(?:;|$)/.test(req.headers.get("cookie") ?? "");
  if (url.pathname === "/api/device/me" && req.method === "GET") {
    return cookie
      ? Response.json({ device })
      : Response.json({ error: { message: "Unpaired" } }, { status: 401 });
  }
  if (url.pathname.startsWith("/api/device/") && req.method === "POST") {
    if (req.headers.get("X-Muster") !== "1") return new Response("missing X-Muster", { status: 403 });
    if (url.pathname === "/api/device/pair") {
      while (pairAttempts[0] < now() - 60000) pairAttempts.shift();
      if (pairAttempts.length >= 5) {
        return Response.json({ error: { message: "Too many pairing attempts" } }, { status: 429 });
      }
      pairAttempts.push(now());
      const body = await req.json().catch(() => ({}));
      if (body.code !== "ABCDEFGHJ" && !/^[a-f0-9]{64}$/i.test(body.code ?? "")) {
        return Response.json({
          error: { message: "Code expired or incorrect. Run muster pair for a new code." },
        }, { status: 403 });
      }
      device.name = body.name || "Android phone";
      return Response.json({ device }, {
        headers: {
          "set-cookie": "muster_device=mock-device; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000",
        },
      });
    }
    if (url.pathname === "/api/device/logout") {
      for (const ws of deviceClients) ws.close();
      return Response.json({}, {
        headers: { "set-cookie": "muster_device=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0" },
      });
    }
  }
  if (url.pathname === "/ws") {
    const isDevice = !url.searchParams.has("token") && cookie;
    if (url.searchParams.get("token") !== TOKEN && !isDevice) {
      return new Response("unauthorized", { status: 401 });
    }
    const { socket, response } = Deno.upgradeWebSocket(req);
    onSocket(socket, isDevice);
    return response;
  }
  if (url.pathname === "/api/health") return Response.json({ ok: true, version: "mock" });
  if (url.pathname === "/m") return Response.redirect(`${url.origin}/m/`, 302);
  const path = url.pathname === "/"
    ? "index.html"
    : url.pathname === "/m/"
    ? "m/index.html"
    : url.pathname.slice(1);
  if (path.includes("..")) return new Response("no", { status: 400 });
  try {
    const body = await Deno.readFile(new URL(path, DIST));
    const ext = path.split(".").pop() ?? "";
    return new Response(body, {
      headers: {
        "content-type": TYPES[ext] ?? "application/octet-stream",
        "cache-control": "no-store",
        ...(path.startsWith("m/")
          ? {
            "content-security-policy":
              "default-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
          }
          : {}),
      },
    });
  } catch {
    return new Response("not found", { status: 404 });
  }
});

console.log(`mock daemon: http://127.0.0.1:${PORT}/#token=${TOKEN}`);
