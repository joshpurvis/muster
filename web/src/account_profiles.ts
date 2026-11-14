import type {
  AccountProfile,
  AccountUsage,
  CodexUsage,
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

/** Is `choice`'s account at a limit now: a current 5-hour or weekly figure at 100% (src/usage.rs
 * `UsageRow::at_limit`)? Unknown or stale usage is not. */
export function atLimit(usage: AccountUsage | null, choice: ProfileChoice, now: number): boolean {
  if (!usage) return false;
  const row = asOf(usage, now).rows.find((r) => (r.choice ?? "default") === choice);
  return !!row && [row.five_hour, row.seven_day].some((w) => w.status === "current" && (w.used_percentage ?? 0) >= 100);
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
/** A window as it stands at `now`: a passed reset reads as reset, an old current figure as stale. */
export function ageWindow(w: UsageWindow, now: number, staleAfterMs: number): UsageWindow {
  if (w.status === "unknown" || w.status === "reset") return w;
  if (w.resets_at != null && w.resets_at * 1000 <= now) return { ...w, status: "reset" };
  if (w.status === "current" && w.observed_at != null && now - w.observed_at > staleAfterMs) {
    return { ...w, status: "stale" };
  }
  return w;
}

export function asOf(usage: AccountUsage, now: number): AccountUsage {
  const age = (w: UsageWindow) => ageWindow(w, now, usage.stale_after_ms);
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

export interface UsageBar {
  /** 0–100 for the bar's fill; null when nothing is known. */
  fill: number | null;
  /** The figure beside the bar: "42%", "42%?" or "–". */
  label: string;
  /** The full story, for the tooltip and screen readers. */
  tip: string;
  /** Only a current figure is drawn in colour. */
  current: boolean;
  level: "ok" | "warn" | "high";
}

export interface AccountBars {
  key: string;
  tool: "claude" | "codex";
  /** Every profile on this account, Default first; "Codex" for Codex. */
  name: string;
  /** Shortest window first: "5h", "Wk". */
  bars: { label: string; bar: UsageBar }[];
}

function bar(title: string, w: UsageWindow, now: number): UsageBar {
  const cell = windowCell(w, now);
  const known = w.used_percentage != null && w.status !== "unknown" && w.status !== "reset";
  const v = known ? Math.max(0, Math.min(100, w.used_percentage!)) : null;
  return {
    fill: v,
    label: known ? cell.main : "–",
    tip: `${title}: ${cell.main}${cell.detail ? ` · ${cell.detail}` : ""}`,
    current: cell.current,
    level: v == null || !cell.current ? "ok" : v >= 90 ? "high" : v >= 75 ? "warn" : "ok",
  };
}

/** One entry per account (profiles on one login share its limits), in the snapshot's order. */
export function accountBars(usage: AccountUsage, now: number): AccountBars[] {
  const out = new Map<string, { rows: UsageRow[] }>();
  for (const r of usage.rows) {
    const g = out.get(r.account);
    if (g) g.rows.push(r);
    else out.set(r.account, { rows: [r] });
  }
  return [...out.entries()].map(([key, { rows }]) => {
    // Rows of one account carry the same observation; prefer one a usable profile shows.
    const r = rows.find((x) => x.eligible) ?? rows[0];
    return {
      key,
      tool: "claude" as const,
      name: rows.map((x) => x.name).join(" · "),
      bars: [
        { label: "5h", bar: bar("5-hour", r.five_hour, now) },
        { label: "Wk", bar: bar("Weekly", r.seven_day, now) },
      ],
    };
  });
}

/** A window length as a bar label and a tooltip title: 300 → "5h"/"5-hour", 10080 → "Wk"/"Weekly". */
export function windowName(minutes: number): { label: string; title: string } {
  if (minutes === 10080) return { label: "Wk", title: "Weekly" };
  if (minutes % 1440 === 0) return { label: `${minutes / 1440}d`, title: `${minutes / 1440}-day` };
  const h = Math.round(minutes / 6) / 10;
  return { label: `${h}h`, title: `${h}-hour` };
}

/** Codex's single account, aged like Claude's figures. Null when nothing is known. */
export function codexBars(u: CodexUsage | null, now: number, staleAfterMs: number): AccountBars | null {
  if (!u?.windows.length) return null;
  const bars = [...u.windows].sort((a, b) => a.window_minutes - b.window_minutes).map((w) => {
    const n = windowName(w.window_minutes);
    const win = ageWindow({
      status: "current",
      used_percentage: w.used_percentage,
      resets_at: w.resets_at,
      observed_at: u.observed_at,
      reported_at: u.observed_at,
      reported_by: null,
    }, now, staleAfterMs);
    return { label: n.label, bar: bar(n.title, win, now) };
  });
  return { key: "codex", tool: "codex", name: "Codex", bars };
}

export interface UsagePeak {
  /** The account it belongs to. */
  name: string;
  /** "82%". */
  label: string;
  level: UsageBar["level"];
  /** Account and the figure's full story, for the tooltip and screen readers. */
  tip: string;
}

/** The fullest current figure across all accounts: what a collapsed Usage heading shows. A
 * figure that is not current never counts; null when there is none. */
export function peakUsage(accounts: AccountBars[]): UsagePeak | null {
  let best: UsagePeak | null = null;
  let max = -1;
  for (const a of accounts) {
    for (const { bar: b } of a.bars) {
      if (!b.current || b.fill == null || b.fill <= max) continue;
      max = b.fill;
      best = { name: a.name, label: b.label, level: b.level, tip: `Highest now: ${a.name} · ${b.tip}` };
    }
  }
  return best;
}
