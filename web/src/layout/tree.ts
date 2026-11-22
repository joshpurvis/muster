// A window's split tree of groups, and pure reducers. No DOM, no signals: everything here is unit
// tested. A leaf ("pane" in the JSON) is a group: its tabs are session ids, one of them active.
//
// Persisted form (opaque to the daemon, stored per window via window.set_layout):
//   { v: 1, root: Layout, focus: paneId, zoom: paneId | null }
// A bare Layout is also accepted on load.

export type Dir = "row" | "col";

// Session ids are positive u32s. Panels use -(id + 1); -1 stays the internal empty-pane
// sentinel. Notes use -(2^40 + id), far below any panel's. The window's one Questions tab is
// -2^52, below every note's. Keeping numeric tabs means existing saved layouts load without
// migration.
const NOTE_BASE = 2 ** 40;
/** The Questions tab (docs/UI.md "Questions"): one per window, in whichever channel is shown. */
export const QUESTIONS_TAB = -(2 ** 52);
/** Stable internal numeric id; saved pinned metadata uses `addon:<slug>`. */
export function addonTab(id: string): number {
  let hash = 5381;
  for (const c of new TextEncoder().encode(id)) hash = (Math.imul(hash, 33) ^ c) >>> 0;
  return QUESTIONS_TAB - 1 - hash;
}
/** The PRs to merge tab (docs/UI.md "PRs to merge"): one per window, like Questions; below every
 * add-on tab's id (those are QUESTIONS_TAB - 1 - a 32-bit hash). */
export const PRS_TAB = QUESTIONS_TAB - 2 ** 33;
export const isPinnedTab = (tab: number): boolean => tab <= QUESTIONS_TAB;
/** An add-on's tab (not Questions, not PRs to merge). */
export const isAddonTab = (tab: number): boolean => tab < QUESTIONS_TAB && tab > PRS_TAB;
export const panelTab = (id: number): number => -id - 1;
export const panelId = (tab: number): number | null =>
  Number.isSafeInteger(tab) && tab <= -2 && tab > -NOTE_BASE ? -tab - 1 : null;
export const noteTab = (id: number): number => -NOTE_BASE - id;
export const noteId = (tab: number): number | null =>
  Number.isSafeInteger(tab) && tab < -NOTE_BASE && tab > QUESTIONS_TAB ? -tab - NOTE_BASE : null;
/** A tab that is a terminal (a session), not a panel or a note. */
export const isSessionTab = (tab: number): boolean => tab > 0;

export interface PaneNode {
  type: "pane";
  id: string;
  tabs: number[];
  active: number | null;
}

export interface SplitNode {
  type: "split";
  dir: Dir;
  sizes: number[];
  children: Layout[];
}

export type Layout = PaneNode | SplitNode;

export interface LayoutState {
  v: 1;
  root: Layout;
  focus: string;
  zoom: string | null;
  /** Legacy layouts only: the window showed the old agents panel (docs/UI.md "Agents in the sidebar"). Missing: closed. */
  agents?: boolean;
}

export type Side = "left" | "right" | "top" | "bottom";
export type DropZone = Side | "center";
export type Direction = "left" | "right" | "up" | "down";

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

let paneSeq = 0;
export function newPaneId(): string {
  paneSeq++;
  return "p" + Date.now().toString(36) + paneSeq.toString(36) +
    Math.floor(Math.random() * 1e4).toString(36);
}

export function pane(tabs: number[] = [], id = newPaneId()): PaneNode {
  return { type: "pane", id, tabs: [...tabs], active: tabs.length ? tabs[0] : null };
}

export function emptyLayout(tabs: number[] = []): LayoutState {
  const p = pane(tabs);
  return { v: 1, root: p, focus: p.id, zoom: null };
}

// ---------- queries ----------

export function allPanes(root: Layout): PaneNode[] {
  const out: PaneNode[] = [];
  const walk = (n: Layout) => {
    if (n.type === "pane") out.push(n);
    else n.children.forEach(walk);
  };
  walk(root);
  return out;
}

export function findPane(root: Layout, id: string): PaneNode | null {
  return allPanes(root).find((p) => p.id === id) ?? null;
}

export function paneOfSession(root: Layout, sid: number): PaneNode | null {
  return allPanes(root).find((p) => p.tabs.includes(sid)) ?? null;
}

export function allSessions(root: Layout): number[] {
  return allPanes(root).flatMap((p) => p.tabs);
}

/** Path of child indices from root to the pane. */
export function pathOfPane(root: Layout, id: string): number[] | null {
  if (root.type === "pane") return root.id === id ? [] : null;
  for (let i = 0; i < root.children.length; i++) {
    const p = pathOfPane(root.children[i], id);
    if (p) return [i, ...p];
  }
  return null;
}

export function nodeAt(root: Layout, path: number[]): Layout | null {
  let n: Layout = root;
  for (const i of path) {
    if (n.type !== "split" || !n.children[i]) return null;
    n = n.children[i];
  }
  return n;
}

/** Rects of every pane in the unit square. */
export function paneRects(root: Layout): Map<string, Rect> {
  const out = new Map<string, Rect>();
  const walk = (n: Layout, r: Rect) => {
    if (n.type === "pane") {
      out.set(n.id, r);
      return;
    }
    let off = 0;
    n.children.forEach((c, i) => {
      const s = n.sizes[i];
      const cr = n.dir === "row"
        ? { x: r.x + off * r.w, y: r.y, w: s * r.w, h: r.h }
        : { x: r.x, y: r.y + off * r.h, w: r.w, h: s * r.h };
      off += s;
      walk(c, cr);
    });
  };
  walk(root, { x: 0, y: 0, w: 1, h: 1 });
  return out;
}

// ---------- structural helpers (immutable) ----------

function mapPanes(root: Layout, f: (p: PaneNode) => Layout): Layout {
  if (root.type === "pane") return f(root);
  return { ...root, children: root.children.map((c) => mapPanes(c, f)) };
}

function evenSizes(n: number): number[] {
  return Array.from({ length: n }, () => 1 / n);
}

function normSizes(sizes: number[]): number[] {
  const clean = sizes.map((s) => (Number.isFinite(s) && s > 0 ? s : 0));
  const sum = clean.reduce((a, b) => a + b, 0);
  if (sum <= 0) return evenSizes(sizes.length);
  return clean.map((s) => s / sum);
}

/**
 * Collapse single-child splits, flatten nested same-direction splits, drop empty panes
 * (keeping at least one pane overall), renormalize sizes.
 */
export function normalize(root: Layout): Layout {
  const rec = (n: Layout): Layout | null => {
    if (n.type === "pane") {
      const tabs = [...new Set(n.tabs)];
      const active = n.active !== null && tabs.includes(n.active) ? n.active : (tabs[0] ?? null);
      return { ...n, tabs, active };
    }
    const kids: Layout[] = [];
    const sizes: number[] = [];
    const base = n.sizes.length === n.children.length ? n.sizes : evenSizes(n.children.length);
    n.children.forEach((c, i) => {
      const r = rec(c);
      if (!r) return;
      if (r.type === "pane" && r.tabs.length === 0) {
        // Empty panes are dropped from splits; the caller keeps one if the tree empties.
        return;
      }
      if (r.type === "split" && r.dir === n.dir) {
        r.children.forEach((gc, j) => {
          kids.push(gc);
          sizes.push(base[i] * r.sizes[j]);
        });
      } else {
        kids.push(r);
        sizes.push(base[i]);
      }
    });
    if (kids.length === 0) return null;
    if (kids.length === 1) return kids[0];
    return { type: "split", dir: n.dir, children: kids, sizes: normSizes(sizes) };
  };
  if (root.type === "split") {
    // Keep a lone empty pane only if everything else is gone.
    const r = rec(root);
    if (r) return r;
    const firstPane = allPanes(root)[0];
    return firstPane ? { ...firstPane, tabs: [], active: null } : pane();
  }
  return rec(root) ?? pane();
}

/** Like normalize, but an explicitly kept empty pane (e.g. freshly split) survives. */
function normalizeKeeping(root: Layout, keep: string | null): Layout {
  if (!keep) return normalize(root);
  // Temporarily give the kept pane a sentinel tab so normalize doesn't drop it.
  const SENT = -1;
  const withSent = mapPanes(
    root,
    (p) => (p.id === keep && p.tabs.length === 0 ? { ...p, tabs: [SENT], active: null } : p),
  );
  const n = normalize(withSent);
  return mapPanes(
    n,
    (p) => (p.tabs.includes(SENT) ? { ...p, tabs: p.tabs.filter((t) => t !== SENT), active: null } : p),
  );
}

function fixFocus(s: LayoutState): LayoutState {
  const panes = allPanes(s.root);
  const focus = panes.some((p) => p.id === s.focus) ? s.focus : panes[0].id;
  const zoom = s.zoom && panes.some((p) => p.id === s.zoom) && panes.length > 1 ? s.zoom : null;
  return { ...s, focus, zoom };
}

function finish(s: LayoutState, keepEmpty: string | null = null): LayoutState {
  return fixFocus({ ...s, root: normalizeKeeping(s.root, keepEmpty) });
}

// ---------- load / reconcile ----------

function validNode(n: unknown): n is Layout {
  if (!n || typeof n !== "object") return false;
  const o = n as Record<string, unknown>;
  if (o.type === "pane") {
    return typeof o.id === "string" && Array.isArray(o.tabs) &&
      (o.tabs as unknown[]).every((t) => typeof t === "number");
  }
  if (o.type === "split") {
    return (o.dir === "row" || o.dir === "col") && Array.isArray(o.children) &&
      Array.isArray(o.sizes) && (o.children as unknown[]).every(validNode);
  }
  return false;
}

export function parseLayout(raw: unknown): LayoutState | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  if (o.v === 1 && validNode(o.root)) {
    return fixFocus({
      v: 1,
      root: normalizeKeeping(o.root, null),
      focus: typeof o.focus === "string" ? o.focus : "",
      zoom: typeof o.zoom === "string" ? o.zoom : null,
      ...(o.agents === true ? { agents: true } : {}),
    });
  }
  if (validNode(raw)) {
    return fixFocus({ v: 1, root: normalize(raw), focus: "", zoom: null });
  }
  return null;
}

/**
 * Drop unknown session ids (a focused group that empties hands focus to its nearest neighbour),
 * append sessions not in any group to the focused group (in the given order), collapse empties.
 */
export function reconcile(s: LayoutState, live: number[]): LayoutState {
  const liveSet = new Set(live);
  let st = fixFocus({ ...s, root: normalize(s.root) });
  for (const id of allSessions(st.root)) if (!liveSet.has(id)) st = removeSession(st, id);
  const present = new Set(allSessions(st.root));
  const missing = live.filter((id) => !present.has(id));
  if (missing.length) {
    st = {
      ...st,
      root: mapPanes(
        st.root,
        (p) =>
          p.id === st.focus ? { ...p, tabs: [...p.tabs, ...missing], active: p.active ?? missing[0] } : p,
      ),
    };
  }
  return finish(st);
}

/**
 * Where a tab sits in its channel's groups (the Questions tab, docs/UI.md "Questions"). `group`,
 * `index`, `alone` and `size` are in terms any channel's groups can follow; a group of its own
 * along a whole window edge is remembered as that edge and its share of the window. `pane`, or
 * `near`, `side` and `share`, say exactly where it was in that same channel, by the ids of the
 * groups around it, so that channel can put it back there.
 */
export interface TabPlace {
  /** Its group in reading order (top-left first); -1 is the last of two or more. */
  group: number;
  /** Its place in that group's strip; -1 is the end. */
  index: number;
  /** It had a group of its own along this edge of the window... */
  alone?: Side;
  /** ...taking this share of the window's width (left, right) or height (top, bottom). */
  size?: number;
  /** Same channel: the id of the group it shared, at `index`... */
  pane?: string;
  /** ...where it was the open tab. */
  open?: true;
  /** Same channel: it had a group of its own on `side` of the groups with these ids (a whole
   * branch of the split tree)... */
  near?: string[];
  side?: Side;
  /** ...taking this share of the split they were in together. */
  share?: number;
}

/** The first tab of the top-left group: the Questions tab's place before it could move. */
export const FIRST_PLACE: TabPlace = { group: 0, index: 0 };

const SIDES: readonly Side[] = ["left", "right", "top", "bottom"];
const MAX_GROUPS = 64;

/** A saved `TabPlace`, or null when it isn't one. */
export function parseTabPlace(raw: unknown): TabPlace | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  const int = (v: unknown) => typeof v === "number" && Number.isSafeInteger(v) && v >= -1 && v <= MAX_GROUPS * 64;
  if (!int(o.group) || !int(o.index)) return null;
  const place: TabPlace = { group: Math.min(o.group as number, MAX_GROUPS), index: o.index as number };
  const share = (v: unknown) => typeof v === "number" && v > 0 && v < 1;
  const id = (v: unknown) => typeof v === "string" && v.length > 0 && v.length <= 64;
  if (SIDES.includes(o.alone as Side)) {
    place.alone = o.alone as Side;
    if (share(o.size)) place.size = o.size as number;
  }
  if (id(o.pane)) {
    place.pane = o.pane as string;
    if (o.open === true) place.open = true;
  } else if (Array.isArray(o.near) && o.near.length > 0 && o.near.length <= MAX_GROUPS && o.near.every(id) &&
    SIDES.includes(o.side as Side) && share(o.share)) {
    place.near = [...o.near as string[]];
    place.side = o.side as Side;
    place.share = o.share as number;
  }
  return place;
}

/** `place` without the parts that only mean something in the channel it was taken in. */
export function generalPlace(place: TabPlace): TabPlace {
  // Not `open` either: in another channel it doesn't take over a group's view.
  const { group, index, alone, size } = place;
  return { group, index, ...(alone ? { alone } : {}), ...(alone && size !== undefined ? { size } : {}) };
}

/** Where tab `tab` sits now (null: not in this layout). */
export function placeOf(s: LayoutState, tab: number): TabPlace | null {
  const panes = allPanes(s.root);
  const i = panes.findIndex((p) => p.tabs.includes(tab));
  if (i < 0) return null;
  const p = panes[i];
  if (p.tabs.length === 1 && panes.length > 1) {
    // A group of its own: the groups beside it in its split, and its share there.
    const path = pathOfPane(s.root, p.id)!;
    const parent = nodeAt(s.root, path.slice(0, -1)) as SplitNode;
    const k = path[path.length - 1];
    const after = k > 0;
    const exact = {
      near: allPanes(parent.children[after ? k - 1 : k + 1]).map((q) => q.id),
      side: (parent.dir === "row" ? (after ? "right" : "left") : (after ? "bottom" : "top")) as Side,
      share: parent.sizes[k],
    };
    const r = paneRects(s.root).get(p.id)!;
    const e = 1e-6;
    const tall = r.y <= e && r.y + r.h >= 1 - e;
    const wide = r.x <= e && r.x + r.w >= 1 - e;
    if (tall && r.x + r.w >= 1 - e) return { group: -1, index: 0, alone: "right", size: r.w, ...exact };
    if (tall && r.x <= e) return { group: 0, index: 0, alone: "left", size: r.w, ...exact };
    if (wide && r.y + r.h >= 1 - e) return { group: -1, index: 0, alone: "bottom", size: r.h, ...exact };
    if (wide && r.y <= e) return { group: 0, index: 0, alone: "top", size: r.h, ...exact };
    return { group: i === panes.length - 1 ? -1 : i, index: 0, ...exact };
  }
  const at = p.tabs.indexOf(tab);
  return {
    group: panes.length > 1 && i === panes.length - 1 ? -1 : i,
    index: at > 0 && at === p.tabs.length - 1 ? -1 : at,
    pane: p.id,
    ...(p.active === tab ? { open: true as const } : {}),
  };
}

const sameIds = (a: string[], b: string[]) => a.length === b.length && a.every((id) => b.includes(id));
const closeTo = (a: number | undefined, b: number | undefined) => Math.abs((a ?? 0.5) - (b ?? 0.5)) < 1e-6;

/** Tab already at `b`, judged by what `b` says (the exact parts when it has them). */
function samePlace(a: TabPlace | null, b: TabPlace): boolean {
  if (!a) return false;
  if (b.near) return !!a.near && a.side === b.side && sameIds(a.near, b.near) && closeTo(a.share, b.share);
  if (b.pane) return a.pane === b.pane && a.index === b.index && (!b.open || a.open === true);
  return a.alone === b.alone && (b.alone ? closeTo(a.size, b.size) : a.group === b.group && a.index === b.index);
}

/** `a` is `b` in the words any channel follows (group and strip, or edge and share). */
export function sameGeneralPlace(a: TabPlace, b: TabPlace): boolean {
  return samePlace(generalPlace(a), generalPlace(b));
}

/**
 * New group `np` on `side` of the branch it sat beside, taking `share` of the split they then
 * share (the inverse of closing it there). That branch is the smallest one (a node, or a run of a
 * split's children) holding every group of `ids` still here: groups added inside it meanwhile (a
 * panel split beside its opener) or closed there don't lose the place. A branch whose split was
 * folded into its parent's (closing a group can do that) is a run of that parent's children,
 * gathered back into its own split. Null when none of those groups is left.
 */
function insertNear(root: Layout, ids: string[], np: PaneNode, side: Side, share: number): Layout | null {
  const here = allPanes(root).map((p) => p.id);
  const want = ids.filter((id) => here.includes(id));
  if (!want.length) return null;
  const dir = sideDir(side);
  const before = side === "left" || side === "top";
  const s = share > 0 && share < 1 ? share : 0.5;
  const held = (n: Layout) => allPanes(n).filter((p) => want.includes(p.id)).length;
  const wrap = (n: Layout): Layout =>
    ({ type: "split", dir, children: before ? [np, n] : [n, np], sizes: before ? [s, 1 - s] : [1 - s, s] });
  // The smallest run holding them all (a whole split is its parent's one-child run, or the root).
  let best = { path: [] as number[], i: -1, j: -1, groups: here.length };
  const walk = (n: Layout, path: number[]) => {
    if (n.type === "pane") return;
    for (let i = 0; i < n.children.length; i++) {
      if (!held(n.children[i])) continue;
      let got = 0, groups = 0;
      for (let j = i; j < n.children.length; j++) {
        got += held(n.children[j]);
        groups += allPanes(n.children[j]).length;
        if (got < want.length) continue;
        const whole = i === 0 && j === n.children.length - 1;
        if (!whole && groups < best.groups) best = { path, i, j, groups };
        break;
      }
    }
    n.children.forEach((c, k) => walk(c, [...path, k]));
  };
  walk(root, []);
  if (best.i < 0) return wrap(root);
  const { i, j } = best;
  const rebuild = (n: Layout, depth: number): Layout => {
    if (n.type !== "split") return n;
    if (depth < best.path.length) {
      const k = best.path[depth];
      return { ...n, children: n.children.map((c, x) => (x === k ? rebuild(c, depth + 1) : c)) };
    }
    if (n.dir === dir) {
      // Beside them in the same split: its share back, the others' scaled down to make room.
      const at = before ? i : j + 1;
      const children = [...n.children];
      const sizes = n.sizes.map((x) => x * (1 - s));
      children.splice(at, 0, np);
      sizes.splice(at, 0, s);
      return { ...n, children, sizes };
    }
    const kids = n.children.slice(i, j + 1);
    const total = n.sizes.slice(i, j + 1).reduce((a, b) => a + b, 0);
    const branch: Layout = kids.length === 1
      ? kids[0]
      : { type: "split", dir: n.dir, children: kids, sizes: normSizes(n.sizes.slice(i, j + 1)) };
    return {
      ...n,
      children: [...n.children.slice(0, i), wrap(branch), ...n.children.slice(j + 1)],
      sizes: [...n.sizes.slice(0, i), total, ...n.sizes.slice(j + 1)],
    };
  };
  return rebuild(root, 0);
}

/**
 * Move tab `tab` (already in the layout) to `place`. Exactly where it was when the place names
 * groups that are still here: into that group's strip, or into a new group beside the same groups
 * at the same share. Otherwise into the group at `group`, clamped to the groups and tabs there
 * are, or, for a place along an edge, into a new group along that whole edge. Focus and every
 * group's selection stay (the tab stays selected if it was).
 */
export function putAt(s: LayoutState, tab: number, place: TabPlace): LayoutState {
  const from = paneOfSession(s.root, tab);
  if (!from || samePlace(placeOf(s, tab), place)) return s;
  const selected = from.active === tab;
  const focused = s.focus === from.id;
  if (place.near && place.side) {
    const base = removeSession(s, tab);
    const np = pane([tab]);
    const root = insertNear(base.root, place.near, np, place.side, place.share ?? 0.5);
    const next = root && finish({ ...base, root, focus: selected && focused ? np.id : base.focus });
    // Along a whole window edge, it stays along that edge (groups beside it may have closed).
    if (next && (!place.alone || placeOf(next, tab)?.alone === place.alone)) return next;
  }
  if (place.alone) {
    const base = removeSession(s, tab);
    const np = pane([tab]);
    const dir = sideDir(place.alone);
    const before = place.alone === "left" || place.alone === "top";
    const size = Math.max(0.1, Math.min(0.9, place.size ?? 0.5));
    const r = base.root;
    const root: Layout = r.type === "split" && r.dir === dir
      ? {
        ...r,
        children: before ? [np, ...r.children] : [...r.children, np],
        sizes: before ? [size, ...r.sizes.map((x) => x * (1 - size))] : [...r.sizes.map((x) => x * (1 - size)), size],
      }
      : { type: "split", dir, children: before ? [np, r] : [r, np], sizes: before ? [size, 1 - size] : [1 - size, size] };
    return finish({ ...base, root, focus: selected && focused ? np.id : base.focus });
  }
  const panes = allPanes(s.root);
  const own = panes.find((p) => p.id === place.pane);
  const target = own ?? panes[place.group < 0 ? panes.length - 1 : Math.min(place.group, panes.length - 1)];
  const rest = target.tabs.filter((t) => t !== tab);
  const at = place.index < 0 ? rest.length : Math.min(place.index, rest.length);
  // Back in its own group, it is the open tab there again if it was.
  const open = selected || (!!own && place.open === true);
  if (target.id === from.id && from.tabs.indexOf(tab) === at && (!open || from.active === tab)) return s;
  const root = mapPanes(s.root, (p) => {
    if (p.id === target.id) {
      const tabs = [...rest];
      tabs.splice(at, 0, tab);
      return { ...p, tabs, active: open ? tab : p.active ?? tab };
    }
    if (p.id !== from.id) return p;
    const left = p.tabs.filter((t) => t !== tab);
    return { ...p, tabs: left, active: p.active === tab ? left[0] ?? null : p.active };
  });
  return finish({ ...s, root, focus: selected && focused ? target.id : s.focus });
}

/** First in the first group (the top-left one), keeping focus and every group's selection. */
export function pinFirst(s: LayoutState, tab: number): LayoutState {
  return putAt(s, tab, FIRST_PLACE);
}

/** A new panel joins the group immediately to its opener's right, or splits there. */
export function placePanel(s: LayoutState, id: number, opener: number): LayoutState {
  return placeBeside(s, panelTab(id), opener);
}

/** Tab `tab` (a panel's or a note's) joins the group to the right of session `opener`'s, or
 * splits there; without the opener in the layout, beside the focused group. */
export function placeBeside(s: LayoutState, tab: number, opener: number): LayoutState {
  if (paneOfSession(s.root, tab)) return s;
  const origin = paneOfSession(s.root, opener)?.id ?? s.focus;
  const right = neighbour(s.root, origin, "right");
  return right ? addTab(s, tab, right) : splitPane(s, "right", tab, origin);
}

// ---------- mutations ----------

export function focusPane(s: LayoutState, id: string): LayoutState {
  if (!findPane(s.root, id)) return s;
  return { ...s, focus: id };
}

export function activateTab(s: LayoutState, sid: number): LayoutState {
  const p = paneOfSession(s.root, sid);
  if (!p) return s;
  return {
    ...s,
    focus: p.id,
    zoom: s.zoom && s.zoom !== p.id ? null : s.zoom,
    root: mapPanes(s.root, (q) => (q.id === p.id ? { ...q, active: sid } : q)),
  };
}

/** Add a session as a tab of a pane (default: focused), at index (default: after active). */
export function addTab(s: LayoutState, sid: number, paneId?: string, index?: number): LayoutState {
  const base = removeFromTabs(s, sid);
  const target = paneId && findPane(base.root, paneId) ? paneId : base.focus;
  const root = mapPanes(base.root, (p) => {
    if (p.id !== target) return p;
    const tabs = [...p.tabs];
    const at = index ?? (p.active !== null ? tabs.indexOf(p.active) + 1 : tabs.length);
    tabs.splice(Math.max(0, Math.min(at, tabs.length)), 0, sid);
    return { ...p, tabs, active: sid };
  });
  return finish({ ...base, root, focus: target });
}

function removeFromTabs(s: LayoutState, sid: number): LayoutState {
  return {
    ...s,
    root: mapPanes(s.root, (p) => {
      if (!p.tabs.includes(sid)) return p;
      const idx = p.tabs.indexOf(sid);
      const tabs = p.tabs.filter((t) => t !== sid);
      const active = p.active === sid ? (tabs[Math.min(idx, tabs.length - 1)] ?? null) : p.active;
      return { ...p, tabs, active };
    }),
  };
}

/** Remove a session everywhere; empty panes collapse (except the last one). */
export function removeSession(s: LayoutState, sid: number): LayoutState {
  const p = paneOfSession(s.root, sid);
  const st = removeFromTabs(s, sid);
  // If the focused pane collapses, move focus to a neighbour instead of the first pane.
  if (p && p.tabs.length === 1 && s.focus === p.id) {
    const rects = paneRects(s.root);
    const r = rects.get(p.id)!;
    let best: string | null = null;
    let bestD = Infinity;
    for (const [id, q] of rects) {
      if (id === p.id) continue;
      const d = Math.hypot(q.x + q.w / 2 - (r.x + r.w / 2), q.y + q.h / 2 - (r.y + r.h / 2));
      if (d < bestD) {
        bestD = d;
        best = id;
      }
    }
    if (best) return finish({ ...st, focus: best });
  }
  return finish(st);
}

function sideDir(side: Side): Dir {
  return side === "left" || side === "right" ? "row" : "col";
}

/** Replace the pane `targetId` with a split containing it and `newNode` on `side`. */
function insertBeside(root: Layout, targetId: string, newNode: Layout, side: Side): Layout {
  const dir = sideDir(side);
  const before = side === "left" || side === "top";
  const rec = (n: Layout): Layout => {
    if (n.type === "pane") {
      if (n.id !== targetId) return n;
      return {
        type: "split",
        dir,
        sizes: [0.5, 0.5],
        children: before ? [newNode, n] : [n, newNode],
      };
    }
    // If the target is a direct child of a same-direction split, insert as a sibling
    // (splitting only the target's share) instead of nesting.
    const idx = n.children.findIndex((c) => c.type === "pane" && c.id === targetId);
    if (idx >= 0 && n.dir === dir) {
      const children = [...n.children];
      const sizes = [...n.sizes];
      const half = sizes[idx] / 2;
      sizes[idx] = half;
      const at = before ? idx : idx + 1;
      children.splice(at, 0, newNode);
      sizes.splice(at, 0, half);
      return { ...n, children, sizes };
    }
    return { ...n, children: n.children.map(rec) };
  };
  return rec(root);
}

/**
 * Split pane `paneId` (default focused) on `side`. The new pane gets `sid` as its only tab
 * (or is empty). Focus moves to the new pane.
 */
export function splitPane(
  s: LayoutState,
  side: Side,
  sid: number | null = null,
  paneId?: string,
): LayoutState {
  const target = paneId && findPane(s.root, paneId) ? paneId : s.focus;
  const base = sid !== null ? removeFromTabs(s, sid) : s;
  const np = pane(sid !== null ? [sid] : []);
  // If removing the tab emptied the target pane itself (splitting a pane with its own only
  // tab), there's nothing to split: just keep it.
  const tgt = findPane(base.root, target);
  if (!tgt) return s;
  if (tgt.tabs.length === 0 && sid !== null) {
    return finish({
      ...base,
      root: mapPanes(base.root, (p) => (p.id === target ? { ...p, tabs: [sid], active: sid } : p)),
      focus: target,
      zoom: null,
    });
  }
  const root = insertBeside(base.root, target, np, side);
  return finish({ ...base, root, focus: np.id, zoom: null }, sid === null ? np.id : null);
}

/** Drag & drop: move a tab to a pane, either into its tabs (center) or as a new split. */
export function moveTab(
  s: LayoutState,
  sid: number,
  targetPaneId: string,
  zone: DropZone,
  index?: number,
): LayoutState {
  const from = paneOfSession(s.root, sid);
  if (!from || !findPane(s.root, targetPaneId)) return s;
  if (zone === "center") {
    if (from.id === targetPaneId && index === undefined) return activateTab(s, sid);
    if (from.id === targetPaneId && index !== undefined) {
      const tabs = from.tabs.filter((t) => t !== sid);
      const oldIdx = from.tabs.indexOf(sid);
      const at = index > oldIdx ? index - 1 : index;
      tabs.splice(Math.max(0, Math.min(at, tabs.length)), 0, sid);
      return {
        ...s,
        focus: from.id,
        root: mapPanes(s.root, (p) => (p.id === from.id ? { ...p, tabs, active: sid } : p)),
      };
    }
    return addTab(s, sid, targetPaneId, index ?? Number.MAX_SAFE_INTEGER);
  }
  // Split. Dropping a pane's only tab on its own edge is a no-op.
  if (from.id === targetPaneId && from.tabs.length === 1) return s;
  const base = removeFromTabs(s, sid);
  const np = pane([sid]);
  const root = insertBeside(base.root, targetPaneId, np, zone);
  return finish({ ...base, root, focus: np.id, zoom: null });
}

export function reorderTab(s: LayoutState, sid: number, index: number): LayoutState {
  const p = paneOfSession(s.root, sid);
  if (!p) return s;
  return moveTab(s, sid, p.id, "center", index);
}

/** Move a tab one place left (-1) or right (+1) within its group; stops at the ends. */
export function shiftTab(s: LayoutState, sid: number, delta: number): LayoutState {
  const p = paneOfSession(s.root, sid);
  if (!p) return s;
  const from = p.tabs.indexOf(sid);
  const to = Math.max(0, Math.min(p.tabs.length - 1, from + delta));
  if (to === from) return s;
  return moveTab(s, sid, p.id, "center", to > from ? to + 1 : to);
}

const DIR_SIDE: Record<Direction, Side> = { left: "left", right: "right", up: "top", down: "bottom" };

export function sideOf(dir: Direction): Side {
  return DIR_SIDE[dir];
}

/**
 * Ctrl+Alt+arrow: move a tab to the end of the neighbouring group in that direction. At the
 * window's edge (no neighbour) its group splits on that side with the tab in the new group, unless
 * it is the group's only tab. Unzooms.
 */
export function moveTabToNeighbour(s: LayoutState, sid: number, dir: Direction): LayoutState {
  const from = paneOfSession(s.root, sid);
  if (!from) return s;
  const n = neighbour(s.root, from.id, dir);
  if (n) return moveTab({ ...s, zoom: null }, sid, n, "center");
  if (from.tabs.length < 2) return s;
  return moveTab(s, sid, from.id, DIR_SIDE[dir]);
}

/** The group at the window's top-right corner (it carries the window controls). */
export function topRightPane(root: Layout): string {
  let best = "";
  let bx = -Infinity;
  let by = Infinity;
  for (const [id, r] of paneRects(root)) {
    const right = r.x + r.w;
    if (right > bx + 1e-6 || (Math.abs(right - bx) <= 1e-6 && r.y < by)) {
      bx = right;
      by = r.y;
      best = id;
    }
  }
  return best;
}

export function setSizes(s: LayoutState, path: number[], sizes: number[]): LayoutState {
  const rec = (n: Layout, depth: number): Layout => {
    if (n.type !== "split") return n;
    if (depth === path.length) return { ...n, sizes: normSizes(sizes) };
    const i = path[depth];
    return { ...n, children: n.children.map((c, j) => (j === i ? rec(c, depth + 1) : c)) };
  };
  const target = nodeAt(s.root, path);
  if (!target || target.type !== "split" || target.children.length !== sizes.length) return s;
  return { ...s, root: rec(s.root, 0) };
}

export function equalize(s: LayoutState, path: number[]): LayoutState {
  const n = nodeAt(s.root, path);
  if (!n || n.type !== "split") return s;
  return setSizes(s, path, evenSizes(n.children.length));
}

/**
 * Move the boundary between two adjacent children of the split at `path` by `delta`
 * (fraction of the split), clamping each side to `minFrac`.
 */
export function dragGutter(
  sizes: number[],
  index: number,
  delta: number,
  minFrac: number,
): number[] {
  const out = [...sizes];
  const a = out[index];
  const b = out[index + 1];
  const total = a + b;
  const min = Math.min(minFrac, total / 2);
  const na = Math.max(min, Math.min(total - min, a + delta));
  out[index] = na;
  out[index + 1] = total - na;
  return out;
}

/**
 * Keyboard resize: move the focused pane's divider in the arrow direction by `amount`
 * (fraction of the parent split). Prefers the divider on the arrow side (grows the pane);
 * at the edge, moves the opposite divider (shrinks it), like tmux resize-pane.
 */
export function resizeFocused(s: LayoutState, dir: Direction, amount = 0.05): LayoutState {
  const path = pathOfPane(s.root, s.focus);
  if (!path) return s;
  const axis: Dir = dir === "left" || dir === "right" ? "row" : "col";
  const forward = dir === "right" || dir === "down";
  // Walk up from the pane to find a split on this axis.
  for (let depth = path.length - 1; depth >= 0; depth--) {
    const sp = path.slice(0, depth);
    const node = nodeAt(s.root, sp);
    if (!node || node.type !== "split" || node.dir !== axis) continue;
    const i = path[depth];
    const n = node.children.length;
    if (forward && i < n - 1) return setSizes(s, sp, dragGutter(node.sizes, i, amount, 0.08));
    if (!forward && i > 0) return setSizes(s, sp, dragGutter(node.sizes, i - 1, -amount, 0.08));
    // At the edge: move the divider on the other side in the arrow's direction (tmux-style).
    if (forward && i > 0) return setSizes(s, sp, dragGutter(node.sizes, i - 1, amount, 0.08));
    if (!forward && i < n - 1) return setSizes(s, sp, dragGutter(node.sizes, i, -amount, 0.08));
  }
  return s;
}

/** Geometric neighbour of the focused pane in a direction (overlap on the other axis wins). */
export function neighbour(root: Layout, fromId: string, dir: Direction): string | null {
  const rects = paneRects(root);
  const r = rects.get(fromId);
  if (!r) return null;
  const eps = 1e-6;
  let best: string | null = null;
  let bestScore = Infinity;
  for (const [id, q] of rects) {
    if (id === fromId) continue;
    let gap: number;
    let overlap: number;
    if (dir === "left" || dir === "right") {
      gap = dir === "left" ? r.x - (q.x + q.w) : q.x - (r.x + r.w);
      overlap = Math.min(r.y + r.h, q.y + q.h) - Math.max(r.y, q.y);
    } else {
      gap = dir === "up" ? r.y - (q.y + q.h) : q.y - (r.y + r.h);
      overlap = Math.min(r.x + r.w, q.x + q.w) - Math.max(r.x, q.x);
    }
    if (gap < -eps || overlap <= eps) continue;
    // Prefer closest, then most overlap, then the one aligned with our top/left.
    const align = dir === "left" || dir === "right" ? Math.abs(q.y - r.y) : Math.abs(q.x - r.x);
    const score = gap * 1000 - overlap * 10 + align;
    if (score < bestScore) {
      bestScore = score;
      best = id;
    }
  }
  return best;
}

export function focusDirection(s: LayoutState, dir: Direction): LayoutState {
  if (s.zoom) return s;
  const n = neighbour(s.root, s.focus, dir);
  return n ? { ...s, focus: n } : s;
}

export function cycleTab(s: LayoutState, delta: number): LayoutState {
  const p = findPane(s.root, s.focus);
  if (!p || p.tabs.length < 2 || p.active === null) return s;
  const i = p.tabs.indexOf(p.active);
  const next = p.tabs[(i + delta + p.tabs.length) % p.tabs.length];
  return activateTab(s, next);
}

export function toggleZoom(s: LayoutState, paneId?: string): LayoutState {
  const id = paneId ?? s.focus;
  if (s.zoom === id) return { ...s, zoom: null };
  if (allPanes(s.root).length < 2) return { ...s, zoom: null };
  return { ...s, zoom: id, focus: id };
}

export interface Size {
  w: number;
  h: number;
}

/** Smallest pane a split may create (px, tab strip included). */
export const MIN_SPLIT: Size = { w: 240, h: 120 };

/** Pixel size of pane `id` when the whole tree is laid out in `box` (gutters ignored). */
export function panePx(root: Layout, id: string, box: Size): Size | null {
  const r = paneRects(root).get(id);
  return r ? { w: r.w * box.w, h: r.h * box.h } : null;
}

/** Whether splitting pane `id` on `side` leaves both halves at least `min`. */
export function splitFits(root: Layout, id: string, side: Side, box: Size, min: Size = MIN_SPLIT): boolean {
  const r = panePx(root, id, box);
  if (!r) return false;
  const across = side === "left" || side === "right";
  const w = across ? r.w / 2 : r.w;
  const h = across ? r.h : r.h / 2;
  return w >= min.w && h >= min.h;
}

/**
 * Whether dropping tab `sid` on pane `paneId` / `zone` (at tab `index` for strips) would
 * leave the layout as it is: a pane's only tab onto its own edge, its own body, or the slot
 * it already occupies in its own strip.
 */
export function isNoopMove(
  s: LayoutState,
  sid: number,
  paneId: string,
  zone: DropZone,
  index?: number,
): boolean {
  const from = paneOfSession(s.root, sid);
  if (!from) return true;
  if (from.id !== paneId) return false;
  if (zone !== "center") return from.tabs.length === 1;
  if (index === undefined) return true;
  const old = from.tabs.indexOf(sid);
  return index === old || index === old + 1;
}

/** Compact structural signature (for tests / debugging). */
export function sig(n: Layout): string {
  if (n.type === "pane") return `[${n.tabs.join(",")}]`;
  return `${n.dir}(${n.children.map(sig).join(" ")})`;
}
