// What the phone's manage controls show and say (pure; see manage.ts for the calls).
import type { Channel, SessionInfo, SessionKind, Workspace } from "../types.ts";

export const KIND_LABEL: Record<SessionKind, string> = { claude: "Claude", codex: "Codex", shell: "Shell" };

/** A named agent's session, as far as the phone knows: `hidden` is only ever set on one. */
export function isAgent(s: SessionInfo, agents: Set<number> | null): boolean {
  return !!s.hidden || !!agents?.has(s.id);
}

export function tildify(path: string, home: string): string {
  const h = home.replace(/\/+$/, "");
  if (!h) return path;
  return path === h ? "~" : path.startsWith(h + "/") ? "~" + path.slice(h.length) : path;
}

/** Workspaces for the picker, most recently used first. The agents' own workspace (where named
 * agents live) is left out: a new tab there would be neither an agent nor a project. */
export function workspaceChoices(
  workspaces: Workspace[],
  channels: Channel[],
  all: SessionInfo[],
  agents: Set<number> | null,
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
}

const MID_TURN = new Set(["working", "stuck", "needs_approval", "asking_question"]);
export const midTurn = (s: SessionInfo) => s.kind !== "shell" && MID_TURN.has(s.state);

/** Restart asks only while an agent is mid-turn (as on the desktop); null = act at once. */
export function restartConfirm(s: SessionInfo): Confirm | null {
  if (s.state === "exited" || !midTurn(s)) return null;
  return {
    title: `Restart ${s.name}?`,
    body: [
      `${KIND_LABEL[s.kind]} is working. Its current turn is lost.`,
      s.kind === "claude"
        ? "It starts again in the same conversation."
        : "The program starts fresh, without this conversation.",
    ],
    action: "Restart",
    danger: true,
  };
}

export function stopConfirm(s: SessionInfo, agent: boolean): Confirm {
  const body = [
    s.kind === "shell"
      ? "Its program ends now, with anything running in it (a build, a dev server)."
      : `${KIND_LABEL[s.kind]} ends now${midTurn(s) ? " and its current turn is lost" : ""}.`,
    "The tab and its output stay, so you can read it or start it again from this menu.",
  ];
  if (agent) body.push(`${s.name} is a named agent: it stays stopped until you restart it.`);
  return { title: `Stop ${s.name}?`, body, action: "Stop", danger: true };
}

/** `last`: the only tab of its task (removing it closes the task, docs/PROTOCOL.md). */
export function closeConfirm(
  s: SessionInfo,
  agent: boolean,
  task: Channel | undefined,
  last: boolean,
): Confirm {
  if (agent) {
    return {
      title: `Close ${s.name}'s tab?`,
      body: [
        `${s.name} is a named agent, so closing its tab only hides it. It keeps running and still gets messages.`,
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

export function archiveConfirm(task: Channel, inTask: SessionInfo[], agents: Set<number> | null): Confirm {
  const live = inTask.filter((s) => s.state !== "exited").length;
  const named = inTask.filter((s) => isAgent(s, agents)).map((s) => s.name);
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
