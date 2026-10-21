// Dev-only: the mock daemon's schedules (docs/PROTOCOL.md "Schedules"), in memory. Plausible, not
// exact: cron lines are read and stepped in the mock's own time zone, nothing fires on its own
// (only Run now sends), and a run's reply comes a second later. dev/mock.ts routes `schedule.*`
// here and seeds a few schedules with history.

import { localInput, zonedToMs } from "../src/schedules.ts";

/** What the mock knows of an agent. */
export interface SchedAgent {
  id: number;
  name: string;
  kind: "claude" | "codex";
  provider: "subscription" | "openrouter";
  accepts_origins: string[];
}

export interface ScheduleDeps {
  now: () => number;
  nextId: () => number;
  broadcast: (event: string, data: unknown) => void;
  agents: () => SchedAgent[];
  /** Records a message to `agent` (and broadcasts it); returns the stored message. */
  sendMessage: (agent: SchedAgent, from: string, text: string, refused?: string | null) => any;
  /** Sets a stored message's reply (and broadcasts it). */
  reply: (msg: any, text: string) => void;
  /** A note by id, or by title (made when none has it). */
  note: (choice: { id?: number; title?: string }) => { id: number; title: string };
  noteTitle: (id: number) => string | null;
  /** A note's "Allow triggers" switch (null: no such note). */
  noteTriggers: (id: number) => boolean | null;
}

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const MIN_GAP = 15 * MIN;
const DAY_NAMES = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAY_SHORT = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MON_SHORT = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const pad = (n: number) => String(n).padStart(2, "0");

// ---------------- cron, read loosely (src/schedules/cron.rs does it properly) ----------------

interface Cron {
  min: number[];
  hour: number[];
  dom: number[];
  mon: number[];
  dow: number[];
  domStar: boolean;
  dowStar: boolean;
}

const MACROS: Record<string, string> = {
  "@hourly": "0 * * * *",
  "@daily": "0 0 * * *",
  "@midnight": "0 0 * * *",
  "@weekly": "0 0 * * 0",
  "@monthly": "0 0 1 * *",
  "@yearly": "0 0 1 1 *",
  "@annually": "0 0 1 1 *",
};

function field(s: string, lo: number, hi: number, what: string): number[] {
  const out = new Set<number>();
  for (const part of s.split(",")) {
    const m = /^(\*|\d+(?:-\d+)?)(?:\/(\d+))?$/.exec(part);
    if (!m) throw new Error(`\`${s}\` isn't a ${what} cron understands`);
    const step = m[2] ? Number(m[2]) : 1;
    let [a, b] = m[1] === "*" ? [lo, hi] : m[1].split("-").map(Number) as [number, number?];
    if (b === undefined) b = m[2] ? hi : a;
    if (step < 1 || a < lo || b! > hi || a > b!) throw new Error(`\`${s}\`: a ${what} goes from ${lo} to ${hi}`);
    for (let v = a; v <= b!; v += step) out.add(what === "day of the week" && v === 7 ? 0 : v);
  }
  return [...out].sort((x, y) => x - y);
}

function parseCron(line: string): Cron {
  const t = MACROS[line.trim()] ?? line.trim();
  const f = t.split(/\s+/);
  if (f.length !== 5) throw new Error(`a cron line has 5 fields (minute hour day-of-month month day-of-week); \`${line}\` has ${f.length}`);
  return {
    min: field(f[0], 0, 59, "minute"),
    hour: field(f[1], 0, 23, "hour"),
    dom: field(f[2], 1, 31, "day of the month"),
    mon: field(f[3], 1, 12, "month"),
    dow: field(f[4], 0, 7, "day of the week"),
    domStar: f[2] === "*",
    dowStar: f[4] === "*",
  };
}

function dayMatches(c: Cron, d: Date): boolean {
  const dom = c.dom.includes(d.getDate()), dow = c.dow.includes(d.getDay());
  if (c.domStar && c.dowStar) return true;
  if (c.domStar) return dow;
  if (c.dowStar) return dom;
  return dom || dow;
}

/** The first fire time strictly after `ms`, in this process's zone. */
function nextAfter(c: Cron, ms: number): number | null {
  const d = new Date(Math.floor(ms / MIN) * MIN + MIN);
  const limit = ms + 4 * 366 * DAY;
  while (d.getTime() < limit) {
    if (!c.mon.includes(d.getMonth() + 1)) {
      d.setMonth(d.getMonth() + 1, 1);
      d.setHours(0, 0, 0, 0);
    } else if (!dayMatches(c, d)) {
      d.setDate(d.getDate() + 1);
      d.setHours(0, 0, 0, 0);
    } else if (!c.hour.includes(d.getHours())) {
      d.setHours(d.getHours() + 1, 0, 0, 0);
    } else if (!c.min.includes(d.getMinutes())) {
      d.setMinutes(d.getMinutes() + 1, 0, 0);
    } else return d.getTime();
  }
  return null;
}

const joinAnd = (xs: string[]) => xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs.at(-1)}`;

function ranges(v: number[]): string {
  const parts: string[] = [];
  for (let i = 0; i < v.length;) {
    let j = i;
    while (j + 1 < v.length && v[j + 1] === v[j] + 1) j++;
    parts.push(j > i + 1 ? `${v[i]}-${v[j]}` : j === i + 1 ? `${v[i]}, ${v[j]}` : String(v[i]));
    i = j + 1;
  }
  return joinAnd(parts);
}

/** A plain-English reading, as src/schedules/cron.rs `describe` gives it. */
function describe(c: Cron): string {
  const allHours = c.hour.length === 24;
  const step = (v: number[], period: number) => {
    if (v.length < 2 || v[0] !== 0) return null;
    const s = v[1] - v[0];
    return period % s === 0 && v.every((x, i) => i === 0 || x - v[i - 1] === s) && v.length === period / s ? s : null;
  };
  const time = (h: number, m: number) => `${pad(h)}:${pad(m)}`;
  let when: string;
  const ms = step(c.min, 60), hs = step(c.hour, 24);
  if (c.min.length === 60 && allHours) when = "Every minute";
  else if (ms && allHours) when = `Every ${ms} minutes`;
  else if (c.min.length === 1 && allHours) when = c.min[0] === 0 ? "Every hour, on the hour" : `Every hour at :${pad(c.min[0])}`;
  else if (c.min.length === 1 && hs) when = `Every ${hs} hours at :${pad(c.min[0])}`;
  else if (c.min.length === 1 && c.hour.length <= 6) when = `At ${joinAnd(c.hour.map((h) => time(h, c.min[0])))}`;
  else if (c.min.length <= 4 && c.hour.length === 1) when = `At ${joinAnd(c.min.map((m) => time(c.hour[0], m)))}`;
  else {
    const mins = c.min.length === 60 ? "every minute" : `minute ${ranges(c.min)}`;
    when = allHours ? `At ${mins}` : `At ${mins} past hour ${ranges(c.hour)}`;
  }
  const order = [1, 2, 3, 4, 5, 6, 0];
  const on = order.filter((d) => c.dow.includes(d));
  const idx = on.map((d) => order.indexOf(d));
  const dow = on.length === 7
    ? "every day"
    : on.join() === "6,0"
    ? "on Saturday and Sunday"
    : on.length >= 3 && idx.every((x, i) => i === 0 || x === idx[i - 1] + 1)
    ? `${DAY_NAMES[on[0]]} to ${DAY_NAMES[on.at(-1)!]}`
    : `on ${joinAnd(on.map((d) => DAY_NAMES[d]))}`;
  const dom = c.dom.length === 1 ? `on day ${c.dom[0]} of the month` : `on days ${ranges(c.dom)} of the month`;
  const days = c.domStar && c.dowStar ? "every day" : c.domStar ? dow : c.dowStar ? dom : `${dom} or ${dow}`;
  const months = c.mon.length === 12 ? "" : `, in ${joinAnd(c.mon.map((m) => MONTHS[m - 1]))}`;
  return `${when}, ${days}${months}`;
}

/** "Tue 21 Oct 2026 at 15:30", in zone `tz`. */
function fmtLong(ms: number, tz: string): string {
  const [date, time] = localInput(ms, tz).split("T");
  const [y, mo, d] = date.split("-").map(Number);
  const dow = new Date(Date.UTC(y, mo - 1, d)).getUTCDay();
  return `${DAY_SHORT[dow]} ${d} ${MON_SHORT[mo - 1]} ${y} at ${time}`;
}

// ---------------- the store ----------------

type Trig =
  | { kind: "note"; note_id: number; quiet_min: number }
  | { kind: "git"; repo: string; branch: string }
  | { kind: "github"; repo: string; event: "pr_opened" | "ci_failed"; branch: string | null; every_min: number }
  | { kind: "url"; name: string };
type When = { kind: "cron"; cron: string } | { kind: "once"; at: number } | Trig;
const isTrig = (w: When): w is Trig => w.kind !== "cron" && w.kind !== "once";

/** "Then…" steps as stored: agents by id. */
interface Step {
  notify: boolean;
  agent_id: number | null;
  prompt: string;
}
interface ThenSteps {
  on_success: Step | null;
  on_failure: Step | null;
}

interface Sched {
  then: ThenSteps | null;
  /** A trigger's held event. */
  pending: { at: number; text: string; count: number; payload: boolean } | null;
  watch: { state: string; detail: string | null; checked_at: number | null; next_at: number | null } | null;
  secret: string | null;
  id: number;
  agent_id: number;
  name: string;
  prompt: string;
  when: When;
  tz: string;
  missed: "auto" | "run_once" | "skip";
  conversation: "continue" | "fresh";
  notify: "on_failure" | "always" | "never";
  note_id: number | null;
  quiet_hours: boolean;
  daily_cap: number | null;
  paused: boolean;
  skip_next: boolean;
  next_run: number | null;
  last_slot: number | null;
  /** Claude subscription agents' recurring schedules: created or renewed at. */
  window_from: number;
  created_at: number;
  updated_at: number;
}

const localZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";

export function scheduleMock(deps: ScheduleDeps) {
  const { now, broadcast } = deps;
  const list: Sched[] = [];
  const runs = new Map<number, any[]>(); // schedule id -> runs, oldest first
  /** OpenRouter spend (src/spend.rs): plausible numbers, never fetched. */
  const spend = {
    key_present: true,
    today: 1.84 as number | null,
    cap: 5,
    other: 0.12,
    caps: new Map<number, number>(),
    spent: new Map<number, number>(),
    checked_at: now() - 3 * MIN,
  };
  const spendJson = () => ({
    key_present: spend.key_present,
    today: spend.today,
    cap: spend.cap,
    other: spend.other,
    agents: deps.agents().filter((a) => a.provider === "openrouter").map((a) => ({
      id: a.id,
      name: a.name,
      spent: spend.spent.get(a.id) ?? 0,
      cap: spend.caps.get(a.id) ?? null,
    })),
    checked_at: spend.checked_at,
    error: null,
    resets_at: (Math.floor(now() / DAY) + 1) * DAY,
  });
  const settings = { paused: false, quiet_start: "23:00", quiet_end: "07:00", unattended_cap: 24, tz: localZone() };
  const settingsJson = () => ({ ...settings, spend: spendJson() });

  const agentOf = (s: Sched) => deps.agents().find((a) => a.id === s.agent_id);
  const subClaude = (a: SchedAgent | undefined) => !!a && a.kind === "claude" && a.provider !== "openrouter";
  const stepAgentIds = (t: ThenSteps | null) =>
    [t?.on_success?.agent_id, t?.on_failure?.agent_id].filter((x): x is number => typeof x === "number");
  const anySubClaude = (s: Sched) =>
    subClaude(agentOf(s)) || stepAgentIds(s.then).some((id) => subClaude(deps.agents().find((a) => a.id === id)));
  const expiresAt = (s: Sched) => anySubClaude(s) && s.when.kind !== "once" ? s.window_from + 7 * DAY : null;

  function firstSlot(when: When, after: number): number | null {
    if (isTrig(when)) return null;
    if (when.kind === "once") {
      if (!(when.at > after)) throw new Error("that time has passed: pick a time in the future");
      if (when.at - after > 366 * DAY) throw new Error("a one-off schedule can be at most a year ahead");
      return when.at;
    }
    const c = parseCron(when.cron);
    let t = nextAfter(c, after);
    if (t === null) throw new Error(`\`${when.cron}\` never runs`);
    const first = t;
    for (let i = 0; i < 60 && t !== null; i++) {
      const n = nextAfter(c, t);
      if (n !== null && n - t < MIN_GAP) {
        throw new Error(
          `\`${when.cron}\` runs ${Math.round((n - t) / MIN)} minutes apart at times; schedules run at most every 15 minutes`,
        );
      }
      t = n;
    }
    return first;
  }

  function triggerReading(w: Trig): string {
    switch (w.kind) {
      case "note":
        return `When the note “${deps.noteTitle(w.note_id) ?? `note ${w.note_id} (deleted)`}” changes (after ${w.quiet_min} minute${
          w.quiet_min === 1 ? "" : "s"
        } of quiet)`;
      case "git":
        return `When ${w.branch} moves in ${w.repo}`;
      case "github":
        return w.event === "pr_opened"
          ? `When a pull request is opened on ${w.repo} (checked every ${w.every_min} minutes)`
          : `When CI fails on ${w.repo}${w.branch ? ` (${w.branch})` : ""} (checked every ${w.every_min} minutes)`;
      case "url":
        return `When its trigger URL is called (muster trigger ${w.name})`;
    }
  }
  const reading = (when: When, tz: string) =>
    isTrig(when) ? triggerReading(when) : when.kind === "once" ? `Once, ${fmtLong(when.at, tz)}` : describe(parseCron(when.cron));

  function inQuiet(ms: number): boolean {
    const [sh, sm] = settings.quiet_start.split(":").map(Number), [eh, em] = settings.quiet_end.split(":").map(Number);
    const d = new Date(ms), m = d.getHours() * 60 + d.getMinutes(), a = sh * 60 + sm, b = eh * 60 + em;
    return a <= b ? m >= a && m < b : m >= a || m < b;
  }

  /** Daily or slower (and one-offs) run once after downtime; hourly or faster ones skip. */
  function missedDefault(when: When): "run_once" | "skip" {
    if (when.kind !== "cron") return "run_once";
    const c = parseCron(when.cron);
    const a = nextAfter(c, now()), b = a !== null ? nextAfter(c, a) : null;
    return a !== null && b !== null && b - a <= HOUR ? "skip" : "run_once";
  }

  function json(s: Sched) {
    const a = agentOf(s);
    const exp = expiresAt(s);
    const expired = exp !== null && now() > exp;
    const r = runs.get(s.id) ?? [];
    return {
      id: s.id,
      agent_id: s.agent_id,
      agent: a?.name ?? null,
      name: s.name,
      prompt: s.prompt,
      when: s.when,
      tz: s.tz,
      reading: reading(s.when, s.tz),
      missed: s.missed,
      missed_effective: s.missed === "auto" ? missedDefault(s.when) : s.missed,
      conversation: s.conversation,
      notify: s.notify,
      note_id: s.note_id,
      note_title: s.note_id !== null ? deps.noteTitle(s.note_id) : null,
      quiet_hours: s.quiet_hours,
      daily_cap: s.daily_cap,
      paused: s.paused,
      skip_next: s.skip_next,
      next_run: s.next_run,
      last_slot: s.last_slot,
      status: expired ? "expired" : s.next_run === null && !isTrig(s.when) ? "done" : s.paused ? "paused" : "active",
      expires_at: exp,
      expires_soon: exp !== null && !expired && exp - now() <= DAY,
      created_at: s.created_at,
      updated_at: s.updated_at,
      last_run: r.filter((x) => x.kind !== "chain").at(-1) ?? null,
      trigger: isTrig(s.when),
      origin: isTrig(s.when) ? "webhook" : "schedule",
      then: thenJson(s.then),
      pending: s.pending,
      watch: s.when.kind === "git" || s.when.kind === "github" ? s.watch ?? { state: "ok", detail: null, checked_at: now() - MIN, next_at: null } : null,
      note: s.when.kind === "note"
        ? { id: s.when.note_id, title: deps.noteTitle(s.when.note_id) ?? "", triggers: !!deps.noteTriggers(s.when.note_id) }
        : null,
      url: s.when.kind === "url" ? `http://127.0.0.1:7433/api/trigger/${s.when.name}` : null,
    };
  }

  const agentName = (id: number | null) => deps.agents().find((a) => a.id === id)?.name ?? null;
  function thenJson(t: ThenSteps | null) {
    if (!t) return null;
    const step = (s: Step | null) => s && { ...s, agent: agentName(s.agent_id), then: null };
    return { on_success: step(t.on_success), on_failure: step(t.on_failure) };
  }
  function thenIn(raw: any, origin: string): ThenSteps | null {
    if (!raw) return null;
    const step = (s: any, failure: boolean): Step | null => {
      if (!s) return null;
      if (s.notify) {
        if (!failure) throw new Error("only the failure branch can just notify you (use Notify me: Always for successes)");
        return { notify: true, agent_id: null, prompt: "" };
      }
      const a = agentNamed(s.agent);
      if (!a.accepts_origins.includes(origin)) {
        throw new Error(`agent ${a.name} doesn't take ${origin === "webhook" ? "triggered" : "scheduled"} work`);
      }
      if (!String(s.prompt ?? "").trim()) throw new Error("the prompt must not be empty");
      return { notify: false, agent_id: a.id, prompt: String(s.prompt) };
    };
    const t = { on_success: step(raw.on_success, false), on_failure: step(raw.on_failure, true) };
    return t.on_success || t.on_failure ? t : null;
  }

  const publish = (s: Sched) => {
    const j = json(s);
    broadcast("schedule_upserted", j);
    return j;
  };
  const get = (id: number) => {
    const s = list.find((x) => x.id === Number(id));
    if (!s) throw new Error(`no schedule with id ${id}`);
    return s;
  };
  const agentNamed = (name: string) => {
    const a = deps.agents().find((x) => x.name.toLowerCase() === String(name ?? "").trim().toLowerCase());
    if (!a) throw new Error(`no agent named ${name}`);
    return a;
  };
  function checkAgent(a: SchedAgent, origin = "schedule") {
    if (a.accepts_origins.includes(origin)) return;
    if (origin === "webhook") {
      throw new Error(`agent ${a.name} doesn't take triggered work: tick Webhook (triggered) under "Takes messages started by" in Edit agent.`);
    }
    throw new Error(
      `agent ${a.name} doesn't take scheduled work: tick Schedule under "Takes messages started by" in Edit agent (or \`muster agent edit ${a.name} --accepts human,schedule\`).${
        subClaude(a)
          ? " Claude subscription agents take scheduled work only if you opt in (read the warning there first); a Codex or OpenRouter agent is the better fit."
          : ""
      }`,
    );
  }
  function cleanName(name: unknown, prompt: string) {
    const n = String(name ?? "").trim();
    if (n) return n.slice(0, 120);
    return (prompt.split("\n").map((l) => l.trim()).find((l) => l) ?? "Schedule").slice(0, 60);
  }
  function capOf(v: unknown): number | null {
    if (v === null || v === undefined || v === 0) return null;
    const n = Number(v);
    if (!Number.isInteger(n) || n < 1 || n > 200) throw new Error("daily_cap is 1 to 200 (or null for none)");
    return n;
  }
  function noteOf(v: any): number | null {
    if (v === null || v === undefined) return null;
    if (typeof v.title === "string" && !v.title.trim()) throw new Error("give the note a title");
    return deps.note(v).id;
  }

  function record(s: Sched, r: any) {
    const arr = runs.get(s.id) ?? [];
    const i = arr.findIndex((x) => x.id === r.id);
    if (i < 0) arr.push(r);
    else arr[i] = r;
    runs.set(s.id, arr.slice(-500));
    broadcast("schedule_run", r);
  }

  function newRun(s: Sched, p: Partial<any>): any {
    return {
      id: deps.nextId(),
      schedule_id: s.id,
      slot: now(),
      kind: "scheduled",
      started_at: now(),
      finished_at: null,
      outcome: "pending",
      late_reply: false,
      reason: null,
      msg_id: null,
      message: null,
      ...p,
    };
  }

  /** A run that reaches the agent: its message, then the reply a second later. */
  function send(s: Sched, run: any) {
    const a = agentOf(s);
    if (!a) {
      record(s, { ...run, outcome: "failed", finished_at: now(), reason: "its agent is gone" });
      return run;
    }
    const origin = isTrig(s.when) ? "webhook" : "schedule";
    const from = `${isTrig(s.when) ? "trigger" : "schedule"}:${s.id}`;
    if (!a.accepts_origins.includes(origin)) {
      const why = `${a.name} accepts only ${a.accepts_origins.join(", ")}-started messages, and this one is origin=${origin}`;
      const m = deps.sendMessage(a, from, s.prompt, why);
      const r = { ...run, outcome: "refused", finished_at: now(), reason: why, msg_id: m.id, message: m };
      record(s, r);
      publish(s);
      return r;
    }
    const prev = (runs.get(s.id) ?? []).filter((x) => x.msg_id !== null).at(-1);
    const text = s.prompt
      .replaceAll("{date}", new Date(run.slot).toISOString().slice(0, 10))
      .replaceAll("{last_run}", prev ? new Date(prev.started_at).toISOString().slice(0, 16).replace("T", " ") : "never")
      .replaceAll("{last_reply}", prev?.message?.reply ?? "")
      .replaceAll("{event}", run.event?.text ?? "(no event)");
    const m = deps.sendMessage(a, from, text);
    const sent = { ...run, msg_id: m.id, message: m };
    record(s, sent);
    publish(s);
    setTimeout(() => {
      deps.reply(m, `Done: ${s.name.toLowerCase()} for ${new Date(run.slot).toDateString()}. Nothing needs you.`);
      const r = { ...sent, outcome: "replied", finished_at: now(), message: m };
      record(s, r);
      if (s.note_id !== null) {
        // The real backend appends the reply to the note; the mock only says it would.
      }
      publish(s);
      // "Then…": the success step, one hop further (the mock replies a second later).
      const step = s.then?.on_success;
      const to = step && deps.agents().find((x) => x.id === step.agent_id);
      if (step && to) {
        const msg = deps.sendMessage(to, `${from}/then:${r.id}`, step.prompt.replaceAll("{reply}", m.reply ?? ""));
        const c = newRun(s, { kind: "chain", parent_run: r.id, step: "s", agent: to.name, msg_id: msg.id, message: msg });
        record(s, c);
        setTimeout(() => {
          deps.reply(msg, "Filed a note with the likely causes.");
          record(s, { ...c, outcome: "replied", finished_at: now(), message: msg });
        }, 1000);
      }
    }, 1000);
    return sent;
  }

  /** A timing as sent: a one-off may be a wall-clock `local` time, read in zone `tz`; or a trigger. */
  function whenIn(raw: any, tz: string, own?: number): When {
    if (raw?.kind === "note") {
      if (deps.noteTriggers(raw.note_id) === null) throw new Error(`no note with id ${raw.note_id}`);
      return { kind: "note", note_id: raw.note_id, quiet_min: raw.quiet_min ?? 2 };
    }
    if (raw?.kind === "git") {
      if (!String(raw.repo).startsWith("/") && !String(raw.repo).startsWith("~")) throw new Error("give the repository as a full path (or ~/…)");
      return { kind: "git", repo: String(raw.repo), branch: String(raw.branch) };
    }
    if (raw?.kind === "github") {
      if (!/^[\w.-]+\/[\w.-]+$/.test(String(raw.repo))) throw new Error(`\`${raw.repo}\` isn't OWNER/NAME (e.g. octocat/hello-world)`);
      return { kind: "github", repo: raw.repo, event: raw.event, branch: raw.branch ?? null, every_min: raw.every_min ?? 5 };
    }
    if (raw?.kind === "url") {
      const n = String(raw.name).toLowerCase();
      if (!/^[a-z0-9][a-z0-9_-]{0,39}$/.test(n)) throw new Error("a trigger name is 1-40 characters of a-z, 0-9, - and _");
      if (list.some((s) => s.id !== own && s.when.kind === "url" && s.when.name === n)) throw new Error(`there is already a trigger named ${n}`);
      return { kind: "url", name: n };
    }
    if (raw?.kind === "cron" && typeof raw.cron === "string") return { kind: "cron", cron: raw.cron };
    if (raw?.kind === "once" && typeof raw.local === "string") {
      const at = zonedToMs(raw.local, tz);
      if (!Number.isFinite(at)) throw new Error(`\`${raw.local}\` isn't a date and time like 2026-10-21T15:30`);
      return { kind: "once", at };
    }
    if (raw?.kind === "once" && typeof raw.at === "number") return { kind: "once", at: raw.at };
    throw new Error('when is required: {kind: "cron", cron}, {kind: "once", at} or {kind: "once", local}');
  }

  function create(p: any) {
    const a = agentNamed(p.agent);
    const tz = String(p.tz ?? "").trim() || localZone();
    const when = whenIn(p.when, tz);
    const origin = isTrig(when) ? "webhook" : "schedule";
    checkAgent(a, origin);
    const prompt = String(p.prompt ?? "");
    if (!prompt.trim()) throw new Error("the prompt must not be empty");
    if (list.length >= 200) throw new Error("200 schedules in all is the limit");
    const t = now();
    const s: Sched = {
      then: thenIn(p.then, origin),
      pending: null,
      watch: null,
      secret: when.kind === "url" ? crypto.randomUUID().replaceAll("-", "") : null,
      id: deps.nextId(),
      agent_id: a.id,
      name: cleanName(p.name, prompt),
      prompt,
      when,
      tz,
      missed: p.missed ?? "auto",
      conversation: p.conversation ?? "continue",
      notify: p.notify ?? "on_failure",
      note_id: noteOf(p.note),
      quiet_hours: p.quiet_hours ?? subClaude(a),
      daily_cap: p.daily_cap === undefined && isTrig(when) ? 10 : capOf(p.daily_cap),
      paused: !!p.paused,
      skip_next: false,
      next_run: firstSlot(when, t),
      last_slot: null,
      window_from: t,
      created_at: t,
      updated_at: t,
    };
    list.push(s);
    return publish(s);
  }

  function update(p: any) {
    const s = get(p.id);
    if ("agent" in p) throw new Error("a schedule's agent can't change: make a new schedule");
    const tz = p.tz !== undefined ? String(p.tz).trim() || localZone() : s.tz;
    const when = p.when !== undefined ? whenIn(p.when, tz, s.id) : s.when;
    const origin = isTrig(when) ? "webhook" : "schedule";
    if (isTrig(when) !== isTrig(s.when)) checkAgent(agentOf(s)!, origin);
    if ("then" in p) s.then = thenIn(p.then, origin);
    // As the backend: an equal timing (compared by meaning, roughly) keeps Skip next.
    const same = JSON.stringify(when) === JSON.stringify(s.when) && tz === s.tz;
    if (!same) {
      s.next_run = firstSlot(when, now());
      s.skip_next = false;
      s.pending = null;
      if (when.kind === "url" && !s.secret) s.secret = crypto.randomUUID().replaceAll("-", "");
    }
    s.when = when;
    s.tz = tz;
    if (p.prompt !== undefined) {
      if (!String(p.prompt).trim()) throw new Error("the prompt must not be empty");
      s.prompt = String(p.prompt);
    }
    if (p.name !== undefined) s.name = cleanName(p.name, s.prompt);
    for (const k of ["missed", "conversation", "notify", "quiet_hours", "paused"] as const) {
      if (p[k] !== undefined) (s as any)[k] = p[k];
    }
    if ("note" in p) s.note_id = noteOf(p.note);
    if ("daily_cap" in p) s.daily_cap = capOf(p.daily_cap);
    s.updated_at = now();
    return publish(s);
  }

  /** Hold an event on trigger `s` (merged with one waiting; runs after the quiet minutes or the
   * 15-minute floor). The mock never fires on its own; Run now still sends. */
  function hold(s: Sched, text: string, payload: boolean, debounceMin = 0) {
    const t = now();
    s.pending = s.pending
      ? { ...s.pending, text: `${s.pending.text}\n${text}`, count: s.pending.count + 1, payload: s.pending.payload || payload }
      : { at: t, text, count: 1, payload };
    const last = (runs.get(s.id) ?? []).filter((r) => r.msg_id !== null).at(-1)?.started_at;
    const floor = last ? last + MIN_GAP : 0;
    s.next_run = Math.max(t + debounceMin * MIN, floor);
    publish(s);
  }

  /** A note's text changed (the human's edit, in the mock). */
  function noteChanged(noteId: number, title: string) {
    if (!deps.noteTriggers(noteId)) return;
    for (const s of list) {
      if (s.when.kind === "note" && s.when.note_id === noteId && !s.paused) {
        hold(s, `The note “${title}” was edited by you.`, false, s.when.quiet_min);
      }
    }
  }

  /** A note's "Allow triggers" changed: its triggers read it again (off drops a held event). */
  function noteTriggersChanged(noteId: number) {
    for (const s of list) {
      if (s.when.kind === "note" && s.when.note_id === noteId) {
        if (!deps.noteTriggers(noteId)) {
          s.pending = null;
          s.next_run = null;
        }
        publish(s);
      }
    }
  }

  function handle(method: string, p: any): unknown {
    switch (method) {
      case "schedule.list": {
        const a = p?.agent ? agentNamed(p.agent) : null;
        return { schedules: list.filter((s) => !a || s.agent_id === a.id).map(json), settings: settingsJson() };
      }
      case "schedule.create":
        return create(p);
      case "schedule.update":
        return update(p);
      case "schedule.remove": {
        const s = get(p.id);
        list.splice(list.indexOf(s), 1);
        runs.delete(s.id);
        broadcast("schedule_removed", { id: s.id, agent_id: s.agent_id });
        return { removed: s.id };
      }
      case "schedule.pause": {
        const s = get(p.id);
        s.paused = p.on ?? true;
        s.updated_at = now();
        return publish(s);
      }
      case "schedule.skip": {
        const s = get(p.id);
        s.skip_next = p.on ?? true;
        s.updated_at = now();
        return publish(s);
      }
      case "schedule.renew": {
        const s = get(p.id);
        if (expiresAt(s) === null) throw new Error("only a Claude subscription agent's recurring schedule expires; this one doesn't");
        s.window_from = now();
        if (s.next_run === null || s.next_run < now()) s.next_run = firstSlot(s.when, now());
        return publish(s);
      }
      case "schedule.run": {
        const s = get(p.id);
        const going = (runs.get(s.id) ?? []).some((r) => r.outcome === "pending");
        if (going) {
          const r = newRun(s, { kind: "manual", outcome: "skipped", finished_at: now(), reason: "busy: the previous run is still going" });
          record(s, r);
          publish(s);
          return r;
        }
        return send(s, newRun(s, { kind: "manual" }));
      }
      case "schedule.runs": {
        const s = get(p.id);
        const limit = Math.min(200, Math.max(1, Number(p.limit ?? 20)));
        const all = [...(runs.get(s.id) ?? [])].reverse().filter((r) => p.before == null || r.id < p.before);
        return { runs: all.slice(0, limit), more: all.length > limit };
      }
      case "schedule.mock_watch": {
        // Dev only: how a git or GitHub watcher is doing ({id, state, detail}).
        const s = get(p.id);
        s.watch = { state: p.state, detail: p.detail ?? null, checked_at: now(), next_at: now() + 10 * MIN };
        return publish(s);
      }
      case "trigger.secret": {
        const s = p.id !== undefined ? get(p.id) : list.find((x) => x.when.kind === "url" && x.when.name === p.name);
        if (!s || s.when.kind !== "url") throw new Error("schedule isn't a trigger URL");
        if (p.rotate || !s.secret) s.secret = crypto.randomUUID().replaceAll("-", "");
        const url = `http://127.0.0.1:7433/api/trigger/${s.when.name}`;
        return { name: s.when.name, url, secret: s.secret, curl: `curl -fsS -X POST -H 'Authorization: Bearer ${s.secret}' --data-binary @- ${url}` };
      }
      case "trigger.fire": {
        const s = list.find((x) => x.when.kind === "url" && x.when.name === String(p.name).toLowerCase());
        if (!s) throw new Error(`no trigger named ${p.name}`);
        hold(s, `\`muster trigger ${p.name}\` was run.`, !!p.payload);
        return { queued: true, trigger: p.name, runs_at: s.next_run };
      }
      case "schedule.preview": {
        const tz = String(p.tz ?? "").trim() || localZone();
        const when = whenIn(p.when, tz, p.id);
        const a = p.agent ? agentNamed(p.agent) : undefined;
        if (isTrig(when)) {
          return {
            reading: reading(when, tz),
            tz,
            when,
            next: [],
            quiet: [],
            quiet_hours_default: subClaude(a),
            missed_default: "run_once",
            expires: subClaude(a),
            trigger: true,
            note_triggers_off: when.kind === "note" && !deps.noteTriggers(when.note_id),
            daily_cap_default: 10,
          };
        }
        const next: number[] = [];
        const first = firstSlot(when, now());
        if (first !== null) next.push(first);
        if (when.kind === "cron") {
          const c = parseCron(when.cron);
          while (next.length < 3) {
            const n = nextAfter(c, next.at(-1)!);
            if (n === null) break;
            next.push(n);
          }
        }
        const quietOn = p.quiet_hours ?? subClaude(a);
        return {
          reading: reading(when, tz),
          tz,
          next,
          quiet: next.map((t) => quietOn && inQuiet(t)),
          quiet_hours_default: subClaude(a),
          missed_default: missedDefault(when),
          expires: subClaude(a) && when.kind === "cron",
        };
      }
      case "schedule.settings.get":
        return settingsJson();
      case "schedule.settings.set": {
        if (p.spend_cap !== undefined) {
          if (!(p.spend_cap >= 0.01 && p.spend_cap <= 1000)) throw new Error("a daily spend cap is $0.01 to $1000");
          spend.cap = p.spend_cap;
        }
        for (const [name, cap] of Object.entries(p.agent_spend_caps ?? {})) {
          const a = agentNamed(name);
          if (cap === null) spend.caps.delete(a.id);
          else spend.caps.set(a.id, Number(cap));
        }
        const hhmm = (v: unknown) => /^([01]\d|2[0-3]):[0-5]\d$/.test(String(v));
        if (p.quiet_start !== undefined && !hhmm(p.quiet_start)) throw new Error("quiet_start is HH:MM, like 23:00");
        if (p.quiet_end !== undefined && !hhmm(p.quiet_end)) throw new Error("quiet_end is HH:MM, like 07:00");
        if (p.unattended_cap !== undefined && !(p.unattended_cap >= 1 && p.unattended_cap <= 200)) {
          throw new Error("unattended_cap is 1 to 200");
        }
        for (const k of ["paused", "quiet_start", "quiet_end", "unattended_cap"] as const) {
          if (p[k] !== undefined) (settings as any)[k] = p[k];
        }
        broadcast("schedule_settings", settingsJson());
        for (const s of list) publish(s);
        return settingsJson();
      }
    }
    throw new Error(`unknown method ${method}`);
  }

  /** Seed: a schedule made `ageMs` ago, with past runs (oldest first) as `[slot, outcome, extra]`. */
  function seed(
    p: any,
    ageMs: number,
    history: {
      slot: number;
      outcome: string;
      kind?: string;
      reason?: string;
      mins?: number;
      reply?: string;
      /** A triggered run: what happened. */
      event?: string;
      /** A chain step: its agent (it follows the run before it). */
      agent?: string;
    }[] = [],
  ) {
    const j = create(p) as { id: number };
    const s = get(j.id);
    s.created_at = s.window_from = s.updated_at = now() - ageMs;
    for (const h of history) {
      const prevRun = (runs.get(s.id) ?? []).at(-1);
      const a = h.agent ? agentNamed(h.agent) : agentOf(s)!;
      const r = newRun(s, {
        slot: h.slot,
        started_at: h.slot,
        finished_at: h.slot + (h.mins ?? 0) * MIN + 20_000,
        kind: h.kind ?? (h.agent ? "chain" : isTrig(s.when) ? "triggered" : "scheduled"),
        outcome: h.outcome,
        reason: h.reason ?? null,
        event: h.event ? { at: h.slot, text: h.event, count: 1, payload: false } : null,
        ...(h.agent ? { parent_run: prevRun?.id ?? null, step: "s", agent: a.name } : {}),
      });
      if (h.outcome !== "skipped") {
        const from = `${isTrig(s.when) ? "trigger" : "schedule"}:${s.id}${h.agent ? `/then:${r.id}` : ""}`;
        const m = deps.sendMessage(a, from, h.agent ? (s.then?.on_success?.prompt ?? s.prompt) : s.prompt, h.outcome === "refused" ? h.reason : null);
        m.created_at = h.slot;
        if (h.reply) {
          m.reply = h.reply;
          m.replied_at = r.finished_at;
        }
        r.msg_id = m.id;
        r.message = m;
      }
      (runs.get(s.id) ?? runs.set(s.id, []).get(s.id)!).push(r);
      s.last_slot = h.slot;
    }
    return s;
  }

  /** The `n`th weekday (Mon-Fri) at 09:00 before now, counting back from the latest. */
  function weekdayAt(n: number, h = 9, m = 0): number {
    const d = new Date(now());
    d.setHours(h, m, 0, 0);
    if (d.getTime() > now()) d.setDate(d.getDate() - 1);
    let left = n;
    for (;;) {
      if (d.getDay() !== 0 && d.getDay() !== 6) {
        if (left === 0) return d.getTime();
        left--;
      }
      d.setDate(d.getDate() - 1);
    }
  }

  return { handle, seed, weekdayAt, noteChanged, noteTriggersChanged, hold: (id: number, text: string) => hold(get(id), text, false) };
}
