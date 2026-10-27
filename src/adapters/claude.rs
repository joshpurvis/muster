//! Claude Code adapter (verified against `claude` v2.1.280).
//!
//! Launch: `claude --mcp-config <session_dir>/mcp.json --settings <session_dir>/settings.json
//! --permission-mode <mode> [extra args] [-- <initial prompt>]`. The mode comes from the
//! channel's permission profile (`permission_mode`).
//! - `settings.json` adds command hooks for this session only (merged with the user's own
//!   settings): every event runs `<muster_bin> hook claude <Event>`, which reads the hook's JSON
//!   payload on stdin and POSTs it to the daemon (see hookcmd.rs).
//! - `mcp.json` registers the daemon's MCP server as `muster` (HTTP transport, bearer session
//!   token). `--strict-mcp-config` is NOT used, so the user's own servers keep working.
//!
//! Screen facts (captured from real sessions, see fixtures/claude_*.txt):
//! - Input box: a `❯` row between two `────` rules. Working: a spinner row right above the box
//!   such as `✽ Misting… (1s · thinking)`; finished turns leave `✻ Worked for 2s · done`.
//! - Permission: `Do you want to proceed?` / `Do you want to create b.txt?` + numbered options
//!   (`1. Yes`, `2. Yes, and always allow …`, `3. No`); typing the digit answers immediately.
//! - Folder trust dialog on fresh worktrees; its default option is "No, exit", so muster never
//!   answers it: it's reported as AskingQuestion for the human.
//! - Esc interrupts a turn; if nothing was produced yet, Claude puts the prompt back in the
//!   input box. Ctrl-C clears a non-empty input box (a second Ctrl-C within ~2s would exit).

use std::path::Path;

use serde_json::{Value, json};

use crate::adapters::tui::{self, clip};
use crate::adapters::*;
use crate::model::*;

pub struct ClaudeAdapter;

/// Per-session MCP tool-call timeout (ms): above muster's 1800s wait_for_state maximum.
pub const MCP_TOOL_TIMEOUT_MS: u64 = 1_900_000;

/// Hook events registered for each session (Claude Code v2.1.280 names).
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
    "SubagentStop",
    "PreCompact",
    "SessionEnd",
];

/// Quote for a POSIX shell (Claude runs hook commands through a shell).
pub(crate) fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub(crate) fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts
        .open(path)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

pub fn settings_json(muster_bin: &Path) -> Value {
    let bin = sh_quote(&muster_bin.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for ev in HOOK_EVENTS {
        hooks.insert(
            ev.to_string(),
            json!([{ "hooks": [{
                "type": "command",
                "command": format!("{bin} hook claude {ev}"),
                "timeout": 10
            }]}]),
        );
    }
    // Muster's own MCP tools are allowed for this session: under `auto` mode every call would
    // otherwise wait on Claude Code's safety classifier (1.5-4 s each). Muster's guardrails
    // (spawn limits, no approving prompts) still apply on the daemon side.
    json!({ "hooks": hooks, "permissions": { "allow": [MUSTER_TOOLS_RULE] } })
}

/// Permission rule matching every tool of the `muster` MCP server.
pub const MUSTER_TOOLS_RULE: &str = "mcp__muster";

/// `--permission-mode` value for the channel's profile. Always passed explicitly, so the channel
/// profile (not a `defaultMode` in the user's settings) decides how the session starts.
/// `--permission-mode` value for a channel profile. `Default` passes no flag, so the user's
/// own Claude Code setting (e.g. `permissions.defaultMode`) applies.
pub fn permission_mode(mode: ClaudeMode) -> Option<&'static str> {
    match mode {
        ClaudeMode::Default => None,
        ClaudeMode::AcceptEdits => Some("acceptEdits"),
        ClaudeMode::Plan => Some("plan"),
        ClaudeMode::Bypass => Some("bypassPermissions"),
    }
}

pub fn mcp_json(daemon_url: &str, token: &str) -> Value {
    json!({
        "mcpServers": {
            "muster": {
                "type": "http",
                "url": format!("{}/mcp", daemon_url.trim_end_matches('/')),
                "headers": { "Authorization": format!("Bearer {token}") }
            }
        }
    })
}

/// "Bash: cargo test", "Edit: src/main.rs", ...
pub(crate) fn tool_detail(payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name")?.as_str()?;
    let input = payload.get("tool_input");
    let get = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str());
    let arg = get("command")
        .or_else(|| get("file_path"))
        .or_else(|| get("path"))
        .or_else(|| get("notebook_path"))
        .or_else(|| get("pattern"))
        .or_else(|| get("url"))
        .or_else(|| get("query"))
        .or_else(|| get("description"))
        .or_else(|| get("prompt"));
    Some(match arg {
        Some(a) => clip(&format!("{tool}: {a}"), 160),
        None => tool.to_string(),
    })
}

/// First question text of an AskUserQuestion tool_input.
fn question_detail(payload: &Value) -> Option<String> {
    let q = payload
        .get("tool_input")?
        .get("questions")?
        .as_array()?
        .first()?
        .get("question")?
        .as_str()?;
    Some(clip(q, 200))
}

fn first_line(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(clip(s.lines().next().unwrap_or(s), max))
}

// ----- screen parsing -----

/// Index of the input-box row (`❯ …` directly below a rule), searching from the bottom.
fn input_row(lines: &[String]) -> Option<usize> {
    (1..lines.len())
        .rev()
        .find(|&i| lines[i].starts_with('❯') && tui::is_rule(&lines[i - 1]))
}

fn is_spinner_row(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    let Some(c) = chars.next() else { return false };
    if c.is_alphanumeric() || c == '❯' || c == '⎿' || c == '●' {
        return false;
    }
    if chars.next() != Some(' ') {
        return false;
    }
    let rest = chars.as_str();
    // "✽ Misting… (1s · thinking)", "* Kerfuffling…". Finished rows read "✻ Worked for 2s · done".
    let head: String = rest.chars().take(40).collect();
    head.contains('…') && rest.chars().next().is_some_and(|c| c.is_alphabetic())
}

fn is_working(lines: &[String], box_row: Option<usize>) -> bool {
    let end = box_row.unwrap_or(lines.len());
    // The spinner sits just above the input box (a tip line may follow it).
    let start = end.saturating_sub(8);
    lines[start..end].iter().any(|l| is_spinner_row(l))
        || lines.iter().any(|l| l.contains("esc to interrupt"))
}

fn trust_dialog(s: &ScreenSnapshot) -> bool {
    s.contains("Yes, I trust this folder")
        || s.contains("Is this a project you created or one you trust")
}

/// Permission dialog: (index of the "Do you want …?" row, options below it).
fn permission_dialog(s: &ScreenSnapshot) -> Option<(usize, Vec<tui::MenuOption>)> {
    let lines = &s.lines;
    let q = tui::rfind(lines, |l| {
        let t = l.trim();
        (t.starts_with("Do you want to") || t.starts_with("Would you like to")) && t.ends_with('?')
    })?;
    let opts = tui::options_in(lines, q + 1, q + 12);
    let has_footer = lines
        .iter()
        .skip(q + 1)
        .take(14)
        .any(|l| l.contains("Esc to cancel"));
    if has_footer && opts.len() >= 2 && opts.iter().any(|o| tui::is_yes_label(&o.label)) {
        Some((q, opts))
    } else {
        None
    }
}

fn permission_detail(lines: &[String], q: usize) -> String {
    // Header block between the rule above the question and the question itself, e.g.
    // "Bash command" / "touch zzz.txt" or "Create file" / "b.txt".
    let top = (0..q)
        .rev()
        .find(|&i| tui::is_solid_rule(&lines[i]))
        .map(|i| i + 1)
        .unwrap_or(q);
    let block: Vec<&str> = lines[top..q]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !tui::is_rule(l))
        .collect();
    match block.as_slice() {
        [head, arg, ..] => clip(&format!("{head}: {arg}"), 160),
        [head] => clip(head, 160),
        [] => clip(lines[q].trim(), 160),
    }
}

/// AskUserQuestion / other selection menus: footer "Enter to select · ↑/↓ to navigate".
fn question_dialog(s: &ScreenSnapshot) -> Option<String> {
    let footer = tui::rfind(&s.lines, |l| {
        l.contains("Enter to select") && l.contains("to navigate")
    })?;
    let lines = &s.lines;
    let first_opt = (0..footer)
        .rev()
        .take(20)
        .find(|&i| tui::parse_option(&lines[i]).is_some_and(|o| o.digit == '1'))?;
    let question = lines[..first_opt]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !tui::is_rule(l))
        .unwrap_or("Claude is asking a question");
    Some(clip(question, 200))
}

fn api_error(lines: &[String], box_row: usize) -> Option<String> {
    let top = (0..box_row)
        .rev()
        .find(|&i| tui::is_rule(&lines[i]))
        .unwrap_or(box_row);
    let start = top.saturating_sub(6);
    lines[start..top]
        .iter()
        .map(|l| l.trim().trim_start_matches('⎿').trim())
        .find(|l| l.starts_with("API Error") || l.starts_with("Claude usage limit reached"))
        .map(|l| clip(l, 200))
}

impl Adapter for ClaudeAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Claude
    }

    fn command(&self, ctx: &SpawnCtx) -> anyhow::Result<CommandSpec> {
        let settings_path = ctx.session_dir.join("claude-settings.json");
        let mcp_path = ctx.session_dir.join("claude-mcp.json");
        write_private(
            &settings_path,
            &serde_json::to_string_pretty(&settings_json(&ctx.muster_bin))?,
        )?;
        write_private(
            &mcp_path,
            &serde_json::to_string_pretty(&mcp_json(&ctx.daemon_url, &ctx.session_token))?,
        )?;
        // --mcp-config is variadic, so --settings (single value) must follow it.
        let mut args = vec![
            "--mcp-config".to_string(),
            mcp_path.to_string_lossy().into_owned(),
            "--settings".to_string(),
            settings_path.to_string_lossy().into_owned(),
        ];
        if let Some(mode) = permission_mode(ctx.permissions.claude_mode) {
            args.extend(["--permission-mode".to_string(), mode.to_string()]);
        }
        args.extend(ctx.extra_args.iter().cloned());
        let mut consumed = false;
        if let Some(p) = ctx.initial_prompt.as_ref().filter(|p| !p.trim().is_empty()) {
            // "--" ends option parsing so the prompt can't be swallowed by a variadic flag in
            // extra_args or be mistaken for a flag itself.
            args.push("--".to_string());
            args.push(p.clone());
            consumed = true;
        }
        // MCP tool calls default to a 10 min timeout; muster's wait_for_state may block up to
        // 1800s. Raise it for this session unless the daemon env already sets more.
        let mut env = std::collections::BTreeMap::new();
        let current = std::env::var("MCP_TOOL_TIMEOUT")
            .ok()
            .and_then(|v| v.replace('_', "").parse::<f64>().ok())
            .unwrap_or(0.0);
        if current < MCP_TOOL_TIMEOUT_MS as f64 {
            env.insert(
                "MCP_TOOL_TIMEOUT".to_string(),
                MCP_TOOL_TIMEOUT_MS.to_string(),
            );
        }
        Ok(CommandSpec {
            program: "claude".to_string(),
            args,
            env,
            consumed_initial_prompt: consumed,
        })
    }

    fn on_hook(&self, event: &str, payload: &Value) -> Option<StateUpdate> {
        use AgentState::*;
        let tool = payload
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // A subagent's hooks carry its `agent_id`. Its tool calls say nothing about the main
        // agent: they arrive while the main agent waits at its prompt or asks the human a
        // question, and setting Working there made the state flip back and forth with the
        // screen parser every second or so. Only its requests for the human still count: a
        // permission request, or a notification that it needs input.
        let subagent = payload.get("agent_id").and_then(|v| v.as_str()).is_some_and(|a| !a.is_empty());
        let wants_human = event == "PermissionRequest"
            || event == "Notification"
                && matches!(
                    payload.get("notification_type").and_then(|v| v.as_str()),
                    Some("elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input")
                );
        if subagent && !wants_human {
            return None;
        }
        match event {
            // SessionStart fires after a positional initial prompt may already be running, so
            // it can't mean Idle; the screen parser moves Starting -> Idle when the box renders.
            "SessionStart" => None,
            "UserPromptSubmit" => Some(match first_line(payload.get("prompt"), 120) {
                Some(p) => StateUpdate::with(Working, p),
                None => StateUpdate::new(Working),
            }),
            "PreToolUse" if tool == "AskUserQuestion" => Some(StateUpdate {
                state: AskingQuestion,
                detail: question_detail(payload).or(Some("Claude is asking a question".into())),
            }),
            "PreToolUse" => Some(StateUpdate {
                state: Working,
                detail: tool_detail(payload),
            }),
            "PermissionRequest" if tool == "AskUserQuestion" => Some(StateUpdate {
                state: AskingQuestion,
                detail: question_detail(payload).or(Some("Claude is asking a question".into())),
            }),
            "PermissionRequest" if tool == "ExitPlanMode" => Some(StateUpdate::with(
                NeedsApproval,
                "Plan ready: approve to start",
            )),
            "PermissionRequest" => Some(StateUpdate {
                state: NeedsApproval,
                detail: tool_detail(payload).or(Some("Permission requested".into())),
            }),
            "PostToolUse" | "PostToolUseFailure" => Some(StateUpdate::new(Working)),
            "PreCompact" => Some(StateUpdate::with(Working, "Compacting conversation")),
            "Notification" => {
                let msg = first_line(payload.get("message"), 200);
                match payload
                    .get("notification_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                {
                    // PermissionRequest already set NeedsApproval with a better detail; the
                    // screen parser covers the case where it didn't fire.
                    "permission_prompt" => None,
                    "idle_prompt" => Some(StateUpdate::new(Idle)),
                    "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input" => {
                        Some(StateUpdate {
                            state: AskingQuestion,
                            detail: msg.or(Some("Claude needs input".into())),
                        })
                    }
                    _ => None,
                }
            }
            "Stop" => Some(
                match first_line(payload.get("last_assistant_message"), 160) {
                    Some(m) => StateUpdate::with(Idle, m),
                    None => StateUpdate::new(Idle),
                },
            ),
            "StopFailure" => {
                let kind = payload
                    .get("error_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let msg = first_line(payload.get("error_message").or(payload.get("message")), 160);
                Some(StateUpdate::with(
                    Error,
                    match msg {
                        Some(m) => format!("{kind}: {m}"),
                        None => format!("Turn failed: {kind}"),
                    },
                ))
            }
            // SubagentStop: the main agent keeps going. SessionEnd: process exit is handled by
            // the PTY watcher.
            _ => None,
        }
    }

    fn detect_from_screen(
        &self,
        screen: &ScreenSnapshot,
        current: AgentState,
    ) -> Option<StateUpdate> {
        use AgentState::*;
        if trust_dialog(screen) {
            return Some(StateUpdate::with(
                AskingQuestion,
                "Claude asks whether to trust this folder: answer in the terminal",
            ));
        }
        if let Some((q, _)) = permission_dialog(screen) {
            return Some(StateUpdate::with(
                NeedsApproval,
                permission_detail(&screen.lines, q),
            ));
        }
        if let Some(q) = question_dialog(screen) {
            return Some(StateUpdate::with(AskingQuestion, q));
        }
        let box_row = input_row(&screen.lines);
        if is_working(&screen.lines, box_row) {
            return Some(StateUpdate::new(Working));
        }
        let row = box_row?;
        if let Some(err) = api_error(&screen.lines, row)
            && matches!(current, Working | Error | Starting)
        {
            return Some(StateUpdate::with(Error, err));
        }
        if current == Error {
            return None;
        }
        Some(StateUpdate::new(Idle))
    }

    fn approve_bytes(&self, decision: Decision, screen: &ScreenSnapshot) -> Option<Vec<u8>> {
        if trust_dialog(screen) {
            return None;
        }
        let (_, opts) = permission_dialog(screen)?;
        match tui::choose(&opts, decision) {
            Some(o) => Some(tui::option_keys(o, false)),
            // Esc cancels the dialog (= deny) if no explicit "No" row was recognized.
            None if decision == Decision::Deny => Some(vec![0x1b]),
            None => None,
        }
    }

    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x1b]
    }

    fn composer_text(&self, screen: &ScreenSnapshot) -> Option<String> {
        if trust_dialog(screen) || permission_dialog(screen).is_some() {
            return None;
        }
        let lines = &screen.lines;
        let row = input_row(lines)?;
        let mut text = vec![lines[row].trim_start_matches('❯').trim().to_string()];
        for l in &lines[row + 1..] {
            if tui::is_rule(l) {
                break;
            }
            text.push(l.trim().to_string());
        }
        let t = text.join("\n").trim().to_string();
        // Dim placeholder suggestions look like `Try "…"`.
        if t.starts_with("Try \"") {
            return Some(String::new());
        }
        Some(t)
    }

    fn clear_input_bytes(&self) -> Option<Vec<u8>> {
        Some(vec![0x03])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::testutil::snap;

    macro_rules! fx {
        ($name:literal) => {
            snap(include_str!(concat!("fixtures/", $name, ".txt")))
        };
    }

    fn detect(s: &ScreenSnapshot, cur: AgentState) -> Option<StateUpdate> {
        ClaudeAdapter.detect_from_screen(s, cur)
    }

    #[test]
    fn trust_dialog_is_a_question_and_never_approved() {
        let s = fx!("claude_trust");
        let u = detect(&s, AgentState::Starting).unwrap();
        assert_eq!(u.state, AgentState::AskingQuestion);
        assert!(u.detail.unwrap().contains("trust"));
        assert_eq!(ClaudeAdapter.approve_bytes(Decision::Approve, &s), None);
        assert_eq!(ClaudeAdapter.approve_bytes(Decision::Deny, &s), None);
    }

    #[test]
    fn idle_screens() {
        for s in [
            fx!("claude_idle_fresh"),
            fx!("claude_idle_done"),
            fx!("claude_ctrlc_once"),
        ] {
            assert_eq!(
                detect(&s, AgentState::Starting).unwrap().state,
                AgentState::Idle
            );
            assert_eq!(ClaudeAdapter.approve_bytes(Decision::Approve, &s), None);
        }
        assert_eq!(
            ClaudeAdapter
                .composer_text(&fx!("claude_idle_done"))
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn working_screens() {
        for s in [fx!("claude_thinking"), fx!("claude_running_tool")] {
            assert_eq!(
                detect(&s, AgentState::Idle).unwrap().state,
                AgentState::Working
            );
        }
    }

    #[test]
    fn bash_permission() {
        let s = fx!("claude_perm_bash");
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(u.detail.as_deref(), Some("Bash command: touch zzz.txt"));
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::Approve, &s),
            Some(b"1".to_vec())
        );
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::ApproveAlways, &s),
            Some(b"2".to_vec())
        );
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::Deny, &s),
            Some(b"3".to_vec())
        );
        assert_eq!(ClaudeAdapter.composer_text(&s), None);
    }

    #[test]
    fn write_permission() {
        let s = fx!("claude_perm_write");
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(u.detail.as_deref(), Some("Create file: b.txt"));
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::Approve, &s),
            Some(b"1".to_vec())
        );
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::ApproveAlways, &s),
            Some(b"2".to_vec())
        );
        assert_eq!(
            ClaudeAdapter.approve_bytes(Decision::Deny, &s),
            Some(b"3".to_vec())
        );
    }

    #[test]
    fn ask_user_question() {
        let s = fx!("claude_question");
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::AskingQuestion);
        assert_eq!(u.detail.as_deref(), Some("Which color?"));
        assert_eq!(ClaudeAdapter.approve_bytes(Decision::Approve, &s), None);
    }

    #[test]
    fn composer_reads_typed_and_restored_text() {
        assert_eq!(
            ClaudeAdapter
                .composer_text(&fx!("claude_typed_multiline"))
                .as_deref(),
            Some("first line\nsecond line")
        );
        let restored = fx!("claude_interrupted_restored");
        assert!(
            ClaudeAdapter
                .composer_text(&restored)
                .unwrap()
                .starts_with("Use the AskUserQuestion tool")
        );
        assert_eq!(
            detect(&restored, AgentState::Working).unwrap().state,
            AgentState::Idle
        );
    }

    #[test]
    fn hooks_map_to_states() {
        let a = ClaudeAdapter;
        let pre =
            json!({"tool_name":"Bash","tool_input":{"command":"cargo test","description":"x"}});
        assert_eq!(
            a.on_hook("PreToolUse", &pre),
            Some(StateUpdate::with(AgentState::Working, "Bash: cargo test"))
        );
        assert_eq!(
            a.on_hook("PermissionRequest", &pre),
            Some(StateUpdate::with(
                AgentState::NeedsApproval,
                "Bash: cargo test"
            ))
        );
        let ask = json!({"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which color?"}]}});
        assert_eq!(
            a.on_hook("PermissionRequest", &ask),
            Some(StateUpdate::with(
                AgentState::AskingQuestion,
                "Which color?"
            ))
        );
        assert_eq!(
            a.on_hook("SessionStart", &json!({"source":"startup"})),
            None
        );
        assert_eq!(
            a.on_hook("Stop", &json!({"last_assistant_message":"Done.\nMore"})),
            Some(StateUpdate::with(AgentState::Idle, "Done."))
        );
        assert_eq!(
            a.on_hook(
                "Notification",
                &json!({"notification_type":"permission_prompt","message":"x"})
            ),
            None
        );
        assert_eq!(
            a.on_hook("StopFailure", &json!({"error_type":"rate_limit"}))
                .unwrap()
                .state,
            AgentState::Error
        );
        assert_eq!(a.on_hook("SubagentStop", &json!({})), None);
    }

    /// A background subagent's tool calls (payloads captured from Claude Code, which adds
    /// `agent_id`) must not pull the main agent out of its question or its idle prompt.
    #[test]
    fn subagent_hooks_leave_the_state_alone() {
        let a = ClaudeAdapter;
        let sub = json!({"tool_name":"Bash","tool_input":{"command":"ls"},
            "agent_id":"a8c3f099539c46aa2","agent_type":"general-purpose"});
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PreCompact", "Stop"] {
            assert_eq!(a.on_hook(event, &sub), None, "{event}");
        }
        assert_eq!(
            a.on_hook("PermissionRequest", &sub),
            Some(StateUpdate::with(AgentState::NeedsApproval, "Bash: ls"))
        );
        let ask = json!({"notification_type":"elicitation_url_dialog","message":"Sign in",
            "agent_id":"a8c3f099539c46aa2"});
        assert_eq!(a.on_hook("Notification", &ask).unwrap().state, AgentState::AskingQuestion);
        let idle = json!({"notification_type":"idle_prompt","agent_id":"a8c3f099539c46aa2"});
        assert_eq!(a.on_hook("Notification", &idle), None);
        // The main agent's own hooks have no agent_id (or an empty one).
        let main = json!({"tool_name":"Bash","tool_input":{"command":"ls"},"agent_id":""});
        assert_eq!(a.on_hook("PreToolUse", &main).unwrap().state, AgentState::Working);
    }

    #[test]
    fn command_writes_configs() {
        let dir = std::env::temp_dir().join(format!("muster-claude-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = SpawnCtx {
            session_id: 7,
            cwd: dir.clone(),
            muster_bin: "/opt/my apps/muster".into(),
            daemon_url: "http://127.0.0.1:7555".into(),
            session_token: "tok123".into(),
            session_dir: dir.clone(),
            extra_args: vec!["--model".into(), "haiku".into()],
            initial_prompt: Some("-do it".into()),
            permissions: Permissions::default(),
            resume: None,
        };
        let spec = ClaudeAdapter.command(&ctx).unwrap();
        assert!(spec.consumed_initial_prompt);
        if std::env::var_os("MCP_TOOL_TIMEOUT").is_none() {
            assert_eq!(
                spec.env.get("MCP_TOOL_TIMEOUT").map(String::as_str),
                Some("1900000")
            );
        }
        assert_eq!(spec.args[0], "--mcp-config");
        assert_eq!(spec.args[2], "--settings");
        // The default profile passes no --permission-mode (the user's own setting applies).
        assert!(!spec.args.iter().any(|a| a == "--permission-mode"));
        assert_eq!(&spec.args[4..], &["--model", "haiku", "--", "-do it"]);
        for (mode, flag) in [
            (ClaudeMode::AcceptEdits, "acceptEdits"),
            (ClaudeMode::Plan, "plan"),
            (ClaudeMode::Bypass, "bypassPermissions"),
        ] {
            let mut c = ctx.clone();
            c.permissions = Permissions {
                codex_sandbox: CodexSandbox::FullAccess,
                claude_mode: mode,
            };
            let a = ClaudeAdapter.command(&c).unwrap().args;
            let i = a.iter().position(|x| x == "--permission-mode").unwrap();
            assert_eq!(a[i + 1], flag);
            assert_eq!(a.iter().filter(|x| *x == "--permission-mode").count(), 1);
            assert!(!a.iter().any(|x| x.contains("dangerously")));
            assert!(i < a.iter().position(|x| x == "--model").unwrap());
        }
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("claude-mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["muster"]["url"],
            "http://127.0.0.1:7555/mcp"
        );
        assert_eq!(
            mcp["mcpServers"]["muster"]["headers"]["Authorization"],
            "Bearer tok123"
        );
        let st: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("claude-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            st["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "'/opt/my apps/muster' hook claude PreToolUse"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("claude-mcp.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
