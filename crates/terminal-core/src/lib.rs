//! Application-independent terminal ownership, screen state and replay.
//! No agents, tasks, application database, HTTP server, or UI dependencies belong here.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub mod pty;
pub mod private_fs;
pub type SessionId = u32;
pub type EventSink = Arc<dyn Fn(TerminalEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TerminalEvent {
    Title { id: SessionId, generation: u64, title: Option<String> },
    Exited { id: SessionId, generation: u64, code: i32 },
    Drained { id: SessionId, generation: u64 },
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as i64
}

/// Plain-text view of the terminal for pattern matching.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// Visible rows, trailing whitespace trimmed.
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cols: u16,
    pub rows: u16,
    /// Unix ms of the last PTY output.
    pub last_output_at: i64,
    /// True if a foreground process other than the session's own process owns the TTY
    /// (only meaningful for shells).
    pub foreground_busy: bool,
}

impl ScreenSnapshot {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    pub fn contains(&self, needle: &str) -> bool {
        self.lines.iter().any(|l| l.contains(needle))
    }
}

