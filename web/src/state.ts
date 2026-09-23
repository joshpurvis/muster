// App state as signals. Server state is replaced/merged from hello + ServerEvents;
// UI prefs persist to localStorage.

import { batch, computed, effect, type Signal, signal } from "@preact/signals";
import type {
  AgentState,
  Channel,
  ChannelId,
  HelloResult,
  ServerEvent,
  SessionId,
  SessionInfo,
  TimelineEvent,
  Workspace,
} from "./types.ts";
import { needsHuman, STATE_PRIORITY } from "./types.ts";
import { rpc } from "./rpc.ts";
import { debounce, load, save } from "./util.ts";
import { emptyLayout, type LayoutState, parseLayout, reconcile } from "./layout/tree.ts";

// ---------------- server state ----------------

export const helloDone = signal(false);
export const meta = signal({ version: "", home: "", default_shell: "" });
export const workspaces = signal<Workspace[]>([]);
export const channels = signal<Channel[]>([]);
export const sessions = signal<Map<SessionId, SessionInfo>>(new Map());

export const channelById = computed(() => new Map(channels.value.map((c) => [c.id, c])));
export const workspaceById = computed(() => new Map(workspaces.value.map((w) => [w.id, w])));

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

/** Channels in sidebar order, grouped by workspace. */
export const channelsByWorkspace = computed(() => {
  const m = new Map<number, Channel[]>();
  for (const c of channels.value) {
    if (c.archived) continue;
    let arr = m.get(c.workspace_id);
    if (!arr) m.set(c.workspace_id, arr = []);
    arr.push(c);
  }
  for (const arr of m.values()) arr.sort((a, b) => a.sort_order - b.sort_order || a.id - b.id);
  return m;
});

/** Flat sidebar order (for Alt+1..9 and Ctrl+Alt+Up/Down). Skips collapsed workspaces? No: all. */
export const channelOrder = computed(() => {
  const out: Channel[] = [];
  for (const w of workspaces.value) out.push(...(channelsByWorkspace.value.get(w.id) ?? []));
  return out;
});

export function aggregateState(list: SessionInfo[] | undefined): AgentState | null {
  if (!list || !list.length) return null;
  let best: AgentState = list[0].state;
  for (const s of list) {
    if (STATE_PRIORITY.indexOf(s.state) < STATE_PRIORITY.indexOf(best)) best = s.state;
  }
  return best;
}

export const inboxItems = computed(() => {
  const items = [...sessions.value.values()].filter((s) =>
    needsHuman(s.state) && channelById.value.get(s.channel_id) &&
    !channelById.value.get(s.channel_id)!.archived
  );
  items.sort((a, b) =>
    STATE_PRIORITY.indexOf(a.state) - STATE_PRIORITY.indexOf(b.state) ||
    a.state_since - b.state_since
  );
  return items;
});

// ---------------- timeline + unread ----------------

export interface TimelineState {
  events: TimelineEvent[];
  loaded: boolean;
  loading: boolean;
  hasMore: boolean;
}

const timelines = new Map<ChannelId, Signal<TimelineState>>();

export function timelineOf(ch: ChannelId): Signal<TimelineState> {
  let t = timelines.get(ch);
  if (!t) {
    t = signal({ events: [], loaded: false, loading: false, hasMore: true });
    timelines.set(ch, t);
  }
  return t;
}

export async function loadTimeline(ch: ChannelId, older = false) {
  const t = timelineOf(ch);
  if (t.value.loading) return;
  if (older && !t.value.hasMore) return;
  const lastBefore = t.value.events[t.value.events.length - 1]?.id;
  t.value = { ...t.value, loading: true };
  try {
    const before = older && t.value.events.length ? t.value.events[0].id : undefined;
    const limit = 150;
    const evs = await rpc.call<TimelineEvent[]>("channel.timeline", { id: ch, before, limit });
    const cur = t.value;
    const full = evs.length >= limit;
    // A refresh after a long disconnect can return a full page that doesn't reach back to what
    // we already have. Merging would hide the hole, so start over from the fresh page instead;
    // scrolling up loads the rest in order.
    // Compare with what we had before the call: live events appended meanwhile would hide it.
    const gap = !older && full && lastBefore !== undefined && evs.length > 0 && evs[0].id > lastBefore;
    const merged = older
      ? [...evs, ...cur.events]
      : gap
      ? mergeEvents(evs, cur.events.filter((e) => e.id > evs[evs.length - 1].id))
      : mergeEvents(cur.events, evs);
    t.value = {
      events: merged,
      loaded: true,
      loading: false,
      hasMore: older || gap || !cur.loaded ? full : cur.hasMore,
    };
  } catch {
    t.value = { ...t.value, loading: false };
  }
}

function mergeEvents(a: TimelineEvent[], b: TimelineEvent[]): TimelineEvent[] {
  const m = new Map<number, TimelineEvent>();
  for (const e of a) m.set(e.id, e);
  for (const e of b) m.set(e.id, e);
  return [...m.values()].sort((x, y) => x.id - y.id);
}

/** Latest "notable" event per channel (what makes a channel bold/unread). */
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

export type View = "channel" | "inbox";
export const view = signal<View>("channel");
export const activeChannelId = signal<ChannelId | null>(load("activeChannel", null));
effect(() => save("activeChannel", activeChannelId.value));

export const activeChannel = computed(() =>
  activeChannelId.value !== null ? channelById.value.get(activeChannelId.value) ?? null : null
);

export interface Prefs {
  sidebarWidth: number;
  sidebarCollapsed: boolean;
  rightWidth: number;
  rightOpen: boolean;
  rightTab: "timeline" | "diff";
  fontSize: number;
  theme: "system" | "dark" | "light";
  lastAgent: "claude" | "codex" | "shell" | "none";
  collapsedWorkspaces: number[];
  recent: string[];
  /** Pass Alt shortcuts (except Alt+Shift+…) through to a focused terminal. */
  altToTerminal: boolean;
}

const defaultPrefs: Prefs = {
  sidebarWidth: 248,
  sidebarCollapsed: false,
  rightWidth: 340,
  rightOpen: false,
  rightTab: "timeline",
  fontSize: 13,
  theme: "system",
  lastAgent: "claude",
  collapsedWorkspaces: [],
  recent: [],
  altToTerminal: false,
};

export const prefs = signal<Prefs>({ ...defaultPrefs, ...load<Partial<Prefs>>("prefs", {}) });
const savePrefs = debounce((p: Prefs) => save("prefs", p), 200);
effect(() => savePrefs(prefs.value));

export function setPref<K extends keyof Prefs>(k: K, v: Prefs[K]) {
  prefs.value = { ...prefs.value, [k]: v };
}

export function pushRecent(key: string) {
  const r = [key, ...prefs.value.recent.filter((x) => x !== key)].slice(0, 20);
  setPref("recent", r);
}

// Overlays (only one modal at a time).
export type Overlay =
  | { kind: "switcher"; initial: string }
  | { kind: "new_channel"; workspaceId?: number }
  | { kind: "add_workspace" }
  | { kind: "shortcuts" }
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

/** Session whose tab close is awaiting inline confirmation. */
export const confirmClose = signal<SessionId | null>(null);
// A pending "Kill?" never survives a channel switch.
effect(() => {
  void activeChannelId.value;
  void view.value;
  confirmClose.value = null;
});

/** States in which closing a tab asks first. */
export const BUSY_STATES = new Set<AgentState>(["working", "starting", "needs_approval", "asking_question"]);

/** The right panel was closed by a narrow window (not by the user; prefs keep their wish). */
export const rightSuppressed = signal(false);
export const rightVisible = computed(() => prefs.value.rightOpen && !rightSuppressed.value);

/** Alt has been held on its own for a moment (sidebar shows Alt+1..9 numbers). */
export const altHeld = signal(false);

/** Session ids being renamed inline (tab or sidebar). */
export const renaming = signal<{ kind: "channel" | "session"; id: number; where: string } | null>(
  null,
);

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

export type SessionListener = (s: SessionInfo, prev: SessionInfo | undefined) => void;
const sessionListeners: SessionListener[] = [];
export function onSessionChange(f: SessionListener) {
  sessionListeners.push(f);
}
export const removedSessionListeners: ((id: SessionId) => void)[] = [];

export function applyHello(h: HelloResult) {
  batch(() => {
    meta.value = { version: h.version, home: h.home, default_shell: h.default_shell };
    workspaces.value = [...h.workspaces].sort((a, b) => a.created_at - b.created_at || a.id - b.id);
    channels.value = h.channels.filter((c) => !c.archived);
    const prev = sessions.value;
    sessions.value = new Map(h.sessions.map((s) => [s.id, s]));
    for (const s of h.sessions) {
      if (!prev.has(s.id) && spawnHolds.has(s.channel_id)) held.add(s.id);
    }
    for (const s of h.sessions) for (const f of sessionListeners) f(s, prev.get(s.id));
    for (const id of prev.keys()) {
      if (!sessions.value.has(id)) { for (const f of removedSessionListeners) f(id); }
    }
    // Drop layouts for channels that are gone; reconcile the rest.
    const m = new Map<ChannelId, LayoutState>();
    for (const [ch, l] of layouts.value) {
      if (channelById.value.has(ch)) m.set(ch, reconcile(l, liveIds(ch)));
    }
    layouts.value = m;
    if (activeChannelId.value !== null && !channelById.value.has(activeChannelId.value)) {
      activeChannelId.value = channelOrder.value[0]?.id ?? null;
    }
    if (activeChannelId.value === null) activeChannelId.value = channelOrder.value[0]?.id ?? null;
    helloDone.value = true;
  });
  // Seed unread state with a little history per channel.
  for (const c of h.channels) {
    rpc.call<TimelineEvent[]>("channel.timeline", { id: c.id, limit: 30 }).then((evs) => {
      for (const e of evs) noteNotable(e);
    }).catch(() => {});
  }
  // Timelines that were open need refreshing (we may have missed events).
  for (const [ch, t] of timelines) if (t.value.loaded) loadTimeline(ch);
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
    case "session_upserted": {
      const prev = sessions.value.get(ev.data.id);
      const m = new Map(sessions.value);
      m.set(ev.data.id, ev.data);
      sessions.value = m;
      for (const f of sessionListeners) f(ev.data, prev);
      if (!prev && spawnHolds.has(ev.data.channel_id)) held.add(ev.data.id);
      if (!prev) reconcileChannel(ev.data.channel_id);
      if (confirmClose.value === ev.data.id && !BUSY_STATES.has(ev.data.state)) confirmClose.value = null;
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
      if (confirmClose.value === ev.data.id) confirmClose.value = null;
      break;
    }
    case "event": {
      const e = ev.data;
      noteNotable(e);
      if (e.channel_id !== null) {
        const t = timelines.get(e.channel_id);
        // Buffer even before the first load finishes; loadTimeline merges by id.
        if (t && !t.value.events.some((x) => x.id === e.id)) {
          t.value = { ...t.value, events: [...t.value.events, e] };
        }
        maybeMarkRead(e.channel_id);
      }
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
    timelines.delete(id);
    if (activeChannelId.value === id) activeChannelId.value = channelOrder.value[0]?.id ?? null;
  });
}

// ---------------- read tracking ----------------

// One pending mark per channel: switching channels quickly must not drop the first one.
const readTimers = new Map<ChannelId, number>();
function markReadSoon(ch: ChannelId) {
  if (readTimers.has(ch)) return;
  // What was on screen when we looked counts as read; events after that (e.g. after leaving
  // the channel within the delay) stay unread.
  const seenAt = Date.now();
  readTimers.set(
    ch,
    setTimeout(() => {
      readTimers.delete(ch);
      const c = channelById.value.get(ch);
      if (!c || c.last_read_at >= seenAt) return;
      channels.value = upsertBy(channels.value, { ...c, last_read_at: seenAt });
      rpc.call("channel.mark_read", { id: ch }).catch(() => {});
    }, 600),
  );
}

export function markRead(ch: ChannelId) {
  if (unread.value.has(ch)) markReadSoon(ch);
}

function maybeMarkRead(ch: ChannelId) {
  if (
    ch === activeChannelId.value && view.value === "channel" && document.visibilityState === "visible" &&
    document.hasFocus()
  ) {
    markReadSoon(ch);
  }
}

effect(() => {
  const ch = activeChannelId.value;
  if (ch !== null && view.value === "channel") markRead(ch);
});

addEventListener("focus", () => {
  if (activeChannelId.value !== null && view.value === "channel") markRead(activeChannelId.value);
});

effect(() => {
  const n = inboxItems.value.length;
  document.title = n ? `(${n}) Muster` : "Muster";
});
