// `pr_decision` app: input checks, the merge command and the event payloads. Pure, so deno
// tests cover it. docs/AGENT_PANELS.md "PR decision, end to end" is the contract.
import { httpsUrl, Problems, type Result } from "../shared/validate.ts";

export const METHODS = ["squash", "merge", "rebase"] as const;
export type Method = typeof METHODS[number];
export const RISKS = ["low", "medium", "high"] as const;
export const CHECKS = ["passing", "failing", "pending", "none"] as const;
/** `gh pr checks --json bucket` values. */
export const CHECK_ITEMS = ["pass", "fail", "pending", "skipping", "cancel"] as const;
export const STATUSES = ["open", "merging", "merged", "failed", "requested_changes"] as const;
export type Status = typeof STATUSES[number];

export interface Change { file: string; note: string; diff: string }
export interface Pr {
  repo: string;
  number: number;
  title: string;
  author: string;
  url: string;
  base: string;
  head: string;
  head_sha: string;
  merge_method_default: Method;
  delete_branch_default: boolean;
  summary: string;
  risk: { level: typeof RISKS[number]; reasons: string[] };
  checks: { state: typeof CHECKS[number]; items: { name: string; state: typeof CHECK_ITEMS[number]; url?: string }[] };
  reviews?: { decision?: string; items: { author: string; state: string }[] };
  changes: Change[];
  rest_note?: string;
  questions: string[];
  answer?: { question: string; text: string };
  status: Status;
  status_detail?: string;
}

const SHA = /^[0-9a-f]{40}$|^[0-9a-f]{64}$/;
const REPO = /^[A-Za-z0-9_.-]{1,100}\/[A-Za-z0-9_.-]{1,100}$/;
/** Git ref rules that matter here: no spaces, control characters or ~^:?*[\\. */
const branchOk = (b: string) => b.length > 0 && b.length <= 255 && ![...b].some((c) => c.charCodeAt(0) < 33 || c.charCodeAt(0) === 127 || "~^:?*[\\".includes(c));

export function validatePr(data: unknown): Result<Pr> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["repo", "number", "title", "author", "url", "base", "head", "head_sha", "merge_method_default",
    "delete_branch_default", "summary", "risk", "checks", "reviews", "changes", "rest_note", "questions", "answer", "status", "status_detail"]);
  const repo = p.string(o, "repo", "", { required: true, max: 201 });
  if (repo !== undefined && !REPO.test(repo)) p.add("repo", "must look like owner/name");
  const number = p.number(o, "number", "", true);
  if (number !== undefined && (!Number.isSafeInteger(number) || number < 1)) p.add("number", "must be a positive whole number");
  const title = p.string(o, "title", "", { required: true, max: 500 });
  const author = p.string(o, "author", "", { required: true, max: 100 });
  const rawUrl = p.string(o, "url", "", { required: true, max: 2000 });
  const url = rawUrl === undefined ? undefined : httpsUrl(rawUrl);
  if (rawUrl !== undefined && !url) p.add("url", "must be an https URL");
  const base = p.string(o, "base", "", { required: true, max: 255 });
  const head = p.string(o, "head", "", { required: true, max: 255 });
  for (const [k, v] of [["base", base], ["head", head]] as const) if (v !== undefined && !branchOk(v)) p.add(k, "is not a valid branch name");
  const head_sha = p.string(o, "head_sha", "", { required: true, max: 64 });
  if (head_sha !== undefined && !SHA.test(head_sha)) p.add("head_sha", "must be the full lowercase commit SHA (40 hex characters), e.g. headRefOid");
  const merge_method_default = p.oneOf(o, "merge_method_default", "", METHODS) ?? "squash";
  const delete_branch_default = p.boolean(o, "delete_branch_default", "") ?? false;
  const summary = p.string(o, "summary", "", { required: true, max: 20_000 });

  let risk: Pr["risk"] | undefined;
  if (o.risk === undefined) p.add("risk", "is required ({level, reasons})");
  else {
    const ro = p.object(o.risk, "risk");
    if (ro) {
      p.known(ro, "risk", ["level", "reasons"]);
      const level = p.oneOf(ro, "level", "risk", RISKS, true);
      const reasons = strings(p, ro, "reasons", "risk", 20, 2000);
      if (level) risk = { level, reasons };
    }
  }

  let checks: Pr["checks"] | undefined;
  if (o.checks === undefined) p.add("checks", "is required ({state, items})");
  else {
    const co = p.object(o.checks, "checks");
    if (co) {
      p.known(co, "checks", ["state", "items"]);
      const state = p.oneOf(co, "state", "checks", CHECKS, true);
      const items: Pr["checks"]["items"] = [];
      p.array(co, "items", "checks", { max: 200 })?.forEach((it, i) => {
        const io = p.object(it, `checks.items[${i}]`);
        if (!io) return;
        p.known(io, `checks.items[${i}]`, ["name", "state", "url"]);
        const name = p.string(io, "name", `checks.items[${i}]`, { required: true, max: 300 });
        const st = p.oneOf(io, "state", `checks.items[${i}]`, CHECK_ITEMS, true);
        const u = p.string(io, "url", `checks.items[${i}]`, { max: 2000 });
        const link = u === undefined ? undefined : httpsUrl(u);
        if (u !== undefined && !link) p.add(`checks.items[${i}].url`, "must be an https URL");
        if (name !== undefined && st) items.push({ name, state: st, url: link });
      });
      if (state) checks = { state, items };
    }
  }

  let reviews: Pr["reviews"];
  if (o.reviews !== undefined) {
    const vo = p.object(o.reviews, "reviews");
    if (vo) {
      p.known(vo, "reviews", ["decision", "items"]);
      const decision = p.string(vo, "decision", "reviews", { max: 50 });
      const items: { author: string; state: string }[] = [];
      p.array(vo, "items", "reviews", { max: 100 })?.forEach((it, i) => {
        const io = p.object(it, `reviews.items[${i}]`);
        if (!io) return;
        const a = p.string(io, "author", `reviews.items[${i}]`, { required: true, max: 100 });
        const s = p.string(io, "state", `reviews.items[${i}]`, { required: true, max: 50 });
        if (a !== undefined && s !== undefined) items.push({ author: a, state: s });
      });
      reviews = { decision: decision || undefined, items };
    }
  }

  const changes: Change[] = [];
  p.array(o, "changes", "", { required: true, max: 50 })?.forEach((c, i) => {
    const co = p.object(c, `changes[${i}]`);
    if (!co) return;
    p.known(co, `changes[${i}]`, ["file", "note", "diff"]);
    const file = p.string(co, "file", `changes[${i}]`, { required: true, max: 1000 });
    const note = p.string(co, "note", `changes[${i}]`, { required: true, max: 5000 });
    const diff = p.string(co, "diff", `changes[${i}]`, { required: true, max: 100_000 });
    if (file !== undefined && note !== undefined && diff !== undefined) changes.push({ file, note, diff });
  });
  const rest_note = p.string(o, "rest_note", "", { max: 2000 });
  const questions = strings(p, o, "questions", "", 20, 2000);
  let answer: Pr["answer"];
  if (o.answer !== undefined && o.answer !== null) {
    const ao = p.object(o.answer, "answer");
    if (ao) {
      p.known(ao, "answer", ["question", "text"]);
      const q = p.string(ao, "question", "answer", { required: true, max: 8000 });
      const t = p.string(ao, "text", "answer", { required: true, max: 20_000 });
      if (q !== undefined && t !== undefined) answer = { question: q, text: t };
    }
  }
  const status = p.oneOf(o, "status", "", STATUSES) ?? "open";
  const status_detail = p.string(o, "status_detail", "", { max: 4000 });
  return p.result({
    repo: repo!, number: number!, title: title!, author: author!, url: url!, base: base!, head: head!, head_sha: head_sha!,
    merge_method_default, delete_branch_default, summary: summary!, risk: risk!, checks: checks!, reviews, changes,
    rest_note, questions, answer, status, status_detail,
  });
}

function strings(p: Problems, o: Record<string, unknown>, key: string, path: string, max: number, len: number): string[] {
  const out: string[] = [];
  p.array(o, key, path, { max })?.forEach((v, i) => {
    const at = `${path ? path + "." : ""}${key}[${i}]`;
    if (typeof v !== "string") p.add(at, "must be a string");
    else if (v.length > len) p.add(at, `is longer than ${len} characters`);
    else if (v.trim()) out.push(v);
  });
  return out;
}

/** The exact command the agent runs on Merge (docs/AGENT_PANELS.md). */
export function mergeCommand(number: number, method: Method, deleteBranch: boolean, headSha: string): string {
  if (!Number.isSafeInteger(number) || number < 1) throw new Error("invalid PR number");
  if (!METHODS.includes(method)) throw new Error("invalid merge method");
  if (!SHA.test(headSha)) throw new Error("invalid head SHA");
  return `gh pr merge ${number} --${method}${deleteBranch ? " --delete-branch" : ""} --match-head-commit ${headSha}`;
}

export interface MergeEvent { action: "merge"; number: number; method: Method; delete_branch: boolean; head_sha: string }

export function mergeEvent(pr: Pr, method: Method, deleteBranch: boolean): { kind: "merge"; text: string; data: MergeEvent } {
  const command = mergeCommand(pr.number, method, deleteBranch, pr.head_sha);
  return {
    kind: "merge",
    text: `Merge PR #${pr.number} (${pr.repo}). Run exactly: ${command}`,
    data: { action: "merge", number: pr.number, method, delete_branch: deleteBranch, head_sha: pr.head_sha },
  };
}

export function requestChangesEvent(pr: Pr, body: string) {
  return {
    kind: "request_changes" as const,
    text: `Request changes on PR #${pr.number} (${pr.repo}) with this review comment:\n${body}`,
    data: { action: "request_changes" as const, number: pr.number, head_sha: pr.head_sha, body },
  };
}

export function askEvent(pr: Pr, question: string) {
  return {
    kind: "ask" as const,
    text: `Question about PR #${pr.number} (${pr.repo}); answer with panel_update answer {question, text}:\n${question}`,
    data: { action: "ask" as const, number: pr.number, question },
  };
}

/** Failing checks block Merge unless the user holds Shift. */
export function checksBlock(pr: Pr): boolean {
  return pr.checks.state === "failing" || pr.checks.items.some((c) => c.state === "fail");
}

/** Whether Merge can be started now, and why not. */
export function mergeBlocked(pr: Pr, shift: boolean, sent: boolean): string | null {
  if (pr.status === "merged") return "Already merged";
  if (pr.status === "merging" || sent) return "Waiting for the agent";
  if (checksBlock(pr) && !shift) return "Checks are failing: hold Shift to merge anyway";
  return null;
}
