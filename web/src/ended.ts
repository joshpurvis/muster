// "Archived sessions" (docs/UI.md): every closed tab, newest first, across channels and
// accounts. The backend keeps a short record of each (src/model.rs `EndedSession`), never its
// output. This file is the list's logic; components/EndedSessions.tsx draws it.
import { signal } from "@preact/signals";
import type { EndedSession } from "./types.ts";
import { fuzzy } from "./util.ts";

/** The backend keeps closed tabs (its hello says how many). */
export const endedSupported = signal(false);
/** Bumped on every `session_removed`: an open list reloads. */
export const endedRevision = signal(0);

/** The terminal palette's six hues (each theme sets its own, readable on its background), as a
 * channel's tag colour: blue, purple, cyan, green, yellow, red. */
const TAG_COLOURS = ["--ansi-4", "--ansi-5", "--ansi-6", "--ansi-2", "--ansi-3", "--ansi-1"];

/** A channel's tag colour: the same for a channel every time (from its id), as a CSS value. */
export function channelColour(id: number): string {
  const i = ((Math.trunc(id) % TAG_COLOURS.length) + TAG_COLOURS.length) % TAG_COLOURS.length;
  return `var(${TAG_COLOURS[i]})`;
}

/** Does `e` match the filter text (its name, title, channel, agent or folder)? */
export function endedMatches(query: string, e: EndedSession): boolean {
  const q = query.trim();
  if (!q) return true;
  return [e.name, e.title ?? "", e.channel_name, e.agent ?? "", e.cwd].some((s) => s && fuzzy(q, s) !== null);
}

/** Local midnight of `at`'s day. */
function dayStart(at: number): number {
  const d = new Date(at);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** "Today", "Yesterday", "Monday" (this week), else "21 Nov" or "21 Nov 2025". */
export function dayLabel(at: number, now: number): string {
  const days = Math.round((dayStart(now) - dayStart(at)) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  const d = new Date(at);
  if (days < 7) return d.toLocaleDateString("en-GB", { weekday: "long" });
  const same = d.getFullYear() === new Date(now).getFullYear();
  return d.toLocaleDateString("en-GB", { day: "numeric", month: "short", ...(same ? {} : { year: "numeric" }) });
}

/** Rows grouped by the day they closed, in the order given (newest first). */
export function byDay(list: EndedSession[], now: number): { day: string; rows: EndedSession[] }[] {
  const out: { day: string; rows: EndedSession[] }[] = [];
  for (const e of list) {
    const day = dayLabel(e.ended_at, now);
    const last = out[out.length - 1];
    if (last?.day === day) last.rows.push(e);
    else out.push({ day, rows: [e] });
  }
  return out;
}

/** "14:02". */
export function clock(at: number): string {
  return new Date(at).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}

/** "Mon 21 Nov 2026, 14:02". */
export function fullTime(at: number): string {
  const d = new Date(at);
  return `${d.toLocaleDateString("en-GB", { weekday: "short", day: "numeric", month: "short", year: "numeric" })}, ${clock(at)}`;
}

/** "under a minute", "25 min", "2 h 5 min", "3 days 4 h". */
export function duration(ms: number): string {
  const min = Math.floor(Math.max(0, ms) / 60_000);
  if (min < 1) return "under a minute";
  if (min < 60) return `${min} min`;
  const h = Math.floor(min / 60);
  if (h < 24) return min % 60 ? `${h} h ${min % 60} min` : `${h} h`;
  const d = Math.floor(h / 24);
  return `${d} ${d === 1 ? "day" : "days"}${h % 24 ? ` ${h % 24} h` : ""}`;
}

/** How the run ended, in words. */
export function endText(e: EndedSession): string {
  if (e.exit_code === null) return "Still running when the tab closed (closing stopped it)";
  return e.exit_code === 0 ? "Had already finished (exit code 0)" : `Had already stopped (exit code ${e.exit_code})`;
}

/** Whose conversation copy the id finds, or null when no id was kept. */
export function conversationKeeper(e: EndedSession): string | null {
  if (!e.conversation) return null;
  return e.kind === "codex" ? "Codex" : e.kind === "claude" ? "Claude Code" : null;
}
