// Split-tree model and pure reducers. No DOM, no signals: everything here is unit tested.
//
// Persisted form (opaque to the daemon, stored via channel.set_layout):
//   { v: 1, root: Layout, focus: paneId, zoom: paneId | null }
// A bare Layout (as in docs/UI.md) is also accepted on load.

export type Dir = "row" | "col";

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
    });
  }
  if (validNode(raw)) {
    return fixFocus({ v: 1, root: normalize(raw), focus: "", zoom: null });
  }
  return null;
}

/**
 * Drop unknown session ids, append sessions not in any pane to the focused pane (in the
 * given order), collapse empties.
 */
export function reconcile(s: LayoutState, live: number[]): LayoutState {
  const liveSet = new Set(live);
  let root = mapPanes(s.root, (p) => ({ ...p, tabs: p.tabs.filter((t) => liveSet.has(t)) }));
  root = normalize(root);
  let st = fixFocus({ ...s, root });
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
 * (or is empty, showing the launcher). Focus moves to the new pane.
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

// ---------- pane chrome ----------

/**
 * What a pane shows above its terminal: a pane-tab strip when it holds 2+ sessions, else a thin
 * header when the tab has 2+ panes, else nothing (a lone pane with one session).
 */
export type PaneChrome = "strip" | "head" | "none";

export function chromeOf(sessionsInPane: number, panesInTab: number): PaneChrome {
  if (sessionsInPane >= 2) return "strip";
  return panesInTab >= 2 ? "head" : "none";
}

/** Chrome of the pane a new session will land in: a new pane-tab of `paneId`, or a split. */
export function chromeForNew(s: LayoutState, type: "tab" | "split", paneId: string): PaneChrome {
  if (type === "split") return "head";
  const p = findPane(s.root, paneId);
  return chromeOf((p?.tabs.length ?? 0) + 1, allPanes(s.root).length);
}

/** Compact structural signature (for tests / debugging). */
export function sig(n: Layout): string {
  if (n.type === "pane") return `[${n.tabs.join(",")}]`;
  return `${n.dir}(${n.children.map(sig).join(" ")})`;
}
