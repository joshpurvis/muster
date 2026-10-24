//! Application-independent terminal ownership, screen state and replay.
//! No agents, tasks, application database, HTTP server, or UI dependencies belong here.
use std::sync::Arc;
pub use muster_terminal_protocol::{ScreenSnapshot, SessionId, TerminalEvent};
pub mod pty;
pub mod replay;
pub mod private_fs;
pub type EventSink = Arc<dyn Fn(TerminalEvent) + Send + Sync>;
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as i64
}
