// The agent's questions in a PR decision panel, answered in place, and the user's draft:
// answers and switched-on options collect locally and ride along with the next event sent
// (Merge, Request changes, Hold, Skip or Send answers), so one message carries everything.
// Pure, so deno tests cover it. src/panel_apps.rs checks the same input rules.
import { fits, type Problems } from "./validate.ts";
import type { CustomAction } from "./actions.ts";

export const QUESTION_KINDS = ["yes_no", "choice", "text"] as const;
export type QuestionKind = typeof QUESTION_KINDS[number];
export interface QuestionOption { label: string; value: string }
export interface Question {
  id: string;
  text: string;
  why?: string;
  kind: QuestionKind;
  /** Choices for `choice`; empty for the others (yes_no has its own, see optionsOf). */
  options: QuestionOption[];
  /** A value of optionsOf(q) (yes_no: "yes" or "no"), or for `text` a suggested answer. */
  recommended?: string;
  /** Merge waits until it is answered. */
  required: boolean;
}

/** Same limits as src/panel_apps.rs. */
export const MAX_QUESTIONS = 20;
export const MAX_QUESTION = 500;
/** An old plain-string question may be longer. */
export const MAX_LEGACY_QUESTION = 2000;
export const MAX_WHY = 300;
export const MAX_OPTIONS = 9;
export const MAX_OPTION_LABEL = 80;
export const MAX_OPTION_VALUE = 100;
/** A typed answer (and a `text` question's suggested answer). */
export const MAX_ANSWER = 1000;
export const YES_NO: QuestionOption[] = [{ label: "Yes", value: "yes" }, { label: "No", value: "no" }];
const ID = /^[a-z0-9][a-z0-9_-]{0,39}$/;

export const optionsOf = (q: Question): QuestionOption[] => q.kind === "yes_no" ? YES_NO : q.options;

/**
 * `questions`: plain strings (the old form, read as kind "text" with ids q1, q2, … by place) or
 * `{id, text, why?, kind, options?, recommended?, required?}`.
 */
export function validateQuestions(p: Problems, o: Record<string, unknown>): Question[] {
  const out: Question[] = [];
  const ids = new Set<string>();
  p.array(o, "questions", "", { max: MAX_QUESTIONS })?.forEach((v, i) => {
    const at = `questions[${i}]`;
    if (typeof v === "string") {
      if (!fits(v, MAX_LEGACY_QUESTION)) { p.add(at, `is longer than ${MAX_LEGACY_QUESTION} characters`); return; }
      if (!v.trim()) return;
      const id = `q${i + 1}`;
      if (ids.has(id)) { p.add(at, `id "${id}" is used twice`); return; }
      ids.add(id);
      out.push({ id, text: v, kind: "text", options: [], required: false });
      return;
    }
    const qo = p.object(v, at);
    if (!qo) { p.errors[p.errors.length - 1] = `${at}: must be a string or an object`; return; }
    p.known(qo, at, ["id", "text", "why", "kind", "options", "recommended", "required"]);
    const id = p.string(qo, "id", at, { required: true, max: 40 });
    if (id !== undefined) {
      if (!ID.test(id)) { p.add(`${at}.id`, "must be lowercase letters, digits, - or _ (at most 40)"); return; }
      if (ids.has(id)) { p.add(`${at}.id`, `"${id}" is used twice`); return; }
      ids.add(id);
    }
    const text = p.string(qo, "text", at, { required: true, max: MAX_QUESTION });
    const why = p.string(qo, "why", at, { max: MAX_WHY });
    const kind = p.oneOf(qo, "kind", at, QUESTION_KINDS, true);
    const required = p.boolean(qo, "required", at) ?? false;
    const options: QuestionOption[] = [];
    if (kind === "choice") {
      const values = new Set<string>();
      p.array(qo, "options", at, { required: true, max: MAX_OPTIONS, min: 2 })?.forEach((c, j) => {
        const oat = `${at}.options[${j}]`;
        const co = p.object(c, oat);
        if (!co) return;
        p.known(co, oat, ["label", "value"]);
        const label = p.string(co, "label", oat, { required: true, max: MAX_OPTION_LABEL });
        const value = p.string(co, "value", oat, { required: true, max: MAX_OPTION_VALUE });
        if (value !== undefined && values.has(value)) { p.add(`${oat}.value`, `"${value}" is used twice`); return; }
        if (value !== undefined) values.add(value);
        if (label !== undefined && value !== undefined) options.push({ label, value });
      });
    } else if (kind && qo.options !== undefined && qo.options !== null) {
      p.add(`${at}.options`, kind === "yes_no" ? "a yes_no question has its own Yes and No; use kind choice for other options" : "a text question takes no options");
    }
    let recommended: string | undefined;
    if (kind === "text") recommended = p.string(qo, "recommended", at, { max: MAX_ANSWER, nonEmpty: true });
    else if (kind) {
      const r = p.string(qo, "recommended", at, { max: MAX_OPTION_VALUE });
      const allowed = kind === "yes_no" ? YES_NO : options;
      if (r !== undefined && !allowed.some((x) => x.value === r)) {
        p.add(`${at}.recommended`, `must be one of the option values (${allowed.map((x) => JSON.stringify(x.value)).join(", ")})`);
      } else recommended = r;
    }
    if (id !== undefined && text !== undefined && kind) out.push({ id, text, why: why?.trim() ? why : undefined, kind, options, recommended, required });
  });
  return out;
}

// ----- the user's draft -----

/** The user's own picks and typed text, by question id. "" for a text question means the user
 * cleared it: unanswered, not the suggestion. */
export type Choices = Record<string, string>;

export interface Draft {
  /** Question id → answer: an option's value, or the typed text. */
  answers: Record<string, string>;
  /** Ids of the toggle actions switched on. */
  options: string[];
}
export const EMPTY_DRAFT: Draft = { answers: {}, options: [] };

/** A question's answer now: the user's pick, else the recommended one. */
export function answerOf(q: Question, choices: Choices): string | undefined {
  const c = Object.hasOwn(choices, q.id) ? choices[q.id] : undefined;
  if (q.kind === "text") {
    const t = (c ?? q.recommended ?? "").trim();
    return t ? t : undefined;
  }
  const valid = (v: string | undefined) => v !== undefined && optionsOf(q).some((x) => x.value === v);
  if (valid(c)) return c;
  return valid(q.recommended) ? q.recommended : undefined;
}

/** Whether the answer is the agent's recommendation, untouched. */
export function isRecommended(q: Question, choices: Choices): boolean {
  return q.recommended !== undefined && answerOf(q, choices) === (q.kind === "text" ? q.recommended.trim() : q.recommended);
}

export function draftAnswers(questions: Question[], choices: Choices): Record<string, string> {
  const out: Record<string, string> = {};
  for (const q of questions) {
    const a = answerOf(q, choices);
    if (a !== undefined) out[q.id] = a;
  }
  return out;
}

/** Required questions still without an answer (Merge waits for them). */
export const missingRequired = (questions: Question[], choices: Choices) => questions.filter((q) => q.required && answerOf(q, choices) === undefined);

/** Drop picks for questions that are gone, or whose pick is no longer one of its options. */
export function pruneChoices(questions: Question[], choices: Choices): Choices {
  const out: Choices = {};
  for (const q of questions) {
    if (!Object.hasOwn(choices, q.id)) continue;
    const c = choices[q.id];
    if (q.kind === "text" || optionsOf(q).some((x) => x.value === c)) out[q.id] = c;
  }
  return out;
}

/** Number key `digit` (1–9) on a focused question: the value it picks, if any. */
export function valueForDigit(q: Question, digit: string): string | undefined {
  if (!/^[1-9]$/.test(digit)) return undefined;
  return optionsOf(q)[Number(digit) - 1]?.value;
}

/** What an answer reads as: its option's label, or the typed text. */
export function answerLabel(q: Question, value: string): string {
  return optionsOf(q).find((x) => x.value === value)?.label ?? value;
}

/** The toggle actions (mode "toggle") switched on, in the panel's order, as event ids. */
export function draftOptions(actions: CustomAction[], on: ReadonlySet<string>): string[] {
  return actions.filter((a) => a.mode === "toggle" && on.has(a.id)).map((a) => a.id);
}

export const draftEmpty = (d: Draft) => !Object.keys(d.answers).length && !d.options.length;

/** A toggle as the user confirmed it: switching on an outward one was confirmed against this, so
 * any change to it (label, description, outward, mode) switches it off again. */
export const toggleSig = (a: CustomAction) => JSON.stringify([a.id, a.label, a.description, a.outward, a.mode]);

/** The draft for people and for the agent's prompt, compact: ` Answers: [id] label; …
 * Also do, for this PR: id (description); ….` (empty when there is nothing). */
export function draftText(questions: Question[], actions: CustomAction[], d: Draft): string {
  const parts: string[] = [];
  const answered = questions.filter((q) => Object.hasOwn(d.answers, q.id));
  if (answered.length) {
    parts.push(`Answers: ${answered.map((q) => {
      const v = d.answers[q.id];
      return `[${q.id}] ${q.kind === "text" ? JSON.stringify(v) : answerLabel(q, v)}`;
    }).join("; ")}.`);
  }
  const opts = d.options.map((id) => actions.find((a) => a.id === id)).filter((a): a is CustomAction => !!a);
  if (opts.length) parts.push(`Also do, for this PR: ${opts.map((a) => `${a.id} (${a.description})`).join("; ")}.`);
  return parts.length ? ` ${parts.join(" ")}` : "";
}

/** A signature of the draft: a confirm is bound to it, so changing an answer or an option while
 * confirming cancels the confirm. */
export const draftSig = (d: Draft) => JSON.stringify([Object.entries(d.answers).sort(), d.options]);
