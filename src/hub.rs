//! The Hub is the daemon's single source of truth: workspaces, channels, live sessions,
//! the event log, and the broadcast bus that clients listen to.
//!
//! CONTRACT: the public method signatures below are used by `agents/*`, `detect.rs`,
//! `inject.rs`, `notify.rs`, `mcp.rs` and `rpc.rs`. The core implementer owns the bodies and
//! private fields; other modules must only use this public surface. Add methods freely;
//! don't change existing signatures without updating every caller.
//!
//! Locking rule: never hold a Hub lock across an `.await` or while calling into another
//! module's code.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agents::ScreenSnapshot;
use crate::model::*;

/// Pushed to every connected client (and to in-daemon listeners such as the injector,
/// detector and notifier). Wire form: `{"event": "<snake_case variant>", "data": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ServerEvent {
    WorkspaceUpserted(Workspace),
    WorkspaceRemoved { id: WorkspaceId },
    ChannelUpserted(Channel),
    ChannelRemoved { id: ChannelId },
    /// Sent on create and on ANY change (state, title, queued count, exit, resize).
    SessionUpserted(SessionInfo),
    SessionRemoved { id: SessionId },
    /// A new timeline/event-log entry.
    Event(Event),
    /// Transient toast for the UI. level: "info" | "warn" | "error"
    Notice { level: String, text: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannel {
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Create a git worktree + branch `muster/<slug>` for this channel. Ignored for non-git
    /// workspaces (cwd is then the workspace path).
    #[serde(default = "yes")]
    pub worktree: bool,
    /// Branch/commit to base the worktree on. Default: the workspace's current HEAD.
    pub base: Option<String>,
    /// Start an agent right away (usual case). None = empty channel.
    pub agent: Option<SessionKind>,
    /// Initial prompt for that agent.
    pub prompt: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnSession {
    pub channel_id: ChannelId,
    pub kind: SessionKind,
    pub name: Option<String>,
    /// Defaults to the channel cwd.
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Set when an agent spawns another agent through MCP.
    #[serde(default)]
    pub parent_id: Option<SessionId>,
}

fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}

pub struct Hub {
    // Private state is owned by the core implementer.
    _private: (),
}

impl Hub {
    /// Base URL of this daemon, e.g. "http://127.0.0.1:7433".
    pub fn daemon_url(&self) -> String {
        todo!()
    }

    // ----- event bus -----

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        todo!()
    }

    pub fn emit(&self, _ev: ServerEvent) {
        todo!()
    }

    /// Persist an event-log entry and broadcast it as `ServerEvent::Event`.
    pub fn log_event(
        &self,
        _channel_id: Option<ChannelId>,
        _session_id: Option<SessionId>,
        _kind: EventKind,
        _data: serde_json::Value,
    ) -> Event {
        todo!()
    }

    /// Timeline for a channel, newest last. `before` = event id for paging back.
    pub fn timeline(&self, _channel_id: ChannelId, _before: Option<i64>, _limit: usize) -> Vec<Event> {
        todo!()
    }

    // ----- queries -----

    pub fn workspaces(&self) -> Vec<Workspace> {
        todo!()
    }
    pub fn channels(&self) -> Vec<Channel> {
        todo!()
    }
    pub fn channel(&self, _id: ChannelId) -> Option<Channel> {
        todo!()
    }
    pub fn workspace(&self, _id: WorkspaceId) -> Option<Workspace> {
        todo!()
    }
    /// All sessions, live and exited (exited ones remain until removed).
    pub fn sessions(&self) -> Vec<SessionInfo> {
        todo!()
    }
    pub fn session(&self, _id: SessionId) -> Option<SessionInfo> {
        todo!()
    }
    /// Resolve a per-session token (hooks, MCP) to its session.
    pub fn session_by_token(&self, _token: &str) -> Option<SessionId> {
        todo!()
    }
    /// Live (running) session ids.
    pub fn live_session_ids(&self) -> Vec<SessionId> {
        todo!()
    }

    // ----- session I/O -----

    /// Raw bytes to the PTY.
    pub fn write_input(&self, _id: SessionId, _bytes: &[u8]) -> anyhow::Result<()> {
        todo!()
    }

    /// Current screen as text, from the headless VT.
    pub fn screen(&self, _id: SessionId) -> Option<ScreenSnapshot> {
        todo!()
    }

    /// Last `lines` lines of output as plain text (scrollback + screen, ANSI stripped).
    pub fn recent_output_text(&self, _id: SessionId, _lines: usize) -> Option<String> {
        todo!()
    }

    /// Unix ms of the last PTY output for a session (0 if none).
    pub fn last_output_at(&self, _id: SessionId) -> i64 {
        todo!()
    }

    /// Increments on every PTY output chunk. Lets pollers skip unchanged sessions.
    pub fn output_seq(&self, _id: SessionId) -> u64 {
        todo!()
    }

    /// Apply a state change with precedence rules:
    /// - `Exited` is terminal.
    /// - A `Screen` update doesn't override a `Hook` update less than 1.5s old, unless it
    ///   reports NeedsApproval/AskingQuestion (prompts are visible before hooks may fire).
    /// - No-op if state and detail are unchanged.
    /// On change: updates SessionInfo, broadcasts SessionUpserted, logs a StateChange event.
    /// Returns true if the state changed.
    pub fn set_state(&self, _id: SessionId, _update: StateUpdate, _source: StateSource) -> bool {
        todo!()
    }

    /// Update the injection-queue length shown in the UI.
    pub fn set_queued(&self, _id: SessionId, _queued: u32) {
        todo!()
    }

    // ----- mutations -----

    pub async fn add_workspace(self: &Arc<Self>, _path: String, _name: Option<String>) -> anyhow::Result<Workspace> {
        todo!()
    }

    pub async fn create_channel(self: &Arc<Self>, _req: CreateChannel) -> anyhow::Result<Channel> {
        todo!()
    }

    pub async fn spawn_session(self: &Arc<Self>, _req: SpawnSession) -> anyhow::Result<SessionInfo> {
        todo!()
    }

    /// SIGHUP the process group, then SIGKILL after a grace period.
    pub fn kill_session(&self, _id: SessionId) -> anyhow::Result<()> {
        todo!()
    }

    /// Handle a hook callback authenticated by session token. Routes to the adapter's
    /// `on_hook`, logs a Hook event, applies the resulting state.
    pub fn handle_hook(&self, _token: &str, _event: &str, _payload: serde_json::Value) -> anyhow::Result<()> {
        todo!()
    }
}
