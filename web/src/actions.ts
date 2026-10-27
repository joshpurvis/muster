// User-facing actions. Components and the command table call these; they talk to the daemon and
// update local state (the window's layout of groups, prefs, windows).

import { signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import {
  agentBySession,
  agentMessages,
  agents,
  agentsPanelOpen,
  knowRun,
  knowSchedule,
  loadScheduleRuns,
  scheduleHistoryOpen,
  scheduleRuns,
  schedules,
  scheduleSettings,
  BUSY_STATES,
  channelById,
  confirmClose,
  firstSessionOf,
  focusedGroup,
  focusedSession,
  focusedTask,
  focusedTabId,
  fullscreen,
  knowPanel,
  panels,
  hereIds,
  holdNewSessions,
  knowAgent,
  knowChannel,
  knowSession,
  knowWindow,
  layout,
  meta,
  needsYou,
  overlay,
  prefs,
  pushRecent,
  pushRecentFolder,
  releaseNewSessions,
  renaming,
  sessionMark,
  sessions,
  sessionsByChannel,
  setPref,
  daemonStopped,
  dismissToastKey,
  saveLayoutNow,
  toast,
  updateLayout,
  windowId,
  windowOfSession,
  windows,
  windowPrefs,
  windowsSupported,
  workspaceById,
  workspaces,
} from "./state.ts";
import * as native from "./native.ts";
import { noteId, noteTab, panelId, panelTab } from "./layout/tree.ts";
import { flushAllDrafts, flushDraft, knowNote, notes, persistAllDrafts, unsavedNotes } from "./notes.ts";
import type { Note } from "./types.ts";
import { inheritedProfile } from "./account_profiles.ts";
import { providerPendingNote, upsertMessage } from "./agents.ts";
import { nextWhen } from "./schedules.ts";
import type {
  Agent,
  Schedule,
  ScheduleRun,
  ScheduleSettings,
  Channel,
  ChannelId,
  Decision,
  Permissions,
  ProfileChoice,
  SessionId,
  SessionInfo,
  SessionKind,
  Win,
  WindowId,
  WindowPrefs,
  CacheTtl,
  Workspace,
} from "./types.ts";
import { KIND_LABEL } from "./types.ts";
import {
  activateTab as activateTabTree,
  addTab,
  cycleTab,
  type Direction,
  type DropZone,
  equalize as equalizeTree,
  findPane,
  focusDirection,
  focusPane as focusPaneTree,
  type LayoutState,
  moveTab,
  moveTabToNeighbour,
  neighbour,
  paneOfSession,
  panePx,
  pathOfPane,
  resizeFocused,
  shiftTab,
  type Side,
  sideOf,
  type Size,
  splitFits,
  splitPane,
  toggleZoom,
} from "./layout/tree.ts";
import * as terms from "./terms.ts";
import { copyText } from "./util.ts";
import {
  basename,
  mainSession,
  midTurn,
  moveChannelLocally,
  newerPrefs,
  nextInCycle,
  renameChanges,
  renameFields,
  windowLabel,
} from "./model.ts";

export function fail(prefix: string) {
  return (e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e);
    toast("error", `${prefix}: ${msg}`);
    throw e;
  };
}

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
const unknownMethod = (e: unknown) => /^unknown method/i.test(errText(e));

// ---------- windows ----------

const WINDOW_KEY = "muster.window";

function storedWindow(): number | null {
  try {
    const v = Number(sessionStorage.getItem(WINDOW_KEY));
    return Number.isInteger(v) && v > 0 ? v : null;
  } catch {
    return null;
  }
}

/** Remember the window a launcher asked for (`#window=<id>`); it wins over a stored one. */
export function rememberWindow(id: number) {
  try {
    sessionStorage.setItem(WINDOW_KEY, String(id));
  } catch { /* storage unavailable: the claim still works for this page load */ }
}

/**
 * Claim this client's window (after every hello: a reconnect re-claims the same one). Without
 * window support every session is a tab here. A failed claim degrades to that too.
 */
let claiming: Promise<void> | null = null;

export function claimWindow(): Promise<void> {
  // One claim at a time (a lost-window event during a claim must not start a second one).
  claiming ??= doClaim().finally(() => (claiming = null));
  return claiming;
}

async function doClaim(): Promise<void> {
  if (!windowsSupported.value) {
    windowId.value = null;
    return;
  }
  const want = windowId.value ?? storedWindow();
  try {
    const w = await rpc.call<Win>("window.claim", want !== null ? { id: want } : {});
    knowWindow(w);
    windowId.value = w.id;
    rememberWindow(w.id);
    if (document.hasFocus()) windowFocused();
  } catch (e) {
    // The socket dropped: the next hello claims again.
    if (!rpc.isOpen) return;
    toast("error", `Couldn't claim a window: ${errText(e)}. Showing every tab here.`);
    windowsSupported.value = false;
    windowId.value = null;
  }
}

export function windowFocused() {
  const id = windowId.value;
  if (id !== null) rpc.call("window.focused", { id }).catch(() => {});
}

export function renameWindow(name: string) {
  const id = windowId.value;
  const n = name.trim();
  if (id === null || !n) return;
  rpc.call<Win>("window.rename", { id, name: n }).then(knowWindow)
    .catch(fail("Rename failed")).catch(() => {});
}

/**
 * Show window `id`: the daemon raises it if it's already open, else launches a client window for
 * it (the native window can't open another itself). Falls back to a browser window, then to a
 * hint.
 */
export function openClientWindow(id: WindowId) {
  rpc.call("window.show", { id }).catch(() => openClientWindowHere(id));
}

function openClientWindowHere(id: WindowId) {
  const url = `${location.origin}${location.pathname}#window=${id}`;
  let w: Window | null = null;
  try {
    w = open(url, "_blank");
  } catch { /* blocked */ }
  if (!w) {
    toast("info", `Run "muster open --window ${id}" to show ${windowLabel({ id, name: String(id) })}.`, 8000);
  }
}

/**
 * Move session `s` (a tab) to window `to` (`session.set_window`). Daemons without per-session
 * windows can only move whole tasks (`channel.move`), so there only a task's lone session moves.
 */
async function setSessionWindow(s: SessionInfo, to: WindowId, from: WindowId | null): Promise<void> {
  try {
    const mark = sessionMark();
    const r = await rpc.call<SessionInfo>("session.set_window", {
      id: s.id,
      window_id: to,
      from_window_id: from ?? undefined,
    });
    knowSession({ ...r, window_id: r.window_id ?? to }, mark);
  } catch (e) {
    if (!unknownMethod(e)) throw e;
    if ((sessionsByChannel.value.get(s.channel_id) ?? []).length > 1) {
      throw new Error("this daemon can only move a whole task; update muster to move one tab");
    }
    const c = await rpc.call<Channel>("channel.move", { id: s.channel_id, window_id: to });
    knowChannel({ ...c, window_id: c.window_id ?? to });
    windows.value = moveChannelLocally(windows.value, c.id, to);
  }
}

/** Bring session `sid` from another window into this window's focused group, as its active tab. */
async function pullHere(sid: SessionId): Promise<boolean> {
  const s = sessions.value.get(sid);
  const here = windowId.value;
  if (!s || here === null) return hereIds.value.has(sid);
  const group = layout.value?.focus;
  holdNewSessions();
  try {
    await setSessionWindow(s, here, windowOfSession(s));
    updateLayout((l) => addTab(l, sid, group), [sid]);
    return hereIds.value.has(sid);
  } catch (e) {
    toast("error", `Couldn't bring the tab here: ${errText(e)}`);
    return false;
  } finally {
    releaseNewSessions();
  }
}

/** Move a tab of this window to another window, or to a new one (which then opens). */
export async function moveTabToWindow(sid: SessionId, target: WindowId | "new") {
  const s = sessions.value.get(sid);
  const pid = panelId(sid);
  const panel = pid === null ? undefined : panels.value.get(pid);
  const nid = noteId(sid);
  const note = nid === null ? undefined : notes.value.get(nid);
  if (!s && !panel && !note) return;
  try {
    let to: WindowId;
    if (target === "new") {
      const w = await rpc.call<Win>("window.create", {});
      knowWindow(w);
      to = w.id;
    } else to = target;
    if (panel) await rpc.call("panel.move", { id: panel.id, window_id: to });
    else if (note) {
      await flushDraft(note.id);
      knowNote(await rpc.call<Note>("note.open", { id: note.id, window_id: to }));
    }
    else if (s) await setSessionWindow(s, to, windowId.value);
    const w = windows.value.find((x) => x.id === to);
    if (target === "new" || (w && !w.open)) openClientWindow(to);
    else toast("info", `Moved to ${windowLabel(w ?? { id: to, name: String(to) })}`, 2500);
  } catch (e) {
    toast("error", `Move failed: ${errText(e)}`);
  }
}

// ---------- selection / navigation ----------

/** Show session `sid` wherever it is: a tab here is selected; one in another window is pulled
 * into the focused group, or with "Jump focuses the other window" that window is raised (pulling
 * when that fails). */
export async function gotoSession(sid: SessionId) {
  const s = sessions.value.get(sid);
  if (!s) return;
  pushRecent("s:" + sid);
  if (!hereIds.value.has(sid)) {
    const other = windowOfSession(s);
    if (prefs.value.jumpRaises && other !== null && other !== windowId.value) {
      const raised = await rpc.call<{ raised: boolean }>("window.raise", { id: other })
        .then((r) => !!r?.raised).catch(() => false);
      if (raised) return;
    }
    if (!(await pullHere(sid))) return;
  }
  revealSession(sid);
}

/** Show panel `id` wherever it is: selected here, or moved here from another window (with
 * "Jump focuses the other window", that window is raised instead when it can be). */
export async function gotoPanel(id: number) {
  const p = panels.value.get(id);
  if (!p) return;
  const tab = panelTab(id);
  pushRecent("p:" + id);
  if (!hereIds.value.has(tab)) {
    const here = windowId.value;
    if (here === null) return;
    if (prefs.value.jumpRaises && p.window_id !== null && p.window_id !== here) {
      const raised = await rpc.call<{ raised: boolean }>("window.raise", { id: p.window_id })
        .then((r) => !!r?.raised).catch(() => false);
      if (raised) return;
    }
    try {
      await rpc.call("panel.move", { id, window_id: here });
      knowPanel({ ...p, window_id: here });
    } catch (e) {
      toast("error", `Couldn't bring the panel here: ${errText(e)}`);
      return;
    }
  }
  revealSession(tab);
}

/** Show note `id`: selected here, or opened here (from closed, or from another window; with
 * "Jump focuses the other window", that window is raised instead when it can be). */
export async function gotoNote(id: number, group?: string) {
  const n = notes.value.get(id);
  if (!n) return;
  const tab = noteTab(id);
  pushRecent("n:" + id);
  if (!hereIds.value.has(tab)) {
    const here = windowId.value;
    if (here === null) return;
    const other = n.window_id !== null && windows.value.some((w) => w.id === n.window_id) ? n.window_id : null;
    if (prefs.value.jumpRaises && other !== null && other !== here) {
      const raised = await rpc.call<{ raised: boolean }>("window.raise", { id: other })
        .then((r) => !!r?.raised).catch(() => false);
      if (raised) return;
    }
    if (group) focusGroup(group);
    try {
      knowNote(await rpc.call<Note>("note.open", { id, window_id: here }));
    } catch (e) {
      toast("error", `Couldn't open the note: ${errText(e)}`);
      return;
    }
  }
  revealSession(tab);
}

/** New note, as a tab of the focused group (or `group`), with the keyboard in it. */
export async function newNote(group?: string, title?: string) {
  const here = windowId.value;
  if (here === null) return toast("error", "Notes need a window: reconnect first");
  if (group) focusGroup(group);
  try {
    const n = await rpc.call<Note>("note.create", { window_id: here, ...(title ? { title } : {}) });
    knowNote(n);
    revealSession(noteTab(n.id));
  } catch (e) {
    toast("error", `Couldn't create a note: ${errText(e)}`);
  }
}

/** The notes list (open and closed notes; reopen, delete, new). */
export function openNotesList() {
  overlay.value = { kind: "notes" };
}

/** Delete note `id` for good, after the app's confirm. */
export function deleteNoteAsk(id: number, after?: () => void) {
  const n = notes.value.get(id);
  if (!n) return;
  const lines = n.lines;
  overlay.value = {
    kind: "confirm",
    title: `Delete “${n.title}”?`,
    body: [
      lines ? `Its ${lines === 1 ? "line" : `${lines} lines`} of text go for good, for you and for every agent.` : "It is empty.",
      "Closing its tab keeps a note; this removes it.",
    ],
    confirmLabel: "Delete note",
    danger: true,
    run: () => {
      rpc.call("note.delete", { id }).then(() => after?.()).catch(fail("Delete failed")).catch(() => {});
    },
  };
}

/** Rename note `id` (inline, in its tab). */
export const renamingNote = signal<number | null>(null);

export function startNoteRename(id: number) {
  exitFullscreen(false);
  renamingNote.value = id;
}

export function renameNote(id: number, title: string) {
  rpc.call<Note>("note.rename", { id, name: title }).then(knowNote).catch(fail("Rename failed")).catch(() => {});
}

/** Alt+J: the next session that needs you, in any window. */
let lastNeedsYou: SessionId | null = null;

export function nextNeedsYou() {
  const list = needsYou.value;
  // Cycle from the focused session when it needs you; else from the last one we went to (it may
  // be in another window that was raised, so focus here didn't move).
  const focused = focusedSession.value?.id ?? null;
  const from = focused !== null && list.some((s) => s.id === focused) ? focused : lastNeedsYou;
  const next = nextInCycle(list, from);
  if (!next) {
    toast("info", "Nothing needs you", 1500);
    return;
  }
  lastNeedsYou = next.id;
  gotoSession(next.id);
}

/** Focus is in a text field outside the terminals (an inline rename, a find bar…). */
function typingElsewhere(): boolean {
  const a = document.activeElement as HTMLElement | null;
  if (!a || a.closest(".term-el")) return false;
  return a.tagName === "INPUT" || a.tagName === "TEXTAREA" || a.isContentEditable;
}

export function focusActiveTerminalSoon() {
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      if (overlay.value || typingElsewhere()) return;
      const s = focusedSession.value;
      if (s) terms.focusTerminal(s.id);
      else {
        const tab = focusedTabId.value;
        const id = tab === null ? null : panelId(tab);
        const note = tab === null ? null : noteId(tab);
        if (id !== null) document.querySelector<HTMLIFrameElement>(`iframe[data-panel-id="${id}"]`)?.focus();
        else if (note !== null) document.querySelector<HTMLTextAreaElement>(`textarea[data-note-id="${note}"]`)?.focus();
      }
    })
  );
}

/** Make tab `sid` of this window the active one of its group, and focus that group. */
export function revealSession(sid: SessionId, focus = true) {
  if (!hereIds.value.has(sid)) return;
  pushRecent("s:" + sid);
  updateLayout((l) => activateTabTree(l, sid));
  if (focus) focusActiveTerminalSoon();
}

export function focusGroup(paneId: string) {
  if (layout.value?.focus === paneId) return;
  updateLayout((l) => focusPaneTree(l, paneId));
}

function withLayout(fn: (l: LayoutState) => LayoutState, focus = true) {
  updateLayout(fn);
  if (focus) focusActiveTerminalSoon();
}

export const focusDir = (d: Direction) => withLayout((l) => focusDirection(l, d));
export const resizeDir = (d: Direction) => withLayout((l) => resizeFocused(l, d, 0.04), false);
export const zoomToggle = () => withLayout((l) => toggleZoom(l));

/** Show tab `tab` (default: the focused tab) alone, filling the window; again (or Esc, or the
 * button) goes back to the layout as it was. Never saved: the layout itself is untouched. */
export function toggleFullscreen(tab: SessionId | null = focusedTabId.value) {
  const l = layout.value;
  if (tab === null || !l) return;
  const pane = paneOfSession(l.root, tab);
  if (!pane) return;
  if (fullscreen.value === pane.id && pane.active === tab) return exitFullscreen();
  revealSession(tab, false);
  focusGroup(pane.id);
  fullscreen.value = pane.id;
  const note = noteId(tab) !== null;
  const terminal = panelId(tab) === null && !note;
  toast("info", terminal ? "Fullscreen. Alt+Shift+Z goes back (Esc stays with the terminal)"
    : note ? "Fullscreen. Alt+Shift+Z goes back" : "Fullscreen. Esc or Alt+Shift+Z goes back", 2500, "fullscreen");
  if (terminal || note) focusActiveTerminalSoon();
  else refocusIfLost();
}

/** `refocus`: when the switch unmounts what had focus (a tab in the hidden strip, the corner
 * button), the keyboard goes to the tab shown. */
export function exitFullscreen(refocus = true) {
  if (fullscreen.value === null) return;
  fullscreen.value = null;
  dismissToastKey("fullscreen");
  if (refocus) refocusIfLost();
}

/** Entering or leaving fullscreen mounts and unmounts tab strips: focus inside one falls to
 * <body>, where no shortcut of a panel's app works. Give it to the visible tab instead. */
function refocusIfLost() {
  requestAnimationFrame(() => {
    const a = document.activeElement;
    if (!a || a === document.body || !a.isConnected) focusActiveTerminalSoon();
  });
}
export const tabStep = (delta: number) => withLayout((l) => cycleTab(l, delta));
export const equalizeAll = () =>
  withLayout((l) => {
    const p = pathOfPane(l.root, l.focus);
    return p && p.length ? equalizeTree(l, p.slice(0, -1)) : equalizeTree(l, []);
  });

/** Alt+1…9: tab N of the focused group. */
export function gotoTabIndex(n: number) {
  const sid = focusedGroup.value?.tabs[n];
  if (sid !== undefined) revealSession(sid);
}

/** Alt+Shift+[ / ]: move the focused tab within its group. */
export function shiftFocusedTab(delta: number) {
  const id = focusedTabId.value;
  if (id !== null) updateLayout((l) => shiftTab(l, id, delta));
}

/** Ctrl+Alt+arrow: move the focused tab to the neighbouring group (a new split at the edge). */
export function moveFocusedTab(dir: Direction) {
  const id = focusedTabId.value;
  const l = layout.value;
  if (id === null || !l) return;
  const from = paneOfSession(l.root, id);
  if (!from) return;
  if (!neighbour(l.root, from.id, dir)) {
    if (from.tabs.length < 2 || !canSplit(sideOf(dir), from.id)) return;
  }
  withLayout((x) => moveTabToNeighbour(x, id, dir));
}

/** A tab dropped on a group (its strip at `index`, its body's center, or an edge: a new split). */
export function dropTab(sid: SessionId, paneId: string, zone: DropZone, index?: number) {
  withLayout((l) => moveTab(l, sid, paneId, zone, index));
}

/** Context menu "Split right / down ▸ Move this tab": the tab goes into a new group beside its own. */
export function splitWithTab(sid: SessionId, side: Side) {
  const l = layout.value;
  const p = l ? paneOfSession(l.root, sid) : null;
  if (!p || p.tabs.length < 2 || !canSplit(side, p.id)) return;
  withLayout((x) => moveTab(x, sid, p.id, side));
}

// ---------- sessions ----------

/** Where a new session goes: a tab of a group (default: focused) or a new split beside it. */
export type Placement =
  | { type: "tab"; group?: string }
  | { type: "split"; side: Side; group?: string };

/** Size of the split area (the box the layout tree is laid out in). */
function splitBox(): Size | null {
  const el = document.querySelector(".panes") ?? document.querySelector(".content");
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 ? { w: r.width, h: r.height } : null;
}

/** Whether group `paneId` (default focused) has room to split on `side`. Toasts when not. */
export function canSplit(side: Side, paneId?: string, quiet = false): boolean {
  const l = layout.value;
  const box = splitBox();
  if (!l || !box) return true; // can't measure
  const id = paneId && findPane(l.root, paneId) ? paneId : l.focus;
  if (splitFits(l.root, id, side, box)) return true;
  if (!quiet) toast("warn", "Not enough room — zoom or close a group");
  return false;
}

/** Terminal grid (cols/rows) a session will get in `placement`, so it starts at the right size. */
function gridFor(placement: Placement): { cols: number; rows: number } | undefined {
  const l = layout.value;
  const box = splitBox();
  if (!l || !box) return undefined;
  const id = placement.group && findPane(l.root, placement.group) ? placement.group : l.focus;
  // A zoomed group fills the whole box; a split unzooms first. An empty window is one group.
  const px = (l.zoom === id && placement.type === "tab") || !document.querySelector(".panes")
    ? box
    : panePx(l.root, id, box);
  if (!px) return undefined;
  let { w, h } = px;
  if (placement.type === "split") {
    if (placement.side === "left" || placement.side === "right") w = w / 2 - 1;
    else h = h / 2 - 1;
  }
  return terms.estimateGrid(w, h - terms.PANE_STRIP_PX);
}

/** Put a session this client created on screen where `placement` says. */
function place(sid: SessionId, placement: Placement) {
  updateLayout(
    (l) =>
      placement.type === "split"
        ? splitPane(l, placement.side, sid, placement.group)
        : addTab(l, sid, placement.group),
    [sid],
  );
  focusActiveTerminalSoon();
}

/** A new session in task `ch` (a tab of this window). */
export async function spawn(
  ch: ChannelId,
  kind: SessionKind,
  placement: Placement = { type: "tab" },
  prompt?: string,
): Promise<SessionInfo | undefined> {
  if (placement.type === "split" && !canSplit(placement.side, placement.group)) return undefined;
  const at = { ...placement, group: placement.group ?? layout.value?.focus };
  const grid = gridFor(at);
  holdNewSessions();
  try {
    const mark = sessionMark();
    const s = await rpc.call<SessionInfo>("session.spawn", {
      channel_id: ch,
      kind,
      account_profile: inheritedProfile(kind, sessionOfGroup(at.group)),
      prompt: prompt || undefined,
      window_id: windowId.value ?? undefined,
      ...grid,
    });
    knowSession(s, mark);
    if (kind !== "shell") setPref("lastAgent", kind);
    place(s.id, at);
    return s;
  } catch (e) {
    toast("error", `Couldn't start ${kind}: ${errText(e)}`);
  } finally {
    releaseNewSessions();
  }
}

/** The last agent kind used (for "same agent" defaults). */
export function lastKind(): SessionKind {
  return prefs.value.lastAgent === "none" ? "shell" : prefs.value.lastAgent;
}

/** A new terminal of `kind` in the focused tab's task, as a tab of the focused group. */
export function spawnHere(kind: SessionKind) {
  const c = focusedTask.value;
  if (c) spawn(c.id, kind, { type: "tab" });
}

/** Alt+N: pick a kind for a new terminal in the focused tab's task (1/2/3, Enter repeats last). */
export async function spawnPicker() {
  const c = focusedTask.value;
  const l = layout.value;
  if (!c || !l) return;
  const group = l.focus;
  const anchor = document.querySelector(`[data-pane="${group}"]`);
  if (!anchor) {
    spawnHere(lastKind());
    return;
  }
  const { openMenu } = await import("./components/Menu.tsx");
  const { spawnMenuItems } = await import("./layout/Pane.tsx");
  const r = anchor.getBoundingClientRect();
  openMenu({
    x: r.left + r.width / 2 - 100,
    y: r.top + Math.min(r.height / 3, 160),
    items: [
      { header: "New terminal in this task" },
      ...spawnMenuItems((k) => spawn(c.id, k, { type: "tab", group })),
    ],
    minWidth: 200,
    select: 1 + ["claude", "codex", "shell"].indexOf(lastKind()),
    onClose: () => focusActiveTerminalSoon(),
  });
}

/** Alt+D / Alt+Shift+D: split the focused group with a new terminal in the focused tab's task. */
export function splitFocused(side: Side, kind: SessionKind = "shell") {
  const c = focusedTask.value;
  if (c) spawn(c.id, kind, { type: "split", side });
}

/**
 * Close a tab: removes its session (the daemon closes a task left without sessions; older
 * daemons are asked to). A busy session gets an inline "Close? ⏎ / Esc" first. A named agent's
 * tab is only hidden: the agent keeps running, and its row in the agents panel shows it again
 * (only the panel's Stop stops it). The daemon decides that too (`session.remove` hides an
 * agent's session), so closing before the agent list has loaded can't stop one.
 */
export function closeTab(sid: SessionId, force = false) {
  const nid = noteId(sid);
  if (nid !== null) {
    // Closing a note's tab keeps the note (the notes list and Jump reopen it). Unsaved typing
    // goes first, so the reopened note has it.
    flushDraft(nid).then(() => rpc.call<Note>("note.close", { id: nid })).then((n) => {
      knowNote(n);
      focusActiveTerminalSoon();
    }).catch(fail("Close failed")).catch(() => {});
    return;
  }
  const pid = panelId(sid);
  if (pid !== null) {
    // Focus was on the panel or its close button: once it is gone, the tab that takes its
    // place gets the keyboard (otherwise focus falls to <body> and typing goes nowhere).
    rpc.call("panel.dismiss", { id: pid }).then(() => {
      const p = panels.value.get(pid);
      if (p) knowPanel({ ...p, closed: true });
      focusActiveTerminalSoon();
    }).catch(fail("Close failed")).catch(() => {});
    return;
  }
  const s = sessions.value.get(sid);
  if (!s) return;
  const agent = agentBySession.value.get(sid);
  if (agent) {
    confirmClose.value = null;
    return hideAgentTab(s, agent.name, force);
  }
  if (!force && BUSY_STATES.has(s.state)) {
    confirmClose.value = sid;
    return;
  }
  confirmClose.value = null;
  removeTab(s);
}

function removeTab(s: SessionInfo) {
  const sid = s.id;
  const last = (sessionsByChannel.value.get(s.channel_id) ?? []).every((x) => x.id === sid);
  const mark = sessionMark();
  rpc.call<{ hidden?: boolean; session?: SessionInfo }>("session.remove", { id: sid })
    .then((r) => {
      // The daemon hides a named agent's tab instead (this client may not know it is one yet).
      if (r?.hidden && r.session) {
        knowSession(r.session, mark);
        return agentHidden(r.session);
      }
      if (last) rpc.call("channel.close_if_empty", { id: s.channel_id }).catch(() => {});
    })
    .catch(fail("Close failed")).catch(() => {});
}

/** An agent's tab was closed (hidden): say it keeps running, with a Show that brings it back
 * (works whatever "Terminal gets Alt keys" says; Alt+G is named only when it reaches the app). */
function agentHidden(s: SessionInfo) {
  focusActiveTerminalSoon();
  if (s.state === "exited") return;
  const name = agentBySession.value.get(s.id)?.name ?? s.name;
  const where = prefs.value.altToTerminal ? "the agents panel" : "the agents panel (Alt+G)";
  toast("info", `${name} keeps running in the background. Show it again from ${where}.`, 7000, undefined, {
    label: "Show",
    run: () => openAgent(name),
  });
}

/** Close a named agent's tab without stopping it (`session.hide`). Daemons without it close
 * the tab as before, which stops the agent, so a busy one asks first there. */
function hideAgentTab(s: SessionInfo, name: string, force: boolean) {
  const mark = sessionMark();
  rpc.call<SessionInfo>("session.hide", { id: s.id }).then((r) => {
    knowSession(r, mark);
    agentHidden({ ...r, name });
  }).catch((e) => {
    if (!unknownMethod(e)) return toast("error", `Close failed: ${errText(e)}`);
    if (!force && BUSY_STATES.has(s.state)) confirmClose.value = s.id;
    else removeTab(s);
  });
}

/** Alt+W: close the focused tab. */
export function closeFocused() {
  const id = focusedTabId.value;
  if (id !== null) closeTab(id);
}

/** Close a task (all its tabs) and delete its worktree (asks first: uncommitted work is lost). */
export function closeTaskRemoveWorktree(c: Channel) {
  overlay.value = {
    kind: "confirm",
    title: "Close tab and remove its worktree?",
    body:
      `Stops the task's terminals and deletes the worktree at ${c.cwd}. Uncommitted changes there are lost. The branch${
        c.branch ? ` ${c.branch}` : ""
      } is kept.`,
    confirmLabel: "Close and remove",
    danger: true,
    run: () =>
      rpc.call("channel.archive", { id: c.id, remove_worktree: true })
        .catch(fail("Close failed")).catch(() => {}),
  };
}

export function restartSession(sid: SessionId, focus = true) {
  rpc.call("session.restart", { id: sid }).then(() => {
    if (focus && focusedSession.value?.id === sid) focusActiveTerminalSoon();
  }).catch(fail("Restart failed")).catch(() => {});
}

export function killSession(sid: SessionId) {
  rpc.call("session.kill", { id: sid }).catch(fail("Kill failed")).catch(() => {});
}

/** After an input interruption: the user has checked the terminal and types again.
 * Nothing that may have been lost is resent. */
export function continueInput(sid: SessionId) {
  rpc.call("session.continue_input", { id: sid })
    .then(() => terms.focusTerminal(sid))
    .catch(fail("Could not continue typing")).catch(() => {});
}

export function interruptSession(sid: SessionId) {
  rpc.call("session.interrupt", { id: sid }).catch(fail("Interrupt failed")).catch(() => {});
}

/** Restart / Interrupt from a tab or terminal menu: asks first while an agent is mid-turn (its
 * turn is lost). Palette commands and shortcuts act at once. Known gap: the dialog holds only the
 * session id, so if another client restarts that session while it is open, confirming acts on
 * the replacement run. */
export function restartOrInterruptAsk(sid: SessionId, what: "restart" | "interrupt") {
  const s = sessions.value.get(sid);
  const act = () => (what === "restart" ? restartSession(sid) : interruptSession(sid));
  if (!s || !midTurn(s)) return act();
  overlay.value = {
    kind: "confirm",
    title: `${KIND_LABEL[s.kind]} is working`,
    body: what === "restart"
      ? s.kind === "claude"
        ? "Restart it? Its current turn is lost; it reopens the same conversation."
        : "Restart it? Its current turn is lost and the program starts fresh."
      : "Stop its current turn?",
    confirmLabel: what === "restart" ? "Restart" : "Interrupt",
    danger: true,
    run: act,
  };
}

export function renameSession(sid: SessionId, name: string) {
  const n = name.trim();
  const s = sessions.value.get(sid);
  if (!n || !s || n === s.name) return;
  const mark = sessionMark();
  rpc.call<SessionInfo>("session.rename", { id: sid, name: n }).then((r) => knowSession(r, mark))
    .catch(fail("Rename failed")).catch(() => {});
}

/** Rename (double-click, F2, menus, palette): edit exactly the names tab `sid` shows, inline in that
 * tab. `from`: where the keyboard goes back to when it closes (the tab, or its terminal). */
export function startRename(sid: SessionId, from: "tab" | "terminal" = "tab") {
  const s = sessions.value.get(sid);
  if (!s) return;
  // The rename fields live in the tab strip, which fullscreen hides.
  exitFullscreen(false);
  const c = channelById.value.get(s.channel_id);
  const list = sessionsByChannel.value.get(s.channel_id) ?? [];
  const ws = c ? workspaceById.value.get(c.workspace_id) : undefined;
  const fields = renameFields(s, c, ws, list.length, mainSession(list)?.id === sid);
  renaming.value = { sid, channel: c ? c.id : null, fields, from };
}

type Renaming = NonNullable<typeof renaming.value>;

/** Inline rename `r` was committed: rename the task and/or the session, whichever changed.
 * `byKey`: closed with Enter (see `endRename`). */
export function commitRename(r: Renaming, v: { task?: string; session?: string }, byKey: boolean) {
  // An editor whose rename already ended (its tab left, a newer one started) renames nothing.
  if (renaming.value !== r) return;
  endRename(r, byKey);
  const ch = renameChanges(r.fields, v);
  if (ch.task !== undefined && r.channel !== null) renameChannel(r.channel, ch.task);
  if (ch.session !== undefined) renameSession(r.sid, ch.session);
}

/** Inline rename `r` is over (never a newer one). Closed from the keyboard, focus goes back to
 * where it was opened (its tab, or its terminal) instead of falling to <body>; closed by clicking
 * away, it stays where the click put it. */
export function endRename(r: Renaming, byKey: boolean) {
  if (renaming.value === r) renaming.value = null;
  if (!byKey) return;
  if (r.from === "terminal") terms.focusTerminal(r.sid);
  else document.querySelector<HTMLElement>(`.tab[data-tab-sid="${r.sid}"]`)?.focus();
}

export function approve(sid: SessionId, decision: Decision) {
  rpc.call("session.approve", { id: sid, decision })
    .catch(fail(decision === "deny" ? "Deny failed" : "Approve failed")).catch(() => {});
}

export function approveFocused(decision: Decision) {
  const s = focusedSession.value;
  if (s && (s.state === "needs_approval" || s.state === "asking_question")) approve(s.id, decision);
}

// ---------- tasks (new tabs) ----------

function expandHome(path: string): string {
  const home = meta.value.home;
  return home ? path.replace(/^~(?=\/|$)/, home) : path;
}

function samePath(a: string, b: string): boolean {
  const n = (p: string) => expandHome(p.trim()).replace(/\/+$/, "") || "/";
  return n(a) === n(b);
}

/** The workspace for a folder, added behind the scenes if needed (`workspace.add` is idempotent). */
export async function ensureWorkspace(path: string): Promise<Workspace> {
  const known = workspaces.value.find((w) => samePath(w.path, path));
  if (known) return known;
  const w = await rpc.call<Workspace>("workspace.add", { path: path.trim() });
  if (!workspaces.value.some((x) => x.id === w.id)) workspaces.value = [...workspaces.value, w];
  return w;
}

export interface NewTabReq {
  account_profile?: ProfileChoice;
  /** Folder: an existing workspace, or a path (added as a workspace when new). */
  workspace_id?: number;
  path?: string;
  agent: SessionKind;
  worktree: boolean;
  base?: string;
  /** Blank: the daemon names the task. */
  name?: string;
  prompt?: string;
  /** Omitted: the daemon's defaults. */
  permissions?: Permissions;
  /** The group the tab joins (default: focused). */
  group?: string;
}

/** How long a new task's first session may take to show up before the tab counts as failed. */
const FIRST_SESSION_MS = 8000;

/**
 * A new tab: a new task (`channel.create`) whose first session joins group `req.group`. Fails
 * when that session never shows up (its start failed): the empty task is closed again.
 */
export async function createTab(req: NewTabReq): Promise<Channel> {
  const ws = req.workspace_id !== undefined
    ? workspaceById.value.get(req.workspace_id) ?? await ensureWorkspace(req.path ?? "")
    : await ensureWorkspace(req.path ?? "");
  const at: Placement = { type: "tab", group: req.group ?? layout.value?.focus };
  const here = windowId.value;
  holdNewSessions();
  try {
    const c = await rpc.call<Channel>("channel.create", {
      workspace_id: ws.id,
      // Daemons without auto naming need a name: the folder's.
      name: req.name?.trim() || (windowsSupported.value ? undefined : basename(ws.path)),
      worktree: req.worktree,
      base: req.base?.trim() || undefined,
      agent: req.agent,
      account_profile: req.account_profile,
      prompt: req.prompt?.trim() || undefined,
      permissions: req.permissions,
      window_id: here ?? undefined,
      ...gridFor(at),
    });
    setPref("lastAgent", req.agent);
    pushRecentFolder(ws.path);
    knowChannel(here !== null ? { ...c, window_id: c.window_id ?? here } : c);
    // Optimistic: the window_upserted that lists the new task may lag the reply.
    if (here !== null) windows.value = moveChannelLocally(windows.value, c.id, here);
    const first = await firstSessionOf(c.id, FIRST_SESSION_MS);
    if (!first) {
      rpc.call("channel.close_if_empty", { id: c.id }).catch(() => {});
      throw new Error(`${req.agent} didn't start`);
    }
    place(first.id, at);
    return c;
  } finally {
    releaseNewSessions();
  }
}

/** The agent a task runs (its first agent session), for "same agent" shortcuts. */
function agentOf(ch: ChannelId): SessionKind | null {
  const list = sessionsByChannel.value.get(ch) ?? [];
  return list.find((s) => s.kind !== "shell")?.kind ?? list[0]?.kind ?? null;
}

/** The task of group `group`'s active tab (default: the focused tab's). */
function taskOfGroup(group?: string): Channel | null {
  const s = sessionOfGroup(group);
  return s ? channelById.value.get(s.channel_id) ?? null : null;
}

function sessionOfGroup(group?: string): SessionInfo | undefined {
  const l = layout.value;
  const p = group && l ? findPane(l.root, group) : null;
  const sid = p?.active ?? focusedSession.value?.id ?? null;
  return sid !== null ? sessions.value.get(sid) : undefined;
}

/**
 * `+` / Alt+T: a new tab in `group` (default focused), in the folder of that group's active tab
 * (its project, or where it has `cd`'d to outside it), with the last agent kind and no
 * worktree. Ctrl+Shift+N (`worktree`): a worktree tab with the focused tab's agent. No tab to
 * take a folder from: the New tab dialog.
 */
export function newTabHere(opts: { worktree?: boolean; agent?: SessionKind; group?: string } = {}) {
  const c = taskOfGroup(opts.group);
  if (!c) {
    newTabDialog(opts.worktree);
    return;
  }
  const ws = workspaceById.value.get(c.workspace_id);
  if (opts.worktree && ws && !ws.is_git) {
    toast("warn", `${basename(ws.path)} isn't a git repository: no worktree possible`);
    return;
  }
  const agent = opts.agent ?? (opts.worktree ? agentOf(c.id) ?? lastKind() : lastKind());
  const s = sessionOfGroup(opts.group);
  // The tab has left its project (`cd` in a shell, `/cd` in Claude): open where it is now.
  const moved = opts.worktree ? null : movedFolder(s, c, ws);
  createTab({
    workspace_id: moved ? undefined : c.workspace_id,
    path: moved ?? ws?.path,
    agent,
    account_profile: inheritedProfile(agent, s),
    worktree: !!opts.worktree,
    group: opts.group,
  })
    .catch((e) => toast("error", `Couldn't open a tab: ${errText(e)}`));
}

/** Tab `s`'s current folder if it is outside its task's folder and the task's project. */
export function movedFolder(s: SessionInfo | undefined, c: Channel, ws: Workspace | undefined): string | null {
  const live = s?.channel_id === c.id ? s.live_cwd : undefined;
  if (!live) return null;
  const inside = (root?: string) => !!root && (live === root || live.startsWith(root.replace(/\/$/, "") + "/"));
  return inside(c.cwd) || inside(ws?.path) ? null : live;
}

export function newTabDialog(worktree?: boolean) {
  overlay.value = { kind: "new_tab", worktree };
}

/** Default folder for new tabs: the focused tab's (where it has moved to, as for `+`), else
 * the last used, else home. */
export function defaultFolder(): string {
  const c = focusedTask.value;
  const ws = c ? workspaceById.value.get(c.workspace_id) : undefined;
  const moved = c ? movedFolder(focusedSession.value ?? undefined, c, ws) : null;
  return moved ?? ws?.path ?? prefs.value.recentFolders[0] ?? workspaces.value[workspaces.value.length - 1]?.path ??
    meta.value.home ?? "~";
}

export function renameChannel(id: ChannelId, name: string) {
  const n = name.trim();
  const c = channelById.value.get(id);
  if (!n || !c || (n === c.name && !c.auto_named)) return;
  rpc.call<Channel>("channel.rename", { id, name: n }).then(knowChannel)
    .catch(fail("Rename failed")).catch(() => {});
}

/** Human-only: the task's agent permission profile (new and restarted sessions). */
export async function setPermissions(id: ChannelId, permissions: Permissions): Promise<Channel> {
  const c = await rpc.call<Channel>("channel.set_permissions", { id, permissions });
  // channel_upserted may lag the reply.
  knowChannel({ ...channelById.value.get(c.id), ...c });
  toast("success", "Permissions saved. They apply to new and restarted sessions.");
  return c;
}

export function openPermissions(id: ChannelId) {
  overlay.value = { kind: "permissions", channelId: id };
}

export function copy(text: string, what = "Copied") {
  copyText(text).then((ok) =>
    ok ? toast("success", what, 1800) : toast("error", "Copy failed: the clipboard isn't available here")
  );
}

/** The terminal menu's Paste. Reading the clipboard needs a permission the browser may refuse;
 * Ctrl+Shift+V pastes through the browser's own paste event, which needs none. */
export function pasteClipboard(sid: SessionId) {
  const failed = () =>
    toast("error", "Muster can't read the clipboard here. Press Ctrl+Shift+V in the terminal to paste.");
  terms.focusTerminal(sid);
  if (!navigator.clipboard?.readText) return failed();
  navigator.clipboard.readText().then((t) => t && terms.terminal(sid)?.paste(t)).catch(failed);
}

// ---------- view ----------

export function setFontSize(px: number) {
  const v = Math.max(9, Math.min(24, px));
  setPref("fontSize", v);
  terms.setFontSize(v);
}

export function cycleTheme() {
  const order = ["system", "dark", "light"] as const;
  const cur = prefs.value.theme;
  setPref("theme", order[(order.indexOf(cur) + 1) % order.length]);
}

export function toggleStatusLine() {
  setPref("statusLine", !prefs.value.statusLine);
  terms.refitAll();
}

export const findOpen = signal<{ sid: number } | null>(null);
export function openFind() {
  const s = focusedSession.value;
  if (s) findOpen.value = { sid: s.id };
}

export function openJump(initial = "") {
  overlay.value = { kind: "jump", initial };
}

// ---------- window and Muster lifecycle ----------

/** Save a layout change that is still waiting, but never hold the page up for long. */
/** Save the layout and unsaved note typing before the page goes. Note typing is kept in this
 * browser first (notes.ts), so a save still on its way when the wait ends isn't lost: the next
 * page of this window saves it. */
function settleLayout(): Promise<unknown> {
  persistAllDrafts();
  return Promise.race([Promise.all([saveLayoutNow(), flushAllDrafts()]), new Promise((r) => setTimeout(r, 2000))]);
}

// A page going away (closed, reloaded by the browser) keeps and sends unsaved note typing first.
addEventListener("pagehide", () => void flushAllDrafts());

/** Go ahead with `go` (`what`: "Reload", "Close") unless note typing is neither saved nor kept
 * in this browser (storage full or blocked): then ask first. */
function leaveUnlessUnsaved(what: string, go: () => void) {
  const left = unsavedNotes();
  if (!left.length || persistAllDrafts()) return go();
  const names = left.map((id) => `“${notes.value.get(id)?.title ?? `Note ${id}`}”`).join(", ");
  overlay.value = {
    kind: "confirm",
    title: "Some note text isn't saved yet",
    body: [
      `${names} ${left.length === 1 ? "has" : "have"} typing Muster couldn't save (it isn't reachable) and this browser couldn't keep.`,
      `${what} now and that typing is lost. Cancel, wait until the note says Saved, then try again.`,
    ],
    confirmLabel: `${what} anyway`,
    danger: true,
    run: go,
  };
}

/** Ctrl+Shift+R: reload this window, which also loads the active UI release. */
export function reloadWindow() {
  settleLayout().finally(() => leaveUnlessUnsaved("Reload", () => location.reload()));
}

/** Close this window only; its tabs stay in it for the next `muster`. */
export async function closeWindow() {
  await settleLayout();
  leaveUnlessUnsaved("Close", async () => {
    if (!(await native.closeWindow())) toast("warn", "This browser won't let Muster close the window. Close it yourself.");
  });
}

/** Show or hide the native windows' system title bar (every window, and new ones). Shown at
 * once here; the backend's answer (and its `prefs_changed`) is the last word, unless another
 * window changed it since (a higher revision). */
export function setDecorations(on: boolean) {
  const prev = windowPrefs.value;
  const mine = prev ? { ...prev, decorations: on } : null;
  if (mine) windowPrefs.value = mine;
  rpc.call<WindowPrefs>("prefs.set", { decorations: on })
    .then((p) => {
      if (newerPrefs(windowPrefs.value, p)) windowPrefs.value = p;
    })
    .catch((e) => {
      // Undo only our own guess, never a change that arrived meanwhile.
      if (windowPrefs.value === mine) windowPrefs.value = prev;
      toast("error", `Couldn't change the title bar: ${errText(e)}`);
    });
}

/** The shell new shell tabs run: a path or a name on PATH, "" for the login shell. False if
 * the backend refused it (the toast says why). */
export async function setShell(shell: string): Promise<boolean> {
  try {
    const p = await rpc.call<WindowPrefs>("prefs.set", { shell: shell.trim() });
    if (newerPrefs(windowPrefs.value, p)) windowPrefs.value = p;
    const now = p.shell ?? meta.value.login_shell;
    meta.value = { ...meta.value, default_shell: now };
    toast("info", `New shell tabs will run ${now}`);
    return true;
  } catch (e) {
    toast("error", `Couldn't change the shell: ${errText(e)}`);
    return false;
  }
}

/** The cold prompt cache warning (`prefs.set {cache_warn, cache_ttl}`). False if refused. */
export async function setCachePrefs(change: { cache_warn?: boolean; cache_ttl?: Partial<CacheTtl> }): Promise<boolean> {
  try {
    const p = await rpc.call<WindowPrefs>("prefs.set", change);
    if (newerPrefs(windowPrefs.value, p)) windowPrefs.value = p;
    toast("success", "Cache warning settings saved.", 2500);
    return true;
  } catch (e) {
    toast("error", `Couldn't save the cache warning settings: ${errText(e)}`);
    return false;
  }
}

/** A backend restart is under way (from the update banner), until the new backend answers. */
export const restartingBackend = signal(false);
let restartTimer = 0;

/** Lifecycle requests are never queued for a later connection: by then they may mean
 * something else (another backend, another build). */
function connectedFor(what: string): boolean {
  if (rpc.isOpen) return true;
  toast("warn", `Not connected to Muster; ${what} once the window has reconnected.`);
  return false;
}

/** Restart the backend into build `expect` (the `updates.backend` id the banner offered;
 * `daemon.reload`): terminals keep running and this page reconnects. The backend refuses a
 * build other than the offered one, or the one it already runs; a restart the CLI refuses comes
 * back as a notice. */
export async function restartBackend(expect: string) {
  if (restartingBackend.value || !connectedFor("restart the backend")) return;
  restartingBackend.value = true;
  clearTimeout(restartTimer);
  // A restart that never happens (refused, see the notice) frees the button again.
  restartTimer = setTimeout(() => (restartingBackend.value = false), 30000);
  try {
    await rpc.call("daemon.reload", { expect });
  } catch (e) {
    // The old backend may go away before it answers: that is the restart working.
    if (/connection lost/i.test(errText(e))) return;
    restartingBackend.value = false;
    toast("error", `Couldn't restart the backend: ${errText(e)}`);
  }
}

/** A new backend answered (after a restart). */
export function backendRestarted() {
  clearTimeout(restartTimer);
  restartingBackend.value = false;
}

/** "Shut down Muster": stop the backend and the terminal host, ending every session. Only the
 * backend's answer or its `daemon_stopping` says it is happening; a dropped connection is not
 * taken as a shutdown (the window keeps reconnecting). */
export async function shutDown() {
  if (!connectedFor("shut down")) return;
  try {
    await rpc.call("daemon.shutdown");
    daemonStopped.value = true;
  } catch (e) {
    // `daemon_stopping` (which suspends this page's connection) usually arrives before the reply.
    if (daemonStopped.value) return;
    toast(
      "error",
      /connection lost/i.test(errText(e))
        ? "The connection dropped before Muster answered; it may not have shut down."
        : `Couldn't shut down Muster: ${errText(e)}`,
    );
  }
}

// ---------- named agents (docs/UI.md "Agents panel") ----------

/** Alt+G: show or hide this window's agents panel (kept in the window's layout). */
export function toggleAgentsPanel() {
  const open = !agentsPanelOpen.value;
  updateLayout((l) => {
    const { agents: _was, ...rest } = l;
    return open ? { ...rest, agents: true } : rest;
  });
  if (!open) focusActiveTerminalSoon();
}

/** An agent's session is running (not exited). */
export function agentLive(a: Agent): boolean {
  const s = a.session_id !== null ? sessions.value.get(a.session_id) : undefined;
  return !!s && s.state !== "exited";
}

/**
 * Click an agent (panel row, Jump): start it if no process runs (a new tab opens in this
 * window), then show its tab here exactly as Jump does (pulled into the focused group when it
 * is in another window, or hidden because its tab was closed). `fresh`: a new conversation,
 * restarting a running one.
 */
export async function openAgent(name: string, fresh = false) {
  const a = agents.value.find((x) => x.name === name);
  if (!a) return;
  pushRecent("a:" + a.id);
  if (!fresh && a.session_id !== null && agentLive(a)) {
    // A provider change applies only when it restarts: say so, as `agent.start` does.
    const pending = providerPendingNote(a, sessions.value.get(a.session_id));
    if (pending) toast("info", pending, 6000);
    return gotoSession(a.session_id);
  }
  try {
    const mark = sessionMark();
    const r = await rpc.call<{ agent: Agent; session: SessionInfo; started: boolean; provider_pending?: boolean }>(
      "agent.start",
      { name: a.name, fresh: fresh || undefined, window_id: windowId.value ?? undefined },
    );
    knowAgent(r.agent);
    knowSession(r.session, mark);
    if (r.provider_pending) {
      const pending = providerPendingNote(r.agent, r.session);
      if (pending) toast("info", pending, 6000);
    }
    await gotoSession(r.session.id);
  } catch (e) {
    toast("error", `Couldn't start ${a.name}: ${errText(e)}`);
  }
}

export function stopAgent(a: Agent) {
  if (a.session_id !== null) killSession(a.session_id);
}

export function setAgentAutostart(a: Agent, on: boolean) {
  rpc.call<Agent>("agent.update", { name: a.name, autostart: on }).then((r) => {
    knowAgent(r);
    toast(
      "info",
      on ? `${a.name} starts with the daemon` : `${a.name} no longer starts with the daemon`,
      2500,
    );
  }).catch(fail("Couldn't change autostart")).catch(() => {});
}

/** Remove an agent (asks first, in the app's own dialog): it stops and its tab closes, its
 * folder stays. */
export function removeAgent(a: Agent) {
  overlay.value = {
    kind: "confirm",
    title: `Remove ${a.name}?`,
    body:
      `Stops it, closes its tab and forgets the agent. Its folder (${a.home_dir}), with its persona and memory/, stays on disk.`,
    confirmLabel: "Remove agent",
    danger: true,
    run: () =>
      rpc.call("agent.remove", { name: a.name }).then(() => {
        agents.value = agents.value.filter((x) => x.id !== a.id);
      }).catch(fail("Remove failed")).catch(() => {}),
  };
}

export function newAgentDialog(template?: string) {
  overlay.value = { kind: "agent", template };
}

// ---------- schedules (docs/UI.md "Agents panel", docs/PROTOCOL.md "Schedules") ----------

/** New schedule; `agent` preselects that agent when it takes scheduled work. */
export function newScheduleDialog(agent?: string) {
  overlay.value = { kind: "schedule", agent };
}

export function editSchedule(id: number) {
  overlay.value = { kind: "schedule", id };
}

export function scheduleSettingsDialog() {
  overlay.value = { kind: "schedule_settings" };
}

/** A `schedule.*` call that answers with the schedule: recorded at once, errors toasted. */
function scheduleCall(method: string, params: Record<string, unknown>, what: string, done?: (s: Schedule) => void) {
  rpc.call<Schedule>(method, params).then((s) => {
    knowSchedule(s);
    done?.(s);
  }).catch(fail(what)).catch(() => {});
}

export function pauseSchedule(s: Schedule, on: boolean) {
  scheduleCall("schedule.pause", { id: s.id, on }, on ? "Couldn't pause it" : "Couldn't resume it");
}

export function skipNextRun(s: Schedule, on: boolean) {
  scheduleCall("schedule.skip", { id: s.id, on }, "Couldn't change Skip next", (r) => {
    if (on && r.next_run !== null) toast("info", `${r.name}: the run ${nextWhen(r.next_run, Date.now())} will be skipped`, 3000);
  });
}

export function renewSchedule(s: Schedule) {
  scheduleCall("schedule.renew", { id: s.id }, "Couldn't renew it", (r) => {
    toast("success", `Renewed ${r.name} for 7 days`, 3000);
  });
}

/** Run now: outside its timing and quiet hours; the agent's own caps still apply. Its history
 * opens so the run shows. */
export function runScheduleNow(s: Schedule) {
  rpc.call<ScheduleRun>("schedule.run", { id: s.id }).then((r) => {
    knowRun(r);
    if (!scheduleHistoryOpen.value.has(s.id)) toggleScheduleHistory(s.id);
    if (r.outcome === "skipped") toast("warn", `${s.name}: skipped${r.reason ? `, ${r.reason}` : ""}`, 5000);
    else toast("info", `${s.name}: sent to ${s.agent ?? "its agent"}`, 2500);
  }).catch(fail("Couldn't run it")).catch(() => {});
}

/** A URL trigger: copy a ready `curl` line (its loopback URL and secret) for a script. */
export function copyTriggerCommand(s: Schedule) {
  rpc.call<{ curl: string; url: string }>("trigger.secret", { id: s.id }).then(async (v) => {
    if (await copyText(v.curl)) {
      toast("success", `Copied the command that calls ${s.name} (it holds the secret: keep it private)`, 4000);
    } else toast("warn", `Couldn't copy it. In a terminal: muster trigger --url ${v.url.split("/").pop()}`, 6000);
  }).catch(fail("Couldn't read the trigger's secret")).catch(() => {});
}

/** A URL trigger: a new secret; the old one stops working. */
export function rotateTriggerSecret(s: Schedule) {
  overlay.value = {
    kind: "confirm",
    title: `New secret for ${s.name}?`,
    body: "Scripts that call it with the old secret stop working until you give them the new one.",
    confirmLabel: "New secret",
    run: () =>
      rpc.call<{ curl: string }>("trigger.secret", { id: s.id, rotate: true }).then(async (v) => {
        const copied = await copyText(v.curl);
        toast("success", copied ? "New secret made; the command with it is copied" : "New secret made; the old one no longer works", 3500);
      }).catch(fail("Couldn't make a new secret")).catch(() => {}),
  };
}

/** "Allow triggers" on a note: only then can note triggers fire on its changes. */
export function setNoteTriggers(id: number, on: boolean) {
  rpc.call<Note>("note.set_triggers", { id, on }).then((n) => {
    knowNote(n);
    toast("info", on ? `Triggers allowed on ${n.title}: its changes can now start runs` : `Triggers off for ${n.title}`, 3000);
  }).catch(fail("Couldn't change it")).catch(() => {});
}

/** Approve an agent's proposal: from now on it runs like any schedule. */
export function approveSchedule(s: Schedule) {
  scheduleCall("schedule.approve", { id: s.id }, "Couldn't approve it", (r) => {
    const next = r.next_run !== null ? `: next ${nextWhen(r.next_run, Date.now())}` : "";
    toast("success", `Approved ${r.name} for ${r.agent ?? "its agent"}${next}.`, 3500);
  });
}

/** Dismiss an agent's proposal: it is removed (it never ran). */
export function dismissProposal(s: Schedule) {
  rpc.call("schedule.remove", { id: s.id }).then(() => {
    schedules.value = schedules.value.filter((x) => x.id !== s.id);
    toast("info", `Dismissed ${s.made_by?.agent ?? "the agent"}'s proposal ${s.name}.`, 3000);
  }).catch(fail("Couldn't dismiss it")).catch(() => {});
}

export function removeSchedule(s: Schedule) {
  overlay.value = {
    kind: "confirm",
    title: `Remove ${s.name}?`,
    body: `It won't run again, and its run history goes with it. Messages it already sent stay in the log${
      s.note_title ? `, and its replies stay in the note ${s.note_title}` : ""
    }.`,
    confirmLabel: "Remove schedule",
    danger: true,
    run: () =>
      rpc.call("schedule.remove", { id: s.id }).then(() => {
        schedules.value = schedules.value.filter((x) => x.id !== s.id);
      }).catch(fail("Remove failed")).catch(() => {}),
  };
}

/** Show or hide a schedule's run history under its row; its runs load the first time. */
export function toggleScheduleHistory(id: number) {
  const open = new Set(scheduleHistoryOpen.value);
  if (open.has(id)) open.delete(id);
  else {
    open.add(id);
    // Loaded once, a history is kept current by `schedule_run` events; a failed one tries again.
    const h = scheduleRuns.value.get(id);
    if (!h || (h.error && !h.loading)) loadScheduleRuns(id);
  }
  scheduleHistoryOpen.value = open;
}

/** Open a run's message and reply in the message overlay (the log may not have it loaded). */
export function openRunMessage(r: ScheduleRun) {
  const m = r.message;
  if (!m) return;
  agentMessages.value = upsertMessage(agentMessages.value, m, Math.max(300, agentMessages.value.length + 1));
  overlay.value = { kind: "agent_message", id: m.id };
}

/** "Pause all schedules": every schedule's slots are skipped (recorded once) until resumed. */
export function setAllSchedulesPaused(paused: boolean) {
  rpc.call<ScheduleSettings>("schedule.settings.set", { paused }).then((s) => {
    scheduleSettings.value = s;
    toast("info", paused ? "All schedules paused" : "Schedules resumed", 2500);
  }).catch(fail(paused ? "Couldn't pause schedules" : "Couldn't resume schedules")).catch(() => {});
}

export function editAgent(name: string, tab: "settings" | "memory" = "settings") {
  overlay.value = { kind: "agent", name, tab };
}


/** Sessions whose prompt delivery was interrupted and not yet resumed or cleared. */
let needsReview = new Set<SessionId>();

function showReviewNotice() {
  if (!needsReview.size) return dismissToastKey("delivery-review");
  const n = needsReview.size;
  toast(
    "warn",
    n === 1
      ? "Prompt delivery needs review in 1 terminal. Right-click its tab and choose “Review queued prompts”."
      : `Prompt delivery needs review in ${n} terminals. Right-click each one’s tab and choose “Review queued prompts”.`,
    12000,
    "delivery-review",
  );
}

/** From hello: the sessions whose delivery is paused for review. */
export function deliveriesNeedReview(ids: SessionId[]) {
  needsReview = new Set(ids);
  showReviewNotice();
}

/** A paused delivery was resumed or its queue cleared. */
function reviewResolved(id: SessionId) {
  if (needsReview.delete(id)) showReviewNotice();
}

/** Review an interrupted delivery before deciding whether to clear its queue. */
export async function reviewQueue(id: SessionId) {
  try {
    const queue = await rpc.call<{ queued: string[]; uncertain: string | null; paused: boolean }>("session.queue", { id });
    const preview = (text: string, max = 300) => text.length > max ? `${text.slice(0, max)}…` : text;
    const waiting = queue.queued.length;
    const body = [
      queue.uncertain
        ? `This prompt may have been typed only partly before Muster stopped: “${preview(queue.uncertain, 160)}”. Check the terminal before sending it again.`
        : "",
      waiting
        ? `${waiting} ${waiting === 1 ? "prompt" : "prompts"} waiting: ${
          queue.queued.slice(0, 3).map((text) => `“${preview(text, 120)}”`).join(" · ")
        }${waiting > 3 ? ` and ${waiting - 3} more` : ""}.`
        : "No prompts are waiting.",
      `${queue.paused ? "Resume sends the waiting prompts and forgets only the interrupted one. " : ""}Clear queue discards every queue record. Text already typed and commands already run stay either way.`,
    ].filter(Boolean);
    overlay.value = {
      kind: "confirm",
      title: queue.paused ? "Review interrupted delivery" : "Queued prompts",
      body,
      confirmLabel: "Clear queue",
      danger: true,
      run: () =>
        rpc.call("session.clear_queue", { id }).then(() => reviewResolved(id))
          .catch(fail("Could not clear queue")).catch(() => {}),
      secondary: queue.paused
        ? {
          label: "Resume queue",
          run: () =>
            rpc.call("session.resume_queue", { id }).then(() => reviewResolved(id))
              .catch(fail("Could not resume queue")).catch(() => {}),
        }
        : undefined,
    };
  } catch (error) {
    toast("error", `Could not read queue: ${errText(error)}`);
  }
}
