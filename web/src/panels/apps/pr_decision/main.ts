// The PR decision panel: show what the user needs to decide, then send exactly one decision.
// Every event is sent synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems } from "../shared/dom.ts";
import { diffStats, parseDiff } from "../shared/diff.ts";
import { Gate } from "../shared/confirm.ts";
import { actionForKey, type CustomAction, customEvent, outwardTarget } from "../shared/actions.ts";
import { actionButton, outwardDrawer } from "../shared/actions_view.ts";
import { renderChart } from "../chart/view.ts";
import { formatBytes } from "../image/logic.ts";
import {
  askEvent, checksBlock, ConfirmGate, eventBytes, formsClosed, holdEvent, MAX_EVENT_BYTES, type Method, mergeBlocked, mergeCommand,
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
/** The media image shown enlarged, by its index in `pr.media`. */
let enlarged: number | null = null;
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

const short = (sha: string) => sha.slice(0, 7);
const triggers: Record<Exclude<Mode, null | "outward">, string> = { confirm: "merge", request: "request", ask: "ask" };
const triggerOf = (m: Exclude<Mode, null>) => m === "outward" ? `act-${outward?.id}` : triggers[m];

function setNotice(text: string, tone: Tone = "info") { notice = { text, tone }; }

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
  if (!pr) return;
  const blocked = mergeBlocked(pr, withShift, !!sent);
  if (blocked) { setNotice(blocked, "warning"); render(); return; }
  override = checksBlock(pr);
  mode = "confirm";
  gate.open(pr);
  notice = null;
  render();
  document.getElementById("confirm-merge")?.focus();
}

function closeMode() {
  const was = mode;
  const back = was ? triggerOf(was) : null;
  if (mode === "confirm") gate.close();
  if (mode === "outward") { outGate.close(); outward = null; }
  mode = null;
  render();
  // Hand the keyboard back to the action that opened the drawer.
  if (back) (document.getElementById(back) as HTMLButtonElement | null)?.focus();
}

/** Skip and Hold: one event each; the agent moves on (Hold also marks it held in the overview). */
function sendSimple(kind: "skip" | "hold") {
  if (!pr || sent) return;
  if (kind === "hold" && holdClosed(pr)) { setNotice(holdClosed(pr)!, "warning"); render(); return; }
  if (mode) closeMode();
  const event = kind === "skip" ? skipEvent(pr) : holdEvent(pr);
  const result = sendEvent(event.kind, event.text, event.data);
  sent = kind;
  setNotice(kind === "skip" ? "Skipped: the agent moves on without changing this PR." : "Held: the agent puts this PR aside to come back to later.");
  render();
  document.getElementById(kind)?.focus();
  result.catch((e) => { sent = null; setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); });
}

const holdClosed = (p: Pr) => p.status === "merged" ? "The PR is merged." : p.status === "queued" ? "The PR is queued to merge." : null;
const target = (p: Pr) => ({ repo: p.repo, number: p.number, head_sha: p.head_sha });

function runAction(a: CustomAction) {
  if (!pr || sentCustom.has(a.id)) return;
  if (a.outward) {
    if (mode === "confirm") gate.close();
    outward = a;
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
  if (!pr || mode !== "outward" || !outward) return;
  const a = outward;
  // Not yet armed: the key that opened it (or its repeat) is still held.
  if (!outGate.canConfirmTarget(outwardTarget(a, target(pr)))) return;
  outGate.close();
  outward = null;
  mode = null;
  sendCustom(a);
  document.getElementById(`act-${a.id}`)?.focus();
}

function enlarge(i: number | null) {
  const was = enlarged;
  enlarged = i;
  render();
  if (i !== null) document.getElementById("lightbox-close")?.focus();
  else if (was !== null) document.getElementById(`media-${was}`)?.focus();
}

function confirmMerge() {
  if (!pr || mode !== "confirm") return;
  // Not yet armed: the key that opened the confirm (or its repeat) is still held.
  if (!gate.canConfirm(pr)) return;
  const blocked = mergeBlocked(pr, override, !!sent);
  if (blocked) { setNotice(blocked, "warning"); closeMode(); return; }
  const event = mergeEvent(pr, method, deleteBranch);
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(event.kind, event.text, event.data);
  sent = "merge";
  mode = null;
  gate.close();
  setNotice(`Merge sent to the agent: it checks the PR is unchanged, runs ${mergeCommand(pr.repo, pr.number, method, deleteBranch, pr.head_sha)} and reports here.`);
  render();
  // Refused (e.g. too many actions in a few seconds): keep the confirm open to try again.
  result.catch((e) => {
    sent = null;
    if (pr && pr.head_sha === event.data.head_sha && pr.base === event.data.base && !mergeBlocked(pr, override, false)) {
      mode = "confirm";
      gate.open(pr);
    }
    setNotice(`Not sent (${e.message}). Try again.`, "danger");
    render();
    focusMode();
  });
}

function formEvent(kind: "request" | "ask", text: string) {
  return kind === "request" ? requestChangesEvent(pr!, text) : askEvent(pr!, text);
}

function submit(kind: "request" | "ask") {
  if (!pr) return;
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
  setNotice(kind === "request" ? "Request for changes sent to the agent." : "Question sent; the answer will appear here.");
  render();
  document.getElementById(triggers[kind])?.focus();
  result.catch((e) => { sent = null; drafts[kind] = text; mode = kind; setNotice(`Not sent (${e.message}). Try again.`, "danger"); render(); focusMode(); });
}

function setMode(next: Exclude<Mode, null | "confirm">) {
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
      override ? h("p", { class: "warn" }, "Checks are failing. You are merging anyway.") : null,
      p.checks.state === "pending" ? h("p", { class: "warn" }, "Some checks are still running.") : null,
      h("p", { class: "muted small" }, `--match-head-commit ties the merge to ${short(p.head_sha)}, the commit shown here: if anyone pushed since, GitHub refuses. The agent also re-checks the base branch first. A merge queue may use its own method.`),
      h("div", { class: "row" },
        h("button", { type: "button", id: "confirm-merge", class: override ? "danger" : "primary", onclick: () => confirmMerge() }, "Confirm merge ", h("kbd", null, "Enter")),
        h("button", { type: "button", onclick: () => closeMode() }, "Cancel ", h("kbd", null, "Esc"))));
  }
  if (mode === "outward" && outward) {
    return outwardDrawer(outward, `#${p.number}`, () => confirmOutward(), () => closeMode());
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
  return h("div", { class: "lightbox", role: "dialog", "aria-modal": "true", "aria-label": m.image.alt, onclick: (e: Event) => { if (e.target === e.currentTarget) enlarge(null); } },
    h("div", { class: "lb-bar" },
      h("span", { class: "muted small lb-src", title: m.image.source }, m.image.source, m.image.bytes !== undefined ? ` · ${formatBytes(m.image.bytes)}` : ""),
      shots.length > 1 ? [
        h("button", { type: "button", "aria-label": "Previous picture", onclick: () => step(-1) }, "←"),
        h("span", { class: "muted small" }, `${at + 1} / ${shots.length}`),
        h("button", { type: "button", "aria-label": "Next picture", onclick: () => step(1) }, "→"),
      ] : null,
      h("button", { type: "button", id: "lightbox-close", onclick: () => enlarge(null) }, "Close ", h("kbd", null, "Esc"))),
    h("div", { class: "lb-stage", onclick: () => enlarge(null) }, h("img", { src: m.image.src, alt: m.image.alt })),
    m.image.caption ? h("p", { class: "lb-caption" }, m.image.caption) : null);
}

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
      h("h1", null, p.title, " ", h("span", { class: "num" }, `#${p.number}`)),
      h("p", { class: "meta muted" },
        `${p.author} wants to merge `, h("code", null, p.head), " into ", h("code", null, p.base), ` in ${p.repo} · head `,
        h("code", { title: p.head_sha }, short(p.head_sha)))),
    statusBanner(p),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("section", null, h("h2", null, "What changed"), h("p", { class: "prose" }, p.summary)),
    mediaSection(p),
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
    answers.length ? h("section", { id: "answers" }, h("h2", null, "Answers"),
      answers.map((a) => h("div", { class: "answer" }, h("p", { class: "q muted" }, a.question), h("p", { class: "prose" }, a.text)))) : null,
    h("div", { class: "actions" },
      actionPanel(p),
      effective ? h("div", { class: `notice ${effective.tone}`, role: effective.tone === "danger" ? "alert" : "status" }, effective.text) : null,
      h("div", { class: "row bar" },
        h("div", { class: "merge-group" }, mergeButton, select,
          h("label", { class: "check" }, box, " Delete branch")),
        h("span", { class: "spacer" }),
        h("button", { type: "button", id: "request", "aria-keyshortcuts": "R", disabled: !!closed || sent === "request_changes", title: closed ?? null, onclick: () => setMode("request") }, "Request changes ", h("kbd", null, "R")),
        h("button", { type: "button", id: "ask", "aria-keyshortcuts": "A", disabled: !!closed, title: closed ?? null, onclick: () => setMode("ask") }, "Ask ", h("kbd", null, "A")),
        h("button", { type: "button", id: "open", "aria-keyshortcuts": "O", onclick: () => openLink(p.url).catch((e) => { setNotice(e.message, "danger"); render(); }) }, "Open on GitHub ", h("kbd", null, "O"))),
      h("div", { class: "row bar second" },
        h("button", { type: "button", id: "skip", "aria-keyshortcuts": "S", disabled: !!sent, title: "Move on without changing this PR", onclick: () => sendSimple("skip") }, "Skip ", h("kbd", null, "S")),
        h("button", { type: "button", id: "hold", "aria-keyshortcuts": "H", disabled: !!sent || !!holdClosed(p), title: holdClosed(p) ?? "Put this PR aside to come back to later", onclick: () => sendSimple("hold") }, "Hold ", h("kbd", null, "H")),
        p.actions.map((a) => actionButton(a, () => runAction(a), sentCustom.has(a.id)))),
      h("p", { id: "merge-hint", class: "hint small", "aria-live": "polite" }),
      closed ? h("p", { class: "hint small" }, `${closed} Request changes and Ask are off.`) : null),
    lightbox(p),
  );
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
    // The enlarged picture takes the keyboard until it closes.
    if (e.key === "Escape") { e.preventDefault(); enlarge(null); }
    else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      const shots = pr.media.flatMap((x, i) => x.kind === "image" ? [i] : []);
      const at = shots.indexOf(enlarged);
      e.preventDefault();
      enlarge(shots[(at + (e.key === "ArrowRight" ? 1 : -1) + shots.length) % shots.length]);
    }
    return;
  }
  if (typing(e)) {
    const target = e.target as HTMLElement;
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && (target.id === "request-text" || target.id === "ask-text")) {
      e.preventDefault();
      submit(target.id === "request-text" ? "request" : "ask");
    } else if (e.key === "Escape" && mode) { e.preventDefault(); closeMode(); }
    return;
  }
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
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
      enlarged = null;
      if (mode === "confirm" || mode === "outward") mode = null;
      pr = null;
      mergeButton = null;
      problems(root, "This PR decision panel's input is invalid", r.errors);
      return;
    }
    const next = r.value;
    const id = `${next.repo}#${next.number}`;
    const changedSha = !!pr && pr.head_sha !== next.head_sha;
    if (gate.input(next) && mode === "confirm") {
      mode = null;
      setNotice("The PR changed while you were confirming. Check it again, then Merge.", "warning");
    }
    if (mode === "outward" && outward) {
      const a = next.actions.find((x) => x.id === outward!.id);
      if (outGate.inputTarget(a ? outwardTarget(a, target(next)) : null)) {
        mode = null;
        outward = null;
        setNotice("The PR or the action changed while you were confirming. Check it, then try again.", "warning");
      } else outward = a!;
    }
    if (enlarged !== null && next.media[enlarged]?.kind !== "image") enlarged = null;
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
  },
});
