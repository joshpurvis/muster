// Pure logic for windows, tabs, naming, Jump ranking and "needs you" cycling. No signals, no
// DOM: everything here is unit tested (model_test.ts).

import type {
  AgentState,
  Channel,
  ChannelId,
  SessionId,
  SessionInfo,
  Win,
  WindowId,
  Workspace,
} from "./types.ts";
import { needsHuman, STATE_PRIORITY } from "./types.ts";
import { fuzzy } from "./util.ts";

// ---------------- windows and tabs ----------------

/** The window that shows channel `c` as a tab, if any. `window_id` wins; daemons that don't
 * send it are matched by the windows' tab lists. */
export function windowIdOf(c: Channel, windows: Win[]): WindowId | null {
  if (c.window_id !== undefined) return c.window_id;
  return windows.find((w) => w.channel_ids.includes(c.id))?.id ?? null;
}

/**
 * Tabs of window `win`, in its order. Channels that name this window but aren't in its
 * `channel_ids` yet (the window event lags the channel event) go last, oldest first.
 * `win === null` (a daemon without windows): every live channel, in `sort_order`.
 */
export function tabsOf(win: Win | null, channels: Channel[]): Channel[] {
  const live = channels.filter((c) => !c.archived);
  if (!win) return live.sort((a, b) => a.sort_order - b.sort_order || a.id - b.id);
  const mine = live.filter((c) =>
    c.window_id === win.id || (c.window_id === undefined && win.channel_ids.includes(c.id))
  );
  const pos = new Map(win.channel_ids.map((id, i) => [id, i]));
  const at = (c: Channel) => pos.get(c.id) ?? Number.MAX_SAFE_INTEGER;
  return mine.sort((a, b) => at(a) - at(b) || a.created_at - b.created_at || a.id - b.id);
}

/** Whether task `c` names window `win` as its window (`window_id`, else the window's list). */
function taskInWindow(c: Channel, win: Win): boolean {
  return c.window_id === win.id || (c.window_id === undefined && win.channel_ids.includes(c.id));
}

/**
 * The window showing session `s` as a tab. `window_id` (v2) wins; daemons without it show a
 * session in its task's window.
 */
export function sessionWindowId(s: SessionInfo, task: Channel | undefined, windows: Win[]): WindowId | null {
  if (s.window_id !== undefined) return s.window_id;
  return task ? windowIdOf(task, windows) : null;
}

/**
 * Sessions shown as tabs in window `win`: its own (`window_id`), or, from daemons without
 * per-session windows, those of its tasks. Only sessions of live tasks. Ordered by their task's
 * place in the window's task list (other tasks after, oldest first), then age: the order new
 * tabs join a group in. `win === null` (no windows): every session of a live task. A hidden
 * agent tab (`hidden`) is in no window.
 */
export function sessionsOfWindow(
  win: Win | null,
  sessions: Iterable<SessionInfo>,
  channels: Channel[],
): SessionInfo[] {
  const live = new Map(channels.filter((c) => !c.archived).map((c) => [c.id, c]));
  const pos = new Map((win?.channel_ids ?? []).map((id, i) => [id, i]));
  const taskKey = (c: Channel) => pos.get(c.id) ?? Number.MAX_SAFE_INTEGER;
  const out = [...sessions].filter((s) => {
    const c = live.get(s.channel_id);
    if (!c || s.hidden) return false;
    if (!win) return true;
    return s.window_id !== undefined ? s.window_id === win.id : taskInWindow(c, win);
  });
  return out.sort((a, b) => {
    const ca = live.get(a.channel_id)!;
    const cb = live.get(b.channel_id)!;
    return taskKey(ca) - taskKey(cb) || (win ? 0 : ca.sort_order - cb.sort_order) ||
      ca.created_at - cb.created_at || ca.id - cb.id || a.created_at - b.created_at || a.id - b.id;
  });
}

export function upsertWindow(list: Win[], w: Win): Win[] {
  const i = list.findIndex((x) => x.id === w.id);
  const out = i < 0 ? [...list, w] : list.map((x) => (x.id === w.id ? w : x));
  return out.sort((a, b) => a.id - b.id);
}

/** Record locally that channel `id` now lives in window `to` at `index` (default end), until
 * the daemon's window events confirm it. */
export function moveChannelLocally(list: Win[], id: ChannelId, to: WindowId, index?: number): Win[] {
  return list.map((w) => {
    const rest = w.channel_ids.filter((x) => x !== id);
    if (w.id !== to) return rest.length === w.channel_ids.length ? w : { ...w, channel_ids: rest };
    const at = index === undefined ? rest.length : Math.max(0, Math.min(index, rest.length));
    rest.splice(at, 0, id);
    return { ...w, channel_ids: rest };
  });
}

/** "Window 2" for default (numeric) names, else the name itself. */
export function windowLabel(w: Pick<Win, "id" | "name">): string {
  const n = (w.name ?? "").trim() || String(w.id);
  return /^\d+$/.test(n) ? `Window ${n}` : n;
}

/** `window=<id>` from a location hash, if it is a positive integer. */
export function windowParam(hash: string): number | null {
  const v = new URLSearchParams(hash.replace(/^#/, "")).get("window");
  if (!v || !/^\d+$/.test(v)) return null;
  const n = Number(v);
  return n > 0 ? n : null;
}

/** document.title: `muster · <window name>`, with `(N) ` when N sessions here need you. */
export function documentTitle(win: Pick<Win, "id" | "name"> | null, needsYou: number): string {
  const base = win ? `muster · ${(win.name ?? "").trim() || win.id}` : "muster";
  return needsYou > 0 ? `(${needsYou}) ${base}` : base;
}

// ---------------- shared window prefs ----------------

/** Take `next` (a `prefs.set` reply or a `prefs_changed` event) over `cur` unless it is older:
 * a reply can arrive after the event of a later change by another window. */
export function newerPrefs<T extends { revision?: number }>(cur: T | null, next: T): boolean {
  return !cur || (next.revision ?? 0) >= (cur.revision ?? 0);
}

// ---------------- names ----------------

export function basename(path: string): string {
  const p = path.replace(/\/+$/, "");
  return p.slice(p.lastIndexOf("/") + 1) || p || "/";
}

/**
 * A task's name. A human-named task shows its name. An auto-named task shows its
 * derived name; until something was derived, the daemon's placeholder (the folder name) is shown
 * muted.
 */
export function taskLabel(
  c: Pick<Channel, "name" | "cwd" | "auto_named">,
  ws?: Pick<Workspace, "name" | "path"> | null,
): { text: string; muted: boolean } {
  const name = (c.name ?? "").trim();
  const folder = basename(ws?.path ?? c.cwd);
  if (!name) return { text: folder, muted: true };
  if (c.auto_named) {
    const placeholders = [folder, basename(c.cwd), ws?.name ?? ""].filter(Boolean);
    if (placeholders.includes(name)) return { text: name, muted: true };
  }
  return { text: name, muted: false };
}

export interface TabLabel {
  /** The session's own name; its channel is already named in the sidebar. */
  text: string;
}

/** Tab labels stay the same when other sessions join or leave the channel. */
export function tabLabel(s: Pick<SessionInfo, "name">): TabLabel {
  return { text: s.name };
}

/** One line for menus, tooltips and accessible labels. */
export function tabText(l: TabLabel): string {
  return l.text;
}

/** A tab rename edits only its session; channels are renamed in the sidebar. */
export interface RenameFields {
  session: string;
}

export function renameFields(s: Pick<SessionInfo, "name">): RenameFields {
  return { session: s.name };
}

/** Commit only a changed, nonblank session name. */
export function renameChanges(
  f: RenameFields,
  v: { session?: string },
): { session?: string } {
  const n = v.session?.trim();
  return n && n !== f.session ? { session: n } : {};
}

/** A rename (of session `sid`, task `channel`) is over once its tab isn't among `shown` (this
 * window's tabs) or its session or task is gone. */
export function renameOutlived(
  r: { sid: number; channel: number | null },
  shown: readonly number[],
  sessions: { has(id: number): boolean },
  tasks: { has(id: number): boolean },
): boolean {
  return !shown.includes(r.sid) || !sessions.has(r.sid) || (r.channel !== null && !tasks.has(r.channel));
}

// ---------------- state ----------------

export function aggregateState(list: SessionInfo[] | undefined): AgentState | null {
  if (!list || !list.length) return null;
  let best: AgentState = list[0].state;
  for (const s of list) {
    if (STATE_PRIORITY.indexOf(s.state) < STATE_PRIORITY.indexOf(best)) best = s.state;
  }
  return best;
}

/** Sessions that need the human, in a stable order: most urgent state first, then the one
 * waiting longest. Only sessions of live channels count. */
export function needsYouOrder(sessions: Iterable<SessionInfo>, live: Set<ChannelId>): SessionInfo[] {
  const out = [...sessions].filter((s) => needsHuman(s.state) && live.has(s.channel_id));
  return out.sort((a, b) =>
    STATE_PRIORITY.indexOf(a.state) - STATE_PRIORITY.indexOf(b.state) ||
    a.state_since - b.state_since || a.id - b.id
  );
}

/** Alt+J: the item after `current` in `list`, wrapping; the first when `current` isn't in it. */
export function nextInCycle<T extends { id: number }>(list: T[], current: number | null): T | null {
  if (!list.length) return null;
  const i = current === null ? -1 : list.findIndex((x) => x.id === current);
  return list[(i + 1) % list.length];
}

// ---------------- Jump ----------------

export interface JumpCandidate {
  /** Stable key, also used for recency ("s:<id>", "f:<path>", "cmd:<id>"). */
  key: string;
  /** Text matched against the query; the label is its prefix (for highlighting). */
  hay: string;
  label: string;
  /** A session that needs the human. */
  needs: boolean;
  /** Urgency among `needs` items (lower first) and how long they've waited. */
  pri?: number;
  since?: number;
  /** Lives in this window. */
  here?: boolean;
  /** Small bias for the kind of item (tabs over folders). */
  boost?: number;
}

export interface Ranked<T> {
  item: T;
  score: number;
  hits: number[];
}

/**
 * Jump ordering. Without a query: what needs you first (most urgent, then longest waiting),
 * then recently used (most recent first), then everything else in the given order. With a
 * query: fuzzy score plus boosts for needing you, recency and this window.
 */
export function rankJump<T extends JumpCandidate>(
  items: T[],
  query: string,
  recent: string[],
  limit = 60,
): Ranked<T>[] {
  const q = query.trim();
  const out: (Ranked<T> & { i: number })[] = [];
  items.forEach((item, i) => {
    const hits: number[] = [];
    const s = fuzzy(q, item.hay, hits);
    if (s === null) return;
    const ri = recent.indexOf(item.key);
    let score = s + (item.boost ?? 0);
    if (q) {
      if (item.needs) score += 8; // above the best recency boost
      if (ri >= 0) score += Math.max(0, 6 - ri * 0.3);
      if (item.here) score += 1;
    }
    out.push({ item, score, hits: hits.filter((h) => h < item.label.length), i });
  });
  if (q) {
    out.sort((a, b) => b.score - a.score || a.i - b.i);
  } else {
    const band = (r: Ranked<T>) => (r.item.needs ? 0 : recent.includes(r.item.key) ? 1 : 2);
    out.sort((a, b) => {
      const d = band(a) - band(b);
      if (d) return d;
      if (band(a) === 0) {
        return (a.item.pri ?? 0) - (b.item.pri ?? 0) || (a.item.since ?? 0) - (b.item.since ?? 0) ||
          a.i - b.i;
      }
      if (band(a) === 1) return recent.indexOf(a.item.key) - recent.indexOf(b.item.key);
      return a.i - b.i;
    });
  }
  return out.slice(0, limit).map(({ item, score, hits }) => ({ item, score, hits }));
}

/**
 * Jump's base order of sessions (before ranking): this window's tabs in `here` order (the layout's
 * groups and strips), then other windows' by window id (no window last), then age.
 */
export function jumpOrder<T extends Pick<SessionInfo, "id" | "created_at">>(
  all: T[],
  here: number[],
  windowOf: (s: T) => WindowId | null,
): T[] {
  const pos = new Map(here.map((id, i) => [id, i]));
  const win = (s: T) => windowOf(s) ?? Number.MAX_SAFE_INTEGER;
  return [...all].sort((a, b) => {
    const pa = pos.get(a.id);
    const pb = pos.get(b.id);
    if (pa !== undefined || pb !== undefined) {
      return (pa ?? Number.MAX_SAFE_INTEGER) - (pb ?? Number.MAX_SAFE_INTEGER);
    }
    return win(a) - win(b) || a.created_at - b.created_at || a.id - b.id;
  });
}

/** Push `key` to the front of a most-recent-first list (deduplicated, capped). */
export function pushFront<T>(list: T[], key: T, max = 20): T[] {
  return [key, ...list.filter((x) => x !== key)].slice(0, max);
}

// ---------------- session replies and the inline close confirmation ----------------

/**
 * Change counters for sessions, so a late RPC reply can't roll state back. Every session event
 * (`session_upserted`, `session_removed`) bumps its session; a hello bumps them all. A call
 * notes `mark()` before it starts; its reply about session `id` is applied only if `fresh(id,
 * mark)`: nothing about that session arrived since (else the event already has the newer truth,
 * or the session is gone and the reply would bring it back).
 */
export class SessionVersions {
  #clock = 0;
  #epoch = 0;
  #seen = new Map<SessionId, number>();

  /** An event about session `id` arrived. */
  bump(id: SessionId) {
    this.#seen.set(id, ++this.#clock);
  }

  /** A hello replaced every session. */
  bumpAll() {
    this.#epoch = ++this.#clock;
    this.#seen.clear();
  }

  mark(): number {
    return this.#clock;
  }

  fresh(id: SessionId, mark: number): boolean {
    return this.#epoch <= mark && (this.#seen.get(id) ?? 0) <= mark;
  }
}

/** The armed "Close?" after focus moved from `prev` to `next`: it never survives a focus change. */
export function confirmAfterFocus(
  pending: SessionId | null,
  prev: SessionId | null,
  next: SessionId | null,
): SessionId | null {
  return prev === next ? pending : null;
}

/** The armed "Close?" after session `s` changed: disarmed when that session stops being busy. */
export function confirmAfterUpsert(
  pending: SessionId | null,
  s: Pick<SessionInfo, "id" | "state">,
  busy: ReadonlySet<AgentState>,
): SessionId | null {
  return pending === s.id && !busy.has(s.state) ? null : pending;
}

/** An agent mid-turn (working, stuck, or waiting on an approval or answer): Restart and Interrupt
 * from a menu ask first, since its turn would be lost. Stuck is still a turn: a quiet one (e.g. a
 * long build) that src/detect.rs flags after 10 minutes without output. Shells and idle agents
 * never ask. */
export function midTurn(s: Pick<SessionInfo, "kind" | "state">): boolean {
  return s.kind !== "shell" && MID_TURN.has(s.state);
}
const MID_TURN = new Set<AgentState>(["working", "stuck", "needs_approval", "asking_question"]);
