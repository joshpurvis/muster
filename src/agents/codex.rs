//! Codex CLI adapter (verified against `codex` v0.155.1).
//!
//! Launch: `codex -c notify=[...] -c mcp_servers.muster.url=... -c
//! mcp_servers.muster.bearer_token_env_var="MUSTER_TOKEN" [-c hooks.<Event>=[...]]... [extra args]
//! [-- <initial prompt>]`. All settings are `-c` overrides for this process only;
//! `~/.codex/config.toml` is never touched.
//! - notify: Codex runs `<muster_bin> hook codex notify <json>` with the JSON payload as the last
//!   argv item (`{"type":"agent-turn-complete","thread-id",...,"input-messages",
//!   "last-assistant-message"}`). Codex also fires it for its internal title-generation
//!   request, which we ignore.
//! - hooks: `hooks.<Event>=[{hooks=[{type="command",command="<bin> hook codex <Event>"}]}]`,
//!   JSON payload on stdin (like Claude's). Codex only runs hooks it has trusted: the first
//!   session shows a "Hooks need review" dialog (reported as AskingQuestion). The command strings
//!   are identical for every session, so trusting them once (Trust all / `/hooks`) sticks
//!   (guess: trust is keyed by the hook definition hash, per the Codex docs). Set
//!   `MUSTER_CODEX_HOOKS=0` in the daemon's env to skip them and rely on notify + screen parsing.
//! - MCP: streamable HTTP server `muster` at `<daemon_url>/mcp`, bearer token read by Codex from
//!   the session's `MUSTER_TOKEN` env var.
//!
//! Screen facts (see fixtures/codex_*.txt): composer row starts with `› `; working shows
//! `• Working (1s • esc to interrupt)`; approvals read `Would you like to run the following
//! command?` with `1. Yes, proceed (y)` / `2. Yes, and don't ask again … (p)` /
//! `3. No, and tell Codex what to do differently (esc)`.

use serde_json::Value;

use crate::agents::claude::{sh_quote, tool_detail};
use crate::agents::tui::{self, clip};
use crate::agents::*;
use crate::model::*;

pub struct CodexAdapter;

pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "PreCompact",
];

/// TOML basic string (JSON string escaping is a valid subset).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn hooks_enabled() -> bool {
    !matches!(
        std::env::var("MUSTER_CODEX_HOOKS").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// The `-c` overrides muster adds for a session.
pub fn config_overrides(muster_bin: &str, daemon_url: &str, with_hooks: bool) -> Vec<String> {
    let mut out = vec![
        format!(
            "notify=[{},{},{},{}]",
            toml_str(muster_bin),
            toml_str("hook"),
            toml_str("codex"),
            toml_str("notify")
        ),
        format!(
            "mcp_servers.muster.url={}",
            toml_str(&format!("{}/mcp", daemon_url.trim_end_matches('/')))
        ),
        "mcp_servers.muster.bearer_token_env_var=\"MUSTER_TOKEN\"".to_string(),
        // Codex's default tool timeout (~60s) would cut off wait_for_state (max 1800s).
        "mcp_servers.muster.tool_timeout_sec=1900".to_string(),
    ];
    if with_hooks {
        let bin = sh_quote(muster_bin);
        for ev in HOOK_EVENTS {
            out.push(format!(
                "hooks.{ev}=[{{hooks=[{{type=\"command\",command={},timeout=10}}]}}]",
                toml_str(&format!("{bin} hook codex {ev}"))
            ));
        }
    }
    out
}

fn first_line(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(clip(s.lines().next().unwrap_or(s), max))
}

/// Codex fires notify for its internal "generate a title" request too.
fn is_title_generation(payload: &Value) -> bool {
    payload
        .get("input-messages")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.starts_with("Generate a concise, single-line task title"))
}

// ----- screen parsing -----

fn trust_dialog(s: &ScreenSnapshot) -> bool {
    s.contains("Do you trust the contents of this directory?")
}

fn hooks_review(s: &ScreenSnapshot) -> bool {
    s.contains("Hooks need review") && s.lines.iter().any(|l| tui::parse_option(l).is_some())
}

/// Approval dialog: (question row, options).
fn approval_dialog(s: &ScreenSnapshot) -> Option<(usize, Vec<tui::MenuOption>)> {
    let lines = &s.lines;
    let q = tui::rfind(lines, |l| {
        let t = l.trim();
        (t.starts_with("Would you like to")
            || t.starts_with("Do you want to")
            || t.starts_with("Allow "))
            && t.ends_with('?')
    })?;
    let opts = tui::options_in(lines, q + 1, q + 16);
    let has_footer = lines
        .iter()
        .skip(q + 1)
        .take(18)
        .any(|l| l.contains("Press enter to confirm") || l.contains("esc to cancel"));
    if has_footer && opts.len() >= 2 && opts.iter().any(|o| tui::is_yes_label(&o.label)) {
        Some((q, opts))
    } else {
        None
    }
}

fn approval_detail(lines: &[String], q: usize) -> String {
    let below = &lines[q + 1..(q + 10).min(lines.len())];
    if let Some(cmd) = below
        .iter()
        .map(|l| l.trim())
        .find_map(|l| l.strip_prefix("$ "))
    {
        return clip(&format!("Run: {cmd}"), 160);
    }
    if let Some(r) = below
        .iter()
        .map(|l| l.trim())
        .find_map(|l| l.strip_prefix("Reason: "))
    {
        return clip(r, 160);
    }
    clip(lines[q].trim(), 160)
}

/// Composer row: the last `› ` row that isn't a menu option.
fn composer_row(lines: &[String]) -> Option<usize> {
    tui::rfind(lines, |l| l.starts_with("› ") || l == "›")
        .filter(|&i| tui::parse_option(&lines[i]).is_none())
}

fn is_working(s: &ScreenSnapshot) -> bool {
    s.lines.iter().any(|l| l.contains("esc to interrupt"))
}

/// Last message before the composer is a `■ …` error (not the "interrupted" notice).
fn error_banner(lines: &[String], composer: usize) -> Option<String> {
    let idx = (0..composer).rev().find(|&i| {
        let t = lines[i].trim_start();
        t.starts_with('■') || t.starts_with('•') || t.starts_with('›') || t.starts_with('✗')
    })?;
    let t = lines[idx].trim_start();
    let msg = t.strip_prefix('■')?.trim();
    if msg.starts_with("Conversation interrupted") {
        return None;
    }
    Some(clip(msg, 200))
}

/// Placeholder texts Codex shows in an empty composer (dim; indistinguishable in plain text).
const PLACEHOLDERS: &[&str] = &[
    "Ask Codex to do anything",
    "Explain this codebase",
    "Summarize recent commits",
    "Implement {feature}",
    "Find and fix a bug in @filename",
    "Write tests for @filename",
    "Improve documentation in @filename",
    "Run /review on my current changes",
    "Use /skills to list available skills",
];

impl Adapter for CodexAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Codex
    }

    fn command(&self, ctx: &SpawnCtx) -> anyhow::Result<CommandSpec> {
        let bin = ctx.muster_bin.to_string_lossy();
        let mut args = Vec::new();
        for o in config_overrides(&bin, &ctx.daemon_url, hooks_enabled()) {
            args.push("-c".to_string());
            args.push(o);
        }
        args.extend(ctx.extra_args.iter().cloned());
        let mut consumed = false;
        if let Some(p) = ctx.initial_prompt.as_ref().filter(|p| !p.trim().is_empty()) {
            args.push("--".to_string());
            args.push(p.clone());
            consumed = true;
        }
        Ok(CommandSpec {
            program: "codex".to_string(),
            args,
            env: Default::default(),
            consumed_initial_prompt: consumed,
        })
    }

    fn on_hook(&self, event: &str, payload: &Value) -> Option<StateUpdate> {
        use AgentState::*;
        match event {
            "notify" => {
                let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if kind != "agent-turn-complete" || is_title_generation(payload) {
                    return None;
                }
                Some(
                    match first_line(payload.get("last-assistant-message"), 160) {
                        Some(m) => StateUpdate::with(Idle, m),
                        None => StateUpdate::new(Idle),
                    },
                )
            }
            // SessionStart fires after a positional initial prompt may already be running, so
            // it can't mean Idle; the screen parser moves Starting -> Idle when the box renders.
            "SessionStart" => None,
            "UserPromptSubmit" => Some(match first_line(payload.get("prompt"), 120) {
                Some(p) => StateUpdate::with(Working, p),
                None => StateUpdate::new(Working),
            }),
            "PreToolUse" => Some(StateUpdate {
                state: Working,
                detail: tool_detail(payload),
            }),
            "PermissionRequest" => Some(StateUpdate {
                state: NeedsApproval,
                detail: tool_detail(payload).or(Some("Permission requested".into())),
            }),
            "PostToolUse" => Some(StateUpdate::new(Working)),
            "PreCompact" => Some(StateUpdate::with(Working, "Compacting conversation")),
            "Stop" => Some(
                match first_line(payload.get("last_assistant_message"), 160) {
                    Some(m) => StateUpdate::with(Idle, m),
                    None => StateUpdate::new(Idle),
                },
            ),
            "Interrupt" => Some(StateUpdate::with(Idle, "Interrupted")),
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
                "Codex asks whether to trust this directory: answer in the terminal",
            ));
        }
        if hooks_review(screen) {
            return Some(StateUpdate::with(
                AskingQuestion,
                "Codex wants muster's hooks reviewed: trust them once in the terminal (or pick continue without)",
            ));
        }
        if let Some((q, _)) = approval_dialog(screen) {
            return Some(StateUpdate::with(
                NeedsApproval,
                approval_detail(&screen.lines, q),
            ));
        }
        if is_working(screen) {
            return Some(StateUpdate::new(Working));
        }
        let row = composer_row(&screen.lines)?;
        if let Some(err) = error_banner(&screen.lines, row)
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
        if trust_dialog(screen) || hooks_review(screen) {
            return None;
        }
        let (_, opts) = approval_dialog(screen)?;
        match tui::choose(&opts, decision) {
            Some(o) => Some(tui::option_keys(o, true)),
            None if decision == Decision::Deny => Some(vec![0x1b]),
            None => None,
        }
    }

    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x1b]
    }

    fn composer_text(&self, screen: &ScreenSnapshot) -> Option<String> {
        if trust_dialog(screen) || hooks_review(screen) || approval_dialog(screen).is_some() {
            return None;
        }
        let lines = &screen.lines;
        let row = composer_row(lines)?;
        let mut text = vec![lines[row].trim_start_matches('›').trim().to_string()];
        // Continuation rows are indented by two spaces; the footer follows a blank row.
        for l in &lines[row + 1..] {
            if l.trim().is_empty() || !l.starts_with("  ") {
                break;
            }
            text.push(l.trim().to_string());
        }
        let t = text.join("\n").trim().to_string();
        if PLACEHOLDERS.contains(&t.as_str()) {
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
    use crate::agents::tui::testutil::snap;
    use serde_json::json;

    macro_rules! fx {
        ($name:literal) => {
            snap(include_str!(concat!("fixtures/", $name, ".txt")))
        };
    }

    fn detect(s: &ScreenSnapshot, cur: AgentState) -> Option<StateUpdate> {
        CodexAdapter.detect_from_screen(s, cur)
    }

    #[test]
    fn trust_and_hooks_review_are_questions() {
        for s in [fx!("codex_trust"), fx!("codex_hooks_review")] {
            assert_eq!(
                detect(&s, AgentState::Starting).unwrap().state,
                AgentState::AskingQuestion
            );
            assert_eq!(CodexAdapter.approve_bytes(Decision::Approve, &s), None);
            assert_eq!(CodexAdapter.approve_bytes(Decision::Deny, &s), None);
        }
    }

    #[test]
    fn idle_and_working() {
        for s in [
            fx!("codex_idle_fresh"),
            fx!("codex_idle_done"),
            fx!("codex_interrupted"),
            fx!("codex_typed"),
        ] {
            assert_eq!(
                detect(&s, AgentState::Working).unwrap().state,
                AgentState::Idle
            );
        }
        assert_eq!(
            detect(&fx!("codex_working"), AgentState::Idle)
                .unwrap()
                .state,
            AgentState::Working
        );
        assert_eq!(
            CodexAdapter
                .composer_text(&fx!("codex_idle_fresh"))
                .as_deref(),
            Some("")
        );
        assert_eq!(
            CodexAdapter.composer_text(&fx!("codex_working")).as_deref(),
            Some("")
        );
        assert_eq!(
            CodexAdapter.composer_text(&fx!("codex_typed")).as_deref(),
            Some("Run exactly this shell command and nothing else: touch zzz.txt")
        );
    }

    #[test]
    fn exec_approval() {
        let s = fx!("codex_approval_exec");
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(u.detail.as_deref(), Some("Run: touch zzz.txt"));
        assert_eq!(
            CodexAdapter.approve_bytes(Decision::Approve, &s),
            Some(b"y".to_vec())
        );
        assert_eq!(
            CodexAdapter.approve_bytes(Decision::ApproveAlways, &s),
            Some(b"p".to_vec())
        );
        assert_eq!(
            CodexAdapter.approve_bytes(Decision::Deny, &s),
            Some(vec![0x1b])
        );
        assert_eq!(CodexAdapter.composer_text(&s), None);
    }

    #[test]
    fn no_prompt_no_bytes() {
        assert_eq!(
            CodexAdapter.approve_bytes(Decision::Approve, &fx!("codex_idle_done")),
            None
        );
    }

    #[test]
    fn error_banner_detected() {
        let mut s = fx!("codex_idle_fresh");
        let row = composer_row(&s.lines).unwrap();
        s.lines[row - 2] = "■ unexpected status 503 Service Unavailable".into();
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::Error);
        assert!(u.detail.unwrap().contains("503"));
        // Interrupted notice is not an error.
        assert_eq!(
            detect(&fx!("codex_interrupted"), AgentState::Working)
                .unwrap()
                .state,
            AgentState::Idle
        );
    }

    #[test]
    fn hooks_and_notify() {
        let a = CodexAdapter;
        let done = json!({"type":"agent-turn-complete","input-messages":["hi"],"last-assistant-message":"ok"});
        assert_eq!(
            a.on_hook("notify", &done),
            Some(StateUpdate::with(AgentState::Idle, "ok"))
        );
        let title = json!({"type":"agent-turn-complete","input-messages":["Generate a concise, single-line task title of at most 36 characters"]});
        assert_eq!(a.on_hook("notify", &title), None);
        let pre = json!({"tool_name":"Bash","tool_input":{"command":"touch zzz.txt"}});
        assert_eq!(
            a.on_hook("PreToolUse", &pre),
            Some(StateUpdate::with(
                AgentState::Working,
                "Bash: touch zzz.txt"
            ))
        );
        assert_eq!(
            a.on_hook("PermissionRequest", &pre).unwrap().state,
            AgentState::NeedsApproval
        );
        assert_eq!(
            a.on_hook("Interrupt", &json!({})).unwrap().state,
            AgentState::Idle
        );
        assert_eq!(
            a.on_hook("Stop", &json!({"last_assistant_message":"ok"})),
            Some(StateUpdate::with(AgentState::Idle, "ok"))
        );
    }

    #[test]
    fn overrides_are_valid_toml_shapes() {
        let o = config_overrides("/opt/my apps/muster", "http://127.0.0.1:7555/", true);
        assert_eq!(
            o[0],
            r#"notify=["/opt/my apps/muster","hook","codex","notify"]"#
        );
        assert_eq!(
            o[1],
            r#"mcp_servers.muster.url="http://127.0.0.1:7555/mcp""#
        );
        assert_eq!(
            o[2],
            r#"mcp_servers.muster.bearer_token_env_var="MUSTER_TOKEN""#
        );
        assert!(o.iter().any(|x| x
            == r#"hooks.Stop=[{hooks=[{type="command",command="'/opt/my apps/muster' hook codex Stop",timeout=10}]}]"#));
        assert_eq!(o[3], "mcp_servers.muster.tool_timeout_sec=1900");
        assert_eq!(config_overrides("/m", "http://x", false).len(), 4);
    }
}
