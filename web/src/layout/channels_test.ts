import {
  changeChannelLayout, folderChannel, loadChannelLayout, reconcileChannels, rememberPlace, savedChannel,
  sidebarRatio, type TabHome, type WindowLayoutState,
} from "./channels.ts";
import {
  addTab, allPanes, allSessions, emptyLayout, findPane, noteTab, panelTab,
  setSizes, sig, splitPane, toggleZoom,
} from "./tree.ts";
import type { Channel } from "../types.ts";

function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}
function assert(value: unknown, message: string) { if (!value) throw new Error(message); }

Deno.test("channel migration partitions mixed tiles, preserving tab order, ratios and selection", () => {
  let old = splitPane(emptyLayout([1, 2, 3]), "right", 4);
  old = addTab(old, 5, old.focus);
  old = setSizes(old, [], [0.3, 0.7]);
  old = toggleZoom(old);
  const tabs = [
    { id: 1, channel: 10 }, { id: 2, channel: 20 }, { id: 3, channel: 10 },
    { id: 4, channel: 20 }, { id: 5, channel: 10 },
  ];
  const migrated = loadChannelLayout(old, [10, 20], tabs);
  equal(migrated.active, 10);
  equal(sig(migrated.layouts[10].root), "row([1,3] [5])");
  equal(sig(migrated.layouts[20].root), "row([2] [4])");
  assert(migrated.layouts[10].root.type === "split", "still split");
  if (migrated.layouts[10].root.type === "split") equal(migrated.layouts[10].root.sizes, [0.3, 0.7]);
  equal(findPane(migrated.layouts[10].root, migrated.layouts[10].focus)?.active, 5);
  equal(migrated.layouts[10].zoom, old.zoom);
  equal(migrated.previous, old);
  equal(loadChannelLayout(JSON.parse(JSON.stringify(migrated)), [10, 20], tabs), migrated);
});

Deno.test("legacy notes follow their tile or opener and stay in that channel after switching", () => {
  let old = emptyLayout([1, noteTab(1)]);
  old = splitPane(old, "right", 2);
  old = addTab(old, noteTab(2));
  const tabs: TabHome[] = [
    { id: 1, channel: 10 }, { id: 2, channel: 20 },
    { id: noteTab(1), channel: null }, { id: noteTab(2), channel: null },
    { id: noteTab(3), channel: null, beside: 1 },
  ];
  const s = loadChannelLayout(old, [10, 20], tabs);
  equal(savedChannel(s, noteTab(1)), 10);
  equal(savedChannel(s, noteTab(2)), 20);
  equal(savedChannel(s, noteTab(3)), 10);
  equal(s.active, 20);
  const next = reconcileChannels({ ...s, active: 20 }, [10, 20], tabs);
  equal(savedChannel(next, noteTab(1)), 10);
  equal(savedChannel(next, noteTab(2)), 20);
});

Deno.test("background arrivals go to their channel without changing the visible workspace", () => {
  let tabs: TabHome[] = [{ id: 1, channel: 10 }, { id: 2, channel: 20 }];
  const s = loadChannelLayout(null, [10, 20], tabs);
  const before = s.layouts[10];
  tabs = [...tabs, { id: 3, channel: 20 }, { id: panelTab(1), channel: 20, beside: 2 }];
  const next = reconcileChannels(s, [10, 20], tabs);
  equal(next.active, 10);
  equal(next.layouts[10], before);
  equal(allSessions(next.layouts[20].root).sort((a, b) => a - b), [-2, 2, 3]);
  equal(allPanes(next.layouts[20].root).length, 2);
});

Deno.test("a late spawn result updates its captured channel after selection changed", () => {
  const tabs = [{ id: 1, channel: 10 }, { id: 2, channel: 20 }, { id: 3, channel: 10 }];
  let s = loadChannelLayout(null, [10, 20], tabs.slice(0, 2));
  s = { ...s, active: 20 };
  const before = s.layouts[20];
  s = changeChannelLayout(s, 10, (l) => splitPane(l, "right", 3));
  s = reconcileChannels(s, [10, 20], tabs);
  equal(s.active, 20);
  equal(s.layouts[20], before);
  equal(sig(s.layouts[10].root), "row([1] [3])");
});

Deno.test("empty channels persist; archive selects a surviving channel and keeps notes accessible", () => {
  let s = loadChannelLayout(null, [10, 20], [{ id: 1, channel: 10 }]);
  s = reconcileChannels(s, [10, 20], []);
  equal(s.active, 10);
  equal(allSessions(s.layouts[10].root), []);
  s = reconcileChannels(s, [20], [{ id: noteTab(1), channel: 0 }]);
  equal(s.active, 20);
  equal(s.layouts[10], undefined);
  equal(savedChannel(s, noteTab(1)), 0);
});

Deno.test("one channel in two windows keeps independent tiles and no foreign tabs", () => {
  const left = loadChannelLayout(splitPane(emptyLayout([1]), "right", 2), [10],
    [{ id: 1, channel: 10 }, { id: 2, channel: 10 }]);
  const right = loadChannelLayout(emptyLayout([3]), [10], [{ id: 3, channel: 10 }]);
  const moved = reconcileChannels(left, [10], [{ id: 1, channel: 10 }]);
  const received = reconcileChannels(right, [10], [{ id: 3, channel: 10 }, { id: 2, channel: 10 }]);
  equal(allSessions(moved.layouts[10].root), [1]);
  equal(allSessions(received.layouts[10].root), [3, 2]);
  equal(allSessions(right.layouts[10].root), [3]);
});

Deno.test("moving tabs reconciles source and destination exactly once without erasing either layout", () => {
  const s = loadChannelLayout(null, [10, 20], [{ id: 1, channel: 10 }, { id: 2, channel: 20 }]);
  const next = reconcileChannels(s, [10, 20], [{ id: 1, channel: 20 }, { id: 2, channel: 20 }]);
  equal(allSessions(next.layouts[10].root), []);
  equal([...allSessions(next.layouts[20].root)].sort(), [1, 2]);
  equal(next.active, 10);
});

Deno.test("a notes-only legacy window has a stable fallback, with no invented project", () => {
  const tabs = [{ id: noteTab(7), channel: null }];
  const s = loadChannelLayout(emptyLayout([noteTab(7)]), [], tabs);
  equal(s.active, 0);
  equal(savedChannel(s, noteTab(7)), 0);
  equal(loadChannelLayout(s, [], tabs), s);
});

Deno.test("sidebar preferences are window-wide, validated and survive channel reloads", () => {
  // Older saves carry the removed grouping fields; they are dropped, not kept.
  const s = loadChannelLayout({ v: 2, layouts: {}, active: 999,
    sidebar: { split: 4, agents: false, channels: true, collapsedProjects: [1, "bad"],
      groupBy: "account", collapsedAccounts: ["claude:7", 42] } }, [10], []);
  equal(s.active, 10);
  equal(s.sidebar, { hidden: false, channels: true, agents: false, split: 0.8 });
  equal(loadChannelLayout(s, [10], []).sidebar, s.sidebar);
  equal(sidebarRatio(NaN), 0.45);
  equal(sidebarRatio(-1), 0.2);
});

Deno.test("a new channel in an open folder joins it, across accounts", () => {
  const ch = (id: number, cwd: string, extra: Partial<Channel> = {}) =>
    ({ id, name: "muster", cwd, owns_worktree: false, archived: false, ...extra }) as Channel;
  const elevated = { codex_sandbox: "full_access", claude_mode: "bypass" } as const;
  const list = [
    ch(1, "/p/muster/.wt/feature", { owns_worktree: true }),
    ch(2, "/p/other"),
    ch(3, "/p/muster"),
    ch(4, "/p/muster"),
    ch(5, "/p/muster", { permissions: elevated }),
  ];
  equal(folderChannel(list, "/p/muster/")?.id, 3);
  equal(folderChannel(list, "/p/muster", { name: "muster" })?.id, 3);
  equal(folderChannel(list, "/p/muster", { name: "  " })?.id, 3);
  equal(folderChannel(list, "/p/muster", { name: "scratch" }), undefined);
  equal(folderChannel(list, "/p/muster/.wt"), undefined);
  equal(folderChannel([ch(6, "/p/muster", { archived: true })], "/p/muster"), undefined);
  // The selected channel wins among matches; a channel with no sessions is never joined.
  equal(folderChannel(list, "/p/muster", { prefer: 4 })?.id, 4);
  equal(folderChannel(list, "/p/muster", { prefer: 2 })?.id, 3);
  equal(folderChannel(list, "/p/muster", { live: (id) => id !== 3 })?.id, 4);
  // An existing worktree channel's own folder joins it too.
  equal(folderChannel(list, "/p/muster/.wt/feature")?.id, 1);
  // Permissions must equal the channel's: defaults never join an elevated channel.
  equal(folderChannel(list, "/p/muster", { prefer: 5 })?.id, 3);
  equal(folderChannel([list[4]], "/p/muster"), undefined);
  equal(folderChannel([list[4]], "/p/muster", { permissions: elevated })?.id, 5);
  equal(folderChannel([list[4]], "/p/muster", { anyPermissions: true })?.id, 5);
  equal(folderChannel(list, "/p/muster", { permissions: { codex_sandbox: "read_only", claude_mode: "plan" } }), undefined);
});

Deno.test("drawer shells stay outside tiles across reconciliation and reload", () => {
  const tabs = [{ id: 1, channel: 10 }, { id: 2, channel: 10 }, { id: 3, channel: 20 }];
  const state = loadChannelLayout(null, [10, 20], tabs);
  state.drawers = { 10: { session: 2, open: true, height: 300 } };
  const next = reconcileChannels(state, [10, 20], tabs);
  equal(allSessions(next.layouts[10].root), [1]);
  equal(next.drawers, state.drawers);
  const restored = loadChannelLayout(JSON.parse(JSON.stringify(next)), [10, 20], tabs);
  equal(restored.layouts, next.layouts);
  equal(restored.drawers, next.drawers);
  const moved = reconcileChannels(next, [10, 20], tabs.map((t) => t.id === 2 ? { ...t, channel: 20 } : t));
  equal(moved.drawers?.[10].session, null);
  assert(allSessions(moved.layouts[20].root).includes(2), "moved shell returns to ordinary tabs");
  equal(reconcileChannels(next, [20], [tabs[2]]).drawers, {});
});

Deno.test("drawer data rejects invalid session ids and clamps saved heights", () => {
  const state = loadChannelLayout({ v: 2, drawers: {
    10: { session: -1, open: "yes", height: 99999 },
    20: { session: 2, open: true, height: "bad" },
  } }, [10, 20], [{ id: 2, channel: 10 }]);
  equal(state.drawers, {
    10: { session: null, open: false, height: 700 },
    20: { session: null, open: true, height: 260 },
  });
  equal(allSessions(state.layouts[10].root), [2]);
});

Deno.test("the Questions tab starts first in the first group of whichever channel is shown", async () => {
  const { QUESTIONS_TAB: Q, activateTab, noteId, panelId, pinFirst } = await import("./tree.ts");
  equal([noteId(Q), panelId(Q)], [null, null]);
  // Two groups; the tab arrives in the focused (right) one, not selected, and moves to the front
  // of the left one without being selected there either.
  const split = splitPane(emptyLayout([1, 2]), "right", 3);
  const l = activateTab(addTab(split, Q, split.focus), 3);
  const pinned = pinFirst(l, Q);
  equal(sig(pinned.root), `row([${Q},1,2] [3])`);
  equal(allPanes(pinned.root)[0].active, 1);
  equal(allPanes(pinned.root)[1].active, 3);
  equal(pinned.focus, l.focus);
  // Selected where it was: still selected in its new place, and focus follows it.
  const moved = pinFirst(addTab(split, Q, split.focus), Q);
  equal(sig(moved.root), `row([${Q},1,2] [3])`);
  equal(allPanes(moved.root)[0].active, Q);
  equal(moved.focus, allPanes(moved.root)[0].id);
  // Moved elsewhere in its own group, pinFirst puts it back (only when it arrives, see below).
  const dragged = pinFirst(emptyLayout([1, Q, 2]), Q);
  equal(sig(dragged.root), `[${Q},1,2]`);
  equal(pinFirst(dragged, Q), dragged);

  // Reconciling: it follows the shown channel and leaves the others; hidden, it is nowhere.
  const tabs = (active: number): TabHome[] => [
    { id: 1, channel: 10 }, { id: 2, channel: 20 }, { id: Q, channel: active, questions: true },
  ];
  let w = loadChannelLayout({ v: 2, active: 10, layouts: {}, sidebar: {} }, [10, 20], tabs(10));
  equal(sig(w.layouts[10].root), `[${Q},1]`);
  equal(sig(w.layouts[20].root), "[2]");
  w = reconcileChannels({ ...w, active: 20 }, [10, 20], tabs(20));
  equal(sig(w.layouts[10].root), "[1]");
  equal(sig(w.layouts[20].root), `[${Q},2]`);
  equal(allPanes(w.layouts[20].root)[0].active, 2);
  // Hiding it is saved with the window; a saved layout without the field shows it.
  const hidden = loadChannelLayout(JSON.parse(JSON.stringify({ ...w, questions: false })), [10, 20],
    tabs(20).slice(0, 2));
  equal(hidden.questions, false);
  equal(sig(hidden.layouts[20].root), "[2]");
  equal(loadChannelLayout(JSON.parse(JSON.stringify(w)), [10, 20], tabs(20)).questions, undefined);
});

const QTAB = -(2 ** 52);
const qTabs = (active: number, extra: TabHome[] = []): TabHome[] => [
  { id: 1, channel: 10 }, { id: 2, channel: 10 }, { id: 3, channel: 20 }, { id: 4, channel: 20 }, ...extra,
  { id: QTAB, channel: active, questions: true },
];
/** Channel 10 with groups [Questions,1] [2] (a two-group channel as it loads today). */
function twoGroups(): WindowLayoutState {
  const w = loadChannelLayout({ v: 2, active: 10, layouts: {}, sidebar: {} }, [10, 20], qTabs(10));
  const ten = splitPane(w.layouts[10], "right", 2, allPanes(w.layouts[10].root)[0].id);
  return reconcileChannels({ ...w, layouts: { ...w.layouts, 10: ten } }, [10, 20], qTabs(10));
}

Deno.test("a dragged Questions tab stays where it is dropped and the window remembers it", async () => {
  const { moveTab } = await import("./tree.ts");
  let w = twoGroups();
  equal(sig(w.layouts[10].root), `row([${QTAB},1] [2])`);
  // Dropped into the right group's strip, after its tab: no snapping back to the front.
  const right = allPanes(w.layouts[10].root)[1].id;
  w = changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, right, "center", 1));
  w = rememberPlace(reconcileChannels(w, [10, 20], qTabs(10)), QTAB);
  equal(sig(w.layouts[10].root), `row([1] [2,${QTAB}])`);
  equal(w.questionsAt, { group: -1, index: -1 });
  // Another change in the channel leaves it there (a new tab joins the focused group after it).
  w = reconcileChannels(w, [10, 20], qTabs(10, [{ id: 5, channel: 10 }]));
  equal(sig(w.layouts[10].root), `row([1] [2,${QTAB},5])`);

  // In another channel it arrives at the same place: the right group of two, at the end.
  const twenty = splitPane(w.layouts[20], "right", 4, allPanes(w.layouts[20].root)[0].id);
  w = reconcileChannels({ ...w, active: 20, layouts: { ...w.layouts, 20: twenty } }, [10, 20], qTabs(20, [{ id: 5, channel: 10 }]));
  equal(sig(w.layouts[20].root), `row([3] [4,${QTAB}])`);
  equal(allSessions(w.layouts[10].root).includes(QTAB), false);
  // In a one-group channel, second in that group, as it was when it left.
  const one = loadChannelLayout(JSON.parse(JSON.stringify({ ...w, active: 10, layouts: { 10: w.layouts[10] } })),
    [10, 20], qTabs(20));
  equal(sig(one.layouts[20].root), `[3,${QTAB},4]`);
});

Deno.test("Questions dragged into a new split along the right edge gets that split in each channel", async () => {
  const { moveTab, paneRects } = await import("./tree.ts");
  let w = twoGroups();
  // Onto the right edge of the right group: a third, full-height group with Questions alone.
  const right = allPanes(w.layouts[10].root)[1].id;
  w = changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, right, "right"));
  w = rememberPlace(reconcileChannels(w, [10, 20], qTabs(10)), QTAB);
  equal(sig(w.layouts[10].root), `row([1] [2] [${QTAB}])`);
  equal(w.questionsAt?.alone, "right");
  const size = w.questionsAt!.size!;
  assert(Math.abs(size - 0.25) < 1e-9, `share ${size}`);

  // Channel 20 (one group) gets Questions in a group of its own on the right, the same share,
  // without moving focus or selection there.
  const focus = w.layouts[20].focus;
  w = reconcileChannels({ ...w, active: 20 }, [10, 20], qTabs(20));
  const l20 = w.layouts[20];
  equal(sig(l20.root), `row([3,4] [${QTAB}])`);
  equal(l20.focus, focus);
  const q = allPanes(l20.root)[1];
  assert(Math.abs(paneRects(l20.root).get(q.id)!.w - size) < 1e-9, "same share of the window");
  // Channel 10, left behind, closes the group it had: no empty group, no copy of the tab.
  equal(sig(w.layouts[10].root), "row([1] [2])");
  // Back in channel 10, its own group on the right again.
  w = reconcileChannels({ ...w, active: 10 }, [10, 20], qTabs(10));
  equal(sig(w.layouts[10].root), `row([1] [2] [${QTAB}])`);
  equal(sig(w.layouts[20].root), "[3,4]");
});

Deno.test("the remembered Questions place survives a save and reload; old saves load as before", async () => {
  const { moveTab } = await import("./tree.ts");
  let w = twoGroups();
  w = rememberPlace(changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, allPanes(l.root)[1].id, "center", 0)), QTAB);
  w = reconcileChannels({ ...w, active: 20 }, [10, 20], qTabs(20));
  w = reconcileChannels({ ...w, active: 10 }, [10, 20], qTabs(10));
  equal(sig(w.layouts[10].root), `row([1] [${QTAB},2])`);
  const saved = JSON.parse(JSON.stringify(w));
  const back = loadChannelLayout(saved, [10, 20], qTabs(10));
  equal(back.questionsAt, { group: -1, index: 0 });
  equal(sig(back.layouts[10].root), `row([1] [${QTAB},2])`);

  // A save from before Questions could move: no questionsAt, the tab first in the first group.
  const old = twoGroups();
  const { questionsAt: _none, ...oldSave } = JSON.parse(JSON.stringify(old));
  const loaded = loadChannelLayout(oldSave, [10, 20], qTabs(10));
  equal(loaded.questionsAt, undefined);
  equal(sig(loaded.layouts[10].root), `row([${QTAB},1] [2])`);
  // ...and switching channels there still puts it first in the first group.
  const switched = reconcileChannels({ ...loaded, active: 20 }, [10, 20], qTabs(20));
  equal(sig(switched.layouts[20].root), `[${QTAB},3,4]`);
  // A broken saved place is ignored.
  for (const bad of [{ group: "x", index: 0 }, { group: 0.5, index: 0 }, { group: -7, index: 0 }, 5, null]) {
    equal(loadChannelLayout({ ...oldSave, questionsAt: bad }, [10, 20], qTabs(10)).questionsAt, undefined);
  }
  equal(loadChannelLayout({ ...oldSave, questionsAt: { group: 1, index: 0, alone: "middle", size: 3 } }, [10, 20],
    qTabs(10)).questionsAt, { group: 1, index: 0 });
});

Deno.test("Questions never vanishes when its group goes: it comes back at its place, never twice", async () => {
  const { removeSession } = await import("./tree.ts");
  let w: WindowLayoutState = { ...twoGroups(), questionsAt: { group: -1, index: 0, alone: "right", size: 0.3 } };
  // Its group taken away (any change that drops the tab): the next reconcile puts it back.
  w = reconcileChannels(changeChannelLayout(w, 10, (l) => removeSession(l, QTAB)), [10, 20], qTabs(10));
  equal(sig(w.layouts[10].root), `row([1] [2] [${QTAB}])`);
  equal(allSessions(w.layouts[10].root).filter((t) => t === QTAB).length, 1);
  // Hidden, then shown again: back in its own group on the right.
  w = reconcileChannels({ ...w, questions: false }, [10, 20], qTabs(10).filter((t) => t.id !== QTAB));
  equal(sig(w.layouts[10].root), "row([1] [2])");
  const { questions: _shown, ...again } = w;
  w = reconcileChannels(again, [10, 20], qTabs(10));
  equal(sig(w.layouts[10].root), `row([1] [2] [${QTAB}])`);
  // A remembered group that no longer exists falls back to the last one there is.
  w = { ...w, questionsAt: { group: 5, index: 9 } };
  w = reconcileChannels(changeChannelLayout(w, 10, (l) => removeSession(l, QTAB)), [10, 20], qTabs(10));
  equal(sig(w.layouts[10].root), `row([1] [2,${QTAB}])`);
  // An empty channel: Questions is its only tab.
  const empty = reconcileChannels({ ...w, active: 30 }, [10, 20, 30], qTabs(30));
  equal(sig(empty.layouts[30].root), `[${QTAB}]`);
});

Deno.test("reload keeps a Questions split inside a column, even before the window shows the tab", async () => {
  const { moveTab } = await import("./tree.ts");
  let w = twoGroups();
  // Onto the bottom edge of the right group: a split inside the right column.
  const right = allPanes(w.layouts[10].root)[1].id;
  w = rememberPlace(reconcileChannels(changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, right, "bottom")),
    [10, 20], qTabs(10)), QTAB);
  equal(sig(w.layouts[10].root), `row([1] col([2] [${QTAB}]))`);
  // Loading: the window doesn't show the tab yet (its tab list has no Questions), only a kept one.
  const saved = JSON.parse(JSON.stringify(w));
  const kept: TabHome = { id: QTAB, channel: null, questions: true, kept: true };
  const loaded = loadChannelLayout(saved, [10, 20], [...qTabs(10).filter((t) => t.id !== QTAB), kept]);
  equal(sig(loaded.layouts[10].root), `row([1] col([2] [${QTAB}]))`);
  // Then shown: it stays put. Or hidden by the window: gone, with no empty group.
  equal(sig(reconcileChannels(loaded, [10, 20], qTabs(10)).layouts[10].root), `row([1] col([2] [${QTAB}]))`);
  equal(sig(reconcileChannels(loaded, [10, 20], qTabs(10).filter((t) => t.id !== QTAB)).layouts[10].root), "row([1] [2])");
  // A kept tab never lands anywhere it wasn't: not in a save without it, not in a legacy tree.
  const { questionsAt: _q, ...plain } = saved;
  const none = loadChannelLayout({ ...plain, layouts: { 10: { v: 1, root: { type: "pane", id: "a", tabs: [1, 2] }, focus: "a", zoom: null } } },
    [10, 20], [...qTabs(10).filter((t) => t.id !== QTAB), kept]);
  equal(allSessions(none.layouts[10].root).includes(QTAB), false);
  const legacy = loadChannelLayout({ type: "pane", id: "a", tabs: [1, 2] }, [10, 20],
    [...qTabs(10).filter((t) => t.id !== QTAB), kept]);
  equal(Object.values(legacy.layouts).some((l) => allSessions(l.root).includes(QTAB)), false);
});

Deno.test("Questions remembers a resize or the tabs around it when it leaves a channel", async () => {
  const { moveTab, paneRects, setSizes } = await import("./tree.ts");
  let w = twoGroups();
  const right = allPanes(w.layouts[10].root)[1].id;
  w = rememberPlace(reconcileChannels(changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, right, "right")),
    [10, 20], qTabs(10)), QTAB);
  // Widened to 40% with the gutter (no Questions move), then hidden and shown again: still 40%.
  w = changeChannelLayout(w, 10, (l) => setSizes(l, [], [0.3, 0.3, 0.4]));
  w = reconcileChannels({ ...w, questions: false }, [10, 20], qTabs(10).filter((t) => t.id !== QTAB));
  assert(Math.abs(w.questionsAt!.size! - 0.4) < 1e-9, `remembered ${w.questionsAt!.size}`);
  const { questions: _h, ...shown } = w;
  w = reconcileChannels(shown, [10, 20], qTabs(10));
  const q = allPanes(w.layouts[10].root).find((p) => p.tabs.includes(QTAB))!;
  assert(Math.abs(paneRects(w.layouts[10].root).get(q.id)!.w - 0.4) < 1e-9, "back at 40%");

  // In the right group's strip, then a tab dragged before it: after a channel switch and back,
  // it is still second there, not first.
  w = twoGroups();
  const r = allPanes(w.layouts[10].root)[1].id;
  w = rememberPlace(changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, r, "center", 0)), QTAB);
  w = reconcileChannels(w, [10, 20], qTabs(10, [{ id: 5, channel: 10 }]));
  w = changeChannelLayout(w, 10, (l) => moveTab(l, 5, r, "center", 0));
  equal(sig(w.layouts[10].root), `row([1] [5,${QTAB},2])`);
  w = reconcileChannels({ ...w, active: 20 }, [10, 20], qTabs(20, [{ id: 5, channel: 10 }]));
  w = reconcileChannels({ ...w, active: 10 }, [10, 20], qTabs(10, [{ id: 5, channel: 10 }]));
  equal(sig(w.layouts[10].root), `row([1] [5,${QTAB},2])`);

  // Leaving a one-group channel keeps "the last group": the next two-group channel gets it on
  // the right, not the left.
  w = twoGroups();
  w = rememberPlace(changeChannelLayout(w, 10, (l) => moveTab(l, QTAB, allPanes(l.root)[1].id, "center")), QTAB);
  equal(w.questionsAt, { group: -1, index: -1 });
  w = reconcileChannels({ ...w, active: 20 }, [10, 20], qTabs(20));
  equal(sig(w.layouts[20].root), `[3,4,${QTAB}]`);
  w = reconcileChannels({ ...w, active: 10 }, [10, 20], qTabs(10));
  equal(w.questionsAt, { group: -1, index: -1 });
  equal(sig(w.layouts[10].root), `row([1] [2,${QTAB}])`);
});

