// User-facing actions. Components and the command table call these; they talk to the
// daemon and update local state (layouts, prefs, tabs, windows).

import { signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import {
  activeChannel,
  activeChannelId,
  activeTabReplacedListeners,
  BUSY_STATES,
  channelBusy,
  channelById,
  channels,
  confirmClose,
  holdNewSessions,
  knowChannel,
  knowSession,
  knowWindow,
  layoutOf,
  layouts,
  meta,
  myWindow,
  needsYou,
  overlay,
  prefs,
  pushRecent,
  pushRecentFolder,
  releaseNewSessions,
  sessions,
  sessionsByChannel,
  setPref,
  tabIdSet,
  tabs,
  toast,
  updateLayout,
  upsertSession,
  windowId,
  windowOf,
  windows,
  windowsSupported,
  workspaceById,
  workspaces,
} from "./state.ts";
import type {
  Channel,
  ChannelId,
  Decision,
  Permissions,
  SessionId,
  SessionInfo,
  SessionKind,
  Win,
  WindowId,
  Workspace,
} from "./types.ts";
import {
  activateTab,
  addTab,
  allSessions,
  chromeForNew,
  cycleTab,
  type Direction,
  type DropZone,
  equalize as equalizeTree,
  findPane,
  focusDirection,
  focusPane as focusPaneTree,
  type LayoutState,
  panePx,
  pathOfPane,
  placeSessions,
  resizeFocused,
  type Side,
  type Size,
  splitFits,
  splitPane,
  toggleZoom,
} from "./layout/tree.ts";
import * as terms from "./terms.ts";
import { copyText } from "./util.ts";
import { basename, moveBy, moveChannelLocally, moveTo, nextInCycle, windowLabel } from "./model.ts";

export function fail(prefix: string) {
  return (e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e);
    toast("error", `${prefix}: ${msg}`);
    throw e;
  };
}

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

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
 * window support every channel is a tab here. A failed claim degrades to that too.
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

/** Open a client window for window `id` (a new browser tab or window). */
export function openClientWindow(id: WindowId) {
  const url = `${location.origin}${location.pathname}#window=${id}`;
  let w: Window | null = null;
  try {
    w = open(url, "_blank");
  } catch { /* blocked */ }
  if (!w) {
    toast("info", `Run "muster open --window ${id}" to show ${windowLabel({ id, name: String(id) })}.`, 8000);
  }
}

/** Move channel `id` into this window (at `index`, default the end). */
async function pullHere(id: ChannelId, index?: number): Promise<boolean> {
  const here = windowId.value;
  if (here === null) return tabIdSet.value.has(id);
  try {
    const c = await rpc.call<Channel>("channel.move", { id, window_id: here, index });
    knowChannel({ ...c, window_id: c.window_id ?? here });
    windows.value = moveChannelLocally(windows.value, id, here, index);
    return true;
  } catch (e) {
    toast("error", `Couldn't bring the tab here: ${errText(e)}`);
    return false;
  }
}

/** Move a tab of this window to another window, or to a new one (which then opens). */
export async function moveTabToWindow(id: ChannelId, target: WindowId | "new") {
  try {
    let to: WindowId;
    if (target === "new") {
      const w = await rpc.call<Win>("window.create", {});
      knowWindow(w);
      to = w.id;
    } else to = target;
    const c = await rpc.call<Channel>("channel.move", { id, window_id: to });
    knowChannel({ ...c, window_id: c.window_id ?? to });
    windows.value = moveChannelLocally(windows.value, id, to);
    const w = windows.value.find((x) => x.id === to);
    if (target === "new" || (w && !w.open)) openClientWindow(to);
    else toast("info", `Moved to ${windowLabel(w ?? { id: to, name: String(to) })}`, 2500);
  } catch (e) {
    toast("error", `Move failed: ${errText(e)}`);
  }
}

// ---------- selection / navigation ----------

export function selectTab(id: ChannelId, focusTerm = true) {
  if (!tabIdSet.value.has(id)) return;
  activeChannelId.value = id;
  pushRecent("c:" + id);
  layoutOf(id);
  if (focusTerm) focusActiveTerminalSoon();
}

// The active tab closed or moved to another window while its terminal had focus: focus falls to
// <body>, so hand it to the tab that took its place (never steal it from a dialog or field).
activeTabReplacedListeners.push(() => {
  const a = document.activeElement;
  if (!a || a === document.body) focusActiveTerminalSoon();
});

export function stepTab(delta: number) {
  const order = tabs.value;
  if (!order.length) return;
  const i = order.findIndex((c) => c.id === activeChannelId.value);
  selectTab(order[(i + delta + order.length) % order.length].id);
}

export function gotoTabIndex(n: number) {
  const c = tabs.value[n];
  if (c) selectTab(c.id);
}

/** Set this window's tab order (drag, Alt+Shift+[ / ]). */
export function reorderTabs(ids: ChannelId[]) {
  const w = myWindow.value;
  if (w) {
    knowWindow({ ...w, channel_ids: ids });
    rpc.call<Win>("window.reorder_tabs", { id: w.id, channel_ids: ids })
      .then(knowWindow)
      .catch((e) => {
        // Roll back the optimistic order unless something newer replaced it meanwhile.
        const cur = windows.value.find((x) => x.id === w.id);
        if (cur && cur.channel_ids.join() === ids.join()) knowWindow({ ...cur, channel_ids: w.channel_ids });
        toast("error", `Reorder failed: ${errText(e)}`);
      });
    return;
  }
  // No windows: the daemon's global channel order is the tab order.
  const before = new Map(channels.value.map((c) => [c.id, c.sort_order]));
  const pos = new Map(ids.map((id, i) => [id, i]));
  channels.value = channels.value.map((c) => (pos.has(c.id) ? { ...c, sort_order: pos.get(c.id)! } : c));
  rpc.call("channel.reorder", { ids }).catch((e) => {
    channels.value = channels.value.map((
      c,
    ) => (before.has(c.id) ? { ...c, sort_order: before.get(c.id)! } : c));
    toast("error", `Reorder failed: ${errText(e)}`);
  });
}

export function dropTabAt(id: ChannelId, slot: number) {
  const ids = tabs.value.map((c) => c.id);
  const next = moveTo(ids, id, slot);
  if (next.join() !== ids.join()) reorderTabs(next);
}

export function moveActiveTab(delta: number) {
  const id = activeChannelId.value;
  if (id === null) return;
  const ids = tabs.value.map((c) => c.id);
  const next = moveBy(ids, id, delta);
  if (next !== ids) reorderTabs(next);
}

/**
 * Show channel `id` (and session `sid` in it), wherever it is. A tab of another window is
 * pulled here, or with "Jump focuses the other window" that window is raised (pulling when
 * that fails).
 */
export async function gotoChannel(id: ChannelId, sid?: SessionId) {
  const c = channelById.value.get(id);
  if (!c) return;
  pushRecent(sid !== undefined ? "s:" + sid : "c:" + id);
  if (!tabIdSet.value.has(id)) {
    const other = windowOf(c);
    if (prefs.value.jumpRaises && other !== null && other !== windowId.value) {
      const raised = await rpc.call<{ raised: boolean }>("window.raise", { id: other })
        .then((r) => !!r?.raised).catch(() => false);
      if (raised) return;
    }
    if (!(await pullHere(id))) return;
  }
  if (sid !== undefined) revealSession(sid);
  else selectTab(id);
}

/** Alt+J: the next session that needs you, in any window. */
let lastNeedsYou: SessionId | null = null;

export function nextNeedsYou() {
  const list = needsYou.value;
  // Cycle from the focused session when it needs you; else from the last one we went to (it may
  // be in another window that was raised, so focus here didn't move).
  const focused = focusedSession()?.id ?? null;
  const from = focused !== null && list.some((s) => s.id === focused) ? focused : lastNeedsYou;
  const next = nextInCycle(list, from);
  if (!next) {
    toast("info", "Nothing needs you", 1500);
    return;
  }
  lastNeedsYou = next.id;
  gotoChannel(next.channel_id, next.id);
}

/** Active session of the focused pane in the active tab. */
export function focusedSession(): SessionInfo | null {
  const c = activeChannel.value;
  if (!c) return null;
  const l = layouts.value.get(c.id);
  if (!l) return null;
  const p = findPane(l.root, l.focus);
  if (!p || p.active === null) return null;
  return sessions.value.get(p.active) ?? null;
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
      const s = focusedSession();
      if (s) terms.focusTerminal(s.id);
    })
  );
}

/** Focus a session of a tab in this window: switch to it, activate its pane-tab. */
export function revealSession(sid: SessionId) {
  const s = sessions.value.get(sid);
  if (!s || !tabIdSet.value.has(s.channel_id)) return;
  selectTab(s.channel_id, false);
  updateLayout(s.channel_id, (l) => activateTab(l, sid));
  focusActiveTerminalSoon();
}

export function focusPane(ch: ChannelId, paneId: string) {
  const l = layouts.value.get(ch);
  if (l && l.focus === paneId) return;
  updateLayout(ch, (l) => focusPaneTree(l, paneId));
}

export function activateSessionTab(ch: ChannelId, sid: SessionId, focus = true) {
  updateLayout(ch, (l) => activateTab(l, sid));
  if (focus) focusActiveTerminalSoon();
}

function withLayout(fn: Parameters<typeof updateLayout>[1], focus = true) {
  const c = activeChannel.value;
  if (!c) return;
  updateLayout(c.id, fn);
  if (focus) focusActiveTerminalSoon();
}

export const focusDir = (d: Direction) => withLayout((l) => focusDirection(l, d));
export const resizeDir = (d: Direction) => withLayout((l) => resizeFocused(l, d, 0.04), false);
export const zoomToggle = () => withLayout((l) => toggleZoom(l));
export const paneTabStep = (delta: number) => withLayout((l) => cycleTab(l, delta));
export const equalizeAll = () =>
  withLayout((l) => {
    const p = pathOfPane(l.root, l.focus);
    return p && p.length ? equalizeTree(l, p.slice(0, -1)) : equalizeTree(l, []);
  });

// ---------- sessions ----------

export type Placement =
  | { type: "tab"; paneId?: string }
  | { type: "split"; side: Side; paneId?: string };

/** Size of the split area (the box the layout tree is laid out in). */
function splitBox(): Size | null {
  const el = document.querySelector(".panes");
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 ? { w: r.width, h: r.height } : null;
}

/** The layout of a tab on screen (only the active tab can be measured). */
function visibleLayout(ch: ChannelId): LayoutState | null {
  return ch === activeChannel.value?.id ? layoutOf(ch) : null;
}

/** Whether pane `paneId` (default focused) has room to split on `side`. Toasts when not. */
export function canSplit(ch: ChannelId, side: Side, paneId?: string, quiet = false): boolean {
  const l = visibleLayout(ch);
  const box = splitBox();
  if (!l || !box) return true; // not on screen: can't measure
  const id = paneId && findPane(l.root, paneId) ? paneId : l.focus;
  if (splitFits(l.root, id, side, box)) return true;
  if (!quiet) toast("warn", "Not enough room — zoom or close a pane");
  return false;
}

/** Chrome (px) above the terminal of a session landing in `placement`. */
function chromeFor(l: LayoutState, placement: Placement, paneId: string): number {
  const c = chromeForNew(l, placement.type, paneId);
  return c === "strip" ? terms.PANE_STRIP_PX : c === "head" ? terms.PANE_HEAD_PX : 0;
}

/** Terminal grid (cols/rows) a session will get in `placement`, so it starts at the right size. */
function gridFor(ch: ChannelId, placement: Placement): { cols: number; rows: number } | undefined {
  const l = visibleLayout(ch);
  const box = splitBox();
  if (!l || !box) return undefined;
  const id = placement.paneId && findPane(l.root, placement.paneId) ? placement.paneId : l.focus;
  // A zoomed pane fills the whole box; a split unzooms first.
  const px = l.zoom === id && placement.type === "tab" ? box : panePx(l.root, id, box);
  if (!px) return undefined;
  let { w, h } = px;
  if (placement.type === "split") {
    if (placement.side === "left" || placement.side === "right") w = w / 2 - 1;
    else h = h / 2 - 1;
  }
  return terms.estimateGrid(w, h - chromeFor(l, placement, id));
}

export async function spawn(
  ch: ChannelId,
  kind: SessionKind,
  placement: Placement = { type: "tab" },
  prompt?: string,
): Promise<SessionInfo | undefined> {
  if (placement.type === "split" && !canSplit(ch, placement.side, placement.paneId)) return undefined;
  const grid = gridFor(ch, placement);
  holdNewSessions(ch);
  let placed: SessionId | undefined;
  try {
    const s = await rpc.call<SessionInfo>("session.spawn", {
      channel_id: ch,
      kind,
      prompt: prompt || undefined,
      ...grid,
    });
    placed = s.id;
    knowSession(s);
    if (kind !== "shell") setPref("lastAgent", kind);
    updateLayout(
      ch,
      (l) =>
        placement.type === "split"
          ? splitPane(l, placement.side, s.id, placement.paneId)
          : addTab(l, s.id, placement.paneId),
    );
    if (ch === activeChannel.value?.id) focusActiveTerminalSoon();
    return s;
  } catch (e) {
    toast("error", `Couldn't start ${kind}: ${errText(e)}`);
  } finally {
    releaseNewSessions(ch, placed);
  }
}

export function spawnHere(kind: SessionKind) {
  const c = activeChannel.value;
  if (!c) return;
  spawn(c.id, kind, { type: "tab", paneId: layoutOf(c.id).focus });
}

/** Alt+N: pick a kind for a new pane-tab in the focused pane (1/2/3, Enter repeats last). */
export async function spawnPicker() {
  const c = activeChannel.value;
  if (!c) return;
  const l = layoutOf(c.id);
  const anchor = document.querySelector(`[data-pane="${l.focus}"]`);
  const last = prefs.value.lastAgent === "none" ? "shell" : prefs.value.lastAgent;
  if (!anchor) {
    spawnHere(last);
    return;
  }
  const { openMenu } = await import("./components/Menu.tsx");
  const { spawnMenuItems } = await import("./layout/Pane.tsx");
  const r = anchor.getBoundingClientRect();
  openMenu({
    x: r.left + r.width / 2 - 100,
    y: r.top + Math.min(r.height / 3, 160),
    items: [
      { header: "New session in this pane" },
      ...spawnMenuItems((k) => spawn(c.id, k, { type: "tab", paneId: l.focus })),
    ],
    minWidth: 200,
    select: 1 + ["claude", "codex", "shell"].indexOf(last),
    onClose: () => focusActiveTerminalSoon(),
  });
}

export function splitFocused(side: Side, kind: SessionKind = "shell") {
  const c = activeChannel.value;
  if (c) spawn(c.id, kind, { type: "split", side });
}

/**
 * Close one session (a pane, or a pane-tab). The last session of a tab closes the tab. Busy
 * sessions get an inline "Close? ⏎ / Esc" first.
 */
export function closeSession(sid: SessionId, force = false) {
  const s = sessions.value.get(sid);
  if (!s) return;
  if ((sessionsByChannel.value.get(s.channel_id) ?? []).length <= 1) {
    closeTab(s.channel_id, force);
    return;
  }
  if (!force && BUSY_STATES.has(s.state)) {
    confirmClose.value = { kind: "session", id: sid };
    return;
  }
  confirmClose.value = null;
  rpc.call("session.remove", { id: sid }).catch(fail("Close failed")).catch(() => {});
}

/** Alt+W: close the focused pane's session (the tab, when it's the last one). */
export function closeFocused() {
  const s = focusedSession();
  if (s) closeSession(s.id);
  else if (activeChannel.value) closeTab(activeChannel.value.id);
}

/** Close a tab: archives the channel (its worktree stays). Asks inline while an agent works. */
export function closeTab(id: ChannelId, force = false) {
  if (!channelById.value.has(id)) return;
  if (!force && channelBusy(id)) {
    confirmClose.value = { kind: "channel", id };
    return;
  }
  confirmClose.value = null;
  rpc.call("channel.archive", { id, remove_worktree: false })
    .catch(fail("Close failed")).catch(() => {});
}

/**
 * A tab dragged onto the active tab's panes: every session of `src` (live and exited) moves into
 * tab `dst` (`session.move`) as one pane at the drop position, in `src`'s pane order with its
 * focused session active, then the emptied `src` closes. A failed move stops there: what moved
 * stays placed, the rest stays in `src`, which stays open.
 */
// Merges into one tab run one at a time: each holds new sessions out of the layout and places
// its own afterwards, and overlapping holds would place one drop's sessions with the other's.
const mergeQueue = new Map<ChannelId, Promise<void>>();

export function mergeTab(src: ChannelId, dst: ChannelId, paneId: string, zone: DropZone, index?: number) {
  const run = (mergeQueue.get(dst) ?? Promise.resolve()).then(() => mergeTabNow(src, dst, paneId, zone, index));
  const done = run.catch(() => {}).finally(() => {
    if (mergeQueue.get(dst) === done) mergeQueue.delete(dst);
  });
  mergeQueue.set(dst, done);
  return run;
}

/**
 * The tab the active tab `id` goes beside when it is dragged onto its own panes: the tab of this
 * window viewed most recently before it, else its neighbour.
 */
export function splitPartner(id: ChannelId): ChannelId | null {
  const here = tabs.value.map((c) => c.id).filter((c) => c !== id);
  for (const k of prefs.value.recent) {
    const c = k.startsWith("c:") ? Number(k.slice(2)) : NaN;
    if (here.includes(c)) return c;
  }
  const order = tabs.value.map((c) => c.id);
  const i = order.indexOf(id);
  return order[i + 1] ?? order[i - 1] ?? null;
}

async function mergeTabNow(src: ChannelId, dst: ChannelId, paneId: string, zone: DropZone, index?: number) {
  const wasActive = activeChannelId.value === src;
  const list = (sessionsByChannel.value.get(src) ?? []).map((s) => s.id);
  if (src === dst || !list.length) return;
  const l = layoutOf(src);
  const inLayout = allSessions(l.root).filter((id) => list.includes(id));
  const order = [...inLayout, ...list.filter((id) => !inLayout.includes(id))];
  const active = findPane(l.root, l.focus)?.active ?? undefined;
  // Held so reconcile doesn't first drop them into the focused pane (and size them for it).
  holdNewSessions(dst);
  const moved: SessionId[] = [];
  try {
    for (const id of order) {
      try {
        // The daemon closes the source tab once it is empty, in the same step: a session another
        // window adds to it meanwhile is never closed with it, and a lost reply can't make us
        // close a tab that still holds (or received) a session.
        upsertSession(
          await rpc.call<SessionInfo>("session.move", {
            id,
            channel_id: dst,
            from_channel_id: src,
            close_empty_source: true,
          }),
        );
        moved.push(id);
      } catch (e) {
        toast("error", `Couldn't move ${sessions.value.get(id)?.name ?? "a session"}: ${errText(e)}`);
        break;
      }
    }
    if (moved.length) updateLayout(dst, (l) => placeSessions(l, moved, paneId, zone, active, index));
  } finally {
    releaseNewSessions(dst);
  }
  // The active tab moved beside another one: show the result.
  if (wasActive && moved.length) selectTab(dst, false);
  if (dst === activeChannel.value?.id) focusActiveTerminalSoon();
}

/**
 * A pane header or pane-tab dropped on the empty part of the tab bar: the session moves into a
 * new tab of the same folder (workspace) and window, which becomes active.
 */
export async function moveSessionToNewTab(src: ChannelId, sid: SessionId) {
  const c = channelById.value.get(src);
  const ws = c ? workspaceById.value.get(c.workspace_id) : undefined;
  if (!c || !sessions.value.has(sid)) return;
  const here = windowId.value;
  let nc: Channel;
  try {
    nc = await rpc.call<Channel>("channel.create", {
      workspace_id: c.workspace_id,
      // Daemons without auto naming need a name: the folder's.
      name: windowsSupported.value ? undefined : basename(ws?.path ?? c.cwd),
      window_id: here ?? undefined,
    });
  } catch (e) {
    toast("error", `Couldn't open a tab: ${errText(e)}`);
    return;
  }
  knowChannel(here !== null ? { ...nc, window_id: nc.window_id ?? here } : nc);
  if (here !== null) windows.value = moveChannelLocally(windows.value, nc.id, here);
  try {
    upsertSession(
      await rpc.call<SessionInfo>("session.move", { id: sid, channel_id: nc.id, from_channel_id: src }),
    );
  } catch (e) {
    toast("error", `Couldn't move the session: ${errText(e)}`);
    // Only if nothing arrived: the move may have happened even though its reply was lost.
    rpc.call("channel.close_if_empty", { id: nc.id }).catch(() => {});
    return;
  }
  selectTab(nc.id);
}

/** Close a tab and delete its worktree (asks first: uncommitted work there is lost). */
export function closeTabRemoveWorktree(c: Channel) {
  overlay.value = {
    kind: "confirm",
    title: "Close tab and remove its worktree?",
    body:
      `Stops its sessions and deletes the worktree at ${c.cwd}. Uncommitted changes there are lost. The branch${
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
    const s = sessions.value.get(sid);
    if (focus && s && s.channel_id === activeChannel.value?.id) focusActiveTerminalSoon();
  }).catch(fail("Restart failed")).catch(() => {});
}

export function killSession(sid: SessionId) {
  rpc.call("session.kill", { id: sid }).catch(fail("Kill failed")).catch(() => {});
}

export function interruptSession(sid: SessionId) {
  rpc.call("session.interrupt", { id: sid }).catch(fail("Interrupt failed")).catch(() => {});
}

export function renameSession(sid: SessionId, name: string) {
  const n = name.trim();
  if (!n) return;
  rpc.call("session.rename", { id: sid, name: n }).catch(fail("Rename failed")).catch(() => {});
}

export function approve(sid: SessionId, decision: Decision) {
  rpc.call("session.approve", { id: sid, decision })
    .catch(fail(decision === "deny" ? "Deny failed" : "Approve failed")).catch(() => {});
}

export function approveFocused(decision: Decision) {
  const s = focusedSession();
  if (s && (s.state === "needs_approval" || s.state === "asking_question")) approve(s.id, decision);
}

// ---------- tabs (channels) ----------

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
  /** Folder: an existing workspace, or a path (added as a workspace when new). */
  workspace_id?: number;
  path?: string;
  agent: SessionKind | null;
  worktree: boolean;
  base?: string;
  /** Blank: the daemon names the tab. */
  name?: string;
  prompt?: string;
  /** Omitted: the daemon's defaults. */
  permissions?: Permissions;
}

/** Split area size when no tab is open (the empty window). */
function estimatedBox(): Size | null {
  const el = document.querySelector(".content");
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 ? { w: r.width, h: r.height } : null;
}

export async function createTab(req: NewTabReq): Promise<Channel> {
  const ws = req.workspace_id !== undefined
    ? workspaceById.value.get(req.workspace_id) ?? await ensureWorkspace(req.path ?? "")
    : await ensureWorkspace(req.path ?? "");
  // A new tab shows one pane with no chrome, filling the split area.
  const box = splitBox() ?? estimatedBox();
  const grid = box ? terms.estimateGrid(box.w, box.h) : undefined;
  const here = windowId.value;
  const c = await rpc.call<Channel>("channel.create", {
    workspace_id: ws.id,
    // Daemons without auto naming need a name: the folder's.
    name: req.name?.trim() || (windowsSupported.value ? undefined : basename(ws.path)),
    worktree: req.worktree,
    base: req.base?.trim() || undefined,
    agent: req.agent ?? undefined,
    prompt: req.prompt?.trim() || undefined,
    permissions: req.permissions,
    window_id: here ?? undefined,
    ...grid,
  });
  setPref("lastAgent", req.agent ?? "none");
  pushRecentFolder(ws.path);
  knowChannel(here !== null ? { ...c, window_id: c.window_id ?? here } : c);
  // Optimistic: the window_upserted that lists the new tab may lag the reply.
  if (here !== null) windows.value = moveChannelLocally(windows.value, c.id, here);
  selectTab(c.id);
  return c;
}

/** The agent a tab runs (its first agent session), for "same agent" shortcuts. */
function agentOf(ch: ChannelId): SessionKind | null {
  const list = sessionsByChannel.value.get(ch) ?? [];
  return list.find((s) => s.kind !== "shell")?.kind ?? list[0]?.kind ?? null;
}

/**
 * `+` / Alt+T: a new tab in the active tab's folder with the last agent kind, no worktree.
 * Ctrl+Shift+N (`worktree`): a worktree tab with the active tab's agent. No active tab: the
 * New tab dialog.
 */
export function newTabHere(opts: { worktree?: boolean; agent?: SessionKind | null } = {}) {
  const c = activeChannel.value;
  if (!c) {
    newTabDialog(opts.worktree);
    return;
  }
  const ws = workspaceById.value.get(c.workspace_id);
  if (opts.worktree && ws && !ws.is_git) {
    toast("warn", `${basename(ws.path)} isn't a git repository: no worktree possible`);
    return;
  }
  const last = prefs.value.lastAgent === "none" ? null : prefs.value.lastAgent;
  const agent = opts.agent !== undefined ? opts.agent : opts.worktree ? agentOf(c.id) ?? last : last;
  createTab({ workspace_id: c.workspace_id, path: ws?.path, agent, worktree: !!opts.worktree })
    .catch((e) => toast("error", `Couldn't open a tab: ${errText(e)}`));
}

export function newTabDialog(worktree?: boolean) {
  overlay.value = { kind: "new_tab", worktree };
}

/** Default folder for new tabs: the active tab's, else the last used, else home. */
export function defaultFolder(): string {
  const c = activeChannel.value;
  const ws = c ? workspaceById.value.get(c.workspace_id) : undefined;
  return ws?.path ?? prefs.value.recentFolders[0] ?? workspaces.value[workspaces.value.length - 1]?.path ??
    meta.value.home ?? "~";
}

export function renameChannel(id: ChannelId, name: string) {
  const n = name.trim();
  const c = channelById.value.get(id);
  if (!n || !c || (n === c.name && !c.auto_named)) return;
  rpc.call<Channel>("channel.rename", { id, name: n }).then(knowChannel)
    .catch(fail("Rename failed")).catch(() => {});
}

/** Human-only: the tab's agent permission profile (new and restarted sessions). */
export async function setPermissions(id: ChannelId, permissions: Permissions): Promise<Channel> {
  const c = await rpc.call<Channel>("channel.set_permissions", { id, permissions });
  // channel_upserted may lag the reply.
  channels.value = channels.value.map((x) => (x.id === c.id ? { ...x, ...c } : x));
  toast("success", "Permissions saved. They apply to new and restarted sessions.");
  return c;
}

export function openPermissions(id: ChannelId) {
  overlay.value = { kind: "permissions", channelId: id };
}

export function copy(text: string, what = "Copied") {
  copyText(text).then((ok) => toast(ok ? "success" : "error", ok ? what : "Copy failed", 1800));
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
  const s = focusedSession();
  if (s) findOpen.value = { sid: s.id };
}

export function openJump(initial = "") {
  overlay.value = { kind: "jump", initial };
}
