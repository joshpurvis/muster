// The `manage` scope on the phone: new tabs, and restart / stop / rename / close of a session or
// task. Only a device paired with `manage` sees these controls; the daemon checks every call
// again (docs/PROTOCOL.md "Remote devices"), so the phone only decides what to show and say.
import { computed, effect, signal } from "@preact/signals";
import { type RefObject } from "preact";
import { useEffect, useLayoutEffect, useRef } from "preact/hooks";
import { rpc } from "../rpc.ts";
import type { Channel, HelloResult, SessionInfo, SessionKind, Workspace } from "../types.ts";
import { type Agents, KIND_LABEL, tildify } from "./manage_logic.ts";
import { closeSession, device, sessions } from "./store.ts";

/** This device was paired with `manage`. Controls stay on screen while reconnecting (so a
 * half-filled form survives) and are disabled until `ready`. */
export const canManage = computed(() => !!device.value?.scopes.includes("manage"));

/** What the phone's own state doesn't keep: workspaces, the home folder and which sessions are
 * named agents' (`agents`: null from a backend without `agent_sessions`). Read fresh whenever a
 * screen that needs it opens or reconnects, since devices get no agent or workspace events. */
export interface ManageInfo {
  workspaces: Workspace[];
  channels: Channel[];
  home: string;
  agents: Agents | null;
}
export const info = signal<ManageInfo | null>(null);

export async function loadInfo(): Promise<ManageInfo> {
  const h = await rpc.call<HelloResult & { agent_sessions?: { id: number; name: string }[] }>("hello");
  const next = {
    workspaces: h.workspaces,
    channels: h.channels,
    home: h.home,
    agents: Array.isArray(h.agent_sessions) ? new Map(h.agent_sessions.map((a) => [a.id, a.name])) : null,
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
export async function openTab(req: NewTab, home: string): Promise<SessionInfo> {
  const ws = req.workspace_id === undefined
    ? await rpc.call<Workspace>("workspace.add", { path: (req.path ?? "").trim() })
    : null;
  // The daemon makes no worktree for a folder outside git: it would run in the folder itself.
  if (req.worktree && ws && !ws.is_git) {
    throw new Error(
      `${tildify(ws.path, home)} isn't a git repository, so it can't have its own worktree. ` +
        "Untick “Own git worktree” to work in the folder itself.",
    );
  }
  const c = await rpc.call<Channel>("channel.create", {
    workspace_id: ws?.id ?? req.workspace_id,
    agent: req.agent,
    worktree: req.worktree,
    prompt: req.agent !== "shell" && req.prompt?.trim() ? req.prompt.trim() : undefined,
  });
  const first = await firstSession(c.id, 8000);
  if (!first) throw new Error(`${KIND_LABEL[req.agent]} didn't start. Check the new tab on your computer.`);
  return first;
}

export const newTabOpen = signal(false);

// ---------- layers: a screen or sheet over the current one ----------

let layers = 0;

/** A history entry for an overlay, so Android's Back closes it before leaving the screen under
 * it. `onGone` runs once the entry is left (Back, or `close`). */
export function useHistoryLayer(onGone: () => void) {
  const id = useRef(0);
  const gone = useRef(false);
  const latest = useRef(onGone);
  latest.current = onGone;
  const top = () => id.current !== 0 && history.state?.musterLayer === id.current;
  useEffect(() => {
    id.current = ++layers;
    history.pushState({ musterLayer: id.current, below: history.state ?? null }, "", location.href);
    const pop = () => {
      if (gone.current || top()) return;
      gone.current = true;
      latest.current();
    };
    addEventListener("popstate", pop);
    return () => {
      removeEventListener("popstate", pop);
      // Unmounted from elsewhere (its session went away): drop the entry too.
      if (!gone.current && top()) {
        gone.current = true;
        history.back();
      }
    };
  }, []);
  return {
    /** Whether this layer's entry is still the current one. */
    current: top,
    close() {
      if (gone.current) return;
      if (top()) history.back();
      else {
        gone.current = true;
        latest.current();
      }
    },
    /** Swap this layer's entry for the session `sid` (Back then goes where Back went before).
     * False, and nothing changes, when the layer is no longer the current entry. */
    replaceWithSession(sid: number): boolean {
      if (gone.current || !top()) return false;
      gone.current = true;
      history.replaceState({ musterSession: true }, "", `#s=${sid}`);
      latest.current();
      return true;
    },
    /** Leave this layer and the session screen under it for the list. */
    leaveSession() {
      if (gone.current) return;
      gone.current = true;
      if (top() && history.state?.below?.musterSession) history.go(-2);
      else closeSession();
    },
  };
}

/** Modal: everything outside `root` is inert (out of reach of Tab and screen readers) while it
 * is open, except status messages; focus moves to `initial` (or the first focusable element)
 * and goes back where it was when it closes. */
export function useModal(root: RefObject<HTMLElement>) {
  useLayoutEffect(() => {
    const before = document.activeElement as HTMLElement | null;
    const made: HTMLElement[] = [];
    let node = root.current;
    while (node?.parentElement && !node.classList.contains("mobile-app")) {
      for (const sibling of node.parentElement.children) {
        if (
          sibling === node || !(sibling instanceof HTMLElement) || sibling.inert ||
          sibling.matches(".toast, [role=status]")
        ) continue;
        sibling.inert = true;
        made.push(sibling);
      }
      node = node.parentElement;
    }
    return () => {
      for (const el of made) el.inert = false;
      if (before?.isConnected) before.focus();
    };
  }, []);
}

/** Focus the element marked `data-autofocus` in `root`, else its first focusable one. */
export function focusFirst(root: HTMLElement | null) {
  root?.querySelector<HTMLElement>("[data-autofocus], button:not(:disabled), input, textarea")?.focus();
}
