//! Agent adapters: everything CLI-specific (how to launch, read hooks, read the screen,
//! submit prompts, approve, interrupt) lives behind this trait. The rest of the daemon is
//! agent-agnostic.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::{AgentState, Decision, SessionId, SessionKind, StateUpdate};

pub mod claude;
pub mod codex;
pub mod shell;

/// Everything an adapter needs to build a launch command.
#[derive(Debug, Clone)]
pub struct SpawnCtx {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    /// Absolute path of the running muster binary (for `muster hook ...` callbacks).
    pub muster_bin: PathBuf,
    /// e.g. "http://127.0.0.1:7433"
    pub daemon_url: String,
    /// Per-session secret. Authenticates hook callbacks and MCP calls as this session.
    pub session_token: String,
    /// Private per-session directory the adapter may write config files into.
    pub session_dir: PathBuf,
    /// Extra args the user supplied for this session (appended after adapter args).
    pub extra_args: Vec<String>,
    /// Optional initial prompt (adapters may pass it on the command line instead of injecting).
    pub initial_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Added on top of the daemon's base session env.
    pub env: BTreeMap<String, String>,
    /// True if `initial_prompt` was consumed via args; otherwise the daemon injects it once ready.
    pub consumed_initial_prompt: bool,
}

/// Plain-text view of the terminal for pattern matching.
#[derive(Debug, Clone, Default)]
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

pub trait Adapter: Send + Sync {
    fn kind(&self) -> SessionKind;

    /// Launch command for a new session.
    fn command(&self, ctx: &SpawnCtx) -> anyhow::Result<CommandSpec>;

    /// Translate a hook callback (`muster hook <kind> <event>` with its JSON payload) into
    /// a state change. `None` means "no state change".
    fn on_hook(&self, event: &str, payload: &serde_json::Value) -> Option<StateUpdate>;

    /// Fallback detection from the rendered screen. Called after output settles and on a
    /// periodic tick. `None` means "keep current state".
    fn detect_from_screen(&self, screen: &ScreenSnapshot, current: AgentState) -> Option<StateUpdate>;

    /// Can a prompt be typed and submitted right now?
    fn is_ready(&self, state: AgentState) -> bool {
        matches!(state, AgentState::Idle)
    }

    /// Bytes that place `text` into the input box without submitting it.
    /// Default: bracketed paste, which keeps multi-line text intact.
    fn prompt_bytes(&self, text: &str) -> Vec<u8> {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    }

    /// Bytes that submit the input box. Written separately after a short delay.
    fn submit_bytes(&self) -> Vec<u8> {
        b"\r".to_vec()
    }

    /// Bytes that answer a permission prompt currently on screen, or `None` if the screen
    /// doesn't show one this adapter understands.
    fn approve_bytes(&self, decision: Decision, screen: &ScreenSnapshot) -> Option<Vec<u8>>;

    /// Bytes that interrupt the current turn.
    fn interrupt_bytes(&self) -> Vec<u8>;
}

pub fn adapter_for(kind: SessionKind) -> &'static dyn Adapter {
    match kind {
        SessionKind::Claude => &claude::ClaudeAdapter,
        SessionKind::Codex => &codex::CodexAdapter,
        SessionKind::Shell => &shell::ShellAdapter,
    }
}
