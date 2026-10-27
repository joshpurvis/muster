//! Codex CLI adapter (launch flags verified against `codex` v0.164.0).
//!
//! Launch: `codex -c notify=[...] -c mcp_servers.muster.url=... -c
//! mcp_servers.muster.bearer_token_env_var="MUSTER_TOKEN" [-c hooks.<Event>=[...]]...
//! --sandbox <mode> [-c sandbox_workspace_write.network_access=<bool>] [extra args]
//! [-- <initial prompt>]`. The sandbox flags come from the channel's permission profile
//! (`sandbox_args`). All settings are `-c` overrides for this process only;
//! `~/.codex/config.toml` is never touched.
//! - Resume: a run the hub gives a thread (`SpawnCtx::resume`; saved outside the stored
//!   arguments, which older backends must keep accepting) launches `codex resume <the same
//!   flags> -- <thread id> [<initial prompt>]`. Checked against `codex resume --help` of
//!   codex-cli 0.164.0: the subcommand takes every flag Muster passes (`-c`, `--sandbox`, `-m`,
//!   `-i`, `--no-alt-screen`) after it, then `[SESSION_ID] [PROMPT]`. A thread Codex no longer
//!   has makes it print "No saved session found with ID <id>. …" (that string is in the 0.164.0
//!   binary) and exit; only that message makes Muster forget the thread
//!   (`adapters::says_conversation_missing`).
//! - approvals: `approval_policy="on-request"` and `approvals_reviewer="auto_review"` give
//!   new sessions the approval behavior of `--approve-for-me`. That shorthand conflicts with
//!   `--sandbox`, so use its settings directly to preserve the channel's selected sandbox.
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

use crate::adapters::claude::{sh_quote, tool_detail};
use crate::adapters::tui::{self, clip};
use crate::adapters::*;
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

/// Sandbox flags for the channel's profile: always an explicit `--sandbox`, plus the
/// workspace-write network switch (set either way, so a `network_access = true` in the user's
/// config.toml can't widen a no-network profile). Approval review is set in `config_overrides`.
pub fn sandbox_args(sandbox: CodexSandbox) -> Vec<String> {
    let (mode, network) = match sandbox {
        CodexSandbox::ReadOnly => ("read-only", None),
        CodexSandbox::WorkspaceWrite => ("workspace-write", Some(false)),
        CodexSandbox::WorkspaceWriteNetwork => ("workspace-write", Some(true)),
        CodexSandbox::FullAccess => ("danger-full-access", None),
    };
    let mut out = vec!["--sandbox".to_string(), mode.to_string()];
    if let Some(n) = network {
        out.push("-c".to_string());
        out.push(format!("sandbox_workspace_write.network_access={n}"));
    }
    out
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
        // `--approve-for-me` conflicts with our explicit --sandbox. Set its approval behavior
        // separately so read-only, workspace/network and full-access profiles keep their bounds.
        "approval_policy=\"on-request\"".to_string(),
        "approvals_reviewer=\"auto_review\"".to_string(),
    ];
    if with_hooks {
        let bin = sh_quote(muster_bin);
        for ev in HOOK_EVENTS {
            // Codex clamps Interrupt hooks to 3s and prints a warning for anything longer.
            let timeout = if *ev == "Interrupt" { 3 } else { 10 };
            out.push(format!(
                "hooks.{ev}=[{{hooks=[{{type=\"command\",command={},timeout={timeout}}}]}}]",
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

/// The subcommand that reopens a saved conversation (`codex resume [OPTIONS] [SESSION_ID]
/// [PROMPT]`). It comes first, so anything Muster adds to a launch goes after it.
pub const RESUME: &str = "resume";


/// Codex fires notify for its internal "generate a title" request too.
pub(crate) fn is_title_generation(payload: &Value) -> bool {
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

/// Question row of an approval: a known opening whose text ends in `?`, on this row or on the
/// next two (a narrow pane wraps the title).
fn is_question_at(lines: &[String], i: usize) -> bool {
    let t = lines[i].trim();
    if !(t.starts_with("Would you like to")
        || t.starts_with("Do you want to")
        || t.starts_with("Allow "))
    {
        return false;
    }
    lines[i..(i + 3).min(lines.len())]
        .iter()
        .map(|l| l.trim())
        .take_while(|t| !t.is_empty())
        .any(|t| t.ends_with('?'))
}

/// Approval dialog: (question row, options). Found from the bottom, since the command shown
/// between the question and the options can be any length: the footer, then the block of
/// options right above it (wrapped labels included), then the question above that. This is
/// the only signal that Codex is asking the human (see `PermissionRequest` in `on_hook`).
fn approval_dialog(s: &ScreenSnapshot) -> Option<(usize, Vec<tui::MenuOption>)> {
    let lines = &s.lines;
    let footer = tui::rfind(lines, |l| {
        l.contains("Press enter to confirm") || l.contains("esc to cancel")
    })?;
    let mut opts = Vec::new();
    let mut top = footer;
    for i in (0..footer).rev() {
        let blank = lines[i].trim().is_empty();
        // Options sit right above the footer, under at most a wrapped label or two.
        if (blank && !opts.is_empty()) || (opts.is_empty() && footer - i > 6) {
            break;
        }
        if let Some(o) = tui::parse_option(&lines[i]) {
            opts.push(o);
            top = i;
        }
    }
    opts.reverse();
    if opts.len() < 2 || !opts.iter().any(|o| tui::is_yes_label(&o.label)) {
        return None;
    }
    if let Some(q) = (0..top).rev().find(|&i| is_question_at(lines, i)) {
        return Some((q, opts));
    }
    // A command taller than the pane can push the question off the top; Codex's "No" option
    // is specific enough to stand in for it.
    opts.iter()
        .any(|o| o.label.contains("tell Codex what to do differently"))
        .then(|| (top, opts))
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
    if tui::parse_option(&lines[q]).is_some() {
        return "Codex is asking for approval".to_string();
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
        let resume = ctx.resume.clone();
        anyhow::ensure!(resume.as_deref().is_none_or(crate::agents::valid_resume_id), "not a Codex thread id: {resume:?}");
        let extra = ctx.extra_args.iter().cloned();
        let mut args = Vec::new();
        if resume.is_some() {
            args.push(RESUME.to_string());
        }
        for o in config_overrides(&bin, &ctx.daemon_url, hooks_enabled()) {
            args.push("-c".to_string());
            args.push(o);
        }
        args.extend(sandbox_args(ctx.permissions.codex_sandbox));
        args.extend(extra);
        let prompt = ctx.initial_prompt.as_ref().filter(|p| !p.trim().is_empty());
        if resume.is_some() || prompt.is_some() {
            args.push("--".to_string());
        }
        args.extend(resume);
        let consumed = prompt.is_some();
        args.extend(prompt.cloned());
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
            // Fires before `approvals_reviewer="auto_review"` decides, and the payload doesn't
            // say whether the reviewer or the human will answer, so it can't mean NeedsApproval:
            // a request the reviewer approves would flash the approval banner. The screen parser
            // reports a real dialog the moment it renders (it overrides hook state at once).
            "PermissionRequest" => None,
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
    use crate::adapters::tui::testutil::snap;
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

    fn row_of(s: &ScreenSnapshot, prefix: &str) -> usize {
        s.lines.iter().position(|l| l.trim().starts_with(prefix)).unwrap()
    }

    #[test]
    fn wrapped_question_and_long_command_approvals() {
        // A narrow pane wraps the question title.
        let mut s = fx!("codex_approval_exec");
        let q = row_of(&s, "Would you like to");
        s.lines[q] = "  Would you like to run the".into();
        s.lines.insert(q + 1, "  following command?".into());
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(CodexAdapter.approve_bytes(Decision::Approve, &s), Some(b"y".to_vec()));

        // A long heredoc puts the options far below the question.
        let mut s = fx!("codex_approval_exec");
        let cmd = row_of(&s, "$ touch");
        s.lines[cmd] = "  $ python3 - <<'PY'".into();
        for i in 0..30 {
            s.lines.insert(cmd + 1 + i, format!("  print({i})"));
        }
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(u.detail.as_deref(), Some("Run: python3 - <<'PY'"));
        assert_eq!(CodexAdapter.approve_bytes(Decision::ApproveAlways, &s), Some(b"p".to_vec()));

        // Taller than the pane: the question scrolled off the top.
        let q = row_of(&s, "Would you like to");
        s.lines.drain(..=cmd + 5);
        assert!(!s.lines[..q.min(s.lines.len())].iter().any(|l| l.contains("Would you like")));
        let u = detect(&s, AgentState::Working).unwrap();
        assert_eq!(u.state, AgentState::NeedsApproval);
        assert_eq!(u.detail.as_deref(), Some("Codex is asking for approval"));
        assert_eq!(CodexAdapter.approve_bytes(Decision::Deny, &s), Some(vec![0x1b]));
    }

    #[test]
    fn options_far_above_the_footer_are_not_an_approval() {
        // An old Yes/No list in the transcript, then an unrelated "esc to cancel" footer.
        let mut s = fx!("codex_idle_done");
        let row = composer_row(&s.lines).unwrap();
        let old = ["  Would you like to run it?", "  1. Yes, proceed (y)", "  2. No (esc)"];
        for (i, l) in old.iter().enumerate() {
            s.lines.insert(row - 2 + i, l.to_string());
        }
        for i in 0..8 {
            s.lines.push(format!("  line {i}"));
        }
        s.lines.push("  esc to cancel".into());
        assert_eq!(approval_dialog(&s), None);
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
        // The auto-reviewer may answer it; only the on-screen dialog means NeedsApproval.
        assert_eq!(a.on_hook("PermissionRequest", &pre), None);
        assert_eq!(
            a.on_hook("Interrupt", &json!({})).unwrap().state,
            AgentState::Idle
        );
        assert_eq!(
            a.on_hook("Stop", &json!({"last_assistant_message":"ok"})),
            Some(StateUpdate::with(AgentState::Idle, "ok"))
        );
    }

    fn ctx(sandbox: CodexSandbox, prompt: Option<&str>) -> SpawnCtx {
        SpawnCtx {
            session_id: 3,
            cwd: "/tmp".into(),
            muster_bin: "/m".into(),
            daemon_url: "http://127.0.0.1:1".into(),
            session_token: "t".into(),
            session_dir: "/tmp".into(),
            extra_args: vec!["--model".into(), "x".into()],
            initial_prompt: prompt.map(str::to_string),
            permissions: Permissions {
                codex_sandbox: sandbox,
                claude_mode: ClaudeMode::Bypass,
            },
            resume: None,
        }
    }

    #[test]
    fn sandbox_profile_maps_to_flags() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            sandbox_args(CodexSandbox::ReadOnly),
            s(&["--sandbox", "read-only"])
        );
        assert_eq!(
            sandbox_args(CodexSandbox::WorkspaceWrite),
            s(&[
                "--sandbox",
                "workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=false"
            ])
        );
        assert_eq!(
            sandbox_args(CodexSandbox::WorkspaceWriteNetwork),
            s(&[
                "--sandbox",
                "workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=true"
            ])
        );
        assert_eq!(
            sandbox_args(CodexSandbox::FullAccess),
            s(&["--sandbox", "danger-full-access"])
        );
        for sb in [
            CodexSandbox::ReadOnly,
            CodexSandbox::WorkspaceWrite,
            CodexSandbox::WorkspaceWriteNetwork,
            CodexSandbox::FullAccess,
        ] {
            let spec = CodexAdapter.command(&ctx(sb, Some("hi"))).unwrap();
            assert_eq!(spec.program, "codex");
            let a = &spec.args;
            // Every profile gets automatic review while keeping its explicit sandbox; never
            // combine --sandbox with the conflicting --approve-for-me shorthand.
            let i = a.iter().position(|x| x == "--sandbox").unwrap();
            let e = a.iter().position(|x| x == "--model").unwrap();
            assert!(i < e);
            assert_eq!(&a[a.len() - 2..], &["--", "hi"]);
            for setting in ["approval_policy=\"on-request\"", "approvals_reviewer=\"auto_review\""] {
                assert_eq!(a.windows(2).filter(|w| w[0] == "-c" && w[1] == setting).count(), 1);
            }
            assert!(!a.iter().any(|x| x.starts_with("--ask-for-approval")
                || x == "-a"
                || x.starts_with("--dangerously")
                || x == "--approve-for-me"));
        }
        let spec = CodexAdapter
            .command(&ctx(CodexSandbox::WorkspaceWriteNetwork, None))
            .unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "sandbox_workspace_write.network_access=true")
        );
        assert!(!spec.consumed_initial_prompt);
    }

    #[test]
    fn resume_builds_the_resume_subcommand_with_every_flag_after_it() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let launch_from = |extra: &[&str], resume: Option<&str>, prompt: Option<&str>| {
            let mut c = ctx(CodexSandbox::WorkspaceWrite, prompt);
            c.extra_args = s(extra);
            c.resume = resume.map(str::to_string);
            CodexAdapter.command(&c).unwrap()
        };
        let launch = |extra: &[&str], prompt: Option<&str>| launch_from(extra, Some(id), prompt);
        let spec = launch(&["-m", "gpt-6-sol", "-c", "model_reasoning_effort=xhigh"], None);
        let a = &spec.args;
        assert_eq!(a[0], "resume", "the subcommand comes first: {a:?}");
        assert_eq!(&a[a.len() - 2..], &["--", id]);
        // Muster's own flags, the sandbox profile and the model/effort all still apply.
        assert!(a.windows(2).any(|w| w[0] == "-c" && w[1].starts_with("notify=")));
        assert!(a.windows(2).any(|w| w[0] == "-c" && w[1].starts_with("mcp_servers.muster.url=")));
        assert!(a.windows(2).any(|w| w == ["--sandbox", "workspace-write"]));
        assert!(a.windows(2).any(|w| w == ["-c", "sandbox_workspace_write.network_access=false"]));
        assert!(a.windows(2).any(|w| w == ["-m", "gpt-6-sol"]) && a.windows(2).any(|w| w == ["-c", "model_reasoning_effort=xhigh"]));
        assert!(a.windows(2).any(|w| w == ["-c", "approval_policy=\"on-request\""]));
        assert!(!a.iter().any(|x| x.starts_with("--resume")), "never passed to Codex as a flag");
        assert!(!spec.consumed_initial_prompt);
        assert_eq!(crate::adapters::resumed_in_launch(SessionKind::Codex, a), Some(Some(id.to_string())));
        // An initial prompt follows the thread id.
        let spec = launch(&["-i", "a.png", "b.png"], Some("go on"));
        assert_eq!(spec.args[0], "resume");
        assert_eq!(&spec.args[spec.args.len() - 6..], &["-i", "a.png", "b.png", "--", id, "go on"]);
        assert!(spec.consumed_initial_prompt);
        assert_eq!(crate::adapters::resumed_in_launch(SessionKind::Codex, &spec.args), Some(Some(id.to_string())));
        // Without a thread nothing changes.
        let spec = launch_from(&["--model", "x"], None, Some("hi"));
        assert_ne!(spec.args[0], "resume");
        assert_eq!(&spec.args[spec.args.len() - 4..], &["--model", "x", "--", "hi"]);
        assert_eq!(crate::adapters::resumed_in_launch(SessionKind::Codex, &spec.args), None);
        let spec = launch_from(&[], None, None);
        assert!(!spec.args.iter().any(|x| x == "resume" || x == "--"));
        // A bad id never reaches a command line.
        let mut c = ctx(CodexSandbox::ReadOnly, None);
        c.resume = Some("--sandbox".into());
        assert!(CodexAdapter.command(&c).is_err());
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
        let without_hooks = config_overrides("/m", "http://x", false);
        assert_eq!(without_hooks.len(), 6);
        assert_eq!(without_hooks[4], "approval_policy=\"on-request\"");
        assert_eq!(without_hooks[5], "approvals_reviewer=\"auto_review\"");
    }
}
