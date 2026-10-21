// Pure logic for schedules (docs/UI.md "Agents panel", docs/PROTOCOL.md "Schedules"): the New /
// Edit schedule dialog's timing presets and agent order, and how rows and run history read. No
// signals, no DOM: unit tested (schedules_test.ts). Cron lines are read by the backend
// (`schedule.preview`); here they are only matched against the presets' shapes.

import type { Agent, RunOutcome, Schedule, ScheduleRun, Then, ThenInput, When, WhenInput } from "./types.ts";

// ---------------- timing presets ----------------

export type Preset = "every15" | "every30" | "hourly" | "daily" | "weekdays" | "weekly" | "once" | "custom";

export const PRESETS: { id: Preset; label: string }[] = [
  { id: "every15", label: "Every 15 minutes" },
  { id: "every30", label: "Every 30 minutes" },
  { id: "hourly", label: "Every hour" },
  { id: "daily", label: "Every day" },
  { id: "weekdays", label: "Weekdays (Monday to Friday)" },
  { id: "weekly", label: "Once a week" },
  { id: "once", label: "Once, at a date and time" },
  { id: "custom", label: "Custom (a cron line)" },
];

/** Sunday first, as cron numbers them (0-6). */
export const DAY_NAMES = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const DAY_SHORT = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_SHORT = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** What the When fields hold. Every preset's own fields are kept while another is picked, so
 * switching back and forth loses nothing. */
export interface WhenDraft {
  preset: Preset;
  /** "HH:MM": Every day, Weekdays, Once a week. */
  time: string;
  /** 0-59: Every hour (at :MM). */
  minute: number;
  /** 0-6, Sunday first: Once a week. */
  day: number;
  /** A datetime-local value, "YYYY-MM-DDTHH:MM", a wall-clock time in the schedule's time zone
   * (the backend reads it there): Once. */
  once: string;
  /** Custom. */
  cron: string;
}

const pad = (n: number) => String(n).padStart(2, "0");

/** Year, month (1-12), day, hour, minute of `ms` in zone `tz` (this browser's when `tz` is empty
 * or not a zone this browser knows). */
function partsIn(ms: number, tz: string): [number, number, number, number, number] {
  if (tz.trim()) {
    try {
      const f = new Intl.DateTimeFormat("en-US", {
        timeZone: tz.trim(),
        hourCycle: "h23",
        year: "numeric",
        month: "numeric",
        day: "numeric",
        hour: "numeric",
        minute: "numeric",
      });
      const p = Object.fromEntries(f.formatToParts(new Date(ms)).map((x) => [x.type, x.value]));
      return [Number(p.year), Number(p.month), Number(p.day), Number(p.hour) % 24, Number(p.minute)];
    } catch { /* an unknown zone: this browser's */ }
  }
  const d = new Date(ms);
  return [d.getFullYear(), d.getMonth() + 1, d.getDate(), d.getHours(), d.getMinutes()];
}

/** A datetime-local value for `ms`: its wall-clock time in zone `tz` (default: this browser's). */
export function localInput(ms: number, tz = ""): string {
  const [y, mo, d, h, mi] = partsIn(ms, tz);
  return `${y}-${pad(mo)}-${pad(d)}T${pad(h)}:${pad(mi)}`;
}

/** The instant a wall-clock `local` ("YYYY-MM-DDTHH:MM") is in zone `tz`; NaN when it isn't one.
 * A time the clocks skip lands after the change. (The backend does this for real; the mock and
 * tests use it.) */
export function zonedToMs(local: string, tz: string): number {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(local);
  if (!m) return NaN;
  const wall = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5]);
  const offset = (ms: number) => {
    const [y, mo, d, h, mi] = partsIn(ms, tz);
    return Date.UTC(y, mo - 1, d, h, mi) - Math.floor(ms / 60_000) * 60_000;
  };
  let ms = wall - offset(wall);
  ms = wall - offset(ms);
  return ms;
}

/** The whole hour at least half an hour ahead of an hour from now, in zone `tz`: a one-off's
 * starting value. */
export function defaultOnce(now: number, tz = ""): string {
  return localInput(now + 90 * 60_000, tz).slice(0, 14) + "00";
}

export function blankWhen(now: number, tz = ""): WhenDraft {
  return { preset: "weekdays", time: "09:00", minute: 0, day: 1, once: defaultOnce(now, tz), cron: "0 9 * * 1-5" };
}

/** "HH:MM" (or "H:MM") to [hour, minute]; null when it isn't a time of day. */
export function parseTime(t: string): [number, number] | null {
  const m = /^\s*(\d{1,2}):(\d{2})\s*$/.exec(t);
  if (!m) return null;
  const h = Number(m[1]), min = Number(m[2]);
  return h < 24 && min < 60 ? [h, min] : null;
}

const num = (s: string, max: number): number | null => {
  if (!/^\d{1,2}$/.test(s)) return null;
  const n = Number(s);
  return n <= max ? n : null;
};

/** The dialog's fields for a stored timing: the preset whose cron shape it has, else Custom (a
 * one-off is Once, shown at its wall-clock time in the schedule's zone `tz`). Fields the timing
 * doesn't set keep `base`'s values. */
export function draftOfWhen(when: When, base: WhenDraft, tz = ""): WhenDraft {
  if (when.kind === "once") return { ...base, preset: "once", once: localInput(when.at, tz) };
  if (when.kind !== "cron") return base;
  const cron = when.cron.trim().replace(/\s+/g, " ");
  const d: WhenDraft = { ...base, preset: "custom", cron };
  if (cron === "*/15 * * * *") return { ...d, preset: "every15" };
  if (cron === "*/30 * * * *") return { ...d, preset: "every30" };
  if (cron === "@hourly") return { ...d, preset: "hourly", minute: 0 };
  if (cron === "@daily" || cron === "@midnight") return { ...d, preset: "daily", time: "00:00" };
  const f = cron.split(" ");
  if (f.length !== 5 || f[2] !== "*" || f[3] !== "*") return d;
  const minute = num(f[0], 59);
  if (minute === null) return d;
  if (f[1] === "*" && f[4] === "*") return { ...d, preset: "hourly", minute };
  const hour = num(f[1], 23);
  if (hour === null) return d;
  const time = `${pad(hour)}:${pad(minute)}`;
  if (f[4] === "*") return { ...d, preset: "daily", time };
  if (f[4] === "1-5") return { ...d, preset: "weekdays", time };
  const day = num(f[4], 7);
  if (day !== null) return { ...d, preset: "weekly", time, day: day % 7 };
  return d;
}

/** The timing the fields describe, or what is wrong with them. A one-off is sent as its
 * wall-clock time (`local`), which the backend reads in the schedule's time zone. */
export function whenOfDraft(d: WhenDraft): { when: WhenInput } | { error: string } {
  const cron = (c: string): { when: WhenInput } => ({ when: { kind: "cron", cron: c } });
  switch (d.preset) {
    case "every15":
      return cron("*/15 * * * *");
    case "every30":
      return cron("*/30 * * * *");
    case "hourly":
      if (!Number.isInteger(d.minute) || d.minute < 0 || d.minute > 59) return { error: "A minute from 0 to 59" };
      return cron(`${d.minute} * * * *`);
    case "daily":
    case "weekdays":
    case "weekly": {
      const t = parseTime(d.time);
      if (!t) return { error: "A time of day, like 09:00" };
      const days = d.preset === "daily" ? "*" : d.preset === "weekdays" ? "1-5" : String(d.day);
      return cron(`${t[1]} ${t[0]} * * ${days}`);
    }
    case "once": {
      if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(d.once)) return { error: "Pick a date and time" };
      return { when: { kind: "once", local: d.once } };
    }
    case "custom": {
      const c = d.cron.trim().replace(/\s+/g, " ");
      if (!c) return { error: "Type a cron line: minute, hour, day of month, month, day of week" };
      return cron(c);
    }
  }
}

/** What Save sends as `when` when editing: nothing unless a timing control changed what the
 * fields describe (a stored `@daily` shown as "Every day at 00:00" is left as it is), or a
 * one-off's time zone changed (its wall-clock time is then read in the new zone). */
export function whenToSend(initial: WhenDraft, cur: WhenDraft, tzChanged: boolean): WhenInput | null {
  const now = whenOfDraft(cur);
  if (!("when" in now)) return null;
  const was = whenOfDraft(initial);
  const changed = !("when" in was) || JSON.stringify(was.when) !== JSON.stringify(now.when);
  return changed || (tzChanged && now.when.kind === "once") ? now.when : null;
}

// ---------------- triggers: "When something happens" (Phase 2) ----------------

export type TriggerKind = "note" | "git" | "pr_opened" | "ci_failed" | "url";

export const TRIGGER_KINDS: { id: TriggerKind; label: string }[] = [
  { id: "note", label: "A note changes" },
  { id: "git", label: "A branch moves in a git repository" },
  { id: "pr_opened", label: "GitHub: a pull request is opened" },
  { id: "ci_failed", label: "GitHub: CI fails" },
  { id: "url", label: "Its trigger URL is called (scripts, muster trigger)" },
];

/** What the "When something happens" fields hold (each kind's fields are kept while another is
 * picked). */
export interface TriggerDraft {
  kind: TriggerKind;
  /** A note's title. */
  note: string;
  /** Minutes of quiet after the last edit. */
  quiet: string;
  repo: string;
  branch: string;
  /** GitHub OWNER/NAME. */
  ghRepo: string;
  /** CI on this branch only (empty: any). */
  ghBranch: string;
  /** Minutes between GitHub checks. */
  every: string;
  /** A URL trigger's name. */
  name: string;
}

export function blankTrigger(): TriggerDraft {
  return { kind: "note", note: "", quiet: "2", repo: "", branch: "master", ghRepo: "", ghBranch: "", every: "5", name: "" };
}

export function isTrigger(w: When | WhenInput): boolean {
  return w.kind !== "cron" && w.kind !== "once";
}

/** The dialog's fields for a stored trigger. */
export function draftOfTrigger(w: When, base: TriggerDraft, noteTitle: (id: number) => string): TriggerDraft {
  switch (w.kind) {
    case "note":
      return { ...base, kind: "note", note: noteTitle(w.note_id), quiet: String(w.quiet_min) };
    case "git":
      return { ...base, kind: "git", repo: w.repo, branch: w.branch };
    case "github":
      return { ...base, kind: w.event, ghRepo: w.repo, ghBranch: w.branch ?? "", every: String(w.every_min) };
    case "url":
      return { ...base, kind: "url", name: w.name };
    default:
      return base;
  }
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,39}$/;
const REPO_RE = /^[A-Za-z0-9_.-]{1,100}\/[A-Za-z0-9_.-]{1,100}$/;

/** "a few words" -> "a-few-words": a URL trigger's name from its schedule's name. */
export function slug(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40).replace(/-+$/, "");
}

/** The trigger the fields describe, or what is wrong with them. A note is found by its title. */
export function whenOfTrigger(d: TriggerDraft, notes: { id: number; title: string }[]): { when: WhenInput } | { error: string } {
  const int = (s: string, lo: number, hi: number) => {
    const n = /^\d+$/.test(s.trim()) ? Number(s.trim()) : NaN;
    return n >= lo && n <= hi ? n : null;
  };
  switch (d.kind) {
    case "note": {
      const t = d.note.trim();
      if (!t) return { error: "Pick the note to watch" };
      const n = notes.find((x) => x.title.trim().toLowerCase() === t.toLowerCase());
      if (!n) return { error: `No note named ${t}` };
      const quiet = int(d.quiet, 1, 120);
      if (quiet === null) return { error: "Wait 1 to 120 minutes of quiet" };
      return { when: { kind: "note", note_id: n.id, quiet_min: quiet } };
    }
    case "git": {
      if (!d.repo.trim()) return { error: "The repository's folder, like ~/projects/muster" };
      if (!d.branch.trim() || /\s/.test(d.branch.trim())) return { error: "A branch name, like master" };
      return { when: { kind: "git", repo: d.repo.trim(), branch: d.branch.trim() } };
    }
    case "pr_opened":
    case "ci_failed": {
      const repo = d.ghRepo.trim().replace(/^https:\/\/github\.com\//, "").replace(/\/$/, "");
      if (!REPO_RE.test(repo)) return { error: "The repository as OWNER/NAME, like octocat/hello-world" };
      const every = int(d.every, 2, 60);
      if (every === null) return { error: "Check every 2 to 60 minutes" };
      const branch = d.kind === "ci_failed" && d.ghBranch.trim() ? d.ghBranch.trim() : null;
      return { when: { kind: "github", repo, event: d.kind, branch, every_min: every } };
    }
    case "url": {
      const n = d.name.trim().toLowerCase();
      if (!NAME_RE.test(n)) return { error: "A name of a-z, 0-9, - and _ (it goes in the URL)" };
      return { when: { kind: "url", name: n } };
    }
  }
}

// ---------------- "Then…" steps ----------------

export interface ThenDraft {
  /** When it replies: ask this agent (empty: nothing). */
  okAgent: string;
  okPrompt: string;
  /** When it fails: nothing, notify, or ask an agent. */
  failMode: "none" | "notify" | "ask";
  failAgent: string;
  failPrompt: string;
}

export function blankThen(): ThenDraft {
  return { okAgent: "", okPrompt: "", failMode: "none", failAgent: "", failPrompt: "" };
}

export function draftOfThen(t: Then | null | undefined): ThenDraft {
  const d = blankThen();
  if (!t) return d;
  if (t.on_success && !t.on_success.notify) {
    d.okAgent = t.on_success.agent ?? "";
    d.okPrompt = t.on_success.prompt;
  }
  if (t.on_failure) {
    if (t.on_failure.notify) d.failMode = "notify";
    else {
      d.failMode = "ask";
      d.failAgent = t.on_failure.agent ?? "";
      d.failPrompt = t.on_failure.prompt;
    }
  }
  return d;
}

/** The steps the fields describe (null: none), or what is wrong with them. */
export function thenOfDraft(d: ThenDraft): { then: ThenInput | null } | { error: string } {
  const out: ThenInput = {};
  if (d.okAgent) {
    if (!d.okPrompt.trim()) return { error: "Write what to ask when it replies" };
    out.on_success = { agent: d.okAgent, prompt: d.okPrompt };
  }
  if (d.failMode === "notify") out.on_failure = { notify: true };
  if (d.failMode === "ask") {
    if (!d.failAgent) return { error: "Pick the agent to ask when it fails" };
    if (!d.failPrompt.trim()) return { error: "Write what to ask when it fails" };
    out.on_failure = { agent: d.failAgent, prompt: d.failPrompt };
  }
  return { then: out.on_success || out.on_failure ? out : null };
}

/** The steps as stored, in the shape `thenOfDraft` gives (to tell whether Save changes them). */
export function thenInputOf(t: Then | null | undefined): ThenInput | null {
  const r = thenOfDraft(draftOfThen(t));
  return "then" in r ? r.then : null;
}

/** A trigger's watcher, as its row shows it: "gh isn't signed in", a git error, or null. */
export function watchLabel(s: Schedule): { text: string; tip: string } | null {
  const w = s.watch;
  if (!w) return null;
  if (w.state === "gh_not_signed_in") return { text: "gh not signed in", tip: "Run gh auth login in a terminal; Muster checks again by itself." };
  if (w.state === "gh_missing") return { text: "gh not installed", tip: w.detail ?? "Install the GitHub CLI (cli.github.com)." };
  if (w.state === "error") return { text: "can't check", tip: w.detail ?? "" };
  return null;
}

/** Arrow keys, Home and End in a radio group: the option they move to, or null for other keys. */
export function segStep(i: number, n: number, key: string): number | null {
  if (key === "ArrowRight" || key === "ArrowDown") return (i + 1) % n;
  if (key === "ArrowLeft" || key === "ArrowUp") return (i - 1 + n) % n;
  if (key === "Home") return 0;
  if (key === "End") return n - 1;
  return null;
}

// ---------------- which agents can take scheduled work ----------------

/** A Claude agent on its subscription (not OpenRouter): opt-in only, 7-day expiry, quiet hours on. */
export function isSubscriptionClaude(a: Agent): boolean {
  return a.kind === "claude" && a.provider !== "openrouter";
}

export interface AgentChoice {
  agent: Agent;
  /** "unattended": Codex or OpenRouter, the fit for scheduled work; "claude": a Claude
   * subscription agent that opted in; "off": doesn't take scheduled work (never selectable). */
  group: "unattended" | "claude" | "off";
}

const GROUP_ORDER = { unattended: 0, claude: 1, off: 2 } as const;

/** The New schedule dialog's agents: Codex and OpenRouter agents first, then Claude subscription
 * agents that accept scheduled work, then the rest (shown, not selectable); by name within each. */
export function scheduleAgents(list: Agent[], origin: "schedule" | "webhook" = "schedule"): AgentChoice[] {
  return list
    .map((agent): AgentChoice => ({
      agent,
      group: !agent.accepts_origins.includes(origin)
        ? "off"
        : isSubscriptionClaude(agent)
        ? "claude"
        : "unattended",
    }))
    .sort((a, b) =>
      GROUP_ORDER[a.group] - GROUP_ORDER[b.group] ||
      a.agent.name.localeCompare(b.agent.name, undefined, { sensitivity: "base" })
    );
}

/** The agent the dialog starts with: the one asked for, if it can take scheduled work, else the
 * first Codex or OpenRouter agent. A Claude subscription agent is never picked for you. */
export function defaultScheduleAgent(choices: AgentChoice[], wanted?: string): string {
  const asked = choices.find((c) => c.agent.name === wanted && c.group !== "off");
  if (asked) return asked.agent.name;
  return choices.find((c) => c.group === "unattended")?.agent.name ?? "";
}

// ---------------- how rows and runs read ----------------

const sameDay = (a: Date, b: Date) =>
  a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
const hhmm = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
const DAY = 24 * 3600_000;

/** A time within the past or coming week as "Mon 09:00", older or later ones "3 Nov 09:00". */
export function shortWhen(ms: number, now: number): string {
  const d = new Date(ms);
  if (Math.abs(ms - now) < 6 * DAY) return `${DAY_SHORT[d.getDay()]} ${hhmm(d)}`;
  return `${d.getDate()} ${MONTH_SHORT[d.getMonth()]} ${hhmm(d)}`;
}

/** A coming time: "today 14:00", "tomorrow 09:00", "Mon 09:00", "3 Nov 09:00". */
export function nextWhen(ms: number, now: number): string {
  const d = new Date(ms), n = new Date(now);
  if (sameDay(d, n)) return `today ${hhmm(d)}`;
  if (sameDay(d, new Date(n.getFullYear(), n.getMonth(), n.getDate() + 1, 12))) return `tomorrow ${hhmm(d)}`;
  return shortWhen(ms, now);
}

/** A preview's run time in full: "Mon 20 Oct 09:00". */
export function longWhen(ms: number): string {
  const d = new Date(ms);
  return `${DAY_SHORT[d.getDay()]} ${d.getDate()} ${MONTH_SHORT[d.getMonth()]} ${hhmm(d)}`;
}

/** What a schedule row's second line says about its next run. */
/** Expired: the backend says so, or its `expires_at` has passed (between slots the backend may
 * not have said yet; the panel's clock does). */
export function isExpired(s: Schedule, now: number): boolean {
  return s.status === "expired" || (s.expires_at !== null && now >= s.expires_at);
}

export function nextLabel(s: Schedule, now: number): { text: string; tone: "next" | "paused" | "expired" | "done" } {
  if (s.status === "done" || (s.next_run === null && s.when.kind === "once")) return { text: "done", tone: "done" };
  if (isExpired(s, now)) return { text: "expired", tone: "expired" };
  if (s.paused) return { text: "paused", tone: "paused" };
  if (s.trigger) {
    // An event waits for its run (the quiet minutes after a note's edits, or the 15-minute floor).
    if (s.next_run !== null) return { text: `runs: ${nextWhen(s.next_run, now)}`, tone: "next" };
    if (s.note && !s.note.triggers) return { text: "triggers off for its note", tone: "paused" };
    return { text: "waiting", tone: "next" };
  }
  if (s.next_run === null) return { text: "no next run", tone: "done" };
  const at = nextWhen(s.next_run, now);
  return { text: s.skip_next ? `skips next: ${at}` : `next: ${at}`, tone: "next" };
}

/** "expires today", "expires tomorrow", "expires Mon 09:00" (Claude subscription agents' recurring
 * schedules, shown the day before), or null. */
export function expiresLabel(s: Schedule, now: number): string | null {
  if (!s.expires_soon || s.expires_at === null || isExpired(s, now)) return null;
  const at = nextWhen(s.expires_at, now);
  return at.startsWith("today") ? "expires today" : at.startsWith("tomorrow") ? "expires tomorrow" : `expires ${at}`;
}

export const OUTCOME_LABEL: Record<RunOutcome, string> = {
  pending: "running",
  replied: "replied",
  blocked: "blocked",
  no_reply: "no reply",
  ended: "agent ended",
  timed_out: "timed out",
  refused: "refused",
  skipped: "skipped",
  failed: "failed",
};

/** How an outcome is coloured: replies green, failures red, skips grey, a run in progress blue. */
export function outcomeTone(o: RunOutcome): "ok" | "bad" | "skip" | "wait" {
  if (o === "replied") return "ok";
  if (o === "skipped") return "skip";
  if (o === "pending") return "wait";
  return "bad";
}

/** "35s", "4m", "1h 5m"; null while a run goes on. */
export function runDuration(r: ScheduleRun): string | null {
  if (r.finished_at === null) return null;
  const s = Math.max(0, Math.round((r.finished_at - r.started_at) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
}

/** A history line: "Mon 09:00 · replied · 4m". Skips have no duration (nothing ran). */
export function runLine(r: ScheduleRun, now: number): string {
  const dur = r.outcome === "skipped" || r.outcome === "refused" ? null : runDuration(r);
  return [shortWhen(r.slot, now), OUTCOME_LABEL[r.outcome], dur].filter(Boolean).join(" · ");
}

/** A run's kind chip: catch-ups and Run now; ordinary runs have none. */
export function runKindLabel(r: ScheduleRun): string | null {
  if (r.kind === "chain") return `then: ${r.agent ?? "agent"}`;
  if (r.kind === "triggered") return "triggered";
  return r.kind === "catch_up" ? "catch-up" : r.kind === "manual" ? "run now" : null;
}

/** The reason a history line shows under it; a catch-up's "catch-up: " is left to its chip. */
export function runReason(r: ScheduleRun): string | null {
  if (!r.reason) return null;
  return r.kind === "catch_up" ? r.reason.replace(/^catch-up:\s*/i, "") || null : r.reason;
}

/** Merge a run into a history list (newest first, by id). */
export function upsertRun(list: ScheduleRun[], r: ScheduleRun): ScheduleRun[] {
  const out = list.filter((x) => x.id !== r.id);
  const i = out.findIndex((x) => x.id < r.id);
  out.splice(i < 0 ? out.length : i, 0, r);
  return out;
}

/** A schedule's last run after `r` was recorded or changed: the newer one wins. */
export function newerRun(cur: ScheduleRun | null, r: ScheduleRun): ScheduleRun | null {
  // A "Then…" step is another agent's: the row's outcome stays its own run's.
  if (r.kind === "chain") return cur;
  return !cur || r.id >= cur.id ? r : cur;
}

/** A history in the order it reads: newest run first, each followed by its "Then…" steps
 * (oldest step first, nested); a step whose run isn't loaded stays where it is. */
export function withSteps(runs: ScheduleRun[]): { run: ScheduleRun; depth: number }[] {
  const ids = new Set(runs.map((r) => r.id));
  const kids = new Map<number, ScheduleRun[]>();
  for (const r of runs) {
    if (r.kind === "chain" && r.parent_run != null && ids.has(r.parent_run)) {
      kids.set(r.parent_run, [...(kids.get(r.parent_run) ?? []), r]);
    }
  }
  const out: { run: ScheduleRun; depth: number }[] = [];
  const add = (r: ScheduleRun, depth: number) => {
    out.push({ run: r, depth });
    for (const k of [...(kids.get(r.id) ?? [])].sort((a, b) => a.id - b.id)) add(k, depth + 1);
  };
  for (const r of runs) {
    if (!(r.kind === "chain" && r.parent_run != null && ids.has(r.parent_run))) add(r, 0);
  }
  return out;
}

/** The panel's order under an agent: soonest next run first, finished ones last, then by name. */
export function sortSchedules(list: Schedule[]): Schedule[] {
  const key = (s: Schedule) => s.next_run ?? Number.MAX_SAFE_INTEGER;
  return [...list].sort((a, b) => key(a) - key(b) || a.name.localeCompare(b.name) || a.id - b.id);
}

// ---------------- the dialog's other fields ----------------

/** "At most N runs a day": empty is no cap; otherwise a whole number from 1 to 200. */
export function parseCap(text: string): { cap: number | null } | { error: string } {
  const t = text.trim();
  if (!t) return { cap: null };
  if (!/^\d+$/.test(t) || Number(t) < 1 || Number(t) > 200) return { error: "A number from 1 to 200, or empty for no cap" };
  return { cap: Number(t) };
}

export type NoteChoice = { id: number } | { title: string } | null;

/** The note a reply goes into: an existing note by its title (any case), or a new one with that
 * title; null for none. */
export function noteChoice(on: boolean, title: string, notes: { id: number; title: string }[]): NoteChoice {
  const t = title.trim();
  if (!on || !t) return null;
  const hit = notes.find((n) => n.title.trim().toLowerCase() === t.toLowerCase());
  return hit ? { id: hit.id } : { title: t };
}

/** Everything the dialog edits, as `schedule.create` / `schedule.update` take it. */
export interface ScheduleFields {
  name: string;
  prompt: string;
  /** null: the timing controls weren't changed (`whenToSend`), so the stored timing stays as is. */
  when: WhenInput | null;
  tz: string;
  missed: Schedule["missed"];
  conversation: Schedule["conversation"];
  notify: Schedule["notify"];
  note: NoteChoice;
  quiet_hours: boolean;
  daily_cap: number | null;
  /** The "Then…" steps (null: none). */
  then?: ThenInput | null;
}

/** What Save sends to `schedule.update`: only the fields that changed. */
export function schedulePatch(before: Schedule, f: ScheduleFields): Partial<ScheduleFields> {
  const p: Partial<ScheduleFields> = {};
  if (f.name.trim() !== before.name) p.name = f.name.trim();
  if (f.prompt !== before.prompt) p.prompt = f.prompt;
  if (f.when !== null) p.when = f.when;
  if (f.tz.trim() && f.tz.trim() !== before.tz) p.tz = f.tz.trim();
  if (f.missed !== before.missed) p.missed = f.missed;
  if (f.conversation !== before.conversation) p.conversation = f.conversation;
  if (f.notify !== before.notify) p.notify = f.notify;
  if (f.quiet_hours !== before.quiet_hours) p.quiet_hours = f.quiet_hours;
  if (f.daily_cap !== before.daily_cap) p.daily_cap = f.daily_cap;
  if (f.then !== undefined && JSON.stringify(f.then) !== JSON.stringify(thenInputOf(before.then))) p.then = f.then;
  const n = f.note;
  if (n === null ? before.note_id !== null : "id" in n ? n.id !== before.note_id : n.title.toLowerCase() !== (before.note_title ?? "").toLowerCase()) {
    p.note = n;
  }
  return p;
}

/** What "If missed: Automatic" does for this timing (the backend's `missed_default`). */
export function missedAutoText(d: "run_once" | "skip" | undefined): string {
  if (d === "run_once") return "for this timing, it runs once when Muster is back.";
  if (d === "skip") return "for this timing, missed runs are skipped: the next one comes soon anyway.";
  return "runs once when Muster is back for daily and slower schedules; skips for hourly and faster ones.";
}
