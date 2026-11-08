import {
  changeChannelLayout, folderChannel, loadChannelLayout, reconcileChannels, savedChannel, sidebarRatio,
  type TabHome,
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
