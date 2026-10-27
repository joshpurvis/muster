import type {
  AccountProfile,
  AccountUsage,
  ProfileChoice,
  SessionInfo,
  UsageRow,
  UsageWindow,
} from "./types.ts";

export function profileName(profiles: AccountProfile[], id: number | null | undefined): string {
  return id == null ? "Default" : profiles.find((p) => p.id === id)?.name ?? `Unavailable profile #${id}`;
}

/** A new Claude tab/split follows the source Claude session, including its explicit Default.
 * A shell/Codex source leaves selection to the workspace default. */
export function inheritedProfile(kind: string, source: SessionInfo | undefined): ProfileChoice | undefined {
  if (kind !== "claude" || source?.kind !== "claude") return undefined;
  return source.account_profile_id ?? "default";
}

export function parseProfileChoice(value: string): ProfileChoice | undefined {
  if (!value) return undefined;
  if (value === "default") return value;
  const id = Number(value);
  if (!Number.isSafeInteger(id) || id <= 0) throw new Error("Choose an available Claude profile");
  return id;
}

// ---------------- usage (src/usage.rs) ----------------

/** "just now", "4 min ago", "2 h ago", "3 days ago". */
export function formatAge(ms: number, now: number): string {
  const m = Math.max(0, Math.floor((now - ms) / 60_000));
  if (m === 0) return "just now";
  if (m < 60) return `${m} min ago`;
  if (m < 48 * 60) return `${Math.floor(m / 60)} h ago`;
  return `${Math.floor(m / (24 * 60))} days ago`;
}

/** A reset time: "14:05" today, else "Fri 14:05". */
export function formatReset(seconds: number, now: number): string {
  const d = new Date(seconds * 1000);
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
  return d.toDateString() === new Date(now).toDateString()
    ? time
    : `${d.toLocaleDateString([], { weekday: "short" })} ${time}`;
}

export interface WindowCell {
  /** The figure itself: a percentage, or "Unknown". */
  main: string;
  /** What qualifies it: when it resets, or why it does not count. */
  detail: string;
  /** Only a current figure counts; everything else is shown dimmed. */
  current: boolean;
  /** At or above the level where an account is not suggested. */
  high: boolean;
}

const pct = (v: number) => `${Math.round(v)}%`;

/** One usage window as a table cell. Only a current figure reads as a plain percentage. */
export function windowCell(w: UsageWindow, now: number): WindowCell {
  const via = w.reported_by ? ` · via ${w.reported_by}` : "";
  const v = w.used_percentage ?? 0;
  switch (w.status) {
    case "current":
      return {
        main: pct(v),
        detail: `resets ${w.resets_at != null ? formatReset(w.resets_at, now) : "?"}${via}`,
        current: true,
        high: v >= 90,
      };
    case "stale":
      return {
        main: pct(v),
        detail: `stale · seen ${w.observed_at != null ? formatAge(w.observed_at, now) : "long ago"}${via}`,
        current: false,
        high: false,
      };
    case "unconfirmed":
      return { main: `${pct(v)}?`, detail: `not confirmed fresh${via}`, current: false, high: false };
    case "reset":
      return {
        main: "Unknown",
        detail: `reset ${w.resets_at != null ? formatReset(w.resets_at, now) : ""}`.trim() + via,
        current: false,
        high: false,
      };
    default:
      return { main: "Unknown", detail: "", current: false, high: false };
  }
}

/** When the row's account was last seen with newly fetched usage. */
export function lastObserved(row: UsageRow, now: number): string {
  const seen = [row.five_hour.observed_at, row.seven_day.observed_at].filter((t): t is number => t != null);
  if (seen.length) return formatAge(Math.max(...seen), now);
  const reported = [row.five_hour.reported_at, row.seven_day.reported_at].filter((t): t is number =>
    t != null
  );
  if (reported.length) return `reported ${formatAge(Math.max(...reported), now)}, not confirmed`;
  return "Never";
}

export const sameChoice = (a: ProfileChoice | undefined, b: ProfileChoice | undefined) =>
  (a ?? "default") === (b ?? "default");

/** A suggestion older than this is dropped: the window refreshes every minute, so an older one
 * means the backend could not be reached. */
export const SUGGESTION_TTL_MS = 150_000;

/** The snapshot as it stands at `now`, judged from its own timestamps: a window whose reset
 * passed reads as reset, a current one past the stale age as stale, and a suggestion that is
 * too old, or whose account is no longer current, is dropped. So a window that can't refresh
 * never keeps showing an expired figure as current, or a suggestion built on one. */
export function asOf(usage: AccountUsage, now: number): AccountUsage {
  const age = (w: UsageWindow): UsageWindow => {
    if (w.status === "unknown" || w.status === "reset") return w;
    if (w.resets_at != null && w.resets_at * 1000 <= now) return { ...w, status: "reset" };
    if (w.status === "current" && w.observed_at != null && now - w.observed_at > usage.stale_after_ms) {
      return { ...w, status: "stale" };
    }
    return w;
  };
  const rows = usage.rows.map((r) => ({ ...r, five_hour: age(r.five_hour), seven_day: age(r.seven_day) }));
  const s = usage.suggestion;
  if (!s) return { ...usage, rows };
  if (now - usage.evaluated_at > SUGGESTION_TTL_MS) {
    return {
      ...usage,
      rows,
      suggestion: null,
      no_suggestion: "Usage could not be refreshed from Muster, so nothing is suggested.",
    };
  }
  const basis = rows.find((r) => sameChoice(r.choice, s.choice));
  if (!basis || basis.five_hour.status !== "current" || basis.seven_day.status !== "current") {
    return { ...usage, rows, suggestion: null, no_suggestion: `${s.name}'s usage is no longer current.` };
  }
  return { ...usage, rows };
}

export type SuggestionView =
  | { kind: "keep"; name: string; reason: string }
  | { kind: "switch"; name: string; reason: string; choice: ProfileChoice }
  | { kind: "none"; reason: string };

/** How the suggestion reads next to the current selection. It never chooses for the human. */
export function suggestionView(usage: AccountUsage, selected: ProfileChoice | undefined): SuggestionView {
  const s = usage.suggestion;
  if (!s) return { kind: "none", reason: usage.no_suggestion ?? "No suggestion." };
  if (s.choices.some((c) => sameChoice(c, selected))) {
    const name = usage.rows.find((r) => sameChoice(r.choice, selected))?.name ?? s.name;
    return { kind: "keep", name, reason: s.reason };
  }
  return { kind: "switch", name: s.name, reason: s.reason, choice: s.choice };
}
