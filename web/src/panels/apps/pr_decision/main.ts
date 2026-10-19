// The PR decision panel: show what the user needs to decide, then send exactly one decision.
// Every event is sent synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { diffStats, parseDiff } from "../shared/diff.ts";
import {
  askEvent, checksBlock, type Method, mergeBlocked, mergeCommand, mergeEvent, METHODS, type Pr, requestChangesEvent, validatePr,
} from "./logic.ts";

type Mode = null | "confirm" | "request" | "ask";
const root = document.getElementById("app")!;
const MAX_DIFF_LINES = 400;

let pr: Pr | null = null;
let warnings: string[] = [];
let identity = "";
let method: Method = "squash";
let deleteBranch = false;
let mode: Mode = null;
let override = false;
const drafts = { request: "", ask: "" };
let shift = false;
/** The decision sent and not yet answered by a panel update. */
let sent: null | "merge" | "request_changes" | "ask" = null;
let notice: { text: string; tone: "info" | "danger" | "warning" } | null = null;
const answers: { question: string; text: string }[] = [];
let mergeButton: HTMLButtonElement | null = null;

const short = (sha: string) => sha.slice(0, 7);

function setNotice(text: string, tone: "info" | "danger" | "warning" = "info") { notice = { text, tone }; }

function link(url: string, label: string) {
  return h("button", { type: "button", class: "link", onclick: () => openLink(url).catch((e) => { setNotice(e.message, "danger"); render(); }) }, label);
}

function updateMergeButton() {
  if (!pr || !mergeButton) return;
  const blocked = mergeBlocked(pr, shift, !!sent);
  mergeButton.disabled = !!blocked && mode !== "confirm";
  mergeButton.title = blocked ?? "Merge (M)";
  mergeButton.querySelector(".label")!.textContent = checksBlock(pr) && shift && !blocked ? "Merge anyway" : "Merge";
}

function openConfirm(withShift: boolean) {
  if (!pr) return;
  const blocked = mergeBlocked(pr, withShift, !!sent);
  if (blocked) { setNotice(blocked, "warning"); render(); return; }
  override = checksBlock(pr);
  mode = "confirm";
  notice = null;
  render();
  document.getElementById("confirm-merge")?.focus();
}

function confirmMerge() {
  if (!pr || mode !== "confirm") return;
  const blocked = mergeBlocked(pr, override, !!sent);
  if (blocked) { setNotice(blocked, "warning"); mode = null; render(); return; }
  const event = mergeEvent(pr, method, deleteBranch);
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  sent = "merge";
  mode = null;
  setNotice(`Merge sent to the agent: it runs the command below and reports here. ${mergeCommand(pr.number, method, deleteBranch, pr.head_sha)}`);
  render();
  result.catch((e) => { sent = null; setNotice(`Not sent: ${e.message}`, "danger"); render(); });
}

function submit(kind: "request" | "ask") {
  if (!pr) return;
  const text = drafts[kind].trim();
  if (!text) { setNotice(kind === "request" ? "Write what should change first." : "Write a question first.", "warning"); render(); focusMode(); return; }
  const event = kind === "request" ? requestChangesEvent(pr, text) : askEvent(pr, text);
  const result = sendEvent(event.kind, event.text, event.data);
  sent = event.kind;
  mode = null;
  drafts[kind] = "";
  setNotice(kind === "request" ? "Request for changes sent to the agent." : "Question sent; the answer will appear here.");
  render();
  result.catch((e) => { sent = null; drafts[kind] = text; mode = kind; setNotice(`Not sent: ${e.message}`, "danger"); render(); });
}

function setMode(next: Mode) {
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

function diffView(diff: string) {
  const lines = parseDiff(diff);
  const shown = lines.slice(0, MAX_DIFF_LINES);
  return h("div", { class: "diff-wrap" },
    h("table", { class: "diff" }, h("tbody", null, shown.map((l) => {
      if (l.kind === "hunk") return h("tr", { class: "hunk" }, h("td", { colspan: 3 }, l.text));
      if (l.kind === "note") return h("tr", { class: "note" }, h("td", { colspan: 3 }, l.text));
      const sign = l.kind === "add" ? "+" : l.kind === "del" ? "-" : " ";
      return h("tr", { class: l.kind },
        h("td", { class: "ln", "aria-hidden": "true" }, l.old ?? ""),
        h("td", { class: "ln", "aria-hidden": "true" }, l.new ?? ""),
        h("td", { class: "code" }, h("span", { class: "sign" }, sign), l.text));
    }))),
    lines.length > shown.length ? h("p", { class: "muted more" }, `${lines.length - shown.length} more lines not shown`) : null);
}

function statusBanner(p: Pr) {
  const map = {
    merging: ["Merging…", "info"], merged: ["Merged", "success"], failed: ["Merge failed", "danger"],
    requested_changes: ["Changes requested", "warning"],
  } as const;
  if (p.status === "open") return null;
  const [label, tone] = map[p.status];
  return h("div", { class: `status ${tone}`, role: "status", "aria-live": "polite" },
    h("strong", null, label), p.status_detail ? h("span", { class: "detail" }, p.status_detail) : null);
}

function actionPanel(p: Pr) {
  if (mode === "confirm") {
    const command = mergeCommand(p.number, method, deleteBranch, p.head_sha);
    return h("div", { class: "drawer", role: "group", "aria-labelledby": "confirm-title" },
      h("div", { id: "confirm-title", class: "drawer-title" }, `Merge #${p.number} into ${p.base}? The agent will run exactly:`),
      h("pre", { class: "command", "aria-label": "Merge command" }, h("code", null, command)),
      override ? h("p", { class: "warn" }, "Checks are failing. You are merging anyway.") : null,
      p.checks.state === "pending" ? h("p", { class: "warn" }, "Some checks are still running.") : null,
      h("p", { class: "muted small" }, `--match-head-commit ties the merge to ${short(p.head_sha)}, the commit shown here: if anyone pushed since, GitHub refuses.`),
      h("div", { class: "row" },
        h("button", { type: "button", id: "confirm-merge", class: override ? "danger" : "primary", onclick: () => confirmMerge() }, "Confirm merge ", h("kbd", null, "Enter")),
        h("button", { type: "button", onclick: () => setMode(null) }, "Cancel ", h("kbd", null, "Esc"))));
  }
  if (mode === "request" || mode === "ask") {
    const kind = mode;
    const label = kind === "request" ? "What should change? This becomes the review comment." : "Your question for the agent";
    const area = h("textarea", { id: `${kind}-text`, rows: 3, maxlength: 8000 }) as HTMLTextAreaElement;
    area.value = drafts[kind];
    area.addEventListener("input", () => { drafts[kind] = area.value; });
    return h("div", { class: "drawer", role: "group" },
      h("label", { for: `${kind}-text`, class: "drawer-title" }, label), area,
      h("div", { class: "row" },
        h("button", { type: "button", class: "primary", id: `${kind}-send`, onclick: () => submit(kind) },
          kind === "request" ? "Request changes" : "Ask", " ", h("kbd", null, "Ctrl+Enter")),
        h("button", { type: "button", onclick: () => setMode(null) }, "Cancel ", h("kbd", null, "Esc"))));
  }
  return null;
}

function render() {
  if (!pr) return;
  const p = pr;
  const scroll = scrollY;
  const focusId = (document.activeElement as HTMLElement | null)?.id;
  const select = h("select", { id: "method", "aria-label": "Merge method", disabled: p.status === "merged" }) as HTMLSelectElement;
  for (const m of METHODS) select.append(h("option", { value: m, selected: m === method }, m === "merge" ? "Merge commit" : m === "squash" ? "Squash" : "Rebase"));
  select.addEventListener("change", () => { method = select.value as Method; if (mode === "confirm") render(); });
  const box = h("input", { type: "checkbox", id: "delete-branch", checked: deleteBranch, disabled: p.status === "merged" }) as HTMLInputElement;
  box.addEventListener("change", () => { deleteBranch = box.checked; if (mode === "confirm") render(); });
  mergeButton = h("button", { type: "button", id: "merge", class: "primary", onclick: (e: Event) => openConfirm((e as MouseEvent).shiftKey || shift) },
    h("span", { class: "label" }, "Merge"), " ", h("kbd", null, "M")) as HTMLButtonElement;
  const done = p.status === "merged";
  const stats = p.changes.map((c) => diffStats(parseDiff(c.diff)));
  const allAnswers = answers.length ? answers : [];

  mount(root, 
    h("header", { class: "head" },
      h("h1", null, p.title, " ", h("span", { class: "num" }, `#${p.number}`)),
      h("p", { class: "meta muted" },
        `${p.author} wants to merge `, h("code", null, p.head), " into ", h("code", null, p.base), ` in ${p.repo} · head `,
        h("code", { title: p.head_sha }, short(p.head_sha)))),
    statusBanner(p),
    notice ? h("div", { class: `notice ${notice.tone}`, role: notice.tone === "danger" ? "alert" : "status" }, notice.text) : null,
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("section", null, h("h2", null, "What changed"), h("p", { class: "prose" }, p.summary)),
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
    h("section", null,
      h("h2", null, p.changes.length ? `Changes worth a look (${p.changes.length})` : "Changes"),
      p.changes.length ? null : h("p", { class: "muted" }, "The agent picked no changes to show."),
      p.changes.map((c, i) => h("article", { class: "change" },
        h("header", null, h("code", { class: "file" }, c.file),
          h("span", { class: "stat" }, h("span", { class: "plus" }, `+${stats[i].added}`), " ", h("span", { class: "minus" }, `−${stats[i].removed}`))),
        h("p", { class: "note" }, c.note),
        diffView(c.diff))),
      p.rest_note ? h("p", { class: "rest muted" }, p.rest_note) : null),
    p.questions.length ? h("section", null, h("h2", null, "The agent's open questions"),
      h("ol", { class: "questions" }, p.questions.map((q) => h("li", null, q))),
      h("p", { class: "muted small" }, "Answer with Ask (A), or say what should change with Request changes (R).")) : null,
    allAnswers.length ? h("section", { "aria-live": "polite" }, h("h2", null, "Answers"),
      allAnswers.map((a) => h("div", { class: "answer" }, h("p", { class: "q muted" }, a.question), h("p", { class: "prose" }, a.text)))) : null,
    h("div", { class: "actions" },
      actionPanel(p),
      h("div", { class: "row bar" },
        h("div", { class: "merge-group" }, mergeButton, select,
          h("label", { class: "check" }, box, " Delete branch")),
        h("span", { class: "spacer" }),
        h("button", { type: "button", id: "request", disabled: done || sent === "request_changes", onclick: () => setMode("request") }, "Request changes ", h("kbd", null, "R")),
        h("button", { type: "button", id: "ask", disabled: done, onclick: () => setMode("ask") }, "Ask ", h("kbd", null, "A")),
        h("button", { type: "button", id: "open", onclick: () => openLink(p.url).catch((e) => { setNotice(e.message, "danger"); render(); }) }, "Open on GitHub ", h("kbd", null, "O")))),
  );
  updateMergeButton();
  scrollTo(0, scroll);
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
}

addEventListener("keydown", (e) => {
  if (e.key === "Shift" && !shift) { shift = true; updateMergeButton(); }
  if (!pr) return;
  if (typing(e)) {
    const target = e.target as HTMLElement;
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && (target.id === "request-text" || target.id === "ask-text")) {
      e.preventDefault();
      submit(target.id === "request-text" ? "request" : "ask");
    } else if (e.key === "Escape" && mode) { e.preventDefault(); setMode(null); }
    return;
  }
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (key === "Escape" && mode) { e.preventDefault(); setMode(null); return; }
  if (key === "Enter" && mode === "confirm") {
    // A focused button handles Enter itself (and Confirm is focused when the step opens).
    if ((e.target as HTMLElement).tagName === "BUTTON") return;
    e.preventDefault();
    confirmMerge();
    return;
  }
  if (e.repeat) return;
  if (key === "m") { e.preventDefault(); openConfirm(e.shiftKey); }
  else if (key === "r" && pr.status !== "merged" && sent !== "request_changes") { e.preventDefault(); setMode("request"); }
  else if (key === "a" && pr.status !== "merged") { e.preventDefault(); setMode("ask"); }
  else if (key === "o") { e.preventDefault(); openLink(pr.url).catch((err) => { setNotice(err.message, "danger"); render(); }); }
});
addEventListener("keyup", (e) => { if (e.key === "Shift" && shift) { shift = false; updateMergeButton(); } });
const pointer = (e: PointerEvent) => { if (e.shiftKey !== shift) { shift = e.shiftKey; updateMergeButton(); } };
addEventListener("pointermove", pointer);
addEventListener("pointerdown", pointer, true);
addEventListener("blur", () => { if (shift) { shift = false; updateMergeButton(); } });

startApp({
  name: "Muster PR decision",
  input: (data) => {
    const r = validatePr(data);
    if (!r.ok) { pr = null; mergeButton = null; problems(root, "This PR decision panel's input is invalid", r.errors); return; }
    const next = r.value;
    const id = `${next.repo}#${next.number}`;
    if (id !== identity) {
      identity = id;
      method = next.merge_method_default;
      deleteBranch = next.delete_branch_default;
      answers.length = 0;
      mode = null;
      notice = null;
    } else if (pr && pr.head_sha !== next.head_sha) {
      if (mode === "confirm") mode = null;
      setNotice(`The PR's head commit changed to ${short(next.head_sha)}. Check the changes again before merging.`, "warning");
    } else if (sent) notice = null;
    // Any update is the agent's reply to what was sent.
    sent = null;
    if (next.answer && !answers.some((a) => a.question === next.answer!.question && a.text === next.answer!.text)) answers.push(next.answer);
    pr = next;
    warnings = r.warnings;
    render();
  },
});
