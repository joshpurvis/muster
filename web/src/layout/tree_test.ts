import {
  activateTab,
  addTab,
  allPanes,
  chromeForNew,
  chromeOf,
  cycleTab,
  dragGutter,
  emptyLayout,
  equalize,
  focusDirection,
  isNoopMove,
  type LayoutState,
  MIN_SPLIT,
  moveTab,
  neighbour,
  normalize,
  pane,
  paneOfSession,
  panePx,
  paneRects,
  parseLayout,
  reconcile,
  removeSession,
  reorderTab,
  resizeFocused,
  sig,
  splitFits,
  splitPane,
  toggleZoom,
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

Deno.test("pane chrome: none alone, header with 2+ panes, strip with 2+ sessions", () => {
  assertEquals(chromeOf(1, 1), "none");
  assertEquals(chromeOf(0, 1), "none", "empty lone pane (launcher)");
  assertEquals(chromeOf(1, 2), "head");
  assertEquals(chromeOf(2, 1), "strip");
  assertEquals(chromeOf(3, 4), "strip", "a strip replaces the header");
});

Deno.test("chromeForNew predicts the chrome where a new session lands", () => {
  let s = emptyLayout([1]);
  const p1 = s.focus;
  assertEquals(chromeForNew(s, "tab", p1), "strip", "second session in the only pane");
  assertEquals(chromeForNew(s, "split", p1), "head", "a split makes two panes");
  s = emptyLayout([]);
  assertEquals(chromeForNew(s, "tab", s.focus), "none", "first session of a new tab");
  s = splitPane(emptyLayout([1]), "right", 2);
  const p2 = paneOfSession(s.root, 2)!.id;
  assertEquals(chromeForNew(s, "tab", p2), "strip");
  s = splitPane(s, "bottom", null);
  assertEquals(chromeForNew(s, "tab", s.focus), "head", "empty pane among others");
});
