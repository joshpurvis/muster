//! Client protocol (docs/PROTOCOL.md): JSON method dispatch shared by the WebSocket and
//! `POST /api/rpc`, plus the WebSocket connection loop with binary terminal frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::devices::{self, Principal, Scope};
use crate::hub::{Attach, CreateChannel, Hub, ServerEvent, SpawnSession};
use crate::inject::{self, SendMode};
use crate::model::*;

pub const FRAME_OUTPUT: u8 = 0x01;
pub const FRAME_INPUT: u8 = 0x02;
pub const FRAME_REPLAY: u8 = 0x03;

/// Outgoing messages buffered per connection before output forwarding applies backpressure.
const OUTBOX_CAP: usize = 256;

/// `session.wait` limits.
const WAIT_DEFAULT_MS: u64 = 600_000;
const WAIT_MAX_MS: u64 = 3_600_000;
/// `agent.ask`: how long to wait for the agent's reply (as MCP `ask_agent`).
const AGENT_ASK_DEFAULT_MS: u64 = 600_000;
const AGENT_ASK_MAX_MS: u64 = 1_800_000;
/// `session.attach` `replay_bytes` bounds.
const REPLAY_BYTES_MIN: usize = 4 * 1024;
/// `session.output` limits.
const OUTPUT_DEFAULT_LINES: usize = 200;
const OUTPUT_MAX_LINES: usize = 10_000;

/// Identifies a WebSocket connection (the holder of a claimed window).
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

pub fn frame(ty: u8, id: SessionId, payload: &[u8]) -> Bytes {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ty);
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(payload);
    Bytes::from(v)
}

pub struct RpcError(pub String);

impl<E: std::fmt::Display> From<E> for RpcError {
    fn from(e: E) -> Self {
        RpcError(format!("{e:#}").replace("\n", " "))
    }
}

type RpcResult = Result<Value, RpcError>;

fn empty_object() -> Value { json!({}) }

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError(format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(v: T) -> RpcResult {
    Ok(serde_json::to_value(v)?)
}

#[derive(Deserialize)]
struct IdParam<T> {
    id: T,
}

#[derive(Deserialize)]
struct Rename<T> {
    id: T,
    name: String,
}

pub fn hello(hub: &Hub) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "ui_protocol": crate::assets::UI_PROTOCOL,
        "workspaces": hub.workspaces(),
        "account_profiles": hub.account_profiles().unwrap_or_default(),
        "channels": hub.channels(),
        "sessions": hub.sessions(),
        "panels": crate::panels::list(hub).unwrap_or_default().into_iter().filter(|p| !p.closed).collect::<Vec<_>>(),
        "panel_origin": format!("http://127.0.0.1:{}", hub.panels.port.load(Ordering::Relaxed)),
        "windows": hub.windows(),
        "home": crate::paths::home_dir().to_string_lossy(),
        "default_shell": crate::adapters::shell::user_shell(),
        "login_shell": crate::adapters::shell::fallback_shell(),
        "paused_deliveries": inject::paused(hub),
        "terminal_host": hub.terminal_host_info(),
        "updates": hub.updates().status(),
        "prefs": hub.ui_prefs(),
    })
}

/// Methods that restart or stop Muster: refused to paired devices and to calls made on a
/// session's behalf (`from_session`, as the CLI sends inside a session); the MCP server has no
/// tool for them and session tokens (MCP, hooks) can't reach this API. That is the whole
/// guarantee: any process of this user that holds the client token (or runs `muster stop`
/// itself) can still restart or stop Muster, as with `channel.set_permissions`.
fn human_only(who: &Principal, method: &str, params: &Value) -> Result<(), RpcError> {
    if !matches!(who, Principal::Local) {
        return Err(RpcError(format!("{method} is only available on this computer")));
    }
    if params.get("from_session").is_some_and(|v| !v.is_null()) {
        return Err(RpcError(format!("{method} can't be called from a session")));
    }
    Ok(())
}

/// May `who` call `method` with `params`? Client-token callers may call anything; a device
/// needs the method's scope (re-read now, so a revoked or expired device fails at once) and may
/// never widen an agent's permissions (docs/ANDROID.md "Scopes").
pub fn authorize(hub: &Hub, who: &Principal, method: &str, params: &Value) -> Result<(), RpcError> {
    let Principal::Device { id } = who else {
        return Ok(());
    };
    let have = hub
        .devices()
        .scope(*id)
        .ok_or_else(|| RpcError("device revoked or expired".into()))?;
    let need = devices::method_scope(method).ok_or_else(|| RpcError(format!("{method} is not available to devices")))?;
    if have < need {
        return Err(RpcError(format!("{method} needs the {need:?} scope").to_lowercase()));
    }
    if let Some(p) = devices::forbidden_param(params) {
        return Err(RpcError(format!("devices may not set {p}")));
    }
    Ok(())
}

/// Every method except `session.attach` / `session.detach` (connection-scoped, WS only).
/// Callers check `authorize` first.
pub async fn dispatch(hub: &Arc<Hub>, who: &Principal, method: &str, params: Value) -> RpcResult {
    match method {
        "hello" => {
            let mut result = hello(hub);
            if matches!(who, Principal::Device { .. }) {
                if let Some(fields) = result.as_object_mut() {
                    fields.remove("panels");
                    fields.remove("panel_origin");
                }
            }
            Ok(result)
        }

        "panel.open" | "panel.update" | "panel.close" | "panel.list" | "panel.wait" | "panel.fetch_event" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { from_session: SessionId, #[serde(default = "empty_object")] arguments: Value }
            let p: P = parse(params)?;
            let tool = match method {
                "panel.open" => "panel_open", "panel.update" => "panel_update", "panel.close" => "panel_close",
                "panel.list" => "list_panels", "panel.fetch_event" => "panel_event", _ => "wait_for_panel_event",
            };
            mcp_json(crate::mcp::call_as(hub, p.from_session, tool, p.arguments).await)
        }
        "panel.resource" => {
            let p: IdParam<PanelId> = parse(params)?;
            Ok(crate::panels::resource(hub, p.id)?)
        }
        "panel.navigate" => {
            // The user's own address bar: desktop client token only (devices have no scope for
            // it, and `from_session` is an unknown field here).
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { id: PanelId, url: String }
            let p: P = parse(params)?;
            let (url, changed) = crate::panels::navigate(hub, p.id, &p.url)?;
            Ok(json!({"url": url, "changed": changed}))
        }
        "panel.preview_check" => {
            let p: IdParam<PanelId> = parse(params)?;
            Ok(crate::web_preview::check_panel(hub, p.id).await?.1)
        }
        "panel.event" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { panel_id: PanelId, kind: String, payload: Value }
            let p: P = parse(params)?;
            crate::panels::event(hub, p.panel_id, p.kind, p.payload)?;
            Ok(json!({}))
        }
        "panel.context" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { panel_id: PanelId, context: Value }
            let p: P = parse(params)?;
            crate::panels::context(hub, p.panel_id, p.context)?;
            Ok(json!({}))
        }
        "panel.dismiss" => {
            let p: IdParam<PanelId> = parse(params)?;
            crate::panels::close(hub, None, p.id, true)?;
            Ok(json!({}))
        }
        "panel.move" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { id: PanelId, window_id: WindowId }
            let p: P = parse(params)?;
            crate::panels::move_to(hub, p.id, p.window_id)?;
            Ok(json!({}))
        }
        "panel.tool" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { panel_id: PanelId, name: String, #[serde(default = "empty_object")] arguments: Value }
            let p: P = parse(params)?;
            if p.name != "get_diff" { return Err(RpcError("tool is not app-visible".into())) }
            let panel = crate::panels::get(hub, p.panel_id)?;
            if panel.closed { return Err(RpcError("panel is closed".into())) }
            let ch = hub.channel(panel.channel_id).filter(|c| !c.archived).ok_or("panel task is gone")?;
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct A { max_bytes: Option<usize> }
            let args: A = parse(p.arguments)?;
            let max = args.max_bytes.unwrap_or(60_000).clamp(1000, 400_000);
            let diff = crate::worktree::diff(std::path::Path::new(&ch.cwd), ch.base_branch.as_deref()).await?;
            let text = crate::mcp::tools::truncate_str(&diff, max);
            Ok(json!({"content":[{"type":"text","text":text}],"isError":false}))
        }

        "daemon.reload" => {
            human_only(who, method, &params)?;
            #[derive(Deserialize)]
            struct P {
                /// The `updates.backend` id the human was offered.
                expect: String,
            }
            let p: P = parse(params)?;
            let exe = crate::updates::reload(hub, &p.expect).await?;
            Ok(json!({ "exe": exe.to_string_lossy() }))
        }
        "daemon.shutdown" => {
            human_only(who, method, &params)?;
            crate::updates::shutdown(hub)?;
            Ok(json!({}))
        }
        "prefs.get" => to_value(hub.ui_prefs()),
        "prefs.set" => {
            if params.get("shell").is_some() {
                // It names a program the backend runs: chosen on this computer only.
                human_only(who, "prefs.set shell", &params)?;
            }
            let p: UiPrefsChange = parse(params)?;
            to_value(hub.set_ui_prefs(p)?)
        }

        "workspace.add" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
                name: Option<String>,
            }
            let p: P = parse(params)?;
            to_value(hub.add_workspace(p.path, p.name).await?)
        }
        "account_profile.list" => {
            require_local(who, method)?;
            to_value(hub.account_profiles()?)
        }
        "account_profile.create" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P { name: String, config_dir: String }
            let p: P = parse(params)?;
            to_value(hub.add_account_profile(&p.name, &p.config_dir)?)
        }
        "account_profile.update" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P { id: i64, name: Option<String>, enabled: Option<bool> }
            let p: P = parse(params)?;
            to_value(hub.update_account_profile(p.id, p.name, p.enabled)?)
        }
        "workspace.set_account_profile" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P { id: WorkspaceId, account_profile: crate::account_profiles::ProfileChoice }
            let p: P = parse(params)?;
            to_value(hub.set_workspace_account_profile(p.id, p.account_profile.id())?)
        }
        "workspace.remove" => {
            let p: IdParam<WorkspaceId> = parse(params)?;
            hub.remove_workspace(p.id).await?;
            Ok(json!({}))
        }

        "channel.create" => {
            let mut p: CreateChannel = parse(params)?;
            p.prov = spawn_prov(hub, p.from_session, p.agent)?;
            to_value(hub.create_channel(p).await?)
        }
        "channel.rename" => {
            let p: Rename<ChannelId> = parse(params)?;
            to_value(hub.rename_channel(p.id, p.name).await?)
        }
        "channel.archive" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                #[serde(default)]
                remove_worktree: bool,
            }
            let p: P = parse(params)?;
            hub.archive_channel(p.id, p.remove_worktree).await?;
            Ok(json!({}))
        }
        "channel.set_permissions" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                permissions: PermissionsInput,
            }
            let p: P = parse(params)?;
            to_value(hub.set_permissions(p.id, p.permissions.into())?)
        }
        "channel.set_layout" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                #[serde(default)]
                layout: Value,
            }
            let p: P = parse(params)?;
            hub.set_layout(p.id, p.layout)?;
            Ok(json!({}))
        }
        "channel.mark_read" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                #[serde(default)]
                at: Option<i64>,
            }
            let p: P = parse(params)?;
            hub.mark_read(p.id, p.at)?;
            Ok(json!({}))
        }
        "channel.reorder" => {
            #[derive(Deserialize)]
            struct P {
                ids: Vec<ChannelId>,
            }
            let p: P = parse(params)?;
            hub.reorder_channels(&p.ids)?;
            Ok(json!({}))
        }
        "channel.post" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                text: String,
            }
            let p: P = parse(params)?;
            if hub.channel(p.id).is_none() {
                return Err(RpcError(format!("no such channel: {}", p.id)));
            }
            if p.text.trim().is_empty() {
                return Err(RpcError("text is empty".into()));
            }
            to_value(hub.log_event(Some(p.id), None, EventKind::Note, json!({ "text": p.text })))
        }
        "channel.timeline" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                before: Option<i64>,
                limit: Option<usize>,
            }
            let p: P = parse(params)?;
            to_value(hub.timeline(p.id, p.before, p.limit.unwrap_or(200)))
        }
        "channel.diff" => {
            let p: IdParam<ChannelId> = parse(params)?;
            Ok(json!({ "diff": hub.channel_diff(p.id).await? }))
        }

        "session.spawn" => {
            let mut p: SpawnSession = parse(params)?;
            p.prov = spawn_prov(hub, p.from_session, Some(p.kind))?;
            to_value(hub.spawn_session(p).await?)
        }
        "session.resize" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                cols: u16,
                rows: u16,
            }
            let p: P = parse(params)?;
            hub.resize_session(p.id, p.cols, p.rows, &who.size_owner())?;
            Ok(json!({}))
        }
        "session.kill" => {
            let p: IdParam<SessionId> = parse(params)?;
            hub.kill_session(p.id)?;
            Ok(json!({}))
        }
        "session.restart" => {
            let p: IdParam<SessionId> = parse(params)?;
            to_value(hub.restart_session(p.id).await?)
        }
        "session.remove" => {
            let p: IdParam<SessionId> = parse(params)?;
            hub.remove_session(p.id)?;
            Ok(json!({}))
        }
        "session.hide" => {
            let p: IdParam<SessionId> = parse(params)?;
            to_value(hub.hide_session(p.id)?)
        }
        "session.rename" => {
            let p: Rename<SessionId> = parse(params)?;
            to_value(hub.rename_session(p.id, p.name)?)
        }
        "channel.close_if_empty" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
            }
            let p: P = parse(params)?;
            Ok(json!({ "closed": hub.archive_if_empty(p.id)? }))
        }
        "session.move" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                channel_id: ChannelId,
                /// Close the source channel if this move emptied it (checked atomically).
                #[serde(default)]
                close_empty_source: bool,
                /// Refuse unless the session is still in this channel (stale-view guard).
                #[serde(default)]
                from_channel_id: Option<ChannelId>,
            }
            let p: P = parse(params)?;
            let (info, from) = hub.move_session_from(p.id, p.channel_id, p.from_channel_id)?;
            if p.close_empty_source && from != p.channel_id {
                hub.archive_if_empty(from)?;
            }
            to_value(info)
        }
        "session.set_window" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                window_id: WindowId,
                /// Refuse unless the session is still in this window (stale-view guard).
                #[serde(default)]
                from_window_id: Option<WindowId>,
            }
            let p: P = parse(params)?;
            to_value(hub.set_session_window(p.id, p.window_id, p.from_window_id)?)
        }
        "session.send" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                text: String,
                #[serde(default)]
                mode: Option<SendMode>,
                /// The session running `muster send` (`$MUSTER_SESSION_ID`).
                #[serde(default)]
                from_session: Option<SessionId>,
            }
            let p: P = parse(params)?;
            let Some(target) = hub.session(p.id) else {
                return Err(RpcError(format!("no such session: {}", p.id)));
            };
            let mode = p.mode.unwrap_or_default();
            match caller(hub, p.from_session)? {
                // An agent's CLI send is its send_message: same guards, same provenance.
                Caller::Agent(s) => {
                    let args = json!({"session_id": p.id, "text": p.text, "interrupt": mode == SendMode::Interrupt});
                    mcp_json(crate::mcp::call_as(hub, s, "send_message", args).await)
                }
                Caller::Shell(s) => {
                    let source = shell_source(hub, s);
                    if let crate::agents::Source::Chain(prov) = source {
                        crate::mcp::guard::check_claude_origin(target.kind, prov.origin).map_err(RpcError)?;
                        if let Some(b) = hub.agents().by_session(p.id) {
                            return Err(RpcError(format!("session {} is the agent {}: use `muster ask {}`", p.id, b.name, b.name)));
                        }
                    }
                    to_value(inject::send(hub, p.id, p.text, mode, format!("session:{s}"), source).await?)
                }
                Caller::Human => to_value(
                    inject::send(hub, p.id, p.text, mode, who.label(), crate::agents::Source::Human).await?,
                ),
            }
        }
        "session.approve" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                decision: Decision,
                /// The `state_seq` / `state_since` of the prompt the caller saw (stale-view guard).
                #[serde(default)]
                state_seq: Option<u64>,
                #[serde(default)]
                state_since: Option<i64>,
            }
            let p: P = parse(params)?;
            // A device answers only the prompt it showed (the desktop and CLI answer what's there).
            if matches!(who, Principal::Device { .. }) && p.state_seq.is_none() {
                return Err(RpcError("state_seq is required".into()));
            }
            inject::approve(hub, p.id, p.decision, &who.label(), (p.state_seq, p.state_since))?;
            Ok(json!({}))
        }
        "session.interrupt" => {
            let p: IdParam<SessionId> = parse(params)?;
            inject::interrupt(hub, p.id)?;
            Ok(json!({}))
        }
        "session.queue" => {
            let p: IdParam<SessionId> = parse(params)?;
            if hub.session(p.id).is_none() { return Err(RpcError(format!("no such session: {}", p.id))) }
            Ok(inject::inspect(hub, p.id))
        }
        "session.clear_queue" => {
            let p: IdParam<SessionId> = parse(params)?;
            inject::clear_queue(hub, p.id)?;
            Ok(json!({}))
        }
        "session.resume_queue" => {
            let p: IdParam<SessionId> = parse(params)?;
            inject::resume_queue(hub, p.id)?;
            Ok(json!({}))
        }
        "session.continue_input" => {
            // After an input interruption: the user has checked the terminal. Nothing lost
            // is resent; typing is accepted again and waiting prompts may be delivered.
            let p: IdParam<SessionId> = parse(params)?;
            hub.continue_input(p.id)?;
            Ok(json!({}))
        }
        "session.screen" => {
            let p: IdParam<SessionId> = parse(params)?;
            if hub.session(p.id).is_none() {
                return Err(RpcError(format!("no such session: {}", p.id)));
            }
            let lines = hub.screen(p.id).map(|s| s.lines).unwrap_or_default();
            Ok(json!({ "lines": lines }))
        }

        "channel.move" => {
            #[derive(Deserialize)]
            struct P {
                id: ChannelId,
                window_id: WindowId,
                #[serde(default)]
                index: Option<usize>,
            }
            let p: P = parse(params)?;
            to_value(hub.move_channel(p.id, p.window_id, p.index)?)
        }

        "window.create" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                name: Option<String>,
            }
            let p: P = parse(params)?;
            to_value(hub.create_window(p.name)?)
        }
        "window.set_layout" => {
            #[derive(Deserialize)]
            struct P {
                id: WindowId,
                #[serde(default)]
                layout: Value,
            }
            let p: P = parse(params)?;
            hub.set_window_layout(p.id, p.layout)?;
            Ok(json!({}))
        }
        "window.focused" => {
            let p: IdParam<WindowId> = parse(params)?;
            hub.window_focused(p.id)?;
            Ok(json!({}))
        }
        "window.rename" => {
            let p: Rename<WindowId> = parse(params)?;
            to_value(hub.rename_window(p.id, p.name)?)
        }
        "window.reorder_tabs" => {
            #[derive(Deserialize)]
            struct P {
                id: WindowId,
                channel_ids: Vec<ChannelId>,
            }
            let p: P = parse(params)?;
            to_value(hub.reorder_tabs(p.id, &p.channel_ids)?)
        }
        "window.show" => {
            let p: IdParam<WindowId> = parse(params)?;
            match hub.window_launch(p.id).map_err(|e| RpcError(e.to_string()))? {
                // Already on screen: bring it forward (best effort; without a WM helper it just
                // stays where it is). Never a second client for a held window.
                crate::hub::WindowLaunch::Held(name) => {
                    let raised = crate::wm::raise(&crate::wm::window_title(&name)).await;
                    return Ok(json!({ "shown": if raised { "raised" } else { "open" } }));
                }
                // Launched moments ago and not claimed yet: don't open a duplicate.
                crate::hub::WindowLaunch::Launching => return Ok(json!({ "shown": "launching" })),
                crate::hub::WindowLaunch::Go => {}
            }
            // A client can't open another native window itself: launch one, like
            // `muster open --window N` from a terminal (the daemon's environment has the display).
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(hub.muster_bin());
            cmd.args(["open", "--window", &p.id.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // SAFETY: setsid is async-signal-safe; the window outlives this request.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    hub.end_window_launch(p.id);
                    return Err(RpcError(format!("launching a window: {e}")));
                }
            };
            // `open` hands off to a detached window process and exits: a non-zero exit means the
            // launch failed. Within 3 s the caller hears about it (and falls back); later, the
            // launch record is dropped so the next `window.show` can retry. Reaped either way.
            let mut wait = tokio::task::spawn_blocking(move || child.wait());
            let failed = |r: &Result<std::io::Result<std::process::ExitStatus>, tokio::task::JoinError>| {
                matches!(r, Ok(Ok(status)) if !status.success())
            };
            match tokio::time::timeout(Duration::from_secs(3), &mut wait).await {
                Ok(r) if failed(&r) => {
                    hub.end_window_launch(p.id);
                    Err(RpcError("launching a window failed (see `muster open --window N` in a terminal)".into()))
                }
                Ok(_) => Ok(json!({ "shown": "launched" })),
                Err(_) => {
                    let (hub, id) = (hub.clone(), p.id);
                    tokio::spawn(async move {
                        let r = wait.await;
                        if failed(&r) {
                            hub.end_window_launch(id);
                            hub.notice("warn", format!("Opening window {id} failed; try `muster open --window {id}`"));
                        }
                    });
                    Ok(json!({ "shown": "launched" }))
                }
            }
        }
        "window.raise" => {
            let p: IdParam<WindowId> = parse(params)?;
            let w = hub
                .window(p.id)
                .ok_or_else(|| RpcError(format!("no such window: {}", p.id)))?;
            let raised = w.open && crate::wm::raise(&crate::wm::window_title(&w.name)).await;
            Ok(json!({ "raised": raised }))
        }

        "session.wait" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                #[serde(default)]
                until: Option<Vec<String>>,
                #[serde(default)]
                timeout_ms: Option<u64>,
                #[serde(default)]
                after_ms: Option<i64>,
            }
            let p: P = parse(params)?;
            let states = match p.until {
                Some(u) if !u.is_empty() => parse_states(&u)?,
                _ => crate::mcp::wait::DEFAULT_STATES.to_vec(),
            };
            let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(WAIT_DEFAULT_MS).min(WAIT_MAX_MS));
            session_wait(hub, p.id, &states, p.after_ms, timeout).await
        }
        "session.input" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                data: String,
                /// Typed keys: unmodified cursor keys are sent in the terminal's current
                /// cursor mode (SS3 in application mode), as a keyboard would.
                #[serde(default)]
                keys: bool,
                /// The session running `muster keys` (`$MUSTER_SESSION_ID`).
                #[serde(default)]
                from_session: Option<SessionId>,
            }
            let p: P = parse(params)?;
            // Keys typed for an agent on a non-human chain can't drive a Claude agent, and never
            // count as the human's input; keys from the human (or a human-rooted shell) do.
            let caller = caller(hub, p.from_session)?;
            let chain = match caller {
                Caller::Agent(s) => Some(crate::agents::outgoing(hub.agents().prov(s))),
                Caller::Shell(s) => hub.agents().prov(s).map(|pv| crate::agents::outgoing(Some(pv))),
                Caller::Human => None,
            };
            match chain {
                Some(r) => {
                    let target = hub.session(p.id).ok_or_else(|| RpcError(format!("no such session: {}", p.id)))?;
                    crate::mcp::guard::check_claude_origin(target.kind, r.origin).map_err(RpcError)?
                }
                // The human's keys (a paired phone, `muster keys` outside a session) take the
                // same path as a client's: no session lookup under the hub lock, no SQLite.
                None => hub.note_human_input(p.id),
            }
            let data = if p.keys && hub.application_cursor(p.id) {
                crate::pty::to_application_cursor(&p.data)
            } else {
                p.data
            };
            hub.write_input(p.id, data.as_bytes())?;
            Ok(json!({}))
        }
        "session.output" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                #[serde(default)]
                lines: Option<usize>,
            }
            let p: P = parse(params)?;
            let n = p.lines.unwrap_or(OUTPUT_DEFAULT_LINES).clamp(1, OUTPUT_MAX_LINES);
            let h = hub.clone();
            let text = tokio::task::spawn_blocking(move || h.recent_output_text(p.id, n))
                .await?
                .ok_or_else(|| RpcError(format!("no such session: {}", p.id)))?;
            let mut lines: Vec<&str> = text.lines().collect();
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
            Ok(json!({ "lines": lines }))
        }

        "fs.complete" => {
            #[derive(Deserialize)]
            struct P {
                prefix: String,
            }
            let p: P = parse(params)?;
            let entries = tokio::task::spawn_blocking(move || fs_complete(&p.prefix)).await?;
            Ok(json!({ "entries": entries }))
        }

        "device.pair_code" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                ttl_s: Option<u64>,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                scopes: Option<Vec<Scope>>,
                #[serde(default)]
                url: Option<String>,
            }
            let p: P = parse(params)?;
            let base = match p.url {
                Some(u) => u,
                None => format!(
                    "https://{}",
                    tailscale_dns_name().await.map_err(|e| RpcError(format!(
                        "{e:#}; pass the address the phone will use with --url https://<host>"
                    )))?
                ),
            };
            let origin = url_origin(&base).ok_or_else(|| RpcError(format!("not an https URL: {base}")))?;
            hub.devices().add_origin(hub.store(), &origin)?;
            let scopes = p.scopes.unwrap_or_else(|| vec![Scope::View, Scope::Interact]);
            let np = hub.devices().begin_pairing(
                p.ttl_s.unwrap_or(devices::PAIR_TTL_DEFAULT_S),
                p.name.filter(|n| !n.trim().is_empty()),
                scopes,
            );
            Ok(json!({
                "pairing_id": np.id,
                "code": np.code,
                "short_code": np.short_code,
                "url": format!("{origin}/m/#pair={}", np.code),
                "origin": origin,
                "expires_at": np.expires_at,
                "port": hub.port(),
            }))
        }
        "device.pair_status" => {
            #[derive(Deserialize)]
            struct P {
                pairing_id: u64,
            }
            let p: P = parse(params)?;
            to_value(
                hub.devices()
                    .pair_status(p.pairing_id)
                    .ok_or_else(|| RpcError(format!("no such pairing: {}", p.pairing_id)))?,
            )
        }
        "device.list" => to_value(hub.devices().list()),
        "device.revoke" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                id: Option<i64>,
                #[serde(default)]
                all: bool,
            }
            let p: P = parse(params)?;
            if p.id.is_none() && !p.all {
                return Err(RpcError("pass id or all: true".into()));
            }
            let ids = hub.devices().revoke(hub.store(), if p.all { None } else { p.id })?;
            Ok(json!({ "revoked": ids }))
        }

        "agent.create" => {
            require_local(who, method)?;
            let p: crate::agents::NewAgent = parse(params)?;
            let b = crate::agents::create(hub, p)?;
            Ok(crate::agents::agent_json(&b, None))
        }
        "agent.update" => {
            require_local(who, method)?;
            let p: crate::agents::AgentUpdate = parse(params)?;
            let b = crate::agents::update(hub, p)?;
            Ok(crate::agents::agent_json(&b, b.session_id.and_then(|s| hub.session(s)).as_ref()))
        }
        "agent.list" => {
            let out: Vec<Value> = hub
                .agents()
                .list()
                .iter()
                .map(|b| crate::agents::agent_json(b, b.session_id.and_then(|s| hub.session(s)).as_ref()))
                .collect();
            Ok(Value::Array(out))
        }
        "agent.templates" => to_value(crate::agents::templates()),
        "agent.remove" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            struct P {
                name: String,
            }
            let p: P = parse(params)?;
            let b = crate::agents::remove(hub, &p.name)?;
            Ok(json!({ "removed": crate::agents::agent_json(&b, None) }))
        }
        "agent.start" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
                #[serde(default)]
                fresh: bool,
                /// Where a new tab goes (the panel's window). Default: the usual placement.
                #[serde(default)]
                window_id: Option<WindowId>,
            }
            let p: P = parse(params)?;
            let bs = crate::agents::start(hub, &p.name, p.fresh, p.window_id).await?;
            Ok(json!({
                "agent": crate::agents::agent_json(&bs.agent, Some(&bs.session)),
                "session": bs.session,
                "started": bs.started,
            }))
        }
        "agent.messages" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                agent: Option<String>,
                #[serde(default)]
                before: Option<i64>,
                #[serde(default)]
                limit: Option<usize>,
            }
            let p: P = parse(params)?;
            Ok(crate::agents::messages(hub, p.agent.as_deref(), p.before, p.limit)?)
        }
        "agent.memory.list" | "agent.memory.read" | "agent.memory.write" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
                #[serde(default)]
                path: Option<String>,
                #[serde(default)]
                text: Option<String>,
            }
            let p: P = parse(params)?;
            let a = hub.agents().get(&p.name).ok_or_else(|| crate::agents::no_agent(hub, &p.name))?;
            let path = || p.path.as_deref().ok_or_else(|| RpcError("path is required".into()));
            match method {
                "agent.memory.list" => Ok(crate::agents::memory_list(&a)?),
                "agent.memory.read" => Ok(crate::agents::memory_read(&a, path()?)?),
                _ => {
                    require_local(who, method)?;
                    let text = p.text.as_deref().ok_or_else(|| RpcError("text is required".into()))?;
                    Ok(crate::agents::memory_write(&a, path()?, text)?)
                }
            }
        }
        "agent.ask" => {
            #[derive(Deserialize)]
            struct P {
                agent: String,
                text: String,
                #[serde(default)]
                timeout_ms: Option<u64>,
                /// The session running `muster ask` (`$MUSTER_SESSION_ID`).
                #[serde(default)]
                from_session: Option<SessionId>,
            }
            let p: P = parse(params)?;
            let timeout_ms = p.timeout_ms.unwrap_or(AGENT_ASK_DEFAULT_MS).clamp(1000, AGENT_ASK_MAX_MS);
            match caller(hub, p.from_session)? {
                // An agent's CLI ask is its ask_agent: the same origin rule, permission check,
                // rate limit and cap as the MCP tool.
                Caller::Agent(s) => {
                    let args = json!({"agent": p.agent, "text": p.text, "timeout_seconds": timeout_ms / 1000});
                    mcp_json(crate::mcp::call_as(hub, s, "ask_agent", args).await)
                }
                Caller::Shell(s) => {
                    Ok(crate::agents::ask(hub, &p.agent, p.text, Some(s), None, Duration::from_millis(timeout_ms)).await?)
                }
                Caller::Human => {
                    Ok(crate::agents::ask(hub, &p.agent, p.text, None, None, Duration::from_millis(timeout_ms)).await?)
                }
            }
        }

        "session.attach" | "session.detach" | "window.claim" => {
            Err(RpcError(format!("{method} is only available over the WebSocket")))
        }
        _ => Err(RpcError(format!("unknown method: {method}"))),
    }
}

/// Agent writes (create, update, remove, memory) are for the client token only: never a paired
/// device, whatever scopes it holds (`devices::method_scope` lists no agent method either).
fn require_local(who: &Principal, method: &str) -> Result<(), RpcError> {
    match who {
        Principal::Local => Ok(()),
        Principal::Device { .. } => Err(RpcError(format!("{method} needs the desktop client"))),
    }
}

/// Who a client call acts for. The CLI passes `$MUSTER_SESSION_ID` as `from_session`, so a
/// call an agent makes through `muster send/keys/ask/new` is that agent's: its provenance, and
/// for `send` and `ask` the MCP guards. A shell session is a human typing in a shell tab (with
/// that shell's provenance, if an agent ever typed into it). Without `from_session` the caller
/// is the human. Limit: an agent that drops the variable and calls the API with the client
/// token directly is the human as far as the daemon can tell (docs/PROTOCOL.md "Limits").
enum Caller {
    Human,
    Shell(SessionId),
    Agent(SessionId),
}

fn caller(hub: &Hub, from: Option<SessionId>) -> Result<Caller, RpcError> {
    let Some(s) = from else { return Ok(Caller::Human) };
    match hub.session(s) {
        None => Err(RpcError(format!("no such session: {s} (from_session)"))),
        Some(i) if i.kind == SessionKind::Shell => Ok(Caller::Shell(s)),
        Some(_) => Ok(Caller::Agent(s)),
    }
}

/// What a shell session's prompt carries: its provenance one hop on, else the human's.
fn shell_source(hub: &Hub, s: SessionId) -> crate::agents::Source {
    match hub.agents().prov(s) {
        Some(p) => crate::agents::Source::Chain(crate::agents::outgoing(Some(p)).prov(None)),
        None => crate::agents::Source::Human,
    }
}

/// Provenance for a session a CLI call starts (`muster new` inside a session): the calling
/// session's, one hop on, with the Claude origin rule. None: the human's.
fn spawn_prov(
    hub: &Hub,
    from: Option<SessionId>,
    kind: Option<SessionKind>,
) -> Result<Option<crate::agents::Prov>, RpcError> {
    let route = match caller(hub, from)? {
        Caller::Human => return Ok(None),
        Caller::Shell(s) => match hub.agents().prov(s) {
            None => return Ok(None),
            p => crate::agents::outgoing(p),
        },
        Caller::Agent(s) => crate::agents::outgoing(hub.agents().prov(s)),
    };
    if let Some(k) = kind {
        crate::mcp::guard::check_claude_origin(k, route.origin).map_err(RpcError)?;
    }
    Ok(Some(route.prov(None)))
}

/// An MCP tool's text result as the RPC result (JSON when it is JSON).
fn mcp_json(r: Result<String, String>) -> RpcResult {
    let text = r.map_err(RpcError)?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// `until` names -> states. `needs_human` is every state that needs the human; `done` is an
/// alias for `idle`.
fn parse_states(names: &[String]) -> Result<Vec<AgentState>, RpcError> {
    let mut out: Vec<AgentState> = Vec::new();
    for n in names {
        let add: Vec<AgentState> = match n.trim() {
            "needs_human" => vec![
                AgentState::NeedsApproval,
                AgentState::AskingQuestion,
                AgentState::Error,
                AgentState::Stuck,
            ],
            "done" => vec![AgentState::Idle],
            other => vec![
                serde_json::from_value(Value::String(other.to_string()))
                    .map_err(|_| RpcError(format!("unknown state: {other}")))?,
            ],
        };
        for s in add {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

/// `session.wait`: block until the session is in one of `states` (or exits, or `timeout`).
/// Without `after_ms` any matching state counts at once; with it, the MCP `wait_for_state`
/// staleness rules apply (a state entered before `after_ms` doesn't count, see mcp::wait).
async fn session_wait(
    hub: &Arc<Hub>,
    id: SessionId,
    states: &[AgentState],
    after_ms: Option<i64>,
    timeout: Duration,
) -> RpcResult {
    use crate::mcp::wait::{Verdict, WaitCtx, evaluate, next_change};
    let mut rx = hub.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let info = hub
            .session(id)
            .ok_or_else(|| RpcError(format!("no such session: {id}")))?;
        let ctx = WaitCtx {
            after_ms: Some(after_ms.unwrap_or(0)),
            last_send_ms: None,
            last_delivery_ms: hub.last_dequeue_at(id),
            settle_grace_ms: 15_000,
            now_ms: now_ms(),
        };
        let verdict = evaluate(&info, states, &ctx);
        let now = tokio::time::Instant::now();
        if verdict != Verdict::Wait || now >= deadline {
            return Ok(json!({
                "session": info,
                "matched": verdict == Verdict::Reached,
                "timed_out": verdict == Verdict::Wait,
            }));
        }
        next_change(&mut rx, id, (deadline - now).min(Duration::from_secs(1))).await;
    }
}

/// Directory completions for a path prefix. Keeps a leading `~` as typed.
pub fn fs_complete(prefix: &str) -> Vec<String> {
    const MAX: usize = 100;
    let prefix = if prefix.is_empty() { "~/" } else { prefix };
    let prefix = if prefix == "~" { "~/" } else { prefix };
    let (dir_part, partial) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("./", prefix),
    };
    let dir = crate::paths::expand_tilde(dir_part.trim_end_matches('/'));
    let dir = if dir_part == "/" {
        std::path::PathBuf::from("/")
    } else {
        dir
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let show_hidden = partial.starts_with('.');
    let lower = partial.to_lowercase();
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| (show_hidden || !n.starts_with('.')) && n.to_lowercase().starts_with(&lower))
        .map(|n| format!("{dir_part}{n}/"))
        .collect();
    out.sort_by_key(|s| s.to_lowercase());
    out.truncate(MAX);
    out
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn response(id: &Value, r: RpcResult) -> Value {
    match r {
        Ok(v) => json!({ "id": id, "result": v }),
        Err(RpcError(m)) => json!({ "id": id, "error": { "message": m } }),
    }
}

/// `tailscale status --json` -> `Self.DNSName` without the trailing dot.
async fn tailscale_dns_name() -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("tailscale")
            .args(["status", "--json"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("`tailscale status` timed out"))?
    .map_err(|e| anyhow::anyhow!("running `tailscale status`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("`tailscale status` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let name = v["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.');
    if name.is_empty() {
        anyhow::bail!("tailscale reports no DNS name for this machine (is MagicDNS on?)");
    }
    Ok(name.to_string())
}

/// `https://host[:port]/...` -> `https://host[:port]` (lowercased host). None for anything else.
pub fn url_origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    let ok = !host.is_empty()
        && !host.contains('@')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    ok.then(|| format!("https://{host}"))
}

/// `POST /api/rpc` body handler: `{method, params, id?}` -> `{id, result}` / `{id, error}`.
pub async fn http_call(hub: &Arc<Hub>, who: &Principal, body: &[u8]) -> Value {
    match serde_json::from_slice::<Request>(body) {
        Ok(req) => {
            let r = match authorize(hub, who, &req.method, &req.params) {
                Ok(()) => dispatch(hub, who, &req.method, req.params).await,
                Err(e) => Err(e),
            };
            response(&req.id, r)
        }
        Err(e) => response(&Value::Null, Err(RpcError(format!("invalid request: {e}")))),
    }
}

type Outbox = mpsc::Sender<Message>;

/// One WebSocket client connection, authenticated as `who`.
pub async fn ws_session(socket: WebSocket, hub: Arc<Hub>, who: Principal) {
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUTBOX_CAP);

    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Server events -> client. If this client falls too far behind the event bus, close the
    // connection: the client reconnects and re-syncs with `hello`.
    // Server events and method responses both go through this task, so a response is sent
    // only after every event emitted before it (e.g. `workspace_upserted` before the
    // `workspace.add` result). If this client falls too far behind the event bus, close the
    // connection: the client reconnects and re-syncs with `hello`.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Message>(OUTBOX_CAP);
    let is_device = matches!(who, Principal::Device { .. });
    let events = {
        let out = out_tx.clone();
        let mut rx = hub.subscribe();
        tokio::spawn(async move {
            // Named agents aren't on the phone (no agent.* method is either; docs/ANDROID.md).
            let forward = |ev: ServerEvent| {
                if is_device
                    && matches!(
                        ev,
                        ServerEvent::AgentUpserted(_) | ServerEvent::AgentRemoved { .. } | ServerEvent::AgentMessage(_)
                        | ServerEvent::PanelUpserted(_) | ServerEvent::PanelRemoved { .. }
                    )
                {
                    return None;
                }
                serde_json::to_string(&ev).ok().map(|t| Message::Text(t.into()))
            };
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(ev) => {
                            if let Some(m) = forward(ev)
                                && out.send(m).await.is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("client lagged {n} events behind; closing so it resyncs");
                            let _ = out.send(Message::Close(None)).await;
                            break;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    resp = resp_rx.recv() => {
                        let Some(resp) = resp else { break };
                        // Flush events that were emitted before this response was produced.
                        loop {
                            match rx.try_recv() {
                                Ok(ev) => {
                                    if let Some(m) = forward(ev) {
                                        let _ = out.send(m).await;
                                    }
                                }
                                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                                    let _ = out.send(Message::Close(None)).await;
                                    return;
                                }
                                Err(_) => break,
                            }
                        }
                        if out.send(resp).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };

    let attachments: Arc<Mutex<HashMap<SessionId, JoinHandle<()>>>> = Arc::default();
    let device_id = match who {
        Principal::Device { id } => Some(id),
        Principal::Local => None,
    };
    // A device's socket closes the moment it is revoked or expires, even while idle or while
    // its reader has stopped reading (the watchdog kills the writer, which unblocks any send
    // stuck on a full outbox). Subscribed before the first check, so a revocation between the
    // cookie check and this point is not missed.
    let closed = Arc::new(tokio::sync::Notify::new());
    let watchdog = device_id.map(|id| {
        let (hub, closed) = (hub.clone(), closed.clone());
        let writer = writer.abort_handle();
        let events = events.abort_handle();
        let mut revoked = hub.devices().subscribe_revoked();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                if hub.devices().scope(id).is_none() {
                    break;
                }
                tokio::select! {
                    r = revoked.recv() => if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                        break;
                    },
                    _ = tick.tick() => {}
                }
            }
            writer.abort();
            events.abort();
            closed.notify_one();
        })
    });

    loop {
        let msg = tokio::select! {
            m = stream.next() => m,
            _ = closed.notified() => break,
        };
        let Some(Ok(msg)) = msg else { break };
        if let Some(id) = device_id
            && hub.devices().scope(id).is_none()
        {
            break;
        }
        match msg {
            Message::Text(text) => {
                let req: Request = match serde_json::from_str(text.as_str()) {
                    Ok(r) => r,
                    Err(e) => {
                        let v = response(&Value::Null, Err(RpcError(format!("invalid request: {e}"))));
                        let _ = out_tx.send(Message::Text(v.to_string().into())).await;
                        continue;
                    }
                };
                if let Err(e) = authorize(&hub, &who, &req.method, &req.params) {
                    let _ = out_tx.send(Message::Text(response(&req.id, Err(e)).to_string().into())).await;
                    continue;
                }
                handle_request(&hub, &who, conn, req, &out_tx, &resp_tx, &attachments).await;
            }
            Message::Binary(data) => {
                if device_id.is_some() && authorize(&hub, &who, "session.input", &Value::Null).is_err() {
                    continue;
                }
                if data.len() >= 5 && data[0] == FRAME_INPUT {
                    let id = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                    // Keystrokes from a client window or a paired phone: the human's input.
                    // The marker is recorded in memory and committed off this path, so a
                    // keystroke never waits on SQLite or the callback lock (Hub::note_human_input).
                    hub.note_human_input(id);
                    if let Err(e) = hub.send_keys(id, &data[5..]) {
                        // Lost or refused keystrokes interrupt their run, which the user sees
                        // on the session (`input_interrupted`) and in a notice.
                        tracing::debug!("input for session {id}: {e:#}");
                        // The backend is stopping: stop reading, so the client shows it is
                        // disconnected (typing paused) instead of sending into a closed link.
                        if hub.input_closed() { break }
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    for (_, h) in attachments.lock().drain() {
        h.abort();
    }
    if let Some(w) = watchdog {
        w.abort();
    }
    hub.release_windows(conn);
    drop(resp_tx);
    events.abort();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_request(
    hub: &Arc<Hub>,
    who: &Principal,
    conn: u64,
    req: Request,
    out: &Outbox,
    resp: &Outbox,
    attachments: &Arc<Mutex<HashMap<SessionId, JoinHandle<()>>>>,
) {
    match req.method.as_str() {
        "session.attach" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                /// Replay at most the last this many bytes of output (phones; see
                /// docs/PROTOCOL.md). Default: the whole buffer.
                #[serde(default)]
                replay_bytes: Option<usize>,
            }
            let (id, limit) = match parse::<P>(req.params) {
                Ok(p) => (p.id, p.replay_bytes.map(|b| b.max(REPLAY_BYTES_MIN))),
                Err(e) => {
                    let _ = out
                        .send(Message::Text(response(&req.id, Err(e)).to_string().into()))
                        .await;
                    return;
                }
            };
            // Stop a previous forwarder for this session first, so it can't enqueue bytes
            // that the new replay already contains.
            let old = attachments.lock().remove(&id);
            if let Some(old) = old {
                old.abort();
                let _ = old.await;
            }
            // Subscribe to the bus before snapshotting so a restart can't slip between.
            let bus = hub.subscribe();
            match hub.attach(id, limit).await {
                Ok(att) => {
                    // Result first, then the replay frame, then live frames (all through the
                    // same ordered outbox, all queued before this call returns).
                    let _ = out
                        .send(Message::Text(response(&req.id, Ok(json!({}))).to_string().into()))
                        .await;
                    let (replay, rx, generation) = match att {
                        Attach::Live(r, rx, g) => (r, Some(rx), Some(g)),
                        Attach::Replay(r) => (r, None, None),
                    };
                    let _ = out.send(Message::Binary(frame(FRAME_REPLAY, id, &replay))).await;
                    let task = tokio::spawn(forward_output(hub.clone(), id, limit, generation, rx, bus, out.clone()));
                    if let Some(other) = attachments.lock().insert(id, task) {
                        other.abort();
                    }
                }
                Err(e) => {
                    let _ = out
                        .send(Message::Text(response(&req.id, Err(e.into())).to_string().into()))
                        .await;
                }
            }
        }
        "window.claim" => {
            #[derive(Deserialize)]
            struct P {
                #[serde(default)]
                id: Option<WindowId>,
            }
            // Through the response queue, after the events it emits.
            let r = parse::<P>(req.params).and_then(|p| to_value(hub.claim_window(conn, p.id)?));
            let _ = resp.send(Message::Text(response(&req.id, r).to_string().into())).await;
        }
        "session.detach" => {
            let r = parse::<IdParam<SessionId>>(req.params).map(|p| {
                if let Some(h) = attachments.lock().remove(&p.id) {
                    h.abort();
                }
                json!({})
            });
            let _ = out.send(Message::Text(response(&req.id, r).to_string().into())).await;
        }
        _ => {
            // Run concurrently: some methods (send, diff, restart) take a while.
            let hub = hub.clone();
            let who = who.clone();
            let resp = resp.clone();
            tokio::spawn(async move {
                // A `session.wait` stops once the connection is gone. Other methods run to
                // completion (cancelling one halfway could leave state behind).
                let r = if matches!(req.method.as_str(), "session.wait" | "panel.wait") {
                    tokio::select! {
                        r = dispatch(&hub, &who, &req.method, req.params) => r,
                        _ = resp.closed() => return,
                    }
                } else {
                    dispatch(&hub, &who, &req.method, req.params).await
                };
                let _ = resp.send(Message::Text(response(&req.id, r).to_string().into())).await;
            });
        }
    }
}

/// Streams live output for one attached session. Re-sends a replay if the client falls
/// behind, and follows the session across restarts (new process = new replay).
async fn forward_output(
    hub: Arc<Hub>,
    id: SessionId,
    limit: Option<usize>,
    mut generation: Option<u64>,
    mut rx: Option<broadcast::Receiver<crate::terminal::Output>>,
    mut bus: broadcast::Receiver<ServerEvent>,
    out: Outbox,
) {
    loop {
        tokio::select! {
            chunk = async {
                match rx.as_mut() {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            } => match chunk {
                Ok(output) => {
                    let (kind, bytes) = match output {
                        crate::terminal::Output::Bytes(bytes) => (FRAME_OUTPUT, bytes),
                        crate::terminal::Output::Replay(bytes) => (FRAME_REPLAY, bytes),
                    };
                    if out.send(Message::Binary(frame(kind, id, &bytes))).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow client: start over from a fresh replay.
                    if !resend_replay(&hub, id, limit, &mut generation, &mut rx, &out).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => rx = None,
            },
            ev = bus.recv() => match ev {
                // Restarted: a new process with an empty replay buffer. Compared by process
                // generation, not pid (a pid can be reused).
                Ok(ServerEvent::SessionUpserted(info))
                    if info.id == id && hub.pty_generation(id).is_some_and(|g| Some(g) != generation) =>
                {
                    if !resend_replay(&hub, id, limit, &mut generation, &mut rx, &out).await {
                        return;
                    }
                }
                Ok(ServerEvent::SessionRemoved { id: rid }) if rid == id => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

async fn resend_replay(
    hub: &Arc<Hub>,
    id: SessionId,
    limit: Option<usize>,
    generation: &mut Option<u64>,
    rx: &mut Option<broadcast::Receiver<crate::terminal::Output>>,
    out: &Outbox,
) -> bool {
    let (replay, new_rx, new_gen) = match hub.attach(id, limit).await {
        Ok(Attach::Live(r, rx, g)) => (r, Some(rx), Some(g)),
        Ok(Attach::Replay(r)) => (r, None, None),
        Err(_) => return false,
    };
    *generation = new_gen;
    *rx = new_rx;
    out.send(Message::Binary(frame(FRAME_REPLAY, id, &replay)))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{NewAgent, Origin, Prov};
    use crate::mcp::McpState;
    use crate::mcp::backend::HubBackend;
    use crate::store::{Store, StoredSession};

    /// A hub with workspace 1, channel 1 and exited sessions 1 (shell), 2 (claude), 3 (codex),
    /// with the MCP server state attached. Nothing here starts a process: every call below is
    /// refused before delivery.
    fn fixture() -> (Arc<Hub>, Arc<McpState<HubBackend>>) {
        let store = Store::open_memory().unwrap();
        let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
        let mut ch = crate::mcp::guard::test_channel(0);
        ch.workspace_id = ws;
        ch.window_id = Some(1);
        store.insert_channel(&ch).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        for kind in [SessionKind::Shell, SessionKind::Claude, SessionKind::Codex] {
            let mut info = crate::mcp::guard::test_session(0, None);
            info.kind = kind;
            info.window_id = Some(1);
            store.insert_session(&StoredSession { info, extra_args: vec![] }).unwrap();
        }
        let cfg = crate::hub::HubConfig {
            port: 0,
            client_token: "t".into(),
            muster_bin: std::path::PathBuf::from("/bin/true"),
            terminal_host: None,
        };
        let hub = Hub::new(cfg, store).unwrap();
        let st = Arc::new(McpState::new(HubBackend(hub.clone())));
        hub.set_mcp(Arc::downgrade(&st));
        (hub, st)
    }

    fn agent(hub: &Hub, name: &str) {
        crate::agents::create(
            hub,
            NewAgent {
                account_profile: None,
                name: name.into(),
                kind: None,
                model: None,
                effort: None,
                persona: None,
                accepts: None,
                permissions: None,
                autostart: false,
            },
        )
        .unwrap();
    }

    async fn call(hub: &Arc<Hub>, method: &str, params: Value) -> Result<Value, String> {
        dispatch(hub, &Principal::Local, method, params).await.map_err(|e| e.0)
    }

    const SCHED: Prov = Prov { origin: Origin::Schedule, hop: 1, msg_id: None };

    #[tokio::test]
    async fn cli_ask_from_an_agent_is_that_agents_ask_agent() {
        let (hub, st) = fixture();
        agent(&hub, "rpc-ada");
        // The codex session handles a scheduled message: its `muster ask` carries that, and no
        // parameter can claim otherwise.
        hub.agents().set_prov(3, Some(SCHED)).unwrap();
        let e = call(&hub, "agent.ask", json!({"agent": "rpc-ada", "text": "hi", "from_session": 3, "origin": "human"}))
            .await
            .unwrap_err();
        assert!(e.contains("accepts only human"), "{e}");
        // Human-rooted again: the MCP send rate limit applies to its CLI asks...
        hub.agents().set_prov(3, None).unwrap();
        for _ in 0..20 {
            st.guard.lock().sends.check_and_record(3, now_ms(), 20, 60_000).unwrap();
        }
        let e = call(&hub, "agent.ask", json!({"agent": "rpc-ada", "text": "hi", "from_session": 3})).await.unwrap_err();
        assert!(e.contains("rate limit"), "{e}");
        // ...and so does the permission check (the caller's tab is narrower than the agent's).
        st.guard.lock().sends.forget(2);
        let plan = Permissions { claude_mode: ClaudeMode::Plan, ..Default::default() };
        hub.set_permissions(1, plan).unwrap();
        let e = call(&hub, "agent.ask", json!({"agent": "rpc-ada", "text": "hi", "from_session": 2})).await.unwrap_err();
        assert!(e.contains("more than your channel"), "{e}");
        // The call is audited like an MCP call.
        let tl = hub.timeline(1, None, 100);
        assert!(tl.iter().any(|e| e.kind == EventKind::McpCall && e.data["args"]["via"] == "cli"));
    }

    #[tokio::test]
    async fn cli_sends_keys_and_spawns_carry_provenance() {
        let (hub, _st) = fixture();
        // An agent's `muster send` is its send_message (guards included: no shell targets).
        let e = call(&hub, "session.send", json!({"id": 1, "text": "ls", "from_session": 3})).await.unwrap_err();
        assert!(e.contains("is a shell"), "{e}");
        // Keys from a session on a scheduled chain can't drive a Claude agent.
        hub.agents().set_prov(1, Some(SCHED)).unwrap();
        let e = call(&hub, "session.input", json!({"id": 2, "data": "x", "from_session": 1})).await.unwrap_err();
        assert!(e.contains("origin=schedule"), "{e}");
        // Nor can `muster new` / session.spawn start one.
        hub.agents().set_prov(3, Some(SCHED)).unwrap();
        let e = call(&hub, "session.spawn", json!({"channel_id": 1, "kind": "claude", "from_session": 3}))
            .await
            .unwrap_err();
        assert!(e.contains("origin=schedule"), "{e}");
        let e = call(&hub, "channel.create", json!({"workspace_id": 1, "agent": "claude", "from_session": 3}))
            .await
            .unwrap_err();
        assert!(e.contains("origin=schedule"), "{e}");
        // The human's keystrokes (no from_session) make the next plain prompt the human's.
        hub.agents().set_prov(2, Some(SCHED)).unwrap();
        let _ = call(&hub, "session.input", json!({"id": 2, "data": "x"})).await;
        // The marker is committed off the input path (the daemon's attribution worker).
        hub.flush_human_input().unwrap();
        let info = hub.session(2).unwrap();
        hub.agents().on_hook(hub.store(), &info, "UserPromptSubmit", &json!({"prompt": "new task"})).unwrap();
        assert_eq!(hub.agents().prov(2), None);
    }

    /// Every event of kind `kind` (`agent_upserted`, ...) received so far, as JSON.
    fn drain(rx: &mut broadcast::Receiver<ServerEvent>, kind: &str) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            let v = serde_json::to_value(&ev).unwrap();
            if v["event"] == kind {
                out.push(v["data"].clone());
            }
        }
        out
    }

    #[tokio::test]
    async fn agent_update_edits_the_record_persona_and_tab_profile() {
        let (hub, _st) = fixture();
        let mut rx = hub.subscribe();
        agent(&hub, "rpc-eve");
        assert_eq!(drain(&mut rx, "agent_upserted").len(), 1);
        let r = call(
            &hub,
            "agent.update",
            json!({"name": "RPC-EVE", "persona": "Writes docs.", "autostart": true, "model": "opus",
                   "accepts": ["human", "schedule"],
                   "permissions": {"claude_mode": "plan", "codex_sandbox": "read_only"}}),
        )
        .await
        .unwrap();
        assert_eq!((r["autostart"].clone(), r["model"].clone()), (json!(true), json!("opus")));
        assert_eq!(r["permissions"]["claude_mode"], "plan");
        assert_eq!(r["accepts_origins"], json!(["human", "schedule"]));
        let persona = std::fs::read_to_string(std::path::Path::new(r["home_dir"].as_str().unwrap()).join("CLAUDE.md")).unwrap();
        assert!(persona.contains("Writes docs."));
        assert_eq!(drain(&mut rx, "agent_upserted").len(), 1);
        // Absent fields stay; an empty model clears it; the kind can't change; bad values refused.
        let r = call(&hub, "agent.update", json!({"name": "rpc-eve", "model": ""})).await.unwrap();
        assert!(r["model"].is_null() && r["autostart"] == true && r["persona"] == "Writes docs.");
        assert!(call(&hub, "agent.update", json!({"name": "rpc-eve", "kind": "codex"})).await.is_err());
        assert!(call(&hub, "agent.update", json!({"name": "rpc-eve", "accepts": []})).await.is_err());
        assert!(call(&hub, "agent.update", json!({"name": "rpc-eve", "effort": "--evil"})).await.is_err());
        assert!(call(&hub, "agent.update", json!({"name": "nobody"})).await.unwrap_err().contains("no agent named"));
        let t = call(&hub, "agent.templates", json!({})).await.unwrap();
        assert_eq!(t.as_array().unwrap().len(), 6);
        call(&hub, "agent.remove", json!({"name": "rpc-eve"})).await.unwrap();
        assert_eq!(drain(&mut rx, "agent_removed")[0]["name"], "rpc-eve");
    }

    #[tokio::test]
    async fn refused_asks_are_logged_and_listed() {
        let (hub, _st) = fixture();
        agent(&hub, "rpc-ivy");
        let mut rx = hub.subscribe();
        // An agent session on a scheduled chain asks a Claude agent: refused, and kept.
        hub.agents().set_prov(3, Some(SCHED)).unwrap();
        let e = call(&hub, "agent.ask", json!({"agent": "rpc-ivy", "text": "nightly report\nmore", "from_session": 3}))
            .await
            .unwrap_err();
        assert!(e.contains("accepts only human"), "{e}");
        // The human's CLI path too (forced origins are daemon-internal; a shell on that chain).
        hub.agents().set_prov(1, Some(SCHED)).unwrap();
        assert!(call(&hub, "agent.ask", json!({"agent": "rpc-ivy", "text": "from a shell", "from_session": 1})).await.is_err());
        let ev = drain(&mut rx, "agent_message");
        assert_eq!(ev.len(), 2);
        let r = call(&hub, "agent.messages", json!({"agent": "rpc-ivy"})).await.unwrap();
        let ms = r["messages"].as_array().unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!((ms[0]["text"].as_str(), ms[0]["from"].as_str()), (Some("from a shell"), Some("session:1")));
        assert_eq!((ms[1]["to"].as_str(), ms[1]["origin"].as_str(), ms[1]["hop"].as_u64()), (Some("rpc-ivy"), Some("schedule"), Some(2)));
        assert!(ms[1]["refused"].as_str().unwrap().contains("accepts only human"));
        // Paged, newest first.
        let r = call(&hub, "agent.messages", json!({"limit": 1})).await.unwrap();
        assert_eq!((r["messages"].as_array().unwrap().len(), r["more"].clone()), (1, json!(true)));
        let before = r["messages"][0]["id"].as_i64().unwrap();
        let r = call(&hub, "agent.messages", json!({"before": before})).await.unwrap();
        assert_eq!((r["messages"].as_array().unwrap().len(), r["more"].clone()), (1, json!(false)));
        // Refusals don't use up the daily cap.
        let id = hub.agents().get("rpc-ivy").unwrap().id;
        assert_eq!(hub.store().count_agent_messages_since(id, 0).unwrap(), 0);
    }

    #[tokio::test]
    async fn agent_memory_over_rpc_and_device_scope() {
        let (hub, _st) = fixture();
        agent(&hub, "rpc-max");
        call(&hub, "agent.memory.write", json!({"name": "rpc-max", "path": "people/ada.md", "text": "likes tests"}))
            .await
            .unwrap();
        let r = call(&hub, "agent.memory.read", json!({"name": "rpc-max", "path": "people/ada.md"})).await.unwrap();
        assert_eq!(r["text"], "likes tests");
        let r = call(&hub, "agent.memory.list", json!({"name": "rpc-max"})).await.unwrap();
        assert!(r["files"].as_array().unwrap().iter().any(|f| f["path"] == "people/ada.md"));
        assert!(call(&hub, "agent.memory.read", json!({"name": "rpc-max", "path": "../CLAUDE.md"})).await.is_err());
        assert!(call(&hub, "agent.memory.write", json!({"name": "rpc-max", "path": "x.md"})).await.is_err());
        // No agent method is ever a device's, and writes need the desktop client token.
        for m in [
            "agent.create", "agent.update", "agent.list", "agent.remove", "agent.start", "agent.ask",
            "agent.templates", "agent.messages", "agent.memory.list", "agent.memory.read", "agent.memory.write",
        ] {
            assert!(devices::method_scope(m).is_none(), "{m}");
        }
        let phone = Principal::Device { id: 1 };
        assert!(authorize(&hub, &phone, "agent.list", &json!({})).is_err());
        assert!(require_local(&phone, "agent.update").is_err());
        let e = dispatch(&hub, &phone, "agent.memory.write", json!({"name": "rpc-max", "path": "y.md", "text": "x"}))
            .await
            .unwrap_err();
        assert!(e.0.contains("desktop client"), "{}", e.0);
    }
    #[tokio::test]
    async fn restart_and_shutdown_are_human_only() {
        let (hub, _st) = fixture();
        let device = Principal::Device { id: 1 };
        for method in ["daemon.reload", "daemon.shutdown"] {
            assert!(crate::devices::method_scope(method).is_none(), "{method}");
            let e = dispatch(&hub, &device, method, json!({})).await.unwrap_err();
            assert!(e.0.contains("only available on this computer"), "{}", e.0);
            let e = call(&hub, method, json!({"from_session": 2})).await.unwrap_err();
            assert!(e.contains("can't be called from a session"), "{e}");
        }
        // Without the offered build, or a known launch path, nothing is started.
        let e = call(&hub, "daemon.reload", json!({})).await.unwrap_err();
        assert!(e.contains("invalid params"), "{e}");
        let e = call(&hub, "daemon.reload", json!({"expect": "abcdefabcdef"})).await.unwrap_err();
        assert!(e.contains("where it was launched from"), "{e}");
        let h = call(&hub, "hello", json!({})).await.unwrap();
        assert_eq!(h["updates"], json!({"ui_release": null, "backend": null, "host": null}));
    }

    #[tokio::test]
    async fn window_prefs_persist_and_broadcast() {
        let (hub, _st) = fixture();
        let mut rx = hub.subscribe();
        assert_eq!(call(&hub, "prefs.get", json!({})).await.unwrap(), json!({"decorations": true, "revision": 0}));
        let set = json!({"decorations": false, "revision": 1});
        assert_eq!(call(&hub, "prefs.set", json!({"decorations": false})).await.unwrap(), set);
        assert_eq!(call(&hub, "hello", json!({})).await.unwrap()["prefs"], set);
        let ev = serde_json::to_value(rx.recv().await.unwrap()).unwrap();
        assert_eq!(ev, json!({"event": "prefs_changed", "data": set}));
        // Fields not given stay (each write still counts); unknown ones are refused.
        assert_eq!(call(&hub, "prefs.set", json!({})).await.unwrap(), json!({"decorations": false, "revision": 2}));
        assert!(call(&hub, "prefs.set", json!({"theme": "dark"})).await.is_err());
    }

    #[tokio::test]
    async fn shell_pref_resolves_validates_and_clears() {
        let (hub, _st) = fixture();
        let sh = crate::adapters::shell::resolve_shell("sh").unwrap();
        assert!(sh.starts_with('/') && sh.ends_with("/sh"), "{sh}");
        let set = call(&hub, "prefs.set", json!({"shell": "sh"})).await.unwrap();
        assert_eq!(set["shell"], json!(sh));
        // Not a program: refused, and the stored choice stays.
        for bad in ["/etc/hostname-not-here", "/etc", "no-such-shell-xyz"] {
            assert!(call(&hub, "prefs.set", json!({"shell": bad})).await.is_err(), "{bad}");
        }
        assert_eq!(call(&hub, "prefs.get", json!({})).await.unwrap()["shell"], json!(sh));
        // "" goes back to the login shell: the field disappears.
        let cleared = call(&hub, "prefs.set", json!({"shell": ""})).await.unwrap();
        assert_eq!(cleared, json!({"decorations": true, "revision": 2}));
        let hello = call(&hub, "hello", json!({})).await.unwrap();
        assert!(hello["login_shell"].as_str().is_some_and(|s| s.starts_with('/')));
        // A program the backend runs: never chosen from a paired device.
        let e = dispatch(&hub, &Principal::Device { id: 1 }, "prefs.set", json!({"shell": "sh"})).await.unwrap_err();
        assert!(e.0.contains("only available on this computer"), "{}", e.0);
    }
}
