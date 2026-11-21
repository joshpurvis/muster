// The PRs to merge tab's pure part (docs/UI.md "PRs to merge"): what state each card is in, the
// order of the list, the four rules and the words on the page, with no signals, RPC or DOM, so
// tests import it alone. prs.ts re-exports it.

import type { Pr, PrMark, PrSize } from "./types.ts";

/** Seconds between the click on Merge and `pr.merge` (Cancel takes Merge's place meanwhile). */
export const COUNTDOWN_S = 5;
/** Merge ignores clicks this long after its card appears or changes, a Cancel, or a reorder. */
export const CLICK_GUARD_MS = 600;
/** GitHub is read again this often while the tab is shown. */
export const REFRESH_EVERY_MS = 120_000;
/** Merged and closed cards keep the tab in the strip this long. */
export const RECENT_MS = 24 * 3600_000;

/** Where a card stands, as the tab shows it. */
export type CardState =
  | "ready" // every rule passes: Merge is on
  | "reading" // not read from GitHub yet
  | "running" // a check is still running
  | "blocked" // a rule fails (a failed check, a conflict, a draft)
  | "stale" // new commits since it was posted
  | "refused" // the human's last Merge didn't merge
  | "merging"
  | "merged"
  | "closed" // closed on GitHub, or merged there by something else
  | "gone"; // withdrawn, or changes asked

export function cardState(p: Pr): CardState {
  switch (p.status) {
    case "merged": return "merged";
    case "merging": return "merging";
    case "stale": return "stale";
    case "closed": return "closed";
    case "withdrawn": case "changes_asked": return "gone";
  }
  if (p.refused) return "refused";
  if (!p.verdict) return "reading";
  if (p.verdict.ok) return "ready";
  return p.verdict.state !== "fail" && p.verdict.commit === "pass" && p.verdict.base !== "fail" && p.verdict.checks === "wait"
    ? "running" : "blocked";
}

/** Merge is on: the card waits, its last read passes every rule, and no refusal stands. */
export function canMerge(p: Pr): boolean {
  return cardState(p) === "ready";
}

export const shortSha = (sha: string | undefined) => (sha ?? "").slice(0, 7);
export const target = (p: Pr) => `${p.repo} #${p.number}`;
export const repoName = (p: Pr) => p.repo.split("/")[1] ?? p.repo;
export const baseName = (p: Pr) => p.github?.base || "the base branch";
/** The title as GitHub has it, else the repo and number until the first read. */
export const titleOf = (p: Pr) => p.github?.title || target(p);

export type Group = "ready" | "cant" | "later";
export const GROUPS: [Group, string][] = [["ready", "Ready to merge"], ["cant", "Can't merge yet"], ["later", "Not now"]];

export function groupOf(p: Pr): Group {
  if (p.later != null) return "later";
  const s = cardState(p);
  return s === "ready" || s === "merging" || s === "merged" ? "ready" : "cant";
}

const GROUP_ORDER: Record<Group, number> = { ready: 0, cant: 1, later: 2 };

/** The cards waiting for the human (waiting, merging, stale): ready first, then can't merge yet,
 * then set aside; oldest first in each. `kept`: cards merged from this window since the tab was
 * shown, which keep their places (marked merged), so nothing shifts under the pointer. */
export function sortWaiting(ps: Iterable<Pr>, kept?: ReadonlySet<number>): Pr[] {
  return [...ps].filter((p) => p.status === "waiting" || p.status === "merging" || p.status === "stale"
    || (p.status === "merged" && kept?.has(p.id)))
    .sort((a, b) => GROUP_ORDER[groupOf(a)] - GROUP_ORDER[groupOf(b)] || a.created_at - b.created_at || a.id - b.id);
}

/** Merged from here, newest first. */
export function sortMerged(ps: Iterable<Pr>): Pr[] {
  const at = (p: Pr) => p.merge?.done_at ?? p.updated_at;
  return [...ps].filter((p) => p.status === "merged").sort((a, b) => at(b) - at(a) || b.id - a.id);
}

/** The tab belongs in the strip: something waits, or something merged or closed lately. */
export function worthShowing(ps: Iterable<Pr>, now: number): boolean {
  for (const p of ps) {
    if (p.status === "waiting" || p.status === "merging" || p.status === "stale") return true;
    if ((p.status === "merged" || p.status === "closed") && now - p.updated_at < RECENT_MS) return true;
  }
  return false;
}

/** The pill's dot: green ready with checks, a ring for no checks or set aside, blue running, amber
 * new commits, red refused or failed. */
export function dotClass(p: Pr): "" | "none" | "run" | "warn" | "bad" {
  const s = cardState(p);
  if (s === "merged") return "";
  if (p.later != null && s !== "stale" && s !== "refused" && s !== "blocked") return "none";
  switch (s) {
    case "ready": return p.github && p.github.checks.total === 0 ? "none" : "";
    case "running": case "merging": return "run";
    case "stale": return "warn";
    case "refused": case "blocked": return "bad";
    default: return "none";
  }
}

/** The short tag in the "All" list. */
export function tagText(p: Pr): { text: string; tone: "" | "ok" | "warn" | "bad" } {
  const s = cardState(p);
  switch (s) {
    case "merged": return { text: "merged ✓", tone: "ok" };
    case "ready": return { text: p.github && p.github.checks.total === 0 ? "no checks" : "checks passed", tone: "" };
    case "reading": return { text: "reading GitHub", tone: "" };
    case "running": return { text: "checks running", tone: "" };
    case "merging": return { text: "merging", tone: "" };
    case "stale": return { text: "new commits", tone: "warn" };
    case "closed": return { text: "closed", tone: "" };
    case "gone": return { text: "left the list", tone: "" };
    case "refused": case "blocked": {
      const v = p.verdict;
      const why = v?.state === "fail" ? (p.github?.draft ? "draft" : "closed")
        : v?.base === "fail" ? "conflict" : v?.checks === "fail" ? "check failed" : "not merged";
      return { text: why, tone: "bad" };
    }
  }
}

export interface Rule {
  mark: PrMark;
  text: string;
  /** A commit id in it, shown in monospace after `text`. */
  sha?: string;
}

/** The four rules, always these four, in this order (null before the first read). */
export function rules(p: Pr): Rule[] | null {
  const r = p.github, v = p.verdict;
  if (!r || !v) return null;
  const state: Rule = v.state === "pass" ? { mark: "pass", text: "Open, and not a draft" }
    : {
      mark: "fail",
      text: r.incomplete ? "GitHub's reply lacked something Muster checks" : r.state === "MERGED" ? "Already merged"
        : r.state === "CLOSED" ? "Closed on GitHub" : r.draft ? "Still a draft" : r.auto_merge ? "Auto-merge is on for it on GitHub"
        : `GitHub says ${r.state || "nothing"}`,
    };
  const commit: Rule = v.commit === "pass" ? { mark: "pass", text: `The commit ${p.poster.label} posted,`, sha: shortSha(p.head_sha) }
    : { mark: "warn", text: "New commits since posting, now", sha: shortSha(r.head_sha) || "unknown" };
  const base: Rule = v.base === "pass" ? { mark: "pass", text: `Fits onto ${baseName(p)} without conflicts` }
    : v.base === "fail" ? { mark: "fail", text: `Conflicts with ${baseName(p)}` }
    : { mark: "wait", text: `GitHub is still working out whether it fits onto ${baseName(p)}` };
  const c = r.checks;
  const checks: Rule = v.checks === "none" ? { mark: "none", text: "GitHub runs no checks here" }
    : v.checks === "pass" ? { mark: "pass", text: `Checks: ${c.passed} of ${c.total} passed` }
    : v.checks === "wait" ? { mark: "wait", text: `Checks: ${c.passed} of ${c.total} passed, ${c.running} still running` }
    : { mark: "fail", text: `Checks: ${c.failed} of ${c.total} failed${c.failing?.length ? ` (${c.failing.join(", ")})` : ""}` };
  return [state, commit, base, checks];
}

/** Why Merge is off, after "Merge is off: " (null: it is on, or the card is done). */
export function offReason(p: Pr): string | null {
  switch (cardState(p)) {
    case "ready": case "merged": case "closed": case "gone": return null;
    case "reading": return "Muster hasn't read it from GitHub yet";
    case "merging": return "it is being merged";
    case "stale": return "new commits since it was posted";
    case "running": return "a check is still running";
    case "refused":
      return p.refused!.unknown
        ? "Muster doesn't know yet whether the last try merged; Check again"
        : `the last try didn't merge (${p.refused!.reason}); Check again once it is fixed`;
    case "blocked": return p.verdict?.reason ?? "a rule fails";
  }
}

export interface Banner {
  tone: "stale" | "refused" | "wait" | "closed";
  head: string;
  body: string;
}

/** The banner under the title, when the card's state has one. */
export function banner(p: Pr, clock: (at: number) => string): Banner | null {
  const who = p.poster.label;
  switch (cardState(p)) {
    case "stale":
      return {
        tone: "stale", head: "Changed since it was posted: new commits",
        body: `${who} pushed commits after posting this card, so the pictures and words below describe an older version. ` +
          `Merge is off. ${p.stale_told ? `Muster told ${who} to post it again with new pictures.` : ""}`.trim(),
      };
    case "refused": {
      const f = p.refused!;
      const said = f.github_said ? ` GitHub said: “${firstLine(f.github_said)}”` : "";
      const told = f.not_told ? `${who} wasn't told (Muster restarted).` : `${toldText(p, "refused")}.`;
      return f.unknown
        ? {
          tone: "refused", head: `Not known yet: ${f.reason}`,
          body: `You clicked Merge at ${clock(p.merge?.at ?? f.at)}. Muster asked GitHub to merge it but couldn't confirm the ` +
            `result, so it may still merge.${said} Check again shows whether it did. ${told}`,
        }
        : {
          tone: "refused", head: `Not merged: ${f.reason}`,
          body: `You clicked Merge at ${clock(p.merge?.at ?? f.at)}.${said} Nothing was merged. ${told}`,
        };
    }
    case "running":
      return {
        tone: "wait", head: "Can't merge yet: a check is still running",
        body: "Muster looks again every 2 minutes while this tab is open, and Merge comes on once every check has passed.",
      };
    case "blocked":
      return { tone: "refused", head: `Can't merge: ${p.verdict?.reason ?? "a rule fails"}`, body: "The rule that fails is marked below. Nothing was merged." };
    case "closed":
      return { tone: "closed", head: "Closed", body: `This pull request was ${p.closed?.reason ?? "closed on GitHub"}. Nothing was merged from here.` };
    default:
      return null;
  }
}

/** The first line of GitHub's words, without gh's suggestions of other flags. */
export function firstLine(said: string): string {
  return said.split("\n").map((l) => l.trim()).find((l) => l && !l.includes("--admin") && !l.includes("--auto")) ?? "";
}

/** Whether the poster got the card's latest message of `kind`: "builder-2 was told", "… is being
 * told", "Muster couldn't tell builder-2: <why>". */
export function toldText(p: Pr, kind: "merged" | "refused" | "changes_asked" | "stale"): string {
  const who = p.poster.label;
  const n = [...(p.outbox ?? [])].reverse().find((x) => x.kind === kind);
  if (!n) return `${who} was told`;
  switch (n.delivery.status) {
    case "pending": case "delivering": return `${who} is being told`;
    case "refused": case "undelivered":
      return `Muster couldn't tell ${who}${n.delivery.note ? `: ${n.delivery.note.replace(/^not delivered( to [^:]+)?: /, "")}` : ""}`;
    default: return `${who} was told`;
  }
}

/** Hyphens that never break a name ("builder-3") across lines. */
export const nb = (t: string) => t.replaceAll("-", "\u2011");

/** A pill's mark: a glyph and a word, never colour alone. */
export function pillMark(p: Pr): { glyph: string; cls: "" | "none" | "run" | "warn" | "bad"; status: string } {
  const cls = dotClass(p);
  const glyph = { "": "✓", none: "○", run: "…", warn: "!", bad: "✕" }[cls];
  return { glyph, cls, status: p.later != null && cardState(p) !== "merged" ? `not now, ${tagText(p).text}` : tagText(p).text };
}

/** Whether picture `i` is left out for being too tall to read: post_pr marked it (`too_tall`), or
 * `loaded` (what `pr.picture` sent, for a card stored before that rule) says so. */
export type LeftOut = (i: number) => boolean;
export const leftOutOf = (p: Pr, loaded?: LeftOut): LeftOut => (i) => !!p.pictures?.[i]?.too_tall || !!loaded?.(i);

/** The line shown in a left-out picture's place. */
export function leftOutText(side: "before" | "after", caption: string, size: PrSize | undefined): string {
  const px = size ? `${size.width} × ${size.height} px, ` : "";
  return `${side === "before" ? "Before" : "After"} picture left out: “${caption}” is ${px}too tall to read here. ` +
    "A crop of one screen would show.";
}

/** Before and after pictures in pairs, in posting order: the nth before beside the nth after (an
 * index into `p.pictures`, or null where one side has none). */
export function pairs(p: Pr): { before: number | null; after: number | null }[] {
  const list = p.pictures ?? [];
  const before = list.flatMap((x, i) => (x.side === "before" ? [i] : []));
  const after = list.flatMap((x, i) => (x.side === "after" ? [i] : []));
  return Array.from({ length: Math.max(before.length, after.length) }, (_, i) => ({ before: before[i] ?? null, after: after[i] ?? null }));
}

/** The large view's order: before 1, after 1, before 2, after 2… */
export function viewOrder(p: Pr, out: LeftOut = leftOutOf(p)): number[] {
  return pairs(p).flatMap((x) => [x.before, x.after].filter((i): i is number => i !== null && !out(i)));
}

/** The picture the large view steps to from picture `at` (`dir` 1 or -1, wrapping), skipping
 * left-out ones. `at` may be left out itself: an old card's picture that turned out too tall once
 * the large view had stepped onto it. */
export function stepPicture(p: Pr, at: number, dir: 1 | -1, out: LeftOut = leftOutOf(p)): number {
  const all = pairs(p).flatMap((x) => [x.before, x.after].filter((i): i is number => i !== null));
  const i = all.indexOf(at);
  if (i < 0) return at;
  for (let k = 1; k <= all.length; k++) {
    const next = all[(i + dir * k + all.length * k) % all.length];
    if (!out(next)) return next;
  }
  return at;
}

/** The other side of picture `at`'s pair (null: it stands alone, or the other side is left out). */
export function otherSide(p: Pr, at: number, out: LeftOut = leftOutOf(p)): number | null {
  const pair = pairs(p).find((x) => x.before === at || x.after === at);
  if (!pair) return null;
  const other = pair.before === at ? pair.after : pair.before;
  return other === null || out(other) ? null : other;
}

/** "pair 1 of 2". */
export function pairText(p: Pr, at: number): string {
  const all = pairs(p);
  const i = all.findIndex((x) => x.before === at || x.after === at);
  return `pair ${i + 1} of ${all.length}`;
}

/** "4 files · +86 −12" parts, or null before the first read. */
export function sizeOf(p: Pr): { files: string; add: string; del: string } | null {
  const r = p.github;
  if (!r) return null;
  return { files: `${r.files} ${r.files === 1 ? "file" : "files"}`, add: `+${r.additions}`, del: `−${r.deletions}` };
}

/** The next card ready to merge after `p` (for "Next waiting"). */
export function nextReady(list: Pr[], p: Pr): Pr | undefined {
  return list.find((x) => x.id !== p.id && canMerge(x) && x.later == null) ?? list.find((x) => x.id !== p.id && canMerge(x));
}

/** The exact command Muster runs for this card (shown in Details). */
export function mergeCommand(p: Pr): string {
  return `gh pr merge ${p.number} --repo ${p.repo} --merge --match-head-commit ${p.head_sha}`;
}
