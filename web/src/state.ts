// App state as signals. Server state is replaced/merged from hello + ServerEvents;
// UI prefs persist to localStorage.

import { batch, computed, effect, signal } from "@preact/signals";
import type {
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
import { debounce, load, save } from "./util.ts";
import { emptyLayout, type LayoutState, parseLayout, reconcile } from "./layout/tree.ts";
import { documentTitle, needsYouOrder, pushFront, tabsOf, upsertWindow, windowIdOf } from "./model.ts";

// ---------------- server state ----------------

export const helloDone = signal(false);
export const meta = signal({ version: "", home: "", default_shell: "" });
export const workspaces = signal<Workspace[]>([]);
export const channels = signal<Channel[]>([]);
export const sessions = signal<Map<SessionId, SessionInfo>>(new Map());

/** Every window the daemon knows (all clients). */
export const windows = signal<Win[]>([]);
/** The daemon has windows (hello carried `windows`). Without them every channel is a tab here. */
export const windowsSupported = signal(false);
/** The window this client claimed (null until claimed, or without window support). */
export const windowId = signal<WindowId | null>(null);

export const channelById = computed(() => new Map(channels.value.map((c) => [c.id, c])));
export const workspaceById = computed(() => new Map(workspaces.value.map((w) => [w.id, w])));
export const windowById = computed(() => new Map(windows.value.map((w) => [w.id, w])));

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

/** This window's tabs, in order. */
export const tabs = computed((): Channel[] => {
  if (!windowsSupported.value) return tabsOf(null, channels.value);
  const w = myWindow.value;
  return w ? tabsOf(w, channels.value) : [];
});

export const tabIdSet = computed(() => new Set(tabs.value.map((c) => c.id)));

/** Window a channel is a tab in (null: none, or no window support). */
export function windowOf(c: Channel): WindowId | null {
  if (!windowsSupported.value) return windowId.value;
  return windowIdOf(c, windows.value);
}

/** Sessions needing the human in any window, in Alt+J order. */
export const needsYou = computed(() =>
  needsYouOrder(sessions.value.values(), new Set(channels.value.filter((c) => !c.archived).map((c) => c.id)))
);

/** How many of those are in this window's tabs. */
export const needsYouHere = computed(() =>
  needsYou.value.filter((s) => tabIdSet.value.has(s.channel_id)).length
);

// ---------------- unread ----------------

/** Latest "notable" event per channel (what makes a tab bold/unread). */
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

// ---------------- UI state ----------------

const activeKey = () => `active.${windowId.value ?? 0}`;
export const activeChannelId = signal<ChannelId | null>(null);

export const activeChannel = computed(() => {
  const id = activeChannelId.value;
  if (id === null || !tabIdSet.value.has(id)) return null;
  return channelById.value.get(id) ?? null;
});

export interface Prefs {
  fontSize: number;
  theme: "system" | "dark" | "light";
  lastAgent: "claude" | "codex" | "shell" | "none";
  /** Jump recency keys ("c:<id>", "s:<id>", "f:<path>"), most recent first. */
  recent: string[];
  /** Folders new tabs were opened in, most recent first. */
  recentFolders: string[];
  statusLine: boolean;
  /** Pass Alt shortcuts (except Alt+Shift+…) through to a focused terminal. */
  altToTerminal: boolean;
  /** Jump / Alt+J focus the other window (window.raise) instead of pulling its tab here. */
  jumpRaises: boolean;
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

// Overlays (only one modal at a time).
export type Overlay =
  | { kind: "jump"; initial: string }
  | { kind: "new_tab"; worktree?: boolean; cwd?: string }
  | { kind: "shortcuts" }
  | { kind: "permissions"; channelId: number }
  | { kind: "rename_window" }
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

// Toasts
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

/** A close awaiting inline confirmation: a session (pane) or a whole tab. */
export type PendingClose = { kind: "session"; id: SessionId } | { kind: "channel"; id: ChannelId };
export const confirmClose = signal<PendingClose | null>(null);
// A pending "Close?" never survives a tab switch.
effect(() => {
  void activeChannelId.value;
  confirmClose.value = null;
});

/** States in which closing asks first. */
export const BUSY_STATES = new Set<AgentState>(["working", "starting", "needs_approval", "asking_question"]);

export function channelBusy(ch: ChannelId): boolean {
  return (sessionsByChannel.value.get(ch) ?? []).some((s) => BUSY_STATES.has(s.state));
}

/** Alt has been held on its own for a moment (tabs show their Alt+1..9 numbers). */
export const altHeld = signal(false);

/** What is being renamed inline. */
export const renaming = signal<{ kind: "channel" | "session"; id: number } | null>(null);

// ---------------- active tab ----------------

let lastOrder: ChannelId[] = [];
// Keep a valid active tab: the stored one for this window, else a neighbour of the one that
// just left (closed or moved to another window), else the first.
effect(() => {
  const list = tabs.value;
  const ids = list.map((c) => c.id);
  const cur = activeChannelId.peek();
  if (!helloDone.value) return;
  if (cur !== null && ids.includes(cur)) {
    lastOrder = ids;
    return;
  }
  let next: ChannelId | null = null;
  if (cur === null) {
    const stored = load<ChannelId | null>(activeKey(), null);
    if (stored !== null && ids.includes(stored)) next = stored;
  } else {
    const i = lastOrder.indexOf(cur);
    const survivors = lastOrder.filter((id) => ids.includes(id));
    if (i >= 0) {
      const before = lastOrder.slice(0, i).filter((id) => ids.includes(id));
      next = survivors[before.length] ?? survivors[before.length - 1] ?? null;
    }
  }
  lastOrder = ids;
  activeChannelId.value = next ?? ids[0] ?? null;
});
effect(() => {
  const id = activeChannelId.value;
  if (id !== null && helloDone.peek()) save(activeKey(), id);
});

// ---------------- layouts ----------------

export const layouts = signal<Map<ChannelId, LayoutState>>(new Map());

const saveTimers = new Map<ChannelId, number>();
const lastSaved = new Map<ChannelId, string>();

function scheduleSave(ch: ChannelId) {
  clearTimeout(saveTimers.get(ch));
  saveTimers.set(
    ch,
    setTimeout(() => {
      saveTimers.delete(ch);
      const l = layouts.value.get(ch);
      if (!l || !channelById.value.has(ch)) return;
      const json = JSON.stringify(l);
      if (lastSaved.get(ch) === json) return;
      lastSaved.set(ch, json);
      rpc.call("channel.set_layout", { id: ch, layout: l }).catch(() => lastSaved.delete(ch));
    }, 400),
  );
}

// Sessions we spawned are placed by the spawn call itself (a tab in a given pane, or a new
// split). While a spawn is in flight, new sessions in that channel are held out of the layout so
// they don't first land in the focused pane, attach there and size the PTY for the wrong pane.
const spawnHolds = new Map<ChannelId, number>();
const held = new Set<SessionId>();

export function holdNewSessions(ch: ChannelId) {
  spawnHolds.set(ch, (spawnHolds.get(ch) ?? 0) + 1);
}

/**
 * Record a session we learned about from an RPC reply. The daemon sends queued events before
 * each reply today, so session_upserted normally arrives first; this keeps placement correct
 * if that ever changes (reconcile drops ids it doesn't know).
 */
export function knowSession(s: SessionInfo) {
  if (sessions.value.has(s.id)) return;
  const m = new Map(sessions.value);
  m.set(s.id, s);
  sessions.value = m;
}

/** Record a channel from an RPC reply (its channel_upserted event may lag). */
export function knowChannel(c: Channel) {
  channels.value = upsertBy(channels.value, c);
}

/** Record a window from an RPC reply. */
export function knowWindow(w: Win) {
  windows.value = upsertWindow(windows.value, w);
}

/** Release one hold; once none remain, anything still held is placed normally. */
export function releaseNewSessions(ch: ChannelId, placed?: SessionId) {
  if (placed !== undefined) held.delete(placed);
  const n = (spawnHolds.get(ch) ?? 1) - 1;
  if (n > 0) {
    spawnHolds.set(ch, n);
    return;
  }
  spawnHolds.delete(ch);
  for (const id of [...held]) if (sessions.value.get(id)?.channel_id === ch) held.delete(id);
  reconcileChannel(ch);
}

export function liveIds(ch: ChannelId): number[] {
  return (sessionsByChannel.value.get(ch) ?? []).filter((s) => !held.has(s.id)).map((s) => s.id);
}

export function layoutOf(ch: ChannelId): LayoutState {
  let l = layouts.value.get(ch);
  if (!l) {
    const c = channelById.value.get(ch);
    l = reconcile((c && parseLayout(c.layout)) || emptyLayout([]), liveIds(ch));
    const m = new Map(layouts.value);
    m.set(ch, l);
    layouts.value = m;
    if (c?.layout) lastSaved.set(ch, JSON.stringify(c.layout));
  }
  return l;
}

export function updateLayout(ch: ChannelId, fn: (l: LayoutState) => LayoutState) {
  const cur = layoutOf(ch);
  const next = fn(cur);
  if (next === cur) return;
  const m = new Map(layouts.value);
  m.set(ch, next);
  layouts.value = m;
  scheduleSave(ch);
}

function reconcileChannel(ch: ChannelId) {
  if (!layouts.value.has(ch)) return; // lazily created on first view
  updateLayout(ch, (l) => {
    const r = reconcile(l, liveIds(ch));
    return JSON.stringify(r) === JSON.stringify(l) ? l : r;
  });
}

// ---------------- server event handling ----------------

function upsertBy<T extends { id: number }>(arr: T[], item: T): T[] {
  const i = arr.findIndex((x) => x.id === item.id);
  if (i < 0) return [...arr, item];
  const out = [...arr];
  out[i] = item;
  return out;
}

export const removedSessionListeners: ((id: SessionId) => void)[] = [];
/** Called when the window this client holds disappeared (it must claim again). */
export const windowLostListeners: (() => void)[] = [];

export function applyHello(h: HelloResult) {
  batch(() => {
    meta.value = { version: h.version, home: h.home, default_shell: h.default_shell };
    workspaces.value = [...h.workspaces].sort((a, b) => a.created_at - b.created_at || a.id - b.id);
    channels.value = h.channels.filter((c) => !c.archived);
    windowsSupported.value = Array.isArray(h.windows);
    windows.value = [...(h.windows ?? [])].sort((a, b) => a.id - b.id);
    const prev = sessions.value;
    sessions.value = new Map(h.sessions.map((s) => [s.id, s]));
    for (const s of h.sessions) {
      if (!prev.has(s.id) && spawnHolds.has(s.channel_id)) held.add(s.id);
    }
    for (const id of prev.keys()) {
      if (!sessions.value.has(id)) { for (const f of removedSessionListeners) f(id); }
    }
    // Drop layouts for channels that are gone; reconcile the rest.
    const m = new Map<ChannelId, LayoutState>();
    for (const [ch, l] of layouts.value) {
      if (channelById.value.has(ch)) m.set(ch, reconcile(l, liveIds(ch)));
    }
    layouts.value = m;
    helloDone.value = true;
  });
  // Seed unread state with a little history per channel.
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
      // Keep our local layout; the daemon doesn't echo set_layout and we own it.
      channels.value = upsertBy(channels.value, ev.data);
      break;
    }
    case "channel_removed":
      removeChannel(ev.data.id);
      break;
    case "window_upserted":
      windows.value = upsertWindow(windows.value, ev.data);
      break;
    case "window_removed":
      windows.value = windows.value.filter((w) => w.id !== ev.data.id);
      if (ev.data.id === windowId.value) { for (const f of windowLostListeners) f(); }
      break;
    case "session_upserted": {
      const prev = sessions.value.get(ev.data.id);
      const m = new Map(sessions.value);
      m.set(ev.data.id, ev.data);
      sessions.value = m;
      if (!prev && spawnHolds.has(ev.data.channel_id)) held.add(ev.data.id);
      if (!prev) reconcileChannel(ev.data.channel_id);
      const cc = confirmClose.value;
      if (cc?.kind === "session" && cc.id === ev.data.id && !BUSY_STATES.has(ev.data.state)) {
        confirmClose.value = null;
      }
      if (cc?.kind === "channel" && cc.id === ev.data.channel_id && !channelBusy(cc.id)) {
        confirmClose.value = null;
      }
      break;
    }
    case "session_removed": {
      const prev = sessions.value.get(ev.data.id);
      if (!prev) break;
      const m = new Map(sessions.value);
      m.delete(ev.data.id);
      sessions.value = m;
      held.delete(ev.data.id);
      for (const f of removedSessionListeners) f(ev.data.id);
      reconcileChannel(prev.channel_id);
      const cc = confirmClose.value;
      if (cc?.kind === "session" && cc.id === ev.data.id) confirmClose.value = null;
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
  }
}

function removeChannel(id: ChannelId) {
  batch(() => {
    channels.value = channels.value.filter((c) => c.id !== id);
    const m = new Map(layouts.value);
    m.delete(id);
    layouts.value = m;
    const cc = confirmClose.value;
    if (cc?.kind === "channel" && cc.id === id) confirmClose.value = null;
  });
}

// ---------------- read tracking ----------------

// One pending mark per channel: switching tabs quickly must not drop the first one. Each
// mark while one is pending moves its `seenAt` forward (what was on screen at the latest look).
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
  if (ch === activeChannel.value?.id && document.visibilityState === "visible" && document.hasFocus()) {
    markReadSoon(ch);
  }
}

effect(() => {
  const c = activeChannel.value;
  if (c) markRead(c.id);
});

addEventListener("focus", () => {
  const c = activeChannel.value;
  if (c) markRead(c.id);
});

effect(() => {
  document.title = documentTitle(windowsSupported.value ? myWindow.value : null, needsYouHere.value);
});
