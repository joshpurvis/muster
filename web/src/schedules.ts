// Pure logic for schedules (docs/UI.md "Agents panel", docs/PROTOCOL.md "Schedules"): the New /
// Edit schedule dialog's timing presets and agent order, and how rows and run history read. No
// signals, no DOM: unit tested (schedules_test.ts). Cron lines are read by the backend
// (`schedule.preview`); here they are only matched against the presets' shapes.

import type { Agent, RunOutcome, Schedule, ScheduleRun, When } from "./types.ts";

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
  /** A datetime-local value, "YYYY-MM-DDTHH:MM" (this browser's zone): Once. */
  once: string;
  /** Custom. */
  cron: string;
}

const pad = (n: number) => String(n).padStart(2, "0");

/** A datetime-local value for `ms`, in this browser's zone. */
export function localInput(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The next whole hour at least an hour from `now`: a one-off's starting value. */
export function defaultOnce(now: number): string {
  const d = new Date(now + 60 * 60_000);
  d.setMinutes(0, 0, 0);
  if (d.getTime() <= now + 30 * 60_000) d.setHours(d.getHours() + 1);
  return localInput(d.getTime());
}

export function blankWhen(now: number): WhenDraft {
  return { preset: "weekdays", time: "09:00", minute: 0, day: 1, once: defaultOnce(now), cron: "0 9 * * 1-5" };
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
 * one-off is Once). Fields the timing doesn't set keep `base`'s values. */
export function draftOfWhen(when: When, base: WhenDraft): WhenDraft {
  if (when.kind === "once") return { ...base, preset: "once", once: localInput(when.at) };
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

/** The timing the fields describe, or what is wrong with them. */
export function whenOfDraft(d: WhenDraft): { when: When } | { error: string } {
  const cron = (c: string): { when: When } => ({ when: { kind: "cron", cron: c } });
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
      const at = d.once ? new Date(d.once).getTime() : NaN;
      if (!Number.isFinite(at)) return { error: "Pick a date and time" };
      return { when: { kind: "once", at } };
    }
    case "custom": {
      const c = d.cron.trim().replace(/\s+/g, " ");
      if (!c) return { error: "Type a cron line: minute, hour, day of month, month, day of week" };
      return cron(c);
    }
  }
}

export function sameWhen(a: When, b: When): boolean {
  if (a.kind === "once" || b.kind === "once") return a.kind === b.kind && (a as { at: number }).at === (b as { at: number }).at;
  return a.cron.trim().replace(/\s+/g, " ") === b.cron.trim().replace(/\s+/g, " ");
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
export function scheduleAgents(list: Agent[]): AgentChoice[] {
  return list
    .map((agent): AgentChoice => ({
      agent,
      group: !agent.accepts_origins.includes("schedule")
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
export function nextLabel(s: Schedule, now: number): { text: string; tone: "next" | "paused" | "expired" | "done" } {
  if (s.status === "done" || (s.next_run === null && s.when.kind === "once")) return { text: "done", tone: "done" };
  if (s.status === "expired") return { text: "expired", tone: "expired" };
  if (s.paused) return { text: "paused", tone: "paused" };
  if (s.next_run === null) return { text: "no next run", tone: "done" };
  const at = nextWhen(s.next_run, now);
  return { text: s.skip_next ? `skips next: ${at}` : `next: ${at}`, tone: "next" };
}

/** "expires today", "expires tomorrow", "expires Mon 09:00" (Claude subscription agents' recurring
 * schedules, shown the day before), or null. */
export function expiresLabel(s: Schedule, now: number): string | null {
  if (!s.expires_soon || s.expires_at === null || s.status === "expired") return null;
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
export function newerRun(cur: ScheduleRun | null, r: ScheduleRun): ScheduleRun {
  return !cur || r.id >= cur.id ? r : cur;
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
  when: When;
  tz: string;
  missed: Schedule["missed"];
  conversation: Schedule["conversation"];
  notify: Schedule["notify"];
  note: NoteChoice;
  quiet_hours: boolean;
  daily_cap: number | null;
}

/** What Save sends to `schedule.update`: only the fields that changed. */
export function schedulePatch(before: Schedule, f: ScheduleFields): Partial<ScheduleFields> {
  const p: Partial<ScheduleFields> = {};
  if (f.name.trim() !== before.name) p.name = f.name.trim();
  if (f.prompt !== before.prompt) p.prompt = f.prompt;
  if (!sameWhen(f.when, before.when)) p.when = f.when;
  if (f.tz.trim() && f.tz.trim() !== before.tz) p.tz = f.tz.trim();
  if (f.missed !== before.missed) p.missed = f.missed;
  if (f.conversation !== before.conversation) p.conversation = f.conversation;
  if (f.notify !== before.notify) p.notify = f.notify;
  if (f.quiet_hours !== before.quiet_hours) p.quiet_hours = f.quiet_hours;
  if (f.daily_cap !== before.daily_cap) p.daily_cap = f.daily_cap;
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
