// `pr_decision` app: input checks, the merge command and the event payloads. Pure, so deno
// tests cover it. docs/AGENT_PANELS.md "PR decision, end to end" is the contract.
import { fits, httpsUrl, Problems, type Result } from "../shared/validate.ts";
import { Gate } from "../shared/confirm.ts";
import { type CustomAction, validateActions } from "../shared/actions.ts";
import { branchOk, type Method, METHODS, type MergeEvent, mergeCommand, mergeEventFor, REPO, SHA } from "../shared/merge.ts";
import { type Choices, type Draft, draftSig, draftText, EMPTY_DRAFT, missingRequired, type Question, toggleSig, validateQuestions } from "../shared/questions.ts";
import { type ImageInput, validateImage } from "../image/logic.ts";
import { type ChartSpec, validateChart } from "../chart/logic.ts";

export { type Method, METHODS, type MergeEvent, mergeCommand };
export const RISKS = ["low", "medium", "high"] as const;
export const CHECKS = ["passing", "failing", "pending", "none"] as const;
/** `gh pr checks --json bucket` values. */
export const CHECK_ITEMS = ["pass", "fail", "pending", "skipping", "cancel"] as const;
export const STATUSES = ["open", "merging", "queued", "merged", "failed", "requested_changes"] as const;
export type Status = typeof STATUSES[number];

export interface Change { file: string; note: string; diff: string }
/** Pictures and charts shown before the diffs. Muster reads an image `path` into `src`. */
export type Media = { kind: "image"; image: ImageInput } | { kind: "chart"; spec: ChartSpec; caption?: string };
export const MEDIA_KINDS = ["image", "chart"] as const;
/** Same caps as src/panel_apps.rs: items, and image bytes in all. */
export const MAX_MEDIA = 8;
export const MAX_MEDIA_BYTES = 6 * 1024 * 1024;
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
  /** One plain sentence: the agent's call, shown first. */
  verdict?: string;
  /** One line: how to undo it if it goes wrong. */
  undo?: string;
  risk: { level: typeof RISKS[number]; reasons: string[] };
  checks: { state: typeof CHECKS[number]; items: { name: string; state: typeof CHECK_ITEMS[number]; url?: string }[] };
  reviews?: { decision?: string; items: { author: string; state: string }[] };
  changes: Change[];
  rest_note?: string;
  questions: Question[];
  answer?: { question: string; text: string };
  status: Status;
  status_detail?: string;
  media: Media[];
  actions: CustomAction[];
}

/** `verdict` and `undo`: one line each, at most this many characters (as in src/panel_apps.rs). */
export const MAX_LINE = 300;

/** An optional one-line string (no line breaks) of at most `max` characters. */
export function oneLine(p: Problems, o: Record<string, unknown>, key: string, path: string, max: number): string | undefined {
  const v = p.string(o, key, path, { max });
  if (v !== undefined && /[\r\n]/.test(v)) { p.add(path ? `${path}.${key}` : key, "must be one line"); return undefined; }
  return v?.trim() ? v : undefined;
}

export function validatePr(data: unknown): Result<Pr> {
  const p = new Problems();
  const o = p.object(data, "data");
  if (!o) return p.result(null as never);
  p.known(o, "", ["repo", "number", "title", "author", "url", "base", "head", "head_sha", "merge_method_default",
    "delete_branch_default", "summary", "verdict", "undo", "risk", "checks", "reviews", "changes", "rest_note", "questions", "answer", "status", "status_detail",
    "media", "actions"]);
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
  const verdict = oneLine(p, o, "verdict", "", MAX_LINE);
  const undo = oneLine(p, o, "undo", "", MAX_LINE);

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
  const questions = validateQuestions(p, o);
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
  const media = validateMedia(p, o);
  const actions = validateActions(p, o);
  return p.result({
    repo: repo!, number: number!, title: title!, author: author!, url: url!, base: base!, head: head!, head_sha: head_sha!,
    merge_method_default, delete_branch_default, summary: summary!, verdict, undo, risk: risk!, checks: checks!, reviews, changes,
    rest_note, questions, answer, status, status_detail, media, actions,
  });
}

/** Each item is checked by its own app's rules (image, chart), its problems prefixed with its
 * place. Like Muster's backend, media is strict: pictures come from a path only (Muster has read
 * them into `src`) and unknown fields are errors, not warnings. */
const MEDIA_FIELDS = { image: ["kind", "path", "alt", "caption", "src", "mime", "bytes"], chart: ["kind", "spec", "caption"] };

function validateMedia(p: Problems, o: Record<string, unknown>): Media[] {
  const out: Media[] = [];
  let bytes = 0;
  p.array(o, "media", "", { max: MAX_MEDIA })?.forEach((m, i) => {
    const at = `media[${i}]`;
    const mo = p.object(m, at);
    if (!mo) return;
    const kind = p.oneOf(mo, "kind", at, MEDIA_KINDS, true);
    if (!kind) return;
    for (const k of Object.keys(mo)) {
      if (k === "url" && kind === "image") p.add(`${at}.url`, "pictures come from a path only: download it into the task directory");
      else if (!MEDIA_FIELDS[kind].includes(k)) p.add(`${at}.${k}`, "unknown field");
    }
    if (kind === "image") {
      const { kind: _, ...rest } = mo;
      if (rest.url !== undefined) return;
      const r = validateImage(rest);
      if (!r.ok) {
        for (const e of r.errors) p.errors.push(e.startsWith("data: ") ? `${at}: ${e.slice(6)}` : `${at}.${e}`);
        return;
      }
      bytes += r.value.bytes ?? 0;
      out.push({ kind, image: r.value });
    } else {
      const caption = p.string(mo, "caption", at, { max: 2000 });
      if (mo.spec === undefined || mo.spec === null) { p.add(`${at}.spec`, "is required (a chart app spec)"); return; }
      const r = validateChart(mo.spec);
      if (!r.ok) { for (const e of r.errors) p.errors.push(`${at}.spec.${e.replace(/^data: /, "")}`); return; }
      for (const w of r.warnings) p.warnings.push(`${at}.spec.${w}`);
      out.push({ kind, spec: r.value, caption });
    }
  });
  if (bytes > MAX_MEDIA_BYTES) p.add("media", `images total ${bytes} bytes; at most ${MAX_MEDIA_BYTES}`);
  return out;
}

function strings(p: Problems, o: Record<string, unknown>, key: string, path: string, max: number, len: number): string[] {
  const out: string[] = [];
  p.array(o, key, path, { max })?.forEach((v, i) => {
    const at = `${path ? path + "." : ""}${key}[${i}]`;
    if (typeof v !== "string") p.add(at, "must be a string");
    else if (!fits(v, len)) p.add(at, `is longer than ${len} characters`);
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

/** The draft as event text: the answers and the options that ride along. */
const extra = (pr: Pr, d: Draft) => draftText(pr.questions, pr.actions, d);

export function mergeEvent(pr: Pr, method: Method, deleteBranch: boolean, draft: Draft = EMPTY_DRAFT): { kind: "merge"; text: string; data: MergeEvent } {
  return mergeEventFor(pr, method, deleteBranch, draft, extra(pr, draft));
}

export function requestChangesEvent(pr: Pr, body: string, draft: Draft = EMPTY_DRAFT) {
  return {
    kind: "request_changes" as const,
    text: `Request changes on PR #${pr.number} (${pr.repo}).${extra(pr, draft)} The review comment:\n${body}`,
    data: { action: "request_changes" as const, repo: pr.repo, number: pr.number, head_sha: pr.head_sha, body, answers: { ...draft.answers }, options: [...draft.options] },
  };
}

export function askEvent(pr: Pr, question: string) {
  return {
    kind: "ask" as const,
    text: `Question about PR #${pr.number} (${pr.repo}); answer with panel_update answer {question, text}:\n${question}`,
    data: { action: "ask" as const, repo: pr.repo, number: pr.number, question },
  };
}

/** Skip: back to the list (or the next PR) without changing this one. */
export function skipEvent(pr: Pr, draft: Draft = EMPTY_DRAFT) {
  return {
    kind: "skip" as const,
    text: `Skip PR #${pr.number} (${pr.repo}) for now: move on without changing it.${extra(pr, draft)}`,
    data: { action: "skip" as const, repo: pr.repo, number: pr.number, head_sha: pr.head_sha, answers: { ...draft.answers }, options: [...draft.options] },
  };
}

/** Hold: put this PR aside to come back to later. */
export function holdEvent(pr: Pr, draft: Draft = EMPTY_DRAFT) {
  return {
    kind: "hold" as const,
    text: `Hold PR #${pr.number} (${pr.repo}): put it aside to come back to later.${extra(pr, draft)}`,
    data: { action: "hold" as const, repo: pr.repo, number: pr.number, head_sha: pr.head_sha, answers: { ...draft.answers }, options: [...draft.options] },
  };
}

/** Send answers: the draft now, while the user is not deciding yet. */
export function answersEvent(pr: Pr, draft: Draft) {
  return {
    kind: "answers" as const,
    text: `The user's answers on PR #${pr.number} (${pr.repo}); nothing is decided yet.${extra(pr, draft)}`,
    data: { action: "answers" as const, repo: pr.repo, number: pr.number, head_sha: pr.head_sha, answers: { ...draft.answers }, options: [...draft.options] },
  };
}

/** Muster refuses events over 16 KiB of JSON; keep a margin for the envelope. */
export const MAX_EVENT_BYTES = 15 * 1024;

export { eventBytes } from "../shared/validate.ts";

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

/** What a confirmation is bound to: any change to these, to the questions, or to the draft
 * that rides along, cancels it. */
export function confirmTarget(pr: Pr, draft: Draft = EMPTY_DRAFT): string {
  // The questions as defined (ids, kinds, options, required flags, texts) and each option's
  // action as described: a new required question, or a changed option, cancels the confirm.
  const options = draft.options.map((id) => { const a = pr.actions.find((x) => x.id === id); return a ? toggleSig(a) : id; });
  return JSON.stringify([pr.repo, pr.number, pr.base, pr.head_sha, pr.checks.state, pr.merge_method_default, pr.questions, draftSig(draft), options]);
}

/**
 * The second step of Merge, bound to what it merges: any change to `confirmTarget(pr)` cancels
 * it (shared/confirm.ts has the arming rule).
 */
export class ConfirmGate extends Gate {
  open(pr: Pr, draft: Draft = EMPTY_DRAFT) { this.openTarget(confirmTarget(pr, draft)); }

  canConfirm(pr: Pr | null, draft: Draft = EMPTY_DRAFT): boolean { return this.canConfirmTarget(pr ? confirmTarget(pr, draft) : null); }

  input(pr: Pr | null, draft: Draft = EMPTY_DRAFT): boolean { return this.inputTarget(pr ? confirmTarget(pr, draft) : null); }
}

/** "a", "a and b", "a, b and c". */
export function joinAnd(items: string[]): string {
  return items.length < 2 ? items[0] ?? "" : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

export interface Readiness {
  /** Nothing Muster can see stands in the way of a merge. */
  ready: boolean;
  /** The verdict card's colour: Muster's own check, never the agent's opinion. */
  tone: "good" | "warn" | "bad";
  /** Full reasons for the card ("answer question 2", "an approving review is still required on GitHub"). */
  reasons: string[];
  /** Short ones for the line under Merge ("question 2", "an approving review"). */
  short: string[];
}

/**
 * Muster's own merge-readiness check, from what the panel knows: required questions still
 * unanswered, failing or running checks, and the review decision GitHub reports. It colours the
 * verdict card and says why Merge waits, so a green card never sits beside a merge GitHub would
 * refuse. Merge itself still opens its confirm (GitHub has the final say, e.g. an admin merge).
 */
export function readiness(pr: Pr, choices: Choices): Readiness {
  if (pr.status === "merged" || pr.status === "queued") return { ready: true, tone: "good", reasons: [], short: [] };
  const reasons: string[] = [], short: string[] = [];
  let bad = false;
  const missing = missingRequired(pr.questions, choices).map((q) => pr.questions.indexOf(q) + 1);
  if (missing.length) {
    const n = joinAnd(missing.map(String));
    reasons.push(`answer question${missing.length === 1 ? "" : "s"} ${n}`);
    short.push(`question${missing.length === 1 ? "" : "s"} ${n}`);
  }
  if (checksBlock(pr)) { bad = true; reasons.push("checks are failing"); short.push("passing checks"); }
  else if (pr.checks.state === "pending") { reasons.push("checks are still running"); short.push("the running checks"); }
  const decision = pr.reviews?.decision?.toUpperCase();
  if (decision === "CHANGES_REQUESTED") { bad = true; reasons.push("a reviewer asked for changes on GitHub"); short.push("the requested changes"); }
  else if (decision === "REVIEW_REQUIRED") { reasons.push("an approving review is still required on GitHub"); short.push("an approving review"); }
  return { ready: reasons.length === 0, tone: reasons.length === 0 ? "good" : bad ? "bad" : "warn", reasons, short };
}
