// Wire types mirroring src/model.rs and src/hub.rs (snake_case JSON).

export type WorkspaceId = number;
export type ChannelId = number;
export type SessionId = number;

export type SessionKind = "claude" | "codex" | "shell";

export type AgentState =
  | "starting"
  | "working"
  | "needs_approval"
  | "asking_question"
  | "idle"
  | "error"
  | "stuck"
  | "exited";

export interface Workspace {
  id: WorkspaceId;
  name: string;
  path: string;
  is_git: boolean;
  created_at: number;
}

export interface Channel {
  id: ChannelId;
  workspace_id: WorkspaceId;
  name: string;
  cwd: string;
  branch: string | null;
  base_branch: string | null;
  owns_worktree: boolean;
  port_base: number;
  archived: boolean;
  layout: unknown;
  created_at: number;
  last_read_at: number;
  sort_order: number;
}

export interface SessionInfo {
  id: SessionId;
  channel_id: ChannelId;
  kind: SessionKind;
  name: string;
  cwd: string;
  state: AgentState;
  state_detail: string | null;
  state_since: number;
  title: string | null;
  parent_id: SessionId | null;
  pid: number | null;
  exit_code: number | null;
  cols: number;
  rows: number;
  queued: number;
  created_at: number;
}

export type EventKind =
  | "note"
  | "prompt"
  | "state_change"
  | "session_started"
  | "session_exited"
  | "hook"
  | "mcp_call"
  | "agent_post"
  | "approval"
  | "system";

export interface TimelineEvent {
  id: number;
  channel_id: ChannelId | null;
  session_id: SessionId | null;
  kind: EventKind;
  // deno-lint-ignore no-explicit-any
  data: any;
  ts: number;
}

export type ServerEvent =
  | { event: "workspace_upserted"; data: Workspace }
  | { event: "workspace_removed"; data: { id: WorkspaceId } }
  | { event: "channel_upserted"; data: Channel }
  | { event: "channel_removed"; data: { id: ChannelId } }
  | { event: "session_upserted"; data: SessionInfo }
  | { event: "session_removed"; data: { id: SessionId } }
  | { event: "event"; data: TimelineEvent }
  | { event: "notice"; data: { level: "info" | "warn" | "error"; text: string } };

export interface HelloResult {
  version: string;
  workspaces: Workspace[];
  channels: Channel[];
  sessions: SessionInfo[];
  home: string;
  default_shell: string;
}

export type Decision = "approve" | "approve_always" | "deny";

export const NEEDS_HUMAN: ReadonlySet<AgentState> = new Set([
  "needs_approval",
  "asking_question",
  "error",
  "stuck",
]);

export function needsHuman(s: AgentState): boolean {
  return NEEDS_HUMAN.has(s);
}

/** Aggregate priority (lower index wins). */
export const STATE_PRIORITY: AgentState[] = [
  "needs_approval",
  "asking_question",
  "error",
  "stuck",
  "working",
  "starting",
  "idle",
  "exited",
];

export const STATE_LABEL: Record<AgentState, string> = {
  starting: "starting",
  working: "working",
  needs_approval: "needs approval",
  asking_question: "asking",
  idle: "idle",
  error: "error",
  stuck: "stuck",
  exited: "exited",
};

export const KIND_LABEL: Record<SessionKind, string> = {
  claude: "Claude",
  codex: "Codex",
  shell: "Shell",
};
