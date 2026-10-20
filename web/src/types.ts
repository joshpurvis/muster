// Wire types mirroring src/model.rs and src/hub.rs (snake_case JSON).

import type { UpdateStatus } from "./updates.ts";

export type WorkspaceId = number;
export type ChannelId = number;
export type SessionId = number;

export type SessionKind = "claude" | "codex" | "shell";

/** A reference to a directory managed exclusively by Claude Code. */
export interface AccountProfile {
  id: number;
  name: string;
  config_dir: string;
  enabled: boolean;
  created_at: number;
}

/** Omitted: inherit. "default": explicitly use the ordinary inherited Claude environment. */
export type ProfileChoice = number | "default";

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
  account_profile_id?: number | null;
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
  /** v1 per-channel split layout (only read to migrate). */
  layout?: unknown;
  created_at: number;
  last_read_at: number;
  sort_order: number;
  /** Agent permission profile (set by the human only). Missing from old daemons. */
  permissions?: Permissions;
  /** Window showing this channel as a tab. Missing from daemons without windows. */
  window_id?: WindowId | null;
  /** Name is derived by the daemon until the human renames the tab. Missing = false. */
  auto_named?: boolean;
}

export type WindowId = number;

/** A client window (docs/PROTOCOL.md "Windows, tabs and naming", "Groups layout (v2)"). */
export interface Win {
  id: WindowId;
  name: string;
  /** Tasks whose default window this is (v1: the tab order). */
  channel_ids: ChannelId[];
  /** The split tree of groups (opaque to the daemon, owned by this UI). Missing before v2. */
  layout?: unknown;
  /** A connection holds it. */
  open: boolean;
  last_focused_at: number;
  created_at: number;
}

export type CodexSandbox = "read_only" | "workspace_write" | "workspace_write_network" | "full_access";
export type ClaudeMode = "default" | "accept_edits" | "plan" | "bypass";

export interface Permissions {
  codex_sandbox: CodexSandbox;
  claude_mode: ClaudeMode;
}

export const DEFAULT_PERMISSIONS: Permissions = {
  codex_sandbox: "workspace_write",
  claude_mode: "default",
};

export interface SessionInfo {
  account_profile_id?: number | null;
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
  /** Last PTY resizer: "local" or "device:<id>". Missing on older daemons. */
  size_owner?: string | null;
  /** Typing is paused: earlier input may not have reached the terminal. Cleared by
   * `session.continue_input` (or a new run). */
  input_interrupted?: boolean;
  /** The folder the process is in now (follows `cd` and Claude's `/cd`); `cwd` is where it started. */
  live_cwd?: string;
  /** Changes with every state/detail change and answered prompt (stale-approval guard). */
  state_seq?: number;
  queued: number;
  /** Built-in subagents the agent is running right now (runtime only). */
  subagents?: number;
  created_at: number;
  /** The window showing this session as a tab. Missing before v2: then its channel's window. */
  window_id?: WindowId | null;
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

/** Where a message chain started (docs/MCP.md "Agents"). */
export type Origin = "human" | "schedule" | "webhook";

/** A named agent (`agent.list`, `agent_upserted`; src/agents.rs `agent_json`). */
export interface Agent {
  account_profile_id?: number | null;
  id: number;
  name: string;
  kind: "claude" | "codex";
  model: string | null;
  effort: string | null;
  persona: string;
  permissions: Permissions;
  accepts_origins: Origin[];
  autostart: boolean;
  home_dir: string;
  /** Its own tab (task). */
  channel_id: ChannelId | null;
  /** Its current session (live or exited). */
  session_id: SessionId | null;
  resumable: boolean;
  created_at: number;
}

/** A message muster delivered to an agent, or refused to (`agent.messages`, `agent_message`). */
export interface AgentMessage {
  id: number;
  /** Sender label: an agent name, `session:<id>`, `human`. */
  from: string;
  from_session: SessionId | null;
  /** The recipient's name (`#<id>` once removed). */
  to: string;
  to_agent: number;
  to_session: SessionId | null;
  origin: Origin;
  hop: number;
  reply_to: number | null;
  created_at: number;
  /** Null for messages from before muster kept the text. */
  text: string | null;
  reply: string | null;
  replied_at: number | null;
  /** Why the agent's policy refused it (never delivered). */
  refused: string | null;
}

/** A New agent template (`agent.templates`). */
export interface AgentTemplate {
  id: string;
  label: string;
  about: string;
  name: string;
  kind: "claude" | "codex";
  model: string | null;
  effort: string | null;
  persona: string;
  permissions: Permissions;
  accepts: Origin[];
  autostart: boolean;
}

/** `agent.models`: what the agent dialog offers per CLI (src/agents.rs `model_catalog`). */
export interface KindModels {
  models: { id: string; note: string | null }[];
  efforts: string[];
}
export type ModelCatalog = Record<"claude" | "codex", KindModels>;

export type ServerEvent =
  | { event: "panel_upserted"; data: Panel }
  | { event: "panel_removed"; data: { id: number } }
  | { event: "account_profiles_changed"; data: AccountProfile[] }
  | { event: "workspace_upserted"; data: Workspace }
  | { event: "workspace_removed"; data: { id: WorkspaceId } }
  | { event: "channel_upserted"; data: Channel }
  | { event: "channel_removed"; data: { id: ChannelId } }
  | { event: "session_upserted"; data: SessionInfo }
  | { event: "session_removed"; data: { id: SessionId } }
  | { event: "event"; data: TimelineEvent }
  | { event: "notice"; data: { level: "info" | "warn" | "error"; text: string } }
  | { event: "window_upserted"; data: Win }
  | { event: "window_removed"; data: { id: WindowId } }
  | { event: "agent_upserted"; data: Agent }
  | { event: "agent_removed"; data: { id: number; name: string } }
  | { event: "agent_message"; data: AgentMessage }
  | { event: "updates_changed"; data: UpdateStatus }
  | { event: "prefs_changed"; data: WindowPrefs }
  | { event: "daemon_stopping"; data: { reason: string } };

/** Preferences every window shares, kept by the backend (`prefs.get` / `prefs.set`). */
export interface WindowPrefs {
  /** Native windows show the system title bar. */
  decorations: boolean;
  /** Shell new shell tabs run; absent: the account's login shell (`hello.login_shell`). */
  shell?: string;
  /** Raised by every `prefs.set` (missing from the first builds that had prefs). */
  revision?: number;
}

export interface HelloResult {
  panels?: Panel[];
  panel_origin?: string;
  account_profiles?: AccountProfile[];
  ui_protocol?: number;
  paused_deliveries?: SessionId[];
  version: string;
  workspaces: Workspace[];
  channels: Channel[];
  sessions: SessionInfo[];
  /** Missing from daemons without windows: then every channel is a tab of this window. */
  windows?: Win[];
  home: string;
  default_shell: string;
  /** The shell used when none is set: the account's login shell, else `$SHELL`. */
  login_shell?: string;
  /** Missing from backends that don't watch for updates. */
  updates?: UpdateStatus;
  /** Missing from backends without shared window preferences. */
  prefs?: WindowPrefs;
}

export interface PanelCsp {
  resourceDomains?: string[];
  connectDomains?: string[];
  frameDomains?: string[];
  baseUriDomains?: string[];
}

export interface Panel {
  id: number;
  channel_id: ChannelId;
  session_id: SessionId;
  window_id: WindowId | null;
  title: string;
  app: "markdown" | "image" | "chart" | "table" | "pr_decision" | "pr_overview" | "board" | "web_preview" | "custom";
  csp: PanelCsp;
  revision: number;
  /** The revision whose `panel_update` last carried data (absent from older backends). */
  data_revision?: number;
  html_revision: number;
  /** A custom panel opened with `kit: true`: Muster puts the kit in front of its HTML. */
  kit?: boolean;
  created_at: number;
  updated_at: number;
  closed: boolean;
}

export interface PanelResource {
  panel: Panel;
  html: string;
  data: Record<string, unknown>;
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
