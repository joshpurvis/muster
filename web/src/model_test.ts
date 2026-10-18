import {
  aggregateState,
  basename,
  confirmAfterFocus,
  confirmAfterUpsert,
  documentTitle,
  type JumpCandidate,
  jumpOrder,
  mainSession,
  moveChannelLocally,
  needsYouOrder,
  nextInCycle,
  pushFront,
  rankJump,
  renameChanges,
  renameFields,
  renameInputs,
  renameOutlived,
  sessionsOfWindow,
  SessionVersions,
  sessionWindowId,
  tabLabel,
  tabsOf,
  tabText,
  taskLabel,
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

function sess(
  id: number,
  channel_id: number,
  state: AgentState,
  state_since = 0,
  extra: Partial<SessionInfo> = {},
): SessionInfo {
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
    ...extra,
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

// ---------------- sessions as tabs (v2) ----------------

const sids = (ss: SessionInfo[]) => ss.map((s) => s.id);

Deno.test("sessionsOfWindow: a session's own window_id decides, whatever its task's window", () => {
  const cs = [ch(1, { window_id: 1 }), ch(2, { window_id: 2 })];
  const w1 = win(1, [1]);
  const ss = [
    sess(10, 1, "idle", 0, { window_id: 1 }),
    sess(11, 1, "idle", 0, { window_id: 2 }), // pulled into window 2, its task stays in 1
    sess(20, 2, "idle", 0, { window_id: 1 }), // pulled here from window 2's task
    sess(21, 2, "idle", 0, { window_id: 2 }),
  ];
  assertEquals(sids(sessionsOfWindow(w1, ss, cs)), [10, 20]);
  assertEquals(sids(sessionsOfWindow(win(2, [2]), ss, cs)), [21, 11], "own tasks first, then others");
});

Deno.test("sessionsOfWindow without per-session windows: those of the window's tasks", () => {
  const cs = [
    ch(1, { window_id: 1 }),
    ch(2, { window_id: 2 }),
    ch(3),
    ch(4, { window_id: 1, archived: true }),
  ];
  const w1 = win(1, [3, 1]);
  const ss = [sess(10, 1, "idle"), sess(20, 2, "idle"), sess(30, 3, "idle"), sess(40, 4, "idle")];
  // Task 3 has no window_id: matched by the window's list, which also orders the tasks.
  assertEquals(sids(sessionsOfWindow(w1, ss, cs)), [30, 10]);
  // No windows at all: every session of a live task.
  assertEquals(sids(sessionsOfWindow(null, ss, cs)), [10, 20, 30]);
});

Deno.test("sessionsOfWindow orders a task's sessions by age", () => {
  const cs = [ch(1, { window_id: 1 })];
  const ss = [
    sess(12, 1, "idle", 0, { created_at: 5, window_id: 1 }),
    sess(11, 1, "idle", 0, { created_at: 9, window_id: 1 }),
  ];
  assertEquals(sids(sessionsOfWindow(win(1, [1]), ss, cs)), [12, 11]);
});

Deno.test("sessionWindowId: window_id wins, else the task's window", () => {
  const ws = [win(1, [1]), win(2, [2])];
  assertEquals(sessionWindowId(sess(1, 1, "idle", 0, { window_id: 2 }), ch(1, { window_id: 1 }), ws), 2);
  assertEquals(
    sessionWindowId(sess(1, 1, "idle", 0, { window_id: null }), ch(1, { window_id: 1 }), ws),
    null,
  );
  assertEquals(sessionWindowId(sess(1, 1, "idle"), ch(1, { window_id: 1 }), ws), 1);
  assertEquals(sessionWindowId(sess(1, 2, "idle"), ch(2), ws), 2, "task matched by the windows' lists");
  assertEquals(sessionWindowId(sess(1, 9, "idle"), undefined, ws), null);
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

Deno.test("taskLabel: human names as is; auto names muted until something was derived", () => {
  const ws = { name: "proj", path: "/home/u/proj" };
  assertEquals(taskLabel(ch(1, { name: "fix auth" }), ws), { text: "fix auth", muted: false });
  assertEquals(taskLabel(ch(1, { name: "Fix the login bug", auto_named: true }), ws), {
    text: "Fix the login bug",
    muted: false,
  });
  assertEquals(taskLabel(ch(1, { name: "proj", auto_named: true }), ws), { text: "proj", muted: true });
  // A worktree task: cwd is <data>/worktrees/<slug>/<id>, the placeholder is still the folder name.
  const wt = ch(7, { name: "proj", auto_named: true, cwd: "/data/worktrees/proj-1/7", owns_worktree: true });
  assertEquals(taskLabel(wt, ws), { text: "proj", muted: true });
  // A human may name a task like its folder: then it isn't a placeholder.
  assertEquals(taskLabel(ch(1, { name: "proj", auto_named: false }), ws), { text: "proj", muted: false });
  // Nothing at all: the folder, muted.
  assertEquals(taskLabel(ch(1, { name: "  " }), ws), { text: "proj", muted: true });
  assertEquals(taskLabel(ch(1, { name: "" }), null), { text: "proj", muted: true });
});

Deno.test("tabLabel: the task's name, plus the session's when the task has several", () => {
  const ws = { name: "proj", path: "/home/u/proj" };
  const c = ch(1, { name: "fix auth" });
  const shell = { ...sess(2, 1, "idle"), name: "shell" };
  assertEquals(tabLabel(shell, c, ws, 1), { text: "fix auth", muted: false, sub: null });
  assertEquals(tabLabel(shell, c, ws, 2), { text: "fix auth", muted: false, sub: "shell" });
  assertEquals(tabText(tabLabel(shell, c, ws, 2)), "fix auth · shell");
  assertEquals(tabText(tabLabel(shell, c, ws, 1)), "fix auth");
  // Still only the folder: muted, with the session part.
  const auto = ch(1, { name: "proj", auto_named: true });
  assertEquals(tabLabel(shell, auto, ws, 3), { text: "proj", muted: true, sub: "shell" });
  // Task unknown (its event lags): the session's own name.
  assertEquals(tabLabel(shell, undefined, ws, 1), { text: "shell", muted: false, sub: null });
});

Deno.test("renameFields: exactly the names the tab shows", () => {
  const ws = { name: "proj", path: "/home/u/proj" };
  const c = ch(1, { name: "fix auth" });
  const shell = { ...sess(2, 1, "idle"), name: "shell" };
  // One session: only the task's name, wherever.
  assertEquals(renameFields(shell, c, ws, 1, true), { task: "fix auth", session: null, focus: "task" });
  assertEquals(renameInputs(renameFields(shell, c, ws, 1, true)), [{ key: "task", value: "fix auth" }]);
  // Several: both, starting in the task's on its main tab, else in the session's.
  assertEquals(renameFields(shell, c, ws, 2, true), { task: "fix auth", session: "shell", focus: "task" });
  assertEquals(renameFields(shell, c, ws, 2, false), {
    task: "fix auth",
    session: "shell",
    focus: "session",
  });
  assertEquals(renameInputs(renameFields(shell, c, ws, 2, false)), [
    { key: "task", value: "fix auth" },
    { key: "session", value: "shell" },
  ]);
  // Still only the folder: the field starts with what the tab shows.
  const auto = ch(1, { name: "proj", auto_named: true });
  assertEquals(renameFields(shell, auto, ws, 1, true).task, "proj");
  // Nothing stored yet: the field shows the folder the tab shows; keeping it renames nothing.
  const unnamed = renameFields(shell, ch(1, { name: "" }), ws, 1, true);
  assertEquals(unnamed.task, "proj");
  assertEquals(renameChanges(unnamed, { task: "proj" }), {});
  assertEquals(renameChanges(unnamed, { task: "fix auth" }), { task: "fix auth" });
  // Task unknown: the tab shows the session's name, so that is what is renamed.
  assertEquals(renameFields(shell, undefined, ws, 1, false), {
    task: null,
    session: "shell",
    focus: "session",
  });
  assertEquals(renameInputs(renameFields(shell, undefined, ws, 1, false)), [{
    key: "session",
    value: "shell",
  }]);
});

Deno.test("renameChanges: only shown fields that changed and aren't blank", () => {
  const both = { task: "fix auth", session: "shell" };
  assertEquals(renameChanges(both, { task: "fix login", session: "shell" }), { task: "fix login" });
  assertEquals(renameChanges(both, { task: "fix auth", session: " tests " }), { session: "tests" });
  assertEquals(renameChanges(both, { task: "a", session: "b" }), { task: "a", session: "b" });
  assertEquals(renameChanges(both, { task: "  ", session: "" }), {});
  // One field blanked, the other changed: only the changed one.
  assertEquals(renameChanges(both, { task: "", session: "logs" }), { session: "logs" });
  assertEquals(renameChanges(both, { task: "fix login", session: "   " }), { task: "fix login" });
  // A field left out of the commit stays as it is.
  assertEquals(renameChanges(both, { session: "logs" }), { session: "logs" });
  assertEquals(renameChanges(both, { task: "fix auth ", session: "shell" }), {});
  // A field the tab doesn't show is never renamed.
  assertEquals(renameChanges({ task: "fix auth", session: null }, { task: "x", session: "y" }), {
    task: "x",
  });
  assertEquals(renameChanges({ task: null, session: "shell" }, { task: "x", session: "y" }), {
    session: "y",
  });
});

Deno.test("renameOutlived: once its tab leaves this window or its session or task is gone", () => {
  const r = { sid: 2, channel: 1 };
  const has = (...ids: number[]) => new Set(ids);
  assertEquals(renameOutlived(r, [1, 2], has(1, 2), has(1)), false);
  assertEquals(renameOutlived(r, [1], has(1, 2), has(1)), true); // moved to another window
  assertEquals(renameOutlived(r, [1, 2], has(1), has(1)), true); // session gone
  assertEquals(renameOutlived(r, [1, 2], has(1, 2), has()), true); // task gone
  // A rename of a tab whose task is unknown only needs its session.
  assertEquals(renameOutlived({ sid: 2, channel: null }, [2], has(2), has()), false);
});

Deno.test("mainSession: a task's first agent, else its first session", () => {
  const a = { id: 1, kind: "shell" as const };
  const b = { id: 2, kind: "codex" as const };
  const c = { id: 3, kind: "claude" as const };
  assertEquals(mainSession([a, b, c])?.id, 2);
  assertEquals(mainSession([a])?.id, 1);
  assertEquals(mainSession([]), null);
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

Deno.test("jumpOrder: this window's tabs in layout order, then other windows' by window and age", () => {
  const at = (id: number, created_at: number) => ({ id, created_at });
  const winOf = new Map([[1, 1], [2, 3], [3, 2], [4, 2], [5, 1], [6, 0]]);
  const all = [at(1, 1), at(2, 2), at(3, 5), at(4, 3), at(5, 4), at(6, 0)];
  // Window 1 shows 5 then 1 (its groups' order); 6 has no window (0 → null here).
  const out = jumpOrder(all, [5, 1], (s) => winOf.get(s.id) || null).map((s) => s.id);
  assertEquals(out, [5, 1, 4, 3, 2, 6]);
});

Deno.test("Jump ranking over tabs of all windows: needs you, then recent, then this window's order", () => {
  const order = jumpOrder(
    [{ id: 1, created_at: 0 }, { id: 2, created_at: 0 }, { id: 3, created_at: 0 }, { id: 4, created_at: 0 }],
    [2, 1],
    (s) => (s.id <= 2 ? 1 : 2),
  );
  const items = order.map((s) =>
    cand("s:" + s.id, "tab " + s.id, {
      here: s.id <= 2,
      needs: s.id === 4,
      pri: 0,
      since: 1,
      boost: 2,
    })
  );
  items.push(cand("f:/p", "p", { boost: -3 }));
  assertEquals(rankJump(items, "", ["s:3"]).map((r) => r.item.key), ["s:4", "s:3", "s:2", "s:1", "f:/p"]);
  // With a query, a tab of this window beats an equal match elsewhere.
  const two = [cand("s:9", "api", { here: false }), cand("s:8", "api", { here: true })];
  assertEquals(rankJump(two, "api", [])[0].item.key, "s:8");
});

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

Deno.test("SessionVersions: a reply is stale once an event about its session arrived", () => {
  const v = new SessionVersions();
  v.bump(1);
  const mark = v.mark();
  assertEquals(v.fresh(1, mark), true, "nothing since the call started");
  assertEquals(v.fresh(7, mark), true, "a session not seen yet (a spawn's reply)");
  v.bump(2);
  assertEquals(v.fresh(1, mark), true, "events about other sessions don't matter");
  v.bump(1);
  assertEquals(v.fresh(1, mark), false, "a later move or removal of the same session wins");
  v.bump(7);
  assertEquals(v.fresh(7, mark), false, "a new session's own event came first");
  assertEquals(v.fresh(1, v.mark()), true, "a call started after the event is fresh");
});

Deno.test("SessionVersions: a hello makes every earlier reply stale", () => {
  const v = new SessionVersions();
  const mark = v.mark();
  v.bumpAll();
  assertEquals(v.fresh(3, mark), false);
  assertEquals(v.fresh(3, v.mark()), true);
});

Deno.test("confirmAfterFocus keeps the Close? only while focus stays", () => {
  assertEquals(confirmAfterFocus(4, 4, 4), 4, "same tab (its metadata changed)");
  assertEquals(confirmAfterFocus(4, 4, 5), null, "another tab");
  assertEquals(confirmAfterFocus(4, 4, null), null, "nothing focused");
  assertEquals(confirmAfterFocus(null, 4, 4), null);
});

Deno.test("confirmAfterUpsert disarms only when that session stops being busy", () => {
  const busy = new Set<AgentState>(["working", "starting"]);
  assertEquals(confirmAfterUpsert(4, { id: 4, state: "working" }, busy), 4, "still busy (e.g. renamed)");
  assertEquals(confirmAfterUpsert(4, { id: 4, state: "idle" }, busy), null, "went idle");
  assertEquals(confirmAfterUpsert(4, { id: 5, state: "idle" }, busy), 4, "another session");
  assertEquals(confirmAfterUpsert(null, { id: 4, state: "idle" }, busy), null);
});
