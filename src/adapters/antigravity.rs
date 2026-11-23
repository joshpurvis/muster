//! Antigravity launch and status for agy 1.5.13. Verified against its local help and strings
//! only. The fixtures are hand-written. No screen has been captured from a real session.

use super::{Adapter, CommandSpec, ScreenSnapshot, SpawnCtx, claude, resumed_in_launch, tui};
use crate::model::{AgentState, AntigravityMode, Decision, SessionKind, StateUpdate};
use serde_json::{Value, json};
use std::path::Path;

pub struct AntigravityAdapter;
pub(crate) const LAST_STATE: &str = "antigravity-state-last";

const TRUST: &[&str] = &[
    "Do you trust the contents of this project?",
    "Yes, I trust this folder",
];
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

fn launch(ctx: &SpawnCtx, settings: bool, mcp: bool) -> anyhow::Result<CommandSpec> {
    let mut args = Vec::new();
    if settings {
        let file = ctx.session_dir.join("antigravity-settings.json");
        claude::write_private(
            &file,
            &serde_json::to_string_pretty(&settings_json(&ctx.muster_bin))?,
        )?;
        args.push(format!("--settings={}", file.display()));
        // The first callback matters even if it repeats the last state of the previous run.
        match std::fs::remove_file(ctx.session_dir.join(LAST_STATE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    if mcp {
        let workspace = ctx.session_dir.join("antigravity-workspace");
        let config = workspace.join(".agents");
        crate::paths::ensure_private_dir(&workspace)?;
        crate::paths::ensure_private_dir(&config)?;
        let server = mcp_json(&ctx.daemon_url, &ctx.session_token);
        claude::write_private(
            &config.join("mcp_config.json"),
            &serde_json::to_string_pretty(&server)?,
        )?;
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

fn manual_dialog(lines: &[String]) -> Option<&'static str> {
    if lines.iter().any(|l| TRUST.iter().any(|s| l.contains(s))) {
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

fn approval_dialog(lines: &[String]) -> Option<(usize, Vec<tui::MenuOption>)> {
    let q = tui::rfind(lines, |l| {
        let t = l.trim();
        APPROVAL_TITLES.contains(&t)
            || (t.ends_with('?')
                && ["Run this", "Allow ", "Accept this"]
                    .iter()
                    .any(|prefix| t.starts_with(prefix)))
    })?;
    let options = tui::options_in(lines, q + 1, q + 12);
    (options.len() >= 2 && options.iter().any(|o| tui::is_yes_label(&o.label)))
        .then_some((q, options))
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
            return Some(StateUpdate::with(
                NeedsApproval,
                "Antigravity is asking for approval",
            ));
        }
        Some(match payload["agent_state"].as_str()? {
            "working" | "thinking" | "tool_use" => StateUpdate::new(Working),
            "idle" => StateUpdate::new(Idle),
            "error" => StateUpdate::with(Error, "Antigravity reported an error"),
            "authenticating" => StateUpdate::with(
                AskingQuestion,
                "Antigravity needs a sign-in: finish it in the terminal",
            ),
            _ => return None,
        })
    }

    fn detect_from_screen(
        &self,
        screen: &ScreenSnapshot,
        current: AgentState,
    ) -> Option<StateUpdate> {
        let lines = &screen.lines;
        if let Some(detail) = manual_dialog(lines) {
            return Some(StateUpdate::with(AgentState::AskingQuestion, detail));
        }
        if let Some((q, _)) = approval_dialog(lines) {
            let mut detail = lines[q].trim().to_string();
            if let Some(reason) = lines
                .iter()
                .skip(q + 1)
                .take(3)
                .find_map(|l| l.trim().strip_prefix("Reason: "))
            {
                detail.push_str(&format!(" ({reason})"));
            }
            return Some(StateUpdate::with(
                AgentState::NeedsApproval,
                tui::clip(&detail, 160),
            ));
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
        if manual_dialog(&screen.lines).is_some() {
            return None;
        }
        let (_, opts) = approval_dialog(&screen.lines)?;
        tui::choose(&opts, decision)
            .map(|o| tui::option_keys(o, false))
            .or_else(|| (decision == Decision::Deny).then(|| b"n".to_vec()))
    }

    fn interrupt_bytes(&self) -> Vec<u8> {
        vec![0x1b]
    }
}

#[cfg(test)]
mod tests {
    // Hand-written from 1.5.13 strings. Live check L4 replaces these with sanitised captures.
    use super::tui::testutil::snap;
    use super::*;
    use crate::model::Permissions;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn dialogs_require_an_explicit_choice() {
        use Decision::*;
        for manual in [
            include_str!("fixtures/antigravity_trust.txt"),
            include_str!("fixtures/antigravity_settings_error.txt"),
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
                b'3',
                Some(vec![b'2']),
            ),
            (
                include_str!("fixtures/antigravity_approval_create.txt"),
                b'2',
                None,
            ),
            (
                include_str!("fixtures/antigravity_approval_edit.txt"),
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
        let no_deny = snap("Allow calling this tool?\nReason: read file\n1. Yes\n2. Yes, always");
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
    fn callback_states() {
        use AgentState::*;
        for (status, state) in [
            ("working", Some(Working)),
            ("thinking", Some(Working)),
            ("tool_use", Some(Working)),
            ("idle", Some(Idle)),
            ("error", Some(Error)),
            ("authenticating", Some(AskingQuestion)),
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
        std::fs::remove_dir_all(dir).unwrap();
    }
}
