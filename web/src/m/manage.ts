// The `manage` scope on the phone: new tabs, and restart / stop / rename / close of a session or
// task. Only a device paired with `manage` sees these controls; the daemon checks every call
// again (docs/PROTOCOL.md "Remote devices"), so the phone only decides what to show and say.
import { computed, effect, signal } from "@preact/signals";
import { rpc } from "../rpc.ts";
import type { Channel, HelloResult, SessionInfo, SessionKind, Workspace } from "../types.ts";
import { KIND_LABEL } from "./manage_logic.ts";
import { device, selected, sessions } from "./store.ts";

/** This device was paired with `manage`. Controls stay on screen while reconnecting (so a
 * half-filled form survives) and are disabled until `ready`. */
export const canManage = computed(() => !!device.value?.scopes.includes("manage"));

/** What the phone's own state doesn't keep: workspaces, the home folder and which sessions are
 * named agents' (`agent_sessions`; null from older backends). Read fresh when a sheet opens,
 * since devices get no agent or workspace events. */
export interface ManageInfo {
  workspaces: Workspace[];
  channels: Channel[];
  home: string;
  agents: Set<number> | null;
}
export const info = signal<ManageInfo | null>(null);

export async function loadInfo(): Promise<ManageInfo> {
  const h = await rpc.call<HelloResult & { agent_sessions?: number[] }>("hello");
  const next = {
    workspaces: h.workspaces,
    channels: h.channels,
    home: h.home,
    agents: h.agent_sessions ? new Set(h.agent_sessions) : null,
  };
  info.value = next;
  return next;
}

/** Wait (up to `ms`) for the first session of a channel just created. */
export function firstSession(channelId: number, ms: number): Promise<SessionInfo | null> {
  return new Promise((resolve) => {
    let done = false;
    let dispose = () => {};
    const finish = (s: SessionInfo | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      queueMicrotask(() => dispose());
      resolve(s);
    };
    const timer = setTimeout(() => finish(null), ms);
    dispose = effect(() => {
      const s = [...sessions.value.values()].find((x) => x.channel_id === channelId);
      if (s) finish(s);
    });
  });
}

export interface NewTab {
  workspace_id?: number;
  path?: string;
  agent: SessionKind;
  worktree: boolean;
  prompt?: string;
}

/** A new task with its first terminal; the daemon picks its window, name and (default)
 * permissions. Resolves with that terminal once it shows up. */
export async function openTab(req: NewTab): Promise<SessionInfo> {
  const workspace_id = req.workspace_id ??
    (await rpc.call<Workspace>("workspace.add", { path: (req.path ?? "").trim() })).id;
  const c = await rpc.call<Channel>("channel.create", {
    workspace_id,
    agent: req.agent,
    worktree: req.worktree,
    prompt: req.agent !== "shell" && req.prompt?.trim() ? req.prompt.trim() : undefined,
  });
  const first = await firstSession(c.id, 8000);
  if (!first) throw new Error(`${KIND_LABEL[req.agent]} didn't start. Check the new tab on your computer.`);
  return first;
}

// ---------- the new-tab screen's place in history (so Android's Back closes it) ----------

export const newTabOpen = signal(false);

export function showNewTab() {
  history.pushState({ musterSheet: "new" }, "", "#new");
  newTabOpen.value = true;
}
export function hideNewTab() {
  if (history.state?.musterSheet) history.back();
  else newTabOpen.value = false;
}
/** Leave the new-tab screen for the terminal just started (Back then goes to the list). */
export function showStarted(s: SessionInfo) {
  history.replaceState({ musterSession: true }, "", `#s=${s.id}`);
  newTabOpen.value = false;
  selected.value = s.id;
}
addEventListener("popstate", () => {
  if (!history.state?.musterSheet) newTabOpen.value = false;
});
