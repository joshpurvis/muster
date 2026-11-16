// The pinned Questions tab (docs/UI.md "Questions"): the open questions agents asked the human,
// holding-up-work first, beside the question shown whole (context, example, pictures, lettered
// options with the asker's pick, the human's own words, what happens if nobody answers). Send
// gives the answer; it goes after a few seconds and can be taken back until the asker has read it.
// "Answered" lists the recent answers with how each reached its asker. Below 760 px wide the list
// and the question take turns.
//
// Left for later steps (docs/UI.md): the key sheet and one-key answers, Later and Skip, the
// product and age filters, find, one question at a time, pasting a picture into an answer.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import {
  afterAnswer, ageText, answeredQuestions, choose, clockText, type Draft, draftFor, drafts, HOLD_SECONDS, howText,
  loadPicture, openQuestions, pickedText, pictures, questions, questionsFilter, readyToSend, refreshQuestion, selectedQuestion,
  sendAnswer, sendsText, setDraft, typeWords, undoAnswer, whatHappened, whoText,
} from "../questions.ts";
import type { Question } from "../types.ts";
import { agents, agentsLoaded, dismissToastKey, sessions, toast } from "../state.ts";
import { isAsleep } from "../agents.ts";
import { wakeCold } from "../cache.ts";
import * as A from "../actions.ts";
import { cx } from "../util.ts";

/** Below this width the list and the question take turns (the phone layout of the mock). */
const NARROW_PX = 760;

function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The "sent, Undo" toast of each question, one per answer given (a new answer gets its own). */
const sentToasts = new Map<number, string>();
function dismissSentToast(id: number) {
  const key = sentToasts.get(id);
  if (key) dismissToastKey(key);
  sentToasts.delete(id);
}

/** After the page changed under the keyboard (Send, Undo): back to its top, focus on its question. */
function showPageTop() {
  requestAnimationFrame(() => {
    const page = document.querySelector<HTMLElement>(".questions-view .qv-page");
    page?.scrollTo(0, 0);
    page?.querySelector<HTMLElement>(".qv-q")?.focus({ preventScroll: true });
  });
}

/** The asker's session as this window has it now: a named agent's current one, else the one that asked. */
function askerSession(q: Question) {
  const agent = q.asker.agent_id != null ? agents.value.find((a) => a.id === q.asker.agent_id) : undefined;
  const sid = agent?.session_id ?? q.asker_now?.session_id ?? q.asker.session_id;
  // A named asker this window no longer lists (once the list is loaded) was removed.
  const gone = q.asker.agent_id != null && agentsLoaded.value && !agent;
  return { agent, gone, session: sid != null ? sessions.value.get(sid) : undefined };
}

export function QuestionsView() {
  const ref = useRef<HTMLDivElement>(null);
  const [narrow, setNarrow] = useState(false);
  /** Narrow: the question is shown instead of the list. */
  const [paging, setPaging] = useState(false);
  const [zoomed, setZoomed] = useState<{ src: string; caption: string } | null>(null);
  const now = useNow();

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setNarrow(el.clientWidth < NARROW_PX));
    ro.observe(el);
    setNarrow(el.clientWidth < NARROW_PX);
    return () => ro.disconnect();
  }, []);

  const filter = questionsFilter.value;
  const open = openQuestions.value;
  const answered = answeredQuestions.value;
  const list = filter === "open" ? open : answered;
  const chosen = selectedQuestion.value === null ? undefined : questions.value.get(selectedQuestion.value);
  // The one picked, even just answered (its page shows how it went), else the first of the list.
  const current = chosen && chosen.status !== "withdrawn" ? chosen : list[0];

  const pick = (q: Question) => {
    selectedQuestion.value = q.id;
    if (narrow) setPaging(true);
    requestAnimationFrame(() => ref.current?.querySelector(".qv-page")?.scrollTo(0, 0));
  };
  const showFilter = (f: "open" | "answered") => {
    questionsFilter.value = f;
    const first = (f === "open" ? open : answered)[0];
    selectedQuestion.value = first?.id ?? null;
    setPaging(false);
  };

  return (
    <div class={cx("questions-view", narrow && "narrow", narrow && paging && "paging")} ref={ref} tabIndex={-1}
      aria-label="Questions">
      <div class="qv-bar" role="toolbar" aria-label="Which questions">
        <h1>Questions</h1>
        <button type="button" class={cx("qv-chip", filter === "open" && "on")} aria-pressed={filter === "open"}
          onClick={() => showFilter("open")}>
          Open <span class="qv-n">{open.length}</span>
        </button>
        <button type="button" class={cx("qv-chip", filter === "answered" && "on")} aria-pressed={filter === "answered"}
          onClick={() => showFilter("answered")}>
          Answered <span class="qv-n">{answered.length}</span>
        </button>
      </div>
      <div class="qv-body">
        <QuestionList list={list} filter={filter} current={current} now={now} onPick={pick} />
        <div class="qv-page">
          {current
            ? <QuestionPage key={current.id} q={current} now={now} narrow={narrow} onBack={() => setPaging(false)} onZoom={setZoomed} />
            : (
              <div class="qv-empty qv-empty-page">
                {filter === "open"
                  ? "No open questions. When an agent needs you, its question shows here, and the agent carries on meanwhile."
                  : "Nothing answered yet."}
              </div>
            )}
        </div>
      </div>
      {zoomed && <Lightbox {...zoomed} onClose={() => setZoomed(null)} />}
    </div>
  );
}

function QuestionList({ list, filter, current, now, onPick }: {
  list: Question[]; filter: "open" | "answered"; current: Question | undefined; now: number; onPick: (q: Question) => void;
}) {
  const groups: [string, Question[]][] = filter === "open"
    ? [["Holding up work", list.filter((q) => q.holding_up)], ["Can wait", list.filter((q) => !q.holding_up)]]
    : [["", list]];
  const shown = groups.filter(([, qs]) => qs.length);
  return (
    <div class="qv-list" role="list" aria-label={filter === "open" ? "Open questions" : "Answered questions"}>
      {shown.length === 0 && <div class="qv-empty">{filter === "open" ? "Nothing open." : "Nothing answered yet."}</div>}
      {shown.map(([title, qs]) => (
        <div key={title} role="group" aria-label={title || undefined}>
          {title && <div class="qv-grp">{title}</div>}
          {qs.map((q) => (
            <div key={q.id} role="listitem">
            <button type="button" class={cx("qv-row", q.id === current?.id && "sel", q.status === "answered" && "done")}
              aria-current={q.id === current?.id ? "true" : undefined} onClick={() => onPick(q)}>
              <span class="qv-top"><span class="qv-qn">{q.number}</span><span class="qv-ttl">{q.question}</span></span>
              <span class="qv-sub">
                {q.product && <><span>{q.product}</span><span aria-hidden="true">·</span></>}
                <span>{q.asker.label}</span>
                {q.status === "open" && <><span aria-hidden="true">·</span><span>{ageText(now - q.created_at)}</span></>}
                {q.answer && (
                  <span class="qv-tag ans">
                    {q.answer.picked.length ? `${q.answer.picked.join(", ")} ✓` : "answered ✓"}
                  </span>
                )}
              </span>
            </button>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

function Pictures({ q, onZoom }: { q: Question; onZoom: (p: { src: string; caption: string }) => void }) {
  const list = q.pictures ?? [];
  useEffect(() => list.forEach((_, i) => loadPicture(q.id, i)), [q.id, list.length]);
  if (!list.length) return null;
  return (
    <>
      {list.map((p, i) => {
        const got = pictures.value.get(`${q.id}:${i}`);
        return (
          <figure key={i} class="qv-fig">
            {got?.src
              ? (
                <button type="button" class="qv-shot" aria-label={`Show full size: ${p.caption}`} data-tip="Full size"
                  onClick={() => onZoom({ src: got.src!, caption: p.caption })}>
                  <img src={got.src} alt={p.caption} />
                </button>
              )
              : <div class="qv-shot-missing">{got?.error ? `Can't show this picture: ${got.error}` : "Loading the picture…"}</div>}
            <figcaption>{p.caption}</figcaption>
          </figure>
        );
      })}
    </>
  );
}

function Lightbox({ src, caption, onClose }: { src: string; caption: string; onClose: () => void }) {
  const close = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    // Back to the picture that opened it once it closes.
    const opener = document.activeElement as HTMLElement | null;
    close.current?.focus();
    return () => opener?.isConnected && opener.focus();
  }, []);
  return (
    <div class="qv-lightbox" role="dialog" aria-modal="true" aria-label={caption}
      onClick={onClose}
      onKeyDown={(e) => {
        if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onClose(); }
        // The only control: the keyboard stays on it.
        else if (e.key === "Tab") { e.preventDefault(); close.current?.focus(); }
      }}>
      <img src={src} alt={caption} />
      <div class="qv-lightbox-cap">{caption}</div>
      <button type="button" class="qv-btn qv-lightbox-close" ref={close} onClick={onClose}>Close (Esc)</button>
    </div>
  );
}

function Head({ q, now, narrow, onBack }: { q: Question; now: number; narrow: boolean; onBack: () => void }) {
  return (
    <>
      {narrow && <button type="button" class="qv-back" onClick={onBack}>‹ Questions</button>}
      <div class="qv-meta">
        <span class="qv-qn">{q.number}</span>
        {q.product && <span class="qv-prod">{q.product}</span>}
        <span class="qv-who">{whoText(q)}</span>
        <span>asked {clockText(q.created_at, now)}{q.status === "open" ? `, ${ageText(now - q.created_at)} ago` : ""}</span>
        {q.holding_up && q.status === "open" && <span class="qv-tag block">Holding up work</span>}
      </div>
      {/* Non-breaking hyphens: a balanced title never breaks "sign-up" across lines. */}
      <h2 class="qv-q" tabIndex={-1}>{q.question.replaceAll("-", "\u2011")}</h2>
      <p class="qv-ctx">{q.context}</p>
      <p class="qv-ex"><b>Example</b>{q.example}</p>
    </>
  );
}

function Details({ q }: { q: Question }) {
  return (
    <details class="qv-details">
      <summary>Details</summary>
      {q.details && <p>{q.details}</p>}
      <p class="qv-note-head">{q.asker.label}'s note to itself (it comes back with your answer):</p>
      <p>{q.note_to_self}</p>
      {(q.pictures ?? []).map((p, i) => <p key={i}>Picture {i + 1}: {p.path}</p>)}
      {q.area !== "other" && <p>About: {q.area.replace("_", " ")}</p>}
    </details>
  );
}

function Links({ q }: { q: Question }) {
  const { session } = askerSession(q);
  return (
    <span class="qv-links">
      {(q.links ?? []).map((l, i) => (
        <a key={i} href={l.url} data-tip={l.url}
          onClick={(e) => { e.preventDefault(); globalThis.open(l.url, "_blank", "noopener,noreferrer"); }}>
          {l.label}
        </a>
      ))}
      {session && (
        <button type="button" class="qv-link" onClick={() => A.gotoSession(session.id)}>Open {q.asker.label}'s tab</button>
      )}
    </span>
  );
}

function QuestionPage({ q, now, narrow, onBack, onZoom }: {
  q: Question; now: number; narrow: boolean; onBack: () => void; onZoom: (p: { src: string; caption: string }) => void;
}) {
  return (
    <div class="qv-card">
      <Head q={q} now={now} narrow={narrow} onBack={onBack} />
      <Pictures q={q} onZoom={onZoom} />
      {q.status === "answered" ? <Answered q={q} now={now} /> : <Answering q={q} />}
      <Details q={q} />
    </div>
  );
}

function Answering({ q }: { q: Question }) {
  const d = draftFor(q, drafts.value);
  const [busy, setBusy] = useState(false);
  const { agent, gone, session } = askerSession(q);
  const after = afterAnswer(q, session, isAsleep(agent, session), session ? wakeCold(session, Date.now()) : false, gone);
  const ready = readyToSend(q, d);
  const many = q.kind === "pick_several";
  const update = (next: Draft) => setDraft(q.id, next);

  const send = async () => {
    if (!ready || busy) return;
    setBusy(true);
    const before = openQuestions.value;
    try {
      const sent = await sendAnswer(q, d);
      // On to the next open one; the last one stays, showing how its answer goes.
      const i = before.findIndex((x) => x.id === q.id);
      const next = before.filter((x) => x.id !== q.id)[Math.min(Math.max(i, 0), before.length - 2)];
      selectedQuestion.value = next?.id ?? q.id;
      showPageTop();
      // Its own toast: its Undo takes back this answer, never one given after it.
      dismissSentToast(q.id);
      const key = `q-sent-${q.id}-${sent.answer?.at ?? Date.now()}`;
      sentToasts.set(q.id, key);
      toast("success", `${q.number} goes to ${sent.asker.label} in ${HOLD_SECONDS} seconds`, 9000, key,
        { label: "Undo", run: () => void undo(sent) });
    } catch (e) {
      toast("error", `Couldn't send the answer to ${q.number}: ${errText(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      {q.options.length > 0 && (
        <>
          {many && <p class="qv-hint">Tick any number.</p>}
          <div class={cx("qv-opts", many && "many")} role="group" aria-label="Options">
            {q.options.map((o) => {
              const on = d.pick.includes(o.letter);
              const theirs = q.pick.includes(o.letter);
              return (
                <button key={o.letter} type="button" class={cx("qv-opt", on && "on")} role={many ? "checkbox" : undefined}
                  aria-checked={many ? on : undefined} aria-pressed={many ? undefined : on}
                  onClick={() => update(choose(q, d, o.letter))}>
                  <span class="qv-letter" aria-hidden="true">{o.letter}</span>
                  {many && <span class="qv-box" aria-hidden="true">{on ? "✓" : ""}</span>}
                  <span class="qv-txt">
                    <span class="qv-lbl">{o.text}</span>
                    {o.why && <span class="qv-why">{o.why}</span>}
                  </span>
                  {theirs && !many && <span class="qv-pick">{q.asker.label}'s pick</span>}
                  {theirs && many && <span class="qv-pick">picked by {q.asker.label}</span>}
                </button>
              );
            })}
          </div>
        </>
      )}
      <div class="qv-say">
        <textarea class={cx(q.kind === "own_words" && "big")} rows={q.kind === "own_words" ? 7 : 1}
          aria-label={q.kind === "own_words" ? "Your answer" : "Or answer in your own words"}
          placeholder={q.kind === "own_words" ? "Type or paste your answer" : "Or answer in your own words"}
          value={d.words}
          onInput={(e) => update(typeWords(d, (e.target as HTMLTextAreaElement).value))}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              void send();
            }
          }} />
      </div>
      {q.kind !== "own_words" && sendsText(q, d) && <p class="qv-sends" aria-live="polite">{sendsText(q, d)}</p>}
      <div class="qv-fate">
        <div><span>If you don't answer</span><span>{q.if_no_answer}</span></div>
        {q.holding_up && <div><span>Holding up</span><span>{q.holding_up}</span></div>}
        <div><span>After you answer</span><span class={cx("qv-dot", after.tone)}>{after.text}</span></div>
      </div>
      <div class="qv-acts">
        <button type="button" class="qv-btn primary" disabled={!ready || busy} onClick={() => void send()}
          data-tip={ready ? `Goes to ${q.asker.label} after ${HOLD_SECONDS} seconds; you can undo it until it reads it` : undefined}
          data-keys={ready ? "Ctrl+Enter" : undefined}>
          {busy ? "Sending…" : "Send answer"}
        </button>
        <Links q={q} />
      </div>
    </>
  );
}

async function undo(q: Question) {
  try {
    await undoAnswer(q);
    dismissSentToast(q.id);
    selectedQuestion.value = q.id;
    questionsFilter.value = "open";
    showPageTop();
    toast("info", `${q.number} is open again, with your answer kept to change`, 4000);
  } catch (e) {
    toast("error", `Couldn't take back the answer to ${q.number}: ${errText(e)}`);
  }
}

function Answered({ q, now }: { q: Question; now: number }) {
  const a = q.answer!;
  const happened = whatHappened(q, now);
  const chosen = pickedText(q, a.picked);
  const rest = a.picked.length ? q.options.filter((o) => !a.picked.includes(o.letter)).map((o) => o.text) : [];
  const [busy, setBusy] = useState(false);
  // Until the asker has read it: look again now and then (its delivery has no event of its own
  // when it is typed in, and a session's queue none at all).
  useEffect(() => {
    if (!q.can_undo) return;
    const t = setInterval(() => refreshQuestion(q.id), 5000);
    return () => clearInterval(t);
  }, [q.id, q.can_undo]);
  return (
    <>
      <div class="qv-answered">
        <div class="qv-ah">{a.how === "tab" ? "Your answer" : "The answer"}</div>
        {chosen.map((c) => <div key={c} class="qv-a">{c}</div>)}
        {a.words && <div class={cx("qv-words", !chosen.length && "qv-a")}>{chosen.length ? `“${a.words}”` : a.words}</div>}
      </div>
      {rest.length > 0 && <p class="qv-other">Not chosen: {rest.join(" · ")}</p>}
      <div class="qv-fate">
        <div><span>How</span><span>{howText(q, now)}</span></div>
        <div><span>What happened</span><span class={cx(happened.warn && "qv-warn")}>{happened.text}</span></div>
      </div>
      <div class="qv-acts">
        {q.can_undo && (
          <button type="button" class="qv-btn" disabled={busy}
            onClick={async () => { setBusy(true); await undo(q); setBusy(false); }}>
            Undo
          </button>
        )}
        <Links q={q} />
      </div>
    </>
  );
}
