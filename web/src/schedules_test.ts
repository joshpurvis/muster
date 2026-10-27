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
  segStep,
  whenToSend,
  zonedToMs,
  isExpired,
  scheduleAgents,
  schedulePatch,
  shortWhen,
  sortSchedules,
  upsertRun,
  newerRun,
  type WhenDraft,
  whenOfDraft,
  blankThen,
  blankTrigger,
  draftOfThen,
  draftOfTrigger,
  isTrigger,
  slug,
  thenOfDraft,
  watchLabel,
  whenOfTrigger,
  withSteps,
  nestedSteps,
  noteOptions,
  isProposal,
  isHeld,
  heldHeadline,
  waitingNotice,
  madeByLabel,
  proposalFacts,
  promptIsLong,
  readingWithZone,
  waitingProposals,
  thenSteps,
} from "./schedules.ts";
import type { Agent, Schedule, ScheduleRun, Then, WhenInput } from "./types.ts";

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
  // A one-off is sent as its wall-clock time; the backend reads it in the schedule's zone.
  const once = whenOfDraft({ ...base, preset: "once", once: "2026-10-21T15:30" });
  eq(once, { when: { kind: "once", local: "2026-10-21T15:30" } });
  const d = draftOfWhen({ kind: "once", at: at(2026, 10, 21, 15, 30) }, base);
  eq([d.preset, d.once], ["once", "2026-10-21T15:30"]);
  eq("error" in whenOfDraft({ ...base, preset: "once", once: "" }), true, "no date");
  eq("error" in whenOfDraft({ ...base, preset: "once", once: "2026-10-21" }), true, "no time");
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

Deno.test("a one-off's time is read and shown in the schedule's time zone, not the browser's", () => {
  // 14:30 UTC on 21 Oct 2026 is 15:30 in London (BST), 10:30 in New York (EDT), 23:30 in Tokyo.
  const t = Date.UTC(2026, 9, 21, 14, 30);
  eq(localInput(t, "Europe/London"), "2026-10-21T15:30");
  eq(localInput(t, "America/New_York"), "2026-10-21T10:30");
  eq(localInput(t, "Asia/Tokyo"), "2026-10-21T23:30");
  eq(draftOfWhen({ kind: "once", at: t }, base, "Asia/Tokyo").once, "2026-10-21T23:30", "Edit shows it in its zone");
  eq(zonedToMs("2026-10-21T15:30", "Europe/London"), t);
  eq(zonedToMs("2026-10-21T10:30", "America/New_York"), t);
  eq(zonedToMs("2026-12-21T15:30", "Europe/London"), Date.UTC(2026, 11, 21, 15, 30), "GMT in winter");
  // A time the clocks skip (01:30 on 29 Mar 2026 in London) lands after the change: 02:30 BST.
  eq(zonedToMs("2026-03-29T01:30", "Europe/London"), Date.UTC(2026, 2, 29, 1, 30));
  eq(Number.isNaN(zonedToMs("tomorrow", "Europe/London")), true);
  // An unknown zone falls back to this browser's.
  eq(localInput(at(2026, 1, 2, 3, 4), "Not/AZone"), "2026-01-02T03:04");
  // The starting value is in the chosen zone too.
  eq(defaultOnce(Date.UTC(2026, 9, 20, 9, 0), "Asia/Tokyo"), "2026-10-20T19:00");
});

Deno.test("Edit sends `when` only when a timing control changed", () => {
  const daily = draftOfWhen({ kind: "cron", cron: "@daily" }, base);
  eq(daily.preset, "daily");
  // Untouched (only the prompt changed): the stored `@daily` is not rewritten to `0 0 * * *`.
  eq(whenToSend(daily, daily, false), null);
  // Another preset is a change; switching back (or a field the preset doesn't use) is none.
  eq(whenToSend(daily, { ...daily, preset: "weekly", time: "00:00" }, false), { kind: "cron", cron: "0 0 * * 1" });
  eq(whenToSend(daily, { ...daily, preset: "daily", day: 3, minute: 30 }, false), null);
  eq(whenToSend(daily, { ...daily, time: "07:00" }, false), { kind: "cron", cron: "0 7 * * *" });
  const custom = draftOfWhen({ kind: "cron", cron: "0 9,17 * * *" }, base);
  eq(whenToSend(custom, custom, true), null, "a recurring timing doesn't need re-sending for a new zone");
  // A one-off whose zone changed is re-sent, so its wall-clock time is read in the new zone.
  const once = draftOfWhen({ kind: "once", at: at(2026, 10, 21, 15, 30) }, base);
  eq(whenToSend(once, once, false), null);
  eq(whenToSend(once, once, true), { kind: "once", local: "2026-10-21T15:30" });
  eq(whenToSend(once, { ...once, preset: "custom", cron: "" }, false), null, "invalid fields send nothing");
});

Deno.test("segmented radio groups: arrows, Home and End", () => {
  eq([segStep(0, 3, "ArrowRight"), segStep(2, 3, "ArrowRight"), segStep(0, 3, "ArrowLeft")], [1, 0, 2]);
  eq([segStep(1, 3, "ArrowDown"), segStep(1, 3, "ArrowUp")], [2, 0]);
  eq([segStep(1, 3, "Home"), segStep(1, 3, "End"), segStep(1, 3, "Enter")], [0, 2, null]);
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
  eq(newerRun(b, a)?.id, 5);
  eq(newerRun(a, b)?.id, 5);
  eq(newerRun(null, a)?.id, 3);
  // A "Then…" step never becomes the row's last run, and reads under its run in the history.
  const step = run({ id: 6, kind: "chain", parent_run: 3 });
  eq(newerRun(b, step)?.id, 5);
  eq(withSteps([step, b, a]).map((x) => `${x.run.id}:${x.depth}`), ["5:0", "3:0", "6:1"]);
  eq(withSteps([run({ id: 9, kind: "chain", parent_run: 1 })]).map((x) => x.depth), [0], "its run not loaded: it stays");
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
  // Its expiry passed between slots: expired by the panel's clock, before the backend says so.
  const soon = sched({ expires_at: NOW + 60_000, expires_soon: true });
  eq(nextLabel(soon, NOW).tone, "next");
  eq(expiresLabel(soon, NOW), "expires today");
  eq(isExpired(soon, NOW + 60_000), true);
  eq(nextLabel(soon, NOW + 60_000), { text: "expired", tone: "expired" });
  eq(expiresLabel(soon, NOW + 60_000), null, "no 'expires' line once it has");
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
    when: null as WhenInput | null,
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

// ---------------- Phase 2: triggers and "Then…" ----------------

Deno.test("trigger fields: each kind, checked", () => {
  const notes = [{ id: 4, title: "Ideas" }];
  const t = { ...blankTrigger(), note: 4 };
  eq(whenOfTrigger(t, notes), { when: { kind: "note", note_id: 4, quiet_min: 2 } }, "a note by its id");
  // Two notes of one name are told apart in the picker.
  eq(noteOptions([{ id: 1, title: "Ideas", preview: "old list", updated_at: 1 }, { id: 2, title: "ideas", preview: "", updated_at: 2 },
    { id: 3, title: "Plan", updated_at: 0 }]), [
    { id: 2, label: "ideas (note 2)" },
    { id: 1, label: "Ideas (note 1: old list)" },
    { id: 3, label: "Plan" },
  ]);
  eq("error" in whenOfTrigger({ ...t, note: 99 }, notes), true);
  eq("error" in whenOfTrigger({ ...t, quiet: "0" }, notes), true);
  eq(whenOfTrigger({ ...t, kind: "git", repo: " ~/x ", branch: "master" }, notes), {
    when: { kind: "git", repo: "~/x", branch: "master" },
  });
  eq("error" in whenOfTrigger({ ...t, kind: "git", repo: "~/x", branch: "two words" }, notes), true);
  eq(whenOfTrigger({ ...t, kind: "pr_opened", ghRepo: "https://github.com/o/r/" }, notes), {
    when: { kind: "github", repo: "o/r", event: "pr_opened", branch: null, every_min: 5 },
  });
  eq(whenOfTrigger({ ...t, kind: "ci_failed", ghRepo: "o/r", ghBranch: "master", every: "10" }, notes), {
    when: { kind: "github", repo: "o/r", event: "ci_failed", branch: "master", every_min: 10 },
  });
  eq("error" in whenOfTrigger({ ...t, kind: "pr_opened", ghRepo: "o/r", every: "1" }, notes), true, "at least every 2 minutes");
  eq(whenOfTrigger({ ...t, kind: "url", name: "Deploy-Done" }, notes), { when: { kind: "url", name: "deploy-done" } });
  eq("error" in whenOfTrigger({ ...t, kind: "url", name: "has space" }, notes), true);
  eq(slug("Deploy finished!  (prod)"), "deploy-finished-prod");
  // A stored trigger fills the fields back.
  const back = draftOfTrigger({ kind: "github", repo: "o/r", event: "ci_failed", branch: null, every_min: 7 }, blankTrigger());
  eq([back.kind, back.ghRepo, back.ghBranch, back.every], ["ci_failed", "o/r", "", "7"]);
  eq(draftOfTrigger({ kind: "note", note_id: 4, quiet_min: 3 }, blankTrigger()).note, 4);
  eq(isTrigger({ kind: "url", name: "x" }), true);
  eq(isTrigger({ kind: "cron", cron: "* * * * *" }), false);
});

Deno.test("Then… steps: drafts and what is sent", () => {
  eq(thenOfDraft(blankThen()), { then: null });
  eq(thenOfDraft({ ...blankThen(), okAgent: "rev", okPrompt: "Look: {reply}" }), {
    then: { on_success: { agent: "rev", prompt: "Look: {reply}" } },
  });
  eq("error" in thenOfDraft({ ...blankThen(), okAgent: "rev", okPrompt: " " }), true);
  eq(thenOfDraft({ ...blankThen(), failMode: "notify" }), { then: { on_failure: { notify: true } } });
  eq("error" in thenOfDraft({ ...blankThen(), failMode: "ask" }), true, "an agent to ask is needed");
  const stored = {
    on_success: { notify: false, agent: "rev", agent_id: 3, prompt: "Look: {reply}", then: null },
    on_failure: { notify: true, agent: null, agent_id: null, prompt: "", then: null },
  };
  const d = draftOfThen(stored);
  eq([d.okAgent, d.failMode], ["rev", "notify"]);
  const before = sched({ then: stored });
  const fields = {
    name: "Daily PR summary",
    prompt: "Summarise PRs",
    when: null as WhenInput | null,
    tz: "Europe/London",
    missed: "auto" as const,
    conversation: "continue" as const,
    notify: "on_failure" as const,
    note: null,
    quiet_hours: false,
    daily_cap: null,
  };
  eq(
    schedulePatch(before, {
      ...fields,
      then: { on_success: { agent: "rev", prompt: "Look: {reply}" }, on_failure: { notify: true } },
    }),
    {},
    "the same steps send nothing",
  );
  eq(schedulePatch(before, { ...fields, then: null }), { then: null });
  // A→B→C made with the API: editing B's prompt in the dialog keeps C.
  const deep = {
    on_success: {
      notify: false, agent: "b", agent_id: 2, prompt: "B {reply}",
      then: { on_success: { notify: false, agent: "c", agent_id: 3, prompt: "C {reply}", then: null }, on_failure: null },
    },
    on_failure: null,
  };
  eq(nestedSteps(deep), 1);
  const edited = thenOfDraft({ ...draftOfThen(deep), okPrompt: "B again: {reply}" }, deep);
  eq(edited, { then: { on_success: { agent: "b", prompt: "B again: {reply}", then: { on_success: { agent: "c", prompt: "C {reply}" } } } } });
  eq(schedulePatch(sched({ then: deep }), { ...fields, then: (thenOfDraft(draftOfThen(deep), deep) as { then: any }).then }), {},
    "unedited deep steps send nothing");
});

Deno.test("a trigger's row and its runs", () => {
  const trig = sched({ trigger: true, when: { kind: "url", name: "x" }, next_run: null });
  eq(nextLabel(trig, NOW), { text: "waiting", tone: "next" });
  eq(nextLabel({ ...trig, next_run: at(2026, 10, 20, 10, 2) }, NOW).text, "runs: today 10:02");
  eq(nextLabel({ ...trig, note: { id: 1, title: "Ideas", triggers: false } }, NOW).text, "triggers off for its note");
  eq(watchLabel({ ...trig, watch: { state: "gh_not_signed_in", detail: null, checked_at: 1, next_at: 2 } })?.text, "gh not signed in");
  eq(watchLabel({ ...trig, watch: { state: "ok", detail: null, checked_at: 1, next_at: 2 } }), null);
  const r = run({ kind: "chain", agent: "rev" });
  eq(runKindLabel(r), "then: rev");
  eq(runKindLabel({ ...r, kind: "triggered" }), "triggered");
  const subs = scheduleAgents([agent("a", "codex", ["human", "schedule"]), agent("b", "codex", ["human", "webhook"])], "webhook");
  eq(subs.map((c) => [c.agent.name, c.group]), [["b", "unattended"], ["a", "off"]], "triggers need the webhook origin");
});

// ---------------- Phase 3: what agents made, the files trigger ----------------

Deno.test("an agent's proposal: waits first, reads plainly, and its chip after approval", () => {
  const made = { agent_id: 2, agent: "reviewer", kind: "proposal" as const, reason: "You asked", at: NOW - 60_000, approved_at: null };
  const p = sched({ id: 7, status: "proposed", next_run: null, made_by: made, quiet_hours: true, daily_cap: 2 });
  eq(isProposal(p), true);
  eq(nextLabel(p, NOW), { text: "waiting for your approval", tone: "paused" });
  eq(madeByLabel(p), null, "the card says who proposed it");
  eq(sortSchedules([sched({ id: 1, next_run: 10 }), p]).map((s) => s.id), [7, 1], "proposals first");
  eq(waitingProposals([sched({ id: 1 }), p]).map((s) => s.id), [7]);
  const facts = proposalFacts(p);
  eq(facts.map((f) => f.label), ["When", "Agent", "Also"]);
  eq(facts[2].value, "skips quiet hours, at most 2 a day");
  const approved = sched({ made_by: { ...made, approved_at: NOW } });
  eq(isProposal(approved), false);
  eq(madeByLabel(approved)?.text, "from reviewer");
  eq(madeByLabel(sched({ made_by: { ...made, kind: "reminder" } }))?.text, "reminder");
  eq(madeByLabel(sched({})), null, "yours have no chip");
});

Deno.test("files trigger fields", () => {
  const t = { ...blankTrigger(), kind: "files" as const };
  eq("error" in whenOfTrigger(t, []), true, "a folder is needed");
  eq(whenOfTrigger({ ...t, folder: " ~/inbox ", pattern: "*.pdf", quiet: "3" }, []), {
    when: { kind: "files", folder: "~/inbox", pattern: "*.pdf", recursive: false, quiet_min: 3 },
  });
  eq(whenOfTrigger({ ...t, folder: "~/inbox", subfolders: true }, []), {
    when: { kind: "files", folder: "~/inbox", pattern: null, recursive: true, quiet_min: 2 },
  });
  eq("error" in whenOfTrigger({ ...t, folder: "~/inbox", pattern: "a/*.pdf" }, []), true);
  eq("error" in whenOfTrigger({ ...t, folder: "~/inbox", quiet: "0" }, []), true);
  const d = draftOfTrigger({ kind: "files", folder: "/home/u/inbox", pattern: "*.pdf", recursive: true, quiet_min: 5 }, blankTrigger());
  eq([d.kind, d.folder, d.pattern, d.subfolders, d.quiet], ["files", "/home/u/inbox", "*.pdf", true, "5"]);
  eq(isTrigger({ kind: "files", folder: "/x", recursive: false, quiet_min: 2 }), true);
});

Deno.test("a proposal's timing says its time zone; a row says it only when it isn't yours", () => {
  const la = sched({ tz: "America/Los_Angeles", reading: "At 08:30, Monday to Friday", when: { kind: "cron", cron: "30 8 * * 1-5" } });
  eq(readingWithZone(la, "Europe/London", true), "At 08:30, Monday to Friday (America/Los Angeles time)");
  eq(readingWithZone(la, "Europe/London"), "At 08:30, Monday to Friday (America/Los Angeles time)", "not yours: said on a row too");
  const home = { ...la, tz: "Europe/London" };
  eq(readingWithZone(home, "Europe/London"), "At 08:30, Monday to Friday", "yours: a row reads as before");
  eq(readingWithZone(home, "Europe/London", true), "At 08:30, Monday to Friday (Europe/London time)", "a proposal always says it");
  // A one-off elsewhere also says when that is here: 08:30 in Los Angeles is 16:30 in London.
  const once = sched({
    tz: "America/Los_Angeles",
    reading: "Once, Tue 27 Oct 2026 at 08:30",
    when: { kind: "once", at: Date.UTC(2026, 9, 27, 15, 30) },
  });
  eq(readingWithZone(once, "Europe/London", true), "Once, Tue 27 Oct 2026 at 08:30 (America/Los Angeles time; 15:30 your time)");
  // ...and the day when it differs (23:30 in Los Angeles is the next morning in London).
  const late = sched({ tz: "America/Los_Angeles", reading: "Once, Tue 27 Oct 2026 at 23:30", when: { kind: "once", at: Date.UTC(2026, 9, 28, 6, 30) } });
  eq(readingWithZone(late, "Europe/London", true), "Once, Tue 27 Oct 2026 at 23:30 (America/Los Angeles time; Wed 06:30 your time)");
  // Triggers have no time.
  const trig = sched({ tz: "America/Los_Angeles", reading: "When its trigger URL is called", when: { kind: "url", name: "x" } });
  eq(readingWithZone(trig, "Europe/London", true), "When its trigger URL is called");
  // The card's When line carries it.
  const made = { agent_id: 2, agent: "reviewer", kind: "proposal" as const, reason: "r", at: NOW, approved_at: null };
  eq(proposalFacts({ ...la, status: "proposed", made_by: made }, "Europe/London")[0].value, "At 08:30, Monday to Friday (America/Los Angeles time)");
});

Deno.test("a long proposal prompt folds, with the rest a click away", () => {
  eq(promptIsLong("Summarise the Ideas note"), false);
  eq(promptIsLong(Array.from({ length: 9 }, (_, i) => `line ${i}`).join("\n")), true, "more than 8 lines");
  eq(promptIsLong("x".repeat(601)), true, "more than 600 characters");
  eq(promptIsLong(Array.from({ length: 8 }, (_, i) => `line ${i}`).join("\n")), false);
});

Deno.test("a schedule made or changed outside the window waits like a proposal, and says why", () => {
  const made = sched({ id: 9, status: "held", next_run: null, updated_at: NOW - 5_000, held: { what: ["made"], at: NOW - 5_000, why: "it came from the command line" } });
  eq([isHeld(made), isProposal(made)], [true, true]);
  eq(nextLabel(made, NOW), { text: "waiting for your approval", tone: "paused" });
  eq(heldHeadline(made), "Made from the command line or a script");
  const changed = { ...made, id: 10, held: { what: ["changed" as const, "resumed" as const], at: NOW - 1_000, why: "x" } };
  eq(heldHeadline(changed), "Changed and resumed from the command line or a script");
  eq(heldHeadline({ ...made, held: { what: ["renewed"], at: 0, why: "x" } }), "Renewed for 7 days from the command line or a script");
  eq(waitingNotice([made]), "1 schedule made or changed outside Muster's window waits for your approval");
  const p = sched({ id: 7, status: "proposed", next_run: null, made_by: { agent_id: 2, agent: "reviewer", kind: "proposal", reason: "r", at: NOW - 60_000, approved_at: null } });
  eq(waitingNotice([p]), "1 schedule proposed by reviewer waits for your approval");
  eq(waitingNotice([p, made, changed]), "3 schedules wait for your approval (1 proposed by agents, 2 from the command line)");
  eq(waitingProposals([changed, sched({ id: 1 }), made, p]).map((s) => s.id), [7, 9, 10], "oldest first");
  eq(isHeld(sched({})), false);
});

Deno.test("the approval card lists every \"Then…\" step, nested ones included", () => {
  const step = (agent: string, prompt: string, then: Then | null = null) => ({ notify: false, agent, agent_id: 1, prompt, then });
  const t: Then = {
    on_success: step("reviewer", "Check {reply}", { on_success: step("deployer", "Ship it", null), on_failure: { notify: true, agent: null, agent_id: null, prompt: "", then: null } }),
    on_failure: step("fixer", "Fix it"),
  };
  eq(thenSteps(t), [
    { depth: 0, when: "When it replies", agent: "reviewer", prompt: "Check {reply}" },
    { depth: 1, when: "When that step replies", agent: "deployer", prompt: "Ship it" },
    { depth: 1, when: "If that step fails", agent: null, prompt: "" },
    { depth: 0, when: "If it fails", agent: "fixer", prompt: "Fix it" },
  ]);
  eq(thenSteps(null), []);
  eq(thenSteps({ on_success: { ...step("x", "p"), agent: null }, on_failure: null })[0].agent, "(a removed agent)");
});
