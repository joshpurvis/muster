import {
  blankWhen,
  defaultOnce,
  defaultScheduleAgent,
  draftOfWhen,
  expiresLabel,
  localInput,
  nextLabel,
  nextWhen,
  noteChoice,
  outcomeTone,
  parseCap,
  parseTime,
  runDuration,
  runKindLabel,
  runLine,
  runReason,
  sameWhen,
  scheduleAgents,
  schedulePatch,
  shortWhen,
  sortSchedules,
  upsertRun,
  newerRun,
  type WhenDraft,
  whenOfDraft,
} from "./schedules.ts";
import type { Agent, Schedule, ScheduleRun, When } from "./types.ts";

function eq<T>(a: T, b: T, msg = "") {
  const x = JSON.stringify(a), y = JSON.stringify(b);
  if (x !== y) throw new Error(`${msg}\n  got:  ${x}\n  want: ${y}`);
}

/** Local time (the helpers read and write this browser's zone). */
const at = (y: number, mo: number, d: number, h = 0, mi = 0) => new Date(y, mo - 1, d, h, mi).getTime();
// Tuesday 20 Oct 2026, 10:00.
const NOW = at(2026, 10, 20, 10, 0);
const base = blankWhen(NOW);

Deno.test("presets round-trip through their cron lines", () => {
  const cases: [Partial<WhenDraft>, string][] = [
    [{ preset: "every15" }, "*/15 * * * *"],
    [{ preset: "every30" }, "*/30 * * * *"],
    [{ preset: "hourly", minute: 5 }, "5 * * * *"],
    [{ preset: "daily", time: "07:30" }, "30 7 * * *"],
    [{ preset: "weekdays", time: "09:00" }, "0 9 * * 1-5"],
    [{ preset: "weekly", time: "17:45", day: 5 }, "45 17 * * 5"],
    [{ preset: "weekly", time: "08:00", day: 0 }, "0 8 * * 0"],
  ];
  for (const [d, cron] of cases) {
    const w = whenOfDraft({ ...base, ...d });
    eq(w, { when: { kind: "cron", cron } }, `${d.preset} -> ${cron}`);
    const back = draftOfWhen({ kind: "cron", cron }, base);
    eq(back.preset, d.preset, `${cron} -> ${d.preset}`);
    eq(whenOfDraft(back), w, `${cron} round trip`);
  }
});

Deno.test("cron lines that aren't a preset's shape stay Custom, as typed", () => {
  for (const cron of ["0 9,17 * * *", "0 */6 * * *", "0 9 1 * *", "0 9 * * 1,3,5", "*/20 * * * *", "0 9 * 1 1-5"]) {
    const d = draftOfWhen({ kind: "cron", cron }, base);
    eq(d.preset, "custom", cron);
    eq(d.cron, cron);
    eq(whenOfDraft(d), { when: { kind: "cron", cron } });
  }
  // Extra spaces don't matter; Sunday as 7 is Sunday; macros map to their preset.
  eq(draftOfWhen({ kind: "cron", cron: " 0  9 * *   1-5 " }, base).preset, "weekdays");
  eq(draftOfWhen({ kind: "cron", cron: "0 8 * * 7" }, base).day, 0);
  eq(draftOfWhen({ kind: "cron", cron: "@hourly" }, base).preset, "hourly");
  eq(draftOfWhen({ kind: "cron", cron: "@daily" }, base).time, "00:00");
  // Fields the timing doesn't set keep the base's values (switching presets loses nothing).
  eq(draftOfWhen({ kind: "cron", cron: "*/15 * * * *" }, base).time, "09:00");
});

Deno.test("one-offs and bad fields", () => {
  const once = whenOfDraft({ ...base, preset: "once", once: "2026-10-21T15:30" });
  eq(once, { when: { kind: "once", at: at(2026, 10, 21, 15, 30) } });
  const d = draftOfWhen({ kind: "once", at: at(2026, 10, 21, 15, 30) }, base);
  eq([d.preset, d.once], ["once", "2026-10-21T15:30"]);
  eq("error" in whenOfDraft({ ...base, preset: "once", once: "" }), true, "no date");
  eq("error" in whenOfDraft({ ...base, preset: "daily", time: "25:00" }), true, "bad hour");
  eq("error" in whenOfDraft({ ...base, preset: "daily", time: "9am" }), true, "not HH:MM");
  eq("error" in whenOfDraft({ ...base, preset: "custom", cron: "  " }), true, "empty cron");
  eq(parseTime("9:05"), [9, 5]);
  eq(parseTime("23:59"), [23, 59]);
  eq(parseTime("24:00"), null);
  eq(localInput(at(2026, 1, 2, 3, 4)), "2026-01-02T03:04");
  // At least half an hour ahead, on the hour.
  eq(defaultOnce(at(2026, 10, 20, 10, 0)), "2026-10-20T11:00");
  eq(defaultOnce(at(2026, 10, 20, 10, 40)), "2026-10-20T12:00");
});

Deno.test("sameWhen ignores spacing and tells kinds apart", () => {
  const c = (cron: string): When => ({ kind: "cron", cron });
  eq(sameWhen(c("0 9 * * 1-5"), c("0  9 * * 1-5")), true);
  eq(sameWhen(c("0 9 * * 1-5"), c("0 9 * * *")), false);
  eq(sameWhen({ kind: "once", at: 5 }, { kind: "once", at: 5 }), true);
  eq(sameWhen({ kind: "once", at: 5 }, c("0 9 * * *")), false);
});

const agent = (name: string, kind: Agent["kind"], accepts: Agent["accepts_origins"], provider?: Agent["provider"]): Agent => ({
  id: name.length,
  name,
  kind,
  provider,
  model: null,
  effort: null,
  persona: "",
  permissions: { codex_sandbox: "workspace_write", claude_mode: "default" },
  accepts_origins: accepts,
  autostart: false,
  home_dir: "/tmp/" + name,
  channel_id: null,
  session_id: null,
  resumable: false,
  created_at: 0,
});

Deno.test("the dialog lists Codex and OpenRouter agents first and never picks a Claude subscription agent", () => {
  const list = [
    agent("alpha", "claude", ["human", "schedule"]),
    agent("orchestrator", "claude", ["human"]),
    agent("worker", "codex", ["human", "schedule", "webhook"]),
    agent("cheap", "claude", ["human", "schedule"], "openrouter"),
    agent("strict", "codex", ["human"]),
  ];
  const c = scheduleAgents(list);
  eq(c.map((x) => `${x.agent.name}:${x.group}`), [
    "cheap:unattended",
    "worker:unattended",
    "alpha:claude",
    "orchestrator:off",
    "strict:off",
  ]);
  eq(defaultScheduleAgent(c), "cheap");
  eq(defaultScheduleAgent(c, "worker"), "worker", "the agent asked for");
  eq(defaultScheduleAgent(c, "alpha"), "alpha", "a Claude agent you picked yourself");
  eq(defaultScheduleAgent(c, "orchestrator"), "cheap", "never one that doesn't take scheduled work");
  eq(defaultScheduleAgent(scheduleAgents([list[0], list[1]])), "", "only Claude subscription agents: nothing preselected");
});

const run = (p: Partial<ScheduleRun>): ScheduleRun => ({
  id: 1,
  schedule_id: 1,
  slot: at(2026, 10, 19, 9, 0),
  kind: "scheduled",
  started_at: at(2026, 10, 19, 9, 0),
  finished_at: at(2026, 10, 19, 9, 4),
  outcome: "replied",
  late_reply: false,
  reason: null,
  msg_id: null,
  message: null,
  ...p,
});

Deno.test("run lines: when, outcome, how long", () => {
  eq(runLine(run({}), NOW), "Mon 09:00 · replied · 4m");
  eq(runLine(run({ outcome: "skipped" }), NOW), "Mon 09:00 · skipped", "a skip has no duration");
  eq(runLine(run({ outcome: "no_reply", finished_at: null }), NOW), "Mon 09:00 · no reply");
  eq(runLine(run({ slot: at(2026, 10, 3, 9, 0) }), NOW), "3 Oct 09:00 · replied · 4m", "over a week ago: the date");
  const s = (ms: number) => runDuration(run({ finished_at: at(2026, 10, 19, 9, 0) + ms }));
  eq([s(35_000), s(4 * 60_000), s(65 * 60_000), s(120 * 60_000)], ["35s", "4m", "1h 5m", "2h"]);
  eq(runKindLabel(run({ kind: "catch_up" })), "catch-up");
  eq(runKindLabel(run({ kind: "manual" })), "run now");
  eq(runKindLabel(run({})), null);
  eq(runReason(run({ kind: "catch_up", reason: "catch-up: missed while Muster was off or asleep" })), "missed while Muster was off or asleep");
  eq(runReason(run({ kind: "catch_up", reason: "catch-up:" })), null, "nothing left besides the chip");
  eq(runReason(run({ outcome: "skipped", reason: "busy: the agent was busy for an hour" })), "busy: the agent was busy for an hour");
  eq(runReason(run({})), null);
  eq(
    (["replied", "skipped", "pending", "blocked", "refused", "timed_out", "failed"] as const).map(outcomeTone),
    ["ok", "skip", "wait", "bad", "bad", "bad", "bad"],
  );
});

Deno.test("history lists merge runs newest first; the newer last run wins", () => {
  const a = run({ id: 3 }), b = run({ id: 5 }), c = run({ id: 4 });
  eq(upsertRun(upsertRun(upsertRun([], a), b), c).map((r) => r.id), [5, 4, 3]);
  eq(upsertRun([b, a], { ...a, outcome: "failed" }).map((r) => `${r.id}${r.outcome}`), ["5replied", "3failed"]);
  eq(newerRun(b, a).id, 5);
  eq(newerRun(a, b).id, 5);
  eq(newerRun(null, a).id, 3);
});

const sched = (p: Partial<Schedule>): Schedule => ({
  id: 1,
  agent_id: 1,
  agent: "worker",
  name: "Daily PR summary",
  prompt: "Summarise PRs",
  when: { kind: "cron", cron: "0 9 * * 1-5" },
  tz: "Europe/London",
  reading: "At 09:00, Monday to Friday",
  missed: "auto",
  missed_effective: "run_once",
  conversation: "continue",
  notify: "on_failure",
  note_id: null,
  note_title: null,
  quiet_hours: false,
  daily_cap: null,
  paused: false,
  skip_next: false,
  next_run: at(2026, 10, 21, 9, 0),
  last_slot: null,
  status: "active",
  expires_at: null,
  expires_soon: false,
  created_at: 0,
  updated_at: 0,
  last_run: null,
  ...p,
});

Deno.test("a row's next-run line", () => {
  eq(nextLabel(sched({}), NOW), { text: "next: tomorrow 09:00", tone: "next" });
  eq(nextLabel(sched({ next_run: at(2026, 10, 20, 14, 0) }), NOW).text, "next: today 14:00");
  eq(nextLabel(sched({ next_run: at(2026, 10, 23, 9, 0) }), NOW).text, "next: Fri 09:00");
  eq(nextLabel(sched({ next_run: at(2026, 11, 3, 9, 0) }), NOW).text, "next: 3 Nov 09:00");
  eq(nextLabel(sched({ skip_next: true }), NOW).text, "skips next: tomorrow 09:00");
  eq(nextLabel(sched({ paused: true, status: "paused" }), NOW).tone, "paused");
  eq(nextLabel(sched({ status: "expired" }), NOW).text, "expired");
  eq(nextLabel(sched({ when: { kind: "once", at: 1 }, next_run: null, status: "done" }), NOW).text, "done");
  eq(nextWhen(at(2026, 10, 21, 0, 5), NOW), "tomorrow 00:05");
  eq(shortWhen(at(2026, 10, 25, 9, 0), NOW), "Sun 09:00");
});

Deno.test("expiry hint for Claude subscription agents' schedules", () => {
  eq(expiresLabel(sched({}), NOW), null);
  eq(expiresLabel(sched({ expires_at: at(2026, 10, 21, 8, 0), expires_soon: true }), NOW), "expires tomorrow");
  eq(expiresLabel(sched({ expires_at: at(2026, 10, 20, 22, 0), expires_soon: true }), NOW), "expires today");
  eq(expiresLabel(sched({ expires_at: at(2026, 10, 21, 8, 0), expires_soon: false }), NOW), null);
  eq(expiresLabel(sched({ expires_at: at(2026, 10, 19, 8, 0), expires_soon: true, status: "expired" }), NOW), null);
});

Deno.test("sorting: soonest first, finished last", () => {
  const list = [
    sched({ id: 1, name: "b", next_run: null }),
    sched({ id: 2, name: "c", next_run: 50 }),
    sched({ id: 3, name: "a", next_run: 10 }),
  ];
  eq(sortSchedules(list).map((s) => s.id), [3, 2, 1]);
});

Deno.test("the daily cap and the reply note", () => {
  eq(parseCap(""), { cap: null });
  eq(parseCap(" 12 "), { cap: 12 });
  eq("error" in parseCap("0"), true);
  eq("error" in parseCap("201"), true);
  eq("error" in parseCap("3.5"), true);
  const notes = [{ id: 7, title: "PR summaries" }];
  eq(noteChoice(false, "PR summaries", notes), null);
  eq(noteChoice(true, "  ", notes), null);
  eq(noteChoice(true, "pr SUMMARIES", notes), { id: 7 }, "an existing note, any case");
  eq(noteChoice(true, "Nightly checks", notes), { title: "Nightly checks" });
});

Deno.test("Save sends only what changed", () => {
  const before = sched({ note_id: 7, note_title: "PR summaries", daily_cap: 4 });
  const same = {
    name: "Daily PR summary",
    prompt: "Summarise PRs",
    when: { kind: "cron", cron: "0  9 * * 1-5" } as When,
    tz: "Europe/London",
    missed: "auto" as const,
    conversation: "continue" as const,
    notify: "on_failure" as const,
    note: { id: 7 },
    quiet_hours: false,
    daily_cap: 4,
  };
  eq(schedulePatch(before, same), {});
  eq(schedulePatch(before, { ...same, when: { kind: "cron", cron: "0 10 * * 1-5" } }), {
    when: { kind: "cron", cron: "0 10 * * 1-5" },
  });
  eq(schedulePatch(before, { ...same, note: null, daily_cap: null }), { daily_cap: null, note: null });
  eq(schedulePatch(before, { ...same, note: { title: "pr summaries" } }), {}, "same note by title");
  eq(schedulePatch(before, { ...same, note: { title: "Other" } }), { note: { title: "Other" } });
  eq(schedulePatch(before, { ...same, name: "  ", conversation: "fresh" }), { name: "", conversation: "fresh" });
  eq(schedulePatch(before, { ...same, tz: " " }), {}, "an emptied zone field keeps the zone");
});
