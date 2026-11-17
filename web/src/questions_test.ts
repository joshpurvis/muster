import {
  afterAnswer, ageText, choose, draftFor, howText, pickedText, readyToSend, sendsText, sortAnswered, sortOpen, typeWords,
  whatHappened, whoText,
} from "./questions_model.ts";
import type { Question, SessionInfo } from "./types.ts";

function equal(a: unknown, b: unknown) {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`Expected ${JSON.stringify(b)}, got ${JSON.stringify(a)}`);
}
function assert(value: unknown, message: string) {
  if (!value) throw new Error(message);
}

function q(id: number, over: Partial<Question> = {}): Question {
  return {
    id, number: `Q${id}`, status: "open", created_at: id * 1000, updated_at: id * 1000,
    asker: { label: "writer", session_id: 3, kind: "codex", agent_id: 7 },
    question: "Which heading goes on the sign-up card?", context: "c", example: "e", kind: "pick_one",
    options: [{ letter: "a", text: "Heading one", why: "Approved already." }, { letter: "b", text: "Heading two" }],
    pick: ["a"], if_no_answer: "The build waits.", area: "other", note_to_self: "on a edit cards.md",
    ...over,
  };
}

const session = (state: SessionInfo["state"]) => ({ id: 3, state } as SessionInfo);

Deno.test("open questions: holding up work first, oldest first in each; answered newest first", () => {
  const list = sortOpen([q(1), q(2, { holding_up: "the build" }), q(3), q(4, { holding_up: "a merge" }),
    q(5, { status: "answered" })]);
  equal(list.map((x) => x.id), [2, 4, 1, 3]);
  const at = (n: number) => ({ picked: [], words: "x", how: "tab" as const, at: n, delivery: { status: "pending" as const } });
  equal(sortAnswered([q(1, { status: "answered", answer: at(5) }), q(2, { status: "answered", answer: at(9) }), q(3)])
    .map((x) => x.id), [2, 1]);
});

Deno.test("the draft starts at the asker's pick; picking several ticks and unticks; something must be there to send", () => {
  const one = q(1);
  const d = draftFor(one, new Map());
  equal(d, { pick: ["a"], words: "" });
  assert(readyToSend(one, d), "the asker's pick is ready to send");
  equal(choose(one, d, "b").pick, ["b"]);
  assert(!readyToSend(one, { pick: [], words: "  " }), "nothing picked, no words");
  assert(readyToSend(one, { pick: [], words: "ask the lead" }), "own words alone");
  const many = q(2, { kind: "pick_several", pick: ["a", "b"] });
  equal(choose(many, { pick: ["a", "b"], words: "" }, "a").pick, ["b"]);
  equal(choose(many, { pick: ["b"], words: "" }, "a").pick, ["a", "b"]);
  const own = q(3, { kind: "own_words", options: [], pick: [] });
  equal(draftFor(own, new Map()), { pick: [], words: "" });
  assert(!readyToSend(own, { pick: ["a"], words: "" }), "own words need words");
  equal(pickedText(one, ["b"]), ["b) Heading two"]);
  // Typing answers instead of the preselected pick ("or in your own words"); a click after that adds an option.
  const typed = typeWords(d, "No, hold it");
  equal(typed, { pick: [], words: "No, hold it" });
  equal(sendsText(one, typed), "Sends your words only.");
  const both = choose(one, typed, "b");
  equal(sendsText(one, both), "Sends b) Heading two, with your words.");
  equal(typeWords(both, "No, hold it for now").pick, ["b"]);
  equal(sendsText(one, d), "Sends a) Heading one.");
  // An option's link reads as its label in the preview, as on the card.
  const linked = q(4, { options: [{ letter: "a", text: "Keep [the old card](muster://panel/9)" }, { letter: "b", text: "New" }] });
  equal(sendsText(linked, { pick: ["a"], words: "" }), "Sends a) Keep the old card.");
  // Clicking the chosen option again unchooses it.
  equal(choose(one, d, "a").pick, []);
  equal(whoText(q(4, { passed_on_by: { label: "lead", session_id: 1, kind: "claude" } })), "writer, passed on by lead");
});

Deno.test("after you answer: how the answer will reach the asker as it is now", () => {
  const one = q(1);
  equal(afterAnswer(one, session("working"), false, false).tone, "busy");
  assert(afterAnswer(one, session("working"), false, false).text.includes("when its current step ends"), "working");
  equal(afterAnswer(one, session("idle"), false, false).tone, "warm");
  assert(afterAnswer(one, session("idle"), false, true).text.includes("gone cold"), "cold");
  assert(afterAnswer(one, session("exited"), true, true).text.includes("asleep"), "asleep");
  assert(afterAnswer(one, undefined, false, false).text.includes("starts it and reaches it by name"), "a named agent not running");
  const plain = q(2, { asker: { label: "Claude (fix)", session_id: 9, kind: "claude" } });
  assert(afterAnswer(plain, undefined, false, false).text.includes("reach nobody"), "an ended top-level session");
  const child = q(3, { asker: { label: "Claude (fix)", session_id: 9, kind: "claude", parent_id: 2 } });
  assert(afterAnswer(child, undefined, false, false).text.includes("session that started it"), "an ended plain session");
  equal(afterAnswer(one, session("idle"), false, false, true).tone, "gone");
});

Deno.test("an answer's how and what happened, in plain words", () => {
  const now = new Date(2026, 10, 16, 14, 30).getTime();
  const at = new Date(2026, 10, 16, 14, 2).getTime();
  const base = { picked: ["a"], words: "", at, delivery: { status: "pending" as const } };
  const tab = q(1, { status: "answered", answer: { ...base, how: "tab" } });
  equal(howText(tab, now), "You answered it here, 14:02.");
  assert(whatHappened(tab, now).text.includes("you can undo it"), "pending");
  const chat = q(2, {
    status: "answered",
    answer: { ...base, words: "no price", how: "chat", recorded_by: { label: "lead", session_id: 1, kind: "claude" } },
  });
  equal(howText(chat, now), `You told lead in chat ("no price"); lead recorded it here, 14:02.`);
  const queued = q(3, { status: "answered", can_undo: true, answer: { ...base, how: "tab", delivery: { status: "delivered", to: "writer", msg_id: 4 } } });
  assert(whatHappened(queued, now).text.startsWith("Waiting for writer"), "queued");
  const read = { ...queued, can_undo: false, read_at: at };
  equal(whatHappened(read, now).text, "writer read it at 14:02.");
  const failed = q(4, {
    status: "answered", can_undo: true,
    answer: { ...base, how: "tab", delivery: { status: "refused", to: "writer", note: "not delivered to writer: the daily cap" } },
  });
  equal(whatHappened(failed, now), { warn: true, text: "It didn't reach writer: the daily cap. Undo it to answer again." });
});

Deno.test("ages read like the mock", () => {
  equal([ageText(20_000), ageText(25 * 60_000), ageText(153 * 60_000), ageText(120 * 60_000), ageText(50 * 3600_000)],
    ["just now", "25 min", "2 h 33 min", "2 h", "2 d"]);
});
