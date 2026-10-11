// User-facing actions. Components and the command table call these; they talk to the daemon and
// update local state (the window's layout of groups, prefs, windows).

import { signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import {
  agents,
  agentsPanelOpen,
  BUSY_STATES,
  channelById,
  confirmClose,
  firstSessionOf,
  focusedGroup,
  focusedSession,
  focusedTask,
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
  toast,
  updateLayout,
  windowId,
  windowOfSession,
  windows,
  windowsSupported,
  workspaceById,
  workspaces,
} from "./state.ts";
import type {
  Agent,
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
import { basename, mainSession, moveChannelLocally, nextInCycle, windowLabel } from "./model.ts";

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
  if (!s) return;
  try {
    let to: WindowId;
    if (target === "new") {
      const w = await rpc.call<Win>("window.create", {});
      knowWindow(w);
      to = w.id;
    } else to = target;
    await setSessionWindow(s, to, windowId.value);
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
  const s = focusedSession.value;
  if (s) updateLayout((l) => shiftTab(l, s.id, delta));
}

/** Ctrl+Alt+arrow: move the focused tab to the neighbouring group (a new split at the edge). */
export function moveFocusedTab(dir: Direction) {
  const s = focusedSession.value;
  const l = layout.value;
  if (!s || !l) return;
  const from = paneOfSession(l.root, s.id);
  if (!from) return;
  if (!neighbour(l.root, from.id, dir)) {
    if (from.tabs.length < 2 || !canSplit(sideOf(dir), from.id)) return;
  }
  withLayout((x) => moveTabToNeighbour(x, s.id, dir));
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
 * daemons are asked to). A busy session gets an inline "Close? ⏎ / Esc" first.
 */
export function closeTab(sid: SessionId, force = false) {
  const s = sessions.value.get(sid);
  if (!s) return;
  if (!force && BUSY_STATES.has(s.state)) {
    confirmClose.value = sid;
    return;
  }
  confirmClose.value = null;
  const last = (sessionsByChannel.value.get(s.channel_id) ?? []).every((x) => x.id === sid);
  rpc.call("session.remove", { id: sid })
    .then(() => {
      if (last) rpc.call("channel.close_if_empty", { id: s.channel_id }).catch(() => {});
    })
    .catch(fail("Close failed")).catch(() => {});
}

/** Alt+W: close the focused tab. */
export function closeFocused() {
  const s = focusedSession.value;
  if (s) closeTab(s.id);
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

export function interruptSession(sid: SessionId) {
  rpc.call("session.interrupt", { id: sid }).catch(fail("Interrupt failed")).catch(() => {});
}

export function renameSession(sid: SessionId, name: string) {
  const n = name.trim();
  const s = sessions.value.get(sid);
  if (!n || !s || n === s.name) return;
  const mark = sessionMark();
  rpc.call<SessionInfo>("session.rename", { id: sid, name: n }).then((r) => knowSession(r, mark))
    .catch(fail("Rename failed")).catch(() => {});
}

/** Double-click / F2 on a tab: the task's main tab renames the task, others the session. */
export function startRename(sid: SessionId) {
  const s = sessions.value.get(sid);
  if (!s) return;
  const main = mainSession(sessionsByChannel.value.get(s.channel_id) ?? []);
  renameIn(main?.id === sid ? "channel" : "session", sid);
}

/** Rename tab `sid`'s task or session inline, in that tab. */
export function renameIn(kind: "channel" | "session", sid: SessionId) {
  const s = sessions.value.get(sid);
  if (s) renaming.value = { kind, id: kind === "channel" ? s.channel_id : sid, sid };
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
  const l = layout.value;
  const p = group && l ? findPane(l.root, group) : null;
  const sid = p?.active ?? focusedSession.value?.id ?? null;
  const s = sid !== null ? sessions.value.get(sid) : undefined;
  return s ? channelById.value.get(s.channel_id) ?? null : null;
}

/**
 * `+` / Alt+T: a new tab in `group` (default focused), in the folder of that group's active tab,
 * with the last agent kind and no worktree. Ctrl+Shift+N (`worktree`): a worktree tab with the
 * focused tab's agent. No tab to take a folder from: the New tab dialog.
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
  createTab({
    workspace_id: c.workspace_id,
    path: ws?.path,
    agent,
    worktree: !!opts.worktree,
    group: opts.group,
  })
    .catch((e) => toast("error", `Couldn't open a tab: ${errText(e)}`));
}

export function newTabDialog(worktree?: boolean) {
  overlay.value = { kind: "new_tab", worktree };
}

/** Default folder for new tabs: the focused tab's, else the last used, else home. */
export function defaultFolder(): string {
  const c = focusedTask.value;
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
  const s = focusedSession.value;
  if (s) findOpen.value = { sid: s.id };
}

export function openJump(initial = "") {
  overlay.value = { kind: "jump", initial };
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
 * is in another window). `fresh`: a new conversation, restarting a running one.
 */
export async function openAgent(name: string, fresh = false) {
  const a = agents.value.find((x) => x.name === name);
  if (!a) return;
  pushRecent("a:" + a.id);
  if (!fresh && a.session_id !== null && agentLive(a)) return gotoSession(a.session_id);
  try {
    const mark = sessionMark();
    const r = await rpc.call<{ agent: Agent; session: SessionInfo; started: boolean }>("agent.start", {
      name: a.name,
      fresh: fresh || undefined,
      window_id: windowId.value ?? undefined,
    });
    knowAgent(r.agent);
    knowSession(r.session, mark);
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

/** Remove an agent (asks first, in the app's own dialog): its tab closes, its folder stays. */
export function removeAgent(a: Agent) {
  overlay.value = {
    kind: "confirm",
    title: `Remove ${a.name}?`,
    body:
      `Closes its tab and forgets the agent. Its folder (${a.home_dir}), with its persona and memory/, stays on disk.`,
    confirmLabel: "Remove agent",
    danger: true,
    run: () =>
      rpc.call("agent.remove", { name: a.name }).then(() => {
        agents.value = agents.value.filter((x) => x.id !== a.id);
      }).catch(fail("Remove failed")).catch(() => {}),
  };
}

export function newAgentDialog() {
  overlay.value = { kind: "agent" };
}

export function editAgent(name: string, tab: "settings" | "memory" = "settings") {
  overlay.value = { kind: "agent", name, tab };
}
