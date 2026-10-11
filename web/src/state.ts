// App state as signals. Server state is replaced/merged from hello + ServerEvents; this window's
// layout (its split tree of groups) is owned here and saved with window.set_layout; UI prefs
// persist to localStorage.

import { batch, computed, effect, signal } from "@preact/signals";
import type {
  Agent,
  AgentMessage,
  AgentState,
  Channel,
  ChannelId,
  HelloResult,
  ServerEvent,
  SessionId,
  SessionInfo,
  TimelineEvent,
  Win,
  WindowId,
  Workspace,
} from "./types.ts";
import { needsHuman } from "./types.ts";
import { rpc } from "./rpc.ts";
import { sessionSizeChanged } from "./terms.ts";
import { debounce, load, save } from "./util.ts";
import { upsertMessage } from "./agents.ts";
import {
  allSessions,
  emptyLayout,
  findPane,
  type LayoutState,
  type PaneNode,
  parseLayout,
  reconcile,
} from "./layout/tree.ts";
import {
  confirmAfterFocus,
  confirmAfterUpsert,
  documentTitle,
  needsYouOrder,
  pushFront,
  sessionsOfWindow,
  SessionVersions,
  sessionWindowId,
  upsertWindow,
} from "./model.ts";

// ---------------- server state ----------------

export const helloDone = signal(false);
export const meta = signal({ version: "", home: "", default_shell: "" });
export const workspaces = signal<Workspace[]>([]);
export const channels = signal<Channel[]>([]);
export const sessions = signal<Map<SessionId, SessionInfo>>(new Map());

/** Every window the daemon knows (all clients). */
export const windows = signal<Win[]>([]);
/** The daemon has windows (hello carried `windows`). Without them every session is a tab here. */
export const windowsSupported = signal(false);
/** The window this client claimed (null until claimed, or without window support). */
export const windowId = signal<WindowId | null>(null);

export const channelById = computed(() => new Map(channels.value.map((c) => [c.id, c])));
export const workspaceById = computed(() => new Map(workspaces.value.map((w) => [w.id, w])));
export const windowById = computed(() => new Map(windows.value.map((w) => [w.id, w])));

/** Sessions per task (channel), oldest first. */
export const sessionsByChannel = computed(() => {
  const m = new Map<ChannelId, SessionInfo[]>();
  for (const s of sessions.value.values()) {
    let arr = m.get(s.channel_id);
    if (!arr) m.set(s.channel_id, arr = []);
    arr.push(s);
  }
  for (const arr of m.values()) arr.sort((a, b) => a.created_at - b.created_at || a.id - b.id);
  return m;
});

export const myWindow = computed(() =>
  windowId.value !== null ? windowById.value.get(windowId.value) ?? null : null
);

/** The sessions shown as tabs in this window (see `sessionsOfWindow`). */
export const windowSessions = computed((): SessionInfo[] => {
  if (!windowsSupported.value) return sessionsOfWindow(null, sessions.value.values(), channels.value);
  const w = myWindow.value;
  return w ? sessionsOfWindow(w, sessions.value.values(), channels.value) : [];
});

export const hereIds = computed(() => new Set(windowSessions.value.map((s) => s.id)));

// ---------------- named agents (docs/UI.md "Agents panel") ----------------

/** Every named agent, by name. Loaded after each hello (`agent.list`), then kept by events. */
export const agents = signal<Agent[]>([]);
/** The daemon knows agents (false: an older daemon answered "unknown method"). */
export const agentsSupported = signal(true);
/** The message log, newest first (`agent.messages`, then `agent_message` events). */
export const agentMessages = signal<AgentMessage[]>([]);
/** The panel's selected agent: the log shows only messages to it. */
export const agentFilter = signal<number | null>(null);

export const agentBySession = computed(() => {
  const m = new Map<SessionId, Agent>();
  for (const a of agents.value) if (a.session_id !== null) m.set(a.session_id, a);
  return m;
});

function sortAgents(list: Agent[]): Agent[] {
  return [...list].sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
}

async function loadAgents() {
  try {
    const [list, log] = await Promise.all([
      rpc.call<Agent[]>("agent.list"),
      rpc.call<{ messages: AgentMessage[] }>("agent.messages", { limit: 100 }),
    ]);
    agentsSupported.value = true;
    agents.value = sortAgents(list);
    agentMessages.value = log.messages;
  } catch (e) {
    if (/^unknown method/i.test(e instanceof Error ? e.message : String(e))) agentsSupported.value = false;
  }
}

/** The window a session is a tab in (null: none). Without window support: this one. */
export function windowOfSession(s: SessionInfo): WindowId | null {
  if (!windowsSupported.value) return windowId.value;
  return sessionWindowId(s, channelById.value.get(s.channel_id), windows.value);
}

const liveTaskIds = computed(() => new Set(channels.value.filter((c) => !c.archived).map((c) => c.id)));

/** Sessions needing the human in any window, in Alt+J order. */
export const needsYou = computed(() => needsYouOrder(sessions.value.values(), liveTaskIds.value));

/** How many of those are tabs of this window. */
export const needsYouHere = computed(() => needsYou.value.filter((s) => hereIds.value.has(s.id)).length);

// ---------------- unread ----------------

/** Latest "notable" event per task (what makes its tabs bold/unread). */
export const latestNotable = signal<Map<ChannelId, number>>(new Map());

export function isNotable(e: TimelineEvent): boolean {
  switch (e.kind) {
    case "agent_post":
    case "session_exited":
      return true;
    case "state_change": {
      const to = e.data?.to as AgentState | undefined;
      const from = e.data?.from as AgentState | undefined;
      return !!to && (needsHuman(to) || (to === "idle" && from === "working"));
    }
    case "system":
      return e.data?.level === "warn" || e.data?.level === "error";
    case "prompt":
      // Prompts from agents (MCP) are notable; the human's own are not.
      return !!e.data?.from && !["human", "user", "ui"].includes(String(e.data.from));
    default:
      return false;
  }
}

function noteNotable(e: TimelineEvent) {
  if (e.channel_id === null || !isNotable(e)) return;
  const cur = latestNotable.value.get(e.channel_id) ?? 0;
  if (e.ts > cur) {
    const m = new Map(latestNotable.value);
    m.set(e.channel_id, e.ts);
    latestNotable.value = m;
  }
}

export const unread = computed(() => {
  const out = new Set<ChannelId>();
  for (const c of channels.value) {
    const t = latestNotable.value.get(c.id) ?? 0;
    if (t > c.last_read_at) out.add(c.id);
  }
  return out;
});

// ---------------- prefs ----------------

export interface Prefs {
  fontSize: number;
  theme: "system" | "dark" | "light";
  lastAgent: "claude" | "codex" | "shell" | "none";
  /** Jump recency keys ("s:<id>", "f:<path>"), most recent first. */
  recent: string[];
  /** Folders new tabs were opened in, most recent first. */
  recentFolders: string[];
  statusLine: boolean;
  /** Pass Alt shortcuts (except Alt+Shift+…) through to a focused terminal. */
  altToTerminal: boolean;
  /** Jump / Alt+J focus the other window (window.raise) instead of pulling its tab here. */
  jumpRaises: boolean;
  /** The agents panel's message log is expanded. */
  agentFeed: boolean;
}

const defaultPrefs: Prefs = {
  fontSize: 13,
  theme: "system",
  lastAgent: "claude",
  recent: [],
  recentFolders: [],
  statusLine: true,
  altToTerminal: false,
  jumpRaises: false,
  agentFeed: true,
};

function loadPrefs(): Prefs {
  const raw = load<Partial<Prefs>>("prefs", {});
  const out: Prefs = { ...defaultPrefs };
  for (const k of Object.keys(defaultPrefs) as (keyof Prefs)[]) {
    const v = raw?.[k];
    // Only keep values of the expected type (old builds stored other shapes).
    if (
      v !== undefined && typeof v === typeof defaultPrefs[k] &&
      Array.isArray(v) === Array.isArray(defaultPrefs[k])
    ) {
      (out as unknown as Record<string, unknown>)[k] = v;
    }
  }
  return out;
}

export const prefs = signal<Prefs>(loadPrefs());
const savePrefs = debounce((p: Prefs) => save("prefs", p), 200);
effect(() => savePrefs(prefs.value));

export function setPref<K extends keyof Prefs>(k: K, v: Prefs[K]) {
  prefs.value = { ...prefs.value, [k]: v };
}

export function pushRecent(key: string) {
  setPref("recent", pushFront(prefs.value.recent, key, 30));
}

export function pushRecentFolder(path: string) {
  setPref("recentFolders", pushFront(prefs.value.recentFolders, path.replace(/\/+$/, "") || "/", 12));
}

// ---------------- overlays, toasts, inline UI ----------------

// Overlays (only one modal at a time).
export type Overlay =
  | { kind: "jump"; initial: string }
  | { kind: "new_tab"; worktree?: boolean; cwd?: string }
  | { kind: "shortcuts" }
  | { kind: "permissions"; channelId: number }
  | { kind: "rename_window" }
  /** New agent (no `name`) or Edit agent, on its Settings or Memory tab. */
  | { kind: "agent"; name?: string; tab?: "settings" | "memory" }
  | { kind: "agent_message"; id: number }
  | {
    kind: "confirm";
    title: string;
    body: string;
    confirmLabel: string;
    danger?: boolean;
    run: () => void;
  }
  | null;
export const overlay = signal<Overlay>(null);

export interface Toast {
  id: number;
  level: "info" | "warn" | "error" | "success";
  text: string;
}
export const toasts = signal<Toast[]>([]);
let toastSeq = 0;
export function toast(level: Toast["level"], text: string, ms?: number) {
  const id = ++toastSeq;
  // Collapse exact duplicates that arrive in a burst.
  if (toasts.value.some((t) => t.text === text && t.level === level)) return;
  toasts.value = [...toasts.value, { id, level, text }].slice(-5);
  setTimeout(() => dismissToast(id), ms ?? (level === "error" ? 8000 : 4000));
}
export function dismissToast(id: number) {
  toasts.value = toasts.value.filter((t) => t.id !== id);
}

/** A tab (session) whose close awaits the inline "Close? ⏎ / Esc". */
export const confirmClose = signal<SessionId | null>(null);

/** States in which closing asks first. */
export const BUSY_STATES = new Set<AgentState>(["working", "starting", "needs_approval", "asking_question"]);

/** Alt has been held on its own for a moment (the focused strip shows its Alt+1..9 numbers). */
export const altHeld = signal(false);

/** What is being renamed inline: a task or a session, edited in tab `sid`. */
export const renaming = signal<{ kind: "channel" | "session"; id: number; sid: SessionId } | null>(null);

// ---------------- the window's layout (groups) ----------------

/** This window's split tree of groups. Null until the window is known. */
export const layout = signal<LayoutState | null>(null);
/** The window `layout` belongs to (undefined: none built yet; null: no window support). */
let layoutOwner: WindowId | null | undefined = undefined;

/** This window shows the agents panel (kept in its layout, like zoom). */
export const agentsPanelOpen = computed(() => !!layout.value?.agents);

export const focusedGroup = computed((): PaneNode | null => {
  const l = layout.value;
  return l ? findPane(l.root, l.focus) : null;
});

/** The focused group's visible tab. */
export const focusedSession = computed((): SessionInfo | null => {
  const p = focusedGroup.value;
  if (!p || p.active === null || !hereIds.value.has(p.active)) return null;
  return sessions.value.get(p.active) ?? null;
});

/** The focused tab's task. */
export const focusedTask = computed((): Channel | null => {
  const s = focusedSession.value;
  return s ? channelById.value.get(s.channel_id) ?? null : null;
});

/** The focused tab's session id: changes only when focus moves, not when its metadata does. */
export const focusedSessionId = computed(() => focusedSession.value?.id ?? null);

// A pending "Close?" never survives a focus change.
let confirmFocus: SessionId | null = null;
effect(() => {
  const id = focusedSessionId.value;
  confirmClose.value = confirmAfterFocus(confirmClose.peek(), confirmFocus, id);
  confirmFocus = id;
});

// Sessions we create are placed by the call that creates them (a tab of a given group, or a new
// split). While such a call is in flight, sessions that newly show up in this window are held out
// of the layout, so they don't first land in the focused group, attach there and size the PTY
// for the wrong group. Once no call is pending, anything still held joins the focused group.
let holds = 0;
const held = signal<ReadonlySet<SessionId>>(new Set());

export function holdNewSessions() {
  holds++;
}

/** Release one hold. */
export function releaseNewSessions() {
  holds = Math.max(0, holds - 1);
  if (holds === 0 && held.peek().size) held.value = new Set();
}

/** Session ids the layout shows: this window's, minus held ones, in joining order. */
const liveIds = computed(() => windowSessions.value.filter((s) => !held.value.has(s.id)).map((s) => s.id));

/** The session ids of the layout in on-screen order (groups, then their strips). */
export const layoutOrder = computed(() => (layout.value ? allSessions(layout.value.root) : []));

/**
 * Change the layout. `fn` gets the current one; the result is reconciled with this window's
 * sessions. `place` names sessions the change puts on screen: they stop being held first.
 */
export function updateLayout(fn: (l: LayoutState) => LayoutState, place: SessionId[] = []) {
  const cur = layout.peek();
  if (!cur) return;
  batch(() => {
    if (place.some((id) => held.peek().has(id))) {
      const h = new Set(held.peek());
      for (const id of place) h.delete(id);
      held.value = h;
    }
    const next = reconcile(fn(cur), liveIds.peek());
    if (JSON.stringify(next) !== JSON.stringify(cur)) layout.value = next;
  });
}

/**
 * Build this window's layout when the window becomes known (or changes): the one the daemon
 * stored (`Window.layout`), else, for a window that has none yet (first v2 run, or a daemon
 * without it), one group holding all its sessions.
 */
effect(() => {
  if (!helloDone.value) return;
  const supported = windowsSupported.value;
  const id = windowId.value;
  if (supported && id === null) return;
  const owner = supported ? id : null;
  if (layoutOwner === owner) return;
  const w = supported ? myWindow.peek() : null;
  const stored = w?.layout ? parseLayout(w.layout) : null;
  layoutOwner = owner;
  lastSaved = stored ? JSON.stringify(w!.layout) : "";
  clearTimeout(saveTimer);
  layout.value = reconcile(stored ?? emptyLayout([]), liveIds.peek());
});

// Every change to this window's sessions reconciles the layout: new ones join the focused group,
// gone ones leave (a focused group that empties hands focus to its neighbour), empty groups close.
effect(() => {
  const ids = liveIds.value;
  const cur = layout.peek();
  if (!cur) return;
  const next = reconcile(cur, ids);
  if (JSON.stringify(next) !== JSON.stringify(cur)) layout.value = next;
});

// Save the layout (debounced). The daemon doesn't echo it back; daemons without
// `window.set_layout` just don't keep it (a reload builds one group).
let saveTimer = 0;
let lastSaved = "";
let setLayoutMissing = false;
effect(() => {
  const l = layout.value;
  const id = windowId.value;
  if (!l || id === null || !windowsSupported.value || setLayoutMissing || layoutOwner !== id) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const cur = layout.peek();
    if (!cur || windowId.peek() !== id || layoutOwner !== id) return;
    const json = JSON.stringify(cur);
    if (json === lastSaved) return;
    lastSaved = json;
    rpc.call("window.set_layout", { id, layout: cur }).catch((e) => {
      lastSaved = "";
      if (/^unknown method/i.test(e instanceof Error ? e.message : String(e))) setLayoutMissing = true;
    });
  }, 400);
});

// ---------------- server event handling ----------------

function upsertBy<T extends { id: number }>(arr: T[], item: T): T[] {
  const i = arr.findIndex((x) => x.id === item.id);
  if (i < 0) return [...arr, item];
  const out = [...arr];
  out[i] = item;
  return out;
}

/** Record a channel from an RPC reply (its channel_upserted event may lag). */
export function knowChannel(c: Channel) {
  channels.value = upsertBy(channels.value, c);
}

/** Record a window from an RPC reply. */
export function knowWindow(w: Win) {
  windows.value = upsertWindow(windows.value, w);
}

export const removedSessionListeners: ((id: SessionId) => void)[] = [];
/** Called when the window this client holds disappeared (it must claim again). */
export const windowLostListeners: (() => void)[] = [];

/** While a placement is pending, hold the sessions that appeared in this window since `before`. */
function holdArrivals(before: ReadonlySet<SessionId>) {
  if (holds === 0) return;
  const now = hereIds.peek();
  const h = new Set(held.peek());
  let changed = false;
  for (const id of now) {
    if (!before.has(id) && !h.has(id)) {
      h.add(id);
      changed = true;
    }
  }
  if (changed) held.value = h;
}

/** Apply a change; sessions it brings into this window are held if a placement is pending. */
function withHolds(fn: () => void) {
  batch(() => {
    const before = hereIds.peek();
    fn();
    holdArrivals(before);
  });
}

export function applyHello(h: HelloResult) {
  batch(() => {
    const before = hereIds.peek();
    meta.value = { version: h.version, home: h.home, default_shell: h.default_shell };
    workspaces.value = [...h.workspaces].sort((a, b) => a.created_at - b.created_at || a.id - b.id);
    channels.value = h.channels.filter((c) => !c.archived);
    windowsSupported.value = Array.isArray(h.windows);
    windows.value = [...(h.windows ?? [])].sort((a, b) => a.id - b.id);
    const prev = sessions.value;
    versions.bumpAll();
    sessions.value = new Map(h.sessions.map((s) => [s.id, s]));
    for (const id of prev.keys()) {
      if (!sessions.value.has(id)) { for (const f of removedSessionListeners) f(id); }
    }
    if (helloDone.peek()) holdArrivals(before);
    helloDone.value = true;
  });
  loadAgents();
  // Seed unread state with a little history per task.
  for (const c of h.channels) {
    if (c.archived) continue;
    rpc.call<TimelineEvent[]>("channel.timeline", { id: c.id, limit: 30 }).then((evs) => {
      for (const e of evs) noteNotable(e);
    }).catch(() => {});
  }
}

export function applyEvent(ev: ServerEvent) {
  switch (ev.event) {
    case "workspace_upserted":
      workspaces.value = upsertBy(workspaces.value, ev.data);
      break;
    case "workspace_removed":
      workspaces.value = workspaces.value.filter((w) => w.id !== ev.data.id);
      break;
    case "channel_upserted": {
      if (ev.data.archived) {
        removeChannel(ev.data.id);
        break;
      }
      withHolds(() => (channels.value = upsertBy(channels.value, ev.data)));
      break;
    }
    case "channel_removed":
      removeChannel(ev.data.id);
      break;
    case "window_upserted": {
      // Our own layout is ours: the daemon stores what we send and never echoes it.
      withHolds(() => (windows.value = upsertWindow(windows.value, ev.data)));
      break;
    }
    case "window_removed":
      windows.value = windows.value.filter((w) => w.id !== ev.data.id);
      if (ev.data.id === windowId.value) { for (const f of windowLostListeners) f(); }
      break;
    case "session_upserted": {
      versions.bump(ev.data.id);
      upsertSession(ev.data);
      sessionSizeChanged(ev.data);
      confirmClose.value = confirmAfterUpsert(confirmClose.peek(), ev.data, BUSY_STATES);
      break;
    }
    case "session_removed": {
      versions.bump(ev.data.id);
      if (!sessions.value.has(ev.data.id)) break;
      const m = new Map(sessions.value);
      m.delete(ev.data.id);
      batch(() => {
        sessions.value = m;
        if (held.peek().has(ev.data.id)) {
          const h = new Set(held.peek());
          h.delete(ev.data.id);
          held.value = h;
        }
        if (confirmClose.value === ev.data.id) confirmClose.value = null;
      });
      for (const f of removedSessionListeners) f(ev.data.id);
      break;
    }
    case "event": {
      const e = ev.data;
      noteNotable(e);
      if (e.channel_id !== null) maybeMarkRead(e.channel_id);
      break;
    }
    case "notice":
      toast(ev.data.level === "error" ? "error" : ev.data.level === "warn" ? "warn" : "info", ev.data.text);
      break;
    case "agent_upserted":
      agents.value = sortAgents([...agents.value.filter((a) => a.id !== ev.data.id), ev.data]);
      break;
    case "agent_removed":
      agents.value = agents.value.filter((a) => a.id !== ev.data.id);
      if (agentFilter.value === ev.data.id) agentFilter.value = null;
      break;
    case "agent_message":
      agentMessages.value = upsertMessage(agentMessages.value, ev.data);
      break;
  }
}

/** Record an agent from an RPC reply (its agent_upserted event may lag). */
export function knowAgent(a: Agent) {
  agents.value = sortAgents([...agents.value.filter((x) => x.id !== a.id), a]);
}

const versions = new SessionVersions();

/** Note before an RPC whose reply carries a session; pass the mark to `knowSession`. */
export function sessionMark(): number {
  return versions.mark();
}

/**
 * Record a session (from an event). One that newly shows up in this window (spawned, or
 * moved here from another window) is held while a placement is pending, else it joins the
 * focused group; one that left (moved to another window) leaves the layout.
 */
export function upsertSession(s: SessionInfo) {
  const m = new Map(sessions.value);
  m.set(s.id, s);
  withHolds(() => (sessions.value = m));
}

/**
 * Record a session from an RPC reply of a call that started at `mark` (`sessionMark()`). Ignored
 * when an event about it arrived since: that event is newer (a later move, or its removal).
 */
export function knowSession(s: SessionInfo, mark: number) {
  if (versions.fresh(s.id, mark)) upsertSession(s);
}

/**
 * The first session of task `ch` (a new task's comes as an event, maybe after the reply).
 * Null when none arrives within `ms`.
 */
export function firstSessionOf(ch: ChannelId, ms: number): Promise<SessionInfo | null> {
  const known = sessionsByChannel.peek().get(ch)?.[0];
  if (known) return Promise.resolve(known);
  return new Promise((resolve) => {
    const finish = (s: SessionInfo | null) => {
      clearTimeout(timer);
      // Not from inside the effect's own run.
      queueMicrotask(stop);
      resolve(s);
    };
    const timer = setTimeout(() => finish(null), ms);
    const stop = effect(() => {
      const s = sessionsByChannel.value.get(ch)?.[0];
      if (s) finish(s);
    });
  });
}

function removeChannel(id: ChannelId) {
  channels.value = channels.value.filter((c) => c.id !== id);
}

// ---------------- read tracking ----------------

// One pending mark per task: switching tabs quickly must not drop the first one. Each mark while
// one is pending moves its `seenAt` forward (what was on screen at the latest look).
const readTimers = new Map<ChannelId, { timer: number; seenAt: number }>();
function markReadSoon(ch: ChannelId) {
  const pending = readTimers.get(ch);
  if (pending) {
    pending.seenAt = Date.now();
    return;
  }
  const entry = { timer: 0, seenAt: Date.now() };
  entry.timer = setTimeout(() => {
    readTimers.delete(ch);
    const seenAt = entry.seenAt;
    const c = channelById.value.get(ch);
    if (!c || c.last_read_at >= seenAt) return;
    channels.value = upsertBy(channels.value, { ...c, last_read_at: seenAt });
    rpc.call("channel.mark_read", { id: ch, at: seenAt }).catch(() => {});
  }, 600);
  readTimers.set(ch, entry);
}

export function markRead(ch: ChannelId) {
  if (unread.value.has(ch)) markReadSoon(ch);
}

function maybeMarkRead(ch: ChannelId) {
  if (ch === focusedTask.value?.id && document.visibilityState === "visible" && document.hasFocus()) {
    markReadSoon(ch);
  }
}

effect(() => {
  const c = focusedTask.value;
  if (c) markRead(c.id);
});

addEventListener("focus", () => {
  const c = focusedTask.value;
  if (c) markRead(c.id);
});

effect(() => {
  document.title = documentTitle(windowsSupported.value ? myWindow.value : null, needsYouHere.value);
});
