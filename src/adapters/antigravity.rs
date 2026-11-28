//! Antigravity launch and status, verified against agy 1.5.13 screens on 23 Nov 2026.

use super::{Adapter, CommandSpec, ScreenSnapshot, SpawnCtx, claude, resumed_in_launch, tui};
use crate::agy_guard::Config as GuardConfig;
use crate::model::{AgentState, AntigravityMode, Decision, SessionKind, StateUpdate};
use serde_json::{Value, json};
use std::path::Path;

pub struct AntigravityAdapter;
pub(crate) const LAST_STATE: &str = "antigravity-state-last";

/// The helper writes at most three short fields. Ignore links, devices and oversized files
/// when checking its deduplication record or adopting a run from an older backend.
pub(crate) fn read_last_state(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    file.take(513).read_to_end(&mut data).ok()?;
    (data.len() <= 512).then_some(data)
}

const PENDING: &str = "Antigravity is asking for approval";

pub(crate) const AUTHENTICATING: &str = "Antigravity is authenticating";
pub(crate) const SIGN_IN: &str = "Antigravity may need a sign-in: check the terminal";

/// The delayed sign-in hint is cleared by a callback; a visible dialog is cleared by the screen.
pub(crate) fn is_screen_dialog(state: AgentState, detail: Option<&str>) -> bool {
    (state == AgentState::AskingQuestion && detail != Some(SIGN_IN))
        || (state == AgentState::NeedsApproval && detail.is_some_and(|d| d != PENDING))
}

const TRUST_TITLE: &str = "Do you trust the contents of this project?";
const SETTINGS_ERROR: &str = "Settings Error";
const APPROVAL_TITLES: &[&str] = &[
    "Run this command?",
    "Allow access to this URL?",
    "Allow calling this tool?",
    "Accept this file edit?",
    "Allow creation of this file?",
];
const WORKING: &[&str] = &["esc to cancel", "esc to interrupt"];
const IDLE_HINTS: &[&str] = &["shift+tab to cycle", "? for shortcuts"];

pub fn mode_args(mode: AntigravityMode) -> Vec<String> {
    vec![
        match mode {
            AntigravityMode::Plan => "--mode=plan",
            AntigravityMode::AcceptEdits => "--mode=accept-edits",
            AntigravityMode::Auto => "--mode=auto",
            AntigravityMode::FullAccess => "--dangerously-skip-permissions",
        }
        .into(),
    ]
}

/// Muster's own MCP tools, and only those, run without an approval prompt, as for Claude
/// (`claude::MUSTER_TOOLS_RULE`). The user's ask and deny rules still come first in agy.
pub const MUSTER_TOOLS_RULE: &str = "mcp(muster/*)";

/// agy 1.5.13 gives the model no tool per MCP tool: it lists each server's tool names and one
/// generic `call_mcp_tool`. Written as `.agents/AGENTS.md` in the private workspace, which agy
/// reads as rules for every tab, a named agent's or not.
pub const TOOLS_INSTRUCTION: &str = "# Muster\n\n\
This session runs in a Muster tab. Muster's tools (`reply`, `ask_agent`, `tell_agent`, `list_agents`, `whoami`, \
`note_read`, `note_append`, `post_question` and the others listed for the server) are on the MCP server `muster`. \
Call them with `call_mcp_tool`: ServerName `muster`, ToolName the tool's name, Arguments its arguments (read the \
tool's schema file first, as your MCP instructions describe).\n\n\
A prompt that starts with `[muster msg <id> from <sender> origin=<origin> hop=<n>]` came from another agent or \
session through Muster, not from the human typing. Antigravity doesn't pass your final message back to the sender, \
so answer it with the `reply` tool (`msg_id` from the header) once you have the answer: one reply per message.\n";

/// agy's own settings file, whose `permissions.allow` rules a session keeps.
pub fn user_settings_file() -> std::path::PathBuf {
    crate::paths::home_dir().join(".gemini/antigravity-cli/settings.json")
}

/// The user's `permissions.allow` rules. Checked with agy 1.5.13: a `permissions.allow` list in
/// `--settings` replaces that list for the session (its `ask` and `deny` lists still apply), so
/// Muster adds its rule to the user's rather than in their place. A missing, unreadable or
/// malformed file gives none (agy reports a malformed one itself).
pub(crate) fn user_allow_rules(file: &Path) -> Vec<String> {
    read_json(file)
        .and_then(|v| v.pointer("/permissions/allow").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect()
}

/// A JSON file of the user's: a regular file of at most 4 MiB (never blocks on a FIFO).
pub(crate) fn read_json(file: &Path) -> Option<Value> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(file).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut data = Vec::new();
    file.take(4 << 20).read_to_end(&mut data).ok()?;
    serde_json::from_slice(&data).ok()
}

/// MCP config files that could name another server `muster`: agy keeps a duplicate name for the
/// server already running, so `MUSTER_TOOLS_RULE` would allow that one instead.
pub fn other_server_files(cwd: &Path) -> [std::path::PathBuf; 2] {
    [crate::paths::home_dir().join(".gemini/config/mcp_config.json"), cwd.join(".agents/mcp_config.json")]
}

/// Any of `files` configures a server named `muster`. An unreadable file counts as none.
pub(crate) fn names_muster(files: &[std::path::PathBuf]) -> bool {
    files.iter().filter_map(|f| read_json(f)).any(|v| v.pointer("/mcpServers/muster").is_some())
}

/// The session's private settings: the title callback, and with `allow` the user's allow rules
/// plus `MUSTER_TOOLS_RULE`.
pub fn settings_json(bin: &Path, allow: Option<Vec<String>>) -> Value {
    let bin = claude::sh_quote(&bin.to_string_lossy());
    let mut v = json!({"title":{"type":"command","command":format!("{bin} hook antigravity state")}});
    if let Some(mut rules) = allow {
        if !rules.iter().any(|r| r == MUSTER_TOOLS_RULE) {
            rules.push(MUSTER_TOOLS_RULE.into());
        }
        v["permissions"] = json!({ "allow": rules });
    }
    v
}

pub fn mcp_json(url: &str, token: &str) -> Value {
    json!({"mcpServers":{"muster":{
        "serverUrl": format!("{}/mcp", url.trim_end_matches('/')),
        "headers": {"Authorization": format!("Bearer {token}")}, "timeoutSeconds": 1900
    }}})
}

fn on(var: &str) -> bool {
    !matches!(
        std::env::var(var)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

fn rules_file(setting: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    match setting {
        Some(v) if v == "off" => None,
        Some(v) => Some(v.into()),
        None => Some(crate::paths::home_dir().join(".claude/CLAUDE.md")),
    }
}

// agy 1.5.13 truncates a rules file at 24,000 bytes. Reserve room for the tools note.
const MAX_RULES: u64 = 24_000 - TOOLS_INSTRUCTION.len() as u64 - 1;

fn claude_rules(args: &[String], source: Option<&Path>) -> anyhow::Result<Option<String>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = source else { return Ok(None) };
    let flags = super::parse_extra_args(SessionKind::Antigravity, args)?;
    let model = flags.iter().rev().find(|(name, _)| *name == "--model");
    if !model.is_some_and(|(_, values)| values[0].starts_with("claude-")) {
        return Ok(None);
    }
    let file = match std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    anyhow::ensure!(file.metadata()?.is_file(), "Antigravity rules must be a regular file");
    let mut text = String::new();
    file.take(MAX_RULES + 1).read_to_string(&mut text)?;
    anyhow::ensure!(text.len() as u64 <= MAX_RULES, "Antigravity rules exceed the workspace rules budget");
    Ok(Some(text))
}

/// Build the credential file outside the previously shared tree. Only publish the
/// completed directory once the old run's entries have been checked and removed.
/// With `guards`, also the owner's guards (src/agy_guard.rs): their list in the session dir, out
/// of the agent's workspace, and an agy hook file that runs them through `muster hook antigravity
/// guard` (agy 1.5.13 loads `.agents/hooks.json` from an `--add-dir` folder too).
fn prepare_workspace(ctx: &SpawnCtx, mcp: bool, guards: &GuardConfig, rules: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let workspace = ctx.session_dir.join(if mcp { "antigravity-workspace" } else { "antigravity-rules" });
    let config = workspace.join(".agents");
    let guard_file = ctx.session_dir.join(crate::agy_guard::CONFIG);
    for path in [&workspace, &config, &config.join("mcp_config.json"), &config.join("AGENTS.md"), &config.join("hooks.json"), &guard_file] {
        crate::agents::refuse_symlink(path)?;
    }
    // The previous run's list goes first: a run without guards never reads an old one.
    match std::fs::remove_file(&guard_file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let stage = ctx
        .session_dir
        .join(format!(".antigravity-{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new().mode(0o700).create(&stage)?;
    let install = || -> anyhow::Result<()> {
        let mut instructions = String::new();
        if mcp {
            let server = mcp_json(&ctx.daemon_url, &ctx.session_token);
            crate::paths::write_private(
                &stage.join(".agents/mcp_config.json"),
                &serde_json::to_vec_pretty(&server)?,
            )?;
            instructions.push_str(TOOLS_INSTRUCTION);
            if rules.is_some() { instructions.push('\n'); }
        }
        if let Some(text) = rules { instructions.push_str(text); }
        crate::paths::write_private(&stage.join(".agents/AGENTS.md"), instructions.as_bytes())?;
        if mcp && !(guards.guards.is_empty() && guards.stop.is_empty()) {
            crate::paths::write_private(&guard_file, &serde_json::to_vec_pretty(guards)?)?;
            let hooks = crate::agy_guard::hooks_json(&ctx.muster_bin, &guard_file, guards);
            crate::paths::write_private(&stage.join(".agents/hooks.json"), &serde_json::to_vec_pretty(&hooks)?)?;
        }
        match std::fs::remove_dir_all(&workspace) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        std::fs::rename(&stage, &workspace)?;
        Ok(())
    };
    if let Err(e) = install() {
        let _ = std::fs::remove_dir_all(stage);
        return Err(e);
    }
    Ok(workspace)
}

/// Launches with Muster's MCP workspace check the server in `/mcp` before the first prompt.
pub fn has_mcp_workspace(args: &[String]) -> bool {
    args.iter().any(|a| a.strip_prefix("--add-dir=").is_some_and(|d| Path::new(d).ends_with("antigravity-workspace")))
}

/// agy's `/mcp` panel lists Muster's server as loaded ("✓ muster  Tools: ...", agy 1.5.13).
pub fn mcp_panel_loaded(screen: &ScreenSnapshot) -> bool {
    let lines = active_dialog_lines(&screen.lines);
    lines.iter().any(|l| l.trim() == "MCP Servers")
        && lines.iter().any(|l| l.trim_start_matches([' ', '>']).starts_with("✓ muster  Tools:"))
}

/// agy's screen before any turn: no submitted prompt ("> ...") above the input box.
pub fn is_startup_screen(screen: &ScreenSnapshot) -> bool {
    let lines = &screen.lines;
    let Some(bottom) = tui::rfind(lines, tui::is_solid_rule) else { return false };
    let Some(top) = tui::rfind(&lines[..bottom], tui::is_solid_rule) else { return false };
    !lines[..top].iter().any(|l| l.starts_with("> ") || l.trim_end() == ">")
}

fn launch(ctx: &SpawnCtx, settings: bool, mcp: bool, user_settings: &Path, other_servers: &[std::path::PathBuf], guards: &GuardConfig, rules_source: Option<&Path>) -> anyhow::Result<CommandSpec> {
    let mut args = Vec::new();
    if settings {
        let file = ctx.session_dir.join("antigravity-settings.json");
        crate::agents::refuse_symlink(&file)?;
        // Another server named `muster` would get the rule: then each call asks, as before.
        let allow = (mcp && !names_muster(other_servers)).then(|| user_allow_rules(user_settings));
        crate::paths::write_private(
            &file,
            &serde_json::to_vec_pretty(&settings_json(&ctx.muster_bin, allow))?,
        )?;
        args.push(format!("--settings={}", file.display()));
        // The first callback matters even if it repeats the last state of the previous run.
        match std::fs::remove_file(ctx.session_dir.join(LAST_STATE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    let rules = claude_rules(&ctx.extra_args, rules_source)?;
    if mcp || rules.is_some() {
        let workspace = prepare_workspace(ctx, mcp, guards, rules.as_deref())?;
        args.push(format!("--add-dir={}", workspace.display()));
    }
    args.extend(mode_args(ctx.permissions.antigravity_mode));
    args.extend_from_slice(&ctx.extra_args);
    let prompt = ctx.initial_prompt.as_ref().filter(|p| {
        !p.trim().is_empty()
            && resumed_in_launch(SessionKind::Antigravity, &ctx.extra_args).is_none()
            && !mcp
    });
    if let Some(p) = prompt {
        args.push(format!("--prompt-interactive={p}"));
    }
    Ok(CommandSpec {
        program: "agy".into(),
        args,
        consumed_initial_prompt: prompt.is_some(),
        env: [
            ("AGY_CLI_DISABLE_AUTO_UPDATE", "true"),
            ("AGY_CLI_HIDE_ACCOUNT_INFO", "true"),
            ("AGY_CLI_OSC7501", "off"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
    })
}

/// The rules around agy's input box, walking up from `bottom`, the last rule above the footer.
/// agy 1.5.13 draws status rows in their own ruled blocks between the box and the footer
/// (`  ● [00:52:20] 25m timer running` while a timer runs). Such a block has no `>` prompt
/// line and only indented rows that start with a symbol; anything else (an empty block in
/// mid-redraw, a box whose `>` line is unreadable, a transcript) stops the walk, so a lost
/// border never makes an older prompt read as the box.
fn composer_rules(lines: &[String], mut bottom: usize) -> Option<(usize, usize, bool)> {
    const MAX_STATUS_BLOCKS: usize = 4;
    let status_block = |block: &[String]| {
        let rows: Vec<&String> = block.iter().filter(|l| !l.trim().is_empty()).collect();
        (1..=3).contains(&rows.len())
            && rows.iter().all(|l| {
                l.starts_with(char::is_whitespace)
                    && l.trim_start().chars().next().is_some_and(|c| !c.is_ascii() && !c.is_alphanumeric())
            })
    };
    let mut top = tui::rfind(&lines[..bottom], tui::is_solid_rule)?;
    let mut walked = false;
    for _ in 0..MAX_STATUS_BLOCKS {
        let block = &lines[top + 1..bottom];
        if block.iter().any(|l| l.starts_with('>')) || !status_block(block) {
            break;
        }
        (bottom, walked) = (top, true);
        top = tui::rfind(&lines[..bottom], tui::is_solid_rule)?;
    }
    Some((top, bottom, walked))
}

// The composer and modal content are separated from past turns by a solid rule.
fn active_dialog_lines(lines: &[String]) -> &[String] {
    tui::rfind(lines, |l| !l.starts_with(char::is_whitespace) && tui::is_solid_rule(l)).map_or(lines, |i| &lines[i + 1..])
}

fn manual_dialog(lines: &[String]) -> Option<&'static str> {
    if lines.iter().any(|l| l.trim() == TRUST_TITLE)
        && lines
            .iter()
            .any(|l| l.trim_start_matches([' ', '>']) == "Yes, I trust this folder")
        && lines
            .iter()
            .any(|l| l.contains("Navigate") && l.contains("enter Confirm"))
    {
        Some("Antigravity asks whether to trust this folder: answer in the terminal")
    } else if lines.iter().any(|l| l.contains(SETTINGS_ERROR))
        && lines
            .iter()
            .any(|l| tui::parse_option(l).is_some() || l.contains("Continue with defaults"))
    {
        Some("Antigravity reports a settings error: answer in the terminal")
    } else {
        None
    }
}

struct ApprovalDialog {
    answerable: bool,
    detail: String,
    options: Vec<tui::MenuOption>,
}

fn approval_dialog(lines: &[String], cols: u16) -> Option<ApprovalDialog> {
    let q = tui::rfind(lines, |l| {
        let t = l.trim();
        APPROVAL_TITLES.contains(&t)
            || (t.ends_with('?')
                && ["Run this", "Allow ", "Accept this"]
                    .iter()
                    .any(|prefix| t.starts_with(prefix)))
    })?;
    let options = tui::options_in(lines, q + 1, q + 12);
    if options.len() < 2
        || !options.iter().any(|o| o.selected)
        || !options.iter().any(|o| tui::is_yes_label(&o.label))
    {
        return None;
    }
    let title = lines[q].trim();
    let before = &lines[..q];
    const MARKER: &str = "Requesting permission for:";
    let start = before.iter().position(|l| l.trim_end() == MARKER);
    let body = start.map_or(before, |i| &before[i + 1..]);
    let ambiguous = before.iter().filter(|l| l.trim() == MARKER).count() > 1;
    let incomplete = (title.starts_with("Run this") && (start.is_none() || body.iter().all(|l| l.trim().is_empty())))
        || body.iter().any(|l| {
            let l = l.trim();
            l.starts_with('…') || l.starts_with("...") || l.ends_with('…') || l.ends_with("...") || l.contains("(truncated)") || l.contains("… +")
        });
    // Snapshots trim the right margin. Preserve explicit command lines, fold a word
    // that did not fit, and rejoin a token that ran all the way to the terminal edge.
    let mut text = String::new();
    for (i, row) in body.iter().enumerate() {
        if i > 0 {
            let used = body[i - 1].chars().count();
            let word = row.split_whitespace().next().unwrap_or("").chars().count();
            if used == usize::from(cols) && !row.starts_with(char::is_whitespace) {
                // Hard wrap in the middle of a token.
            } else if used + 1 + word > usize::from(cols) {
                text.push(' ');
            } else {
                text.push('\n');
            }
        }
        text.push_str(row.strip_prefix("   ").unwrap_or(row));
    }
    let reason = lines[q + 1..]
        .iter()
        .find_map(|l| l.trim().strip_prefix("Reason: "));
    let detail = if ambiguous {
        format!("{title} Multiple permission markers are visible. Check the full request in the terminal.")
    } else if incomplete {
        format!("{title} The command is cut off. Check the full request in the terminal.")
    } else if !text.trim().is_empty() {
        format!("{title} {}", text.trim())
    } else if let Some(reason) = reason {
        format!("{title} ({reason})")
    } else {
        title.to_string()
    };
    Some(ApprovalDialog {
        answerable: !ambiguous && !incomplete && detail != title,
        detail,
        options,
    })
}

impl Adapter for AntigravityAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::Antigravity
    }

    fn command(&self, ctx: &SpawnCtx) -> anyhow::Result<CommandSpec> {
        launch(
            ctx,
            on("MUSTER_ANTIGRAVITY_SETTINGS"),
            on("MUSTER_ANTIGRAVITY_MCP"),
            &user_settings_file(),
            &other_server_files(&ctx.cwd),
            &crate::agy_guard::load(&ctx.cwd),
            rules_file(std::env::var_os("MUSTER_ANTIGRAVITY_RULES")).as_deref(),
        )
    }

    fn on_hook(&self, event: &str, payload: &Value) -> Option<StateUpdate> {
        use AgentState::*;
        if event != "state" {
            return None;
        }
        if payload["tool_confirmation_pending"] == true {
            return Some(StateUpdate::with(NeedsApproval, PENDING));
        }
        Some(match payload["agent_state"].as_str()? {
            "working" | "thinking" | "tool_use" => StateUpdate::new(Working),
            "idle" => StateUpdate::new(Idle),
            "authenticating" => StateUpdate::with(Starting, AUTHENTICATING),
            "error" => StateUpdate::with(Error, "Antigravity reported an error"),
            _ => return None,
        })
    }

    fn detect_from_screen(
        &self,
        screen: &ScreenSnapshot,
        current: AgentState,
    ) -> Option<StateUpdate> {
        let lines = &screen.lines;
        let dialog = active_dialog_lines(lines);
        if let Some(detail) = manual_dialog(dialog) {
            return Some(StateUpdate::with(AgentState::AskingQuestion, detail));
        }
        if let Some(request) = approval_dialog(dialog, screen.cols) {
            return Some(StateUpdate::with(AgentState::NeedsApproval, request.detail));
        }
        let tail_has = |n, hints: &[&str]| {
            lines
                .iter()
                .rev()
                .take(n)
                .any(|l| hints.iter().any(|h| l.contains(h)))
        };
        if tail_has(15, WORKING) {
            Some(StateUpdate::new(AgentState::Working))
        } else if current != AgentState::Error && tail_has(8, IDLE_HINTS) {
            Some(StateUpdate::new(AgentState::Idle))
        } else {
            None
        }
    }

    fn approve_bytes(&self, decision: Decision, screen: &ScreenSnapshot) -> Option<Vec<u8>> {
        let dialog = active_dialog_lines(&screen.lines);
        if manual_dialog(dialog).is_some() {
            return None;
        }
        let request = approval_dialog(dialog, screen.cols)?;
        if !request.answerable {
            return None;
        }
        let opts = request.options;
        if decision == Decision::ApproveAlways {
            return opts
                .iter()
                .find(|o| {
                    o.digit == '2'
                        && o.label
                            .starts_with("Yes, and always allow in this conversation")
                })
                .map(|o| tui::option_keys(o, false));
        }
        tui::choose(&opts, decision)
            .map(|o| tui::option_keys(o, false))
            .or_else(|| (decision == Decision::Deny).then(|| b"n".to_vec()))
    }

    fn composer_text(&self, screen: &ScreenSnapshot) -> Option<String> {
        let lines = &screen.lines;
        let bottom = tui::rfind(lines, tui::is_solid_rule)?;
        // A panel can leave a complete, inactive composer visible above its controls. agy
        // 1.5.13 drops "? for shortcuts" while the box holds text, leaving the right-aligned
        // status ("auto · Gemini 4 Neon · high") alone on the line under it.
        let status_only = lines.get(bottom + 1).map(String::as_str).filter(|l| {
            let t = l.trim_start();
            l.len() - t.len() >= 8 && t.split(" · ").count() >= 2
        });
        // A typed slash command opens agy's command menu under the box instead.
        let menu = lines[bottom + 1..].iter().take(12).any(|l| l.contains("enter Select"))
            && lines.get(bottom + 1).is_some_and(|l| l.trim_start_matches([' ', '>']).starts_with('/'));
        let footer = lines[bottom + 1..]
            .iter()
            .find_map(|l| l.trim_start().strip_prefix("? for shortcuts"))
            .or(status_only)
            .or(menu.then_some(""))?;
        let (top, bottom, walked) = composer_rules(lines, bottom)?;
        // Muster's copy of the screen can lose the box's top border (24 Nov: agy redrew a wrapped
        // line with a file link over it). The border found is then an older one, above an earlier
        // prompt in the transcript, and that prompt and its answer read as unsent text that held
        // the queue for hours. The box starts at its last prompt line, `>` in column 0: a draft's
        // further lines are indented, and so is everything agy prints between prompts.
        let start = (top + 1..bottom).rev().find(|&i| lines[i].starts_with('>')).unwrap_or(top + 1);
        let (first, rest) = lines[start..bottom].split_first()?;
        // Above status rows, the box is only taken as it is drawn: a draft's further lines are
        // indented or blank (a paragraph break), and its last line is not blank. A submitted
        // prompt in the transcript is followed by agy's column-0 `○`/`●` rows or by the blank line
        // before the next box, so a box whose own `>` line was lost never borrows the prompt of
        // the turn above.
        if walked
            && (rest.iter().any(|l| !l.trim().is_empty() && !l.starts_with(char::is_whitespace))
                || rest.last().is_some_and(|l| l.trim().is_empty()))
        {
            return None;
        }
        let first = if first.trim_end() == ">" {
            ""
        } else {
            first.strip_prefix("> ")?
        };
        let text = std::iter::once(first)
            .chain(rest.iter().map(|s| s.trim_end()))
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim_end();
        let mode = footer.split('·').next()?.trim();
        if matches!((mode, text),
            ("auto", "Auto mode: edits & safe commands auto-approved (shift+tab to cycle)") |
            ("accept-edits", "Accept-edits mode: file edits auto-approved (shift+tab to cycle)") |
            ("plan", "Plan mode: research & plan only (shift+tab to cycle)")) {
            return Some(String::new());
        }
        Some(text.to_string())
    }

    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x1b]
    }
}

#[cfg(test)]
mod tests {
    use super::tui::testutil::snap;
    use super::*;
    use crate::model::Permissions;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn dialogs_require_an_explicit_choice() {
        use Decision::*;
        // File prompts and settings errors were not in the captures. Exercise their known labels.
        for manual in [
            include_str!("fixtures/antigravity_trust.txt"),
            "Settings Error\nUnable to read settings.json\n1. Exit and fix manually\n2. Continue with defaults this session",
        ] {
            let screen = snap(manual);
            assert_eq!(
                AntigravityAdapter
                    .detect_from_screen(&screen, AgentState::Idle)
                    .unwrap()
                    .state,
                AgentState::AskingQuestion
            );
            for d in [Approve, ApproveAlways, Deny] {
                assert_eq!(AntigravityAdapter.approve_bytes(d, &screen), None);
            }
        }
        for (text, deny, always) in [
            (
                include_str!("fixtures/antigravity_approval_command.txt"),
                b'4',
                Some(vec![b'2']),
            ),
            (
                "Create file\nsrc/alpha.txt\nAllow creation of this file?\n> 1. Yes, allow creation\n  2. No, deny creation",
                b'2',
                None,
            ),
            (
                "src/alpha.txt\nAccept this file edit?\n> 1. Yes, accept this change\n  2. No, reject this change",
                b'2',
                None,
            ),
        ] {
            let screen = snap(text);
            assert_eq!(
                AntigravityAdapter
                    .detect_from_screen(&screen, AgentState::Working)
                    .unwrap()
                    .state,
                AgentState::NeedsApproval
            );
            assert_eq!(
                AntigravityAdapter.approve_bytes(Approve, &screen),
                Some(vec![b'1'])
            );
            assert_eq!(
                AntigravityAdapter.approve_bytes(Deny, &screen),
                Some(vec![deny])
            );
            assert_eq!(
                AntigravityAdapter.approve_bytes(ApproveAlways, &screen),
                always
            );
        }
        let no_deny = snap("Allow calling this tool?\nReason: read file\n> 1. Yes\n2. Yes, always");
        assert_eq!(
            AntigravityAdapter.approve_bytes(Deny, &no_deny),
            Some(b"n".to_vec())
        );
        assert_eq!(
            AntigravityAdapter
                .detect_from_screen(&no_deny, AgentState::Idle)
                .unwrap()
                .detail
                .as_deref(),
            Some("Allow calling this tool? (read file)")
        );
    }

    #[test]
    fn incomplete_or_ambiguous_approval_is_terminal_only() {
        let cmd = include_str!("fixtures/antigravity_approval_command.txt");
        for (screen, reason) in [
            (cmd.replace("   touch beta.txt && ls", "   printf alpha\n   Requesting permission for:\n   ls"), "Multiple"),
            (cmd.replace("Requesting permission for:", "  Requesting permission for:"), "cut off"),
            (cmd.replace("   touch beta.txt && ls", "   ls…"), "cut off"),
            (cmd.replace("Requesting permission for:", ""), "cut off"),
            (cmd.replace("   touch beta.txt && ls", ""), "cut off"),
        ] {
            let screen = snap(&screen);
            let update = AntigravityAdapter.detect_from_screen(&screen, AgentState::Idle).unwrap();
            assert!(update.detail.unwrap().contains(reason));
            for d in [Decision::Approve, Decision::ApproveAlways, Decision::Deny] {
                assert!(AntigravityAdapter.approve_bytes(d, &screen).is_none());
            }
        }
    }

    #[test]
    fn approval_identity_survives_a_phone_rewrap() {
        let cmd = include_str!("fixtures/antigravity_approval_command.txt");
        let request = |s: &ScreenSnapshot| AntigravityAdapter.detect_from_screen(s, AgentState::NeedsApproval).unwrap();
        let wide = request(&snap(cmd));
        for (cols, text) in [(18, "   touch beta.txt\n   && ls"), (12, "   touch bet\na.txt && ls")] {
            let mut narrow = snap(&cmd.replace("   touch beta.txt && ls", text));
            narrow.cols = cols;
            assert_eq!(request(&narrow), wide);
        }
        assert_ne!(request(&snap(&cmd.replace("beta.txt", "alpha.txt"))), wide);
        let multiline = snap(&cmd.replace("   touch beta.txt && ls", "   touch beta.txt\n   ls"));
        assert_ne!(request(&multiline), request(&snap(&cmd.replace(" && ls", " ls"))));
    }

    #[test]
    fn screen_markers() {
        use AgentState::*;
        let idle = include_str!("fixtures/antigravity_idle_auto.txt");
        for (screen, current, expected) in [
            (idle, Starting, Some(Idle)),
            (idle, Working, Some(Idle)),
            (idle, Error, None),
            (
                include_str!("fixtures/antigravity_working.txt"),
                Idle,
                Some(Working),
            ),
            ("Esc to cancel", Starting, None),
            ("alpha", Working, None),
        ] {
            assert_eq!(
                AntigravityAdapter
                    .detect_from_screen(&snap(screen), current)
                    .map(|u| u.state),
                expected
            );
        }
        assert_eq!(AntigravityAdapter.interrupt_bytes(), [0x1b]);
    }

    #[test]
    fn composer_and_closed_dialogs() {
        let idle = include_str!("fixtures/antigravity_idle_auto.txt");
        let placeholder = "Auto mode: edits & safe commands auto-approved (shift+tab to cycle)";
        for (row, expected) in [
            (placeholder, ""),
            ("Plan mode: research & plan only (shift+tab to cycle)", "Plan mode: research & plan only (shift+tab to cycle)"),
            (
                "Auto mode: my draft (shift+tab to cycle)",
                "Auto mode: my draft (shift+tab to cycle)",
            ),
            ("alpha", "alpha"),
            ("alpha\n  beta", "alpha\n  beta"),
            ("", ""),
        ] {
            let screen = snap(&idle.replace(placeholder, row));
            assert_eq!(
                AntigravityAdapter.composer_text(&screen).as_deref(),
                Some(expected)
            );
        }
        let plan = "Plan mode: research & plan only (shift+tab to cycle)";
        let screen = snap(&idle.replace(placeholder, plan).replace("auto ·", "plan ·"));
        assert_eq!(AntigravityAdapter.composer_text(&screen).as_deref(), Some(""));
        // The empty box in accept-edits mode, as agy 1.5.13 draws it; under another mode it is text.
        let edits = include_str!("fixtures/antigravity_idle_accept_edits.txt");
        assert_eq!(AntigravityAdapter.composer_text(&snap(edits)).as_deref(), Some(""));
        let edits_text = "Accept-edits mode: file edits auto-approved (shift+tab to cycle)";
        assert_eq!(AntigravityAdapter.composer_text(&snap(&idle.replace(placeholder, edits_text))).as_deref(), Some(edits_text));
        let draft = placeholder.replace("edits &", "edits  &");
        assert_eq!(AntigravityAdapter.composer_text(&snap(&idle.replace(placeholder, &draft))).as_deref(), Some(draft.as_str()));
        assert!(AntigravityAdapter.clear_input_bytes().is_none());
        // The box's top border lost in Muster's copy of the screen (24 Nov, the writer's tab): the
        // border found is the one above the previous prompt, but the box is still empty, and a
        // draft in it still reads as one.
        let garbled = include_str!("fixtures/antigravity_garbled_border.txt");
        assert_eq!(AntigravityAdapter.composer_text(&snap(garbled)).as_deref(), Some(""));
        let typed_in = garbled.replace("\n>\n", "\n> my draft\n  second line\n");
        assert_eq!(AntigravityAdapter.composer_text(&snap(&typed_in)).as_deref(), Some("my draft\n  second line"));
        let panel = snap(include_str!("fixtures/antigravity_mcp_panel.txt"));
        assert!(AntigravityAdapter.composer_text(&panel).is_none());
        // A typed `/clear` opens agy's command menu under the box (agy 1.5.13).
        let menu = include_str!("fixtures/antigravity_slash_menu.txt");
        assert_eq!(AntigravityAdapter.composer_text(&snap(menu)).as_deref(), Some("/clear"));
        assert!(AntigravityAdapter.composer_text(&snap(&menu.replace("enter Select", "enter Pick"))).is_none());
        // Startup screens hold no turn; one with a turn is not one.
        assert!(is_startup_screen(&snap(idle)));
        assert!(is_startup_screen(&snap(include_str!("fixtures/antigravity_typed_prompt.txt"))));
        assert!(!is_startup_screen(&snap(menu)));
        assert!(!is_startup_screen(&panel));
        assert!(mcp_panel_loaded(&panel) && !mcp_panel_loaded(&snap(idle)));
        assert!(!mcp_panel_loaded(&snap(&include_str!("fixtures/antigravity_mcp_panel.txt").replace("✓ muster", "✗ muster"))));
        // A prompt Muster typed, as agy 1.5.13 shows it: no "? for shortcuts" under the box.
        let typed = include_str!("fixtures/antigravity_typed_prompt.txt");
        assert_eq!(
            AntigravityAdapter.composer_text(&snap(typed)).as_deref(),
            Some("[muster msg 1 from human origin=human hop=0]\n  What is 17 plus 25? Answer with the number.")
        );
        let status = "                                                                                             auto · Gemini 4 Neon · high";
        assert!(typed.contains(status));
        for not_a_box in [status.trim_start(), "MCP Servers", ""] {
            assert!(AntigravityAdapter.composer_text(&snap(&typed.replace(status, not_a_box))).is_none(), "{not_a_box:?}");
        }
        let command = include_str!("fixtures/antigravity_approval_command.txt");
        for closed in [
            include_str!("fixtures/antigravity_approved.txt"),
            include_str!("fixtures/antigravity_denied.txt"),
        ] {
            let screen = snap(&format!("{command}\n{closed}"));
            assert_eq!(
                AntigravityAdapter.composer_text(&screen).as_deref(),
                Some("")
            );
            assert!(
                AntigravityAdapter
                    .approve_bytes(Decision::Approve, &screen)
                    .is_none()
            );
        }
        let persistent_only = command.replace(
            "2. Yes, and always allow in this conversation",
            "2. More details",
        );
        assert!(
            AntigravityAdapter
                .approve_bytes(Decision::ApproveAlways, &snap(&persistent_only))
                .is_none()
        );
    }

    #[test]
    fn composer_above_status_rows() {
        // agy 1.5.13 with a timer running: the timer row sits in its own ruled block between
        // the box and the footer (26 Nov, an agent that set a 25 minute timer stopped taking
        // messages because the box read as missing).
        let timer = include_str!("fixtures/antigravity_timer_row.txt");
        let row = "  ● [00:52:20] 25m timer running";
        let rule = "─".repeat(120);
        let read = |text: &str| AntigravityAdapter.composer_text(&snap(text));
        assert_eq!(read(timer).as_deref(), Some(""));
        let draft = timer.replace("\n>\n", "\n> my draft\n  second line\n");
        assert_eq!(read(&draft).as_deref(), Some("my draft\n  second line"));
        // Two status rows: in one block, and each in its own block.
        let task = "  ● 1 task running (/tasks)";
        assert_eq!(read(&timer.replace(row, &format!("{row}\n{task}"))).as_deref(), Some(""));
        let stacked = timer.replace(row, &format!("{row}\n{rule}\n{task}"));
        assert_eq!(read(&stacked).as_deref(), Some(""));
        assert_eq!(read(&stacked.replace("\n>\n", "\n> my draft\n")).as_deref(), Some("my draft"));
        // A lost top border above the box still finds the box, not the previous prompt.
        let lost = timer.replacen(&format!("\n{rule}\n>\n"), "\n>\n", 1);
        assert_eq!(read(&lost).as_deref(), Some(""));
        // Blocks that are not status rows stop the walk: the box is then not found.
        for not_status in [
            "",                             // mid-redraw: two rules with nothing between
            "  my draft continues",          // indented text, not a status row
            "● Scheduling 1 timer",          // a transcript line in column 0
            &format!("{row}\n{row}\n{row}\n{row}"), // too many rows for a status block
        ] {
            let screen = timer.replace(&format!("{row}\n"), &format!("{not_status}\n"));
            assert!(read(&screen).is_none(), "{not_status:?}");
        }
        // A box whose prompt line was lost does not borrow the prompt of the turn above it.
        let no_prompt = timer.replace("\n>\n", "\n  ● stray\n");
        assert!(read(&no_prompt).is_none());
        // A pasted message with a paragraph break: agy 1.5.13 draws the blank line as a blank row
        // in the box (captured 26 Nov), and Muster reads the box right after pasting.
        let paragraphs = "[muster msg 7 from human origin=human hop=0]\n  First paragraph here.\n\n  Second paragraph after a blank line.";
        assert_eq!(read(&timer.replace("\n>\n", &format!("\n> {paragraphs}\n"))).as_deref(), Some(paragraphs));
        // The box's top border and `>` line both lost, over an answer of indented text only: the
        // blank line agy leaves before the box keeps the older prompt out.
        let thoughts = "○ Thought for 4.1s (A timer fits: the build takes about 20 minutes...)\n● Scheduling 1 timer (1500s: check the build) (ctrl+o to expand)\n\n";
        let lost_both = timer.replacen(thoughts, "", 1).replacen(&format!("{rule}\n>\n"), "", 1);
        assert_eq!(lost_both.matches(&rule).count(), 3, "fixture shape changed");
        assert!(read(&lost_both).is_none());
    }

    #[test]
    fn callback_states() {
        use AgentState::*;
        for (status, state) in [
            ("working", Some(Working)),
            ("thinking", Some(Working)),
            ("tool_use", Some(Working)),
            ("idle", Some(Idle)),
            ("error", Some(Error)),
            ("authenticating", Some(Starting)),
            ("initializing", None),
            ("reviewing", None),
            ("unknown", None),
        ] {
            assert_eq!(
                AntigravityAdapter
                    .on_hook("state", &json!({"agent_state":status}))
                    .map(|u| u.state),
                state
            );
        }
        let pending = json!({"agent_state":"idle", "tool_confirmation_pending":true});
        assert_eq!(
            AntigravityAdapter.on_hook("state", &pending).unwrap().state,
            NeedsApproval
        );
        assert_eq!(AntigravityAdapter.on_hook("other", &pending), None);
    }

    #[test]
    fn launch_files_flags_and_resumed_prompts() {
        let dir = std::env::temp_dir().join(format!("muster-agy-{:x}", rand::random::<u64>()));
        crate::paths::ensure_private_dir(&dir).unwrap();
        // Outside the session dir, which must start empty.
        let user = std::env::temp_dir().join(format!("muster-agy-user-{:x}.json", rand::random::<u64>()));
        let mut ctx = SpawnCtx {
            session_dir: dir.clone(),
            cwd: dir.clone(),
            session_id: 1,
            muster_bin: "/tmp/demo app/muster".into(),
            daemon_url: "http://127.0.0.1:1234".into(),
            session_token: "demo-token".into(),
            permissions: Permissions::default(),
            initial_prompt: Some("read alpha".into()),
            extra_args: vec!["--model".into(), "demo".into()],
            resume: None,
        };
        let empty = GuardConfig::default();
        for (mode, flag) in [
            (AntigravityMode::Plan, "--mode=plan"),
            (AntigravityMode::AcceptEdits, "--mode=accept-edits"),
            (AntigravityMode::Auto, "--mode=auto"),
            (
                AntigravityMode::FullAccess,
                "--dangerously-skip-permissions",
            ),
        ] {
            ctx.permissions.antigravity_mode = mode;
            let spec = launch(&ctx, false, false, &user, &[], &empty, None).unwrap();
            assert_eq!(
                spec.args,
                [flag, "--model", "demo", "--prompt-interactive=read alpha"]
            );
            assert!(spec.consumed_initial_prompt);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let file = dir.join("antigravity-settings.json");
        let workspace = dir.join("antigravity-workspace");
        let mcp = workspace.join(".agents/mcp_config.json");
        let settings = format!("--settings={}", file.display());
        let add_dir = format!("--add-dir={}", workspace.display());
        assert_eq!(launch(&ctx, true, false, &user, &[], &empty, None).unwrap().args[0], settings);
        assert!(!workspace.exists());
        let read = |p| serde_json::from_slice::<Value>(&std::fs::read(p).unwrap()).unwrap();
        // Without Muster's MCP server there is no rule for it, and the user's list is untouched.
        assert!(read(&file).get("permissions").is_none());
        // A fresh run with Muster's tools gets its first prompt after `/clear`, through the queue.
        let fresh = launch(&ctx, false, true, &user, &[], &empty, None).unwrap();
        assert_eq!(fresh.args, [add_dir.as_str(), "--dangerously-skip-permissions", "--model", "demo"]);
        assert!(!fresh.consumed_initial_prompt && has_mcp_workspace(&fresh.args));
        assert!(!has_mcp_workspace(&launch(&ctx, true, false, &user, &[], &empty, None).unwrap().args));
        ctx.extra_args = vec!["--conversation".into(), "conv-demo".into()];
        let without_mcp = launch(&ctx, true, false, &user, &[], &empty, None).unwrap();
        assert!(!has_mcp_workspace(&without_mcp.args));
        let spec = launch(&ctx, true, true, &user, &[], &empty, None).unwrap();
        assert_eq!(spec.program, "agy");
        assert!(!spec.consumed_initial_prompt);
        assert!(has_mcp_workspace(&spec.args), "a resumed launch must wait for Muster's tools too");
        assert!(!has_mcp_workspace(&["--add-dir=/repo/memory".to_string()]));
        assert_eq!(
            spec.args,
            [
                &settings,
                &add_dir,
                "--dangerously-skip-permissions",
                "--conversation",
                "conv-demo"
            ]
        );
        assert_eq!(spec.env["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
        assert_eq!(spec.env["AGY_CLI_HIDE_ACCOUNT_INFO"], "true");
        assert_eq!(spec.env["AGY_CLI_OSC7501"], "off");
        let title = json!({"type":"command","command":"'/tmp/demo app/muster' hook antigravity state"});
        // No user settings file: Muster's rule alone.
        assert_eq!(read(&file), json!({"title": title, "permissions": {"allow": [MUSTER_TOOLS_RULE]}}));
        let clash = std::env::temp_dir().join(format!("muster-agy-clash-{:x}.json", rand::random::<u64>()));
        std::fs::write(&clash, r#"{"mcpServers":{"muster":{"command":"other"}}}"#).unwrap();
        launch(&ctx, true, true, &user, &[clash.clone()], &empty, None).unwrap();
        assert_eq!(read(&file), json!({"title": title}), "another `muster` server gets no rule");
        std::fs::remove_file(clash).unwrap();
        launch(&ctx, true, true, &user, &[], &empty, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join(".agents/AGENTS.md")).unwrap(),
            TOOLS_INSTRUCTION
        );
        assert_eq!(
            read(&mcp),
            json!({"mcpServers":{"muster":{"serverUrl":"http://127.0.0.1:1234/mcp",
            "headers":{"Authorization":"Bearer demo-token"}, "timeoutSeconds":1900}}})
        );
        for p in [&file, &mcp, &workspace, &workspace.join(".agents"), &workspace.join(".agents/AGENTS.md")] {
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        // No guards: no hook file and no guard list.
        let hooks = workspace.join(".agents/hooks.json");
        let guard_list = dir.join(crate::agy_guard::CONFIG);
        assert!(!hooks.exists() && !guard_list.exists());
        // The owner's guards: their list in the session dir (out of the agent's workspace), and a
        // hook file in the workspace that runs them through Muster.
        let guards = GuardConfig { cwd: dir.to_string_lossy().into(), stop: vec![], guards: vec![crate::agy_guard::Guard { matcher: "Bash".into(), command: "demo-guard".into(), timeout_s: 10 }] };
        launch(&ctx, true, true, &user, &[], &guards, None).unwrap();
        assert_eq!(read(&guard_list), json!({"cwd": dir.to_string_lossy(), "guards": [{"matcher": "Bash", "command": "demo-guard", "timeout_s": 10}]}));
        let hook = &read(&hooks)["muster-guards"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(hook["command"], format!("'/tmp/demo app/muster' hook antigravity guard {}", guard_list.display()));
        for p in [&hooks, &guard_list] {
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o077, 0);
        }
        // The next run without guards drops both.
        launch(&ctx, true, true, &user, &[], &empty, None).unwrap();
        assert!(!hooks.exists() && !guard_list.exists());
        std::fs::remove_dir_all(workspace.join(".agents")).unwrap();
        let other = dir.join("fixture");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, workspace.join(".agents")).unwrap();
        assert!(launch(&ctx, false, true, &user, &[], &empty, None).is_err());
        assert_eq!(std::fs::read_dir(other).unwrap().count(), 0);
        std::fs::remove_file(workspace.join(".agents")).unwrap();
        std::fs::write(workspace.join("old-run"), "fixture").unwrap();
        launch(&ctx, false, true, &user, &[], &empty, None).unwrap();
        assert!(!workspace.join("old-run").exists());
        std::fs::remove_file(&mcp).unwrap();
        std::os::unix::fs::symlink(dir.join("absent"), &mcp).unwrap();
        assert!(launch(&ctx, false, true, &user, &[], &empty, None).is_err());
        assert!(!dir.join("absent").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_model_sessions_snapshot_rules_at_launch() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let source = dir.join("alpha.md");
        let mut ctx = SpawnCtx {
            session_dir: dir.into(), cwd: dir.into(), session_id: 1, muster_bin: "/opt/muster".into(),
            daemon_url: "http://127.0.0.1:1".into(), session_token: "alpha".into(),
            permissions: Permissions::default(), initial_prompt: None, extra_args: vec![], resume: None,
        };
        let config = GuardConfig::default();
        let launch_with = |ctx: &SpawnCtx, mcp, file: Option<&Path>| launch(ctx, false, mcp, &dir.join("settings"), &[], &config, file);
        let rules = dir.join("antigravity-workspace/.agents/AGENTS.md");
        assert!(rules_file(None).unwrap().ends_with(".claude/CLAUDE.md"));
        let selected = rules_file(Some(source.clone().into_os_string()));
        let off = rules_file(Some("off".into()));
        assert!(off.is_none());
        ctx.extra_args = vec!["--model=claude-alpha".into()];
        launch_with(&ctx, true, selected.as_deref()).unwrap();
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), TOOLS_INSTRUCTION);
        std::fs::write(&source, "Alpha rule.\n").unwrap();
        launch_with(&ctx, true, selected.as_deref()).unwrap();
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), format!("{TOOLS_INSTRUCTION}\nAlpha rule.\n"));
        std::fs::write(&source, "Beta rule.").unwrap();
        assert!(!std::fs::read_to_string(&rules).unwrap().contains("Beta"));
        let spec = launch_with(&ctx, false, selected.as_deref()).unwrap();
        assert!(!has_mcp_workspace(&spec.args));
        assert_eq!(std::fs::read_to_string(dir.join("antigravity-rules/.agents/AGENTS.md")).unwrap(), "Beta rule.");
        assert!(!dir.join("antigravity-rules/.agents/mcp_config.json").exists());
        launch_with(&ctx, true, off.as_deref()).unwrap();
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), TOOLS_INSTRUCTION);
        ctx.extra_args.extend(["--model".into(), "gemini-beta".into()]);
        launch_with(&ctx, true, selected.as_deref()).unwrap();
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), TOOLS_INSTRUCTION);
        ctx.extra_args = vec!["--model".into(), "claude-beta".into()];
        let link = dir.join("link");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(launch_with(&ctx, true, Some(&link)).is_err());
        std::fs::write(&source, vec![b'x'; MAX_RULES as usize + 1]).unwrap();
        assert!(launch_with(&ctx, true, Some(&source)).is_err());
    }

    #[test]
    fn the_users_allow_rules_are_kept_beside_muster_tools() {
        let dir = std::env::temp_dir().join(format!("muster-agy-rules-{:x}", rand::random::<u64>()));
        crate::paths::ensure_private_dir(&dir).unwrap();
        let file = dir.join("settings.json");
        let rules = |text: &str| {
            std::fs::write(&file, text).unwrap();
            settings_json(Path::new("muster"), Some(user_allow_rules(&file)))["permissions"].clone()
        };
        // The shape agy 1.5.13 documents and reads; ask and deny stay in the user's file only.
        assert_eq!(
            rules(r#"{"model":"demo","permissions":{"allow":["command(git status)","mcp(linter/*)"],"deny":["command(sudo)"]}}"#),
            json!({"allow": ["command(git status)", "mcp(linter/*)", MUSTER_TOOLS_RULE]})
        );
        assert_eq!(rules(r#"{"permissions":{"allow":["mcp(muster/*)"]}}"#), json!({"allow": [MUSTER_TOOLS_RULE]}));
        for other in ["", "not json", "[]", r#"{"permissions":{"allow":"command(git)"}}"#, r#"{"permissions":null}"#] {
            assert_eq!(rules(other), json!({"allow": [MUSTER_TOOLS_RULE]}), "{other:?}");
        }
        assert_eq!(rules(r#"{"permissions":{"allow":[1,"read_url(example.com)"]}}"#), json!({"allow": ["read_url(example.com)", MUSTER_TOOLS_RULE]}));
        std::fs::remove_file(&file).unwrap();
        assert!(user_allow_rules(&file).is_empty());
        // A folder or a FIFO in its place gives no rules and never blocks.
        std::fs::create_dir(&file).unwrap();
        assert!(user_allow_rules(&file).is_empty());
        std::fs::remove_dir(&file).unwrap();
        assert!(std::process::Command::new("mkfifo").arg(&file).status().unwrap().success());
        assert!(user_allow_rules(&file).is_empty());
        assert!(settings_json(Path::new("muster"), None).get("permissions").is_none());
        // Another server named `muster` in the user's or the folder's MCP config: no rule.
        let other = dir.join("mcp_config.json");
        std::fs::write(&other, r#"{"mcpServers":{"linter":{"command":"lint"}}}"#).unwrap();
        assert!(!names_muster(&[dir.join("absent.json"), other.clone()]));
        std::fs::write(&other, r#"{"mcpServers":{"muster":{"serverUrl":"http://127.0.0.1:9/mcp"}}}"#).unwrap();
        assert!(names_muster(&[dir.join("absent.json"), other.clone()]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
