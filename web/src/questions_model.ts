// The Questions tab's pure part (docs/UI.md "Questions"): ordering, drafts and the words on the
// page, with no signals, RPC or DOM, so tests import it alone. questions.ts re-exports it.

import type { AgentState, Question, SessionInfo } from "./types.ts";

/** How long an answer waits before it is sent (`questions::TAB_HOLD`), for the words on screen. */
export const HOLD_SECONDS = 8;

/** Holding up work first, then the rest; oldest first in each. */
export function sortOpen(qs: Iterable<Question>): Question[] {
  return [...qs].filter((q) => q.status === "open")
    .sort((a, b) => Number(!a.holding_up) - Number(!b.holding_up) || a.created_at - b.created_at || a.id - b.id);
}

/** Newest answer first. */
export function sortAnswered(qs: Iterable<Question>): Question[] {
  return [...qs].filter((q) => q.status === "answered")
    .sort((a, b) => (b.answer?.at ?? b.updated_at) - (a.answer?.at ?? a.updated_at) || b.id - a.id);
}

export interface Draft {
  pick: string[];
  words: string;
}

/** The draft of `q`: what was picked or typed, else the asker's pick, ready to send. */
export function draftFor(q: Question, all: Map<number, Draft>): Draft {
  return all.get(q.id) ?? { pick: q.kind === "own_words" ? [] : [...q.pick], words: "" };
}

/** A click on option `letter`: tick or untick it (pick several), else choose it. */
export function choose(q: Question, d: Draft, letter: string): Draft {
  if (q.kind !== "pick_several") return { ...d, pick: [letter] };
  const pick = d.pick.includes(letter) ? d.pick.filter((l) => l !== letter) : [...d.pick, letter].sort();
  return { ...d, pick };
}

/** Is there something to send? Own words need words; the rest a pick or words. */
export function readyToSend(q: Question, d: Draft): boolean {
  return q.kind === "own_words" ? d.words.trim() !== "" : d.pick.length > 0 || d.words.trim() !== "";
}

/** "just now", "25 min", "2 h 33 min", "3 d". */
export function ageText(ms: number): string {
  const min = Math.floor(Math.max(0, ms) / 60_000);
  if (min < 1) return "just now";
  if (min < 60) return `${min} min`;
  const h = Math.floor(min / 60);
  if (h < 24) return min % 60 ? `${h} h ${min % 60} min` : `${h} h`;
  return `${Math.floor(h / 24)} d`;
}

/** "14:02" today, else "15 Nov 23:10". */
export function clockText(at: number, now: number = Date.now()): string {
  const d = new Date(at);
  const time = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === new Date(now).toDateString()) return time;
  return `${d.getDate()} ${d.toLocaleString("en-GB", { month: "short" })} ${time}`;
}

/** The asker, and the lead that passed it on. */
export function whoText(q: Question): string {
  return q.passed_on_by ? `${q.asker.label}, passed on by ${q.passed_on_by.label}` : q.asker.label;
}

/** "a) Yes, merge it" for each picked letter. */
export function pickedText(q: Question, letters: string[]): string[] {
  return letters.map((l) => {
    const o = q.options.find((x) => x.letter === l);
    return o ? `${l}) ${o.text}` : l;
  });
}

const BUSY: ReadonlySet<AgentState> = new Set(["working", "starting"]);
const PROMPT: ReadonlySet<AgentState> = new Set(["needs_approval", "asking_question"]);

/**
 * "After you answer": where the asker is now, and so how the answer reaches it. `s` is the
 * asker's session as this window has it (live), `asleep` whether that named agent sleeps, `cold`
 * whether its prompt cache has gone cold (the next message re-reads its whole conversation).
 */
export function afterAnswer(q: Question, s: SessionInfo | undefined, asleep: boolean, cold: boolean):
  { tone: "warm" | "busy" | "cold"; text: string } {
  const who = q.asker.label;
  const named = q.asker.agent_id != null;
  if (asleep) {
    return { tone: "cold", text: `${who} is asleep. Your answer wakes it${cold ? "; its memory of this has gone cold, so it re-reads its conversation first" : ""}.` };
  }
  if (!s || s.state === "exited") {
    return named
      ? { tone: "cold", text: `${who} isn't running. Your answer starts it and reaches it by name.` }
      : { tone: "cold", text: "The session that asked has ended. Your answer goes to the session that started it." };
  }
  if (BUSY.has(s.state)) return { tone: "busy", text: `${who} is working. Your answer reaches it when its current step ends.` };
  if (PROMPT.has(s.state)) {
    return { tone: "busy", text: `${who} is waiting on a prompt in its tab. Your answer reaches it once that is dealt with.` };
  }
  if (s.state === "stuck" || s.state === "error") {
    return { tone: "busy", text: `${who} looks stuck. Your answer waits until it is between steps.` };
  }
  if (cold) {
    return { tone: "cold", text: `${who} is idle and its memory of this has gone cold: your answer makes it re-read its conversation first.` };
  }
  return { tone: "warm", text: `${who} is idle. Your answer reaches it as soon as it is sent.` };
}

/** "How": who answered, where and when. */
export function howText(q: Question, now: number = Date.now()): string {
  const a = q.answer;
  if (!a) return "";
  const when = clockText(a.at, now);
  if (a.how === "tab") return `You answered it here, ${when}.`;
  if (a.how === "default") return `Nobody answered by ${when}, so the asker's default was taken.`;
  const by = a.recorded_by?.label ?? "An agent";
  return `You told ${by} in chat ("${a.words}"); ${by} recorded it here, ${when}.`;
}

/** "What happened": where the answer is on its way to the asker. `warn`: it didn't arrive. */
export function whatHappened(q: Question, now: number = Date.now()): { text: string; warn?: boolean } {
  const a = q.answer;
  if (!a) return { text: "" };
  const to = a.delivery.to ?? q.asker.label;
  switch (a.delivery.status) {
    case "pending":
      return { text: `Goes to ${to} in a few seconds. Until then, and until ${to} reads it, you can undo it.` };
    case "delivering":
      return { text: `On its way to ${to} now.` };
    case "delivered":
      if (q.read_at) return { text: `${to} read it at ${clockText(q.read_at, now)}.` };
      if (q.can_undo) return { text: `Waiting for ${to} to finish its current step. Until it reads the answer you can undo it.` };
      return { text: a.delivery.note ? `Delivered: ${a.delivery.note}.` : `Delivered to ${to}.` };
    case "not_needed":
      return { text: a.delivery.note ?? "Nothing to send: the one who recorded it is the asker." };
    case "refused":
    case "undelivered":
      return {
        warn: true,
        text: `It didn't reach ${to}: ${(a.delivery.note ?? "no reason given").replace(/^not delivered( to [^:]+)?: /, "")}.` +
          (q.can_undo ? " Undo it to answer again." : ""),
      };
  }
}
