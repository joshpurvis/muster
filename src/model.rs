//! Shared data model. Everything that crosses a module boundary or the wire lives here.
//! Wire format is JSON with snake_case fields; enums are snake_case strings.

use serde::{Deserialize, Serialize};

pub type WorkspaceId = i64;
pub type ChannelId = i64;
pub type SessionId = u32;
pub type WindowId = i64;
pub type PanelId = i64;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PanelCsp {
    #[serde(default)]
    pub resource_domains: Vec<String>,
    #[serde(default)]
    pub connect_domains: Vec<String>,
    #[serde(default)]
    pub frame_domains: Vec<String>,
    #[serde(default)]
    pub base_uri_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub id: PanelId,
    pub channel_id: ChannelId,
    pub session_id: SessionId,
    pub window_id: Option<WindowId>,
    pub title: String,
    pub app: String,
    pub csp: PanelCsp,
    pub revision: i64,
    /// The revision whose update last carried `data` (0 in records from before it existed).
    /// A web preview reloads its page on a data change, not on a title-only update.
    #[serde(default)]
    pub data_revision: i64,
    pub html_revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed: bool,
    pub context: Option<serde_json::Value>,
    pub dropped_events: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelEvent {
    /// The stored event's id (0 for the synthetic `overflow` notice). It stays the same when an
    /// undelivered batch returns to its panel, and `panel_event` fetches an event by it.
    #[serde(default)]
    pub id: i64,
    pub panel_id: PanelId,
    pub kind: String,
    pub payload: serde_json::Value,
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Claude,
    Codex,
    Shell,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Claude => "claude",
            SessionKind::Codex => "codex",
            SessionKind::Shell => "shell",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(SessionKind::Claude),
            "codex" => Some(SessionKind::Codex),
            "shell" => Some(SessionKind::Shell),
            _ => None,
        }
    }
}

/// Normalized agent state. Shells use `Idle` (prompt) / `Working` (foreground job running).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Working,
    NeedsApproval,
    AskingQuestion,
    Idle,
    Error,
    Stuck,
    /// Process has exited (or the daemon restarted and the PTY is gone).
    Exited,
}

impl AgentState {
    /// States that put an item in the decision inbox.
    pub fn needs_human(self) -> bool {
        matches!(
            self,
            AgentState::NeedsApproval | AgentState::AskingQuestion | AgentState::Error | AgentState::Stuck
        )
    }
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Starting => "starting",
            AgentState::Working => "working",
            AgentState::NeedsApproval => "needs_approval",
            AgentState::AskingQuestion => "asking_question",
            AgentState::Idle => "idle",
            AgentState::Error => "error",
            AgentState::Stuck => "stuck",
            AgentState::Exited => "exited",
        }
    }
}

/// Where a state change came from. Hooks beat screen parsing: a screen-derived update
/// never overrides a hook-derived state younger than a short grace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    Hook,
    Screen,
    Process,
    Timer,
    Daemon,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateUpdate {
    pub state: AgentState,
    /// Short human text, e.g. "Bash: cargo test" or the question being asked.
    pub detail: Option<String>,
}

impl StateUpdate {
    pub fn new(state: AgentState) -> Self {
        Self { state, detail: None }
    }
    pub fn with(state: AgentState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Absolute path to the repo or folder.
    pub path: String,
    pub is_git: bool,
    pub created_at: i64,
    /// Default for new Claude sessions only. Existing sessions retain their selection.
    #[serde(default)]
    pub account_profile_id: Option<crate::account_profiles::ProfileId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Working directory for sessions: the worktree path, or the workspace path when no worktree.
    pub cwd: String,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    /// True when `cwd` is a worktree muster created (and may remove on archive).
    pub owns_worktree: bool,
    /// First port of this channel's reserved block (exported as PORT / MUSTER_PORT_BASE).
    pub port_base: u16,
    pub archived: bool,
    /// Opaque JSON owned by the frontend (pane/split tree). Daemon stores it verbatim.
    pub layout: Option<serde_json::Value>,
    pub created_at: i64,
    /// Unix ms of the last event the human has seen (for unread badges / catch me up).
    pub last_read_at: i64,
    pub sort_order: i64,
    /// What agents started in this channel may do. Set by the human only (UI / RPC), applied
    /// when a session's process starts.
    #[serde(default)]
    pub permissions: Permissions,
    /// Client window this channel is a tab in (None: adopted by the next `window.claim`).
    #[serde(default)]
    pub window_id: Option<WindowId>,
    /// Created without a name: the daemon derives `name` until the human renames it.
    #[serde(default)]
    pub auto_named: bool,
    /// Name derived from the first prompt sent to the channel (auto naming input). Internal.
    #[serde(skip)]
    pub first_prompt: Option<String>,
}

/// A client window. Its tabs are the sessions with this `window_id` (arranged by `layout`);
/// `channel_ids` are the tasks (non-archived channels) whose default window it is, in
/// `sort_order`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    /// Default: the id as a string.
    pub name: String,
    /// Tasks whose window this is, in order.
    pub channel_ids: Vec<ChannelId>,
    /// A client connection holds (shows) this window.
    pub open: bool,
    /// Unix ms of the last `window.focused`.
    pub last_focused_at: i64,
    pub created_at: i64,
    /// Opaque JSON owned by the frontend (the split tree of groups). Stored verbatim.
    #[serde(default)]
    pub layout: Option<serde_json::Value>,
}

/// Codex `--sandbox` policy for a channel's Codex sessions. Ordered least to most permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexSandbox {
    /// `--sandbox read-only`
    ReadOnly,
    /// `--sandbox workspace-write`, no network (Codex's own default).
    #[default]
    WorkspaceWrite,
    /// `--sandbox workspace-write` plus `sandbox_workspace_write.network_access=true`.
    WorkspaceWriteNetwork,
    /// `--sandbox danger-full-access`: no sandbox at all.
    FullAccess,
}

/// Claude Code `--permission-mode` for a channel's Claude sessions. Ordered least to most
/// permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeMode {
    /// `plan`
    Plan,
    /// `default` (asks before edits and commands; "Manual" in the CLI).
    #[default]
    Default,
    /// `acceptEdits`
    AcceptEdits,
    /// `bypassPermissions`: no permission prompts.
    Bypass,
}

/// Per-channel agent permission profile. Defaults match the CLIs' own defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub codex_sandbox: CodexSandbox,
    #[serde(default)]
    pub claude_mode: ClaudeMode,
}

/// A complete profile as a client sends it (both fields required, so a partial update can't
/// silently reset the other half to its default).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsInput {
    pub codex_sandbox: CodexSandbox,
    pub claude_mode: ClaudeMode,
}

impl From<PermissionsInput> for Permissions {
    fn from(p: PermissionsInput) -> Self {
        Permissions {
            codex_sandbox: p.codex_sandbox,
            claude_mode: p.claude_mode,
        }
    }
}

impl Permissions {
    /// Would an agent of `kind` get more in a channel with `self` than in one with `other`?
    /// (Shells are not governed by the profile.)
    pub fn exceeds_for(&self, other: &Permissions, kind: SessionKind) -> bool {
        match kind {
            SessionKind::Codex => self.codex_sandbox > other.codex_sandbox,
            SessionKind::Claude => self.claude_mode > other.claude_mode,
            SessionKind::Shell => false,
        }
    }

    /// More permissive than the defaults in any respect.
    #[allow(dead_code)] // used by tests and documented for clients
    pub fn is_elevated(&self) -> bool {
        matches!(
            self.codex_sandbox,
            CodexSandbox::WorkspaceWriteNetwork | CodexSandbox::FullAccess
        ) || matches!(self.claude_mode, ClaudeMode::AcceptEdits | ClaudeMode::Bypass)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub channel_id: ChannelId,
    pub kind: SessionKind,
    /// Immutable launch selection. None preserves ordinary inherited Claude behavior.
    #[serde(default)]
    pub account_profile_id: Option<crate::account_profiles::ProfileId>,
    pub name: String,
    pub cwd: String,
    pub state: AgentState,
    pub state_detail: Option<String>,
    /// Unix ms when `state` last changed.
    pub state_since: i64,
    /// Window title set by the program via OSC 0/2, if any.
    pub title: Option<String>,
    /// Session that spawned this one through MCP, if any.
    pub parent_id: Option<SessionId>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    /// Messages waiting in the injection queue.
    pub queued: u32,
    /// Built-in subagents (Claude Code's Agent/Task tool) the agent is running right now, from
    /// its hooks. Runtime only: not persisted, 0 after a restart.
    #[serde(default)]
    pub subagents: u32,
    pub created_at: i64,
    /// Client window this session is a tab in (None: adopted by the next `window.claim`).
    #[serde(default)]
    pub window_id: Option<WindowId>,
    /// Changes whenever the state or its detail changes, and when a prompt is answered: an
    /// approval carrying the `state_seq` the caller saw never answers a different prompt.
    /// Runtime only (unique within one daemon run).
    #[serde(default)]
    pub state_seq: u64,
    /// Who set the terminal size last: "local" (a client-token client) or "device:<id>".
    /// Runtime only: None after a daemon restart.
    #[serde(default)]
    pub size_owner: Option<String>,
    /// Typing into the current run is paused: earlier input may not have reached it
    /// (`terminal::InputInterrupted`). Cleared by `session.continue_input` or a new run.
    /// Not a session column; the interrupted runs are saved separately.
    #[serde(default)]
    pub input_interrupted: bool,
    /// The folder the process is in now (`/proc/<pid>/cwd`): follows a shell's `cd` and
    /// Claude's `/cd`, while `cwd` stays the folder it was launched in (restarts use that).
    /// Runtime only: sampled every second or so, None until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_cwd: Option<String>,
}

/// Timeline / event-log entry kinds. Terminal output is NOT stored here (see session logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Human wrote a note in the channel. data: {text}
    Note,
    /// Human (or MCP) sent a prompt to an agent. data: {text, session_id, from, queued}
    Prompt,
    /// data: {session_id, from, to, detail}
    StateChange,
    /// data: {session_id, kind, name, parent_id?}
    SessionStarted,
    /// data: {session_id, exit_code}
    SessionExited,
    /// Hook callback. data: {session_id, event, summary: {tool_name?, notification_type?, …}}
    /// (identifying fields only, never the full payload)
    Hook,
    /// An agent called an MCP tool. data: {session_id, tool, args, ok}
    McpCall,
    /// Agent posted to the channel via MCP. data: {session_id, text}
    AgentPost,
    /// data: {session_id, decision}
    Approval,
    /// Generic daemon notice. data: {text, level}
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub channel_id: Option<ChannelId>,
    pub session_id: Option<SessionId>,
    pub kind: EventKind,
    pub data: serde_json::Value,
    /// Unix ms.
    pub ts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    /// Approve and don't ask again for this kind of action (if the CLI offers it).
    ApproveAlways,
    Deny,
}

/// One entry in the decision inbox. Derived from session state; not persisted separately.
#[allow(dead_code)] // wire type documented for clients; not built by the daemon yet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub session_id: SessionId,
    pub channel_id: ChannelId,
    pub state: AgentState,
    pub detail: Option<String>,
    pub since: i64,
}

/// Where `UiPrefs` are kept (`runtime_state`).
pub const UI_PREFS_KEY: &str = "ui-prefs";

/// Preferences every window shares, kept by the backend so a new native window knows them
/// before it is built (`prefs.get` / `prefs.set`). Per-viewer prefs stay in localStorage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    /// Native windows show the system title bar.
    pub decorations: bool,
    /// Absolute path of the shell new shell sessions run. None: the account's login shell
    /// (`adapters::shell::user_shell`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Raised by every `prefs.set`, so a client can tell a stale reply from a newer change.
    pub revision: u64,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self { decorations: true, shell: None, revision: 0 }
    }
}

/// `prefs.set`: only the fields given change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPrefsChange {
    pub decorations: Option<bool>,
    /// A path or a name on `PATH`; "" goes back to the login shell.
    pub shell: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
