//! Agent adapters: everything CLI-specific (how to launch, read hooks, read the screen,
//! submit prompts, approve, interrupt) lives behind this trait. The rest of the daemon is
//! agent-agnostic.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::{AgentState, Decision, Permissions, SessionId, SessionKind, StateUpdate};

pub mod claude;
pub mod codex;
pub mod shell;
pub mod tui;

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
    /// The channel's permission profile (chosen by the human), mapped to CLI flags.
    pub permissions: Permissions,
}

/// Refuse extra args that would change what an agent may do: the channel's permission profile
/// (set by the human, applied as flags before the extra args) must stay what decides it, and the
/// CLIs let a later flag win. Shells take anything.
pub fn check_extra_args(kind: SessionKind, args: &[String]) -> anyhow::Result<()> {
    let (flags, config_flags): (&[&str], &[&str]) = match kind {
        SessionKind::Claude => (
            &["--permission-mode", "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions"],
            &[],
        ),
        SessionKind::Codex => (
            &[
                "--sandbox",
                "-s",
                "--full-auto",
                "--dangerously-bypass-approvals-and-sandbox",
                "--yolo",
                "--profile",
                "-p",
            ],
            &["-c", "--config"],
        ),
        SessionKind::Shell => return Ok(()),
    };
    let name = |a: &str| a.split_once('=').map_or(a, |(n, _)| n).to_string();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let n = name(a);
        if flags.contains(&n.as_str()) {
            anyhow::bail!("{n} changes the agent's permissions; set the tab's permissions instead");
        }
        if config_flags.contains(&n.as_str()) {
            // `-c key=value`, `--config key=value`, `--config=key=value`.
            let kv = match a.split_once('=') {
                Some((_, rest)) if n == "--config" || n == "-c" => Some(rest.to_string()),
                _ => it.peek().map(|v| v.to_string()),
            };
            if kv.as_deref().is_some_and(|kv| kv.trim_start().starts_with("sandbox")) {
                anyhow::bail!("{n} sandbox settings change the agent's permissions; set the tab's permissions instead");
            }
        }
    }
    Ok(())
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

    /// Text currently typed into the agent's input box, if this adapter can read it from
    /// the screen. `Some("")` = input box visible and empty; `None` = unknown.
    /// Used by the injector to verify submission and to avoid leaving half-typed input.
    fn composer_text(&self, _screen: &ScreenSnapshot) -> Option<String> {
        None
    }

    /// Bytes that clear a non-empty input box without doing anything else, if known.
    /// Only sent when `composer_text` reported non-empty text.
    fn clear_input_bytes(&self) -> Option<Vec<u8>> {
        None
    }
}

pub fn adapter_for(kind: SessionKind) -> &'static dyn Adapter {
    match kind {
        SessionKind::Claude => &claude::ClaudeAdapter,
        SessionKind::Codex => &codex::CodexAdapter,
        SessionKind::Shell => &shell::ShellAdapter,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(kind: SessionKind, args: &[&str]) -> bool {
        check_extra_args(kind, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).is_ok()
    }

    #[test]
    fn extra_args_cannot_change_permissions() {
        use SessionKind::*;
        assert!(check(Codex, &["-m", "gpt-6-sol", "-c", "model_reasoning_effort=xhigh"]));
        assert!(check(Claude, &["--model", "haiku"]));
        for bad in [
            &["--sandbox", "danger-full-access"][..],
            &["-s", "read-only"],
            &["--sandbox=danger-full-access"],
            &["--full-auto"],
            &["--yolo"],
            &["--dangerously-bypass-approvals-and-sandbox"],
            &["--profile", "wide"],
            &["-c", "sandbox_workspace_write.network_access=true"],
            &["--config", "sandbox_mode=danger-full-access"],
            &["--config=sandbox_mode=danger-full-access"],
        ] {
            assert!(!check(Codex, bad), "{bad:?}");
        }
        for bad in [
            &["--permission-mode", "bypassPermissions"][..],
            &["--permission-mode=acceptEdits"],
            &["--dangerously-skip-permissions"],
        ] {
            assert!(!check(Claude, bad), "{bad:?}");
        }
        assert!(check(Shell, &["--sandbox", "-c", "sandbox"]), "shells take anything");
    }
}
