// The sidebar's one list (docs/UI.md "Channels sidebar"): each channel in its saved order, with
// the named agent that owns it, agents whose channel isn't in this window after them, the
// "Needs you" box, and what each agent's row flags. Pure functions, tested in sidebar_test.ts.
import type { Agent, Channel, ChannelId, Schedule } from "./types.ts";
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
  return !!r && !isProposal(s) && outcomeTone(r.outcome) === "bad" && hidden[s.id] !== r.id;
}

export type NeedsItem = { kind: "proposal"; schedule: Schedule } | { kind: "run"; schedule: Schedule };

/** What the "Needs you" box holds: schedules waiting for approval, oldest first, then schedules
 * whose last run failed, newest failure first. Empty: the box isn't shown. */
export function needsYouItems(schedules: Schedule[], hidden: HiddenRuns): NeedsItem[] {
  const props = waitingProposals(schedules).map((s): NeedsItem => ({ kind: "proposal", schedule: s }));
  const runs = schedules.filter((s) => failedRun(s, hidden))
    .sort((a, b) => (b.last_run!.started_at - a.last_run!.started_at) || a.id - b.id)
    .map((s): NeedsItem => ({ kind: "run", schedule: s }));
  return [...props, ...runs];
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
  const n = (k: number, one: string, many: string) => `${k} ${k === 1 ? one : many}`;
  if (waiting || failed) {
    const parts = [
      waiting && `${n(waiting, "schedule waits", "schedules wait")} for your approval`,
      failed && `${n(failed, "schedule's", "schedules'")} last run failed`,
    ].filter(Boolean);
    return { tone: failed ? "bad" : "wait", count: waiting + failed, label: parts.join("; ") };
  }
  return { tone: "plain", count: own.length, label: n(own.length, "schedule", "schedules") };
}

/** Hide a schedule's failed run; entries of schedules that are gone are dropped. */
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
