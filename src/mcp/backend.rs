//! The slice of the daemon the MCP server needs. `HubBackend` is the real implementation;
//! tests use an in-memory mock so protocol, guardrails and tools run without a live hub.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::hub::{CreateChannel, Hub, ServerEvent, SpawnSession};
use crate::inject::{self, SendMode, SendOutcome};
use crate::model::*;

pub trait Backend: Send + Sync + 'static {
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
    fn send(
        &self,
        id: SessionId,
        text: String,
        mode: SendMode,
        from: String,
    ) -> impl Future<Output = anyhow::Result<SendOutcome>> + Send;
    fn diff(
        &self,
        cwd: String,
        base: Option<String>,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Used to roll back a channel spawn_agent created when the spawn itself fails.
    fn archive_channel(
        &self,
        id: ChannelId,
        remove_worktree: bool,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Clone)]
pub struct HubBackend(pub Arc<Hub>);

impl Backend for HubBackend {
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
        self.0.create_channel(req).await
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
    ) -> anyhow::Result<SendOutcome> {
        inject::send(&self.0, id, text, mode, from).await
    }
    async fn diff(&self, cwd: String, base: Option<String>) -> anyhow::Result<String> {
        crate::worktree::diff(Path::new(&cwd), base.as_deref()).await
    }
    async fn archive_channel(&self, id: ChannelId, remove_worktree: bool) -> anyhow::Result<()> {
        self.0.archive_channel(id, remove_worktree).await
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
