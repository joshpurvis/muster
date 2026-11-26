// The sidebar's one list (docs/UI.md "Channels sidebar"): each channel in its saved order, with
// the named agent that owns it, agents whose channel isn't in this window after them, the
// "Needs you" box, and what each agent's row flags. Pure functions, tested in sidebar_test.ts.
import type { Agent, Channel, ChannelId, QueueHeld, Schedule, SessionInfo } from "./types.ts";
import { isProposal, outcomeTone, waitingProposals } from "./schedules.ts";

/** One row: a channel (with the agent that owns it, if any), or an agent with no channel here. */
export interface SidebarRow {
  key: string;
  channel: Channel | null;
  agent: Agent | null;
}

/** Which agent owns each channel (`agent.channel_id`). Two claiming one: the older agent. */
export function ownerOf(agents: Agent[]): Map<ChannelId, Agent> {
  const out = new Map<ChannelId, Agent>();
  for (const a of [...agents].sort((x, y) => x.id - y.id)) {
    if (a.channel_id !== null && !out.has(a.channel_id)) out.set(a.channel_id, a);
  }
  return out;
}

/** The window's channels in their order, each with its agent; then every agent whose channel
 * isn't among them (never started, archived, or shown in another window), by name, so each
 * agent keeps a row somewhere. */
export function sidebarRows(channels: Channel[], agents: Agent[]): SidebarRow[] {
  const owner = ownerOf(agents);
  const rows: SidebarRow[] = channels.map((c) => ({ key: `c${c.id}`, channel: c, agent: owner.get(c.id) ?? null }));
  const shown = new Set(rows.flatMap((r) => (r.agent ? [r.agent.id] : [])));
  const rest = agents.filter((a) => !shown.has(a.id)).sort((x, y) => x.name.localeCompare(y.name) || x.id - y.id);
  return [...rows, ...rest.map((a) => ({ key: `a${a.id}`, channel: null, agent: a }))];
}

/** The agent's own name, shown faint beside its channel's when the two differ (nothing is renamed). */
export function otherName(channel: Channel | null, agent: Agent | null): string | null {
  if (!channel || !agent || agent.name === channel.name) return null;
  return agent.name;
}

/** Failed runs the human hid from Needs you: schedule id → the run hidden (a per-viewer pref). */
export type HiddenRuns = Readonly<Record<string, number>>;

/** The schedule's last run failed (blocked, refused, no reply, timed out, ended, failed) and the
 * human hasn't hidden that run. A newer run replaces it, so the next failure shows again. */
export function failedRun(s: Schedule, hidden: HiddenRuns): boolean {
  const r = s.last_run;
  return !!r && !isProposal(s) && s.status !== "expired" && outcomeTone(r.outcome) === "bad" && hidden[s.id] !== r.id;
}

/** The value Hide records for an expired schedule: below every run id, and new each time it
 * expires (a renewal moves `expires_at`). */
export function expiredKey(s: Schedule): number {
  return -(s.expires_at ?? 0);
}

/** The schedule expired (a Claude subscription agent's, after 7 days) and the human hasn't hidden
 * this expiry. It goes when they renew or remove it. */
export function expiredShown(s: Schedule, hidden: HiddenRuns): boolean {
  return s.status === "expired" && hidden[s.id] !== expiredKey(s);
}

/** A failed row in words: "failed twice", "failed 3 times"; null for a single failure. */
export function failingLabel(s: Schedule): string | null {
  const n = s.failing?.runs ?? 0;
  return n < 2 ? null : n === 2 ? "failed twice" : `failed ${n} times`;
}

export type NeedsItem =
  | { kind: "proposal"; schedule: Schedule }
  | { kind: "expired"; schedule: Schedule }
  | { kind: "run"; schedule: Schedule };

/** What the "Needs you" box holds: schedules waiting for approval, oldest first, then expired
 * schedules, first to expire first, then schedules whose last run failed, newest failure first.
 * Empty: the box isn't shown. */
export function needsYouItems(schedules: Schedule[], hidden: HiddenRuns): NeedsItem[] {
  const props = waitingProposals(schedules).map((s): NeedsItem => ({ kind: "proposal", schedule: s }));
  const expired = schedules.filter((s) => expiredShown(s, hidden))
    .sort((a, b) => ((a.expires_at ?? 0) - (b.expires_at ?? 0)) || a.id - b.id)
    .map((s): NeedsItem => ({ kind: "expired", schedule: s }));
  const runs = schedules.filter((s) => failedRun(s, hidden))
    .sort((a, b) => (b.last_run!.started_at - a.last_run!.started_at) || a.id - b.id)
    .map((s): NeedsItem => ({ kind: "run", schedule: s }));
  return [...props, ...expired, ...runs];
}

/** A session whose queued messages are held, for Needs you: `who` is its agent's name, else the
 * session's own. */
export interface HeldQueue {
  session: SessionInfo;
  who: string;
  held: QueueHeld;
}

/** The sessions whose queued messages are held (`queue_held`), the longest wait first. They
 * lead the "Needs you" box: messages that sit untyped until you look. */
export function heldQueues(sessions: Iterable<SessionInfo>, agents: Agent[]): HeldQueue[] {
  const out: HeldQueue[] = [];
  for (const s of sessions) {
    if (!s.queue_held) continue;
    out.push({ session: s, who: agents.find((a) => a.session_id === s.id)?.name ?? s.name, held: s.queue_held });
  }
  return out.sort((a, b) => a.held.since - b.held.since || a.session.id - b.session.id);
}

/** "42 min", "1 h 5 min", "2 h": how long a queue has been held. */
export function heldFor(ms: number): string {
  const min = Math.max(0, Math.floor(ms / 60_000));
  const h = Math.floor(min / 60), m = min % 60;
  return h === 0 ? `${m} min` : m === 0 ? `${h} h` : `${h} h ${m} min`;
}

/** A held queue in words: "3 messages held 42 min · from lead, manager" and why, as a sentence. */
export function heldText(held: QueueHeld, now: number): { line: string; why: string } {
  const what = held.prompts === 1 ? "1 message" : `${held.prompts} messages`;
  const from = held.senders.length ? ` · from ${held.senders.join(", ")}` : "";
  const why = held.reason ? held.reason[0].toUpperCase() + held.reason.slice(1) : "";
  return { line: `${what} held ${heldFor(now - held.since)}${from}`, why };
}

/** An agent row's chip: amber while proposals wait, red while a run failed (both count), else
 * the number of its schedules (shown only when the sidebar is wide); null with none. */
export interface RowChip {
  tone: "bad" | "wait" | "plain";
  count: number;
  /** Its accessible name and tooltip. */
  label: string;
}

export function rowChip(agentId: number, schedules: Schedule[], hidden: HiddenRuns): RowChip | null {
  const own = schedules.filter((s) => s.agent_id === agentId);
  if (!own.length) return null;
  const waiting = own.filter(isProposal).length;
  const failed = own.filter((s) => failedRun(s, hidden)).length;
  const expired = own.filter((s) => expiredShown(s, hidden)).length;
  const n = (k: number, one: string, many: string) => `${k} ${k === 1 ? one : many}`;
  if (waiting || failed || expired) {
    const parts = [
      waiting && `${n(waiting, "schedule waits", "schedules wait")} for your approval`,
      expired && `${n(expired, "schedule", "schedules")} expired`,
      failed && `${n(failed, "schedule's", "schedules'")} last run failed`,
    ].filter(Boolean);
    return { tone: failed || expired ? "bad" : "wait", count: waiting + failed + expired, label: parts.join("; ") };
  }
  return { tone: "plain", count: own.length, label: n(own.length, "schedule", "schedules") };
}

/** Hide a schedule's failed run (`runId`), or its expiry (`expiredKey`); entries of schedules
 * that are gone are dropped. */
export function hideRun(hidden: HiddenRuns, scheduleId: number, runId: number, live: readonly number[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const [k, v] of Object.entries(hidden)) if (live.includes(Number(k))) out[k] = v;
  out[scheduleId] = runId;
  return out;
}

/** What a key does to an agent's card: → on a focused row opens it (focus moves in); Esc, or ←
 * on one of its buttons, closes it and puts focus back on the row. */
export function cardKey(key: string, where: "row" | "card", onButton = true): "open" | "close" | null {
  if (where === "row") return key === "ArrowRight" ? "open" : null;
  if (key === "Escape") return "close";
  if (key === "ArrowLeft" && onButton) return "close";
  return null;
}
