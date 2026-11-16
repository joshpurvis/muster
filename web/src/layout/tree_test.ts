import {
  activateTab,
  addTab,
  allPanes,
  cycleTab,
  dragGutter,
  emptyLayout,
  equalize,
  focusDirection,
  isNoopMove,
  type LayoutState,
  MIN_SPLIT,
  moveTab,
  moveTabToNeighbour,
  neighbour,
  normalize,
  pane,
  paneOfSession,
  panePx,
  paneRects,
  panelTab,
  panelId,
  isSessionTab,
  noteId,
  noteTab,
  placeBeside,
  placePanel,
  parseLayout,
  reconcile,
  removeSession,
  reorderTab,
  resizeFocused,
  shiftTab,
  sig,
  splitFits,
  splitPane,
  toggleZoom,
  topRightPane,
} from "./tree.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}
function assert(cond: unknown, msg = "assert") {
  if (!cond) throw new Error(msg);
}
function near(a: number, b: number) {
  return Math.abs(a - b) < 1e-9;
}

function focusedTabs(s: LayoutState) {
  return allPanes(s.root).find((p) => p.id === s.focus)!.tabs;
}

Deno.test("panel tabs preserve old numeric layouts and avoid the empty sentinel", () => {
  const old = emptyLayout([1, 2]);
  const loaded = parseLayout(JSON.parse(JSON.stringify(old)))!;
  assertEquals(sig(loaded.root), "[1,2]");
  assertEquals(panelTab(1), -2);
  assertEquals(panelId(-2), 1);
  assertEquals(panelId(-1), null);
  assertEquals(panelId(1), null);
  const next = placePanel(loaded, 1, 1);
  assertEquals(sig(next.root), "row([1,2] [-2])");
  assertEquals(sig(parseLayout(JSON.parse(JSON.stringify(next)))!.root), sig(next.root));
});

Deno.test("note tabs have their own ids, apart from panels and sessions", () => {
  assertEquals(noteId(noteTab(1)), 1);
  assertEquals(noteId(noteTab(123456)), 123456);
  assertEquals(panelId(noteTab(1)), null);
  assertEquals(noteId(panelTab(1)), null);
  assertEquals(noteId(-1), null);
  assertEquals(noteId(5), null);
  assert(!isSessionTab(noteTab(1)) && !isSessionTab(panelTab(1)) && isSessionTab(3));
  let s = placeBeside(splitPane(emptyLayout([1]), "right", 2), noteTab(7), 1);
  assertEquals(sig(s.root), `row([1] [2,${noteTab(7)}])`);
  s = parseLayout(JSON.parse(JSON.stringify(s)))!;
  assertEquals(sig(s.root), `row([1] [2,${noteTab(7)}])`);
  s = reconcile(s, [1, 2]);
  assertEquals(sig(s.root), "row([1] [2])");
});

Deno.test("new panels join the opener's right group and move like terminal tabs", () => {
  let s = splitPane(emptyLayout([1]), "right", 2);
  s = placePanel(s, 10, 1);
  assertEquals(sig(s.root), "row([1] [2,-11])");
  const left = paneOfSession(s.root, 1)!;
  s = moveTab(s, -11, left.id, "center");
  assertEquals(sig(s.root), "row([1,-11] [2])");
  assertEquals(sig(placePanel(s, 10, 1).root), sig(s.root), "reload must not re-place an existing panel");
  s = reconcile(s, [1, 2]);
  assertEquals(sig(s.root), "row([1] [2])");
});

Deno.test("emptyLayout has one pane", () => {
  const s = emptyLayout([1, 2]);
  assertEquals(sig(s.root), "[1,2]");
  assertEquals(focusedTabs(s), [1, 2]);
});

Deno.test("split right puts new session in a new focused pane", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  assertEquals(sig(s.root), "row([1] [2])");
  assertEquals(focusedTabs(s), [2]);
  s = splitPane(s, "bottom", 3);
  assertEquals(sig(s.root), "row([1] col([2] [3]))");
  s = splitPane(s, "bottom", 4);
  // Same-direction split inserts as sibling rather than nesting.
  assertEquals(sig(s.root), "row([1] col([2] [3] [4]))");
  const sp = s.root.type === "split" ? s.root.children[1] : null;
  assert(sp && sp.type === "split");
  if (sp && sp.type === "split") {
    assert(near(sp.sizes[0], 0.5) && near(sp.sizes[1], 0.25) && near(sp.sizes[2], 0.25));
  }
});

Deno.test("split left/top insert before", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "left", 2);
  assertEquals(sig(s.root), "row([2] [1])");
  s = splitPane(s, "top", 3);
  assertEquals(sig(s.root), "row(col([3] [2]) [1])");
});

Deno.test("split with session that is already in focused pane moves it", () => {
  let s = emptyLayout([1, 2]);
  s = splitPane(s, "right", 2);
  assertEquals(sig(s.root), "row([1] [2])");
});

Deno.test("split with the pane's only tab keeps it in place", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 1);
  assertEquals(sig(s.root), "[1]");
});

Deno.test("split without a session keeps an empty launcher pane", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", null);
  assertEquals(sig(s.root), "row([1] [])");
  assertEquals(focusedTabs(s), []);
});

Deno.test("removeSession collapses empty panes and single-child splits", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  s = splitPane(s, "bottom", 3);
  s = removeSession(s, 3);
  assertEquals(sig(s.root), "row([1] [2])");
  // Focus moved to a neighbour, not a random pane
  assertEquals(focusedTabs(s), [2]);
  s = removeSession(s, 2);
  assertEquals(sig(s.root), "[1]");
  s = removeSession(s, 1);
  assertEquals(sig(s.root), "[]");
  assertEquals(allPanes(s.root).length, 1);
});

Deno.test("removeSession picks next tab as active", () => {
  let s = emptyLayout([1, 2, 3]);
  s = activateTab(s, 2);
  s = removeSession(s, 2);
  assertEquals(allPanes(s.root)[0].active, 3);
  s = removeSession(s, 3);
  assertEquals(allPanes(s.root)[0].active, 1);
});

Deno.test("normalize flattens nested same-direction splits with scaled sizes", () => {
  const n = normalize({
    type: "split",
    dir: "row",
    sizes: [0.5, 0.5],
    children: [
      pane([1], "a"),
      { type: "split", dir: "row", sizes: [0.5, 0.5], children: [pane([2], "b"), pane([3], "c")] },
    ],
  });
  assertEquals(sig(n), "row([1] [2] [3])");
  if (n.type === "split") assertEquals(n.sizes, [0.5, 0.25, 0.25]);
});

Deno.test("normalize drops empty panes and dedupes tabs", () => {
  const n = normalize({
    type: "split",
    dir: "col",
    sizes: [0.3, 0.7],
    children: [pane([], "a"), pane([1, 1, 2], "b")],
  });
  assertEquals(sig(n), "[1,2]");
});

Deno.test("reconcile drops unknown and appends missing to focused pane", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  s = reconcile(s, [2, 3, 4]);
  assertEquals(sig(s.root), "[2,3,4]");
  let t = emptyLayout([1]);
  t = splitPane(t, "right", 2);
  t = { ...t, focus: allPanes(t.root)[0].id };
  t = reconcile(t, [1, 2, 5]);
  assertEquals(sig(t.root), "row([1,5] [2])");
});

Deno.test("parseLayout accepts wrapped and bare layouts, rejects junk", () => {
  const a = parseLayout({ v: 1, root: pane([1], "x"), focus: "x", zoom: null });
  assert(a && a.focus === "x");
  const b = parseLayout({
    type: "split",
    dir: "row",
    sizes: [1, 1],
    children: [pane([1], "x"), pane([2], "y")],
  });
  assert(b && sig(b.root) === "row([1] [2])" && b.focus === "x");
  if (b && b.root.type === "split") assertEquals(b.root.sizes, [0.5, 0.5]);
  assertEquals(parseLayout(null), null);
  assertEquals(parseLayout({ type: "pane" }), null);
  assertEquals(parseLayout({ v: 1, root: { type: "split", dir: "x", children: [], sizes: [] } }), null);
});

Deno.test("moveTab center moves into target pane and focuses it", () => {
  let s = emptyLayout([1, 2]);
  s = splitPane(s, "right", 3);
  const left = paneOfSession(s.root, 1)!.id;
  s = moveTab(s, 3, left, "center");
  assertEquals(sig(s.root), "[1,2,3]");
  assertEquals(allPanes(s.root)[0].active, 3);
});

Deno.test("moveTab to edge creates split", () => {
  let s = emptyLayout([1, 2]);
  const p = allPanes(s.root)[0].id;
  s = moveTab(s, 2, p, "bottom");
  assertEquals(sig(s.root), "col([1] [2])");
  s = moveTab(s, 1, paneOfSession(s.root, 2)!.id, "left");
  // Pane with 1 emptied and collapsed; 1 is now left of 2.
  assertEquals(sig(s.root), "row([1] [2])");
});

Deno.test("moveTab lone tab onto own edge is a no-op", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  const before = sig(s.root);
  s = moveTab(s, 2, paneOfSession(s.root, 2)!.id, "left");
  assertEquals(sig(s.root), before);
});

Deno.test("reorderTab within pane", () => {
  let s = emptyLayout([1, 2, 3]);
  s = reorderTab(s, 1, 3);
  assertEquals(sig(s.root), "[2,3,1]");
  s = reorderTab(s, 1, 0);
  assertEquals(sig(s.root), "[1,2,3]");
  s = reorderTab(s, 3, 1);
  assertEquals(sig(s.root), "[1,3,2]");
});

Deno.test("addTab inserts after active", () => {
  let s = emptyLayout([1, 2, 3]);
  s = activateTab(s, 1);
  s = addTab(s, 9);
  assertEquals(sig(s.root), "[1,9,2,3]");
  assertEquals(allPanes(s.root)[0].active, 9);
});

Deno.test("dragGutter clamps to min", () => {
  assertEquals(dragGutter([0.5, 0.5], 0, 0.1, 0.1), [0.6, 0.4]);
  const r = dragGutter([0.5, 0.5], 0, 0.9, 0.1);
  assert(near(r[0], 0.9) && near(r[1], 0.1));
  const r2 = dragGutter([0.2, 0.3, 0.5], 1, -0.5, 0.05);
  assert(near(r2[1], 0.05) && near(r2[2], 0.75) && near(r2[0], 0.2));
});

Deno.test("equalize", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  s = splitPane(s, "right", 3);
  s = equalize(s, []);
  if (s.root.type === "split") assert(s.root.sizes.every((x) => near(x, 1 / 3)));
  else throw new Error("expected split");
});

Deno.test("paneRects and neighbour", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  s = splitPane(s, "bottom", 3);
  const r = paneRects(s.root);
  const p2 = paneOfSession(s.root, 2)!.id;
  const p3 = paneOfSession(s.root, 3)!.id;
  const p1 = paneOfSession(s.root, 1)!.id;
  assertEquals(r.get(p3), { x: 0.5, y: 0.5, w: 0.5, h: 0.5 });
  assertEquals(neighbour(s.root, p3, "up"), p2);
  assertEquals(neighbour(s.root, p3, "left"), p1);
  assertEquals(neighbour(s.root, p1, "right"), p2); // aligned with top wins
  assertEquals(neighbour(s.root, p1, "left"), null);
  s = focusDirection(s, "left");
  assertEquals(s.focus, p1);
});

Deno.test("resizeFocused grows toward direction", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2); // focus on right pane
  s = resizeFocused(s, "left", 0.1);
  if (s.root.type === "split") assert(near(s.root.sizes[0], 0.4));
  s = resizeFocused(s, "right", 0.1); // at right edge: pushes the other boundary back
  if (s.root.type === "split") assert(near(s.root.sizes[0], 0.5));
  // No vertical split: no-op
  const before = JSON.stringify(s);
  s = resizeFocused(s, "up", 0.1);
  assertEquals(JSON.stringify(s), before);
});

Deno.test("cycleTab wraps", () => {
  let s = emptyLayout([1, 2, 3]);
  s = cycleTab(s, -1);
  assertEquals(allPanes(s.root)[0].active, 3);
  s = cycleTab(s, 1);
  assertEquals(allPanes(s.root)[0].active, 1);
});

Deno.test("zoom toggles and clears on split", () => {
  let s = emptyLayout([1]);
  s = toggleZoom(s);
  assertEquals(s.zoom, null); // single pane: nothing to zoom
  s = splitPane(s, "right", 2);
  s = toggleZoom(s);
  assertEquals(s.zoom, s.focus);
  s = toggleZoom(s);
  assertEquals(s.zoom, null);
  s = toggleZoom(s);
  s = splitPane(s, "bottom", 3);
  assertEquals(s.zoom, null);
});

Deno.test("activateTab in another pane clears zoom", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  s = toggleZoom(s);
  s = activateTab(s, 1);
  assertEquals(s.zoom, null);
  assertEquals(s.focus, paneOfSession(s.root, 1)!.id);
});

Deno.test("panePx scales unit rects to the box", () => {
  let s = emptyLayout([1]);
  s = splitPane(s, "right", 2);
  const p2 = paneOfSession(s.root, 2)!.id;
  assertEquals(panePx(s.root, p2, { w: 1000, h: 600 }), { w: 500, h: 600 });
  assertEquals(panePx(s.root, "nope", { w: 1000, h: 600 }), null);
});

Deno.test("splitFits checks the halved size against the minimum", () => {
  let s = emptyLayout([1]);
  const box = { w: 1000, h: 600 };
  const p1 = s.focus;
  assert(splitFits(s.root, p1, "right", box), "1000px wide pane splits right");
  assert(splitFits(s.root, p1, "bottom", box), "600px tall pane splits down");
  assert(!splitFits(s.root, p1, "right", { w: 400, h: 600 }), "400px can't halve to 240");
  assert(!splitFits(s.root, p1, "bottom", { w: 1000, h: 200 }), "200px can't halve to 120");
  assert(splitFits(s.root, p1, "right", { w: 2 * MIN_SPLIT.w, h: MIN_SPLIT.h }), "exact minimum fits");
  // Repeated right splits of the focused pane stop once halves would be too narrow.
  let n = 1;
  while (splitFits(s.root, s.focus, "right", box)) s = splitPane(s, "right", ++n);
  assertEquals(allPanes(s.root).length, 3); // 1000 -> 500 -> 250; 250/2 < 240
});

Deno.test("isNoopMove recognises drops that change nothing", () => {
  let s = emptyLayout([1]);
  const p1 = s.focus;
  assert(isNoopMove(s, 1, p1, "right"), "only tab onto own edge");
  assert(isNoopMove(s, 1, p1, "center"), "own body");
  s = addTab(s, 2, p1);
  assert(!isNoopMove(s, 1, p1, "right"), "pane with two tabs can split off one");
  assert(isNoopMove(s, 1, p1, "center", 0), "same slot (before itself)");
  assert(isNoopMove(s, 1, p1, "center", 1), "same slot (after itself)");
  assert(!isNoopMove(s, 1, p1, "center", 2), "moving to the end is a change");
  s = splitPane(s, "right", 3);
  const p3 = paneOfSession(s.root, 3)!.id;
  assert(!isNoopMove(s, 1, p3, "center"), "other pane");
  assert(!isNoopMove(s, 3, p1, "left"), "other pane edge");
  assert(isNoopMove(s, 99, p1, "center"), "unknown session");
});

// ---------- groups (v2: the window's layout; a tab is one session) ----------

/** Row [1,2] | col([3] [4]): four groups' worth of tabs in three groups. */
function threeGroups(): LayoutState {
  let s = emptyLayout([1, 2]);
  s = splitPane(s, "right", 3);
  s = splitPane(s, "bottom", 4);
  return s;
}

Deno.test("groups reconcile with the window's sessions: new ones join the focused group", () => {
  let s = threeGroups();
  assertEquals(sig(s.root), "row([1,2] col([3] [4]))");
  s = { ...s, focus: paneOfSession(s.root, 1)!.id };
  // 5 arrived (spawned elsewhere, or moved here from another window), 3 left (closed or moved).
  s = reconcile(s, [1, 2, 4, 5]);
  assertEquals(sig(s.root), "row([1,2,5] [4])");
  assertEquals(findActive(s), 1, "the focused group keeps its active tab");
  // An empty window starts one group with its sessions in order.
  const e = reconcile(emptyLayout([]), [7, 8]);
  assertEquals(sig(e.root), "[7,8]");
  assertEquals(findActive(e), 7);
});

Deno.test("closing the last tab of a group removes the group; focus goes to a neighbour", () => {
  let s = threeGroups();
  const g4 = paneOfSession(s.root, 4)!.id;
  s = { ...s, focus: g4 };
  s = reconcile(s, [1, 2, 3]);
  assertEquals(sig(s.root), "row([1,2] [3])");
  assertEquals(findActive(s), 3, "the group above takes focus, not the first one");
  s = reconcile(s, [1, 2]);
  assertEquals(sig(s.root), "[1,2]");
  s = reconcile(s, []);
  assertEquals(sig(s.root), "[]", "an empty window keeps one empty group");
});

Deno.test("reconcile keeps a layout that already matches (no churn)", () => {
  const s = threeGroups();
  assertEquals(JSON.stringify(reconcile(s, [1, 2, 3, 4])), JSON.stringify(s));
});

Deno.test("dragging a tab between groups: into a strip at the marker, onto a body, onto an edge", () => {
  let s = threeGroups();
  const g1 = paneOfSession(s.root, 1)!.id;
  const g3 = paneOfSession(s.root, 3)!.id;
  // Into another group's strip, before its first tab.
  s = moveTab(s, 2, g3, "center", 0);
  assertEquals(sig(s.root), "row([1] col([2,3] [4]))");
  assertEquals(s.focus, g3);
  assertEquals(findActive(s), 2, "the dropped tab is active in its new group");
  // Onto another group's right edge: a new split beside that group only.
  s = moveTab(s, 4, g1, "right");
  assertEquals(sig(s.root), "row([1] [4] [2,3])");
  assertEquals(findActive(s), 4);
  // Onto a body's center: the end of that group's tabs; the emptied group closes.
  s = moveTab(s, 4, g3, "center");
  assertEquals(sig(s.root), "row([1] [2,3,4])");
});

Deno.test("dragging a group's active tab onto its own edge splits it off", () => {
  let s = emptyLayout([1, 2]);
  const g = s.focus;
  assert(!isNoopMove(s, 2, g, "bottom"));
  s = moveTab(s, 2, g, "bottom");
  assertEquals(sig(s.root), "col([1] [2])");
});

Deno.test("Ctrl+Alt+arrow moves a tab to the neighbouring group, splitting at the edge", () => {
  let s = threeGroups();
  // 2 → right: the nearest group on the right that overlaps (the top one, 3).
  s = moveTabToNeighbour(s, 2, "right");
  assertEquals(sig(s.root), "row([1] col([3,2] [4]))");
  assertEquals(findActive(s), 2);
  // 2 → down: into the group below.
  s = moveTabToNeighbour(s, 2, "down");
  assertEquals(sig(s.root), "row([1] col([3] [4,2]))");
  // 2 → down again: no group below, so the group splits downward with 2 in the new group.
  s = moveTabToNeighbour(s, 2, "down");
  assertEquals(sig(s.root), "row([1] col([3] [4] [2]))");
  // A group's only tab at the edge stays put.
  const before = JSON.stringify(s);
  assertEquals(JSON.stringify(moveTabToNeighbour(s, 2, "down")), before);
  assertEquals(JSON.stringify(moveTabToNeighbour(s, 1, "left")), JSON.stringify(s));
  // Left from the right column: back to the first group; the emptied group closes.
  s = moveTabToNeighbour(s, 2, "left");
  assertEquals(sig(s.root), "row([1,2] col([3] [4]))");
});

Deno.test("Ctrl+Alt+arrow while zoomed unzooms", () => {
  let s = threeGroups();
  s = toggleZoom(s, paneOfSession(s.root, 1)!.id);
  assert(s.zoom !== null);
  s = moveTabToNeighbour(s, 1, "right");
  assertEquals(s.zoom, null);
  assertEquals(sig(s.root), "row([2] col([3,1] [4]))");
});

Deno.test("shiftTab reorders within the group and stops at the ends", () => {
  let s = emptyLayout([1, 2, 3]);
  s = shiftTab(s, 1, 1);
  assertEquals(sig(s.root), "[2,1,3]");
  s = shiftTab(s, 1, 1);
  assertEquals(sig(s.root), "[2,3,1]");
  const end = s;
  assert(shiftTab(s, 1, 1) === end, "already last");
  s = shiftTab(s, 2, -1);
  assertEquals(sig(s.root), "[2,3,1]", "already first");
  s = shiftTab(s, 3, -1);
  assertEquals(sig(s.root), "[3,2,1]");
});

Deno.test("topRightPane: the group in the window's top-right corner carries the controls", () => {
  const one = emptyLayout([1]);
  assertEquals(topRightPane(one.root), one.focus);
  const s = threeGroups();
  assertEquals(topRightPane(s.root), paneOfSession(s.root, 3)!.id);
  const col = splitPane(emptyLayout([1]), "bottom", 2);
  assertEquals(topRightPane(col.root), paneOfSession(col.root, 1)!.id, "top of a column");
  const row = splitPane(col, "right", 3);
  // col([1] row([2] [3])): the top group spans the full width, so it is the corner.
  assertEquals(topRightPane(row.root), paneOfSession(row.root, 1)!.id);
});

function findActive(s: LayoutState): number | null {
  return allPanes(s.root).find((p) => p.id === s.focus)?.active ?? null;
}

Deno.test("placeOf names a tab's place so putAt can rebuild it in another layout", async () => {
  const { findPane, placeOf, putAt, setSizes } = await import("./tree.ts");
  const T = 99;
  // A group of its own along the bottom edge, a third of the height.
  let l = splitPane(emptyLayout([1, 2]), "bottom", T);
  l = setSizes(l, [], [2 / 3, 1 / 3]);
  const at = placeOf(l, T)!;
  assertEquals([at.alone, Math.round(at.size! * 300)], ["bottom", 100]);
  const other = putAt(addTab(splitPane(emptyLayout([5]), "right", 6), T), T, at);
  assertEquals(sig(other.root), `col(row([5] [6]) [${T}])`);
  // Along the left edge.
  const left = placeOf(splitPane(emptyLayout([1, 2]), "left", T), T)!;
  assertEquals(left.alone, "left");
  assertEquals(sig(putAt(emptyLayout([5, T]), T, left).root), `row([${T}] [5])`);
  // Alone in the middle column is not an edge: it is the middle group.
  let mid = splitPane(emptyLayout([1]), "right", 2);
  mid = splitPane(mid, "left", T, findPane(mid.root, mid.focus)!.id);
  assertEquals(sig(mid.root), `row([1] [${T}] [2])`);
  const m = placeOf(mid, T)!;
  assertEquals([m.group, m.index, m.alone, m.side, m.share], [1, 0, undefined, "right", 0.25]);
  assertEquals(m.near, [paneOfSession(mid.root, 1)!.id]);
  assertEquals(placeOf(emptyLayout([1, 2]), T), null);
});

/** A layout with its split sizes, rounded (to compare a rebuilt layout with the one it was). */
function sized(n: LayoutState["root"]): string {
  if (n.type === "pane") return `[${n.tabs.join(",")}]`;
  return `${n.dir}(${n.children.map((c, i) => `${sized(c)}:${n.sizes[i].toFixed(4)}`).join(" ")})`;
}

Deno.test("putAt puts a tab back exactly where it was in the same layout, by the groups around it", async () => {
  const { placeOf, putAt, setSizes, generalPlace } = await import("./tree.ts");
  const T = 99;
  /** Take the tab out (its group closes), add it back to the focused group, then put it at its place. */
  const roundTrip = (l: LayoutState) => {
    const at = JSON.parse(JSON.stringify(placeOf(l, T)));
    const gone = removeSession(l, T);
    const back = putAt(reconcile(gone, [...allPanes(gone.root).flatMap((p) => p.tabs), T]), T, at);
    assertEquals(sized(back.root), sized(l.root));
    // Every other group keeps its id: the same groups, not look-alikes.
    assertEquals(allPanes(back.root).filter((p) => !p.tabs.includes(T)).map((p) => p.id),
      allPanes(l.root).filter((p) => !p.tabs.includes(T)).map((p) => p.id));
    return { gone, back };
  };
  // Under the right group, inside its column, a third of its height.
  let inner = splitPane(splitPane(emptyLayout([1]), "right", 2), "bottom", T);
  inner = setSizes(inner, [1], [2 / 3, 1 / 3]);
  inner = setSizes(inner, [], [0.4, 0.6]);
  assertEquals(sig(inner.root), `row([1] col([2] [${T}]))`);
  assertEquals(sig(roundTrip(inner).gone.root), "row([1] [2])");
  // The middle of three in a column.
  let three = splitPane(emptyLayout([1]), "bottom", 2);
  three = splitPane(three, "top", T, paneOfSession(three.root, 2)!.id);
  three = setSizes(three, [], [0.5, 0.2, 0.3]);
  assertEquals(sig(three.root), `col([1] [${T}] [2])`);
  roundTrip(three);
  // Under a pair of groups whose row folds into the window's row when it closes: gathered back.
  let fold = splitPane(emptyLayout([1]), "right", 3);
  fold = splitPane(fold, "bottom", T, paneOfSession(fold.root, 1)!.id);
  fold = splitPane(fold, "right", 2, paneOfSession(fold.root, 1)!.id);
  fold = setSizes(fold, [], [0.7, 0.3]);
  assertEquals(sig(fold.root), `row(col(row([1] [2]) [${T}]) [3])`);
  assertEquals(sig(roundTrip(fold).gone.root), "row([1] [2] [3])");
  // Left of everything, along the window edge.
  roundTrip(setSizes(splitPane(splitPane(emptyLayout([1]), "bottom", 2), "left", T), [], [0.2, 0.8]));
  // In a shared group, third in its strip and the open tab there: back third, open again.
  let shared = splitPane(emptyLayout([1]), "right", 2);
  shared = addTab(addTab(addTab(shared, 3), T), 4);
  shared = activateTab(shared, T);
  shared = activateTab(shared, 1);
  assertEquals(sig(shared.root), `row([1] [2,3,${T},4])`);
  const { back } = roundTrip(shared);
  assertEquals(paneOfSession(back.root, T)!.active, T);
  assertEquals(back.focus, shared.focus);

  // The groups it was beside are gone (a terminal closed meanwhile): the place any channel can
  // follow, never lost or doubled.
  const at = placeOf(inner, T)!;
  const gone = removeSession(removeSession(inner, T), 2);
  const fallback = putAt(reconcile(gone, [1, 3, T]), T, at);
  assertEquals(sig(fallback.root), `[${T},1,3]`);
  // The general place drops what only means something in its own channel.
  assertEquals(generalPlace(at), { group: -1, index: 0 });
  assertEquals(generalPlace(placeOf(shared, T)!), { group: -1, index: 2 });
});

Deno.test("putAt keeps an exact place when groups were added or closed beside it, and any share", async () => {
  const { placeOf, putAt, setSizes, placeBeside } = await import("./tree.ts");
  const T = 99;
  // col(row([1] col([2] [3])) [4]) with T beside the [2]/[3] column... built as: [2] over [3] at
  // top right, T to their right, [4] across the bottom.
  let l = splitPane(emptyLayout([1]), "bottom", 4);
  l = splitPane(l, "right", 2, paneOfSession(l.root, 1)!.id);
  l = splitPane(l, "bottom", 3, paneOfSession(l.root, 2)!.id);
  l = splitPane(l, "right", T, paneOfSession(l.root, 2)!.id);
  const at = JSON.parse(JSON.stringify(placeOf(l, T)));
  const gone = removeSession(l, T);
  // Away, a panel opens beside terminal 2 (a new group to its right): the place still holds.
  const panel = placeBeside(gone, -5, 2);
  assertEquals(allPanes(panel.root).length, allPanes(gone.root).length + 1);
  const back = putAt(reconcile(panel, [1, 2, 3, 4, -5, T]), T, at);
  assert(paneOfSession(back.root, T)!.tabs.length === 1, `its own group again: ${sig(back.root)}`);
  // Away, terminal 3 closes: still its own group beside what is left of that branch.
  const closed = removeSession(gone, 3);
  const back2 = putAt(reconcile(closed, [1, 2, 4, T]), T, at);
  assertEquals(paneOfSession(back2.root, T)!.tabs.length, 1);
  assertEquals(sig(back2.root), `col(row([1] [2] [${T}]) [4])`);

  // A narrow share (4% of a wide window) comes back at 4%, not at a minimum.
  let narrow = splitPane(emptyLayout([1]), "right", T);
  narrow = setSizes(narrow, [], [0.96, 0.04]);
  const p = placeOf(narrow, T)!;
  const again = putAt(reconcile(removeSession(narrow, T), [1, T]), T, p);
  assertEquals(sized(again.root), sized(narrow.root));
});

Deno.test("a split along a whole window edge stays along it when the groups beside it closed", async () => {
  const { placeOf, putAt, placeBeside } = await import("./tree.ts");
  const T = 99;
  // [1] over [2], Questions full height on the right.
  let l = splitPane(emptyLayout([1]), "bottom", 2);
  l = splitPane(l, "right", T, paneOfSession(l.root, 1)!.id);
  l = { ...l, root: normalize(l.root) };
  const edge = putAt(l, T, { group: -1, index: 0, alone: "right", size: 0.3 });
  const at = JSON.parse(JSON.stringify(placeOf(edge, T)));
  assertEquals(at.alone, "right");
  // Away: a panel opens beside 2, then 2 closes.
  let away = placeBeside(removeSession(edge, T), -5, 2);
  away = removeSession(away, 2);
  const back = putAt(reconcile(away, [1, -5, T]), T, at);
  assertEquals(placeOf(back, T)!.alone, "right");
});
