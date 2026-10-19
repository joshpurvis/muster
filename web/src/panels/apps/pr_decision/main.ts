// The PR decision panel: show what the user needs to decide, then send exactly one decision.
// Every event is sent synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems, reveal, trapTab } from "../shared/dom.ts";
import { diffStats, parseDiff } from "../shared/diff.ts";
import { Gate } from "../shared/confirm.ts";
import { actionForKey, type CustomAction, customEvent, outwardTarget } from "../shared/actions.ts";
import {
  answerLabel, answerOf, type Choices, type Draft, draftAnswers, draftEmpty, draftOptions, draftSig, isRecommended, MAX_ANSWER,
  missingRequired, optionsOf, pruneChoices, type Question, toggleSig, valueForDigit,
} from "../shared/questions.ts";
import { actionsGroup, helpButton, outwardDrawer, shortcutsDrawer } from "../shared/actions_view.ts";
import { renderChart } from "../chart/view.ts";
import { formatBytes } from "../image/logic.ts";
import {
  answersEvent, askEvent, checksBlock, ConfirmGate, eventBytes, formsClosed, holdEvent, MAX_EVENT_BYTES, type Method, mergeBlocked, mergeCommand,
  mergeEvent, METHODS, type Pr, requestChangesEvent, skipEvent, validatePr,
} from "./logic.ts";

type Mode = null | "confirm" | "request" | "ask" | "outward";
type Tone = "info" | "danger" | "warning" | "success";
const root = document.getElementById("app")!;
const MAX_DIFF_LINES = 400;

let pr: Pr | null = null;
let warnings: string[] = [];
let identity = "";
let method: Method = "squash";
let deleteBranch = false;
let mode: Mode = null;
const gate = new ConfirmGate();
/** The outward custom action being confirmed (mode "outward"), and its own armed gate. */
let outward: CustomAction | null = null;
const outGate = new Gate();
/** Custom actions sent since the last update. */
const sentCustom = new Set<string>();
/** The media image shown enlarged (a modal view), by its index in `pr.media`, and its source:
 * an update that replaces that picture closes the view. */
let enlarged: number | null = null;
let enlargedSrc = "";
/** The enlarged picture at actual size (scroll or drag to pan) instead of fitted. */
let actualSize = false;
/** "Your actions" unfolded in a small panel; the shortcut list shown. */
let actionsOpen = false;
let showKeys = false;
const chartTables = new Map<number, boolean>();
let chartWidth = 0;
let override = false;
const drafts = { request: "", ask: "" };
let shift = false;
/** The decision sent and not yet answered by a panel update. */
let sent: null | "merge" | "request_changes" | "ask" | "skip" | "hold" = null;
/** Feedback for the last action, shown beside the sticky actions so it is always in view. */
let notice: { text: string; tone: Tone } | null = null;
const answers: { question: string; text: string }[] = [];
let newAnswer = false;
let mergeButton: HTMLButtonElement | null = null;
/** The user's answers to the agent's questions (their own picks and typed text, by id): a
 * local draft that rides along with the next event sent. */
let choices: Choices = {};
/** Toggle actions switched on, with the action as it was when switched on (toggleSig). */
const toggles = new Map<string, string>();
/** The outward drawer is for switching a toggle on, not for sending a `now` action. */
let outwardToggle = false;
/** The answers last sent (draftSig of the answers alone): Send answers shows "Sent" until they change. */
let sentAnswers: string | null = null;

const short = (sha: string) => sha.slice(0, 7);
const triggers: Record<Exclude<Mode, null | "outward">, string> = { confirm: "merge", request: "request", ask: "ask" };
const triggerOf = (m: Exclude<Mode, null>) => m === "outward" ? `act-${outward?.id}` : triggers[m];

function setNotice(text: string, tone: Tone = "info") { notice = { text, tone }; }

const draft = (p: Pr | null = pr): Draft => p
  ? { answers: draftAnswers(p.questions, choices), options: draftOptions(p.actions, new Set(toggles.keys())) }
  : { answers: {}, options: [] };
const answersSig = (d: Draft) => draftSig({ answers: d.answers, options: [] });
/** A decision (not a question) was sent and the agent has not answered with an update yet. */
const deciding = () => sent !== null && sent !== "ask";
const hasToggles = (p: Pr) => p.actions.some((a) => a.mode === "toggle");
/** Text that would make the event exceed Muster's event limit. */
const tooBig = (event: { kind: string; text: string; data: object }) => eventBytes(event) > MAX_EVENT_BYTES;

/** The draft changed (an answer or an option): an open Merge confirm was bound to the old one. */
function draftChanged(full = true) {
  if (pr && mode === "confirm" && gate.input(pr, draft())) {
    mode = null;
    setNotice("Your answers or options changed while you were confirming. Check them, then Merge again.", "warning");
    full = true;
  }
  if (full) render();
  else refreshDraftRow();
}

/** A send carried the draft: its options went with it (they are one-shot) and switch off; the
 * answers stay as they are. Returns how to put it back if the send is refused. */
function spendDraft(d: Draft): () => void {
  const before = new Map(toggles);
  const was = sentAnswers;
  for (const id of d.options) toggles.delete(id);
  if (Object.keys(d.answers).length) sentAnswers = answersSig(d);
  return () => {
    toggles.clear();
    for (const [k, v] of before) toggles.set(k, v);
    sentAnswers = was;
  };
}

function pick(q: Question, value: string) {
  if (enlarged !== null || !pr || formsClosed(pr)) return;
  choices = { ...choices, [q.id]: value };
  draftChanged();
  focusQuestion(q.id);
}

/** Focus a question: its picked option (or the first), or its text box. */
function focusQuestion(id: string) {
  const box = document.getElementById(`q-${id}`);
  if (!box) return;
  const el = box.querySelector<HTMLElement>('[role="radio"][aria-checked="true"]') ?? box.querySelector<HTMLElement>('[role="radio"], input');
  el?.focus({ preventScroll: true });
  reveal(el ?? box, document.querySelector(".actions"));
}

/** Tab and Shift+Tab inside the questions: the next or previous question (false past the ends). */
function moveQuestion(from: string, by: number): boolean {
  if (!pr) return false;
  const next = pr.questions[pr.questions.findIndex((q) => q.id === from) + by];
  if (!next) return false;
  focusQuestion(next.id);
  return true;
}

/** Send answers: the draft now, without deciding. */
function sendAnswers() {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr || deciding()) return;
  const closed = formsClosed(pr);
  if (closed) { setNotice(`Not sent: ${closed}`, "warning"); render(); return; }
  const d = draft();
  if (draftEmpty(d)) { setNotice("Nothing to send yet: answer a question or switch on one of your options.", "warning"); render(); return; }
  if (!d.options.length && sentAnswers === answersSig(d)) { setNotice("These answers were sent already.", "info"); render(); return; }
  const event = answersEvent(pr, d);
  if (tooBig(event)) { setNotice("Too long to send: shorten your typed answers.", "warning"); render(); return; }
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  const undo = spendDraft(d);
  setNotice(`Sent to the agent: ${summaryWords(d)}. Nothing is decided yet.`);
  render();
  result.catch((e) => { undo(); setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); });
}

/** "2 answers and Needs QA" */
function summaryWords(d: Draft): string {
  const n = Object.keys(d.answers).length;
  const labels = d.options.map((id) => pr?.actions.find((a) => a.id === id)?.label ?? id);
  const parts = [n ? `${n} answer${n === 1 ? "" : "s"}` : "", ...labels].filter(Boolean);
  return parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}` : parts[0] ?? "nothing";
}

/** Switch a toggle action on or off; an outward one asks first before it is switched on. */
function toggleAction(a: CustomAction) {
  if (!pr || formsClosed(pr)) { if (pr) { setNotice(`${formsClosed(pr)} Options can no longer be sent.`, "warning"); render(); } return; }
  if (toggles.has(a.id)) { toggles.delete(a.id); draftChanged(); document.getElementById(`act-${a.id}`)?.focus(); return; }
  if (a.outward) {
    if (mode === "confirm") gate.close();
    outward = a;
    outwardToggle = true;
    mode = "outward";
    outGate.openTarget(outwardTarget(a, target(pr)));
    notice = null;
    render();
    document.getElementById("confirm-outward")?.focus();
    return;
  }
  toggles.set(a.id, toggleSig(a));
  draftChanged();
  document.getElementById(`act-${a.id}`)?.focus();
}

function link(url: string, label: string) {
  return h("button", { type: "button", class: "link", onclick: () => openLink(url).catch((e) => { setNotice(e.message, "danger"); render(); }) }, label);
}

function updateMergeButton() {
  if (!pr || !mergeButton) return;
  const blocked = mergeBlocked(pr, shift, !!sent);
  mergeButton.disabled = !!blocked && mode !== "confirm";
  mergeButton.querySelector(".label")!.textContent = checksBlock(pr) && shift && !blocked ? "Merge anyway" : "Merge";
  const hint = document.getElementById("merge-hint");
  // The Shift override is the one reason the button itself cannot explain: say it beside it.
  if (hint) hint.textContent = blocked && mode !== "confirm" && checksBlock(pr) && blocked.includes("Shift+M") ? blocked : "";
}

function openConfirm(withShift: boolean) {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr) return;
  const blocked = mergeBlocked(pr, withShift, !!sent);
  if (blocked) { setNotice(blocked, "warning"); render(); return; }
  const missing = missingRequired(pr.questions, choices);
  if (missing.length) {
    setNotice(`Answer question ${pr.questions.indexOf(missing[0]) + 1} first: the agent needs it before merging.`, "warning");
    render();
    focusQuestion(missing[0].id);
    return;
  }
  override = checksBlock(pr);
  mode = "confirm";
  gate.open(pr, draft());
  notice = null;
  render();
  document.getElementById("confirm-merge")?.focus();
}

function closeMode() {
  const was = mode;
  const back = was ? triggerOf(was) : null;
  if (mode === "confirm") gate.close();
  if (mode === "outward") { outGate.close(); outward = null; outwardToggle = false; }
  mode = null;
  render();
  // Hand the keyboard back to the action that opened the drawer.
  if (back) (document.getElementById(back) as HTMLButtonElement | null)?.focus();
}

/** Skip and Hold: one event each; the agent moves on (Hold also marks it held in the overview). */
function sendSimple(kind: "skip" | "hold") {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr || sent) return;
  if (kind === "hold" && holdClosed(pr)) { setNotice(holdClosed(pr)!, "warning"); render(); return; }
  if (mode) closeMode();
  const d = formsClosed(pr) ? draft(null) : draft();
  const event = kind === "skip" ? skipEvent(pr, d) : holdEvent(pr, d);
  if (tooBig(event)) { setNotice("Too long to send: shorten your typed answers.", "warning"); render(); return; }
  const result = sendEvent(event.kind, event.text, event.data);
  sent = kind;
  const undo = spendDraft(d);
  const withDraft = draftEmpty(d) ? "" : ` With it: ${summaryWords(d)}.`;
  setNotice(kind === "skip" ? `Skipped: the agent moves on without deciding this PR.${withDraft}` : `Held: the agent puts this PR aside to come back to later.${withDraft}`);
  render();
  document.getElementById(kind)?.focus();
  result.catch((e) => { sent = null; undo(); setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); });
}

const holdClosed = (p: Pr) => p.status === "merged" ? "The PR is merged." : p.status === "queued" ? "The PR is queued to merge." : null;
const target = (p: Pr) => ({ repo: p.repo, number: p.number, head_sha: p.head_sha, base: p.base, status: p.status });

function runAction(a: CustomAction) {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr) return;
  if (a.mode === "toggle") { toggleAction(a); return; }
  if (sentCustom.has(a.id)) return;
  if (a.outward) {
    if (mode === "confirm") gate.close();
    outward = a;
    outwardToggle = false;
    mode = "outward";
    outGate.openTarget(outwardTarget(a, target(pr)));
    notice = null;
    render();
    document.getElementById("confirm-outward")?.focus();
    return;
  }
  sendCustom(a);
}

function sendCustom(a: CustomAction) {
  const event = customEvent(a, target(pr!));
  const result = sendEvent(event.kind, event.text, event.data);
  sentCustom.add(a.id);
  setNotice(`Sent: ${a.label}. The agent will ${a.description.replace(/^./, (c) => c.toLowerCase())}`);
  render();
  result.catch((e) => { sentCustom.delete(a.id); setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); });
}

function confirmOutward() {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr || mode !== "outward" || !outward) return;
  const a = outward;
  // Not yet armed: the key that opened it (or its repeat) is still held.
  if (!outGate.canConfirmTarget(outwardTarget(a, target(pr)))) return;
  outGate.close();
  outward = null;
  mode = null;
  if (outwardToggle) {
    outwardToggle = false;
    toggles.set(a.id, toggleSig(a));
    setNotice(`${a.label} is on: it goes with your next send.`);
    render();
  } else sendCustom(a);
  document.getElementById(`act-${a.id}`)?.focus();
}

/** Open (or step) the enlarged picture, a modal view: everything behind it is inert, and an
 * open Merge or outward confirm is closed first, so nothing can be confirmed unseen. */
function enlarge(i: number | null) {
  const was = enlarged;
  if (i !== null && pr?.media[i]?.kind !== "image") i = null;
  if (i !== null && (mode === "confirm" || mode === "outward")) {
    gate.close();
    outGate.close();
    outward = null;
    outwardToggle = false;
    mode = null;
    setNotice("The confirm was closed while you look at the pictures. Start it again when you are ready.", "warning");
  }
  if (i !== was) actualSize = false;
  enlarged = i;
  const m = i === null ? undefined : pr?.media[i];
  enlargedSrc = m?.kind === "image" ? m.image.src : "";
  render();
  if (i !== null) document.getElementById("lightbox-close")?.focus();
  else if (was !== null) (document.getElementById(`media-${was}`) ?? document.querySelector("h1"))?.focus();
}

function confirmMerge() {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr || mode !== "confirm") return;
  // Not yet armed: the key that opened the confirm (or its repeat) is still held.
  const d = draft();
  if (!gate.canConfirm(pr, d)) return;
  const blocked = mergeBlocked(pr, override, !!sent);
  if (blocked) { setNotice(blocked, "warning"); closeMode(); return; }
  const event = mergeEvent(pr, method, deleteBranch, d);
  if (tooBig(event)) { setNotice("Too long to send: shorten your typed answers.", "warning"); render(); return; }
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  sent = "merge";
  mode = null;
  gate.close();
  const undo = spendDraft(d);
  setNotice(`Merge sent to the agent${draftEmpty(d) ? "" : ` with ${summaryWords(d)}`}: it checks the PR is unchanged, runs ${mergeCommand(pr.repo, pr.number, method, deleteBranch, pr.head_sha)} and reports here.`);
  render();
  // Refused (e.g. too many actions in a few seconds): keep the confirm open to try again.
  result.catch((e) => {
    sent = null;
    undo();
    if (pr && pr.head_sha === event.data.head_sha && pr.base === event.data.base && !mergeBlocked(pr, override, false)) {
      mode = "confirm";
      gate.open(pr, draft());
    }
    setNotice(`Not sent (${e.message}). Try again.`, "danger");
    render();
    focusMode();
  });
}

function formEvent(kind: "request" | "ask", text: string) {
  return kind === "request" ? requestChangesEvent(pr!, text, draft()) : askEvent(pr!, text);
}

function submit(kind: "request" | "ask") {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr) return;
  const closed = formsClosed(pr);
  if (closed) { setNotice(`Not sent: ${closed}`, "warning"); render(); return; }
  const text = drafts[kind].trim();
  if (!text) { setNotice(kind === "request" ? "Write what should change first." : "Write a question first.", "warning"); render(); focusMode(); return; }
  const event = formEvent(kind, text);
  if (eventBytes(event) > MAX_EVENT_BYTES) { setNotice("Too long to send: shorten the text.", "warning"); render(); focusMode(); return; }
  const result = sendEvent(event.kind, event.text, event.data);
  sent = event.kind;
  mode = null;
  drafts[kind] = "";
  const undo = event.kind === "request_changes" ? spendDraft(event.data as unknown as Draft) : () => {};
  setNotice(kind === "request" ? "Request for changes sent to the agent." : "Question sent; the answer will appear here.");
  render();
  document.getElementById(triggers[kind])?.focus();
  result.catch((e) => { sent = null; undo(); drafts[kind] = text; mode = kind; setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); focusMode(); });
}

function setMode(next: Exclude<Mode, null | "confirm">) {
  if (enlarged !== null) return;
  mode = next;
  render();
  focusMode();
}

function focusMode() {
  if (mode === "request" || mode === "ask") document.getElementById(`${mode}-text`)?.focus();
  else if (mode === "confirm") document.getElementById("confirm-merge")?.focus();
}

function badge(text: string, tone: string) { return h("span", { class: `badge ${tone}` }, text); }

const checkIcon: Record<string, [string, string]> = {
  pass: ["✓", "success"], fail: ["✕", "danger"], pending: ["●", "warning"], skipping: ["–", "muted"], cancel: ["–", "muted"],
};
const checksLabel = { passing: ["Checks passing", "success"], failing: ["Checks failing", "danger"], pending: ["Checks pending", "warning"], none: ["No checks", "muted"] };
const reviewTone = (s: string) => /APPROVED/i.test(s) ? "success" : /CHANGES_REQUESTED/i.test(s) ? "danger" : "muted";
const words = (s: string) => s.toLowerCase().replace(/_/g, " ");
const statusText = {
  merging: ["Merging…", "info"], queued: ["Queued to merge", "info"], merged: ["Merged", "success"], failed: ["Failed", "danger"],
  requested_changes: ["Changes requested", "warning"],
} as const;

function diffView(diff: string) {
  const lines = parseDiff(diff);
  const shown = lines.slice(0, MAX_DIFF_LINES);
  return h("div", { class: "diff-wrap" },
    h("table", { class: "diff" }, h("tbody", null, shown.map((l) => {
      if (l.kind === "hunk") return h("tr", { class: "hunk" }, h("td", { colspan: 3 }, l.text));
      if (l.kind === "note" || l.kind === "meta") return h("tr", { class: l.kind }, h("td", { colspan: 3 }, l.text));
      const sign = l.kind === "add" ? "+" : l.kind === "del" ? "-" : " ";
      return h("tr", { class: l.kind },
        h("td", { class: "ln", "aria-hidden": "true" }, l.old ?? ""),
        h("td", { class: "ln", "aria-hidden": "true" }, l.new ?? ""),
        h("td", { class: "code" }, h("span", { class: "sign" }, sign), l.text));
    }))),
    lines.length > shown.length ? h("p", { class: "muted more" }, `${lines.length - shown.length} more lines not shown`) : null,
    lines.length ? null : h("p", { class: "muted more" }, "No diff lines."));
}

function statusBanner(p: Pr) {
  // An open PR's detail is the agent's note on what it did (e.g. for one of the user's actions).
  if (p.status === "open") return p.status_detail ? h("div", { class: "status info note" }, h("span", { class: "detail" }, p.status_detail)) : null;
  const [label, tone] = statusText[p.status];
  return h("div", { class: `status ${tone}` },
    h("strong", null, label), p.status_detail ? h("span", { class: "detail" }, p.status_detail) : null);
}

function actionPanel(p: Pr) {
  if (mode === "confirm") {
    const command = mergeCommand(p.repo, p.number, method, deleteBranch, p.head_sha);
    return h("div", { class: "drawer", role: "group", "aria-labelledby": "confirm-title" },
      h("div", { id: "confirm-title", class: "drawer-title" }, `Merge #${p.number} into ${p.base} in ${p.repo}? The agent will run exactly:`),
      h("pre", { class: "command", "aria-label": "Merge command" }, h("code", null, command)),
      draftSummary(p, draft()),
      override ? h("p", { class: "warn" }, "Checks are failing. You are merging anyway.") : null,
      p.checks.state === "pending" ? h("p", { class: "warn" }, "Some checks are still running.") : null,
      h("p", { class: "muted small" }, `--match-head-commit ties the merge to ${short(p.head_sha)}, the commit shown here: if anyone pushed since, GitHub refuses. The agent also re-checks the base branch first. A merge queue may use its own method.`),
      h("div", { class: "row" },
        h("button", { type: "button", id: "confirm-merge", class: override ? "danger" : "primary", onclick: () => confirmMerge() }, "Confirm merge ", h("kbd", null, "Enter")),
        h("button", { type: "button", onclick: () => closeMode() }, "Cancel ", h("kbd", null, "Esc"))));
  }
  if (mode === "outward" && outward) {
    return outwardDrawer(outward, `#${p.number}`, () => confirmOutward(), () => closeMode(), outwardToggle);
  }
  if (mode === "request" || mode === "ask") {
    const kind = mode;
    const closed = formsClosed(p);
    const label = kind === "request" ? "What should change? This becomes the review comment." : "Your question for the agent";
    const area = h("textarea", { id: `${kind}-text`, rows: 3, "aria-describedby": `${kind}-count` }) as HTMLTextAreaElement;
    area.value = drafts[kind];
    const count = h("span", { id: `${kind}-count`, class: "count muted small" });
    const send = h("button", { type: "button", class: "primary", id: `${kind}-send`, onclick: () => submit(kind) },
      kind === "request" ? "Request changes" : "Ask", " ", h("kbd", null, "Ctrl+Enter")) as HTMLButtonElement;
    const measure = () => {
      const left = MAX_EVENT_BYTES - eventBytes(formEvent(kind, drafts[kind].trim()));
      count.textContent = left < 0 ? `${-left} bytes too long` : left < 2000 ? `${left} bytes left` : "";
      count.className = `count small ${left < 0 ? "danger" : "muted"}`;
      send.disabled = left < 0 || !!closed;
    };
    area.addEventListener("input", () => { drafts[kind] = area.value; measure(); });
    measure();
    return h("div", { class: "drawer", role: "group" },
      h("label", { for: `${kind}-text`, class: "drawer-title" }, label), area,
      closed ? h("p", { class: "warn" }, `${closed} This can no longer be sent.`) : null,
      h("div", { class: "row" }, send,
        h("button", { type: "button", onclick: () => closeMode() }, "Cancel ", h("kbd", null, "Esc")), count));
  }
  return null;
}

const clip = (t: string, n: number) => [...t].length > n ? `${[...t].slice(0, n - 1).join("")}…` : t;

/** In the Merge confirm: exactly what goes with the merge. */
function draftSummary(p: Pr, d: Draft) {
  if (!p.questions.length && !hasToggles(p)) return null;
  const answered = p.questions.filter((q) => Object.hasOwn(d.answers, q.id));
  const opts = d.options.map((id) => p.actions.find((a) => a.id === id)).filter((a): a is CustomAction => !!a);
  if (!answered.length && !opts.length) return h("p", { class: "muted small draft-none" }, "No answers or options go with it.");
  return h("div", { class: "draft-summary", "aria-label": "Also sent with the merge" },
    h("p", { class: "drawer-sub" }, "Also sent with it:"),
    h("ul", null,
      answered.map((q) => h("li", null, h("span", { class: "muted" }, `${p.questions.indexOf(q) + 1}. ${clip(q.text, 90)} `),
        h("strong", null, clip(answerLabel(q, d.answers[q.id]), 120)), isRecommended(q, choices) ? h("span", { class: "muted small" }, " (recommended)") : null)),
      opts.map((a) => h("li", null, h("strong", null, a.label), a.outward ? h("span", { class: "out" }, " ↗") : null, h("span", { class: "muted" }, `: ${a.description}`)))));
}

/** The agent's questions, answered in place: buttons for yes/no and choices (the recommended
 * one picked and marked), a short text box for the rest. */
function questionsSection(p: Pr) {
  if (!p.questions.length) return null;
  const closed = formsClosed(p);
  return h("section", { class: "questions-form", "aria-labelledby": "questions-title" },
    h("h2", { id: "questions-title" }, p.questions.length === 1 ? "The agent asks" : `The agent asks (${p.questions.length})`),
    h("p", { class: "muted small qhint" }, closed
      ? `${closed} Answers can no longer be sent.`
      : "Your answers go with your next decision, or send them now. Number keys pick; Tab moves to the next question."),
    p.questions.map((q, i) => questionView(q, i, !!closed)));
}

function questionView(q: Question, i: number, closed: boolean) {
  const answer = answerOf(q, choices);
  const label = `q-${q.id}-label`;
  const head = h("div", { class: "q-head" },
    h("span", { class: "q-num", "aria-hidden": "true" }, String(i + 1)),
    h("span", { class: "q-text", id: label }, q.text),
    q.required ? h("span", { class: "badge warning q-req" }, "Needed to merge") : null);
  const why = q.why ? h("p", { class: "q-why muted", id: `q-${q.id}-why` }, q.why) : null;
  let body: HTMLElement;
  if (q.kind === "text") {
    const input = h("input", { type: "text", id: `q-${q.id}-text`, class: "q-input", maxlength: MAX_ANSWER, autocomplete: "off",
      "aria-labelledby": label, "aria-describedby": q.why ? `q-${q.id}-why` : null, placeholder: "A short answer", disabled: closed }) as HTMLInputElement;
    input.value = Object.hasOwn(choices, q.id) ? choices[q.id] : q.recommended ?? "";
    input.addEventListener("input", () => { choices = { ...choices, [q.id]: input.value }; draftChanged(false); });
    body = h("div", { class: "q-text-row" }, input,
      q.recommended ? h("span", { class: "muted small" }, isRecommended(q, choices) ? "Suggested by the agent" : "") : null);
  } else {
    const opts = optionsOf(q);
    const at = opts.findIndex((o) => o.value === answer);
    body = h("div", { class: "q-options", role: "radiogroup", "aria-labelledby": label, "aria-describedby": q.why ? `q-${q.id}-why` : null },
      opts.map((o, j) => h("button", {
        type: "button", role: "radio", id: `q-${q.id}-o${j}`, class: `q-opt${j === at ? " on" : ""}`, "aria-checked": String(j === at),
        tabindex: j === (at < 0 ? 0 : at) ? 0 : -1, disabled: closed, "aria-keyshortcuts": j < 9 ? String(j + 1) : null,
        onclick: () => pick(q, o.value),
      }, j < 9 ? h("kbd", null, String(j + 1)) : null, " ", o.label,
      o.value === q.recommended ? h("span", { class: "rec" }, " Recommended") : null)));
  }
  return h("div", { class: `question${answer !== undefined ? " answered" : ""}`, id: `q-${q.id}`, "data-q": q.id }, head, why, body);
}

/** In the sticky bar: what goes with the next decision, and Send answers. */
function draftRow(p: Pr) {
  if (!p.questions.length && !hasToggles(p)) return null;
  const d = draft();
  const closed = formsClosed(p);
  const n = Object.keys(d.answers).length;
  const labels = d.options.map((id) => p.actions.find((a) => a.id === id)).filter((a): a is CustomAction => !!a);
  const done = !d.options.length && n > 0 && sentAnswers === answersSig(d);
  const what = [p.questions.length ? `${n} of ${p.questions.length} answered` : "", ...labels.map((a) => `${a.label}${a.outward ? " ↗" : ""}`)].filter(Boolean);
  return h("div", { class: "row draft-row", id: "draft-row" },
    h("span", { class: "draft-what small" }, h("span", { class: "muted" }, closed ? "Draft: " : "With your next decision: "),
      what.length ? what.join(" · ") : "nothing yet"),
    h("button", { type: "button", id: "send-answers", "aria-keyshortcuts": "Control+Enter",
      disabled: !!closed || deciding() || draftEmpty(d) || done, title: "Send your answers and options now, without deciding",
      onclick: () => sendAnswers() }, done ? "Answers sent ✓" : "Send answers", " ", h("kbd", null, "Ctrl+Enter")));
}

function refreshDraftRow() {
  if (!pr) return;
  const now = document.getElementById("draft-row");
  const next = draftRow(pr);
  if (now && next) now.replaceWith(next);
}

/** The agent's verdict first, then how to undo it. */
function verdictBlock(p: Pr) {
  if (!p.verdict && !p.undo) return null;
  return h("section", { class: `verdict risk-${p.risk.level}`, "aria-label": "The agent's verdict" },
    p.verdict ? h("p", { class: "verdict-text" }, p.verdict) : null,
    p.undo ? h("p", { class: "undo" }, h("span", { class: "undo-icon", "aria-hidden": "true" }, "↩ "), p.undo) : null);
}

function mediaSection(p: Pr) {
  if (!p.media.length) return null;
  const images = p.media.filter((m) => m.kind === "image").length;
  return h("section", { class: "media", "aria-label": "Pictures and charts" },
    h("h2", null, "Pictures and charts"),
    h("div", { class: `media-grid${images === 1 ? " single" : ""}` }, p.media.map((m, i) => {
      if (m.kind === "image") {
        const img = m.image;
        return h("figure", { class: "shot" },
          h("button", { type: "button", class: "thumb", id: `media-${i}`, "aria-label": `Enlarge: ${img.alt}`, onclick: () => enlarge(i) },
            h("img", { src: img.src, alt: img.alt, decoding: "async", draggable: "false" })),
          img.caption ? h("figcaption", null, img.caption) : null);
      }
      return h("figure", { class: "media-chart" },
        h("div", { class: "chart-view", "data-chart": i }),
        m.caption ? h("figcaption", null, m.caption) : null);
    })));
}

/** Charts need the laid-out width, so they are drawn after the rest is mounted (and on resize). */
function drawCharts() {
  if (!pr) return;
  const p = pr;
  for (const el of document.querySelectorAll<HTMLElement>("[data-chart]")) {
    const i = Number(el.dataset.chart);
    const m = p.media[i];
    if (m?.kind !== "chart") continue;
    chartWidth = el.clientWidth;
    renderChart(el, m.spec, {
      id: `chart-${i}`, width: el.clientWidth, minWidth: 220, maxHeight: 300, heading: "h3", showTable: chartTables.get(i) ?? false,
      onToggle: () => { chartTables.set(i, !(chartTables.get(i) ?? false)); drawCharts(); },
    });
  }
}

function lightbox(p: Pr) {
  if (enlarged === null) return null;
  const m = p.media[enlarged];
  if (m?.kind !== "image") return null;
  const shots = p.media.flatMap((x, i) => x.kind === "image" ? [i] : []);
  const at = shots.indexOf(enlarged);
  const step = (by: number) => enlarge(shots[(at + by + shots.length) % shots.length]);
  const img = h("img", { src: m.image.src, alt: m.image.alt, draggable: "false" });
  const stage = h("div", { class: `lb-stage ${actualSize ? "actual" : "fit"}`, tabindex: "0", "aria-label": "Picture; Z toggles actual size, drag or arrow keys scroll" }, img);
  // At actual size, drag to pan; a click without a drag toggles the size.
  let drag: { x: number; y: number; moved: boolean } | null = null;
  stage.addEventListener("pointerdown", (e) => { drag = { x: e.clientX, y: e.clientY, moved: false }; });
  stage.addEventListener("pointermove", (e) => {
    if (!drag || !actualSize || !(e.buttons & 1)) return;
    const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
    stage.scrollBy(-dx, -dy);
    drag.x = e.clientX;
    drag.y = e.clientY;
  });
  img.addEventListener("click", () => { if (!drag?.moved) toggleSize(); drag = null; });
  return h("div", { class: "lightbox", role: "dialog", "aria-modal": "true", "aria-label": m.image.alt },
    h("div", { class: "lb-bar" },
      h("span", { class: "muted small lb-src", title: m.image.source }, m.image.source, m.image.bytes !== undefined ? ` · ${formatBytes(m.image.bytes)}` : ""),
      shots.length > 1 ? [
        h("button", { type: "button", "aria-label": "Previous picture", onclick: () => step(-1) }, "←"),
        h("span", { class: "muted small" }, `${at + 1} / ${shots.length}`),
        h("button", { type: "button", "aria-label": "Next picture", onclick: () => step(1) }, "→"),
      ] : null,
      h("button", { type: "button", id: "lightbox-zoom", "aria-pressed": String(actualSize), onclick: () => toggleSize() },
        actualSize ? "Fit" : "Actual size", " ", h("kbd", null, "Z")),
      h("button", { type: "button", id: "lightbox-close", onclick: () => enlarge(null) }, "Close ", h("kbd", null, "Esc"))),
    stage,
    m.image.caption ? h("p", { class: "lb-caption" }, m.image.caption) : null);
}

function setKeys(on: boolean) {
  showKeys = on;
  render();
  document.getElementById(on ? "shortcuts-close" : "keys-help")?.focus();
}

function toggleSize() {
  actualSize = !actualSize;
  const focus = document.activeElement?.id;
  render();
  if (focus) document.getElementById(focus)?.focus({ preventScroll: true });
}

const KEYS: [string, string][] = [["M", "Merge (shows the exact command first; Shift+M with failing checks)"], ["R", "Request changes"],
  ["A", "Ask the agent"], ["O", "Open on GitHub"], ["S", "Skip: move on, change nothing"], ["H", "Hold: put aside for later"],
  ["1–9", "Pick that answer in the focused question"], ["Tab", "Next question (Shift+Tab: previous)"],
  ["Ctrl+Enter", "Send your answers and options now (they also go with your next decision)"],
  ["Enter", "Confirm, in a confirm step"], ["Esc", "Close a confirm, a box or a picture"], ["?", "These shortcuts"]];

function render() {
  if (!pr) return;
  const p = pr;
  const scroll = scrollY;
  const focusId = (document.activeElement as HTMLElement | null)?.id;
  const select = h("select", { id: "method", "aria-label": "Merge method", disabled: p.status === "merged" || p.status === "queued" }) as HTMLSelectElement;
  for (const m of METHODS) select.append(h("option", { value: m, selected: m === method }, m === "merge" ? "Merge commit" : m === "squash" ? "Squash" : "Rebase"));
  select.addEventListener("change", () => { method = select.value as Method; if (mode === "confirm") render(); });
  const box = h("input", { type: "checkbox", id: "delete-branch", checked: deleteBranch, disabled: p.status === "merged" || p.status === "queued" }) as HTMLInputElement;
  box.addEventListener("change", () => { deleteBranch = box.checked; if (mode === "confirm") render(); });
  mergeButton = h("button", {
    type: "button", id: "merge", class: "primary", "aria-describedby": "merge-hint", "aria-keyshortcuts": "M",
    onclick: (e: Event) => openConfirm((e as MouseEvent).shiftKey || shift),
  }, h("span", { class: "label" }, "Merge"), " ", h("kbd", null, "M")) as HTMLButtonElement;
  const closed = formsClosed(p);
  const stats = p.changes.map((c) => diffStats(parseDiff(c.diff)));
  const effective = notice ?? (p.status !== "open" ? { text: `${statusText[p.status][0]}${p.status_detail ? `: ${p.status_detail}` : ""}`, tone: statusText[p.status][1] as Tone }
    : p.status_detail ? { text: p.status_detail, tone: "info" as Tone } : null);

  mount(root,
    h("header", { class: "head" },
      h("div", { class: "title-row" },
        h("h1", { tabindex: "-1" }, p.title, " ", h("span", { class: "num" }, `#${p.number}`)),
        h("button", { type: "button", id: "open", class: "link open-gh", "aria-keyshortcuts": "O", onclick: () => openLink(p.url).catch((e) => { setNotice(e.message, "danger"); render(); }) }, "Open on GitHub ", h("kbd", null, "O"))),
      h("p", { class: "meta muted" },
        `${p.author} wants to merge `, h("code", null, p.head), " into ", h("code", null, p.base), ` in ${p.repo} · head `,
        h("code", { title: p.head_sha }, short(p.head_sha)))),
    statusBanner(p),
    verdictBlock(p),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("section", null, h("h2", null, "What changed"), h("p", { class: "prose" }, p.summary)),
    // The decision signals come before the pictures: they must never be scrolled away by them.
    h("section", { class: "grid" },
      h("div", null,
        h("h2", null, "Risk"),
        badge(`${p.risk.level[0].toUpperCase()}${p.risk.level.slice(1)} risk`, { low: "success", medium: "warning", high: "danger" }[p.risk.level]),
        p.risk.reasons.length ? h("ul", { class: "reasons" }, p.risk.reasons.map((r) => h("li", null, r))) : null),
      h("div", null,
        h("h2", null, "Checks"),
        badge(checksLabel[p.checks.state][0], checksLabel[p.checks.state][1]),
        p.checks.items.length ? h("ul", { class: "checks" }, p.checks.items.map((c) => {
          const [icon, tone] = checkIcon[c.state];
          return h("li", null, h("span", { class: `icon ${tone}`, "aria-hidden": "true" }, icon),
            c.url ? link(c.url, c.name) : h("span", null, c.name), h("span", { class: "muted state" }, c.state));
        })) : null),
      h("div", null,
        h("h2", null, "Reviews"),
        p.reviews?.decision ? badge(words(p.reviews.decision), reviewTone(p.reviews.decision)) : h("span", { class: "muted" }, "No review decision"),
        p.reviews?.items.length ? h("ul", { class: "checks" }, p.reviews.items.map((r) =>
          h("li", null, h("span", null, r.author), h("span", { class: `state ${reviewTone(r.state)}` }, words(r.state))))) : null)),
    questionsSection(p),
    mediaSection(p),
    h("section", null,
      h("h2", null, p.changes.length ? `Changes worth a look (${p.changes.length})` : "Changes"),
      p.changes.length ? null : h("p", { class: "muted" }, "The agent picked no changes to show."),
      p.changes.map((c, i) => h("article", { class: "change" },
        h("header", null, h("code", { class: "file" }, c.file),
          h("span", { class: "stat" }, h("span", { class: "plus" }, `+${stats[i].added}`), " ", h("span", { class: "minus" }, `−${stats[i].removed}`))),
        h("p", { class: "note" }, c.note),
        diffView(c.diff))),
      p.rest_note ? h("p", { class: "rest muted" }, p.rest_note) : null),
    answers.length ? h("section", { id: "answers" }, h("h2", null, "The agent's answers"),
      answers.map((a) => h("div", { class: "answer" }, h("p", { class: "q muted" }, a.question), h("p", { class: "prose" }, a.text)))) : null,
    h("div", { class: `actions${mode || showKeys ? " has-drawer" : ""}` },
      showKeys ? shortcutsDrawer(KEYS, p.actions, () => setKeys(false), true) : null,
      actionPanel(p),
      draftRow(p),
      effective ? h("div", { class: `notice ${effective.tone}`, role: effective.tone === "danger" ? "alert" : "status" }, effective.text) : null,
      h("div", { class: "row bar" },
        h("div", { class: "merge-group" }, mergeButton, select,
          h("label", { class: "check" }, box, " Delete branch")),
        h("span", { class: "spacer" }),
        h("button", { type: "button", id: "request", "aria-keyshortcuts": "R", disabled: !!closed || sent === "request_changes", title: closed ?? null, onclick: () => setMode("request") }, "Request changes ", h("kbd", null, "R")),
        h("button", { type: "button", id: "ask", "aria-keyshortcuts": "A", disabled: !!closed, title: closed ?? null, onclick: () => setMode("ask") }, "Ask ", h("kbd", null, "A")),
        h("button", { type: "button", id: "skip", "aria-keyshortcuts": "S", disabled: !!sent, title: "Move on without changing this PR", onclick: () => sendSimple("skip") }, "Skip ", h("kbd", null, "S")),
        h("button", { type: "button", id: "hold", "aria-keyshortcuts": "H", disabled: !!sent || !!holdClosed(p), title: holdClosed(p) ?? "Put this PR aside to come back to later", onclick: () => sendSimple("hold") }, "Hold ", h("kbd", null, "H")),
        helpButton(() => setKeys(!showKeys), showKeys)),
      actionsGroup(p.actions, runAction, (a) => a.mode === "toggle" ? !!closed : sentCustom.has(a.id), actionsOpen,
        () => { actionsOpen = !actionsOpen; render(); document.getElementById("your-actions")?.focus(); },
        (a) => a.mode === "toggle" ? toggles.has(a.id) : undefined),
      h("p", { id: "merge-hint", class: "hint small", "aria-live": "polite" }),
      closed ? h("p", { class: "hint small" }, `${closed} Request changes and Ask are off.`) : null),
    lightbox(p),
  );
  // The enlarged picture is modal: everything behind it is inert (no focus, no clicks).
  for (const el of root.children) (el as HTMLElement).inert = enlarged !== null && !el.classList.contains("lightbox");
  drawCharts();
  updateMergeButton();
  scrollTo(0, scroll);
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
  if (newAnswer) {
    newAnswer = false;
    document.querySelector("#answers .answer:last-child")?.scrollIntoView({ block: "nearest" });
  }
}

addEventListener("keydown", (e) => {
  if (e.key === "Shift" && !shift) { shift = true; updateMergeButton(); }
  // A held key's auto-repeat never acts on an open confirm (nor activates its focused button).
  const outRepeat = outGate.keyDown(e.code, e.key, e.repeat);
  if (gate.keyDown(e.code, e.key, e.repeat) && mode === "confirm") { e.preventDefault(); return; }
  if (outRepeat && mode === "outward") { e.preventDefault(); return; }
  if (!pr) return;
  if (enlarged !== null) {
    // The enlarged picture is modal: it takes the keyboard (Tab stays inside it) until it closes.
    const box = document.querySelector(".lightbox");
    if (box && trapTab(box, e)) return;
    const onStage = (e.target as HTMLElement).classList?.contains("lb-stage");
    if (e.key === "Escape") { e.preventDefault(); enlarge(null); }
    else if ((e.key === "z" || e.key === "Z") && !e.ctrlKey && !e.metaKey && !e.altKey) { e.preventDefault(); toggleSize(); }
    else if ((e.key === "ArrowRight" || e.key === "ArrowLeft") && !(onStage && actualSize)) {
      const shots = pr.media.flatMap((x, i) => x.kind === "image" ? [i] : []);
      const at = shots.indexOf(enlarged);
      e.preventDefault();
      enlarge(shots[(at + (e.key === "ArrowRight" ? 1 : -1) + shots.length) % shots.length]);
    } else if (!box?.contains(e.target as Node)) e.preventDefault();
    return;
  }
  // Inside a question: number keys pick, arrows step, Tab goes to the next question, Enter in a
  // text answer too, Ctrl+Enter sends the answers.
  const qBox = (e.target as HTMLElement).closest?.("[data-q]") as HTMLElement | null;
  const q = qBox ? pr.questions.find((x) => x.id === qBox.dataset.q) : undefined;
  if (q && !e.altKey && !e.metaKey) {
    if (e.key === "Tab" && !e.ctrlKey) { if (moveQuestion(q.id, e.shiftKey ? -1 : 1)) e.preventDefault(); return; }
    if (e.key === "Enter" && e.ctrlKey) { e.preventDefault(); sendAnswers(); return; }
    if (q.kind === "text") {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); if (!moveQuestion(q.id, 1)) document.getElementById("send-answers")?.focus(); return; }
    } else if (!e.ctrlKey) {
      if (/^[0-9]$/.test(e.key)) {
        e.preventDefault();
        const v = valueForDigit(q, e.key);
        if (v !== undefined && !e.repeat) pick(q, v);
        return;
      }
      const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key];
      if (step) {
        e.preventDefault();
        const opts = optionsOf(q);
        const at = opts.findIndex((o) => o.value === answerOf(q, choices));
        pick(q, opts[((at < 0 ? (step > 0 ? -1 : 0) : at) + step + opts.length) % opts.length].value);
        return;
      }
    }
  }
  if (typing(e)) {
    const target = e.target as HTMLElement;
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && (target.id === "request-text" || target.id === "ask-text")) {
      e.preventDefault();
      submit(target.id === "request-text" ? "request" : "ask");
    } else if (e.key === "Escape" && mode) { e.preventDefault(); closeMode(); }
    return;
  }
  if (e.key === "Enter" && e.ctrlKey && !e.altKey && !e.metaKey && !mode) { e.preventDefault(); sendAnswers(); return; }
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (key === "Escape" && showKeys) { e.preventDefault(); setKeys(false); return; }
  if (key === "?" ) { e.preventDefault(); setKeys(!showKeys); return; }
  if (key === "Escape" && mode) { e.preventDefault(); closeMode(); return; }
  if (key === "Enter" && mode === "outward") {
    e.preventDefault();
    if ((e.target as HTMLElement).id === "confirm-outward" || (e.target as HTMLElement).tagName !== "BUTTON") confirmOutward();
    else (e.target as HTMLButtonElement).click();
    return;
  }
  if (key === "Enter" && mode === "confirm") {
    // Only an armed confirm may send; a focused Confirm button gets no native activation from
    // this key, so the same press is never handled twice.
    e.preventDefault();
    if ((e.target as HTMLElement).id === "confirm-merge" || (e.target as HTMLElement).tagName !== "BUTTON") confirmMerge();
    else (e.target as HTMLButtonElement).click();
    return;
  }
  if (e.repeat) return;
  if (key === "m") { e.preventDefault(); openConfirm(e.shiftKey); }
  else if (key === "r" && !formsClosed(pr) && sent !== "request_changes") { e.preventDefault(); setMode("request"); }
  else if (key === "a" && !formsClosed(pr)) { e.preventDefault(); setMode("ask"); }
  else if (key === "o") { e.preventDefault(); openLink(pr.url).catch((err) => { setNotice(err.message, "danger"); render(); }); }
  // With a confirm open, Skip, Hold and the user's actions wait until it is answered.
  else if (mode === "confirm" || mode === "outward") return;
  else if (key === "s") { e.preventDefault(); sendSimple("skip"); }
  else if (key === "h") { e.preventDefault(); sendSimple("hold"); }
  else {
    const a = actionForKey(pr.actions, key);
    if (a) { e.preventDefault(); runAction(a); }
  }
});
addEventListener("keyup", (e) => {
  if (e.key === "Shift" && shift) { shift = false; updateMergeButton(); }
  gate.keyUp(e.code);
  outGate.keyUp(e.code);
});
new ResizeObserver(() => {
  const el = document.querySelector<HTMLElement>("[data-chart]");
  if (el && Math.abs(el.clientWidth - chartWidth) > 4) drawCharts();
}).observe(root);
const pointer = (e: PointerEvent) => { if (e.shiftKey !== shift) { shift = e.shiftKey; updateMergeButton(); } };
addEventListener("pointermove", pointer);
addEventListener("pointerdown", pointer, true);
addEventListener("blur", () => { gate.blur(); outGate.blur(); if (shift) { shift = false; updateMergeButton(); } });

startApp({
  name: "Muster PR decision",
  input: (data) => {
    const r = validatePr(data);
    if (!r.ok) {
      // Nothing may stay confirmed across invalid input; the user starts again from Merge.
      gate.input(null);
      outGate.inputTarget(null);
      outward = null;
      outwardToggle = false;
      enlarged = null;
      if (mode === "confirm" || mode === "outward") mode = null;
      pr = null;
      mergeButton = null;
      problems(root, "This PR decision panel's input is invalid", r.errors);
      return;
    }
    const next = r.value;
    let closedView = false;
    const id = `${next.repo}#${next.number}`;
    const changedSha = !!pr && pr.head_sha !== next.head_sha;
    // The draft follows the agent's questions and actions: answers to questions that are gone,
    // and options whose action is gone or changed (an outward one was confirmed as it was), drop.
    if (id !== identity) { choices = {}; toggles.clear(); sentAnswers = null; }
    choices = pruneChoices(next.questions, choices);
    for (const [k, sig] of toggles) {
      const a = next.actions.find((x) => x.id === k);
      if (!a || toggleSig(a) !== sig) toggles.delete(k);
    }
    if (gate.input(next, draft(next)) && mode === "confirm") {
      mode = null;
      setNotice("The PR changed while you were confirming. Check it again, then Merge.", "warning");
    }
    if (mode === "outward" && outward) {
      const a = next.actions.find((x) => x.id === outward!.id);
      if (outGate.inputTarget(a && a.mode === outward.mode ? outwardTarget(a, target(next)) : null)) {
        mode = null;
        outward = null;
        outwardToggle = false;
        setNotice("The PR or the action changed while you were confirming. Check it, then try again.", "warning");
      } else outward = a!;
    }
    // The enlarged picture was replaced or removed: close the view rather than show another.
    const still = enlarged === null ? undefined : next.media[enlarged];
    if (enlarged !== null && (still?.kind !== "image" || still.image.src !== enlargedSrc)) { enlarged = null; enlargedSrc = ""; actualSize = false; closedView = true; }
    if (id !== identity) {
      identity = id;
      method = next.merge_method_default;
      deleteBranch = next.delete_branch_default;
      answers.length = 0;
      chartTables.clear();
      enlarged = null;
      if (mode === "confirm") mode = null;
      notice = null;
    } else if (changedSha) {
      setNotice(`The PR's head commit changed to ${short(next.head_sha)}. Check the changes again before merging.`, "warning");
    } else if (sent || sentCustom.size) notice = null;
    // Any update is the agent's reply to what was sent.
    sent = null;
    sentCustom.clear();
    if (next.answer && !answers.some((a) => a.question === next.answer!.question && a.text === next.answer!.text)) {
      answers.push(next.answer);
      newAnswer = true;
    }
    pr = next;
    warnings = r.warnings;
    render();
    if (closedView) document.querySelector<HTMLElement>("h1")?.focus();
  },
});
