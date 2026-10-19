// `pr_decision` app: input checks, the merge command and the event payloads. Pure, so deno
// tests cover it. docs/AGENT_PANELS.md "PR decision, end to end" is the contract.
import { httpsUrl, Problems, type Result } from "../shared/validate.ts";

export const METHODS = ["squash", "merge", "rebase"] as const;
export type Method = typeof METHODS[number];
export const RISKS = ["low", "medium", "high"] as const;
export const CHECKS = ["passing", "failing", "pending", "none"] as const;
/** `gh pr checks --json bucket` values. */
export const CHECK_ITEMS = ["pass", "fail", "pending", "skipping", "cancel"] as const;
export const STATUSES = ["open", "merging", "queued", "merged", "failed", "requested_changes"] as const;
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

const SHA = /^[0-9a-f]{40}$/;
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
  const head_sha = p.string(o, "head_sha", "", { required: true, max: 40 });
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
      if (state) {
        // The badge must never contradict the items: with items, their states decide.
        const derived = deriveChecks(items);
        if (derived && derived !== state) p.warnings.push(`checks.state: says ${state} but the items are ${derived}; showing ${derived}`);
        checks = { state: derived ?? state, items };
      }
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

/** The aggregate of check items: any failure fails, else anything running is pending. */
export function deriveChecks(items: { state: typeof CHECK_ITEMS[number] }[]): typeof CHECKS[number] | null {
  if (!items.length) return null;
  if (items.some((c) => c.state === "fail")) return "failing";
  if (items.some((c) => c.state === "pending")) return "pending";
  return "passing";
}

/** The exact command the agent runs on Merge (docs/AGENT_PANELS.md). `--repo` pins the target
 * even when the agent's environment sets GH_REPO; `--match-head-commit` pins the commit. */
export function mergeCommand(repo: string, number: number, method: Method, deleteBranch: boolean, headSha: string): string {
  if (!REPO.test(repo)) throw new Error("invalid repo");
  if (!Number.isSafeInteger(number) || number < 1) throw new Error("invalid PR number");
  if (!METHODS.includes(method)) throw new Error("invalid merge method");
  if (!SHA.test(headSha)) throw new Error("invalid head SHA");
  return `gh pr merge ${number} --repo ${repo} --${method}${deleteBranch ? " --delete-branch" : ""} --match-head-commit ${headSha}`;
}

export interface MergeEvent {
  action: "merge"; repo: string; number: number; base: string; method: Method; delete_branch: boolean; head_sha: string;
}

export function mergeEvent(pr: Pr, method: Method, deleteBranch: boolean): { kind: "merge"; text: string; data: MergeEvent } {
  const command = mergeCommand(pr.repo, pr.number, method, deleteBranch, pr.head_sha);
  return {
    kind: "merge",
    text: `Merge PR #${pr.number} (${pr.repo}) into ${pr.base}. Run exactly: ${command}`,
    data: { action: "merge", repo: pr.repo, number: pr.number, base: pr.base, method, delete_branch: deleteBranch, head_sha: pr.head_sha },
  };
}

export function requestChangesEvent(pr: Pr, body: string) {
  return {
    kind: "request_changes" as const,
    text: `Request changes on PR #${pr.number} (${pr.repo}) with this review comment:\n${body}`,
    data: { action: "request_changes" as const, repo: pr.repo, number: pr.number, head_sha: pr.head_sha, body },
  };
}

export function askEvent(pr: Pr, question: string) {
  return {
    kind: "ask" as const,
    text: `Question about PR #${pr.number} (${pr.repo}); answer with panel_update answer {question, text}:\n${question}`,
    data: { action: "ask" as const, repo: pr.repo, number: pr.number, question },
  };
}

/** Muster refuses events over 16 KiB of JSON; keep a margin for the envelope. */
export const MAX_EVENT_BYTES = 15 * 1024;

/** UTF-8 size of the `ui/message` params an event becomes (what the backend measures). */
export function eventBytes(event: { kind: string; text: string; data: object }): number {
  const params = { role: "user", content: { type: "text", text: event.text }, _meta: { muster: { kind: event.kind, data: event.data } } };
  return new TextEncoder().encode(JSON.stringify(params)).length;
}

/** Failing checks block Merge unless the user holds Shift. */
export function checksBlock(pr: Pr): boolean {
  return pr.checks.state === "failing" || pr.checks.items.some((c) => c.state === "fail");
}

/** Whether Merge can be started now, and why not. */
export function mergeBlocked(pr: Pr, shift: boolean, sent: boolean): string | null {
  if (pr.status === "merged") return "Already merged";
  if (pr.status === "queued") return "Queued to merge on GitHub";
  if (pr.status === "merging" || sent) return "Waiting for the agent";
  if (checksBlock(pr) && !shift) return "Checks are failing: Shift+M (or Shift+click) merges anyway";
  return null;
}

/** Why Request changes and Ask can no longer be sent, or null while they can. */
export function formsClosed(pr: Pr): string | null {
  if (pr.status === "merged") return "The PR is merged.";
  if (pr.status === "queued") return "The PR is queued to merge.";
  if (pr.status === "requested_changes") return "Changes were requested.";
  return null;
}

/** What a confirmation is bound to: any change to these cancels it. */
export function confirmTarget(pr: Pr): string {
  return JSON.stringify([pr.repo, pr.number, pr.base, pr.head_sha, pr.checks.state, pr.merge_method_default]);
}

/** Keys that are only held to modify another (Shift for "merge anyway"); they never block arming. */
export const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]);

/**
 * The second step of Merge. It is armed only once no key is held, so the key press that opened
 * it, or its auto-repeat, can never also confirm; and any change to what it confirms, or an
 * invalid update, cancels it.
 */
export class ConfirmGate {
  private held = new Set<string>();
  target: string | null = null;
  armed = false;

  /** `code` is the physical key (KeyboardEvent.code), so Shift+M and its "m" keyup match.
   * Returns true when the browser's default action must be prevented (a repeat while open). */
  keyDown(code: string, key: string, repeat: boolean): boolean {
    if (!MODIFIERS.has(key)) this.held.add(code);
    return this.target !== null && repeat;
  }

  keyUp(code: string) {
    this.held.delete(code);
    if (this.target !== null && this.held.size === 0) this.armed = true;
  }

  /** Focus left the app: keyups may never come. */
  blur() {
    this.held.clear();
    if (this.target !== null) this.armed = true;
  }

  open(pr: Pr) {
    this.target = confirmTarget(pr);
    this.armed = this.held.size === 0;
  }

  close() {
    this.target = null;
    this.armed = false;
  }

  get isOpen() { return this.target !== null; }

  /** A confirm may send only when armed and still about the same PR state. */
  canConfirm(pr: Pr | null): boolean {
    return this.target !== null && this.armed && pr !== null && confirmTarget(pr) === this.target;
  }

  /** New input: returns true if it cancelled an open confirm. */
  input(pr: Pr | null): boolean {
    if (this.target === null) return false;
    if (pr && confirmTarget(pr) === this.target) return false;
    this.close();
    return true;
  }
}
