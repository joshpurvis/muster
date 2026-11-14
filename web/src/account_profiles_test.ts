import {
  accountBars,
  asOf,
  atLimit,
  codexBars,
  formatAge,
  inheritedProfile,
  lastObserved,
  parseProfileChoice,
  peakUsage,
  profileName,
  suggestionView,
  windowCell,
} from "./account_profiles.ts";
import type { AccountProfile, AccountUsage, SessionInfo, UsageRow, UsageWindow } from "./types.ts";

const NOW = Date.UTC(2026, 9, 26, 12, 0, 0);
const win = (w: Partial<UsageWindow>): UsageWindow => ({
  status: "unknown",
  used_percentage: null,
  resets_at: null,
  observed_at: null,
  reported_at: null,
  reported_by: null,
  ...w,
});
const row = (r: Partial<UsageRow>): UsageRow => ({
  choice: "default",
  name: "Default",
  enabled: true,
  eligible: true,
  ineligible_reason: null,
  account: "account-0",
  same_account_as: [],
  five_hour: win({}),
  seven_day: win({}),
  ...r,
});

Deno.test("usage cells never present unknown, stale, unconfirmed or reset figures as current", () => {
  const resets = NOW / 1000 + 3600;
  const current = windowCell(
    win({ status: "current", used_percentage: 92.4, resets_at: resets, observed_at: NOW }),
    NOW,
  );
  equal([current.main, current.current, current.high], ["92%", true, true]);
  equal(current.detail.startsWith("resets "), true);
  equal(windowCell(win({}), NOW), { main: "Unknown", detail: "", current: false, high: false });
  const stale = windowCell(
    win({ status: "stale", used_percentage: 20, resets_at: resets, observed_at: NOW - 2 * 3600_000 }),
    NOW,
  );
  equal([stale.main, stale.detail, stale.current], ["20%", "stale · seen 2 h ago", false]);
  const unconfirmed = windowCell(
    win({ status: "unconfirmed", used_percentage: 20, reported_by: "Work" }),
    NOW,
  );
  equal([unconfirmed.main, unconfirmed.detail, unconfirmed.current], [
    "20%?",
    "not confirmed fresh · via Work",
    false,
  ]);
  const reset = windowCell(win({ status: "reset", used_percentage: 99, resets_at: NOW / 1000 - 60 }), NOW);
  equal([reset.main, reset.current, reset.detail.startsWith("reset ")], ["Unknown", false, true]);
});

Deno.test("last observed prefers confirmed freshness and says when there is none", () => {
  equal(lastObserved(row({}), NOW), "Never");
  equal(
    lastObserved(row({ five_hour: win({ observed_at: NOW - 4 * 60_000, reported_at: NOW }) }), NOW),
    "4 min ago",
  );
  equal(
    lastObserved(row({ seven_day: win({ reported_at: NOW - 60_000 }) }), NOW),
    "reported 1 min ago, not confirmed",
  );
  equal(formatAge(NOW - 3 * 86_400_000, NOW), "3 days ago");
  equal(formatAge(NOW + 5000, NOW), "just now");
});

Deno.test("the suggestion only offers a switch, never makes it", () => {
  const usage: AccountUsage = {
    evaluated_at: NOW,
    stale_after_ms: 1_800_000,
    rows: [
      row({}),
      row({ choice: 2, name: "Work", account: "account-2" }),
      row({ choice: 3, name: "Laptop", account: "account-2" }),
    ],
    suggestion: { choice: 2, name: "Work", choices: [2, 3], reason: "Work used 25%." },
    no_suggestion: null,
  };
  equal(suggestionView(usage, "default"), {
    kind: "switch",
    name: "Work",
    reason: "Work used 25%.",
    choice: 2,
  });
  equal(suggestionView(usage, undefined).kind, "switch");
  equal(suggestionView(usage, 3), { kind: "keep", name: "Laptop", reason: "Work used 25%." });
  equal(
    suggestionView({ ...usage, suggestion: null, no_suggestion: "Unknown is never free." }, 2),
    { kind: "none", reason: "Unknown is never free." },
  );
});

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

Deno.test("Claude splits inherit the source profile even when workspace defaults differ", () => {
  const source = { kind: "claude", account_profile_id: 7 } as SessionInfo;
  equal(inheritedProfile("claude", source), 7);
  equal(inheritedProfile("shell", source), undefined);
  equal(inheritedProfile("codex", source), undefined);
  equal(inheritedProfile("claude", { kind: "codex" } as SessionInfo), undefined);
  equal(inheritedProfile("claude", undefined), undefined);
});

Deno.test("a default-login Claude parent explicitly pins Default instead of inheriting a new workspace default", () => {
  equal(inheritedProfile("claude", { kind: "claude", account_profile_id: null } as SessionInfo), "default");
  equal(inheritedProfile("claude", { kind: "claude" } as SessionInfo), "default");
  equal(parseProfileChoice(""), undefined);
  equal(parseProfileChoice("default"), "default");
  equal(parseProfileChoice("9"), 9);
});

Deno.test("missing or disabled profile bindings stay visible instead of appearing as Default", () => {
  const profiles = [{ id: 4, name: "Work", enabled: false }] as AccountProfile[];
  equal(profileName(profiles, 4), "Work");
  equal(profileName(profiles, 5), "Unavailable profile #5");
  equal(profileName(profiles, null), "Default");
  for (const value of ["0", "-1", "1.5", "NaN", "personal", "9007199254740992"]) {
    let rejected = false;
    try {
      parseProfileChoice(value);
    } catch {
      rejected = true;
    }
    equal(rejected, true);
  }
});

Deno.test("a snapshot that can't be refreshed expires its own figures and suggestion", () => {
  const current = (at: number) =>
    win({
      status: "current",
      used_percentage: 20,
      resets_at: NOW / 1000 + 3600,
      observed_at: at,
      reported_at: at,
    });
  const usage: AccountUsage = {
    evaluated_at: NOW,
    stale_after_ms: 1_800_000,
    rows: [
      row({
        choice: 2,
        name: "Work",
        account: "account-2",
        five_hour: current(NOW),
        seven_day: current(NOW),
      }),
    ],
    suggestion: { choice: 2, name: "Work", choices: [2], reason: "Work used 20%." },
    no_suggestion: null,
  };
  equal(asOf(usage, NOW + 60_000), usage);
  // Past the stale age: stale, and no suggestion built on it.
  const later = asOf({ ...usage, evaluated_at: NOW + 1_790_000 }, NOW + 1_800_001);
  equal([later.rows[0].five_hour.status, later.suggestion], ["stale", null]);
  equal(later.no_suggestion, "Work's usage is no longer current.");
  // Past the reset: reset, whatever it said before.
  equal(asOf(usage, NOW + 3_600_000).rows[0].five_hour.status, "reset");
  // A snapshot nobody could refresh for minutes keeps no suggestion.
  const old = asOf(usage, NOW + 200_000);
  equal([old.suggestion, old.no_suggestion], [
    null,
    "Usage could not be refreshed from Muster, so nothing is suggested.",
  ]);
});

Deno.test("the sidebar shows one entry per account and only colours current figures", () => {
  const resets = NOW / 1000 + 3600;
  const usage: AccountUsage = {
    evaluated_at: NOW,
    stale_after_ms: 600_000,
    suggestion: null,
    no_suggestion: null,
    rows: [
      row({ five_hour: win({ status: "current", used_percentage: 93, resets_at: resets, observed_at: NOW }) }),
      row({ choice: 1, name: "Main", account: "account-0" }),
      row({
        choice: 2,
        name: "Work",
        account: "account-2",
        five_hour: win({ status: "stale", used_percentage: 80, observed_at: NOW - 3_600_000 }),
      }),
    ],
  };
  const bars = accountBars(usage, NOW);
  equal(bars.map((b) => b.name), ["Default · Main", "Work"]);
  const [five0, week0] = bars[0].bars.map((b) => b.bar);
  const five1 = bars[1].bars[0].bar;
  equal([five0.label, five0.level, five0.current], ["93%", "high", true]);
  equal([week0.label, week0.fill], ["–", null]);
  equal([five1.label, five1.level, five1.current], ["80%", "ok", false]);
  if (!five1.tip.includes("stale")) throw new Error(five1.tip);
});

Deno.test("Codex shows its own windows, labelled by length, and ages like Claude's", () => {
  const u = {
    observed_at: NOW - 60_000,
    plan: "pro",
    windows: [
      { window_minutes: 10080, used_percentage: 76, resets_at: NOW / 1000 + 86_400 },
      { window_minutes: 300, used_percentage: 20, resets_at: NOW / 1000 - 1 },
    ],
  };
  const c = codexBars(u, NOW, 600_000)!;
  equal(c.bars.map((b) => [b.label, b.bar.label, b.bar.level]), [["5h", "–", "ok"], ["Wk", "76%", "warn"]]);
  equal(codexBars(u, NOW + 700_000, 600_000)!.bars[1].bar.current, false);
  equal(codexBars(null, NOW, 600_000), null);
});

Deno.test("the collapsed Usage heading shows the fullest current figure, never a stale one", () => {
  const resets = NOW / 1000 + 3600;
  const usage: AccountUsage = {
    evaluated_at: NOW,
    stale_after_ms: 600_000,
    suggestion: null,
    no_suggestion: null,
    rows: [
      row({
        five_hour: win({ status: "current", used_percentage: 40, resets_at: resets, observed_at: NOW }),
        seven_day: win({ status: "current", used_percentage: 78, resets_at: resets, observed_at: NOW }),
      }),
      row({
        choice: 2,
        name: "Work",
        account: "account-2",
        five_hour: win({ status: "stale", used_percentage: 95, observed_at: NOW - 3_600_000 }),
      }),
    ],
  };
  const peak = peakUsage(accountBars(usage, NOW))!;
  equal([peak.name, peak.label, peak.level], ["Default", "78%", "warn"]);
  if (!peak.tip.includes("Default") || !peak.tip.includes("Weekly")) throw new Error(peak.tip);
  equal(peakUsage(accountBars({ ...usage, rows: [usage.rows[1]] }, NOW)), null);
  equal(peakUsage([]), null);
});

Deno.test("an account is at its limit only on a current figure of 100%", () => {
  const usage = (r: Partial<UsageRow>): AccountUsage => ({
    evaluated_at: NOW,
    stale_after_ms: 600_000,
    rows: [row({}), row({ choice: 2, name: "Work", ...r })],
    suggestion: null,
    no_suggestion: null,
  });
  const full = win({ status: "current", used_percentage: 100, resets_at: NOW / 1000 + 3600, observed_at: NOW });
  equal(atLimit(null, 2, NOW), false);
  equal(atLimit(usage({}), 2, NOW), false);
  equal(atLimit(usage({ seven_day: full }), 2, NOW), true);
  equal(atLimit(usage({ seven_day: full }), "default", NOW), false);
  equal(atLimit(usage({ five_hour: { ...full, used_percentage: 99 } }), 2, NOW), false);
  equal(atLimit(usage({ seven_day: { ...full, status: "stale" } }), 2, NOW), false);
  // Its reset passed: the old figure no longer applies.
  equal(atLimit(usage({ seven_day: full }), 2, NOW + 3601_000), false);
});
