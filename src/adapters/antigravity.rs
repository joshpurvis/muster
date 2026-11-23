//! Antigravity launch and status, verified against agy 1.5.13 screens on 23 Nov 2026.

use super::{Adapter, CommandSpec, ScreenSnapshot, SpawnCtx, claude, resumed_in_launch, tui};
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

/// Only a visible dialog can override the run's latest state callback.
pub(crate) fn is_screen_dialog(state: AgentState, detail: Option<&str>) -> bool {
    state == AgentState::AskingQuestion
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

pub fn settings_json(bin: &Path) -> Value {
    let bin = claude::sh_quote(&bin.to_string_lossy());
    json!({"title":{"type":"command","command":format!("{bin} hook antigravity state")}})
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

/// Build the credential file outside the previously shared tree. Only publish the
/// completed directory once the old run's entries have been checked and removed.
fn prepare_workspace(ctx: &SpawnCtx) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let workspace = ctx.session_dir.join("antigravity-workspace");
    let config = workspace.join(".agents");
    for path in [&workspace, &config, &config.join("mcp_config.json")] {
        crate::agents::refuse_symlink(path)?;
    }
    let stage = ctx
        .session_dir
        .join(format!(".antigravity-{:016x}", rand::random::<u64>()));
    std::fs::DirBuilder::new().mode(0o700).create(&stage)?;
    let install = || -> anyhow::Result<()> {
        let server = mcp_json(&ctx.daemon_url, &ctx.session_token);
        crate::paths::write_private(
            &stage.join(".agents/mcp_config.json"),
            &serde_json::to_vec_pretty(&server)?,
        )?;
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

fn launch(ctx: &SpawnCtx, settings: bool, mcp: bool) -> anyhow::Result<CommandSpec> {
    let mut args = Vec::new();
    if settings {
        let file = ctx.session_dir.join("antigravity-settings.json");
        crate::agents::refuse_symlink(&file)?;
        crate::paths::write_private(
            &file,
            &serde_json::to_vec_pretty(&settings_json(&ctx.muster_bin))?,
        )?;
        args.push(format!("--settings={}", file.display()));
        // The first callback matters even if it repeats the last state of the previous run.
        match std::fs::remove_file(ctx.session_dir.join(LAST_STATE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    if mcp {
        let workspace = prepare_workspace(ctx)?;
        args.push(format!("--add-dir={}", workspace.display()));
    }
    args.extend(mode_args(ctx.permissions.antigravity_mode));
    args.extend_from_slice(&ctx.extra_args);
    let prompt = ctx.initial_prompt.as_ref().filter(|p| {
        !p.trim().is_empty()
            && resumed_in_launch(SessionKind::Antigravity, &ctx.extra_args).is_none()
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

// The composer and modal content are separated from past turns by a solid rule.
fn active_dialog_lines(lines: &[String]) -> &[String] {
    tui::rfind(lines, tui::is_solid_rule).map_or(lines, |i| &lines[i + 1..])
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
    title: String,
    detail: String,
    options: Vec<tui::MenuOption>,
}

fn approval_dialog(lines: &[String]) -> Option<ApprovalDialog> {
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
    let start = tui::rfind(before, |l| l.trim() == "Requesting permission for:");
    let body = match start {
        Some(i) => &before[i + 1..],
        None if title == "Run this command?" => &before[..0],
        None => before,
    };
    let body = body.join("\n");
    let reason = lines[q + 1..]
        .iter()
        .find_map(|l| l.trim().strip_prefix("Reason: "));
    let detail = if !body.trim().is_empty() {
        format!("{title} {}", body.trim())
    } else if let Some(reason) = reason {
        format!("{title} ({reason})")
    } else {
        title.to_string()
    };
    Some(ApprovalDialog {
        title: title.into(),
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
        if let Some(request) = approval_dialog(dialog) {
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
        let request = approval_dialog(dialog)?;
        if request.detail == request.title {
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
        // A panel can leave a complete, inactive composer visible above its controls.
        if !lines[bottom + 1..]
            .iter()
            .any(|l| l.trim_start().starts_with("? for shortcuts"))
        {
            return None;
        }
        let top = tui::rfind(&lines[..bottom], tui::is_solid_rule)?;
        let (first, rest) = lines[top + 1..bottom].split_first()?;
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
        let placeholder = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if matches!(
            placeholder.as_str(),
            "Auto mode: edits & safe commands auto-approved (shift+tab to cycle)"
                | "Plan mode: research & plan only (shift+tab to cycle)"
        ) {
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
            ("Plan mode: research & plan only (shift+tab to cycle)", ""),
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
        assert!(AntigravityAdapter.clear_input_bytes().is_none());
        let panel = snap(include_str!("fixtures/antigravity_mcp_panel.txt"));
        assert!(AntigravityAdapter.composer_text(&panel).is_none());
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
    fn callback_states() {
        use AgentState::*;
        for (status, state) in [
            ("working", Some(Working)),
            ("thinking", Some(Working)),
            ("tool_use", Some(Working)),
            ("idle", Some(Idle)),
            ("error", Some(Error)),
            ("authenticating", None),
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
            let spec = launch(&ctx, false, false).unwrap();
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
        assert_eq!(launch(&ctx, true, false).unwrap().args[0], settings);
        assert!(!workspace.exists());
        assert_eq!(launch(&ctx, false, true).unwrap().args[0], add_dir);
        ctx.extra_args = vec!["--conversation".into(), "conv-demo".into()];
        let spec = launch(&ctx, true, true).unwrap();
        assert_eq!(spec.program, "agy");
        assert!(!spec.consumed_initial_prompt);
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
        let read = |p| serde_json::from_slice::<Value>(&std::fs::read(p).unwrap()).unwrap();
        assert_eq!(
            read(&file),
            json!({"title":{"type":"command","command":"'/tmp/demo app/muster' hook antigravity state"}})
        );
        assert_eq!(
            read(&mcp),
            json!({"mcpServers":{"muster":{"serverUrl":"http://127.0.0.1:1234/mcp",
            "headers":{"Authorization":"Bearer demo-token"}, "timeoutSeconds":1900}}})
        );
        for p in [&file, &mcp, &workspace, &workspace.join(".agents")] {
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        std::fs::remove_dir_all(workspace.join(".agents")).unwrap();
        let other = dir.join("fixture");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, workspace.join(".agents")).unwrap();
        assert!(launch(&ctx, false, true).is_err());
        assert_eq!(std::fs::read_dir(other).unwrap().count(), 0);
        std::fs::remove_file(workspace.join(".agents")).unwrap();
        std::fs::write(workspace.join("old-run"), "fixture").unwrap();
        launch(&ctx, false, true).unwrap();
        assert!(!workspace.join("old-run").exists());
        std::fs::remove_file(&mcp).unwrap();
        std::os::unix::fs::symlink(dir.join("absent"), &mcp).unwrap();
        assert!(launch(&ctx, false, true).is_err());
        assert!(!dir.join("absent").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
