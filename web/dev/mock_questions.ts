// Dev-only: the mock daemon's questions for the human (docs/PROTOCOL.md "Questions"), in memory.
// Plausible, not exact: an answer from the tab waits `hold_ms` (8 s, as `questions::TAB_HOLD`;
// `mock.question_hold` changes it), then is "delivered": read at once by an idle asker, queued
// (and so still undoable) for a working one. `mock.question_post` and `mock.question_record`
// stand in for the agents' `post_question` and a lead's `record_answer`. dev/mock.ts routes
// `question.*` here. A question posted with `cold_k` has an asker whose conversation is cold and
// that large (thousands of tokens): the tab says the answer starts it in a new conversation, and
// its delivery does (src/relay.rs, the cold-answer rule).

import type { Question } from "../src/types.ts";

export interface QuestionDeps {
  now: () => number;
  broadcast: (event: string, data: unknown) => void;
  /** A mock session's label and state (undefined: none with that id). */
  session: (id: number) => { name: string; state: string; kind: string } | undefined;
}

/** A small picture for questions that carry one: an order card for a made-up bakery, drawn as SVG.
 *  All sample data here is invented; nothing is taken from a real job. */
const CARD_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="920" height="340" viewBox="0 0 920 340">
<rect width="920" height="340" fill="#fffaf2"/><rect x="18" y="18" width="884" height="304" rx="14" fill="none" stroke="#b5651d" stroke-width="4"/>
<text x="58" y="96" font-family="Inter, sans-serif" font-size="34" font-weight="700" fill="#2b1d12">Fresh bread on your doorstep</text>
<text x="58" y="142" font-family="Inter, sans-serif" font-size="17" fill="#4a3a2c">We bake on Thursday night and bring a box of three loaves to your door</text>
<text x="58" y="168" font-family="Inter, sans-serif" font-size="17" fill="#4a3a2c">before 8 on Friday. Skip any week you like.</text>
<rect x="58" y="200" width="132" height="44" rx="6" fill="#f2c46d"/><text x="76" y="228" font-family="Inter, sans-serif" font-size="16" font-weight="600" fill="#2b1d12">Order a box</text>
<text x="58" y="290" font-family="Inter, sans-serif" font-size="14" fill="#6b5a4a">Delivered in town · Pause any week · Loaves change with the season</text></svg>`;

export function questionMock(d: QuestionDeps) {
  const all = new Map<number, Question>();
  let next = 1;
  let holdMs = 8000;
  /** Questions whose asker's conversation is cold and large: its size in thousands of tokens. */
  const coldK = new Map<number, number>();
  const freshWhy = (k: number) => `the old one was cold and large, about ${k}k tokens and idle 3 h`;

  const view = (q: Question): Question => {
    const s = d.session(q.asker.session_id);
    const out: Question = { ...q, number: `Q${q.id}` };
    if (q.status === "open") {
      out.asker_now = s
        ? { session_id: q.asker.session_id, state: s.state as never, running: s.state !== "exited", asleep: false, cold: coldK.has(q.id),
          new_conversation: coldK.has(q.id) && s.state === "idle" ? { why: freshWhy(coldK.get(q.id)!), size_k: coldK.get(q.id)! } : null }
        : { state: "gone", running: false, asleep: false, cold: false };
    }
    return out;
  };
  const publish = (q: Question) => d.broadcast("question_upserted", view(q));
  const get = (id: number) => {
    const q = all.get(Number(id));
    if (!q) throw new Error(`no question Q${id} (see list_questions)`);
    return q;
  };

  function post(p: any): Question {
    const id = p.number ?? next;
    next = Math.max(next, id) + 1;
    const s = d.session(p.asker_session);
    const at = d.now() - (p.age_min ?? 0) * 60_000;
    const q: Question = {
      id, number: `Q${id}`, status: "open", created_at: at, updated_at: at,
      asker: { label: p.asker ?? s?.name ?? `session ${p.asker_session}`, session_id: p.asker_session, kind: (s?.kind ?? "codex") as never,
        ...(p.named ? { agent_id: 1000 + p.asker_session } : {}) },
      product: p.product, question: p.question, context: p.context, example: p.example, kind: p.kind ?? "pick_one",
      options: (p.options ?? []).map((o: any, i: number) => ({ letter: "abcdef"[i], text: o.text, ...(o.why ? { why: o.why } : {}) })),
      pick: p.pick ?? [], if_no_answer: p.if_no_answer, holding_up: p.holding_up, area: p.area ?? "other",
      pictures: p.picture ? [{ path: "/home/user/shots/card.svg", caption: p.picture }] : undefined,
      links: p.links, details: p.details, note_to_self: p.note_to_self ?? "On a: update the card and rebuild the recipe pages.",
    };
    all.set(id, q);
    if (p.cold_k) coldK.set(id, Number(p.cold_k));
    publish(q);
    return view(q);
  }

  function deliver(q: Question) {
    const a = q.answer;
    if (!a || a.delivery.status !== "pending") return;
    const s = d.session(q.asker.session_id);
    const busy = !!s && ["working", "starting", "needs_approval", "asking_question"].includes(s.state);
    const k = busy ? undefined : coldK.get(q.id);
    const started = k ? `started ${q.asker.label} in a new conversation, because ${freshWhy(k)}; ` : "";
    a.delivery = { status: "delivered", to: q.asker.label, msg_id: 9000 + q.id, at: d.now(),
      note: busy ? `queued for ${q.asker.label}: typed in when it is between steps` : `${started}typed into ${q.asker.label}`,
      ...(k ? { fresh: { why: freshWhy(k), replaces: "conv-old" } } : {}) };
    q.can_undo = busy;
    q.read_at = busy ? null : d.now();
    q.updated_at = d.now();
    publish(q);
  }

  return {
    post,
    handle(method: string, p: any): unknown {
      switch (method) {
        case "question.list": {
          const want = p.status ?? "open";
          const qs = [...all.values()].filter((q) => want === "all" || q.status === want).sort((a, b) => a.id - b.id);
          return { questions: qs.map(view), shown: qs.length, total: qs.length };
        }
        case "question.get":
          return view(get(p.id));
        case "question.picture": {
          const q = get(p.id);
          if (!q.pictures?.[p.index]) throw new Error(`Q${q.id} has no picture ${Number(p.index) + 1}`);
          return { src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(CARD_SVG)}`, caption: q.pictures[p.index].caption };
        }
        case "question.answer": {
          const q = get(p.id);
          if (q.status !== "open") throw new Error(`Q${q.id} was answered already`);
          const pick: string[] = p.pick ?? [];
          const words = String(p.words ?? "").trim();
          if (q.kind === "own_words" && !words) throw new Error(`Q${q.id} asks for your own words: type your answer first`);
          if (!pick.length && !words) throw new Error("pick an option or type your answer first");
          q.status = "answered";
          q.answer = { picked: [...pick].sort(), words, how: "tab", at: d.now(),
            delivery: { status: "pending", note: `sent in ${holdMs / 1000} s unless you undo it` } };
          q.can_undo = true;
          q.updated_at = d.now();
          publish(q);
          const at = q.answer.at;
          setTimeout(() => { if (q.answer?.at === at) deliver(q); }, holdMs);
          return view(q);
        }
        case "question.undo": {
          const q = get(p.id);
          if (q.status !== "answered" || !q.answer) throw new Error(`Q${q.id} has no answer to take back`);
          if (!q.can_undo) throw new Error(`${q.asker.label} has read your answer to Q${q.id} already, so it can't be taken back`);
          q.status = "open";
          delete q.answer;
          delete q.can_undo;
          delete q.read_at;
          q.updated_at = d.now();
          publish(q);
          return view(q);
        }
        case "mock.question_post":
          return post(p);
        case "mock.question_hold":
          holdMs = Number(p.ms) || 0;
          return { hold_ms: holdMs };
        case "mock.question_record": {
          // A lead records what the human said in chat.
          const q = get(p.id);
          q.status = "answered";
          q.answer = { picked: p.pick ?? [], words: p.words, how: "chat", at: d.now() - (p.ago_min ?? 0) * 60_000,
            recorded_by: { label: p.by ?? "lead", session_id: 0, kind: "claude" },
            delivery: { status: "delivered", to: q.asker.label, msg_id: 9000 + q.id, note: `typed into ${q.asker.label}` } };
          q.read_at = q.answer.at + 60_000;
          q.can_undo = false;
          publish(q);
          return view(q);
        }
      }
      throw new Error(`unknown method: ${method}`);
    },
  };
}
