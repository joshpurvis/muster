// What the phone's manage controls show and say (pure; see manage.ts for the calls).
import type { Channel, SessionInfo, SessionKind, Workspace } from "../types.ts";

export const KIND_LABEL: Record<SessionKind, string> = { claude: "Claude", codex: "Codex", shell: "Shell" };

/** Named agents' sessions (`hello.agent_sessions`): session id → the agent's name
 * (`hello.agent_names`), or null when the backend doesn't send names. */
export type Agents = Map<number, string | null>;

/** The named agent session `s` belongs to, or null. `hidden` is only ever set on an agent's
 * session, so a hidden one counts even before the list is known. The tab's name stands in when
 * the agent's isn't known. */
export function agentName(s: SessionInfo, agents: Agents | null): string | null {
  if (agents?.has(s.id)) return agents.get(s.id) || s.name;
  return s.hidden ? s.name : null;
}

export function tildify(path: string, home: string): string {
  const h = home.replace(/\/+$/, "");
  if (!h) return path;
  return path === h ? "~" : path.startsWith(h + "/") ? "~" + path.slice(h.length) : path;
}

/** A folder suggestion as its own name plus the folder it is in (`~/a/b/` → `b`, `~/a/`). */
export function splitFolder(entry: string): { name: string; parent: string } {
  const clean = entry.replace(/\/+$/, "");
  const cut = clean.lastIndexOf("/");
  if (cut < 0) return { name: clean || entry, parent: "" };
  return { name: clean.slice(cut + 1) || "/", parent: clean.slice(0, cut + 1) };
}

/** Workspaces for the picker, most recently used first. The agents' own workspace (where named
 * agents live) is left out: a new tab there would be neither an agent nor a project. */
export function workspaceChoices(
  workspaces: Workspace[],
  channels: Channel[],
  all: SessionInfo[],
  agents: Agents | null,
): Workspace[] {
  const byId = new Map(channels.map((c) => [c.id, c]));
  const used = new Map<number, number>();
  const touch = (ws: number, at: number) => used.set(ws, Math.max(used.get(ws) ?? 0, at));
  for (const c of channels) touch(c.workspace_id, c.created_at);
  const agentWorkspaces = new Set<number>();
  for (const s of all) {
    const c = byId.get(s.channel_id);
    if (!c) continue;
    touch(c.workspace_id, s.created_at);
    if (agents?.has(s.id)) agentWorkspaces.add(c.workspace_id);
  }
  return workspaces
    .filter((w) => !(w.name === "agents" && agentWorkspaces.has(w.id)))
    .sort((a, b) => (used.get(b.id) ?? b.created_at) - (used.get(a.id) ?? a.created_at) || a.id - b.id);
}

// ---------- confirmations (plain words: what happens, what stays) ----------

export interface Confirm {
  title: string;
  body: string[];
  action: string;
  danger: boolean;
  /** What it acts on (ids), so the same words about another task or tab don't count as the same. */
  targets?: string;
}

/** Two confirmations say the same thing about the same targets (so one already read still
 * describes what happens). */
export const sameConfirm = (a: Confirm, b: Confirm) => JSON.stringify(a) === JSON.stringify(b);

const MID_TURN = new Set(["working", "stuck", "needs_approval", "asking_question"]);
export const midTurn = (s: SessionInfo) => s.kind !== "shell" && MID_TURN.has(s.state);

/** Restart asks only while an agent is mid-turn (as on the desktop); null = act at once. Claude
 * and Codex both reopen their last conversation on a restart. */
export function restartConfirm(s: SessionInfo): Confirm | null {
  if (s.state === "exited" || !midTurn(s)) return null;
  return {
    title: `Restart ${s.name}?`,
    body: [
      `${KIND_LABEL[s.kind]} is working. Its current turn is lost.`,
      "It starts again and reopens its last conversation.",
    ],
    action: "Restart",
    danger: true,
  };
}

/** `agent`: the named agent's name, or null. */
export function stopConfirm(s: SessionInfo, agent: string | null): Confirm {
  const body = [
    s.kind === "shell"
      ? "Its program ends now, with anything running in it (a build, a dev server)."
      : `${KIND_LABEL[s.kind]} ends now${midTurn(s) ? " and its current turn is lost" : ""}.`,
    "The tab and its output stay, so you can read it or start it again from this menu.",
  ];
  if (agent) {
    body.push(
      `${agent} is a named agent: the next message or schedule for it starts it again.`,
    );
  }
  return { title: `Stop ${s.name}?`, body, action: "Stop", danger: true };
}

/** `last`: the only tab of its task (removing it closes the task, docs/PROTOCOL.md). */
export function closeConfirm(
  s: SessionInfo,
  agent: string | null,
  task: Channel | undefined,
  last: boolean,
): Confirm {
  if (agent) {
    return {
      title: `Close ${agent}'s tab?`,
      body: [
        `${agent} is a named agent, so closing its tab only hides it. It keeps running and still gets messages.`,
        "Show it again from the agents panel on your computer. To end it, use Stop.",
      ],
      action: "Close tab",
      danger: false,
    };
  }
  const body = [
    s.state === "exited"
      ? "The tab and its saved output are removed."
      : "Its program stops and the tab is removed with its output.",
  ];
  if (last && task) {
    body.push(`It's the last tab of the task “${task.name}”, so the task closes too. ${keptText(task)}`);
  }
  body.push("This can't be undone.");
  return { title: `Close ${s.name}?`, body, action: "Close tab", danger: true };
}

export function archiveConfirm(task: Channel, inTask: SessionInfo[], agents: Agents | null): Confirm {
  const live = inTask.filter((s) => s.state !== "exited").length;
  const named = inTask.map((s) => agentName(s, agents)).filter((n): n is string => !!n);
  const tabs = inTask.length === 1 ? "its tab" : `its ${inTask.length} tabs`;
  const body = [
    live
      ? `Stops ${live === 1 ? "its running terminal" : `its ${live} running terminals`} and removes ${tabs}.`
      : `Removes ${tabs}.`,
  ];
  if (named.length) {
    const them = named.length > 1 ? "them" : "it";
    body.push(
      `That includes the named agent${named.length > 1 ? "s" : ""} ${
        named.join(", ")
      }: archiving stops ${them} too.`,
    );
  }
  body.push(keptText(task));
  return { title: `Archive “${task.name}”?`, body, action: "Archive task", danger: true };
}

function keptText(task: Channel): string {
  if (task.owns_worktree) {
    return `Its worktree folder${task.branch ? ` and the branch ${task.branch}` : ""} stay on your computer.`;
  }
  return "Its folder and files stay on your computer.";
}
