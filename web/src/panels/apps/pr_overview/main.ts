// The PR overview panel: every open PR of a repo as a card, the agent's triage on each, and the
// user's pick sent back (open, hold, next, or one of their own actions). Every event is sent
// synchronously from the click or key handler that caused it.
import { openLink, sendEvent, startApp, typing } from "../shared/client.ts";
import { h, mount, problems, reveal } from "../shared/dom.ts";
import { iconEl } from "../../kit/icons.ts";
import { avatarColor } from "../../kit/parts.ts";
import { rich } from "../../kit/format.ts";
import { Gate } from "../shared/confirm.ts";
import { actionForKey, type CustomAction, customEvent, outwardTarget } from "../shared/actions.ts";
import { actionsGroup, helpButton, outwardDrawer, shortcutsDrawer } from "../shared/actions_view.ts";
import {
  age, arrange, canHold, followCurrent, GROUP_LABELS, holdEvent, type Item, nextEvent, nextPr, openEvent, type Overview, queue,
  quickMergeBlocked, quickMergeEvent, quickTarget, quickWins, type Sort, SORT_LABELS, SORTS, size, summary, validateOverview,
} from "./logic.ts";
import { mergeCommand } from "../shared/merge.ts";

type Tone = "info" | "danger" | "warning" | "success";
const root = document.getElementById("app")!;

let view: Overview | null = null;
let warnings: string[] = [];
/** The user's sort once they pick one; until then the agent's. */
let userSort: Sort | null = null;
let filter = "";
let selected: number | null = null;
let notice: { text: string; tone: Tone } | null = null;
/** `kind:number` of events sent since the last update (with the send that set it): not sent
 * twice before the agent answers. */
const sent = new Map<string, number>();
let sendSeq = 0;
let outward: { action: CustomAction; pr: Item } | null = null;
/** The quick-win card whose Merge is being confirmed (the same armed gate as outward actions). */
let merging: Item | null = null;
/** Show only the quick wins. */
let quickOnly = false;
/** "Your actions" unfolded in a small panel; the shortcut list shown. */
let actionsOpen = false;
let showKeys = false;
const gate = new Gate();

const sortNow = () => userSort ?? view?.sort ?? "risk";
const ordered = () => view ? arrange(view.prs, sortNow(), filter, quickOnly).flatMap((g) => g.items) : [];
const selectedPr = () => view?.prs.find((p) => p.number === selected) ?? null;
/** The one place a PR's target is built: what its events name, and what a confirm is bound to. */
const target = (pr: Item, repo = view!.repo) => ({ repo, number: pr.number, head_sha: pr.head_sha, status: pr.status });
/** The card that has the keyboard, if any: keys act on it, never on a selection moved elsewhere. */
const focusedCard = () => {
  const el = document.activeElement as HTMLElement | null;
  return el?.getAttribute("role") === "option" && el.id.startsWith("pr-") ? Number(el.id.slice(3)) : null;
};
const KEYS: [string, string][] = [["J / ↓", "Next card"], ["K / ↑", "Previous card"], ["Enter / O", "Open the PR: the agent reviews it"],
  ["M", "Merge a quick win (shows the exact command first)"], ["H", "Hold: put aside for later"], ["N", "Go through the PRs in this order"], ["/", "Filter (Esc clears)"], ["?", "These shortcuts"]];

function setKeys(on: boolean) {
  showKeys = on;
  render();
  document.getElementById(on ? "shortcuts-close" : "keys-help")?.focus();
}

const riskText = { high: ["High risk", "danger"], medium: ["Medium risk", "warning"], low: ["Low risk", "success"] } as const;
const reviewText = { APPROVED: ["Approved", "success"], CHANGES_REQUESTED: ["Changes requested", "danger"], REVIEW_REQUIRED: ["Review required", "muted"], NONE: ["No review", "muted"] } as const;
const statusText = {
  new: ["", "muted"], ready: ["Review ready", "info"], reviewing: ["Reviewing", "info"], merging: ["Merging…", "info"], merged: ["Merged", "success"],
  queued: ["Queued", "info"], held: ["Held", "warning"],
  skipped: ["Skipped", "muted"], changes_requested: ["Changes requested", "warning"], failed: ["Failed", "danger"],
} as const;


function send(ev: { kind: string; text: string; data: object }, key: string, ok: string) {
  // Synchronous post inside the user's click or key handler.
  const result = sendEvent(ev.kind, ev.text, ev.data);
  const mine = ++sendSeq;
  sent.set(key, mine);
  notice = { text: ok, tone: "info" };
  render();
  // A late refusal frees only its own mark: never one a later send set after an update.
  result.catch((e) => { if (sent.get(key) === mine) sent.delete(key); notice = { text: `Not sent (${e.message}). Try again.`, tone: "danger" }; render(); });
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

/** Merge on a quick-win card: shows the exact command first, like pr_decision. */
function openMerge(pr: Item | null) {
  if (!view || !pr) return;
  const blocked = quickMergeBlocked(pr, sent.has(`merge:${pr.number}`));
  if (blocked) { notice = { text: blocked, tone: "warning" }; render(); return; }
  outward = null;
  merging = pr;
  gate.openTarget(quickTarget(view.repo, pr));
  notice = null;
  render();
  document.getElementById("confirm-merge")?.focus();
}

function confirmMerge() {
  if (!view || !merging) return;
  const pr = merging;
  // Not yet armed (the key that opened it is still held), or the card changed.
  if (!gate.canConfirmTarget(quickTarget(view.repo, pr))) return;
  const blocked = quickMergeBlocked(pr, sent.has(`merge:${pr.number}`));
  if (blocked) { cancelConfirm(); notice = { text: blocked, tone: "warning" }; render(); return; }
  const event = quickMergeEvent(view.repo, pr);
  merging = null;
  gate.close();
  send(event, `merge:${pr.number}`, `Merge #${pr.number} sent to the agent: it checks the PR is unchanged, runs ${mergeCommand(view.repo, pr.number, pr.merge_method_default, pr.delete_branch_default, pr.head_sha!)} and reports on the card.`);
  document.getElementById(`pr-${pr.number}`)?.focus();
}

function mergeDrawer(repo: string, pr: Item) {
  const command = mergeCommand(repo, pr.number, pr.merge_method_default, pr.delete_branch_default, pr.head_sha!);
  return h("div", { class: "drawer", role: "group", "aria-labelledby": "confirm-title" },
    h("div", { id: "confirm-title", class: "drawer-title" }, `Merge #${pr.number} into ${pr.base} in ${repo}? The agent will run exactly:`),
    h("pre", { class: "command", "aria-label": "Merge command" }, h("code", null, command)),
    pr.verdict ? h("p", { class: "verdict-line" }, h("strong", null, "Quick win: "), pr.verdict) : null,
    h("p", { class: "muted small" }, `--match-head-commit ties the merge to ${pr.head_sha!.slice(0, 7)}, the commit the agent reviewed: if anyone pushed since, GitHub refuses. The agent re-checks the PR first.`),
    h("div", { class: "row" },
      h("button", { type: "button", id: "confirm-merge", class: "primary", onclick: () => confirmMerge() }, "Confirm merge ", h("kbd", null, "Enter")),
      h("button", { type: "button", onclick: () => cancelConfirm() }, "Cancel ", h("kbd", null, "Esc"))));
}

function runAction(a: CustomAction) {
  const pr = selectedPr();
  if (!view || !pr) { notice = { text: "Select a PR first.", tone: "warning" }; render(); return; }
  if (sent.has(`custom:${a.id}:${pr.number}`)) return;
  if (a.outward) {
    merging = null;
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

/** Close the open confirm (an outward action's or a Merge) and hand the keyboard back. */
function cancelConfirm() {
  const back = outward ? `act-${outward.action.id}` : merging ? "merge" : null;
  outward = null;
  merging = null;
  gate.close();
  render();
  if (back) document.getElementById(back)?.focus();
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

const KIT_TONE: Record<string, string> = { info: "accent", success: "good", warning: "warn", danger: "bad", muted: "plain" };
function badge(text: string, tone: string, extra = "") { return h("span", { class: `mk-badge badge ${KIT_TONE[tone] ?? tone} ${extra}`.trim() }, text); }

const checkIcon = { passing: ["check", "good", "Passing"], failing: ["x", "bad", "Failing"], pending: ["clock", "warn", "Pending"], none: ["minus", "plain", "No checks"] } as const;

/** + and − in proportion, on a bar whose length grows with the size of the change. */
function sizeBar(pr: Item) {
  const add = pr.additions ?? 0, del = pr.deletions ?? 0, tot = add + del;
  const w = Math.min(46, 8 + Math.log10(tot + 1) * 14);
  return h("span", { class: "size ds2", title: `${size(pr)} lines changed` },
    h("span", { class: "plus a" }, `+${add}`), h("span", { class: "minus d" }, `−${del}`),
    tot ? h("span", { class: "bar", style: `width:${w.toFixed(0)}px`, "aria-hidden": "true" },
      h("i", { class: "a", style: `width:${((add / tot) * 100).toFixed(1)}%` }), h("i", { class: "d", style: `width:${((del / tot) * 100).toFixed(1)}%` })) : null);
}

function card(pr: Item, now: number) {
  const st = statusText[pr.status];
  const sizeKnown = pr.additions !== undefined || pr.deletions !== undefined;
  const isSel = pr.number === selected;
  const ck = pr.checks ? checkIcon[pr.checks] : null;
  const quickReady = pr.quick_win && pr.status !== "merged" && pr.status !== "queued";
  const mergeOff = quickReady ? quickMergeBlocked(pr, sent.has(`merge:${pr.number}`)) : null;
  return h("div", {
    class: `card mk-card prc risk-${pr.risk ?? "none"}${isSel ? " selected" : ""}${pr.number === view!.current ? " current" : ""} g-${pr.status}`,
    id: `pr-${pr.number}`, role: "option", "aria-selected": String(isSel), tabindex: isSel ? 0 : -1, "aria-keyshortcuts": quickReady ? "M" : null,
    onclick: () => { if (selected !== pr.number) select(pr.number); },
    ondblclick: () => open(pr),
  },
  h("div", { class: "h" },
    pr.number === view!.current ? h("span", { class: "badge now" }, "Now") : null,
    h("span", { class: "num no" }, `#${pr.number}`),
    h("span", { class: "title tt" }, pr.title)),
  h("div", { class: "bd" },
    pr.quick_win ? h("span", { class: "mk-badge badge accent plain quick" }, iconEl("zap"), "Quick win") : null,
    pr.risk ? badge(riskText[pr.risk][0], riskText[pr.risk][1], "risk") : null),
  h("div", { class: "meta m" },
    h("span", { class: "who" }, h("span", { class: "mk-avatar", style: `--a:${avatarColor(pr.author)}`, "aria-hidden": "true" }, pr.author[0]), pr.author),
    pr.created !== undefined ? h("span", null, age(pr.created, now)) : null,
    sizeKnown ? sizeBar(pr) : null,
    pr.files !== undefined ? h("span", null, `${pr.files} file${pr.files === 1 ? "" : "s"}`) : null,
    ck ? h("span", { class: `ck ${ck[1]}` }, iconEl(ck[0]), ck[2] === "Failing" ? "Checks failing" : ck[2]) : null,
    pr.review ? h("span", null, reviewText[pr.review][0]) : null,
    pr.draft ? h("span", { class: "draft" }, "Draft") : null,
    pr.labels.map((l) => h("span", { class: "mk-chip label" }, l))),
  pr.verdict || pr.summary ? h("p", { class: "s summary" }, pr.verdict ? h("b", { class: "verdict-line" }, pr.verdict, " ") : null, pr.summary ?? "") : null,
  quickReady || st[0] || pr.status_detail ? h("div", { class: "st" },
    st[0] ? badge(st[0], st[1], "status") : null,
    pr.status_detail ? h("span", { class: "detail" }, pr.status_detail) : null,
    quickReady && pr.head_sha ? h("span", { class: "reviewed" }, iconEl("check"), `Reviewed at ${pr.head_sha.slice(0, 7)}, ${pr.merge_method_default}${pr.delete_branch_default ? ", delete branch" : ""}`) : null,
    // A pointer shortcut only: an option may hold no controls, so screen readers and the keyboard
    // reach the same Merge as M on the card or the action bar's Merge (#merge-selected).
    quickReady ? h("button", { type: "button", id: `merge-${pr.number}`, class: "sm primary merge-quick", "aria-hidden": "true", tabindex: -1, disabled: !!mergeOff || !!merging,
      title: mergeOff ?? `Merge #${pr.number}: ${pr.verdict ?? ""}`, onclick: (e: Event) => { e.stopPropagation(); if (selected !== pr.number) select(pr.number, false); openMerge(pr); } },
    iconEl("merge"), `Merge #${pr.number}`, h("kbd", null, "M")) : null) : null);
}

function actionBar() {
  const v = view!;
  const pr = selectedPr();
  const nextUp = nextPr(ordered(), v.current);
  const busy = (k: string) => !!pr && sent.has(`${k}:${pr.number}`);
  const quick = pr && pr.quick_win && pr.status !== "merged" && pr.status !== "queued" ? pr : null;
  return h("div", { class: `actions${outward || merging || showKeys ? " has-drawer" : ""}` },
    showKeys ? shortcutsDrawer(KEYS, v.actions, () => setKeys(false)) : null,
    outward ? outwardDrawer(outward.action, `#${outward.pr.number}`, () => confirmOutward(), () => cancelConfirm()) : null,
    merging ? mergeDrawer(v.repo, merging) : null,
    notice ? h("div", { class: `notice ${notice.tone}`, role: notice.tone === "danger" ? "alert" : "status" }, notice.text) : null,
    h("div", { class: "row bar" },
      h("button", { type: "button", id: "open", class: "primary", "aria-keyshortcuts": "Enter O", disabled: !pr || busy("open"), onclick: () => open(selectedPr()) },
        pr ? `Open #${pr.number}` : "Open", " ", h("kbd", null, "Enter")),
      quick ? h("button", { type: "button", id: "merge-selected", "aria-keyshortcuts": "M", disabled: !!quickMergeBlocked(quick, sent.has(`merge:${quick.number}`)) || !!merging,
        title: quickMergeBlocked(quick, sent.has(`merge:${quick.number}`)) ?? `Merge the quick win #${quick.number} (shows the exact command first)`, onclick: () => openMerge(selectedPr()) },
        iconEl("merge"), `Merge #${quick.number}`, " ", h("kbd", null, "M")) : null,
      h("button", { type: "button", id: "hold", class: "ghost", "aria-keyshortcuts": "H", disabled: !pr || !canHold(pr) || busy("hold"), onclick: () => hold(selectedPr()) }, "Hold ", h("kbd", null, "H")),
      h("button", { type: "button", id: "next", class: "ghost", "aria-keyshortcuts": "N", disabled: !nextUp, title: nextUp ? `Next: #${nextUp.number}` : "Nothing left to review", onclick: () => next() },
        "Next", nextUp ? ` #${nextUp.number}` : "", " ", h("kbd", null, "N")),
      helpButton(() => setKeys(!showKeys), showKeys),
      h("span", { class: "spacer" }),
      pr?.url ? h("button", { type: "button", id: "github", class: "ghost", onclick: () => openLink(pr.url!).catch((e) => { notice = { text: e.message, tone: "danger" }; render(); }) }, iconEl("external"), "GitHub") : null),
    actionsGroup(v.actions, runAction, (a) => !pr || sent.has(`custom:${a.id}:${pr.number}`), actionsOpen,
      () => { actionsOpen = !actionsOpen; render(); document.getElementById("your-actions")?.focus(); }),
    h("p", { class: "keys muted small" }, `J/K move · Enter open${quickWins(v.prs).length ? " · M merge a quick win" : ""} · H hold · N next · / filter · ? keys`));
}

function render() {
  if (!view) return;
  const v = view;
  const scroll = scrollY;
  const active = document.activeElement as HTMLInputElement | null;
  const focusId = active?.id;
  const caret = focusId === "filter" ? [active!.selectionStart, active!.selectionEnd] : null;
  const groups = arrange(v.prs, sortNow(), filter, quickOnly);
  const wins = quickWins(v.prs).length;
  const shown = groups.flatMap((g) => g.items);
  if (!shown.some((p) => p.number === selected)) selected = shown[0]?.number ?? null;
  const c = summary(v.prs);
  const now = Date.now();
  const sortSeg = h("div", { class: "mk-seg sort", role: "group", "aria-label": "Sort" }, SORTS.map((x) => h("button", {
    type: "button", id: `sort-${x}`, "aria-pressed": String(x === sortNow()), onclick: () => { userSort = x; render(); document.getElementById(`sort-${x}`)?.focus(); },
  }, SORT_LABELS[x])));
  const filterBox = h("input", { type: "search", id: "filter", class: "mk-input", placeholder: "Filter: title, author, label", "aria-label": "Filter PRs (/)", autocomplete: "off" }) as HTMLInputElement;
  filterBox.value = filter;
  filterBox.addEventListener("input", () => { filter = filterBox.value; render(); });
  const head = h("header", { class: "app-head head" });
  head.innerHTML = `<h1 class="mk-title" tabindex="-1"></h1>${v.takeaway ? `<p class="mk-takeaway">${rich(v.takeaway)}</p>` : ""}`;
  head.querySelector("h1")!.append(v.title ?? "Open pull requests", " ", h("span", { class: "mk-scope" }, v.repo));
  const dot = (n: number, what: string, color: string) => n ? h("span", { style: `--c:${color}` }, h("i"), h("b", null, String(n)), ` ${what}`) : null;

  mount(root,
    head,
    h("p", { class: "po-sum", id: "counts" },
      h("span", null, h("b", null, String(c.open)), " open"),
      dot(c.quick, `quick win${c.quick === 1 ? "" : "s"}`, "var(--mk-accent)"),
      dot(c.failing, "failing checks", "var(--mk-bad)"),
      dot(c.review, "need review", "var(--mk-warn)"),
      dot(c.held, "held", "var(--mk-text-3)"),
      dot(c.merged, "merged", "var(--mk-good)")),
    warnings.length ? h("p", { class: "warnings" }, warnings.join("; ")) : null,
    h("div", { class: "tools po-tools" }, sortSeg,
      h("label", { class: "mk-search" }, iconEl("search"), filterBox, h("kbd", { "aria-hidden": "true" }, "/")),
      wins || quickOnly ? h("button", { type: "button", id: "quick-only", class: `sm qw quick-toggle${quickOnly ? " on" : ""}`, "aria-pressed": String(quickOnly),
        title: "Show only the quick wins: reviewed, small, green and low risk; each can be merged from here",
        onclick: () => { quickOnly = !quickOnly; render(); } }, iconEl("filter"), `Only quick wins (${wins})`) : null),
    v.prs.length === 0 ? h("p", { class: "empty muted" }, "No open pull requests.") : null,
    v.prs.length && !shown.length ? h("p", { class: "empty muted" }, quickOnly && !filter ? "No quick wins right now." : `No PR matches “${filter}”.`) : null,
    shown.length ? h("div", { class: "list", role: "listbox", "aria-label": "Pull requests", "aria-activedescendant": selected ? `pr-${selected}` : null },
      groups.map((g) => h("div", { role: "group", "aria-labelledby": `group-${g.group}` },
        h("h2", { class: `group grp g-${g.group}`, id: `group-${g.group}` }, `${GROUP_LABELS[g.group]} (${g.items.length})`),
        g.items.map((p) => card(p, now))))) : null,
    actionBar(),
  );
  scrollTo(0, scroll);
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
  if (caret) filterBox.setSelectionRange(caret[0], caret[1]);
}

addEventListener("keydown", (e) => {
  if (gate.keyDown(e.code, e.key, e.repeat) && (outward || merging)) { e.preventDefault(); return; }
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
  if (outward || merging) {
    if (key === "Escape") { e.preventDefault(); cancelConfirm(); }
    else if (key === "Enter") {
      // Only an armed confirm sends; a focused button gets no native activation from this key.
      e.preventDefault();
      const id = merging ? "confirm-merge" : "confirm-outward";
      if (t.id === id || t.tagName !== "BUTTON") { if (merging) confirmMerge(); else confirmOutward(); }
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
  else if (key === "m") { e.preventDefault(); openMerge(selectedPr()); }
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
      merging = null;
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
    } else if (merging) {
      // Any change to the card (or another repo) cancels the Merge confirm.
      const pr = next.prs.find((x) => x.number === merging!.number);
      if (gate.inputTarget(pr && next.repo === view?.repo ? quickTarget(next.repo, pr) : null)) {
        merging = null;
        notice = { text: "The PR changed while you were confirming. Check it, then Merge again.", tone: "warning" };
      } else if (pr) merging = pr;
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
