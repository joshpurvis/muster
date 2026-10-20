// The PR decision panel: show what the user needs to decide, then send exactly one decision.
// Every event is sent synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems, reveal, trapTab } from "../shared/dom.ts";
import { diffStats, parseDiff } from "../shared/diff.ts";
import { Gate } from "../shared/confirm.ts";
import { actionForKey, type CustomAction, customEvent, outwardTarget } from "../shared/actions.ts";
import {
  answerLabel, answerOf, type Draft, draftEmpty, draftSig, isRecommended, MAX_ANSWER, missingRequired, optionsOf, type Question,
  valueForDigit,
} from "../shared/questions.ts";
import { DraftState } from "../shared/draft.ts";
import { actionsGroup, helpButton, outwardDrawer, shortcutsDrawer } from "../shared/actions_view.ts";
import { chart } from "../../kit/chart.ts";
import { iconEl } from "../../kit/icons.ts";
import { avatarColor } from "../../kit/parts.ts";
import { formatBytes } from "../image/logic.ts";
import { freshView, type Viewer, viewer, type ViewState } from "../shared/viewer.ts";
import {
  answersEvent, askEvent, checksBlock, ConfirmGate, eventBytes, formsClosed, holdEvent, MAX_EVENT_BYTES, type Method, mergeBlocked, mergeCommand,
  joinAnd, mergeEvent, METHODS, type Pr, readiness, requestChangesEvent, skipEvent, validatePr,
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

/** The media image shown enlarged (a modal view), by its index in `pr.media`, and its source:
 * an update that replaces that picture closes the view. */
let enlarged: number | null = null;
let enlargedSrc = "";
/** The enlarged picture's zoom (Fit, or a scale with its scroll), kept across re-renders, and
 * the viewer drawing it. */
let lbView: ViewState = freshView();
let lbViewer: Viewer | null = null;
/** "Your actions" unfolded in a small panel; the shortcut list shown. */
let actionsOpen = false;
let showKeys = false;
const chartTables = new Map<number, boolean>();
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
/** The user's answers to the agent's questions and the toggle options switched on: a local
 * draft that rides along with the next event sent (shared/draft.ts). */
const ds = new DraftState();
/** The outward drawer is for switching a toggle on, not for sending a `now` action. */
let outwardToggle = false;
/** Counts the agent's updates and the sends: a refused send may only undo what is still its
 * own (it never re-enables, reopens or restores anything after an update or a later send). */
let inputGen = 0;
let sendSeq = 0;
/** `now` actions sent, by id, with the send that sent them. */
const sentNow = new Map<string, number>();

const short = (sha: string) => sha.slice(0, 7);
const triggers: Record<Exclude<Mode, null | "outward">, string> = { confirm: "merge", request: "request", ask: "ask" };
const triggerOf = (m: Exclude<Mode, null>) => m === "outward" ? `act-${outward?.id}` : triggers[m];

function setNotice(text: string, tone: Tone = "info") { notice = { text, tone }; }

const draft = (p: Pr | null = pr): Draft => p ? ds.draft(p.questions, p.actions) : { answers: {}, options: [] };
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
  else { refreshDraftRow(); refreshReadiness(); }
}

/** A send carried the draft: its options went with it (they are one-shot) and switch off; the
 * answers stay. Returns the refusal handler's undo: it restores exactly what this send spent,
 * only while nothing changed since, and says whether it did. */
function spendDraft(d: Draft): () => boolean {
  const rollback = ds.spend(d);
  return () => rollback(pr?.actions ?? []);
}

/** The notice for a refused send; `back` says whether the draft was put back as it was. */
const refused = (e: Error, back: boolean) => back || !pr
  ? `Not sent (${e.message}). Try again.`
  : `Not sent (${e.message}). Things changed since, so check your answers and options, then send again.`;

function pick(q: Question, value: string) {
  if (enlarged !== null || !pr || formsClosed(pr)) return;
  ds.setChoice(q.id, value);
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
  // While Merge is being confirmed the draft goes with the merge: sending it now would change
  // what the confirm shows under it.
  if (mode === "confirm") { setNotice("Confirm or cancel the merge first: your answers and options go with it.", "warning"); render(); return; }
  const closed = formsClosed(pr);
  if (closed) { setNotice(`Not sent: ${closed}`, "warning"); render(); return; }
  const d = draft();
  if (draftEmpty(d)) { setNotice("Nothing to send yet: answer a question or switch on one of your options.", "warning"); render(); return; }
  if (!d.options.length && ds.sentAnswers === answersSig(d)) { setNotice("These answers were sent already.", "info"); render(); return; }
  const event = answersEvent(pr, d);
  if (tooBig(event)) { setNotice("Too long to send: shorten your typed answers.", "warning"); render(); return; }
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  sendSeq++;
  const undo = spendDraft(d);
  const acts = d.options.length ? ` The agent will now do: ${summaryWords({ answers: {}, options: d.options })}.` : "";
  setNotice(`Sent to the agent: ${summaryWords(d)}.${acts} The PR is not decided yet.`);
  render();
  result.catch((e) => { setNotice(refused(e, undo()), "danger"); draftChanged(); });
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
  if (ds.isOn(a)) { ds.switchOff(a.id); draftChanged(); document.getElementById(`act-${a.id}`)?.focus(); return; }
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
  ds.switchOn(a);
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
  if (holdForRequired(pr)) return;
  override = checksBlock(pr);
  mode = "confirm";
  gate.open(pr, draft());
  notice = null;
  render();
  document.getElementById("confirm-merge")?.focus();
}

/** A required question without an answer holds Merge: say which, and take the user to it.
 * Checked when Merge opens and again right before it sends. */
function holdForRequired(p: Pr): boolean {
  const missing = missingRequired(p.questions, ds.choices);
  if (!missing.length) return false;
  if (mode === "confirm") { gate.close(); mode = null; }
  setNotice(`Answer question ${p.questions.indexOf(missing[0]) + 1} first: the agent needs it before merging.`, "warning");
  render();
  focusQuestion(missing[0].id);
  return true;
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
  const mine = ++sendSeq;
  const undo = spendDraft(d);
  const withDraft = draftEmpty(d) ? "" : ` With it: ${summaryWords(d)}.`;
  setNotice(kind === "skip" ? `Skipped: the agent moves on without deciding this PR.${withDraft}` : `Held: the agent puts this PR aside to come back to later.${withDraft}`);
  render();
  document.getElementById(kind)?.focus();
  result.catch((e) => { if (sendSeq === mine) sent = null; setNotice(refused(e, undo()), "danger"); draftChanged(); });
}

const holdClosed = (p: Pr) => p.status === "merged" ? "The PR is merged." : p.status === "queued" ? "The PR is queued to merge." : null;
const target = (p: Pr) => ({ repo: p.repo, number: p.number, head_sha: p.head_sha, base: p.base, status: p.status });

function runAction(a: CustomAction) {
  // Nothing acts while a picture covers the panel.
  if (enlarged !== null || !pr) return;
  if (a.mode === "toggle") { toggleAction(a); return; }
  if (sentNow.has(a.id)) return;
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
  const mine = ++sendSeq;
  sentNow.set(a.id, mine);
  setNotice(`Sent: ${a.label}. The agent will ${a.description.replace(/^./, (c) => c.toLowerCase())}`);
  render();
  // Only this send's own mark comes off: never one a later send (after an update) set.
  result.catch((e) => { if (sentNow.get(a.id) === mine) sentNow.delete(a.id); setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); });
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
    ds.switchOn(a);
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
  if (i !== was) lbView = freshView();
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
  // The questions may have changed since Merge opened (that cancels the confirm too): check again.
  if (holdForRequired(pr)) return;
  const event = mergeEvent(pr, method, deleteBranch, d);
  if (tooBig(event)) { setNotice("Too long to send: shorten your typed answers.", "warning"); render(); return; }
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  sent = "merge";
  mode = null;
  gate.close();
  const mine = ++sendSeq;
  const ig = inputGen;
  const undo = spendDraft(d);
  setNotice(`Merge sent to the agent${draftEmpty(d) ? "" : ` with ${summaryWords(d)}`}: it checks the PR is unchanged, runs ${mergeCommand(pr.repo, pr.number, method, deleteBranch, pr.head_sha)} and reports here.`);
  render();
  // Refused (e.g. too many actions in a few seconds): keep the confirm open to try again.
  result.catch((e) => {
    if (sendSeq === mine) sent = null;
    const back = undo();
    // Reopen the confirm only if nothing happened since: no update, no other send, no edit.
    if (back && sendSeq === mine && inputGen === ig && mode === null && pr && pr.head_sha === event.data.head_sha && pr.base === event.data.base
      && !mergeBlocked(pr, override, false) && !missingRequired(pr.questions, ds.choices).length) {
      mode = "confirm";
      gate.open(pr, draft());
    }
    setNotice(refused(e, back), "danger");
    draftChanged();
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
  const mine = ++sendSeq;
  const ig = inputGen;
  const undo = event.kind === "request_changes" ? spendDraft(event.data as unknown as Draft) : () => true;
  setNotice(kind === "request" ? "Request for changes sent to the agent." : "Question sent; the answer will appear here.");
  render();
  document.getElementById(triggers[kind])?.focus();
  result.catch((e) => {
    if (sendSeq === mine) sent = null;
    const back = undo();
    // The text comes back only into the same PR, with nothing sent or updated since and no new
    // text started; otherwise the notice says it was not sent.
    if (sendSeq === mine && inputGen === ig && mode === null && !drafts[kind] && pr && !formsClosed(pr)) { drafts[kind] = text; mode = kind; }
    setNotice(refused(e, back), "danger");
    draftChanged();
    focusMode();
  });
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

const KIT_TONE: Record<string, string> = { info: "accent", success: "good", warning: "warn", danger: "bad", muted: "plain" };
function badge(text: string, tone: string) { return h("span", { class: `mk-badge badge ${KIT_TONE[tone] ?? tone}` }, text); }

const checkIcon: Record<string, [string, string]> = {
  pass: ["check", "success"], fail: ["x", "danger"], pending: ["clock", "warning"], skipping: ["minus", "muted"], cancel: ["minus", "muted"],
};
const checksLabel = { passing: ["Passing", "success"], failing: ["Failing", "danger"], pending: ["Pending", "warning"], none: ["No checks", "muted"] };
const reviewTone = (s: string) => /APPROVED/i.test(s) ? "success" : /CHANGES_REQUESTED/i.test(s) ? "danger" : /REVIEW_REQUIRED/i.test(s) ? "warning" : "muted";
const words = (s: string) => { const w = s.toLowerCase().replace(/_/g, " "); return w[0].toUpperCase() + w.slice(1); };
const decisionText = (s: string) => /REVIEW_REQUIRED/i.test(s) ? "Approval needed" : words(s);
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
      const sign = l.kind === "add" ? "+" : l.kind === "del" ? "−" : " ";
      return h("tr", { class: l.kind },
        h("td", { class: "ln", "aria-hidden": "true" }, l.kind === "del" ? l.old ?? "" : l.new ?? l.old ?? ""),
        h("td", { class: "sign", "aria-hidden": "true" }, sign),
        h("td", { class: "code" }, h("span", { class: "sr-only" }, l.kind === "add" ? "added: " : l.kind === "del" ? "removed: " : ""), l.text));
    }))),
    lines.length > shown.length ? h("p", { class: "muted more" }, `${lines.length - shown.length} more lines not shown`) : null,
    lines.length ? null : h("p", { class: "muted more" }, "No diff lines."));
}

/** Five blocks in proportion to the lines added and removed, like GitHub's. */
function sizeBlocks(added: number, removed: number) {
  const tot = added + removed;
  const a = tot ? Math.round((added / tot) * 5) : 0, d = tot ? Math.min(5 - a, Math.round((removed / tot) * 5)) : 0;
  return Array.from({ length: 5 }, (_, i) => h("i", { class: i < a ? "a" : i < a + d ? "d" : "" }));
}

function statusBanner(p: Pr) {
  // An open PR's detail is the agent's note on what it did (e.g. for one of the user's actions).
  if (p.status === "open") return p.status_detail ? h("div", { class: "status info note" }, h("span", { class: "detail" }, p.status_detail)) : null;
  const [label, tone] = statusText[p.status];
  return h("div", { class: `status ${tone}` },
    h("strong", null, label), p.status_detail ? h("span", { class: "detail" }, p.status_detail) : null);
}

/** The agent's call first, with how to undo it; its colour comes from Muster's own check of
 * whether the PR can merge (readiness), so a green card never sits beside a merge GitHub would refuse. */
function verdictBlock(p: Pr) {
  const r = readiness(p, ds.choices);
  const gate = p.status === "open" && !r.ready;
  if (!p.verdict && !p.undo && !gate) return null;
  const vc = { good: "var(--mk-good)", warn: "var(--mk-warn)", bad: "var(--mk-bad)" }[r.tone];
  return h("section", { class: `verdict tone-${r.tone}`, style: `--vc:${vc}`, "aria-label": "The agent's verdict" },
    h("span", { class: "vi", "aria-hidden": "true" }, iconEl("shield")),
    p.verdict ? h("p", { class: "verdict-text v" }, p.verdict) : null,
    p.undo ? h("p", { class: "undo u" }, iconEl("undo"), h("span", null, p.undo)) : null,
    gate ? h("div", { class: "gate", id: "readiness" }, iconEl("clock"),
      h("span", null, `Not ready yet: ${joinAnd(r.reasons)}.`, h("small", null, "Checked by Muster against the PR, not the agent's opinion."))) : null);
}

function factsBlock(p: Pr) {
  const level = p.risk.level;
  const on = { low: 1, medium: 2, high: 3 }[level];
  const tone = { low: "success", medium: "warning", high: "danger" }[level];
  const decision = p.reviews?.decision;
  return h("section", { class: "facts grid", "aria-label": "Risk, checks and reviews" },
    h("div", { class: `mk-card fact risk-${level}` },
      h("div", { class: "fh" }, h("h2", null, "Risk"), badge(`${level[0].toUpperCase()}${level.slice(1)}`, tone)),
      h("div", { class: "riskbar", "aria-hidden": "true" }, [1, 2, 3].map((i) => h("i", { class: i <= on ? "on" : "" }))),
      p.risk.reasons.length ? h("ul", { class: "reasons" }, p.risk.reasons.map((r) => h("li", { class: "reason" }, r))) : null),
    h("div", { class: "mk-card fact" },
      h("div", { class: "fh" }, h("h2", null, "Checks"), badge(checksLabel[p.checks.state][0], checksLabel[p.checks.state][1])),
      p.checks.items.length ? h("ul", { class: "checks" }, p.checks.items.map((c) => {
        const [icon, t] = checkIcon[c.state];
        return h("li", { class: `c-${t}` }, iconEl(icon), c.url ? link(c.url, c.name) : h("span", { class: "name" }, c.name), h("span", { class: "state st" }, c.state));
      })) : h("p", { class: "muted none" }, "No check runs reported.")),
    h("div", { class: "mk-card fact" },
      h("div", { class: "fh" }, h("h2", null, "Reviews"), decision ? badge(decisionText(decision), reviewTone(decision)) : badge("No decision", "muted")),
      p.reviews?.items.length ? h("ul", { class: "checks" }, p.reviews.items.map((r) =>
        h("li", null, h("span", { class: "mk-avatar", style: `--a:${avatarColor(r.author)}`, "aria-hidden": "true" }, r.author[0]), h("span", { class: "name" }, r.author),
          h("span", { class: `state st ${reviewTone(r.state)}` }, words(r.state).toLowerCase())))) : null,
      decision && /REVIEW_REQUIRED/i.test(decision) ? h("p", { class: "needs" }, iconEl("users"), "Needs an approving review") : null,
      !decision && !p.reviews?.items.length ? h("p", { class: "muted none" }, "No reviews yet.") : null));
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

/** In the Merge confirm: exactly what goes with the merge. */
function draftSummary(p: Pr, d: Draft) {
  if (!p.questions.length && !hasToggles(p)) return null;
  const answered = p.questions.filter((q) => Object.hasOwn(d.answers, q.id));
  const opts = d.options.map((id) => p.actions.find((a) => a.id === id)).filter((a): a is CustomAction => !!a);
  if (!answered.length && !opts.length) return h("p", { class: "muted small draft-none" }, "No answers or options go with it.");
  return h("div", { class: "draft-summary", "aria-label": "Also sent with the merge" },
    h("p", { class: "drawer-sub" }, "Also sent with it:"),
    h("ul", null,
      answered.map((q) => h("li", null, h("span", { class: "muted" }, `${p.questions.indexOf(q) + 1}. ${q.text} `),
        h("strong", null, answerLabel(q, d.answers[q.id])), isRecommended(q, ds.choices) ? h("span", { class: "muted small" }, " (recommended)") : null)),
      opts.map((a) => h("li", null, h("strong", null, a.label), a.outward ? h("span", { class: "out" }, " ↗") : null, h("span", { class: "muted" }, `: ${a.description}`)))));
}

/** The agent's questions, answered in place: buttons for yes/no and choices (the recommended
 * one picked and marked), a short text box for the rest. */
function questionsSection(p: Pr) {
  if (!p.questions.length) return null;
  const closed = formsClosed(p);
  const answered = p.questions.filter((q) => answerOf(q, ds.choices) !== undefined).length;
  return h("section", { class: "questions-form pd-sec", "aria-labelledby": "questions-title" },
    h("h2", { id: "questions-title" }, "The agent asks", h("span", { class: "count" }, `${answered} of ${p.questions.length} answered`),
      h("span", { class: "r" }, questionsChip(p))),
    h("p", { class: "muted small qhint" }, closed
      ? `${closed} Answers can no longer be sent.`
      : "Your answers go with your next decision, or send them now. Number keys pick; Tab moves to the next question."),
    p.questions.map((q, i) => questionView(q, i, !!closed)));
}

function questionView(q: Question, i: number, closed: boolean) {
  const answer = answerOf(q, ds.choices);
  const label = `q-${q.id}-label`;
  const head = h("div", { class: "q-head" },
    h("span", { class: "q-num", "aria-hidden": "true" }, String(i + 1)),
    h("span", { class: "q-text", id: label }, q.text),
    q.required ? h("span", { class: "q-req req", id: `q-${q.id}-req` }, "Needed to merge") : null);
  const described = [q.why ? `q-${q.id}-why` : "", q.required ? `q-${q.id}-req` : ""].filter(Boolean).join(" ") || null;
  const why = q.why ? h("p", { class: "q-why muted", id: `q-${q.id}-why` }, q.why) : null;
  let body: HTMLElement;
  if (q.kind === "text") {
    const input = h("input", { type: "text", id: `q-${q.id}-text`, class: "q-input", maxlength: MAX_ANSWER, autocomplete: "off",
      "aria-labelledby": label, "aria-describedby": described, "aria-required": q.required ? "true" : null, required: q.required,
      placeholder: "A short answer", disabled: closed }) as HTMLInputElement;
    input.value = Object.hasOwn(ds.choices, q.id) ? ds.choices[q.id] : q.recommended ?? "";
    input.addEventListener("input", () => { ds.setChoice(q.id, input.value); draftChanged(false); });
    body = h("div", { class: "q-text-row" }, input,
      q.recommended ? h("span", { class: "muted small" }, isRecommended(q, ds.choices) ? "Suggested by the agent" : "") : null);
  } else {
    const opts = optionsOf(q);
    const at = opts.findIndex((o) => o.value === answer);
    body = h("div", { class: "q-options", role: "radiogroup", "aria-labelledby": label, "aria-describedby": described,
      "aria-required": q.required ? "true" : null },
      opts.map((o, j) => h("button", {
        type: "button", role: "radio", id: `q-${q.id}-o${j}`, class: `q-opt opt${j === at ? " on" : ""}`, "aria-checked": String(j === at),
        tabindex: j === (at < 0 ? 0 : at) ? 0 : -1, disabled: closed, "aria-keyshortcuts": j < 9 ? String(j + 1) : null,
        onclick: () => pick(q, o.value),
      }, j < 9 ? h("kbd", null, String(j + 1)) : null, o.label,
      o.value === q.recommended ? h("span", { class: "rec" }, iconEl("sparkle"), "Recommended") : null)));
  }
  return h("div", { class: `question mk-card q${answer !== undefined ? " answered done" : ""}`, id: `q-${q.id}`, "data-q": q.id }, head, why, body);
}

/** In the header: the agent's questions are one click (or G) away, however far down they are. */
function questionsChip(p: Pr) {
  if (!p.questions.length) return null;
  const open = p.questions.filter((q) => answerOf(q, ds.choices) === undefined).length;
  const n = p.questions.length;
  return h("button", { type: "button", id: "to-questions", class: `sm q-chip${open ? " open" : ""}`, "aria-keyshortcuts": "G", onclick: () => toQuestions() },
    open ? `Answer ${open} question${open === 1 ? "" : "s"}` : `Review ${n} answer${n === 1 ? "" : "s"}`, iconEl("arrowDown"), h("kbd", null, "G"));
}

/** Go to the first question still without an answer, else the first one. */
function toQuestions() {
  if (!pr?.questions.length || enlarged !== null) return;
  const q = pr.questions.find((x) => answerOf(x, ds.choices) === undefined) ?? pr.questions[0];
  focusQuestion(q.id);
}

/** In the sticky bar: what goes with the next decision, and Send answers. */
function draftRow(p: Pr) {
  if (!p.questions.length && !hasToggles(p)) return null;
  const d = draft();
  const closed = formsClosed(p);
  const n = Object.keys(d.answers).length;
  const labels = d.options.map((id) => p.actions.find((a) => a.id === id)).filter((a): a is CustomAction => !!a);
  const done = !d.options.length && n > 0 && ds.sentAnswers === answersSig(d);
  const opts = labels.map((a) => `${a.label}${a.outward ? " ↗" : ""}`);
  // What the button sends, by name: answers, the options switched on (which the agent then does), or both.
  const label = done ? "Answers sent ✓" : n && labels.length ? "Send answers and options" : labels.length ? "Send options" : "Send answers";
  return h("div", { class: "row draft-row", id: "draft-row" },
    h("span", { class: "draft-what small" }, h("span", { class: "muted" }, closed ? "Draft: " : "With your next decision: "),
      p.questions.length ? h("button", { type: "button", class: "link", id: "draft-count", title: "Go to the questions (G)", onclick: () => toQuestions() },
        `${n} of ${p.questions.length} answered`) : null,
      opts.length ? `${p.questions.length ? " · " : ""}${opts.join(" · ")}` : p.questions.length ? "" : "nothing yet"),
    h("button", { type: "button", id: "send-answers", class: "ghost sm", "aria-keyshortcuts": "Control+Enter",
      disabled: !!closed || deciding() || draftEmpty(d) || done || mode === "confirm",
      title: mode === "confirm" ? "They go with the merge you are confirming" : "Send your answers, and the options switched on for the agent to do, now; the PR stays undecided",
      onclick: () => sendAnswers() }, label, " ", h("kbd", null, "Ctrl+Enter")));
}

function refreshDraftRow() {
  if (!pr) return;
  const now = document.getElementById("draft-row");
  const next = draftRow(pr);
  if (now && next) now.replaceWith(next);
}

/** Typing an answer changes readiness: refresh everything that shows it (the verdict's colour
 * and reasons, the answered counts, the question cards, why Merge waits) without rebuilding
 * the question the user is typing in. */
function refreshReadiness() {
  if (!pr) return;
  const p = pr;
  const verdict = root.querySelector(":scope > section.verdict");
  const nextVerdict = verdictBlock(p);
  if (verdict && nextVerdict) verdict.replaceWith(nextVerdict);
  else if (verdict) verdict.remove();
  else if (nextVerdict) root.querySelector(":scope > p.warnings, :scope > section.facts")?.before(nextVerdict);
  const answered = p.questions.filter((q) => answerOf(q, ds.choices) !== undefined).length;
  const count = document.querySelector("#questions-title .count");
  if (count) count.textContent = `${answered} of ${p.questions.length} answered`;
  const chip = document.getElementById("to-questions"), nextChip = questionsChip(p);
  if (chip && nextChip) chip.replaceWith(nextChip);
  for (const q of p.questions) {
    const done = answerOf(q, ds.choices) !== undefined;
    document.getElementById(`q-${q.id}`)?.classList.toggle("answered", done);
    document.getElementById(`q-${q.id}`)?.classList.toggle("done", done);
  }
  const ready = readiness(p, ds.choices);
  const waits = p.status === "open" && !ready.ready;
  mergeButton?.classList.toggle("waits", waits);
  mergeButton?.setAttribute("aria-describedby", waits ? "merge-hint merge-why" : "merge-hint");
  const why = document.getElementById("merge-why");
  const nextWhy = waits && mode !== "confirm" ? mergeWhy(ready) : null;
  if (why && nextWhy) why.replaceWith(nextWhy);
  else if (why) why.remove();
  else if (nextWhy) root.querySelector(".actbar > .row.bar")?.after(nextWhy);
  updateMergeButton();
}

const mergeWhy = (ready: { short: string[] }) => h("p", { id: "merge-why", class: "why small" }, iconEl("clock"), `Merge waits for ${joinAnd(ready.short)}`);

function mediaSection(p: Pr) {
  if (!p.media.length) return null;
  const images = p.media.filter((m) => m.kind === "image").length;
  return h("section", { class: "media pd-sec", "aria-labelledby": "media-title" },
    h("h2", { id: "media-title" }, "Pictures and charts"),
    h("div", { class: `media-grid${images === 1 ? " single" : ""}` }, p.media.map((m, i) => {
      if (m.kind === "image") {
        const img = m.image;
        return h("figure", { class: "shot mk-card" },
          h("button", { type: "button", class: "thumb", id: `media-${i}`, "aria-label": `Enlarge: ${img.alt}`, onclick: () => enlarge(i) },
            h("img", { src: img.src, alt: img.alt, decoding: "async", draggable: "false" })),
          img.caption ? h("figcaption", null, img.caption) : null);
      }
      const shown = chartTables.get(i) ?? false;
      return h("figure", { class: "media-chart mk-card" },
        m.spec.title ? h("h3", { class: "chart-title" }, m.spec.title) : null,
        h("div", { class: "chart-view", "data-chart": i }),
        h("figcaption", null, m.caption ? h("span", null, m.caption) : h("span"),
          h("button", { type: "button", class: "ghost sm", id: `chart-${i}-toggle`, "aria-expanded": String(shown),
            onclick: () => { chartTables.set(i, !shown); render(); document.getElementById(`chart-${i}-toggle`)?.focus(); } },
          iconEl("table"), shown ? "Hide data table" : "Data table")));
    })));
}

/** Charts need the laid-out width, so they are drawn after the rest is mounted; each then
 * redraws itself when its width changes. */
function drawCharts() {
  if (!pr) return;
  const p = pr;
  for (const el of document.querySelectorAll<HTMLElement>("[data-chart]")) {
    const i = Number(el.dataset.chart);
    const m = p.media[i];
    if (m?.kind !== "chart") continue;
    chart(el, m.spec, { height: 220, showTable: chartTables.get(i) ?? false, legendValues: false });
  }
}

function lightbox(p: Pr) {
  const m = enlarged === null ? undefined : p.media[enlarged];
  if (enlarged === null || m?.kind !== "image") {
    lbViewer?.destroy();
    lbViewer = null;
    return null;
  }
  const shots = p.media.flatMap((x, i) => x.kind === "image" ? [i] : []);
  const at = shots.indexOf(enlarged);
  const step = (by: number) => enlarge(shots[(at + by + shots.length) % shots.length]);
  const img = h("img", { src: m.image.src, alt: m.image.alt });
  lbViewer?.destroy();
  lbViewer = viewer(img, lbView, {
    stageClass: "lb-stage",
    label: "Picture; + and − zoom, 0 fits, 1 is actual size, Z toggles, drag or arrow keys scroll",
    ids: { zoomIn: "lightbox-zoom-in", zoomOut: "lightbox-zoom-out", fit: "lightbox-fit", actual: "lightbox-zoom" },
  });
  const stage = lbViewer.stage;
  lbViewer.readout.classList.add("muted", "small", "lb-readout");
  return h("div", { class: "lightbox", role: "dialog", "aria-modal": "true", "aria-label": m.image.alt },
    h("div", { class: "lb-bar" },
      h("span", { class: "muted small lb-src", title: m.image.source }, m.image.source, m.image.bytes !== undefined ? ` · ${formatBytes(m.image.bytes)}` : ""),
      shots.length > 1 ? [
        h("button", { type: "button", "aria-label": "Previous picture", onclick: () => step(-1) }, "←"),
        h("span", { class: "muted small" }, `${at + 1} / ${shots.length}`),
        h("button", { type: "button", "aria-label": "Next picture", onclick: () => step(1) }, "→"),
      ] : null,
      lbViewer.readout,
      lbViewer.controls,
      h("button", { type: "button", id: "lightbox-close", onclick: () => enlarge(null) }, "Close ", h("kbd", null, "Esc"))),
    stage,
    m.image.caption ? h("p", { class: "lb-caption" }, m.image.caption) : null);
}

function setKeys(on: boolean) {
  showKeys = on;
  render();
  document.getElementById(on ? "shortcuts-close" : "keys-help")?.focus();
}

const KEYS: [string, string][] = [["M", "Merge (shows the exact command first; Shift+M with failing checks)"], ["R", "Request changes"],
  ["A", "Ask the agent"], ["O", "Open on GitHub"], ["S", "Skip: move on, change nothing"], ["H", "Hold: put aside for later"],
  ["G", "Go to the agent's questions"], ["1–9", "Pick that answer in the focused question"], ["Tab", "Next question (Shift+Tab: previous)"],
  ["Ctrl+Enter", "Send your answers and options now (otherwise they go with your next decision)"],
  ["Enter", "Confirm, in a confirm step"], ["Esc", "Close a confirm, a box or a picture"], ["?", "These shortcuts"]];

function render() {
  if (!pr) return;
  const p = pr;
  const scroll = scrollY;
  const focusId = (document.activeElement as HTMLElement | null)?.id;
  const select = h("select", { id: "method", class: "method", "aria-label": "Merge method", disabled: p.status === "merged" || p.status === "queued" }) as HTMLSelectElement;
  for (const m of METHODS) select.append(h("option", { value: m, selected: m === method }, m === "merge" ? "Merge commit" : m === "squash" ? "Squash" : "Rebase"));
  select.addEventListener("change", () => { method = select.value as Method; if (mode === "confirm") render(); });
  const box = h("input", { type: "checkbox", id: "delete-branch", checked: deleteBranch, disabled: p.status === "merged" || p.status === "queued" }) as HTMLInputElement;
  box.addEventListener("change", () => { deleteBranch = box.checked; if (mode === "confirm") render(); });
  const ready = readiness(p, ds.choices);
  const waits = p.status === "open" && !ready.ready;
  mergeButton = h("button", {
    type: "button", id: "merge", class: `primary${waits ? " waits" : ""}`, "aria-describedby": waits ? "merge-hint merge-why" : "merge-hint", "aria-keyshortcuts": "M",
    onclick: (e: Event) => openConfirm((e as MouseEvent).shiftKey || shift),
  }, iconEl("merge"), h("span", { class: "label" }, "Merge"), h("kbd", null, "M")) as HTMLButtonElement;
  const closed = formsClosed(p);
  const stats = p.changes.map((c) => diffStats(parseDiff(c.diff)));
  const effective = notice ?? (p.status !== "open" ? { text: `${statusText[p.status][0]}${p.status_detail ? `: ${p.status_detail}` : ""}`, tone: statusText[p.status][1] as Tone }
    : p.status_detail ? { text: p.status_detail, tone: "info" as Tone } : null);
  const files = p.changes.length;

  mount(root,
    h("header", { class: "head" },
      h("div", { class: "pd-meta" },
        h("span", { class: "mk-chip" }, p.repo),
        h("span", null, files ? `${files} change${files === 1 ? "" : "s"} worth a look` : "no changes picked to show"),
        h("button", { type: "button", id: "open", class: "ghost sm open-gh", "aria-keyshortcuts": "O", onclick: () => openLink(p.url).catch((e) => { setNotice(e.message, "danger"); render(); }) },
          "Open on GitHub", iconEl("external"), h("kbd", null, "O"))),
      h("h1", { class: "mk-title", tabindex: "-1" }, p.title, " ", h("span", { class: "num" }, `#${p.number}`)),
      h("p", { class: "meta pd-by" },
        h("span", { class: "mk-avatar", style: `--a:${avatarColor(p.author)}`, "aria-hidden": "true" }, p.author[0]),
        h("b", null, p.author), " wants to merge ",
        h("code", { class: "mk-chip" }, iconEl("branch"), p.head), h("span", { class: "sr-only" }, " into "), iconEl("arrowRight"),
        h("code", { class: "mk-chip" }, p.base), h("span", { class: "muted" }, ` in ${p.repo} · head`),
        h("code", { class: "mk-chip", title: p.head_sha }, short(p.head_sha)))),
    statusBanner(p),
    verdictBlock(p),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    // The decision signals come before the pictures: they must never be scrolled away by them.
    factsBlock(p),
    h("section", { class: "pd-sec" }, h("h2", null, "What changed"), h("p", { class: "prose" }, p.summary)),
    questionsSection(p),
    mediaSection(p),
    h("section", { class: "pd-sec" },
      h("h2", null, files ? "Changes worth a look" : "Changes", files ? h("span", { class: "count" }, String(files)) : null),
      files ? null : h("p", { class: "muted" }, "The agent picked no changes to show."),
      p.changes.map((c, i) => h("article", { class: "change mk-card file" },
        h("header", { class: "fh2" }, h("code", { class: "file fn" }, c.file),
          h("span", { class: "stat ds" }, h("span", { class: "plus a" }, `+${stats[i].added}`), h("span", { class: "minus d" }, `−${stats[i].removed}`),
            h("span", { class: "blocks", "aria-hidden": "true" }, sizeBlocks(stats[i].added, stats[i].removed)))),
        h("p", { class: "note" }, iconEl("sparkle"), h("span", null, c.note)),
        diffView(c.diff))),
      p.rest_note ? h("p", { class: "rest muted" }, p.rest_note) : null),
    answers.length ? h("section", { id: "answers", class: "pd-sec" }, h("h2", null, "The agent's answers"),
      answers.map((a) => h("div", { class: "answer mk-card" }, h("p", { class: "q muted" }, a.question), h("p", { class: "prose" }, a.text)))) : null,
    h("div", { class: `actions actbar${mode || showKeys ? " has-drawer" : ""}` },
      showKeys ? shortcutsDrawer(KEYS, p.actions, () => setKeys(false), true) : null,
      actionPanel(p),
      draftRow(p),
      effective ? h("div", { class: `notice ${effective.tone}`, role: effective.tone === "danger" ? "alert" : "status" }, effective.text) : null,
      h("div", { class: "row bar" },
        h("div", { class: "merge-group" }, mergeButton, select,
          h("label", { class: "check" }, box, " Delete branch")),
        h("span", { class: "grp-sep", "aria-hidden": "true" }),
        h("button", { type: "button", id: "request", "aria-keyshortcuts": "R", disabled: !!closed || sent === "request_changes", title: closed ?? null, onclick: () => setMode("request") }, "Request changes ", h("kbd", null, "R")),
        h("button", { type: "button", id: "ask", "aria-keyshortcuts": "A", disabled: !!closed, title: closed ?? null, onclick: () => setMode("ask") }, "Ask ", h("kbd", null, "A")),
        h("span", { class: "spacer" }),
        h("button", { type: "button", id: "skip", class: "ghost", "aria-keyshortcuts": "S", disabled: !!sent, title: "Move on without changing this PR", onclick: () => sendSimple("skip") }, "Skip ", h("kbd", null, "S")),
        h("button", { type: "button", id: "hold", class: "ghost", "aria-keyshortcuts": "H", disabled: !!sent || !!holdClosed(p), title: holdClosed(p) ?? "Put this PR aside to come back to later", onclick: () => sendSimple("hold") }, "Hold ", h("kbd", null, "H")),
        helpButton(() => setKeys(!showKeys), showKeys)),
      waits && mode !== "confirm" ? mergeWhy(ready) : null,
      actionsGroup(p.actions, runAction, (a) => a.mode === "toggle" ? !!closed : sentNow.has(a.id), actionsOpen,
        () => { actionsOpen = !actionsOpen; render(); document.getElementById("your-actions")?.focus(); },
        (a) => a.mode === "toggle" ? ds.isOn(a) : undefined),
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
    else if (lbViewer?.key(e)) return;
    else if ((e.key === "ArrowRight" || e.key === "ArrowLeft") && !(onStage && lbViewer?.overflowing())) {
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
        const at = opts.findIndex((o) => o.value === answerOf(q, ds.choices));
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
  // Ctrl+Enter sends the answers; during a Merge confirm it explains why not (they go with it).
  if (e.key === "Enter" && e.ctrlKey && !e.altKey && !e.metaKey && (!mode || mode === "confirm")) { e.preventDefault(); sendAnswers(); return; }
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
  else if (key === "g" && pr.questions.length) { e.preventDefault(); toQuestions(); }
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
const pointer = (e: PointerEvent) => { if (e.shiftKey !== shift) { shift = e.shiftKey; updateMergeButton(); } };
addEventListener("pointermove", pointer);
addEventListener("pointerdown", pointer, true);
addEventListener("blur", () => { gate.blur(); outGate.blur(); if (shift) { shift = false; updateMergeButton(); } });

startApp({
  name: "Muster PR decision",
  input: (data) => {
    const r = validatePr(data);
    inputGen++;
    if (!r.ok) {
      // Nothing may stay confirmed across invalid input; the user starts again from Merge.
      ds.touch();
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
    ds.input(id, next.questions, next.actions);
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
    if (enlarged !== null && (still?.kind !== "image" || still.image.src !== enlargedSrc)) { enlarged = null; enlargedSrc = ""; lbView = freshView(); closedView = true; }
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
    } else if (sent || sentNow.size) notice = null;
    // Any update is the agent's reply to what was sent.
    sent = null;
    sentNow.clear();
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
