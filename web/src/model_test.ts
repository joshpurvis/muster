import {
  aggregateState,
  basename,
  documentTitle,
  isNoopSlot,
  type JumpCandidate,
  moveBy,
  moveChannelLocally,
  moveTo,
  needsYouOrder,
  nextInCycle,
  pushFront,
  rankJump,
  tabLabel,
  tabsOf,
  upsertWindow,
  windowIdOf,
  windowLabel,
  windowParam,
} from "./model.ts";
import type { AgentState, Channel, SessionInfo, Win } from "./types.ts";

function assertEquals<T>(actual: T, expected: T, msg?: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg ?? "assertEquals"}\n  actual:   ${a}\n  expected: ${e}`);
}

function ch(id: number, extra: Partial<Channel> = {}): Channel {
  return {
    id,
    workspace_id: 1,
    name: `c${id}`,
    cwd: "/home/u/proj",
    branch: null,
    base_branch: null,
    owns_worktree: false,
    port_base: 20000,
    archived: false,
    layout: null,
    created_at: id,
    last_read_at: 0,
    sort_order: id,
    ...extra,
  };
}

function win(id: number, channel_ids: number[], extra: Partial<Win> = {}): Win {
  return { id, name: String(id), channel_ids, open: true, last_focused_at: 0, created_at: 0, ...extra };
}

function sess(id: number, channel_id: number, state: AgentState, state_since = 0): SessionInfo {
  return {
    id,
    channel_id,
    kind: "claude",
    name: `s${id}`,
    cwd: "/",
    state,
    state_detail: null,
    state_since,
    title: null,
    parent_id: null,
    pid: null,
    exit_code: null,
    cols: 80,
    rows: 24,
    queued: 0,
    created_at: 0,
  };
}

const ids = (cs: Channel[]) => cs.map((c) => c.id);

// ---------------- tabs and windows ----------------

Deno.test("tabsOf follows the window's order and keeps other windows' tabs out", () => {
  const cs = [
    ch(1, { window_id: 1 }),
    ch(2, { window_id: 1 }),
    ch(3, { window_id: 2 }),
    ch(4, { window_id: 1 }),
  ];
  assertEquals(ids(tabsOf(win(1, [4, 1, 2]), cs)), [4, 1, 2]);
  assertEquals(ids(tabsOf(win(2, [3]), cs)), [3]);
});

Deno.test("tabsOf appends tabs whose window event hasn't arrived yet, oldest first", () => {
  const cs = [
    ch(5, { window_id: 1, created_at: 50 }),
    ch(1, { window_id: 1 }),
    ch(4, { window_id: 1, created_at: 40 }),
  ];
  assertEquals(ids(tabsOf(win(1, [1]), cs)), [1, 4, 5]);
});

Deno.test("tabsOf: channel window_id wins over a stale channel_ids entry", () => {
  // The channel moved to window 2; window 1's event still lists it.
  const cs = [ch(1, { window_id: 1 }), ch(2, { window_id: 2 })];
  assertEquals(ids(tabsOf(win(1, [1, 2]), cs)), [1]);
});

Deno.test("tabsOf: channels without window_id are matched by channel_ids", () => {
  const cs = [ch(1), ch(2), ch(3)];
  assertEquals(ids(tabsOf(win(1, [3, 1]), cs)), [3, 1]);
});

Deno.test("tabsOf skips archived channels and unassigned ones", () => {
  const cs = [ch(1, { window_id: 1, archived: true }), ch(2, { window_id: null }), ch(3, { window_id: 1 })];
  assertEquals(ids(tabsOf(win(1, [1, 2, 3]), cs)), [3]);
});

Deno.test("tabsOf without windows: every live channel in sort_order", () => {
  const cs = [ch(1, { sort_order: 2 }), ch(2, { sort_order: 0 }), ch(3, { sort_order: 1, archived: true })];
  assertEquals(ids(tabsOf(null, cs)), [2, 1]);
});

Deno.test("windowIdOf prefers the channel's window_id", () => {
  const ws = [win(1, [1]), win(2, [2])];
  assertEquals(windowIdOf(ch(1, { window_id: 2 }), ws), 2);
  assertEquals(windowIdOf(ch(2), ws), 2);
  assertEquals(windowIdOf(ch(9), ws), null);
  assertEquals(windowIdOf(ch(1, { window_id: null }), ws), null);
});

Deno.test("moveTo uses insertion slots like a drop marker", () => {
  assertEquals(moveTo([1, 2, 3, 4], 1, 3), [2, 3, 1, 4]);
  assertEquals(moveTo([1, 2, 3, 4], 4, 0), [4, 1, 2, 3]);
  assertEquals(moveTo([1, 2, 3, 4], 2, 4), [1, 3, 4, 2]);
  assertEquals(moveTo([1, 2, 3], 2, 1), [1, 2, 3], "own slot");
  assertEquals(moveTo([1, 2, 3], 9, 0), [1, 2, 3], "unknown id");
});

Deno.test("moveBy steps one place and stops at the ends", () => {
  assertEquals(moveBy([1, 2, 3], 2, -1), [2, 1, 3]);
  assertEquals(moveBy([1, 2, 3], 2, 1), [1, 3, 2]);
  const l = [1, 2, 3];
  assertEquals(moveBy(l, 1, -1) === l, true, "first can't go left");
  assertEquals(moveBy(l, 3, 1) === l, true, "last can't go right");
});

Deno.test("isNoopSlot: the two slots around a tab change nothing", () => {
  assertEquals(isNoopSlot([1, 2, 3], 2, 1), true);
  assertEquals(isNoopSlot([1, 2, 3], 2, 2), true);
  assertEquals(isNoopSlot([1, 2, 3], 2, 0), false);
  assertEquals(isNoopSlot([1, 2, 3], 2, 3), false);
});

Deno.test("moveChannelLocally moves a tab between windows' orders", () => {
  const ws = [win(1, [1, 2, 3]), win(2, [7])];
  const out = moveChannelLocally(ws, 2, 2);
  assertEquals(out.map((w) => w.channel_ids), [[1, 3], [7, 2]]);
  assertEquals(moveChannelLocally(ws, 3, 2, 0).map((w) => w.channel_ids), [[1, 2], [3, 7]]);
  // Within one window: acts as a reorder.
  assertEquals(moveChannelLocally(ws, 3, 1, 0)[0].channel_ids, [3, 1, 2]);
  // Untouched windows keep their identity (no needless re-render).
  assertEquals(moveChannelLocally(ws, 1, 1)[1] === ws[1], true);
});

Deno.test("upsertWindow replaces by id and keeps ids ascending", () => {
  let ws = upsertWindow([], win(2, []));
  ws = upsertWindow(ws, win(1, [5]));
  ws = upsertWindow(ws, win(2, [9]));
  assertEquals(ws.map((w) => [w.id, w.channel_ids]), [[1, [5]], [2, [9]]]);
});

Deno.test("windowParam reads a positive window id from the hash", () => {
  assertEquals(windowParam("#code=abc&window=3"), 3);
  assertEquals(windowParam("window=12"), 12);
  assertEquals(windowParam("#window=0"), null);
  assertEquals(windowParam("#window=x"), null);
  assertEquals(windowParam("#token=t"), null);
});

Deno.test("windowLabel and documentTitle", () => {
  assertEquals(windowLabel({ id: 2, name: "2" }), "Window 2");
  assertEquals(windowLabel({ id: 2, name: "review" }), "review");
  assertEquals(windowLabel({ id: 4, name: "" }), "Window 4");
  assertEquals(documentTitle({ id: 2, name: "2" }, 0), "muster · 2");
  assertEquals(documentTitle({ id: 2, name: "review" }, 3), "(3) muster · review");
  assertEquals(documentTitle(null, 1), "(1) muster");
});

// ---------------- names ----------------

Deno.test("basename", () => {
  assertEquals(basename("/home/u/proj/"), "proj");
  assertEquals(basename("proj"), "proj");
  assertEquals(basename("/"), "/");
});

Deno.test("tabLabel: human names as is; auto names muted until something was derived", () => {
  const ws = { name: "proj", path: "/home/u/proj" };
  assertEquals(tabLabel(ch(1, { name: "fix auth" }), ws), { text: "fix auth", muted: false });
  assertEquals(tabLabel(ch(1, { name: "Fix the login bug", auto_named: true }), ws), {
    text: "Fix the login bug",
    muted: false,
  });
  assertEquals(tabLabel(ch(1, { name: "proj", auto_named: true }), ws), { text: "proj", muted: true });
  // A worktree tab: cwd is <data>/worktrees/<slug>/<id>, the placeholder is still the folder name.
  const wt = ch(7, { name: "proj", auto_named: true, cwd: "/data/worktrees/proj-1/7", owns_worktree: true });
  assertEquals(tabLabel(wt, ws), { text: "proj", muted: true });
  // A human may name a tab like its folder: then it isn't a placeholder.
  assertEquals(tabLabel(ch(1, { name: "proj", auto_named: false }), ws), { text: "proj", muted: false });
  // Nothing at all: the folder, muted.
  assertEquals(tabLabel(ch(1, { name: "  " }), ws), { text: "proj", muted: true });
  assertEquals(tabLabel(ch(1, { name: "" }), null), { text: "proj", muted: true });
});

// ---------------- needs you ----------------

Deno.test("aggregateState picks the most urgent", () => {
  assertEquals(aggregateState([sess(1, 1, "idle"), sess(2, 1, "working")]), "working");
  assertEquals(aggregateState([sess(1, 1, "error"), sess(2, 1, "needs_approval")]), "needs_approval");
  assertEquals(aggregateState([]), null);
});

Deno.test("needsYouOrder: urgent states first, then longest waiting; live channels only", () => {
  const list = [
    sess(1, 1, "error", 10),
    sess(2, 1, "needs_approval", 30),
    sess(3, 2, "needs_approval", 20),
    sess(4, 2, "working", 0),
    sess(5, 3, "asking_question", 5),
    sess(6, 9, "needs_approval", 1), // archived channel
  ];
  assertEquals(needsYouOrder(list, new Set([1, 2, 3])).map((s) => s.id), [3, 2, 5, 1]);
});

Deno.test("Alt+J cycles through what needs you and wraps", () => {
  const order = [{ id: 3 }, { id: 2 }, { id: 5 }];
  assertEquals(nextInCycle(order, null)?.id, 3, "nothing focused: most urgent");
  assertEquals(nextInCycle(order, 42)?.id, 3, "focused session doesn't need you");
  assertEquals(nextInCycle(order, 3)?.id, 2);
  assertEquals(nextInCycle(order, 2)?.id, 5);
  assertEquals(nextInCycle(order, 5)?.id, 3, "wraps");
  assertEquals(nextInCycle([{ id: 7 }], 7)?.id, 7, "one item stays put");
  assertEquals(nextInCycle([], 1), null);
});

// ---------------- Jump ----------------

function cand(key: string, label: string, extra: Partial<JumpCandidate> = {}): JumpCandidate {
  return { key, label, hay: label, needs: false, ...extra };
}

Deno.test("rankJump without a query: needs you, then recent, then the given order", () => {
  const items = [
    cand("c:1", "alpha", { here: true }),
    cand("c:2", "beta"),
    cand("s:3", "gamma", { needs: true, pri: 1, since: 5 }),
    cand("s:4", "delta", { needs: true, pri: 0, since: 50 }),
    cand("s:5", "eps", { needs: true, pri: 1, since: 1 }),
    cand("f:/x", "x"),
  ];
  const out = rankJump(items, "", ["f:/x", "c:2"]).map((r) => r.item.key);
  assertEquals(out, ["s:4", "s:5", "s:3", "f:/x", "c:2", "c:1"]);
});

Deno.test("rankJump with a query: fuzzy match, boosted by needing you and recency", () => {
  const items = [
    cand("c:1", "api server"),
    cand("c:2", "api client", { needs: true }),
    cand("c:3", "docs"),
    cand("c:4", "api tests"),
  ];
  const out = rankJump(items, "api", ["c:4"]).map((r) => r.item.key);
  assertEquals(out[0], "c:2", "needs you wins a tie on text");
  assertEquals(out[1], "c:4", "then recent");
  assertEquals(out.includes("c:3"), false, "non-matches are dropped");
  const hits = rankJump(items, "api", [])[0].hits;
  assertEquals(hits, [0, 1, 2]);
});

Deno.test("rankJump highlights only within the label and respects the limit", () => {
  const items = [cand("c:1", "web", { hay: "web /home/u/web-app" })];
  assertEquals(rankJump(items, "app", [])[0].hits, [], "matched in the path part only");
  const many = Array.from({ length: 100 }, (_, i) => cand("c:" + i, "tab " + i));
  assertEquals(rankJump(many, "", [], 60).length, 60);
});

Deno.test("pushFront dedupes and caps", () => {
  assertEquals(pushFront([1, 2, 3], 2), [2, 1, 3]);
  assertEquals(pushFront([1, 2, 3], 4, 3), [4, 1, 2]);
});
