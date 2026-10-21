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

/// How an allowed extra-arg flag takes its value.
#[derive(Clone, Copy, PartialEq)]
enum Val {
    /// No value.
    None,
    /// Exactly one (`--flag v`, `--flag=v`, `-fv`).
    One,
    /// An optional one (`--flag`, `--flag=v`, or `--flag v` when `v` doesn't start with `-`).
    Opt,
    /// One or more (`-i a.png b.png`).
    Many,
}

/// Extra args users may pass to agent CLIs. An allowlist: the channel's permission profile (set
/// by the human, applied as flags before the extra args) must stay what decides what an agent
/// may do, and the CLIs let a later flag win, so anything that could widen it (sandbox,
/// approval policy, allowed tools, extra dirs, settings, config profiles) is refused. Shells
/// take anything.
const CLAUDE_ARGS: &[(&str, Option<char>, Val)] = &[
    ("--model", None, Val::One),
    ("--effort", None, Val::One),
    ("--fallback-model", None, Val::One),
    ("--append-system-prompt", None, Val::One),
    ("--continue", Some('c'), Val::None),
    ("--resume", Some('r'), Val::Opt),
    ("--verbose", None, Val::None),
    ("--debug", Some('d'), Val::Opt),
];
const CODEX_ARGS: &[(&str, Option<char>, Val)] = &[
    ("--model", Some('m'), Val::One),
    ("--config", Some('c'), Val::One),
    ("--image", Some('i'), Val::Many),
    ("--no-alt-screen", None, Val::None),
];
/// `codex -c key=value` keys that don't touch permissions.
const CODEX_CONFIG_KEYS: &[&str] = &[
    "model",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_verbosity",
    "hide_agent_reasoning",
    "show_raw_agent_reasoning",
];

pub fn check_extra_args(kind: SessionKind, args: &[String]) -> anyhow::Result<()> {
    parse_extra_args(kind, args).map(|_| ())
}

/// The allowlisted flags in `args`, each as its long name with its values, exactly as
/// `check_extra_args` reads them (`--flag v`, `--flag=v`, `-fv`, `-f v`). Shells: none.
pub fn parse_extra_args<'a>(kind: SessionKind, args: &'a [String]) -> anyhow::Result<Vec<(&'static str, Vec<&'a str>)>> {
    let table = match kind {
        SessionKind::Claude => CLAUDE_ARGS,
        SessionKind::Codex => CODEX_ARGS,
        SessionKind::Shell => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    let allowed = || {
        table
            .iter()
            .map(|(l, s, _)| s.map_or(l.to_string(), |c| format!("-{c}/{l}")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let refuse = |a: &str| {
        anyhow::anyhow!(
            "{a} is not allowed as an agent argument (allowed: {}). Permissions come from the tab's permission profile",
            allowed()
        )
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        // Which flag, and a value attached to it (`--flag=v`, `-fv`).
        let (entry, attached) = if let Some(long) = a.strip_prefix("--") {
            let (name, v) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v)));
            (table.iter().find(|(l, _, _)| l[2..] == *name), v)
        } else if let Some(short) = a.strip_prefix('-').filter(|s| !s.is_empty()) {
            let c = short.chars().next().unwrap();
            let rest = &short[c.len_utf8()..];
            (table.iter().find(|(_, s, _)| *s == Some(c)), (!rest.is_empty()).then_some(rest))
        } else {
            (None, None)
        };
        let Some(&(long, _, val)) = entry else {
            return Err(refuse(a));
        };
        let mut values: Vec<&str> = attached.into_iter().collect();
        match val {
            Val::None if !values.is_empty() => return Err(refuse(a)),
            Val::None => {}
            Val::One if values.is_empty() => {
                let v = args.get(i).ok_or_else(|| anyhow::anyhow!("{a} needs a value"))?;
                values.push(v);
                i += 1;
            }
            Val::One => {}
            Val::Opt | Val::Many => {
                while (values.is_empty() || val == Val::Many)
                    && let Some(v) = args.get(i).filter(|v| !v.starts_with('-'))
                {
                    values.push(v);
                    i += 1;
                    if val == Val::Opt {
                        break;
                    }
                }
            }
        }
        if kind == SessionKind::Codex && long == "--config" {
            for kv in &values {
                let key = kv.split_once('=').map_or(*kv, |(k, _)| k).trim();
                if !CODEX_CONFIG_KEYS.contains(&key) {
                    anyhow::bail!(
                        "-c {key} is not allowed as an agent argument (allowed keys: {}). Permissions come from the tab's permission profile",
                        CODEX_CONFIG_KEYS.join(", ")
                    );
                }
            }
        }
        out.push((long, values));
    }
    Ok(out)
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

pub use muster_terminal_core::ScreenSnapshot;

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
        for ok in [
            &["-m", "gpt-6-sol", "-c", "model_reasoning_effort=xhigh"][..],
            &["--model=gpt-6-sol", "-cmodel_reasoning_effort=high", "--config", "model_verbosity=low"],
            &["-mgpt-6-luna"],
            &["-i", "a.png", "b.png", "--no-alt-screen"],
        ] {
            assert!(check(Codex, ok), "{ok:?}");
        }
        for ok in [
            &["--model", "haiku"][..],
            &["--model=sonnet", "--effort", "high", "--fallback-model", "haiku"],
            &["-c"],
            &["-r", "abc123", "--verbose"],
            &["--resume"],
            &["-d", "--append-system-prompt", "be brief"],
        ] {
            assert!(check(Claude, ok), "{ok:?}");
        }
        for bad in [
            &["--sandbox", "danger-full-access"][..],
            &["-s", "read-only"],
            &["-sdanger-full-access"],
            &["--sandbox=danger-full-access"],
            &["--full-auto"],
            &["--yolo"],
            &["--dangerously-bypass-approvals-and-sandbox"],
            &["--profile", "wide"],
            &["-pwide"],
            &["-a", "never"],
            &["--ask-for-approval", "never"],
            &["--add-dir", "/"],
            &["-c", "sandbox_workspace_write.network_access=true"],
            &["-csandbox_mode=danger-full-access"],
            &["--config=sandbox_mode=danger-full-access"],
            &["-c", "approval_policy=never"],
            &["-c", "profiles.loose.sandbox_mode=danger-full-access"],
            &["--model"],
            &["stray-positional"],
        ] {
            assert!(!check(Codex, bad), "{bad:?}");
        }
        for bad in [
            &["--permission-mode", "bypassPermissions"][..],
            &["--permission-mode=acceptEdits"],
            &["--dangerously-skip-permissions"],
            &["--allowedTools", "Bash"],
            &["--allowed-tools", "Bash"],
            &["--add-dir", "/"],
            &["--settings", "/tmp/x.json"],
            &["--mcp-config", "x.json"],
            &["-cx"],
        ] {
            assert!(!check(Claude, bad), "{bad:?}");
        }
        assert!(check(Shell, &["--sandbox", "-c", "sandbox"]), "shells take anything");
    }
}
