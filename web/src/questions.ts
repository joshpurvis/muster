// Questions agents posted for the human (docs/UI.md "Questions", docs/PROTOCOL.md "Questions").
// The pinned Questions tab lists the open ones and the recently answered ones; the human answers
// with a click on an option or in their own words. `question.list {full}` after each hello, then
// `question_upserted` events. Answering and taking an answer back go through `question.answer` and
// `question.undo`, which the daemon accepts only from a click in this window.
//
// The pure part (ordering, drafts, the words on the page) is questions_model.ts, re-exported here;
// nothing here touches the DOM or the layout.

import { computed, signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import type { Question } from "./types.ts";
import { type Draft, draftFor as draftIn, sortAnswered, sortOpen } from "./questions_model.ts";

export * from "./questions_model.ts";

/** Every question this window knows: the open ones and the newest answered ones, by id. */
export const questions = signal<Map<number, Question>>(new Map());
/** The backend has questions (false: an older one; the tab and its count hide). */
export const questionsSupported = signal(false);
/** The question shown in the tab (null: the first of the list). */
export const selectedQuestion = signal<number | null>(null);
/** Which list the tab shows. */
export const questionsFilter = signal<"open" | "answered">("open");

/** Answered questions loaded with each hello. */
const ANSWERED_LIMIT = 50;

export const openQuestions = computed(() => sortOpen(questions.value.values()));
export const answeredQuestions = computed(() => sortAnswered(questions.value.values()));
/** The count on the tab and in "needs you". */
export const openQuestionCount = computed(() => (questionsSupported.value ? openQuestions.value.length : 0));

export function applyQuestion(q: Question) {
  const next = new Map(questions.value);
  next.set(q.id, q);
  questions.value = next;
}

export async function loadQuestions() {
  try {
    const [open, answered] = await Promise.all([
      rpc.call<{ questions: Question[] }>("question.list", { status: "open", full: true }),
      rpc.call<{ questions: Question[] }>("question.list", { status: "answered", full: true, limit: ANSWERED_LIMIT }),
    ]);
    questions.value = new Map([...open.questions, ...answered.questions].map((q) => [q.id, q]));
    questionsSupported.value = true;
  } catch (e) {
    // An older backend: no questions, or no `full`.
    if (/unknown (method|field)/i.test(e instanceof Error ? e.message : String(e))) questionsSupported.value = false;
  }
}

/** Read question `id` again (whether its answer was read yet is worked out when asked). */
export function refreshQuestion(id: number) {
  rpc.call<Question>("question.get", { id }).then(applyQuestion).catch(() => {});
}

/** An agent message changed (typed in, replied to): the answer it carries may have been read. */
export function refreshForMessage(msg: number) {
  for (const q of questions.value.values()) {
    if (q.answer?.delivery.msg_id === msg) refreshQuestion(q.id);
  }
}

// ---------- the answer being made ----------

/** What the human has picked or typed per question; kept when the tab is left or the page changes. */
export const drafts = signal<Map<number, Draft>>(new Map());

/** The draft of `q` (see `questions_model.ts`), from this window's drafts. */
export function draftFor(q: Question, all: Map<number, Draft> = drafts.value): Draft {
  return draftIn(q, all);
}

export function setDraft(id: number, d: Draft) {
  const next = new Map(drafts.value);
  next.set(id, d);
  drafts.value = next;
}

/** Send the human's answer (it goes after `HOLD_SECONDS`, unless taken back). */
export async function sendAnswer(q: Question, d: Draft): Promise<Question> {
  const pick = q.kind === "own_words" ? [] : d.pick;
  const sent = await rpc.call<Question>("question.answer", { id: q.id, pick, words: d.words.trim() });
  applyQuestion(sent);
  const rest = new Map(drafts.value);
  rest.delete(q.id);
  drafts.value = rest;
  return sent;
}

/** Take the human's answer back: the question is open again, with that answer as its draft. */
export async function undoAnswer(q: Question): Promise<Question> {
  const was = q.answer;
  const back = await rpc.call<Question>("question.undo", { id: q.id });
  applyQuestion(back);
  if (was) setDraft(q.id, { pick: [...was.picked], words: was.words });
  return back;
}

// ---------- pictures ----------

/** A question's pictures, read through `question.picture` when first shown. Key `<id>:<index>`. */
export const pictures = signal<Map<string, { src?: string; error?: string }>>(new Map());

export function loadPicture(id: number, index: number) {
  const key = `${id}:${index}`;
  if (pictures.value.has(key)) return;
  const set = (v: { src?: string; error?: string }) => {
    const next = new Map(pictures.value);
    next.set(key, v);
    pictures.value = next;
  };
  set({});
  rpc.call<{ src: string }>("question.picture", { id, index })
    .then((r) => set({ src: r.src }))
    .catch((e) => set({ error: e instanceof Error ? e.message : String(e) }));
}

