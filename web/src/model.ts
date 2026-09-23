// Pure logic for windows, tabs, naming, Jump ranking and "needs you" cycling. No signals, no
// DOM: everything here is unit tested (model_test.ts).

import type { AgentState, Channel, ChannelId, SessionInfo, Win, WindowId, Workspace } from "./types.ts";
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

/** Move `id` to insertion slot `index` of `ids` (a slot counts gaps before removal, as a drop
 * marker does). Unknown ids leave the list as is. */
export function moveTo(ids: number[], id: number, index: number): number[] {
  const from = ids.indexOf(id);
  if (from < 0) return ids;
  const out = ids.filter((x) => x !== id);
  const at = index > from ? index - 1 : index;
  out.splice(Math.max(0, Math.min(at, out.length)), 0, id);
  return out;
}

/** Move `id` one step left (-1) or right (+1); stops at the ends. */
export function moveBy(ids: number[], id: number, delta: number): number[] {
  const from = ids.indexOf(id);
  if (from < 0) return ids;
  const to = Math.max(0, Math.min(ids.length - 1, from + delta));
  if (to === from) return ids;
  const out = [...ids];
  out.splice(from, 1);
  out.splice(to, 0, id);
  return out;
}

/** Whether a drop at insertion slot `index` would leave `id` where it is. */
export function isNoopSlot(ids: number[], id: number, index: number): boolean {
  const from = ids.indexOf(id);
  return from < 0 || index === from || index === from + 1;
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

// ---------------- names ----------------

export function basename(path: string): string {
  const p = path.replace(/\/+$/, "");
  return p.slice(p.lastIndexOf("/") + 1) || p || "/";
}

/**
 * What a tab shows. A human-named tab shows its name. An auto-named tab shows its derived
 * name; until something was derived, the daemon's placeholder (the folder name) is shown
 * muted.
 */
export function tabLabel(
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
  /** Stable key, also used for recency ("c:<id>", "s:<id>", "f:<path>", "cmd:<id>"). */
  key: string;
  /** Text matched against the query; the label is its prefix (for highlighting). */
  hay: string;
  label: string;
  /** A session that needs the human (or a tab holding one). */
  needs: boolean;
  /** Urgency among `needs` items (lower first) and how long they've waited. */
  pri?: number;
  since?: number;
  /** Lives in this window. */
  here?: boolean;
  /** Small bias for the kind of item (tabs over sessions over folders). */
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

/** Push `key` to the front of a most-recent-first list (deduplicated, capped). */
export function pushFront<T>(list: T[], key: T, max = 20): T[] {
  return [key, ...list.filter((x) => x !== key)].slice(0, max);
}
