// Pure logic for the agents panel (docs/UI.md "Agents panel"): an agent's status from its
// session, the message log, and the New / Edit dialog's rules. No signals, no DOM: unit tested
// (agents_test.ts).

import type { Agent, AgentMessage, AgentState, Origin, SessionInfo } from "./types.ts";
import { STATE_LABEL } from "./types.ts";

/** What a row shows: the dot's state, whether a process runs, and the state's label and start. */
export interface AgentStatus {
  /** The dot (null: never started, no session). */
  state: AgentState | null;
  /** Its session is running (clicking it only pulls the tab). */
  live: boolean;
  label: string;
  /** Unix ms the state began (null: never started). */
  since: number | null;
}

export function agentStatus(a: Agent, s: SessionInfo | undefined): AgentStatus {
  if (!s || a.session_id === null) return { state: null, live: false, label: "stopped", since: null };
  if (s.state === "exited") return { state: "exited", live: false, label: "stopped", since: s.state_since };
  return { state: s.state, live: true, label: STATE_LABEL[s.state], since: s.state_since };
}

/** "now", "45s", "12m", "3h", "2d". */
export function ago(ms: number, now: number): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 10) return "now";
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** The first non-blank line, trimmed (persona hover, message preview). */
export function firstLine(text: string | null | undefined): string {
  return (text ?? "").split("\n").map((l) => l.trim()).find((l) => l) ?? "";
}

/** Merge a message (new, refused or just replied) into the log, newest first, at most `cap`. */
export function upsertMessage(list: AgentMessage[], m: AgentMessage, cap = 300): AgentMessage[] {
  const out = list.filter((x) => x.id !== m.id);
  const i = out.findIndex((x) => x.id < m.id);
  out.splice(i < 0 ? out.length : i, 0, m);
  return out.slice(0, cap);
}

/** The log as the panel shows it: messages to agent `id` (or all), newest first. */
export function feedFor(list: AgentMessage[], id: number | null): AgentMessage[] {
  return id === null ? list : list.filter((m) => m.to_agent === id);
}

/** Origins a Claude agent shouldn't take unattended (the dialog's one-line warning). */
export function unattendedWarning(kind: Agent["kind"], accepts: Origin[]): boolean {
  return kind === "claude" && accepts.some((o) => o === "schedule" || o === "webhook");
}

export const UNATTENDED_WARNING =
  "Anthropic's consumer terms are unclear about unattended use; scheduled work is safer on a Codex agent.";

/** Agent names: a letter or digit, then letters, digits, `-` or `_` (src/agents.rs `check_name`). */
export function nameError(name: string, taken: string[]): string | null {
  const n = name.trim();
  if (!n) return "Give it a name";
  if (n.length > 40 || !/^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(n)) {
    return "Letters, digits, - and _ only, starting with a letter or digit";
  }
  if (taken.some((t) => t.toLowerCase() === n.toLowerCase())) return `An agent named ${n} already exists`;
  return null;
}

/** The Model / Effort selects' entry for a typed value (a custom model id, an unknown level). Never
 * a real value: those are trimmed and have no spaces (src/agents.rs `agent_args`). */
export const CUSTOM = " custom";

/** Which entry a Model or Effort select shows: "" (Default) for no value, the value itself when
 * it is listed, else CUSTOM, as when the person chose "Custom…" (`typing`). A stored value the
 * list doesn't know is kept and shown as custom, never dropped. */
export function choiceOf(value: string, listed: string[], typing: boolean): string {
  const v = value.trim();
  if (typing) return CUSTOM;
  if (!v) return "";
  return listed.includes(v) ? v : CUSTOM;
}

/** Switching an agent's kind keeps a model or effort only when the new kind lists it. */
export function keepForKind(value: string, listed: string[]): string {
  return listed.includes(value.trim()) ? value : "";
}

/** A memory file path the daemon accepts: relative, plain parts (no `..`, `.` or empty parts). */
export function memoryPathError(path: string): string | null {
  const p = path.trim();
  if (!p) return "Name the file";
  if (p.startsWith("/") || p.endsWith("/")) return "A file name inside memory/, like notes.md";
  if (p.split("/").some((part) => !part || part === "." || part === "..")) return "No .. or empty parts";
  return null;
}
