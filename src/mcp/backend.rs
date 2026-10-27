//! The slice of the daemon the MCP server needs. `HubBackend` is the real implementation;
//! tests use an in-memory mock so protocol, guardrails and tools run without a live hub.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::agents::{Agent, AgentMessage, Delivery, MsgStatus, Outgoing, Prov, Source};
use crate::hub::{CreateChannel, Hub, ServerEvent, SpawnSession};
use crate::inject::{self, SendMode, SendOutcome};
use crate::model::*;

pub trait Backend: Send + Sync + 'static {
    fn panel_tool(&self, _caller: SessionId, _name: &str, _args: serde_json::Value) -> impl Future<Output = anyhow::Result<serde_json::Value>> + Send {
        async { anyhow::bail!("panels unavailable in this backend") }
    }
    fn note_tool(&self, _caller: SessionId, _name: &str, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("notes unavailable in this backend")
    }
    fn schedule_tool(&self, _caller: SessionId, _name: &str, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("schedules unavailable in this backend")
    }
    /// Persistent application guard state; absent in pure protocol fixtures.
    fn guard_store(&self) -> Option<crate::store::Store> { None }
    fn session_by_token(&self, token: &str) -> Option<SessionId>;
    fn session(&self, id: SessionId) -> Option<SessionInfo>;
    fn sessions(&self) -> Vec<SessionInfo>;
    fn live_session_ids(&self) -> Vec<SessionId>;
    fn channel(&self, id: ChannelId) -> Option<Channel>;
    fn channels(&self) -> Vec<Channel>;
    fn workspace(&self, id: WorkspaceId) -> Option<Workspace>;
    /// Visible screen as text.
    fn screen_text(&self, id: SessionId) -> Option<String>;
    fn recent_output(&self, id: SessionId, lines: usize) -> Option<String>;
    fn subscribe(&self) -> broadcast::Receiver<ServerEvent>;
    fn log_event(
        &self,
        channel_id: Option<ChannelId>,
        session_id: Option<SessionId>,
        kind: EventKind,
        data: serde_json::Value,
    );
    fn create_channel(
        &self,
        req: CreateChannel,
    ) -> impl Future<Output = anyhow::Result<Channel>> + Send;
    fn spawn_session(
        &self,
        req: SpawnSession,
    ) -> impl Future<Output = anyhow::Result<SessionInfo>> + Send;
    /// `source`: the provenance the target takes on when the prompt is submitted.
    fn send(
        &self,
        id: SessionId,
        text: String,
        mode: SendMode,
        from: String,
        source: Source,
    ) -> impl Future<Output = anyhow::Result<SendOutcome>> + Send;
    fn diff(
        &self,
        cwd: String,
        base: Option<String>,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Used to roll back a channel spawn_session created when the spawn itself fails.
    fn archive_channel(
        &self,
        id: ChannelId,
        remove_worktree: bool,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    // ----- agents (src/agents.rs) -----
    fn agents(&self) -> Vec<Agent>;
    /// The caller's provenance (None: human-rooted).
    fn prov(&self, caller: SessionId) -> Option<Prov>;
    /// Messages delivered to the agent at or after `since` (unix ms).
    fn agent_messages_since(&self, agent_id: i64, since: i64) -> usize;
    /// Start the agent if needed, record and send the message (`agents::deliver`).
    fn agent_deliver(&self, out: Outgoing) -> impl Future<Output = anyhow::Result<Delivery>> + Send;
    /// Where an agent message stands: delivered, running, its reply (a late `reply` left for a
    /// waiting ask included).
    fn agent_message_status(&self, msg_id: i64) -> MsgStatus;
    /// Keep a message the agent's policy refused, for the log (`agents::record_refused`).
    fn agent_refused(&self, m: AgentMessage, why: &str);
    /// An ask waits for message `msg_id` until the guard drops: a late `reply` to it goes to
    /// that ask (src/agent_reply.rs).
    fn agent_wait(&self, _msg_id: i64) -> Option<crate::agent_reply::WaitGuard> { None }
    /// The caller is delivering an ask to `agent_id` (its message id comes after): until the
    /// guard drops, a reply waits for that ask too.
    fn agent_asking(&self, _agent_id: i64, _caller: SessionId) -> Option<crate::agent_reply::WaitGuard> { None }
    /// The caller (an agent) answers a message it received, after its turn (`agent_reply::reply`).
    fn agent_reply(&self, _caller: SessionId, _msg_id: i64, _text: String) -> impl Future<Output = anyhow::Result<serde_json::Value>> + Send {
        async { anyhow::bail!("replies unavailable in this backend") }
    }
}

#[derive(Clone)]
pub struct HubBackend(pub Arc<Hub>);

impl Backend for HubBackend {
    async fn panel_tool(&self, caller: SessionId, name: &str, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        crate::panels::tool(&self.0, caller, name, args).await
    }
    fn note_tool(&self, caller: SessionId, name: &str, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        crate::notes::tool(&self.0, caller, name, args)
    }
    fn schedule_tool(&self, caller: SessionId, name: &str, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        crate::schedules::agent::tool(&self.0, caller, name, args)
    }
    fn guard_store(&self) -> Option<crate::store::Store> { Some(self.0.store().clone()) }
    fn session_by_token(&self, token: &str) -> Option<SessionId> {
        self.0.session_by_token(token)
    }
    fn session(&self, id: SessionId) -> Option<SessionInfo> {
        self.0.session(id)
    }
    fn sessions(&self) -> Vec<SessionInfo> {
        self.0.sessions()
    }
    fn live_session_ids(&self) -> Vec<SessionId> {
        self.0.live_session_ids()
    }
    fn channel(&self, id: ChannelId) -> Option<Channel> {
        self.0.channel(id)
    }
    fn channels(&self) -> Vec<Channel> {
        self.0.channels()
    }
    fn workspace(&self, id: WorkspaceId) -> Option<Workspace> {
        self.0.workspace(id)
    }
    fn screen_text(&self, id: SessionId) -> Option<String> {
        self.0.screen(id).map(|s| s.text())
    }
    fn recent_output(&self, id: SessionId, lines: usize) -> Option<String> {
        // The hub's tail includes the blank rows below the cursor, so a small N can come back
        // all whitespace. Over-fetch by a screen height, drop trailing blank lines, keep N.
        let rows = self.0.session(id).map(|s| s.rows as usize).unwrap_or(0);
        let text = self.0.recent_output_text(id, lines + rows)?;
        Some(last_lines(&text, lines))
    }
    fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.0.subscribe()
    }
    fn log_event(
        &self,
        channel_id: Option<ChannelId>,
        session_id: Option<SessionId>,
        kind: EventKind,
        data: serde_json::Value,
    ) {
        self.0.log_event(channel_id, session_id, kind, data);
    }
    async fn create_channel(&self, req: CreateChannel) -> anyhow::Result<Channel> {
        self.0.create_mcp_channel(req).await
    }
    async fn spawn_session(&self, req: SpawnSession) -> anyhow::Result<SessionInfo> {
        self.0.spawn_session(req).await
    }
    async fn send(
        &self,
        id: SessionId,
        text: String,
        mode: SendMode,
        from: String,
        source: Source,
    ) -> anyhow::Result<SendOutcome> {
        inject::send(&self.0, id, text, mode, from, source).await
    }
    async fn diff(&self, cwd: String, base: Option<String>) -> anyhow::Result<String> {
        crate::worktree::diff(Path::new(&cwd), base.as_deref()).await
    }
    async fn archive_channel(&self, id: ChannelId, remove_worktree: bool) -> anyhow::Result<()> {
        self.0.archive_channel(id, remove_worktree).await
    }
    fn agents(&self) -> Vec<Agent> {
        self.0.agents().list()
    }
    fn prov(&self, caller: SessionId) -> Option<Prov> {
        self.0.agents().prov(caller)
    }
    fn agent_messages_since(&self, agent_id: i64, since: i64) -> usize {
        // Unreadable: count as full (refuse) rather than unlimited.
        self.0
            .store()
            .count_agent_messages_since(agent_id, since)
            .unwrap_or(usize::MAX)
    }
    async fn agent_deliver(&self, out: Outgoing) -> anyhow::Result<Delivery> {
        crate::agents::deliver(&self.0, out).await
    }
    fn agent_message_status(&self, msg_id: i64) -> MsgStatus {
        crate::agent_reply::status(&self.0, msg_id)
    }
    fn agent_refused(&self, m: AgentMessage, why: &str) {
        crate::agents::record_refused(&self.0, &m, why)
    }
    fn agent_wait(&self, msg_id: i64) -> Option<crate::agent_reply::WaitGuard> {
        Some(self.0.replies.wait(msg_id))
    }
    fn agent_asking(&self, agent_id: i64, caller: SessionId) -> Option<crate::agent_reply::WaitGuard> {
        Some(self.0.replies.asking(agent_id, Some(caller)))
    }
    async fn agent_reply(&self, caller: SessionId, msg_id: i64, text: String) -> anyhow::Result<serde_json::Value> {
        crate::agent_reply::reply(&self.0, caller, msg_id, text).await
    }
}

/// Last `n` lines of `text` after dropping trailing blank lines.
pub fn last_lines(text: &str, n: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let end = all
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    all[end.saturating_sub(n)..end].join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn last_lines_skips_trailing_blanks() {
        assert_eq!(super::last_lines("a\nb\nc\n\n   \n\n", 2), "b\nc");
        assert_eq!(super::last_lines("a\n", 5), "a");
        assert_eq!(super::last_lines("\n\n", 3), "");
    }
}
