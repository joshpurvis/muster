import { cardKey, failedRun, hideRun, needsYouItems, otherName, ownerOf, rowChip, sidebarRows } from "./sidebar.ts";
import type { Agent, Channel, RunOutcome, Schedule, ScheduleRun } from "./types.ts";

function equal<T>(a: T, b: T) {
  const x = JSON.stringify(a), y = JSON.stringify(b);
  if (x !== y) throw new Error(`got:  ${x}\nwant: ${y}`);
}

const ch = (id: number, name: string): Channel => ({
  id, workspace_id: 1, name, cwd: "/w", branch: null, base_branch: null, owns_worktree: false, port_base: 0,
  archived: false, created_at: 0, last_read_at: 0, sort_order: id,
});
const ag = (id: number, name: string, channel_id: number | null): Agent => ({ id, name, channel_id } as unknown as Agent);
const run = (id: number, outcome: RunOutcome, started_at = id): ScheduleRun =>
  ({ id, outcome, started_at } as unknown as ScheduleRun);
const sch = (id: number, agent_id: number, extra: Partial<Schedule> = {}): Schedule =>
  ({ id, agent_id, name: `s${id}`, status: "active", last_run: null, updated_at: id, made_by: null, ...extra } as Schedule);
const proposal = (id: number, agent_id: number, at = id) =>
  sch(id, agent_id, { status: "proposed", made_by: { agent: "alpha", at, kind: "proposal", approved_at: null } as any });

Deno.test("each channel row carries the agent that owns it; agents without a channel here follow", () => {
  const rows = sidebarRows(
    [ch(10, "planning"), ch(11, "notes-task"), ch(12, "beta")],
    [ag(2, "beta", 12), ag(1, "alpha", 10), ag(3, "gamma", null), ag(4, "delta", 99)],
  );
  equal(rows.map((r) => [r.key, r.channel?.name ?? null, r.agent?.name ?? null]), [
    ["c10", "planning", "alpha"],
    ["c11", "notes-task", null],
    ["c12", "beta", "beta"],
    // Not started yet, or its channel is in another window: still a row, by name.
    ["a4", null, "delta"],
    ["a3", null, "gamma"],
  ]);
});

Deno.test("two agents claiming one channel: the older keeps the row, the other still gets one", () => {
  equal(ownerOf([ag(5, "b", 10), ag(2, "a", 10)]).get(10)?.name, "a");
  const rows = sidebarRows([ch(10, "x")], [ag(5, "b", 10), ag(2, "a", 10)]);
  equal(rows.map((r) => r.agent?.name), ["a", "b"]);
});

Deno.test("both names show only where the agent's and its channel's names differ", () => {
  equal(otherName(ch(10, "planning"), ag(1, "alpha", 10)), "alpha");
  equal(otherName(ch(10, "alpha"), ag(1, "alpha", 10)), null);
  equal(otherName(ch(10, "planning"), null), null);
  equal(otherName(null, ag(1, "alpha", null)), null);
});

Deno.test("Needs you holds proposals oldest first, then failed runs newest first; hidden runs leave", () => {
  const list = [
    sch(1, 1, { last_run: run(100, "replied") }),
    sch(2, 1, { last_run: run(101, "blocked", 500) }),
    proposal(3, 2, 900),
    sch(4, 2, { last_run: run(102, "failed", 700), paused: true }),
    proposal(5, 1, 100),
    sch(6, 2, { last_run: run(103, "skipped") }),
    sch(7, 2, { last_run: run(104, "pending") }),
  ];
  equal(needsYouItems(list, {}).map((i) => [i.kind, i.schedule.id]), [
    ["proposal", 5], ["proposal", 3], ["run", 4], ["run", 2],
  ]);
  equal(needsYouItems(list, { 4: 102 }).map((i) => i.schedule.id), [5, 3, 2]);
  equal(needsYouItems([sch(1, 1)], {}), []);
  // A newer failed run of a hidden schedule shows again; hiding one schedule's run hides no other's.
  equal(failedRun(sch(2, 1, { last_run: run(105, "timed_out") }), { 2: 101 }), true);
  equal(failedRun(sch(2, 1, { last_run: run(102, "failed") }), { 4: 102 }), true);
});

Deno.test("an agent's row chip: red for a failed run, amber for a proposal, else its schedule count", () => {
  const list = [proposal(1, 1), sch(2, 1, { last_run: run(10, "blocked") }), sch(3, 2), sch(4, 2), proposal(5, 3)];
  equal(rowChip(1, list, {}), {
    tone: "bad", count: 2, label: "1 schedule waits for your approval; 1 schedule's last run failed",
  });
  equal(rowChip(1, list, { 2: 10 }), { tone: "wait", count: 1, label: "1 schedule waits for your approval" });
  equal(rowChip(2, list, {}), { tone: "plain", count: 2, label: "2 schedules" });
  equal(rowChip(3, list, {})?.tone, "wait");
  equal(rowChip(9, list, {}), null);
});

Deno.test("hiding a run is remembered per schedule; schedules that are gone are dropped", () => {
  equal(hideRun({}, 7, 70, [7]), { 7: 70 });
  // A newer failure of the same schedule replaces the old entry; a removed schedule's entry goes.
  equal(hideRun({ 7: 70, 8: 80, 9: 90 }, 7, 71, [7, 8]), { 7: 71, 8: 80 });
  // However many are hidden, one stays hidden until its schedule runs again.
  const many = Object.fromEntries(Array.from({ length: 300 }, (_, i) => [i, i]));
  equal(hideRun(many, 500, 1, [...Array(301).keys(), 500])[0], 0);
});

Deno.test("keys: → opens a focused row's card; Esc closes it, ← too from one of its buttons", () => {
  equal(cardKey("ArrowRight", "row"), "open");
  equal(cardKey("ArrowLeft", "row"), null);
  equal(cardKey("Enter", "row"), null);
  equal(cardKey("Escape", "card"), "close");
  equal(cardKey("ArrowLeft", "card"), "close");
  // In the prompt box ← scrolls the text instead.
  equal(cardKey("ArrowLeft", "card", false), null);
  equal(cardKey("ArrowRight", "card"), null);
});
