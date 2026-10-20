// `pr_overview` app: every open PR of a repo as a card the agent keeps up to date, with sorting,
// a filter and the events that send the user's pick back. Pure, so deno tests cover it.
// docs/MCP.md "Built-in apps" is the contract; src/panel_apps.rs checks the same shape.
import { fits, httpsUrl, Problems, type Result } from "../shared/validate.ts";
import { type CustomAction, validateActions } from "../shared/actions.ts";
import { branchOk, type Method, METHODS, mergeEventFor, REPO, SHA } from "../shared/merge.ts";
import { EMPTY_DRAFT } from "../shared/questions.ts";

export const RISKS = ["low", "medium", "high"] as const;
export const CHECKS = ["passing", "failing", "pending", "none"] as const;
/** gh's `reviewDecision` values; "" (no decision) reads as NONE. */
export const REVIEWS = ["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", "NONE"] as const;
export const STATUSES = ["new", "ready", "reviewing", "merging", "merged", "queued", "held", "skipped", "changes_requested", "failed"] as const;
export type Status = typeof STATUSES[number];
export const SORTS = ["risk", "quick", "oldest"] as const;
export type Sort = typeof SORTS[number];
export const SORT_LABELS: Record<Sort, string> = { risk: "Riskiest first", quick: "Quick wins first", oldest: "Oldest" };
export const MAX_PRS = 200;

export interface Item {
  number: number;
  title: string;
  author: string;
  url?: string;
  created_at?: string;
  /** created_at in ms, when it parses. */
  created?: number;
  draft: boolean;
  additions?: number;
  deletions?: number;
  files?: number;
  checks?: typeof CHECKS[number];
  review?: typeof REVIEWS[number];
  labels: string[];
  risk?: typeof RISKS[number];
  summary?: string;
  status: Status;
  status_detail?: string;
  head_sha?: string;
  base?: string;
  /** The agent ran the full one-snapshot review and found it small, green and low-risk: the
   * card can be merged from here. Such a card has head_sha, base and verdict. */
  quick_win: boolean;
  /** One plain sentence: the agent's call. */
  verdict?: string;
  merge_method_default: Method;
  delete_branch_default: boolean;
}

export interface Overview {
  repo: string;
  title?: string;
  /** The agent's one-sentence finding, under the title. */
  takeaway?: string;
  prs: Item[];
  current?: number;
  sort: Sort;
  actions: CustomAction[];
}

/** What gh prints, and what Muster's backend accepts (`date_ok`). */
const DATE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})$/;
/** A card's `verdict`: one line, as in pr_decision and src/panel_apps.rs. */
export const MAX_VERDICT = 300;

function count(p: Problems, o: Record<string, unknown>, key: string, at: string): number | undefined {
  const v = p.number(o, key, at);
  if (v !== undefined && (!Number.isSafeInteger(v) || v < 0)) { p.add(`${at}.${key}`, "must be a whole number, 0 or more"); return undefined; }
  return v;
}

export function validateOverview(data: unknown): Result<Overview> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["repo", "title", "takeaway", "prs", "current", "sort", "actions"]);
  const repo = p.string(o, "repo", "", { required: true, max: 201 });
  if (repo !== undefined && !REPO.test(repo)) p.add("repo", "must look like owner/name");
  const title = p.string(o, "title", "", { max: 200 });
  const takeaway = p.string(o, "takeaway", "", { max: 600 });
  const prs: Item[] = [];
  const seen = new Set<number>();
  p.array(o, "prs", "", { required: true, max: MAX_PRS })?.forEach((it, i) => {
    const at = `prs[${i}]`;
    const io = p.object(it, at);
    if (!io) return;
    p.known(io, at, ["number", "title", "author", "url", "created_at", "draft", "additions", "deletions", "files", "checks",
      "review", "labels", "risk", "summary", "status", "status_detail", "head_sha", "base", "quick_win", "verdict", "merge_method_default",
      "delete_branch_default"]);
    const number = p.number(io, "number", at, true);
    if (number !== undefined && (!Number.isSafeInteger(number) || number < 1)) { p.add(`${at}.number`, "must be a positive whole number"); return; }
    if (number !== undefined && seen.has(number)) { p.add(`${at}.number`, `#${number} is listed twice`); return; }
    if (number !== undefined) seen.add(number);
    const t = p.string(io, "title", at, { required: true, max: 500 });
    const author = p.string(io, "author", at, { required: true, max: 100 });
    const rawUrl = p.string(io, "url", at, { max: 2000 });
    const url = rawUrl === undefined ? undefined : httpsUrl(rawUrl);
    if (rawUrl !== undefined && !url) p.add(`${at}.url`, "must be an https URL");
    const created_at = p.string(io, "created_at", at, { max: 40 });
    const created = created_at === undefined ? undefined : DATE.test(created_at) ? Date.parse(created_at) : NaN;
    if (created_at !== undefined && !Number.isFinite(created)) p.add(`${at}.created_at`, "must be a date such as 2026-10-18T09:30:00Z");
    const draft = p.boolean(io, "draft", at) ?? false;
    const additions = count(p, io, "additions", at);
    const deletions = count(p, io, "deletions", at);
    const files = count(p, io, "files", at);
    const checks = p.oneOf(io, "checks", at, CHECKS);
    let review: Item["review"];
    if (io.review === "") review = "NONE";
    else review = p.oneOf(io, "review", at, REVIEWS);
    const labels: string[] = [];
    p.array(io, "labels", at, { max: 20 })?.forEach((l, j) => {
      if (typeof l !== "string" || !l.trim() || !fits(l, 100)) p.add(`${at}.labels[${j}]`, "must be a label name (1 to 100 characters)");
      else labels.push(l);
    });
    const risk = p.oneOf(io, "risk", at, RISKS);
    const summary = p.string(io, "summary", at, { max: 500 });
    const status = p.oneOf(io, "status", at, STATUSES) ?? "new";
    const status_detail = p.string(io, "status_detail", at, { max: 500 });
    const head_sha = p.string(io, "head_sha", at, { max: 40 });
    if (head_sha !== undefined && !SHA.test(head_sha)) p.add(`${at}.head_sha`, "must be the full lowercase commit SHA (40 hex characters)");
    const base = p.string(io, "base", at, { max: 255 });
    if (base !== undefined && !branchOk(base)) p.add(`${at}.base`, "is not a valid branch name");
    const quick_win = p.boolean(io, "quick_win", at) ?? false;
    let verdict = p.string(io, "verdict", at, { max: MAX_VERDICT });
    if (verdict !== undefined && /[\r\n]/.test(verdict)) { p.add(`${at}.verdict`, "must be one line"); verdict = undefined; }
    const merge_method_default = p.oneOf(io, "merge_method_default", at, METHODS) ?? "squash";
    const delete_branch_default = p.boolean(io, "delete_branch_default", at) ?? false;
    if (quick_win) {
      // Merge from the overview needs what the merge command and its checks are built from.
      if (io.head_sha === undefined || io.head_sha === null) p.add(`${at}.head_sha`, "is required for a quick win (the head the review saw)");
      if (io.base === undefined || io.base === null) p.add(`${at}.base`, "is required for a quick win (the branch it merges into)");
      if (!verdict?.trim()) p.add(`${at}.verdict`, "is required for a quick win (one plain sentence)");
      if (checks !== "passing") p.add(`${at}.quick_win`, "needs checks passing");
      if (draft) p.add(`${at}.quick_win`, "cannot be a draft");
    }
    if (number !== undefined && t !== undefined && author !== undefined) {
      prs.push({
        number, title: t, author, url, created_at, created: Number.isFinite(created) ? created : undefined, draft, additions, deletions, files,
        checks, review, labels, risk, summary: summary?.trim() ? summary : undefined, status, status_detail, head_sha, base, quick_win,
        verdict: verdict?.trim() ? verdict : undefined, merge_method_default, delete_branch_default,
      });
    }
  });
  const current = p.number(o, "current", "");
  if (current !== undefined && !seen.has(current)) p.add("current", `#${current} is not in prs`);
  const sort = p.oneOf(o, "sort", "", SORTS) ?? "risk";
  const actions = validateActions(p, o);
  return p.result({ repo: repo!, title, takeaway, prs, current, sort, actions });
}

// ----- order, groups and filter -----

const RISK_RANK = { high: 0, medium: 1, low: 2 } as const;
const CHECK_RANK = { failing: 0, pending: 1, none: 2, passing: 2 } as const;

/** Lines changed, when known. */
export const size = (pr: Item) => (pr.additions ?? 0) + (pr.deletions ?? 0);
const known = (pr: Item) => pr.additions !== undefined || pr.deletions !== undefined;

function compare(sort: Sort): (a: Item, b: Item) => number {
  const by = (...keys: ((x: Item) => number)[]) => (a: Item, b: Item) => {
    for (const k of keys) { const d = k(a) - k(b); if (d) return d; }
    return a.number - b.number;
  };
  if (sort === "risk") {
    // High risk first, unknown risk after low; failing checks and bigger PRs break ties.
    return by((x) => x.risk ? RISK_RANK[x.risk] : 3, (x) => x.checks ? CHECK_RANK[x.checks] : 2, (x) => -size(x));
  }
  if (sort === "quick") {
    // Quick wins (reviewed: small, green, low risk) first; then ready to go: not a draft, checks
    // not failing, low risk, small.
    return by((x) => x.quick_win ? 0 : 1, (x) => x.draft ? 1 : 0, (x) => x.checks === "failing" ? 2 : x.checks === "pending" ? 1 : 0,
      (x) => x.risk ? 2 - RISK_RANK[x.risk] : 1, (x) => known(x) ? size(x) : Number.MAX_SAFE_INTEGER);
  }
  return by((x) => x.created ?? Number.MAX_SAFE_INTEGER);
}

export function sortPrs(prs: Item[], sort: Sort): Item[] {
  return [...prs].sort(compare(sort));
}

export type Group = "todo" | "done" | "held";
export const GROUP_LABELS: Record<Group, string> = { todo: "To review", done: "Decided", held: "Held" };

export function groupOf(pr: Item): Group {
  if (pr.status === "held") return "held";
  if (pr.status === "merged" || pr.status === "merging" || pr.status === "queued" || pr.status === "changes_requested") return "done";
  return "todo";
}

/** What the checks and review badges say, so the filter finds what the user sees. */
export const CHECK_WORDS: Record<typeof CHECKS[number], string> = { passing: "checks passing", failing: "checks failing", pending: "checks pending", none: "no checks" };
export const REVIEW_WORDS: Record<typeof REVIEWS[number], string> = { APPROVED: "approved", CHANGES_REQUESTED: "changes requested", REVIEW_REQUIRED: "review required", NONE: "no review" };

/** Every word must match the number, title, author, a label, the summary, the status or a badge. */
export function matches(pr: Item, filter: string): boolean {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return true;
  const hay = [`#${pr.number}`, pr.title, pr.author, ...pr.labels, pr.summary ?? "", pr.status.replace(/_/g, " "),
    pr.draft ? "draft" : "", pr.risk ? `${pr.risk} risk` : "", pr.checks ? CHECK_WORDS[pr.checks] : "",
    pr.review ? REVIEW_WORDS[pr.review] : "", pr.quick_win ? "quick win" : "", pr.verdict ?? ""].join("\n").toLowerCase();
  return words.every((w) => hay.includes(w));
}

/** The shown cards: filtered (only quick wins, if asked), sorted, held ones grouped at the end. */
export function arrange(prs: Item[], sort: Sort, filter: string, quickOnly = false): { group: Group; items: Item[] }[] {
  const shown = sortPrs(prs.filter((pr) => matches(pr, filter) && (!quickOnly || pr.quick_win)), sort);
  return (["todo", "done", "held"] as const).map((group) => ({ group, items: shown.filter((pr) => groupOf(pr) === group) }))
    .filter((g) => g.items.length);
}

/**
 * What "Next" goes through: the PRs still to review (new, ready or skipped) in the shown order,
 * starting after `current` and wrapping, so a skipped PR (status unchanged) is not offered
 * again straight away. Held and decided PRs are left out.
 */
export function queue(ordered: Item[], current?: number): Item[] {
  const at = current === undefined ? -1 : ordered.findIndex((x) => x.number === current);
  const out: Item[] = [];
  for (let i = 1; i <= ordered.length; i++) {
    const pr = ordered[(at + i + ordered.length) % ordered.length];
    if ((pr.status === "new" || pr.status === "ready" || pr.status === "skipped") && pr.number !== current) out.push(pr);
  }
  return out;
}

export const nextPr = (ordered: Item[], current?: number): Item | undefined => queue(ordered, current)[0];

/** "3d", "5h", "12m": how long the PR has been open. */
export function age(created: number | undefined, now: number): string {
  if (created === undefined) return "";
  const m = Math.max(0, Math.floor((now - created) / 60_000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 60) return `${d}d`;
  return `${Math.floor(d / 30)}mo`;
}

/** The one line of counts under the title: what is open, and what needs the user. */
export function summary(prs: Item[]) {
  const open = prs.filter((p) => groupOf(p) !== "done");
  return {
    open: open.length,
    quick: open.filter((p) => p.quick_win).length,
    failing: open.filter((p) => p.checks === "failing").length,
    review: open.filter((p) => p.review === "REVIEW_REQUIRED").length,
    held: prs.filter((p) => p.status === "held").length,
    merged: prs.filter((p) => p.status === "merged").length,
  };
}

/** Counts for the header: open, held and decided. */
export function tally(prs: Item[]): Record<Group, number> {
  const t: Record<Group, number> = { todo: 0, done: 0, held: 0 };
  for (const pr of prs) t[groupOf(pr)]++;
  return t;
}

// ----- events -----

export function openEvent(repo: string, pr: Item) {
  return {
    kind: "open" as const,
    text: `Open PR #${pr.number} (${repo}): review it and show its pr_decision panel.`,
    data: { action: "open" as const, repo, number: pr.number },
  };
}

export function holdEvent(repo: string, pr: Item) {
  return {
    kind: "hold" as const,
    text: `Hold PR #${pr.number} (${repo}): put it aside to come back to later.`,
    data: { action: "hold" as const, repo, number: pr.number },
  };
}

/** `number` is the PR to review now and `order` the ones after it (never `number` again), in
 * the order the user sees (see queue). */
export function nextEvent(repo: string, todo: Item[]) {
  const [pr, ...rest] = todo;
  return {
    kind: "next" as const,
    text: `Go through the PRs in this order: review PR #${pr.number} (${repo}) now${rest.length ? `, then ${rest.map((x) => `#${x.number}`).join(", ")}` : ""}.`,
    data: { action: "next" as const, repo, number: pr.number, order: rest.map((x) => x.number) },
  };
}

/**
 * The selection after an update. While the user is moving through the cards (a card has focus)
 * their pick stays: the agent moving on must not change what Enter or H act on under them.
 * Otherwise the selection follows the agent to its new `current`.
 */
export function followCurrent(selected: number | null, before: number | undefined, after: number | undefined, userOnCard: boolean): number | null {
  if (after === undefined || after === before || userOnCard) return selected;
  return after;
}

/** Whether Hold makes sense for this PR now. */
export const canHold = (pr: Item) => groupOf(pr) === "todo";

// ----- quick wins: merge from the overview -----

/** Statuses from which a quick-win card can be merged here. */
const MERGEABLE: readonly Status[] = ["new", "ready", "skipped", "held", "failed"];

/** Why this card cannot be merged from the overview now, or null. */
export function quickMergeBlocked(pr: Item, sent: boolean): string | null {
  if (!pr.quick_win) return "Not a quick win: open it to review it.";
  if (sent) return "Waiting for the agent.";
  if (!MERGEABLE.includes(pr.status)) return `#${pr.number} is ${pr.status.replace(/_/g, " ")}.`;
  if (pr.checks !== "passing" || pr.draft || !pr.head_sha || !pr.base) return "Not ready to merge: open it to review it.";
  return null;
}

export const quickWins = (prs: Item[]) => prs.filter((p) => p.quick_win && !quickMergeBlocked(p, false));

/** The merge event for a quick-win card: the same shape as pr_decision's, with the card's
 * repo, base and head commit, and nothing riding along. */
export function quickMergeEvent(repo: string, pr: Item) {
  if (!pr.head_sha || !pr.base) throw new Error("a quick win needs head_sha and base");
  return mergeEventFor({ repo, number: pr.number, base: pr.base, head_sha: pr.head_sha }, pr.merge_method_default, pr.delete_branch_default, EMPTY_DRAFT,
    " (a quick win, from the overview)");
}

/** What a quick-win Merge confirm is bound to: any change to the card (or the repo) cancels it. */
export function quickTarget(repo: string, pr: Item): string {
  return JSON.stringify([repo, pr]);
}
