// The PR overview panel: every open PR of a repo as a card, the agent's triage on each, and the
// user's pick sent back (open, hold, next, or one of their own actions). Every event is sent
// synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems, reveal } from "../shared/dom.ts";
import { Gate } from "../shared/confirm.ts";
import { actionForKey, type CustomAction, customEvent, outwardTarget } from "../shared/actions.ts";
import { actionsGroup, helpButton, outwardDrawer, shortcutsDrawer } from "../shared/actions_view.ts";
import {
  age, arrange, canHold, followCurrent, GROUP_LABELS, holdEvent, type Item, nextEvent, nextPr, openEvent, type Overview, queue, type Sort,
  SORT_LABELS, SORTS, size, tally, validateOverview,
} from "./logic.ts";

type Tone = "info" | "danger" | "warning" | "success";
const root = document.getElementById("app")!;

let view: Overview | null = null;
let warnings: string[] = [];
/** The user's sort once they pick one; until then the agent's. */
let userSort: Sort | null = null;
let filter = "";
let selected: number | null = null;
let notice: { text: string; tone: Tone } | null = null;
/** `kind:number` of events sent since the last update: not sent twice before the agent answers. */
const sent = new Set<string>();
let outward: { action: CustomAction; pr: Item } | null = null;
/** "Your actions" unfolded in a small panel; the shortcut list shown. */
let actionsOpen = false;
let showKeys = false;
const gate = new Gate();

const sortNow = () => userSort ?? view?.sort ?? "risk";
const ordered = () => view ? arrange(view.prs, sortNow(), filter).flatMap((g) => g.items) : [];
const selectedPr = () => view?.prs.find((p) => p.number === selected) ?? null;
/** The one place a PR's target is built: what its events name, and what a confirm is bound to. */
const target = (pr: Item, repo = view!.repo) => ({ repo, number: pr.number, head_sha: pr.head_sha, status: pr.status });
/** The card that has the keyboard, if any: keys act on it, never on a selection moved elsewhere. */
const focusedCard = () => {
  const el = document.activeElement as HTMLElement | null;
  return el?.getAttribute("role") === "option" && el.id.startsWith("pr-") ? Number(el.id.slice(3)) : null;
};
const KEYS: [string, string][] = [["J / ↓", "Next card"], ["K / ↑", "Previous card"], ["Enter / O", "Open the PR: the agent reviews it"],
  ["H", "Hold: put aside for later"], ["N", "Go through the PRs in this order"], ["/", "Filter (Esc clears)"], ["?", "These shortcuts"]];

function setKeys(on: boolean) {
  showKeys = on;
  render();
  document.getElementById(on ? "shortcuts-close" : "keys-help")?.focus();
}

const riskText = { high: ["High risk", "danger"], medium: ["Medium risk", "warning"], low: ["Low risk", "success"] } as const;
const checksText = { passing: ["✓ Checks", "success"], failing: ["✕ Checks failing", "danger"], pending: ["● Checks pending", "warning"], none: ["No checks", "muted"] } as const;
const reviewText = { APPROVED: ["Approved", "success"], CHANGES_REQUESTED: ["Changes requested", "danger"], REVIEW_REQUIRED: ["Review required", "muted"], NONE: ["No review", "muted"] } as const;
const statusText = {
  new: ["", "muted"], reviewing: ["Reviewing", "info"], merged: ["Merged", "success"], queued: ["Queued", "info"], held: ["Held", "warning"],
  skipped: ["Skipped", "muted"], changes_requested: ["Changes requested", "warning"], failed: ["Failed", "danger"],
} as const;

function badge(text: string, tone: string, extra = "") { return h("span", { class: `badge ${tone} ${extra}`.trim() }, text); }

function send(ev: { kind: string; text: string; data: object }, key: string, ok: string) {
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(ev.kind, ev.text, ev.data);
  sent.add(key);
  notice = { text: ok, tone: "info" };
  render();
  result.catch((e) => { sent.delete(key); notice = { text: `Not sent (${e.message}). Try again.`, tone: "danger" }; render(); });
}

function open(pr: Item | null) {
  if (!view || !pr) return;
  if (sent.has(`open:${pr.number}`)) return;
  send(openEvent(view.repo, pr), `open:${pr.number}`, `Sent: open #${pr.number}. The agent reviews it and shows its decision panel.`);
}

function hold(pr: Item | null) {
  if (!view || !pr) return;
  if (!canHold(pr)) { notice = { text: `#${pr.number} is already ${statusText[pr.status][0].toLowerCase() || "decided"}.`, tone: "warning" }; render(); return; }
  if (sent.has(`hold:${pr.number}`)) return;
  send(holdEvent(view.repo, pr), `hold:${pr.number}`, `Sent: hold #${pr.number}. It moves to Held.`);
}

function next() {
  if (!view) return;
  const todo = queue(ordered(), view.current);
  const pr = todo[0];
  if (!pr) { notice = { text: "Nothing left to review: every PR is held or decided.", tone: "warning" }; render(); return; }
  if (sent.has(`next:${pr.number}`)) return;
  selected = pr.number;
  send(nextEvent(view.repo, todo), `next:${pr.number}`, `Sent: go through ${todo.length} PR${todo.length === 1 ? "" : "s"} in order, starting with #${pr.number}.`);
}

function runAction(a: CustomAction) {
  const pr = selectedPr();
  if (!view || !pr) { notice = { text: "Select a PR first.", tone: "warning" }; render(); return; }
  if (sent.has(`custom:${a.id}:${pr.number}`)) return;
  if (a.outward) {
    outward = { action: a, pr };
    gate.openTarget(outwardTarget(a, target(pr)));
    notice = null;
    render();
    document.getElementById("confirm-outward")?.focus();
    return;
  }
  send(customEvent(a, target(pr)), `custom:${a.id}:${pr.number}`, `Sent: ${a.label} on #${pr.number}.`);
}

function confirmOutward() {
  if (!view || !outward) return;
  const { action, pr } = outward;
  // Not yet armed: the key that opened it (or its repeat) is still held.
  if (!gate.canConfirmTarget(outwardTarget(action, target(pr)))) return;
  outward = null;
  gate.close();
  send(customEvent(action, target(pr)), `custom:${action.id}:${pr.number}`, `Sent: ${action.label} on #${pr.number}.`);
  document.getElementById(`act-${action.id}`)?.focus();
}

function cancelOutward() {
  const id = outward?.action.id;
  outward = null;
  gate.close();
  render();
  if (id) document.getElementById(`act-${id}`)?.focus();
}

function select(n: number, focus = true) {
  const old = selected;
  selected = n;
  // In place, not a re-render: the card under the pointer stays the same node (double-click).
  for (const [id, on] of [[old, false], [n, true]] as const) {
    const el = id === null ? null : document.getElementById(`pr-${id}`);
    if (!el) continue;
    el.classList.toggle("selected", on);
    el.setAttribute("aria-selected", String(on));
    el.tabIndex = on ? 0 : -1;
  }
  document.querySelector(".list")?.setAttribute("aria-activedescendant", `pr-${n}`);
  const focused = (document.activeElement as HTMLElement | null)?.id;
  document.querySelector(".actions")?.replaceWith(actionBar());
  if (focused && focused !== `pr-${old}`) document.getElementById(focused)?.focus({ preventScroll: true });
  if (focus) {
    const el = document.getElementById(`pr-${n}`);
    el?.focus({ preventScroll: true });
    reveal(el, document.querySelector(".actions"));
  }
}

function move(by: number) {
  const list = ordered();
  if (!list.length) return;
  const at = list.findIndex((p) => p.number === selected);
  const i = !Number.isFinite(by) ? (by > 0 ? list.length - 1 : 0)
    : at < 0 ? (by > 0 ? 0 : list.length - 1) : Math.max(0, Math.min(list.length - 1, at + by));
  select(list[i].number);
}

function card(pr: Item, now: number) {
  const st = statusText[pr.status];
  const sizeKnown = pr.additions !== undefined || pr.deletions !== undefined;
  const meta = [
    pr.author, age(pr.created, now),
    sizeKnown ? h("span", { class: "size", title: `${size(pr)} lines changed` }, h("span", { class: "plus" }, `+${pr.additions ?? 0}`), " ", h("span", { class: "minus" }, `−${pr.deletions ?? 0}`)) : null,
    pr.files !== undefined ? `${pr.files} file${pr.files === 1 ? "" : "s"}` : null,
  ].filter(Boolean);
  const isSel = pr.number === selected;
  return h("div", {
    class: `card risk-${pr.risk ?? "none"}${isSel ? " selected" : ""}${pr.number === view!.current ? " current" : ""} g-${pr.status}`,
    id: `pr-${pr.number}`, role: "option", "aria-selected": String(isSel), tabindex: isSel ? 0 : -1,
    onclick: () => { if (selected !== pr.number) select(pr.number); },
    ondblclick: () => open(pr),
  },
  h("div", { class: "top" },
    h("span", { class: "num" }, `#${pr.number}`),
    h("span", { class: "title" }, pr.title),
    pr.number === view!.current ? badge("Now", "info", "now") : null,
    st[0] ? badge(st[0], st[1], "status") : null),
  h("div", { class: "meta muted" },
    meta.flatMap((m, i) => i ? [" · ", m] : [m]),
    pr.draft ? [" · ", h("span", { class: "draft" }, "Draft")] : null),
  h("div", { class: "chips" },
    pr.risk ? badge(riskText[pr.risk][0], riskText[pr.risk][1]) : null,
    pr.checks ? badge(checksText[pr.checks][0], checksText[pr.checks][1]) : null,
    pr.review ? badge(reviewText[pr.review][0], reviewText[pr.review][1]) : null,
    pr.labels.map((l) => h("span", { class: "label" }, l))),
  pr.summary ? h("p", { class: "summary" }, pr.summary) : null,
  pr.status_detail ? h("p", { class: "detail muted" }, pr.status_detail) : null);
}

function actionBar() {
  const v = view!;
  const pr = selectedPr();
  const nextUp = nextPr(ordered(), v.current);
  const busy = (k: string) => !!pr && sent.has(`${k}:${pr.number}`);
  return h("div", { class: `actions${outward || showKeys ? " has-drawer" : ""}` },
    showKeys ? shortcutsDrawer(KEYS, v.actions, () => setKeys(false)) : null,
    outward ? outwardDrawer(outward.action, `#${outward.pr.number}`, () => confirmOutward(), () => cancelOutward()) : null,
    notice ? h("div", { class: `notice ${notice.tone}`, role: notice.tone === "danger" ? "alert" : "status" }, notice.text) : null,
    h("div", { class: "row bar" },
      h("button", { type: "button", id: "open", class: "primary", "aria-keyshortcuts": "Enter O", disabled: !pr || busy("open"), onclick: () => open(selectedPr()) },
        pr ? `Open #${pr.number}` : "Open", " ", h("kbd", null, "Enter")),
      h("button", { type: "button", id: "hold", "aria-keyshortcuts": "H", disabled: !pr || !canHold(pr) || busy("hold"), onclick: () => hold(selectedPr()) }, "Hold ", h("kbd", null, "H")),
      h("button", { type: "button", id: "next", "aria-keyshortcuts": "N", disabled: !nextUp, title: nextUp ? `Next: #${nextUp.number}` : "Nothing left to review", onclick: () => next() },
        "Next", nextUp ? ` #${nextUp.number}` : "", " ", h("kbd", null, "N")),
      helpButton(() => setKeys(!showKeys), showKeys),
      pr?.url ? h("button", { type: "button", id: "github", class: "link", onclick: () => openLink(pr.url!).catch((e) => { notice = { text: e.message, tone: "danger" }; render(); }) }, "GitHub") : null),
    actionsGroup(v.actions, runAction, (a) => !pr || sent.has(`custom:${a.id}:${pr.number}`), actionsOpen,
      () => { actionsOpen = !actionsOpen; render(); document.getElementById("your-actions")?.focus(); }),
    h("p", { class: "keys muted small" }, "J/K move · Enter open · H hold · N next · / filter · ? keys"));
}

function render() {
  if (!view) return;
  const v = view;
  const scroll = scrollY;
  const active = document.activeElement as HTMLInputElement | null;
  const focusId = active?.id;
  const caret = focusId === "filter" ? [active!.selectionStart, active!.selectionEnd] : null;
  const groups = arrange(v.prs, sortNow(), filter);
  const shown = groups.flatMap((g) => g.items);
  if (!shown.some((p) => p.number === selected)) selected = shown[0]?.number ?? null;
  const counts = tally(v.prs);
  const now = Date.now();
  const sortSelect = h("select", { id: "sort", "aria-label": "Sort" }) as HTMLSelectElement;
  for (const s of SORTS) sortSelect.append(h("option", { value: s, selected: s === sortNow() }, SORT_LABELS[s]));
  sortSelect.addEventListener("change", () => { userSort = sortSelect.value as Sort; render(); });
  const filterBox = h("input", { type: "search", id: "filter", placeholder: "Filter  /", "aria-label": "Filter PRs", autocomplete: "off" }) as HTMLInputElement;
  filterBox.value = filter;
  filterBox.addEventListener("input", () => { filter = filterBox.value; render(); });

  mount(root,
    h("header", { class: "head" },
      h("h1", null, v.title ?? "Open pull requests"),
      h("p", { class: "muted repo" }, v.repo, " · ",
        `${counts.todo} to review`, counts.done ? ` · ${counts.done} decided` : "", counts.held ? ` · ${counts.held} held` : "")),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("div", { class: "tools" }, sortSelect, filterBox),
    v.prs.length === 0 ? h("p", { class: "empty muted" }, "No open pull requests.") : null,
    v.prs.length && !shown.length ? h("p", { class: "empty muted" }, `No PR matches “${filter}”.`) : null,
    shown.length ? h("div", { class: "list", role: "listbox", "aria-label": "Pull requests", "aria-activedescendant": selected ? `pr-${selected}` : null },
      groups.map((g) => h("div", { role: "group", "aria-labelledby": `group-${g.group}` },
        h("h2", { class: `group g-${g.group}`, id: `group-${g.group}` }, `${GROUP_LABELS[g.group]} (${g.items.length})`),
        g.items.map((p) => card(p, now))))) : null,
    actionBar(),
  );
  scrollTo(0, scroll);
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
  if (caret) filterBox.setSelectionRange(caret[0], caret[1]);
}

addEventListener("keydown", (e) => {
  if (gate.keyDown(e.code, e.key, e.repeat) && outward) { e.preventDefault(); return; }
  if (!view) return;
  const t = e.target as HTMLElement;
  if (typing(e)) {
    if (t.id === "filter") {
      if (e.key === "Escape") {
        e.preventDefault();
        if (filter) { filter = ""; render(); document.getElementById("filter")?.focus(); }
        else if (selected) select(selected);
      } else if (e.key === "Enter" || e.key === "ArrowDown") { e.preventDefault(); if (selected) select(selected); }
    }
    return;
  }
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (outward) {
    if (key === "Escape") { e.preventDefault(); cancelOutward(); }
    else if (key === "Enter") {
      // Only an armed confirm sends; a focused button gets no native activation from this key.
      e.preventDefault();
      if (t.id === "confirm-outward" || t.tagName !== "BUTTON") confirmOutward();
      else t.click();
    }
    return;
  }
  if (key === "Enter" && t.tagName === "BUTTON") return; // the button's own click
  if (key === "Escape" && showKeys) { e.preventDefault(); setKeys(false); return; }
  if (key === "?") { e.preventDefault(); setKeys(!showKeys); return; }
  // Keys act on the card that has focus; if the selection moved (the agent went on), take it back.
  const onCard = focusedCard();
  if (onCard !== null && onCard !== selected && !["j", "k", "ArrowDown", "ArrowUp", "Home", "End"].includes(key)) select(onCard, false);
  if (e.repeat && !["j", "k", "ArrowDown", "ArrowUp"].includes(key)) return;
  if (key === "j" || key === "ArrowDown") { e.preventDefault(); move(1); }
  else if (key === "k" || key === "ArrowUp") { e.preventDefault(); move(-1); }
  else if (key === "Home" && t.getAttribute("role") === "option") { e.preventDefault(); move(-Infinity); }
  else if (key === "End" && t.getAttribute("role") === "option") { e.preventDefault(); move(Infinity); }
  else if (key === "Enter" || key === "o") { e.preventDefault(); open(selectedPr()); }
  else if (key === "h") { e.preventDefault(); hold(selectedPr()); }
  else if (key === "n") { e.preventDefault(); next(); }
  else if (key === "/") { e.preventDefault(); document.getElementById("filter")?.focus(); }
  else if (key === "Escape" && notice) { notice = null; render(); }
  else {
    const a = actionForKey(view.actions, key);
    if (a) { e.preventDefault(); runAction(a); }
  }
});
addEventListener("keyup", (e) => gate.keyUp(e.code));
addEventListener("blur", () => gate.blur());

startApp({
  name: "Muster PR overview",
  input: (data) => {
    const r = validateOverview(data);
    if (!r.ok) {
      view = null;
      outward = null;
      gate.inputTarget(null);
      problems(root, "This PR overview panel's input is invalid", r.errors);
      return;
    }
    const next = r.value;
    if (view && view.repo !== next.repo) { userSort = null; filter = ""; selected = null; }
    // The agent moved on to a PR: follow it, unless the user is on a card (then their pick stays,
    // so Enter and H never act on a PR they did not choose).
    const before = selected;
    selected = followCurrent(selected, view?.current, next.current, focusedCard() !== null);
    const moved = selected !== before;
    if (outward) {
      const a = next.actions.find((x) => x.id === outward!.action.id);
      const pr = next.prs.find((x) => x.number === outward!.pr.number);
      const t = a && pr && next.repo === view?.repo ? outwardTarget(a, target(pr, next.repo)) : null;
      if (gate.inputTarget(t)) { outward = null; notice = { text: "The PR or the action changed while you were confirming. Check it, then try again.", tone: "warning" }; }
      else if (a && pr) outward = { action: a, pr };
    } else if (sent.size) notice = null;
    // Any update is the agent's reply to what was sent.
    sent.clear();
    view = next;
    warnings = r.warnings;
    render();
    // Show where the selection went (focus stays where the user left it).
    if (moved && selected !== null) reveal(document.getElementById(`pr-${selected}`), document.querySelector(".actions"));
  },
});
