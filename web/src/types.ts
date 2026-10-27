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
  /** A named agent's closed tab (`session.hide`): it runs on in no window until it is shown
   * again (`session.set_window`). Missing from older daemons. */
  hidden?: boolean;
  /** What its current (or last) run bills to, set when it starts: a named agent's provider
   * change shows here only once a restart applies it. Missing from older daemons. */
  provider?: Provider;
  /** Unix ms of the last model request the daemon saw (a turn's start or end); null before
   * the conversation's first turn, and for shells. Missing from older daemons. */
  last_turn_at?: number | null;
  /** When the prompt cache has likely expired (`last_turn_at` + the provider's lifetime);
   * null for shells, before the first turn, or with the warning off. */
  cache_cold_at?: number | null;
  /** The cache lifetime that applies to this run, in minutes (its kind, provider, launch
   * environment and the prefs); null like `cache_cold_at`. */
  cache_ttl_min?: number | null;
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
/** Where an agent's model runs (src/openrouter.rs): the CLI's own login, or OpenRouter (pay per
 * token). Missing from backends without OpenRouter support. */
export type Provider = "subscription" | "openrouter";

export interface Agent {
  account_profile_id?: number | null;
  provider?: Provider;
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
  /** Typed in after the agent's prompt cache had likely expired: how long it had been idle
   * (ms). Null or missing: warm, not delivered yet, or an older daemon. */
  cold_idle_ms?: number | null;
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
  /** `efforts`: this model's levels where they differ from the CLI's (null: the CLI's). */
  models: { id: string; note: string | null; efforts: string[] | null }[];
  efforts: string[];
}
export type ModelCatalog = Record<"claude" | "codex", KindModels>;

/** One model from OpenRouter's public list (`openrouter.models`). */
export interface OpenRouterModel {
  /** What `--model` / `-m` gets, e.g. `anthropic/claude-sonnet-5.5`. */
  id: string;
  name: string;
  /** The id's prefix without `~`, for grouping. */
  vendor: string;
  context_length: number | null;
  /** US dollars per million tokens. */
  prompt_per_m: number | null;
  completion_per_m: number | null;
  created: number | null;
  /** An alias (`~anthropic/claude-opus-latest`): the model it points at now. */
  alias_of?: string | null;
}

/** `openrouter.models`: the daemon's cached copy of OpenRouter's list, filtered for coding agents. */
export interface OpenRouterModels {
  key_present: boolean;
  models: OpenRouterModel[];
  /** Unix ms of the fetch (null: never fetched). */
  fetched_at: number | null;
  stale: boolean;
  /** Why the last fetch failed (null: it didn't). */
  error: string | null;
}

// ---------------- schedules (docs/PROTOCOL.md "Schedules") ----------------

/** The GitHub events a trigger polls for. */
export type GhEvent = "pr_opened" | "ci_failed";
/** What a trigger waits for (Phase 2): runs have origin `webhook`, shown as "Triggered". */
export type TriggerWhen =
  | { kind: "note"; note_id: number; quiet_min: number }
  | { kind: "git"; repo: string; branch: string }
  | { kind: "github"; repo: string; event: GhEvent; branch: string | null; every_min: number }
  | { kind: "url"; name: string };
/** When a schedule runs: a 5-field cron line (or `@daily` and friends), once at a time, or when
 * something happens (a trigger). */
export type When = { kind: "cron"; cron: string } | { kind: "once"; at: number } | TriggerWhen;
/** What `schedule.create` / `.update` / `.preview` take: a one-off may also be a wall-clock time
 * ("2026-10-21T15:30") the backend reads in the schedule's `tz`. */
export type WhenInput =
  | { kind: "cron"; cron: string }
  | { kind: "once"; at: number }
  | { kind: "once"; local: string }
  | { kind: "note"; note_id: number; quiet_min?: number }
  | { kind: "git"; repo: string; branch: string }
  | { kind: "github"; repo: string; event: GhEvent; branch?: string | null; every_min?: number }
  | { kind: "url"; name: string };

/** A "Then…" step: ask an agent (the prompt may quote `{reply}`), or on failure just notify. */
export interface ThenStep {
  notify: boolean;
  agent: string | null;
  agent_id: number | null;
  prompt: string;
  then: Then | null;
}
export interface Then {
  on_success: ThenStep | null;
  on_failure: ThenStep | null;
}
/** What `schedule.create` / `.update` take as `then` (agents by name). */
export interface ThenInput {
  on_success?: { agent: string; prompt: string; then?: ThenInput } | null;
  on_failure?: { notify: true } | { agent: string; prompt: string; then?: ThenInput } | null;
}
/** An event a trigger holds for its next run (or recorded with a triggered run). */
export interface TriggerEvent {
  at: number;
  /** What happened, one line per event. */
  text: string;
  count: number;
  /** Data came with it (trigger URL, `muster trigger`). */
  payload: boolean;
}

export type MissedRule = "auto" | "run_once" | "skip";
export type RunOutcome =
  | "pending"
  | "replied"
  | "blocked"
  | "no_reply"
  | "ended"
  | "timed_out"
  | "refused"
  | "skipped"
  | "failed";

/** One run of a schedule (`schedule.runs`, `schedule_run`, `Schedule.last_run`). */
export interface ScheduleRun {
  id: number;
  schedule_id: number;
  /** The planned time (unix ms); now for Run now. */
  slot: number;
  kind: "scheduled" | "catch_up" | "manual" | "triggered" | "chain";
  started_at: number;
  finished_at: number | null;
  outcome: RunOutcome;
  /** Replied after the wait ended (the outcome reads "replied"). */
  late_reply: boolean;
  /** Why it was skipped, refused, blocked or failed; a catch-up's note. */
  reason: string | null;
  msg_id: number | null;
  /** The message it sent, with its reply, as `agent.messages` has it. */
  message: AgentMessage | null;
  /** A "Then…" step: the run it follows, its place ("s"/"f" per level), its agent. */
  parent_run?: number | null;
  step?: string | null;
  agent?: string | null;
  /** A triggered run: what happened. */
  event?: TriggerEvent | null;
}

/** A git or GitHub trigger's watcher. */
export interface TriggerWatch {
  state: "ok" | "waiting" | "gh_not_signed_in" | "gh_missing" | "error";
  detail: string | null;
  checked_at: number | null;
  next_at: number | null;
}

/** A recurring or one-off message to a named agent (`schedule.list`, `schedule_upserted`). */
export interface Schedule {
  id: number;
  agent_id: number;
  agent: string | null;
  name: string;
  prompt: string;
  when: When;
  /** IANA name, e.g. "Europe/London". */
  tz: string;
  /** "At 09:00, Monday to Friday", "Once, Tue 21 Oct 2026 at 15:30". */
  reading: string;
  missed: MissedRule;
  missed_effective: "run_once" | "skip";
  conversation: "continue" | "fresh";
  notify: "on_failure" | "always" | "never";
  note_id: number | null;
  note_title: string | null;
  quiet_hours: boolean;
  daily_cap: number | null;
  paused: boolean;
  /** The next slot will be skipped. */
  skip_next: boolean;
  /** Null: a one-off that ran. */
  next_run: number | null;
  last_slot: number | null;
  status: "active" | "paused" | "expired" | "done";
  /** Claude subscription agents' recurring schedules only. */
  expires_at: number | null;
  expires_soon: boolean;
  created_at: number;
  updated_at: number;
  last_run: ScheduleRun | null;
  /** Phase 2 (absent from older backends): fired by an event, not the clock. */
  trigger?: boolean;
  origin?: "schedule" | "webhook";
  then?: Then | null;
  /** A trigger's event waiting for its run (at `next_run`). */
  pending?: TriggerEvent | null;
  watch?: TriggerWatch | null;
  /** A note trigger's note, and whether the human allows triggers on it. */
  note?: { id: number; title: string; triggers: boolean } | null;
  /** A URL trigger's address (its secret: `trigger.secret`). */
  url?: string | null;
}

/** OpenRouter spend today (UTC day) and the daily caps (`ScheduleSettings.spend`). */
export interface SpendInfo {
  key_present: boolean;
  /** Dollars billed to the key today; null before a reading. */
  today: number | null;
  /** The overall daily cap (always on). */
  cap: number;
  /** Spent while no agent of Muster's was working (other use of the key). */
  other: number;
  agents: { id: number; name: string; spent: number; cap: number | null }[];
  checked_at: number | null;
  error: string | null;
  resets_at: number;
}

/** Scheduling settings (`schedule.settings.get` / `.set`, `schedule_settings`). */
export interface ScheduleSettings {
  /** "Pause all schedules". */
  paused: boolean;
  /** "HH:MM". */
  quiet_start: string;
  quiet_end: string;
  /** Unattended (scheduled or triggered) messages per agent per 24 hours. */
  unattended_cap: number;
  /** This machine's zone (read-only). */
  tz: string;
  /** OpenRouter spend and caps (Phase 2 backends). */
  spend?: SpendInfo;
}

/** `schedule.preview`: what a timing means, before saving it. */
export interface SchedulePreview {
  reading: string;
  tz: string;
  /** Up to 3 next runs (unix ms). */
  next: number[];
  /** Each of those falls in quiet hours (and quiet hours apply). */
  quiet: boolean[];
  quiet_hours_default: boolean;
  missed_default: "run_once" | "skip";
  expires: boolean;
  /** A trigger (no times to list). */
  trigger?: boolean;
  /** A note trigger whose note doesn't allow triggers yet. */
  note_triggers_off?: boolean;
  daily_cap_default?: number;
}

export type ServerEvent =
  | { event: "panel_upserted"; data: Panel }
  | { event: "panel_removed"; data: { id: number } }
  | { event: "note_upserted"; data: Note }
  | { event: "note_removed"; data: { id: number } }
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
  | { event: "schedule_upserted"; data: Schedule }
  | { event: "schedule_removed"; data: { id: number; agent_id: number } }
  | { event: "schedule_run"; data: ScheduleRun }
  | { event: "schedule_settings"; data: ScheduleSettings }
  | { event: "updates_changed"; data: UpdateStatus }
  | { event: "prefs_changed"; data: WindowPrefs }
  | { event: "daemon_stopping"; data: { reason: string } }
  // Connection-scoped, only to a client that asked `session.attach` for a `replay` kind.
  | {
    event: "attach_info";
    data: {
      id: SessionId;
      kind: "full" | "screen" | "delta";
      offset: number;
      generation: number | null;
      epoch: string;
    };
  };

/** Preferences every window shares, kept by the backend (`prefs.get` / `prefs.set`). */
/** Prompt-cache lifetimes in minutes (`prefs.cache_ttl`, src/cache.rs). */
export interface CacheTtl {
  claude: number;
  claude_openrouter: number;
  codex: number;
  codex_openrouter: number;
}

export interface WindowPrefs {
  /** Native windows show the system title bar. */
  decorations: boolean;
  /** Shell new shell tabs run; absent: the account's login shell (`hello.login_shell`). */
  shell?: string;
  /** Warn when an agent's prompt cache has likely expired. Missing from older backends. */
  cache_warn?: boolean;
  /** Each CLI/provider pair's prompt-cache lifetime, in minutes. */
  cache_ttl?: CacheTtl;
  /** Raised by every `prefs.set` (missing from the first builds that had prefs). */
  revision?: number;
}

export interface HelloResult {
  panels?: Panel[];
  panel_origin?: string;
  /** Missing from backends without notes. */
  notes?: NoteSummary[];
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
  /** Whether the backend has an OpenRouter key (never the key). Missing: no OpenRouter support. */
  openrouter?: { key_present: boolean };
  /** The backend's own pid (missing from older backends): tells a restarted backend apart. */
  pid?: number;
  /** The backend's build: its content id, as `updates.backend` names a waiting one (missing from
   * older backends). */
  build?: string | null;
  /** The terminal host this backend runs on (null or missing: none, or an older backend). */
  terminal_host?: { instance: string; pid: number } | null;
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
  /** Opened with `notify_on_close: true`: closing it prompts the idle agent (absent from older backends). */
  notify_on_close?: boolean;
  created_at: number;
  updated_at: number;
  closed: boolean;
}

/** Who last changed a note's text. */
export interface NoteAuthor {
  kind: "human" | "agent";
  name?: string | null;
  session_id?: SessionId | null;
}

/** A plain-text note shared by the human and agents (docs/PROTOCOL.md "Notes"). */
export interface Note {
  id: number;
  title: string;
  body: string;
  /** Counts changes to `body`. */
  rev: number;
  created_at: number;
  updated_at: number;
  updated_by: NoteAuthor;
  /** The window showing its tab; null while closed. */
  window_id: WindowId | null;
  /** Shown beside this session when it first appears (`note_open`). */
  beside: SessionId | null;
  /** Raised by every change (text, title, window): the newer record wins. */
  version: number;
  /** "Allow triggers": a schedule's note trigger may fire on its changes (the human's switch). */
  triggers?: boolean;
}

/** A note without its text (`hello.notes`, `note.list`). */
export interface NoteSummary extends Omit<Note, "body"> {
  open: boolean;
  bytes: number;
  lines: number;
  /** The first non-blank line, at most 120 characters. */
  preview: string;
}

/** What the UI knows of a note: its summary, and its text once fetched or sent by an event. */
export type NoteInfo = Omit<NoteSummary, "open"> & { body?: string };

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
