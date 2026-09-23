// User-facing actions. Components and the command table call these; they talk to the
// daemon and update local state (layouts, prefs, view).

import { signal } from "@preact/signals";
import { rpc } from "./rpc.ts";
import {
  activeChannelId,
  BUSY_STATES,
  channelById,
  channelOrder,
  channels,
  channelsByWorkspace,
  confirmClose,
  holdNewSessions,
  knowSession,
  layoutOf,
  layouts,
  overlay,
  prefs,
  pushRecent,
  releaseNewSessions,
  rightSuppressed,
  rightVisible,
  sessions,
  sessionsByChannel,
  setPref,
  toast,
  updateLayout,
  view,
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
  Workspace,
} from "./types.ts";
import {
  activateTab,
  addTab,
  cycleTab,
  type Direction,
  equalize as equalizeTree,
  findPane,
  focusDirection,
  focusPane as focusPaneTree,
  type LayoutState,
  panePx,
  pathOfPane,
  resizeFocused,
  type Side,
  type Size,
  splitFits,
  splitPane,
  toggleZoom,
} from "./layout/tree.ts";
import * as terms from "./terms.ts";
import { copyText } from "./util.ts";

export function fail(prefix: string) {
  return (e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e);
    toast("error", `${prefix}: ${msg}`);
    throw e;
  };
}

function quiet<T>(p: Promise<T>): Promise<T | undefined> {
  return p.catch(() => undefined);
}

// ---------- selection / navigation ----------

export function selectChannel(id: ChannelId, focusTerm = true) {
  if (!channelById.value.has(id)) return;
  view.value = "channel";
  activeChannelId.value = id;
  pushRecent("c:" + id);
  layoutOf(id);
  if (focusTerm) focusActiveTerminalSoon();
}

export function openInbox() {
  view.value = view.value === "inbox" ? "channel" : "inbox";
  if (view.value === "channel") focusActiveTerminalSoon();
}

export function stepChannel(delta: number) {
  const order = channelOrder.value;
  if (!order.length) return;
  const i = order.findIndex((c) => c.id === activeChannelId.value);
  const next = order[(i + delta + order.length) % order.length];
  selectChannel(next.id);
}

export function jumpChannel(n: number) {
  const c = channelOrder.value[n];
  if (c) selectChannel(c.id);
}

/** Active session of the focused pane in the active channel. */
export function focusedSession(): SessionInfo | null {
  const ch = activeChannelId.value;
  if (ch === null) return null;
  const l = layouts.value.get(ch);
  if (!l) return null;
  const p = findPane(l.root, l.focus);
  if (!p || p.active === null) return null;
  return sessions.value.get(p.active) ?? null;
}

export function focusActiveTerminalSoon() {
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      if (overlay.value || view.value !== "channel") return;
      const s = focusedSession();
      if (s) terms.focusTerminal(s.id);
    })
  );
}

/** Focus a session: switch to its channel, activate its tab, focus the terminal. */
export function revealSession(sid: SessionId) {
  const s = sessions.value.get(sid);
  if (!s) return;
  selectChannel(s.channel_id, false);
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
  const ch = activeChannelId.value;
  if (ch === null || view.value !== "channel") return;
  updateLayout(ch, fn);
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

// ---------- sessions ----------

export type Placement =
  | { type: "tab"; paneId?: string }
  | { type: "split"; side: Side; paneId?: string };

/** Size of the split area of the channel view (the box the layout tree is laid out in). */
function splitBox(): Size | null {
  const el = document.querySelector(".panes");
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 ? { w: r.width, h: r.height } : null;
}

/** The layout as it is on screen (zoom only changes what's drawn, splits unzoom). */
function visibleLayout(ch: ChannelId): LayoutState | null {
  return channelById.value.has(ch) ? layoutOf(ch) : null;
}

/** Whether pane `paneId` (default focused) has room to split on `side`. Toasts when not. */
export function canSplit(ch: ChannelId, side: Side, paneId?: string, quiet = false): boolean {
  const l = visibleLayout(ch);
  const box = splitBox();
  if (!l || !box || ch !== activeChannelId.value) return true; // not on screen: can't measure
  const id = paneId && findPane(l.root, paneId) ? paneId : l.focus;
  if (splitFits(l.root, id, side, box)) return true;
  if (!quiet) toast("warn", "Not enough room — zoom or close a pane");
  return false;
}

/** Terminal grid (cols/rows) a session will get in `placement`, so it starts at the right size. */
function gridFor(ch: ChannelId, placement: Placement): { cols: number; rows: number } | undefined {
  const l = visibleLayout(ch);
  const box = splitBox();
  if (!l || !box || ch !== activeChannelId.value) return undefined;
  const id = placement.paneId && findPane(l.root, placement.paneId) ? placement.paneId : l.focus;
  // A zoomed pane fills the whole box; a split unzooms first.
  const px = l.zoom === id && placement.type === "tab" ? box : panePx(l.root, id, box);
  if (!px) return undefined;
  let { w, h } = px;
  if (placement.type === "split") {
    if (placement.side === "left" || placement.side === "right") w = w / 2 - 1;
    else h = h / 2 - 1;
  }
  return terms.estimateGrid(w, h - terms.TAB_STRIP_PX);
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
    if (ch === activeChannelId.value) focusActiveTerminalSoon();
    return s;
  } catch (e) {
    toast("error", `Couldn't start ${kind}: ${e instanceof Error ? e.message : e}`);
  } finally {
    releaseNewSessions(ch, placed);
  }
}

export function spawnHere(kind: SessionKind) {
  const ch = activeChannelId.value;
  if (ch === null) return;
  const l = layoutOf(ch);
  spawn(ch, kind, { type: "tab", paneId: l.focus });
}

/** Alt+N: open the session picker on the focused pane's "+" (1/2/3, Enter repeats last). */
export async function spawnPicker() {
  const ch = activeChannelId.value;
  if (ch === null) return;
  const l = layoutOf(ch);
  const btn = document.querySelector(`[data-tabstrip="${l.focus}"] .tab-add`);
  const last = prefs.value.lastAgent === "none" ? "shell" : prefs.value.lastAgent;
  if (!btn) {
    spawnHere(last);
    return;
  }
  const { openMenuAt } = await import("./components/Menu.tsx");
  const { spawnMenuItems } = await import("./layout/Pane.tsx");
  const items = spawnMenuItems((k) => spawn(ch, k, { type: "tab", paneId: l.focus }));
  openMenuAt(btn, [{ header: "New session in this pane" }, ...items], {
    alignRight: false,
    select: 1 + ["claude", "codex", "shell"].indexOf(last),
    onClose: () => focusActiveTerminalSoon(),
  });
}

export function splitFocused(side: Side, kind: SessionKind = "shell") {
  const ch = activeChannelId.value;
  if (ch === null) return;
  view.value = "channel";
  spawn(ch, kind, { type: "split", side });
}

/** Close a tab. Busy sessions get an inline confirm first (tab turns into "Kill? ⏎ / Esc"). */
export function closeSession(sid: SessionId, force = false) {
  const s = sessions.value.get(sid);
  if (!s) return;
  if (!force && BUSY_STATES.has(s.state)) {
    confirmClose.value = sid;
    return;
  }
  confirmClose.value = null;
  rpc.call("session.remove", { id: sid }).catch(fail("Close failed")).catch(() => {});
}

export function sessionInActiveChannel(sid: SessionId): boolean {
  const s = sessions.value.get(sid);
  return !!s && view.value === "channel" && s.channel_id === activeChannelId.value;
}

export function closeFocused() {
  const s = focusedSession();
  if (s) closeSession(s.id);
}

export function restartSession(sid: SessionId, focus = true) {
  rpc.call("session.restart", { id: sid }).then(() => {
    const s = sessions.value.get(sid);
    if (focus && s && s.channel_id === activeChannelId.value) focusActiveTerminalSoon();
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

export async function send(sid: SessionId, text: string, mode: "queue" | "interrupt" = "queue") {
  const r = await rpc.call<{ delivered: boolean; queued: number }>("session.send", {
    id: sid,
    text,
    mode,
  });
  return r;
}

export function clearQueue(sid: SessionId) {
  rpc.call("session.clear_queue", { id: sid }).catch(fail("Clear queue failed")).catch(() => {});
}

// ---------- channels ----------

export interface NewChannelReq {
  workspace_id: number;
  name: string;
  worktree: boolean;
  base?: string;
  agent: SessionKind | null;
  prompt?: string;
  /** Omitted: the daemon's defaults. */
  permissions?: Permissions;
}

export async function createChannel(req: NewChannelReq): Promise<Channel> {
  // A new channel shows one pane filling the split area; size its first session for that.
  // (Ignored by daemons that don't take cols/rows here; the first resize on attach fixes it.)
  const box = splitBox() ?? estimatedChannelBox();
  const grid = box ? terms.estimateGrid(box.w, box.h - terms.TAB_STRIP_PX) : undefined;
  const c = await rpc.call<Channel>("channel.create", {
    workspace_id: req.workspace_id,
    name: req.name,
    worktree: req.worktree,
    base: req.base || undefined,
    agent: req.agent ?? undefined,
    prompt: req.prompt || undefined,
    permissions: req.permissions,
    ...grid,
  });
  setPref("lastAgent", req.agent ?? "none");
  // The channel_upserted event may lag the response.
  if (!channels.value.some((x) => x.id === c.id)) channels.value = [...channels.value, c];
  selectChannel(c.id);
  return c;
}

/** Split area size when no channel is open (welcome screen): center minus header and composer. */
function estimatedChannelBox(): Size | null {
  const el = document.querySelector(".center");
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return { w: r.width, h: r.height - 44 - 64 };
}

export function renameChannel(id: ChannelId, name: string) {
  const n = name.trim();
  const c = channelById.value.get(id);
  if (!n || !c || n === c.name) return;
  rpc.call("channel.rename", { id, name: n }).catch(fail("Rename failed")).catch(() => {});
}

/** Human-only: the channel's agent permission profile (new and restarted sessions). */
export async function setPermissions(id: ChannelId, permissions: Permissions): Promise<Channel> {
  const c = await rpc.call<Channel>("channel.set_permissions", { id, permissions });
  // channel_upserted may lag the reply.
  channels.value = channels.value.map((x) => (x.id === c.id ? c : x));
  toast("success", "Permissions saved. They apply to new and restarted sessions.");
  return c;
}

export function openPermissions(id: ChannelId) {
  overlay.value = { kind: "permissions", channelId: id };
}

export function archiveChannel(id: ChannelId, removeWorktree: boolean) {
  rpc.call("channel.archive", { id, remove_worktree: removeWorktree })
    .then(() => toast("info", "Channel archived"))
    .catch(fail("Archive failed")).catch(() => {});
}

export function reorderChannels(ids: ChannelId[]) {
  rpc.call("channel.reorder", { ids }).catch(fail("Reorder failed")).catch(() => {});
}

export function postNote(ch: ChannelId, text: string) {
  return rpc.call("channel.post", { id: ch, text });
}

export function markChannelRead(id: ChannelId) {
  quiet(rpc.call("channel.mark_read", { id }));
}

export async function addWorkspace(path: string, name?: string): Promise<Workspace> {
  const w = await rpc.call<Workspace>("workspace.add", { path, name: name || undefined });
  // The workspace_upserted event may lag the response.
  if (!workspaces.value.some((x) => x.id === w.id)) workspaces.value = [...workspaces.value, w];
  return w;
}

export function removeWorkspace(id: number) {
  rpc.call("workspace.remove", { id }).then(() => toast("info", "Workspace removed"))
    .catch(fail("Remove failed")).catch(() => {});
}

export function copy(text: string, what = "Copied") {
  copyText(text).then((ok) => toast(ok ? "success" : "error", ok ? what : "Copy failed", 1800));
}

// ---------- UI toggles ----------

export function toggleSidebar() {
  setPref("sidebarCollapsed", !prefs.value.sidebarCollapsed);
  terms.refitAll();
}

export function toggleRight(tab?: "timeline" | "diff") {
  const p = prefs.value;
  const open = rightVisible.value;
  if (tab && open && p.rightTab !== tab) {
    setPref("rightTab", tab);
    return;
  }
  if (tab) setPref("rightTab", tab);
  // Asking for the panel always wins over a narrow window's auto-close.
  rightSuppressed.value = false;
  setPref("rightOpen", !open);
}

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

export const composerFocus = signal(0);
export function focusComposer() {
  view.value = "channel";
  composerFocus.value++;
}

export const findOpen = signal<{ sid: number } | null>(null);
export function openFind() {
  const s = focusedSession();
  if (s) findOpen.value = { sid: s.id };
}

export function newChannelDialog(workspaceId?: number) {
  overlay.value = { kind: "new_channel", workspaceId };
}

export function channelsInWorkspace(wid: number): Channel[] {
  return channelsByWorkspace.value.get(wid) ?? [];
}

export function channelSessions(ch: ChannelId): SessionInfo[] {
  return sessionsByChannel.value.get(ch) ?? [];
}
