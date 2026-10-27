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
/// A `screen` replay's size when the client gives no `replay_bytes` (docs/PROTOCOL.md).
const SCREEN_REPLAY_BYTES: usize = 64 * 1024;
/// A device's socket closes after this long without any frame from it. The daemon pings
/// every 30 s (browsers answer on their own) and the phone pings too, so only a phone that is
/// gone (out of coverage, killed) stays silent this long.
const DEVICE_SILENCE: Duration = Duration::from_secs(90);
/// A device's socket closes when one frame to it takes longer than this to send: a link that
/// swallows everything (out of coverage) or a client that stopped reading would otherwise hold
/// the connection, its queue and its session tails until TCP gives up.
const DEVICE_WRITE_STALL: Duration = Duration::from_secs(30);
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
        "account_usage": hub.account_usage().ok(),
        "channels": hub.channels(),
        "sessions": hub.sessions(),
        // Which sessions are named agents' (`session.remove` hides those instead of stopping
        // them), so a client without `agent.list` (a paired phone) can say so before it closes one,
        // and (`agent_names`, keyed by session id) under the agent's own name.
        "agent_sessions": hub.agents().list().into_iter().filter_map(|a| a.session_id).collect::<Vec<_>>(),
        "agent_names": hub.agents().list().into_iter()
            .filter_map(|a| a.session_id.map(|id| (id.to_string(), json!(a.name))))
            .collect::<serde_json::Map<_, _>>(),
        "panels": crate::panels::list(hub).unwrap_or_default().into_iter().filter(|p| !p.closed).collect::<Vec<_>>(),
        "panel_origin": format!("http://127.0.0.1:{}", hub.panels.port.load(Ordering::Relaxed)),
        "notes": crate::notes::list(hub).unwrap_or_default(),
        "windows": hub.windows(),
        "home": crate::paths::home_dir().to_string_lossy(),
        "default_shell": crate::adapters::shell::user_shell(),
        "login_shell": crate::adapters::shell::fallback_shell(),
        "paused_deliveries": inject::paused(hub),
        "terminal_host": hub.terminal_host_info(),
        "updates": hub.updates().status(),
        // The running build's content id, so a restart can tell the build it started answers.
        "build": hub.updates().build(),
        "prefs": hub.ui_prefs(),
        // The backend's own pid, so a CLI that can't see it (a sandboxed command in its own PID
        // namespace) can still tell the discovery file names this backend.
        "pid": std::process::id(),
        // Whether the backend has an OpenRouter key; never the key itself.
        "openrouter": { "key_present": hub.openrouter_key().is_some() },
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
/// Callers check `authorize` first. Anything but a click in Muster's window (`Via::Outside`).
pub async fn dispatch(hub: &Arc<Hub>, who: &Principal, method: &str, params: Value) -> RpcResult {
    dispatch_via(hub, who, method, params, Via::Outside).await
}

/// Where a call that could make or widen unattended work came from (`from_window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// A click in the owner's window: a WebSocket of Muster's own UI, holding a window, whose
    /// program is known and runs outside every session.
    Window,
    /// Anything else: the CLI, `/api/rpc`, a script's WebSocket, a connection Muster can't
    /// attribute. A new or widened schedule waits for the owner's click (`schedules::Held`); a
    /// setting that loosens a limit is refused.
    Outside,
}

/// `dispatch`, told where the call came from.
pub async fn dispatch_via(hub: &Arc<Hub>, who: &Principal, method: &str, params: Value, via: Via) -> RpcResult {
    match method {
        // A liveness check: the phone calls it while visible to notice a dead socket.
        "ping" => Ok(json!({})),
        "hello" => {
            let mut result = hello(hub);
            if matches!(who, Principal::Device { .. }) {
                if let Some(fields) = result.as_object_mut() {
                    fields.remove("panels");
                    fields.remove("panel_origin");
                    fields.remove("notes");
                }
            }
            Ok(result)
        }

        // Notes (docs/PROTOCOL.md "Notes"). `from_session` (the CLI inside a session) only says
        // who wrote; without it the change is the human's.
        "note.list" => to_value(crate::notes::list(hub)?),
        "note.search" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { query: String }
            let p: P = parse(params)?;
            Ok(json!(crate::notes::search(hub, &p.query)?))
        }
        "note.get" => {
            let p: IdParam<NoteId> = parse(params)?;
            to_value(crate::notes::get(hub, p.id)?)
        }
        // An agent's writes through the CLI run as its MCP note tools: the same rate limit and
        // audit as over MCP (docs/MCP.md "Notes").
        "note.create" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { #[serde(default)] title: Option<String>, #[serde(default)] body: String,
                #[serde(default)] window_id: Option<WindowId>, #[serde(default)] from_session: Option<SessionId> }
            let p: P = parse(params)?;
            if let Some(s) = agent_session(hub, p.from_session)? {
                let mut n = agent_note(hub, s, "note_write", json!({"title": p.title, "text": p.body})).await?;
                if let Some(w) = p.window_id { n = crate::notes::open(hub, n.id, w, None)?; }
                return to_value(n);
            }
            to_value(crate::notes::create(hub, p.title.as_deref(), &p.body, crate::notes::author(hub, p.from_session)?, p.window_id, None)?)
        }
        "note.write" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { id: NoteId, body: String, base_rev: i64, #[serde(default)] title: Option<String>,
                #[serde(default)] from_session: Option<SessionId> }
            let p: P = parse(params)?;
            if let Some(s) = agent_session(hub, p.from_session)? {
                return to_value(agent_note(hub, s, "note_write",
                    json!({"note_id": p.id, "text": p.body, "rev": p.base_rev, "title": p.title})).await?);
            }
            to_value(crate::notes::write(hub, p.id, &p.body, p.base_rev, crate::notes::author(hub, p.from_session)?, p.title.as_deref())?)
        }
        "note.append" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { id: NoteId, text: String, #[serde(default)] from_session: Option<SessionId> }
            let p: P = parse(params)?;
            if let Some(s) = agent_session(hub, p.from_session)? {
                return to_value(agent_note(hub, s, "note_append", json!({"note_id": p.id, "text": p.text})).await?);
            }
            to_value(crate::notes::append(hub, p.id, &p.text, crate::notes::author(hub, p.from_session)?)?)
        }
        "note.rename" => {
            let p: Rename<NoteId> = parse(params)?;
            to_value(crate::notes::rename(hub, p.id, &p.name)?)
        }
        "note.open" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { id: NoteId, #[serde(default)] window_id: Option<WindowId>, #[serde(default)] beside: Option<SessionId>,
                #[serde(default)] from_session: Option<SessionId> }
            let p: P = parse(params)?;
            // No window: beside the calling session (the CLI's `muster note open` in a session).
            let (window, beside) = match (p.window_id, p.from_session) {
                (Some(w), _) => (w, p.beside),
                (None, Some(s)) => (crate::notes::window_of(hub, s)?, Some(s)),
                (None, None) => return Err(RpcError("note.open needs window_id (or from_session)".into())),
            };
            to_value(crate::notes::open(hub, p.id, window, beside)?)
        }
        "note.close" => {
            let p: IdParam<NoteId> = parse(params)?;
            to_value(crate::notes::close(hub, p.id)?)
        }
        "note.delete" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)]
            struct P { id: NoteId, #[serde(default)] from_session: Option<SessionId> }
            let p: P = parse(params)?;
            crate::notes::check_delete_from(hub, p.from_session)?;
            crate::notes::delete(hub, p.id)?;
            Ok(json!({}))
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
        "daemon.reload_handoff" => {
            human_only(who, method, &params)?;
            #[derive(Deserialize)]
            struct P {
                /// The `muster` binary running the `reload` command.
                exe: std::path::PathBuf,
            }
            let p: P = parse(params)?;
            let exe = crate::updates::reload_handoff(hub, &p.exe)?;
            Ok(json!({ "exe": exe.to_string_lossy() }))
        }
        "daemon.shutdown" => {
            human_only(who, method, &params)?;
            crate::updates::shutdown(hub)?;
            Ok(json!({}))
        }
        "daemon.restart" => {
            human_only(who, method, &params)?;
            let expect: crate::updates::Expect = parse(params)?;
            let exe = crate::updates::restart(hub, expect).await?;
            Ok(json!({ "exe": exe.to_string_lossy() }))
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
        "account_usage.get" => {
            require_local(who, method)?;
            to_value(hub.account_usage()?)
        }
        "account_profile.same_account" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P { id: i64, with: Option<i64> }
            let p: P = parse(params)?;
            to_value(hub.set_same_account(p.id, p.with)?)
        }
        "account_profile.login_changed" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P { account_profile: crate::account_profiles::ProfileChoice }
            let p: P = parse(params)?;
            hub.account_login_changed(p.account_profile)?;
            to_value(hub.account_usage()?)
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
            // A named agent's tab under the start lock, like every start of it (src/sleep.rs).
            to_value(crate::agents::restart_session(hub, p.id).await?)
        }
        "session.remove" => {
            let p: IdParam<SessionId> = parse(params)?;
            // Closing a named agent's tab never stops it, whatever the client knew about agents
            // (its list may not have loaded yet): it is hidden instead. `agent.remove` removes it.
            if hub.agents().by_session(p.id).is_some() {
                return Ok(json!({ "hidden": true, "session": hub.hide_session(p.id)? }));
            }
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
                    // An asleep agent wakes for it (src/sleep.rs).
                    let (id, _delivering) = crate::sleep::for_send(hub, p.id).await?;
                    to_value(inject::send(hub, id, p.text, mode, format!("session:{s}"), source).await?)
                }
                Caller::Human => {
                    let (id, _delivering) = crate::sleep::for_send(hub, p.id).await?;
                    to_value(inject::send(hub, id, p.text, mode, who.label(), crate::agents::Source::Human).await?)
                }
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
            // Admitted until the terminal host has them, so a sleep can't stop the run they are
            // typed into (src/sleep.rs).
            let admitted = match chain {
                Some(r) => {
                    let target = hub.session(p.id).ok_or_else(|| RpcError(format!("no such session: {}", p.id)))?;
                    crate::mcp::guard::check_claude_origin(target.kind, r.origin).map_err(RpcError)?;
                    // Keys can start a turn as well as a prompt can: an OpenRouter session's
                    // spend caps apply to unattended keys too.
                    if r.origin != crate::agents::Origin::Human && target.provider == crate::openrouter::Provider::OpenRouter {
                        hub.spend.ensure_fresh(hub).await;
                    }
                    crate::spend::raw_input_gate(hub, p.id, r.origin).map_err(RpcError)?;
                    // Keys can't wake an asleep agent: only a message does (its guard rails apply).
                    crate::sleep::admit(hub, p.id)?
                }
                // The human's keys (a paired phone, `muster keys` outside a session) take the
                // same path as a client's: no session lookup under the hub lock, no SQLite.
                None => {
                    hub.note_human_input(p.id);
                    // An asleep agent's tab: held until it has woken (src/sleep.rs).
                    match crate::sleep::keys(hub, p.id, p.data.as_bytes()) {
                        crate::sleep::Keys::Held => return Ok(json!({})),
                        crate::sleep::Keys::Pass(admitted) => admitted,
                    }
                }
            };
            let data = if p.keys && hub.application_cursor(p.id) {
                crate::pty::to_application_cursor(&p.data)
            } else {
                p.data
            };
            hub.write_input_admitted(p.id, data.as_bytes(), admitted)?;
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
        "device.list" => {
            let devices = hub.devices();
            let list: Vec<Value> = devices
                .list()
                .into_iter()
                .map(|d| {
                    let mut v = json!(d);
                    v["push"] = devices.push_subscription(d.id).map_or(Value::Null, |s| s.summary());
                    v
                })
                .collect();
            Ok(json!(list))
        }
        "push.key" | "push.status" | "push.subscribe" | "push.unsubscribe" | "push.test" => {
            push_method(hub, who, method, params).await
        }
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
            // Letting an agent take scheduled or triggered work, or moving one that does to or
            // from the subscription, only with a click in the owner's window: checked with the
            // record it changes, under one lock (`agents::widens_unattended`).
            let b = crate::agents::update(hub, p, via == Via::Outside)?;
            Ok(crate::agents::agent_view(hub, &b))
        }
        "agent.list" => {
            let out: Vec<Value> = hub
                .agents()
                .list()
                .iter()
                .map(|b| crate::agents::agent_view(hub, b))
                .collect();
            Ok(Value::Array(out))
        }
        "agent.templates" => to_value(crate::agents::templates()),
        "agent.models" => Ok(crate::agents::model_catalog()),
        "openrouter.models" => Ok(hub.openrouter_models().models(hub.openrouter_key().is_some()).await),
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
            if let Some(at) = bs.woke {
                crate::sleep::note_woke(hub, &bs.agent, &bs.session, at, "you chose Wake", true);
            }
            Ok(json!({
                "agent": crate::agents::agent_json(&bs.agent, Some(&bs.session)),
                // The running process keeps the provider it started with: a change shows here
                // until a restart (Start fresh, or Stop then Start) applies it.
                "provider_pending": !bs.started && bs.session.provider != bs.agent.provider,
                "session": bs.session,
                "started": bs.started,
                "woke": bs.woke.is_some(),
            }))
        }
        "agent.sleep" => {
            require_local(who, method)?;
            #[derive(Deserialize)]
            struct P {
                name: String,
            }
            let p: P = parse(params)?;
            let a = crate::sleep::sleep(hub, &p.name, crate::sleep::Why::Manual).await?;
            Ok(crate::agents::agent_view(hub, &a))
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
        "agent.memory.list" | "agent.memory.read" | "agent.memory.write" | "agent.memory.add"
        | "agent.memory.delete" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
                #[serde(default)]
                path: Option<String>,
                #[serde(default)]
                file_name: Option<String>,
                #[serde(default)]
                text: Option<String>,
            }
            let p: P = parse(params)?;
            let a = hub.agents().get(&p.name).ok_or_else(|| crate::agents::no_agent(hub, &p.name))?;
            let path = || p.path.as_deref().ok_or_else(|| RpcError("path is required".into()));
            let text = || p.text.as_deref().ok_or_else(|| RpcError("text is required".into()));
            match method {
                "agent.memory.list" => Ok(crate::agents::memory_list(&a)?),
                "agent.memory.read" => Ok(crate::agents::memory_read(&a, path()?)?),
                _ => {
                    require_local(who, method)?;
                    match method {
                        "agent.memory.write" => Ok(crate::agents::memory_write(&a, path()?, text()?)?),
                        "agent.memory.add" => {
                            let f = p.file_name.as_deref().ok_or_else(|| RpcError("file_name is required".into()))?;
                            Ok(crate::agents::memory_add(&a, f, text()?)?)
                        }
                        _ => Ok(crate::agents::memory_delete(&a, path()?)?),
                    }
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
        // `muster reply` inside an agent's session: its MCP `reply` (src/agent_reply.rs). Only the
        // agent a message was delivered to answers it; the human and shells never do.
        "agent.reply" => {
            #[derive(Deserialize)]
            struct P {
                msg_id: i64,
                text: String,
                #[serde(default)]
                from_session: Option<SessionId>,
            }
            let p: P = parse(params)?;
            match caller(hub, p.from_session)? {
                Caller::Agent(s) => {
                    let args = json!({"msg_id": p.msg_id, "text": p.text});
                    mcp_json(crate::mcp::call_as(hub, s, "reply", args).await)
                }
                Caller::Shell(_) | Caller::Human => Err(RpcError(
                    "only the agent a message was delivered to can reply to it: run `muster reply` in that agent's session".into(),
                )),
            }
        }

        // Schedules (docs/PROTOCOL.md "Schedules"). Only the human makes, approves or changes them:
        // a call on an agent session's behalf is refused, except what an agent may do through its
        // MCP schedule tools (propose, set a reminder, pause, delete its own), which `muster
        // schedule propose/remind/pause/rm` inside an agent's session run as that agent.
        m if m.starts_with("schedule.") || m.starts_with("trigger.") || m == "note.set_triggers" => {
            require_local(who, method)?;
            let from = params.get("from_session").and_then(|v| v.as_u64()).map(|v| v as SessionId);
            let reads = matches!(method, "schedule.list" | "schedule.runs" | "schedule.preview" | "schedule.settings.get");
            if let Some(r) = agent_schedule_call(hub, method, &params, from).await {
                return r;
            }
            // An agent's session, or a shell tab an agent has typed into (its provenance is an
            // agent's chain): only a shell the human drives may make or change schedules, so
            // unattended work can't set up more of itself.
            if !reads {
                let refused = match caller(hub, from)? {
                    Caller::Human => false,
                    Caller::Agent(_) => true,
                    Caller::Shell(s) => hub.agents().prov(s).is_some(),
                };
                if refused {
                    let what = match method {
                        "note.set_triggers" => "only the human allows triggers on a note",
                        "schedule.approve" => "only the human approves an agent's proposal (an agent can only propose)",
                        m if m.starts_with("trigger.") => "only the human fires triggers from Muster or sees their secrets",
                        _ => "only the human makes and changes schedules",
                    };
                    return Err(RpcError(format!(
                        "{method} can't be called from an agent's session, or from a shell an agent has typed into: {what}"
                    )));
                }
            }
            #[derive(Deserialize)]
            struct Id {
                id: i64,
                #[serde(default)]
                #[allow(dead_code)]
                from_session: Option<SessionId>,
            }
            match method {
                "schedule.list" => {
                    #[derive(Deserialize)]
                    struct P {
                        #[serde(default)]
                        agent: Option<String>,
                    }
                    let p: P = parse(params)?;
                    Ok(crate::schedules::list(hub, p.agent.as_deref())?)
                }
                // Outside the owner's window a new schedule, or a change that widens one, is made
                // but held: it runs only once the owner approves it with a click.
                "schedule.create" => Ok(match via {
                    Via::Window => crate::schedules::create(hub, parse(params)?)?,
                    Via::Outside => crate::schedules::create_held(hub, parse(params)?, crate::schedules::HELD_WHY)?,
                }),
                "schedule.update" => Ok(match via {
                    Via::Window => crate::schedules::update(hub, parse(params)?)?,
                    Via::Outside => crate::schedules::update_held(hub, parse(params)?, crate::schedules::HELD_WHY)?,
                }),
                "schedule.remove" => Ok(crate::schedules::remove(hub, parse::<Id>(params)?.id)?),
                "schedule.pause" | "schedule.skip" => {
                    #[derive(Deserialize)]
                    struct P {
                        id: i64,
                        #[serde(default = "yes")]
                        on: bool,
                    }
                    fn yes() -> bool {
                        true
                    }
                    let p: P = parse(params)?;
                    let outside = outside(via);
                    Ok(if method == "schedule.pause" {
                        crate::schedules::set_paused(hub, p.id, p.on, outside)?
                    } else {
                        crate::schedules::set_skip(hub, p.id, p.on, outside)?
                    })
                }
                "schedule.renew" => Ok(crate::schedules::renew(hub, parse::<Id>(params)?.id, outside(via))?),
                // Never here (the CLI, `/api/rpc`, a script): only a click in Muster's window
                // approves, through `approve_from_window`.
                "schedule.approve" => Err(RpcError(APPROVE_BY_CLICK.into())),
                "schedule.run" => Ok(crate::schedules::run_now(hub, parse::<Id>(params)?.id)?),
                "schedule.runs" => {
                    #[derive(Deserialize)]
                    struct P {
                        id: i64,
                        #[serde(default)]
                        before: Option<i64>,
                        #[serde(default)]
                        limit: Option<usize>,
                    }
                    let p: P = parse(params)?;
                    Ok(crate::schedules::runs(hub, p.id, p.before, p.limit)?)
                }
                "schedule.preview" => {
                    #[derive(Deserialize)]
                    struct P {
                        when: crate::schedules::WhenInput,
                        #[serde(default)]
                        tz: Option<String>,
                        #[serde(default)]
                        quiet_hours: Option<bool>,
                        #[serde(default)]
                        agent: Option<String>,
                        /// The schedule being edited (a URL trigger keeps its own name).
                        #[serde(default)]
                        id: Option<i64>,
                        /// The "Then…" steps' agents (their rules show in the preview too).
                        #[serde(default)]
                        then_agents: Vec<String>,
                    }
                    let p: P = parse(params)?;
                    Ok(crate::schedules::preview(hub, &p.when, p.tz.as_deref(), p.quiet_hours, p.agent.as_deref(), p.id, &p.then_agents)?)
                }
                "trigger.fire" => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct P {
                        name: String,
                        #[serde(default)]
                        payload: Option<String>,
                        #[serde(default)]
                        #[allow(dead_code)]
                        from_session: Option<SessionId>,
                    }
                    let p: P = parse(params)?;
                    Ok(crate::schedules::fire(hub, &p.name, p.payload.as_deref())?)
                }
                "trigger.secret" => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct P {
                        #[serde(default)]
                        id: Option<i64>,
                        #[serde(default)]
                        name: Option<String>,
                        #[serde(default)]
                        rotate: bool,
                        #[serde(default)]
                        #[allow(dead_code)]
                        from_session: Option<SessionId>,
                    }
                    let p: P = parse(params)?;
                    let id = match (p.id, p.name.as_deref()) {
                        (Some(id), _) => id,
                        (None, Some(n)) => crate::schedules::triggers::by_url_name(hub, n)
                            .map(|s| s.id)
                            .ok_or_else(|| RpcError(format!("no trigger named {n}")))?,
                        (None, None) => return Err(RpcError("trigger.secret needs id or name".into())),
                    };
                    Ok(crate::schedules::triggers::secret_json(hub, id, p.rotate)?)
                }
                "note.set_triggers" => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct P {
                        id: NoteId,
                        on: bool,
                        #[serde(default)]
                        #[allow(dead_code)]
                        from_session: Option<SessionId>,
                    }
                    let p: P = parse(params)?;
                    if p.on && via == Via::Outside {
                        return Err(RpcError(
                            "allowing triggers on a note lets its changes start runs, so it takes a click in Muster's window \
                             (the note's status line, or the trigger's ⋯ menu in the Agents panel). From here you can only switch them off"
                                .into(),
                        ));
                    }
                    to_value(crate::notes::set_triggers(hub, p.id, p.on)?)
                }
                "schedule.settings.get" => {
                    // Today's OpenRouter spend is shown with them: read the key again when the
                    // last reading is over a minute old (`schedule_settings` follows).
                    if hub.spend.key_present() {
                        let h = hub.clone();
                        tokio::spawn(async move { h.spend.ensure_fresh(&h).await });
                    }
                    Ok(crate::schedules::settings_json(hub, &hub.schedules.settings()))
                }
                "schedule.settings.set" => Ok(crate::schedules::set_settings(hub, parse(params)?, via == Via::Window)?),
                _ => Err(RpcError(format!("unknown method: {method}"))),
            }
        }

        "session.attach" | "session.detach" | "window.claim" => {
            Err(RpcError(format!("{method} is only available over the WebSocket")))
        }
        _ => Err(RpcError(format!("unknown method: {method}"))),
    }
}

/// `muster schedule propose/remind` (`schedule.propose`, `schedule.remind`: an agent's only), and
/// `pause` and `rm` inside an agent's session: run as that agent's MCP schedule tool, so the
/// CLI and MCP share one set of rules. None: an ordinary schedule call (the human's, or refused).
async fn agent_schedule_call(hub: &Arc<Hub>, method: &str, params: &Value, from: Option<SessionId>) -> Option<RpcResult> {
    let agent_only = matches!(method, "schedule.propose" | "schedule.remind");
    let session = match caller(hub, from) {
        Ok(Caller::Agent(s)) => s,
        Err(e) => return agent_only.then_some(Err(e)),
        Ok(_) if agent_only => {
            return Some(Err(RpcError(format!(
                "{method} is for agents (it runs as the agent whose session calls it): make a schedule with `muster schedule add`"
            ))));
        }
        Ok(_) => return None,
    };
    let mut args = params.clone();
    let o = args.as_object_mut()?;
    o.remove("from_session");
    let tool = match method {
        "schedule.propose" => "propose_schedule",
        "schedule.remind" => "set_reminder",
        // Pausing is the agent's (its own or one that runs on it); resuming stays the human's.
        "schedule.pause" if o.get("on").is_none_or(|v| v.as_bool() == Some(true)) => {
            let id = o.get("id").cloned().unwrap_or(Value::Null);
            args = json!({"schedule_id": id});
            "pause_schedule"
        }
        "schedule.remove" => {
            let id = o.get("id").cloned().unwrap_or(Value::Null);
            args = json!({"schedule_id": id});
            "delete_schedule"
        }
        _ => return None,
    };
    Some(mcp_json(crate::mcp::call_as(hub, session, tool, args).await))
}

/// Why `schedule.approve` was refused from anything but a click in Muster's window.
pub const APPROVE_BY_CLICK: &str = "a schedule waiting for approval (an agent's proposal, or one made or changed outside Muster's \
     window) is approved only with a click on Approve in Muster's window (the Agents panel): not from the command line, a script \
     or an agent, so an agent can't approve its own";

/// How a WebSocket connection came in (`server::ws_upgrade`).
#[derive(Debug, Clone, Default)]
pub struct WsLink {
    /// It sent one of the UI's own origins (Muster's window or the UI in a browser; a script's
    /// WebSocket usually sends none).
    pub ui_origin: bool,
    /// The TCP peer and the address it connected to, to find the processes behind it.
    pub peer: Option<std::net::SocketAddr>,
    pub server: Option<std::net::SocketAddr>,
}

/// `Some(why)` for a call from outside the owner's window (`schedules::hold_change`).
fn outside(via: Via) -> Option<&'static str> {
    (via == Via::Outside).then_some(crate::schedules::HELD_WHY)
}

/// The methods that make or widen unattended work, which the WebSocket checks the window for
/// before they run (`from_window`); `schedule.approve` has its own (`approve_from_window`).
fn widening(method: &str) -> bool {
    matches!(
        method,
        "schedule.create" | "schedule.update" | "schedule.pause" | "schedule.skip" | "schedule.renew"
            | "schedule.settings.set" | "note.set_triggers" | "agent.update"
    )
}

/// Is this call a click in the owner's window? Only a WebSocket that came in with the client
/// token and an `Origin` of Muster's own UI (`server::ws_upgrade`), that has claimed a window
/// (`window.claim`, what the desktop window and the UI in a browser do when they load), names no
/// session (`from_session`: the CLI, or an agent), and whose program is known and runs outside
/// every session's process tree (`server::connection_holder`: a program an agent started, one
/// that hides what it has open, or one Muster can't look up, as on a system without `/proc`, is
/// not the window). Err: why not, in words. With the owner's user an agent can still get round
/// this (docs/SCHEDULING_PLAN.md "Approval and its limits").
pub async fn from_window(hub: &Arc<Hub>, who: &Principal, conn: u64, link: &WsLink, from: Option<SessionId>) -> Result<(), String> {
    if !matches!(who, Principal::Local) {
        return Err("it comes from a paired device".into());
    }
    if from.is_some() {
        return Err("it was made on a session's behalf (the command line or an agent)".into());
    }
    if !link.ui_origin || !hub.holds_window(conn) {
        return Err("it doesn't come from a window of Muster's (a script, or a page that holds no window)".into());
    }
    let (Some(peer), Some(server)) = (link.peer, link.server) else {
        return Err("Muster can't tell which program it comes from".into());
    };
    let roots: std::collections::HashSet<u32> = hub.sessions().iter().filter_map(|s| s.pid).collect();
    let holder = tokio::task::spawn_blocking(move || crate::server::connection_holder(peer, server, &roots))
        .await
        .unwrap_or_else(|e| crate::server::Holder::Unknown(format!("the lookup failed: {e}")));
    match holder {
        crate::server::Holder::Outside => Ok(()),
        crate::server::Holder::Inside(pid) => {
            tracing::warn!("a window call refused: the connection's process {pid} runs inside a Muster session");
            Err("this connection comes from a program started in one of Muster's sessions".into())
        }
        crate::server::Holder::Unknown(why) => {
            tracing::warn!("a window call refused: its program can't be told ({why})");
            Err(format!("Muster can't tell which program this connection comes from ({why})"))
        }
    }
}

/// `schedule.approve`, the owner approving an agent's proposal or a schedule made or changed
/// outside the window: only a click in the owner's window (`from_window`), never the CLI or
/// `/api/rpc` (`dispatch` refuses it). `seen`: the schedule's `updated_at` the owner looked at;
/// approving refuses a schedule that changed since.
pub async fn approve_from_window(hub: &Arc<Hub>, who: &Principal, conn: u64, link: &WsLink, params: Value) -> RpcResult {
    require_local(who, "schedule.approve")?;
    let from = params.get("from_session").and_then(|v| v.as_u64()).map(|v| v as SessionId);
    from_window(hub, who, conn, link, from).await.map_err(|why| RpcError(format!("{APPROVE_BY_CLICK} ({why})")))?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct P {
        id: i64,
        #[serde(default)]
        seen: Option<i64>,
        #[serde(default)]
        #[allow(dead_code)]
        from_session: Option<SessionId>,
    }
    let p: P = parse(params)?;
    Ok(crate::schedules::approve(hub, p.id, p.seen)?)
}

/// Whether a connection gets `ev`: `own_device` is the paired device it belongs to, None for the
/// desktop client token. Named agents, schedules, panels and notes aren't on the phone (no
/// agent.*, panel.* or note.* method is either; docs/ANDROID.md); a phone's alert status
/// (`push_changed`) goes to that phone only, never to other phones or the desktop.
fn forwarded(own_device: Option<i64>, ev: &ServerEvent) -> bool {
    if let ServerEvent::PushChanged { device_id } = ev {
        return own_device == Some(*device_id);
    }
    own_device.is_none()
        || !matches!(
            ev,
            ServerEvent::AgentUpserted(_) | ServerEvent::AgentRemoved { .. } | ServerEvent::AgentMessage(_)
            | ServerEvent::ScheduleUpserted(_) | ServerEvent::ScheduleRemoved { .. }
            | ServerEvent::ScheduleRun(_) | ServerEvent::ScheduleSettings(_)
            | ServerEvent::PanelUpserted(_) | ServerEvent::PanelRemoved { .. }
            | ServerEvent::NoteUpserted(_) | ServerEvent::NoteRemoved { .. }
        )
}

/// `push.status` and what the other `push.*` methods answer: the daemon's public key and this
/// device's own subscription (its endpoint too, so the phone can tell whether its browser's
/// subscription is the one the daemon has), never the keys.
fn push_status(hub: &Hub, device: i64) -> RpcResult {
    let sub = hub.devices().push_subscription(device).map(|s| {
        let mut v = s.summary();
        v["endpoint"] = json!(s.endpoint);
        v
    });
    Ok(json!({ "public_key": crate::push::vapid()?.public_key, "subscription": sub }))
}

/// Phone alerts (docs/PROTOCOL.md "Phone alerts"): a paired device manages its own Web Push
/// subscription, one per device. Never for the client token: there is no phone to alert.
async fn push_method(hub: &Arc<Hub>, who: &Principal, method: &str, params: Value) -> RpcResult {
    let Principal::Device { id } = *who else {
        return Err(RpcError(format!("{method} is for a paired phone's own alerts")));
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    match method {
        "push.subscribe" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                endpoint: String,
                p256dh: String,
                auth: String,
            }
            let p: P = parse(params)?;
            let refused = |e: String| RpcError(format!("{}{e}", crate::push::REFUSED));
            let endpoint = crate::push::check_endpoint(&p.endpoint, &crate::push::allowed_hosts()).map_err(refused)?;
            crate::push::check_keys(&p.p256dh, &p.auth).map_err(refused)?;
            hub.devices().set_push(
                hub.store(),
                id,
                crate::push::Subscription {
                    endpoint: p.endpoint,
                    p256dh: p.p256dh.trim().to_string(),
                    auth: p.auth.trim().to_string(),
                    created_at: now_ms(),
                    last_ok_at: None,
                    last_error: None,
                    last_error_at: None,
                },
            )?;
            tracing::info!("device {id}: phone alerts on (via {})", endpoint.host);
            crate::push::changed(hub, id);
        }
        "push.unsubscribe" => {
            parse::<Empty>(params)?;
            if hub.devices().remove_push(hub.store(), id)? {
                tracing::info!("device {id}: phone alerts off");
                crate::push::changed(hub, id);
            }
        }
        "push.test" => {
            parse::<Empty>(params)?;
            let outcome = crate::push::test(hub, id).await.map_err(RpcError)?;
            let mut v = push_status(hub, id)?;
            v["delivered"] = json!(outcome == crate::push::Outcome::Delivered);
            v["error"] = match outcome {
                crate::push::Outcome::Delivered => Value::Null,
                crate::push::Outcome::Gone(code) => {
                    json!(format!("the push service says this phone's subscription is gone ({code}); turn alerts on again"))
                }
                crate::push::Outcome::Failed(e) => json!(e),
            };
            return Ok(v);
        }
        _ => {
            parse::<Empty>(params)?;
        }
    }
    push_status(hub, id)
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
/// `from_session` when it is an agent's (Claude or Codex) session: its note writes go through
/// its MCP tools. A shell tab, or no session, writes as the human.
fn agent_session(hub: &Hub, from: Option<SessionId>) -> Result<Option<SessionId>, RpcError> {
    Ok(if crate::notes::author(hub, from)?.kind == "agent" { from } else { None })
}

/// Run note tool `tool` as agent session `caller`, then return the whole note it touched.
async fn agent_note(hub: &Arc<Hub>, caller: SessionId, tool: &str, mut args: Value) -> Result<Note, RpcError> {
    if let Some(o) = args.as_object_mut() { o.retain(|_, v| !v.is_null()); }
    let out = mcp_json(crate::mcp::call_as(hub, caller, tool, args).await)?;
    let id = out["note_id"].as_i64().ok_or_else(|| RpcError(format!("{tool} returned no note")))?;
    Ok(crate::notes::get(hub, id)?)
}

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
pub async fn ws_session(socket: WebSocket, hub: Arc<Hub>, who: Principal, link: WsLink) {
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUTBOX_CAP);
    let is_device = matches!(who, Principal::Device { .. });
    // Ends the reader loop: the writer stopped, or the watchdog closed the connection.
    let closed = Arc::new(tokio::sync::Notify::new());

    let writer = {
        let closed = closed.clone();
        // Local windows keep today's unbounded sends; a device's are bounded (DEVICE_WRITE_STALL).
        let stall = if is_device { DEVICE_WRITE_STALL } else { Duration::MAX };
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                match tokio::time::timeout(stall, sink.send(m)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => break,
                    Err(_) => {
                        tracing::debug!("device connection stopped taking frames; closing");
                        break;
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), sink.close()).await;
            closed.notify_one();
        })
    };

    // Server events -> client. If this client falls too far behind the event bus, close the
    // connection: the client reconnects and re-syncs with `hello`.
    // Server events and method responses both go through this task, so a response is sent
    // only after every event emitted before it (e.g. `workspace_upserted` before the
    // `workspace.add` result). If this client falls too far behind the event bus, close the
    // connection: the client reconnects and re-syncs with `hello`.
    let (resp_tx, mut resp_rx) = mpsc::channel::<Message>(OUTBOX_CAP);
    let own_device = match who {
        Principal::Device { id } => Some(id),
        Principal::Local => None,
    };
    let events = {
        let out = out_tx.clone();
        let mut rx = hub.subscribe();
        tokio::spawn(async move {
            let forward = |ev: ServerEvent| {
                if !forwarded(own_device, &ev) {
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
    // A device's socket closes the moment it is revoked or expires, and after DEVICE_SILENCE
    // without a frame from it, even while idle, while its reader has stopped reading, or while
    // a request is stuck on a full outbox (the watchdog kills the writer, which fails every
    // pending send). Subscribed before the first check, so a revocation between the cookie
    // check and this point is not missed.
    let started = std::time::Instant::now();
    // Milliseconds after `started` of the last frame received.
    let heard = Arc::new(AtomicU64::new(0));
    let watchdog = device_id.map(|id| {
        let (hub, closed, heard) = (hub.clone(), closed.clone(), heard.clone());
        let writer = writer.abort_handle();
        let events = events.abort_handle();
        let mut revoked = hub.devices().subscribe_revoked();
        let pinger = out_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            let mut ticks = 0u64;
            loop {
                if hub.devices().scope(id).is_none() {
                    break;
                }
                let silent = started.elapsed().saturating_sub(Duration::from_millis(heard.load(Ordering::Relaxed)));
                if silent > DEVICE_SILENCE {
                    tracing::debug!("device connection silent for {silent:?}; closing");
                    break;
                }
                tokio::select! {
                    r = revoked.recv() => if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                        break;
                    },
                    // Every 30 s. The browser's pong (or any frame) keeps the socket within
                    // DEVICE_SILENCE. Skipped while the outbox is full: that client isn't
                    // reading anyway, and the write stall closes it.
                    _ = tick.tick() => {
                        if ticks % 3 == 0 { let _ = pinger.try_send(Message::Ping(Bytes::new())); }
                        ticks += 1;
                    }
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
        heard.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
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
                handle_request(&hub, &who, conn, &link, req, &out_tx, &resp_tx, &attachments).await;
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
                    // An asleep agent's tab: held until it has woken (src/sleep.rs). Admitted
                    // until the terminal host has them, so a sleep can't stop the run meanwhile.
                    let admitted = match crate::sleep::keys(&hub, id, &data[5..]) {
                        crate::sleep::Keys::Held => continue,
                        crate::sleep::Keys::Pass(admitted) => admitted,
                    };
                    if let Err(e) = hub.send_keys_admitted(id, &data[5..], admitted) {
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

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    hub: &Arc<Hub>,
    who: &Principal,
    conn: u64,
    link: &WsLink,
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
                /// docs/PROTOCOL.md). Default: the whole buffer (`full`), 64 KiB (`screen`).
                #[serde(default)]
                replay_bytes: Option<usize>,
                /// `full` (default), `screen` or `{since}`; given, the client also gets
                /// `attach_info` before every replay frame.
                #[serde(default)]
                replay: Option<ReplayParam>,
            }
            let (id, mode, since, limit) = match parse::<P>(req.params) {
                Ok(p) => {
                    let mode = ReplayMode {
                        info: p.replay.is_some(),
                        screen: !matches!(p.replay, None | Some(ReplayParam::Kind(ReplayKind::Full))),
                    };
                    let limit = match mode.screen {
                        true => Some(p.replay_bytes.unwrap_or(SCREEN_REPLAY_BYTES)),
                        false => p.replay_bytes,
                    };
                    let since = match p.replay {
                        Some(ReplayParam::Since { since }) => Some(since),
                        _ => None,
                    };
                    (p.id, mode, since, limit.map(|b| b.max(REPLAY_BYTES_MIN)))
                }
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
            // The tail starts before the replay is taken, so it covers the client's offset.
            let mut tail = if mode.screen { hub.join_tail(id) } else { None };
            // Read before the replay is taken: a resize bumps it only once the host has the new
            // size, so the replay is drawn at this revision's size or a later one.
            let mut size_rev = hub.size_rev(id);
            match hub.attach(id, limit).await {
                Ok(att) => {
                    let (kind, replay, rx, generation, offset) = match att {
                        Attach::Live { replay, rx, generation, offset } => {
                            tail.take_if(|t| t.generation() != generation);
                            // Only while the bytes still draw the same way: same daemon run,
                            // run and terminal size (no resize since, nor during this attach),
                            // and the run still going (an ended run's replay moves a
                            // full-screen view into history, which appending can't do).
                            let delta = match (&since, &tail) {
                                (Some(s), Some(t))
                                    if s.epoch == hub.replay_epoch() && s.generation == generation
                                        && s.size_rev.is_some() && s.size_rev == size_rev =>
                                {
                                    t.delta(s.offset, offset).await
                                }
                                _ => None,
                            }
                            .filter(|_| hub.size_rev(id) == size_rev && hub.pty_alive(id));
                            if hub.size_rev(id) != size_rev {
                                // Resized while the replay was taken: unknown which size it has.
                                size_rev = None;
                            }
                            // Whichever is smaller. A full-screen program's screen is often a
                            // few KiB while its repaints add up fast; a screen under its cap
                            // already holds all the history the daemon still has.
                            match delta {
                                Some(delta) if delta.len() <= replay.len() => {
                                    ("delta", delta, Some(rx), Some(generation), offset)
                                }
                                _ => (mode.kind(), replay, Some(rx), Some(generation), offset),
                            }
                        }
                        Attach::Replay(r) => (mode.kind(), r, None, None, 0),
                    };
                    // Result first, then the replay frame, then live frames (all through the
                    // same ordered outbox, all queued before this call returns).
                    let _ = out
                        .send(Message::Text(response(&req.id, Ok(json!({}))).to_string().into()))
                        .await;
                    if mode.info {
                        let _ = out.send(attach_info(hub, id, kind, offset, generation, size_rev)).await;
                    }
                    let _ = out.send(Message::Binary(frame(FRAME_REPLAY, id, &replay))).await;
                    let mut state = Forward { limit, mode, generation, offset, tail };
                    state.sent(offset);
                    let task = tokio::spawn(forward_output(hub.clone(), id, state, rx, bus, out.clone()));
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
        // Making or widening unattended work: as asked only from a click in the owner's
        // window (`from_window`); anywhere else it is held for one, or refused.
        m if widening(m) => {
            let (hub, who, resp, link) = (hub.clone(), who.clone(), resp.clone(), link.clone());
            tokio::spawn(async move {
                let from = req.params.get("from_session").and_then(|v| v.as_u64()).map(|v| v as SessionId);
                let via = match from_window(&hub, &who, conn, &link, from).await {
                    Ok(()) => Via::Window,
                    Err(_) => Via::Outside,
                };
                let r = dispatch_via(&hub, &who, &req.method, req.params, via).await;
                let _ = resp.send(Message::Text(response(&req.id, r).to_string().into())).await;
            });
        }
        // Approving: only a click in a UI window (see `approve_from_window`).
        "schedule.approve" => {
            let (hub, who, resp, link) = (hub.clone(), who.clone(), resp.clone(), link.clone());
            tokio::spawn(async move {
                let r = approve_from_window(&hub, &who, conn, &link, req.params).await;
                let _ = resp.send(Message::Text(response(&req.id, r).to_string().into())).await;
            });
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

/// `session.attach`'s `replay` param (docs/PROTOCOL.md "Remote devices").
#[derive(Deserialize)]
#[serde(untagged)]
enum ReplayParam {
    Kind(ReplayKind),
    Since { since: Since },
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ReplayKind {
    Full,
    Screen,
}

/// Where a client's terminal stands: the raw output offset it has reached in run
/// `generation` of this daemon run (`epoch`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Since {
    offset: u64,
    generation: u64,
    epoch: String,
    /// Absent from phones loaded before it existed: then unknown, so never a delta.
    #[serde(default)]
    size_rev: Option<u64>,
}

/// How an attachment's replays are sent.
#[derive(Clone, Copy)]
struct ReplayMode {
    /// The client named a `replay` kind: `attach_info` precedes every replay frame.
    info: bool,
    /// Replays are bounded screens (`screen` or `since`), not the whole buffer.
    screen: bool,
}

impl ReplayMode {
    fn kind(self) -> &'static str {
        if self.screen { "screen" } else { "full" }
    }
}

/// The connection-scoped message before a replay frame: what it is and the offset it ends
/// at (the client counts live bytes on from there). Never a hub broadcast, so it can't reach
/// another client or overtake the frame. `size_rev`: the terminal size revision the replay was
/// drawn at; None when unknown (the client then can't resume from it).
fn attach_info(hub: &Hub, id: SessionId, kind: &str, offset: u64, generation: Option<u64>, size_rev: Option<u64>) -> Message {
    let info = json!({"event": "attach_info", "data": {
        "id": id, "kind": kind, "offset": offset, "generation": generation, "epoch": hub.replay_epoch(),
        "size_rev": size_rev,
    }});
    Message::Text(info.to_string().into())
}

/// One attachment's forwarding state.
struct Forward {
    limit: Option<usize>,
    mode: ReplayMode,
    /// The run the live bytes belong to (None: an old run's replay only).
    generation: Option<u64>,
    /// The raw output offset after the last byte sent.
    offset: u64,
    /// Keeps the session's raw tail while attached, for a later `delta` (screen mode only).
    tail: Option<crate::replay_tail::TailUse>,
}

impl Forward {
    fn sent(&mut self, offset: u64) {
        self.offset = offset;
        if let Some(t) = &mut self.tail {
            t.reached = offset;
        }
    }
}

/// Streams live output for one attached session. Re-sends a replay if the client falls
/// behind, and follows the session across restarts (new process = new replay).
async fn forward_output(
    hub: Arc<Hub>,
    id: SessionId,
    mut state: Forward,
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
                Ok(crate::terminal::Output::Bytes(bytes)) => {
                    if out.send(Message::Binary(frame(FRAME_OUTPUT, id, &bytes))).await.is_err() {
                        return;
                    }
                    state.sent(state.offset + bytes.len() as u64);
                }
                Ok(crate::terminal::Output::Replay(bytes, offset)) => {
                    // The host took this replay at some earlier moment: its size revision is
                    // unknown, so the client is told it can't resume from it.
                    if state.mode.info
                        && out.send(attach_info(&hub, id, state.mode.kind(), offset, state.generation, None)).await.is_err()
                    {
                        return;
                    }
                    if out.send(Message::Binary(frame(FRAME_REPLAY, id, &bytes))).await.is_err() {
                        return;
                    }
                    state.sent(offset);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow client: start over from a fresh replay.
                    if !resend_replay(&hub, id, &mut state, &mut rx, &out).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => rx = None,
            },
            ev = bus.recv() => match ev {
                // Restarted: a new process with an empty replay buffer. Compared by process
                // generation, not pid (a pid can be reused).
                Ok(ServerEvent::SessionUpserted(info))
                    if info.id == id && hub.pty_generation(id).is_some_and(|g| Some(g) != state.generation) =>
                {
                    if !resend_replay(&hub, id, &mut state, &mut rx, &out).await {
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
    state: &mut Forward,
    rx: &mut Option<broadcast::Receiver<crate::terminal::Output>>,
    out: &Outbox,
) -> bool {
    // A restarted run gets a tail of its own, joined before its replay is taken.
    if state.mode.screen && state.tail.as_ref().map(|t| t.generation()) != hub.pty_generation(id) {
        state.tail = hub.join_tail(id);
    }
    let size_rev = hub.size_rev(id);
    let (replay, new_rx, new_gen, offset) = match hub.attach(id, state.limit).await {
        Ok(Attach::Live { replay, rx, generation, offset }) => (replay, Some(rx), Some(generation), offset),
        Ok(Attach::Replay(r)) => (r, None, None, 0),
        Err(_) => return false,
    };
    state.tail.take_if(|t| Some(t.generation()) != new_gen);
    state.generation = new_gen;
    *rx = new_rx;
    let size_rev = size_rev.filter(|r| Some(*r) == hub.size_rev(id));
    if state.mode.info && out.send(attach_info(hub, id, state.mode.kind(), offset, new_gen, size_rev)).await.is_err() {
        return false;
    }
    let sent = out.send(Message::Binary(frame(FRAME_REPLAY, id, &replay))).await.is_ok();
    state.sent(offset);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{NewAgent, Origin, Prov};
    use crate::mcp::McpState;
    use crate::mcp::backend::HubBackend;
    use crate::store::{Store, StoredSession};

    #[test]
    fn a_phones_alert_status_goes_to_that_phone_only() {
        let ev = ServerEvent::PushChanged { device_id: 4 };
        assert!(forwarded(Some(4), &ev));
        assert!(!forwarded(Some(5), &ev), "another phone");
        assert!(!forwarded(None, &ev), "the desktop");
        let note = ServerEvent::NoteRemoved { id: 1 };
        assert!(forwarded(None, &note) && !forwarded(Some(4), &note));
        assert!(forwarded(Some(4), &ServerEvent::SessionRemoved { id: 1 }));
    }

    /// A hub with workspace 1, channel 1 and exited sessions 1 (shell), 2 (claude), 3 (codex),
    /// with the MCP server state attached. Nothing here starts a process: every call below is
    /// refused before delivery.
    fn fixture() -> (Arc<Hub>, Arc<McpState<HubBackend>>) {
        fixture_keyed(None)
    }

    fn fixture_keyed(openrouter_key: Option<crate::openrouter::ApiKey>) -> (Arc<Hub>, Arc<McpState<HubBackend>>) {
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
            openrouter_key,
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
                provider: None,
                name: name.into(),
                kind: None,
                model: None,
                effort: None,
                persona: None,
                accepts: None,
                permissions: None,
                autostart: false,
                sleep_after_min: None,
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
    async fn cli_note_writes_from_an_agent_share_its_mcp_rate_limit() {
        let (hub, st) = fixture();
        let n = call(&hub, "note.create", json!({"title": "Shared", "body": "a", "from_session": 3})).await.unwrap();
        assert_eq!(n["updated_by"]["kind"], "agent");
        let id = n["id"].as_i64().unwrap();
        let n = call(&hub, "note.append", json!({"id": id, "text": "b", "from_session": 3})).await.unwrap();
        assert_eq!((n["body"].as_str(), n["rev"].as_i64()), (Some("a\nb"), Some(2)));
        for _ in 0..30 {
            st.guard.lock().posts.check_and_record(3, now_ms(), 30, 60_000).ok();
        }
        for (method, params) in [
            ("note.append", json!({"id": id, "text": "c", "from_session": 3})),
            ("note.write", json!({"id": id, "body": "c", "base_rev": 2, "from_session": 3})),
            ("note.create", json!({"body": "c", "from_session": 3})),
        ] {
            let e = call(&hub, method, params).await.unwrap_err();
            assert!(e.contains("rate limit"), "{method}: {e}");
        }
        // The human (no session) isn't limited.
        assert_eq!(call(&hub, "note.append", json!({"id": id, "text": "c"})).await.unwrap()["rev"], 3);
        let tl = hub.timeline(1, None, 100);
        assert!(tl.iter().any(|e| e.kind == EventKind::McpCall && e.data["tool"] == "note_append"), "audited like MCP");
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
        // Edit agent in the owner's window (taking scheduled work needs that click).
        let r = dispatch_via(
            &hub,
            &Principal::Local,
            "agent.update",
            json!({"name": "RPC-EVE", "persona": "Writes docs.", "autostart": true, "model": "opus",
                   "accepts": ["human", "schedule"],
                   "permissions": {"claude_mode": "plan", "codex_sandbox": "read_only"}}),
            Via::Window,
        )
        .await
        .map_err(|e| e.0)
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
        let m = call(&hub, "agent.models", json!({})).await.unwrap();
        for kind in ["claude", "codex"] {
            assert!(!m[kind]["models"].as_array().unwrap().is_empty() && !m[kind]["efforts"].as_array().unwrap().is_empty());
        }
        assert!(m["claude"]["models"].as_array().unwrap().iter().any(|x| x["id"] == "opus"));
        call(&hub, "agent.remove", json!({"name": "rpc-eve"})).await.unwrap();
        assert_eq!(drain(&mut rx, "agent_removed")[0]["name"], "rpc-eve");
    }

    #[tokio::test]
    async fn openrouter_needs_the_key_and_no_reply_carries_it() {
        const KEY: &str = "sk-or-rpc-fixture-secret";
        let (hub, _st) = fixture();
        assert_eq!(call(&hub, "hello", json!({})).await.unwrap()["openrouter"]["key_present"], false);
        let e = call(&hub, "agent.create", json!({"name": "or-none", "provider": "openrouter"})).await.unwrap_err();
        assert!(e.contains("OPENROUTER_API_KEY"), "{e}");
        agent(&hub, "or-sub");
        let e = call(&hub, "agent.update", json!({"name": "or-sub", "provider": "openrouter"})).await.unwrap_err();
        assert!(e.contains("OPENROUTER_API_KEY"), "{e}");
        assert_eq!(call(&hub, "agent.list", json!({})).await.unwrap()[0]["provider"], "subscription");

        let (hub, _st) = fixture_keyed(crate::openrouter::ApiKey::new(KEY));
        hub.openrouter_models().seed(crate::model::now_ms(), crate::openrouter::parse_models(
            include_bytes!("fixtures/openrouter_models.json")).unwrap());
        let a = call(&hub, "agent.create", json!({"name": "or-claude", "provider": "openrouter",
            "model": "anthropic/claude-sonnet-5.5"})).await.unwrap();
        assert_eq!((a["provider"].as_str(), a["model"].as_str()), (Some("openrouter"), Some("anthropic/claude-sonnet-5.5")));
        let e = call(&hub, "agent.create", json!({"name": "or-codex", "kind": "codex", "provider": "openrouter"})).await.unwrap_err();
        assert!(e.contains("needs a model"), "{e}");
        call(&hub, "agent.create", json!({"name": "or-codex", "kind": "codex", "provider": "openrouter",
            "model": "openai/gpt-6.1-sol"})).await.unwrap();
        let e = call(&hub, "agent.update", json!({"name": "or-codex", "model": ""})).await.unwrap_err();
        assert!(e.contains("needs a model"), "{e}");
        let r = call(&hub, "agent.update", json!({"name": "or-claude", "provider": "subscription", "model": "opus"})).await.unwrap();
        assert_eq!(r["provider"], "subscription");
        let models = call(&hub, "openrouter.models", json!({})).await.unwrap();
        assert_eq!(models["key_present"], true);
        assert!(models["models"].as_array().unwrap().iter().any(|m| m["id"] == "openai/gpt-6.1-sol"));
        let hello = call(&hub, "hello", json!({})).await.unwrap();
        assert_eq!(hello["openrouter"]["key_present"], true);
        for reply in [hello, models, call(&hub, "agent.list", json!({})).await.unwrap(),
            call(&hub, "agent.models", json!({})).await.unwrap()] {
            assert!(!reply.to_string().contains(KEY));
        }
        // The persona files and the store never hold it either.
        for name in ["or-claude", "or-codex"] {
            let a = hub.agents().get(name).unwrap();
            for f in ["CLAUDE.md", "AGENTS.md"] {
                if let Ok(text) = std::fs::read_to_string(std::path::Path::new(&a.home_dir).join(f)) {
                    assert!(!text.contains(KEY));
                }
            }
        }
        assert!(!format!("{:?}", hub.store().load_agents().unwrap()).contains(KEY));
        // A device never learns about OpenRouter's models.
        assert!(crate::devices::method_scope("openrouter.models").is_none());
    }

    #[tokio::test]
    async fn cli_reply_is_the_agents_own_mcp_reply() {
        let (hub, st) = fixture();
        agent(&hub, "rpc-rex");
        let rex = hub.agents().get("rpc-rex").unwrap();
        hub.agents().link_session(hub.store(), rex.id, Some(3)).unwrap();
        let route = crate::agents::Route { origin: Origin::Human, hop: 0, reply_to: None };
        let m = hub.store().insert_agent_message(&crate::agents::message_record(&rex, "human", None, route, "how long?")).unwrap();
        hub.store().set_agent_message_delivered(m, now_ms() - 60_000).unwrap();
        // The human and shells never reply; a session that isn't the agent can't either.
        for from in [Value::Null, json!(1), json!(2)] {
            let e = call(&hub, "agent.reply", json!({"msg_id": m, "text": "x", "from_session": from})).await.unwrap_err();
            assert!(e.contains("only the agent") || e.contains("isn't one"), "{e}");
        }
        // `!` would run as a command where it is typed.
        let e = call(&hub, "agent.reply", json!({"msg_id": m, "text": "!rm -rf x", "from_session": 3})).await.unwrap_err();
        assert!(e.contains("shell command"), "{e}");
        let r = call(&hub, "agent.reply", json!({"msg_id": m, "text": "two hours", "from_session": 3})).await.unwrap();
        assert_eq!((r["status"].as_str(), r["delivered"].as_bool()), (Some("shown"), Some(true)), "{r}");
        let logged = crate::agents::messages(&hub, Some("rpc-rex"), None, None).unwrap();
        assert_eq!(logged["messages"][0]["answer"]["text"], "two hours");
        // Once per message; refusals don't use up the send rate limit, deliveries do.
        let e = call(&hub, "agent.reply", json!({"msg_id": m, "text": "again", "from_session": 3})).await.unwrap_err();
        assert!(e.contains("already replied"), "{e}");
        assert!(st.guard.lock().sends.check_and_record(3, now_ms(), 1, 60_000).is_err(), "the reply used a slot");
        assert!(st.guard.lock().sends.check_and_record(3, now_ms(), 2, 60_000).is_ok(), "and only it did");
        // Audited like any MCP call.
        let tl = hub.timeline(1, None, 100);
        assert!(tl.iter().any(|e| e.kind == EventKind::McpCall && e.data["tool"] == "reply" && e.data["args"]["via"] == "cli"));
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
        // Adding: never replaces, suffixes a taken name, refuses paths; deleting removes the file.
        let r = call(&hub, "agent.memory.add", json!({"name": "rpc-max", "file_name": "guide.md", "text": "a"}))
            .await
            .unwrap();
        assert_eq!((r["path"].clone(), r["renamed"].clone()), (json!("guide.md"), json!(false)));
        let r = call(&hub, "agent.memory.add", json!({"name": "rpc-max", "file_name": "guide.md", "text": "b"}))
            .await
            .unwrap();
        assert_eq!((r["path"].clone(), r["renamed"].clone()), (json!("guide-2.md"), json!(true)));
        for bad in ["../CLAUDE.md", "people/x.md", ".env"] {
            let p = json!({"name": "rpc-max", "file_name": bad, "text": "x"});
            assert!(call(&hub, "agent.memory.add", p).await.is_err(), "{bad}");
        }
        assert!(call(&hub, "agent.memory.add", json!({"name": "rpc-max", "text": "x"})).await.is_err());
        call(&hub, "agent.memory.delete", json!({"name": "rpc-max", "path": "guide-2.md"})).await.unwrap();
        assert!(call(&hub, "agent.memory.read", json!({"name": "rpc-max", "path": "guide-2.md"})).await.is_err());
        assert!(call(&hub, "agent.memory.delete", json!({"name": "rpc-max", "path": "../CLAUDE.md"})).await.is_err());
        // Text that isn't valid UTF-8 (a lone surrogate) never reaches the handler.
        let raw = r#"{"name": "rpc-max", "file_name": "bad.md", "text": "\ud800"}"#;
        assert!(serde_json::from_str::<Value>(raw).is_err());
        // No agent method is ever a device's, and writes need the desktop client token.
        for m in [
            "agent.create", "agent.update", "agent.list", "agent.remove", "agent.start", "agent.ask", "agent.reply",
            "agent.templates", "agent.models", "agent.messages", "agent.memory.list", "agent.memory.read",
            "agent.memory.write", "agent.memory.add", "agent.memory.delete",
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
        for (m, p) in [
            ("agent.memory.add", json!({"name": "rpc-max", "file_name": "y.md", "text": "x"})),
            ("agent.memory.delete", json!({"name": "rpc-max", "path": "people/ada.md"})),
        ] {
            let e = dispatch(&hub, &phone, m, p).await.unwrap_err();
            assert!(e.0.contains("desktop client"), "{m}: {}", e.0);
        }
        assert!(call(&hub, "agent.memory.read", json!({"name": "rpc-max", "path": "people/ada.md"})).await.is_ok());
    }
    #[tokio::test]
    async fn hello_lists_named_agents_sessions() {
        let (hub, _st) = fixture();
        let empty = call(&hub, "hello", json!({})).await.unwrap();
        assert_eq!((empty["agent_sessions"].clone(), empty["agent_names"].clone()), (json!([]), json!({})));
        agent(&hub, "rpc-ada");
        let id = hub.agents().get("rpc-ada").unwrap().id;
        hub.agents().link_session(hub.store(), id, Some(2)).unwrap();
        // A device sees it too: it has no `agent.list`.
        let hello = dispatch(&hub, &Principal::Device { id: 1 }, "hello", json!({})).await.map_err(|e| e.0).unwrap();
        assert_eq!(hello["agent_sessions"], json!([2]));
        assert_eq!(hello["agent_names"], json!({ "2": "rpc-ada" }));
    }

    #[tokio::test]
    async fn restart_and_shutdown_are_human_only() {
        let (hub, _st) = fixture();
        let device = Principal::Device { id: 1 };
        for method in ["daemon.reload", "daemon.shutdown", "daemon.reload_handoff", "daemon.restart"] {
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
        // A restart refuses before anything stops, and tells windows nothing; a refused one
        // leaves the next try free (not "already restarting").
        let mut rx = hub.subscribe();
        for _ in 0..2 {
            let e = call(&hub, "daemon.restart", json!({"backend": null, "host": null})).await.unwrap_err();
            assert!(e.contains("where it was launched from") && e.contains("nothing was stopped"), "{e}");
        }
        assert!(call(&hub, "daemon.restart", json!({"host": 7})).await.unwrap_err().contains("invalid params"));
        assert!(drain(&mut rx, "daemon_stopping").is_empty(), "no daemon_stopping without a restart");
        let h = call(&hub, "hello", json!({})).await.unwrap();
        assert_eq!(h["updates"], json!({"ui_release": null, "backend": null, "host": null}));
        // The key handoff: refused (so the CLI reloads the usual way) without a key, without a
        // known launch path, and for a binary other than the one the backend started from.
        let e = call(&hub, "daemon.reload_handoff", json!({"exe": "/bin/true"})).await.unwrap_err();
        assert!(e.contains(crate::updates::HANDOFF_REFUSED) && e.contains("launched from"), "{e}");
        assert_eq!(h["pid"], std::process::id());
        let (keyed, _st) = fixture_keyed(crate::openrouter::ApiKey::new("sk-or-rpc-fixture-secret"));
        let e = call(&keyed, "daemon.reload_handoff", json!({"exe": "/bin/true"})).await.unwrap_err();
        assert!(e.contains(crate::updates::HANDOFF_REFUSED) && e.contains("launched from"), "{e}");
        assert!(!e.contains("sk-or-rpc"));
    }

    #[tokio::test]
    async fn window_prefs_persist_and_broadcast() {
        let (hub, _st) = fixture();
        let mut rx = hub.subscribe();
        let ttl = json!({"claude": 60, "claude_openrouter": 5, "codex": 30, "codex_openrouter": 10});
        let prefs = |d: bool, r: u64| json!({"decorations": d, "cache_warn": true, "cache_ttl": ttl, "revision": r});
        assert_eq!(call(&hub, "prefs.get", json!({})).await.unwrap(), prefs(true, 0));
        let set = prefs(false, 1);
        assert_eq!(call(&hub, "prefs.set", json!({"decorations": false})).await.unwrap(), set);
        assert_eq!(call(&hub, "hello", json!({})).await.unwrap()["prefs"], set);
        let ev = serde_json::to_value(rx.recv().await.unwrap()).unwrap();
        assert_eq!(ev, json!({"event": "prefs_changed", "data": set}));
        // Fields not given stay (each write still counts); unknown ones are refused.
        assert_eq!(call(&hub, "prefs.set", json!({})).await.unwrap(), prefs(false, 2));
        assert!(call(&hub, "prefs.set", json!({"theme": "dark"})).await.is_err());
        // The cold-cache warning: one lifetime at a time, in minutes (1 to a day).
        let p = call(&hub, "prefs.set", json!({"cache_warn": false, "cache_ttl": {"codex": 720}})).await.unwrap();
        assert_eq!((p["cache_warn"].clone(), p["cache_ttl"]["codex"].clone(), p["cache_ttl"]["claude"].clone()),
            (json!(false), json!(720), json!(60)));
        for bad in [json!({"codex": 0}), json!({"claude": 1441}), json!({"gemini": 5})] {
            assert!(call(&hub, "prefs.set", json!({"cache_ttl": bad})).await.is_err(), "{bad}");
        }
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
        assert_eq!(cleared["revision"], json!(2));
        assert!(cleared.get("shell").is_none());
        let hello = call(&hub, "hello", json!({})).await.unwrap();
        assert!(hello["login_shell"].as_str().is_some_and(|s| s.starts_with('/')));
        // A program the backend runs: never chosen from a paired device.
        let e = dispatch(&hub, &Principal::Device { id: 1 }, "prefs.set", json!({"shell": "sh"})).await.unwrap_err();
        assert!(e.0.contains("only available on this computer"), "{}", e.0);
    }

    #[tokio::test]
    async fn agents_propose_from_their_session_but_only_the_human_approves() {
        let (hub, _st) = fixture();
        agent(&hub, "rpc-sam"); // Claude: takes human work only
        crate::agents::create(&hub, NewAgent {
            account_profile: None, provider: None, name: "rpc-wk".into(), kind: Some(SessionKind::Codex), model: None,
            effort: None, persona: None, accepts: None, permissions: None, autostart: false, sleep_after_min: None,
        }).unwrap();
        let sam = hub.agents().get("rpc-sam").unwrap();
        let wk = hub.agents().get("rpc-wk").unwrap();
        hub.agents().link_session(hub.store(), sam.id, Some(2)).unwrap();
        hub.agents().link_session(hub.store(), wk.id, Some(3)).unwrap();
        // `muster schedule remind` in an agent's session is its set_reminder (its rules, audited).
        let e = call(&hub, "schedule.remind", json!({"prompt": "p", "in_minutes": 20, "from_session": 2})).await.unwrap_err();
        assert!(e.contains("doesn't take scheduled work"), "{e}");
        let r = call(&hub, "schedule.propose", json!({"prompt": "p", "cron": "0 9 * * 1-5", "reason": "weekday check", "from_session": 3})).await.unwrap();
        let id = r["schedule_id"].as_i64().unwrap();
        assert_eq!(r["status"], "proposed");
        let tl = hub.timeline(1, None, 100);
        assert!(tl.iter().any(|e| e.kind == EventKind::McpCall && e.data["tool"] == "propose_schedule"), "audited like MCP");
        // The human has no agent-only methods.
        let e = call(&hub, "schedule.propose", json!({"prompt": "p", "cron": "0 9 * * *", "reason": "r"})).await.unwrap_err();
        assert!(e.contains("is for agents"), "{e}");
        // Approving, resuming, making or changing a schedule: never from an agent's session, nor
        // from a shell an agent has typed into.
        hub.agents().set_prov(1, Some(SCHED)).unwrap();
        for from in [3, 2, 1] {
            for (m, p) in [
                ("schedule.approve", json!({"id": id})),
                ("schedule.pause", json!({"id": id, "on": false})),
                ("schedule.update", json!({"id": id, "prompt": "wider"})),
                ("schedule.create", json!({"agent": "rpc-wk", "prompt": "p", "when": {"kind": "cron", "cron": "0 9 * * *"}})),
                ("schedule.settings.set", json!({"watch_folders": ["/tmp"]})),
            ] {
                let mut p = p;
                p["from_session"] = json!(from);
                let e = call(&hub, m, p).await.unwrap_err();
                assert!(e.contains("can't be called from an agent's session"), "{m} from {from}: {e}");
            }
        }
        assert!(hub.schedules.get(id).unwrap().waiting_approval());
        // Pausing its own is the agent's (the MCP pause_schedule); the human approves, and only
        // with a click in a window: never through `dispatch` (the CLI, `/api/rpc`).
        let r = call(&hub, "schedule.pause", json!({"id": id, "from_session": 3})).await.unwrap();
        assert_eq!(r["status"], "proposed", "still waiting, now also paused");
        let e = call(&hub, "schedule.approve", json!({"id": id})).await.unwrap_err();
        assert!(e.contains("only with a click on Approve"), "{e}");
        let (window, _conn) = window_link();
        hub.claim_window(7, None).unwrap();
        let r = approve_from_window(&hub, &Principal::Local, 7, &window, json!({"id": id})).await.map_err(|e| e.0).unwrap();
        assert_eq!(r["status"], "paused");
        assert!(r["made_by"]["approved_at"].is_i64());
    }

    #[tokio::test]
    async fn approving_takes_a_click_in_a_window_never_a_script_or_an_agent() {
        let (hub, _st) = fixture();
        crate::agents::create(&hub, NewAgent {
            account_profile: None, provider: None, name: "rpc-wk".into(), kind: Some(SessionKind::Codex), model: None,
            effort: None, persona: None, accepts: None, permissions: None, autostart: false, sleep_after_min: None,
        }).unwrap();
        let wk = hub.agents().get("rpc-wk").unwrap();
        hub.agents().link_session(hub.store(), wk.id, Some(3)).unwrap();
        let propose = || async {
            call(&hub, "schedule.propose", json!({"prompt": "p", "cron": "0 9 * * 1-5", "reason": "r", "from_session": 3})).await.unwrap()["schedule_id"]
                .as_i64()
                .unwrap()
        };
        let id = propose().await;
        let (ok, _conn) = window_link();
        let refused = |r: RpcResult| {
            let e = r.unwrap_err().0;
            assert!(e.contains("only with a click on Approve"), "{e}");
        };
        // A connection that holds no window (a script's WebSocket), or one without the UI's
        // origin, or a device: refused.
        refused(approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id})).await);
        hub.claim_window(21, None).unwrap();
        let no_origin = WsLink { ui_origin: false, ..ok.clone() };
        refused(approve_from_window(&hub, &Principal::Local, 21, &no_origin, json!({"id": id})).await);
        assert!(approve_from_window(&hub, &Principal::Device { id: 1 }, 21, &ok, json!({"id": id})).await.is_err());
        // Saying it comes from an agent's session, or a shell an agent drives: refused.
        refused(approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id, "from_session": 3})).await);
        hub.agents().set_prov(1, Some(SCHED)).unwrap();
        refused(approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id, "from_session": 1})).await);
        assert!(hub.schedules.get(id).unwrap().waiting_approval());
        // A connection Muster can't attribute (no address to look up; on a system without
        // /proc, or one not listed there): refused, never taken as the window.
        let unknown = WsLink { ui_origin: true, peer: None, server: None };
        let e = approve_from_window(&hub, &Principal::Local, 21, &unknown, json!({"id": id})).await.unwrap_err().0;
        assert!(e.contains("can't tell which program"), "{e}");
        let unlisted = WsLink { server: Some("127.0.0.1:1".parse().unwrap()), ..ok.clone() };
        let e = approve_from_window(&hub, &Principal::Local, 21, &unlisted, json!({"id": id})).await.unwrap_err().0;
        assert!(e.contains("can't tell which program"), "{e}");
        // A window's page whose connection comes from a program inside a session's process tree
        // (here: this test process is that session's process): refused. The same connection
        // from outside every session: approved.
        #[cfg(target_os = "linux")]
        {
            hub.set_pid_for_test(2, Some(std::process::id()));
            let e = approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id})).await.unwrap_err().0;
            assert!(e.contains("started in one of Muster's sessions"), "{e}");
            assert!(hub.schedules.get(id).unwrap().waiting_approval());
            hub.set_pid_for_test(2, None);
        }
        // What the owner looked at must still be what is approved.
        let e = approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id, "seen": 1})).await.unwrap_err().0;
        assert!(e.contains("changed since you looked"), "{e}");
        let r = approve_from_window(&hub, &Principal::Local, 21, &ok, json!({"id": id})).await.map_err(|e| e.0).unwrap();
        assert_eq!(r["status"], "active");
    }

    /// A loopback connection held by this test process (outside every session): what Muster's
    /// window looks like to `from_window`. Keep the streams alive while it is used.
    fn window_link() -> (WsLink, (std::net::TcpStream, std::net::TcpStream, std::net::TcpListener)) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_end, _) = listener.accept().unwrap();
        let link = WsLink { ui_origin: true, peer: Some(client.local_addr().unwrap()), server: Some(listener.local_addr().unwrap()) };
        (link, (client, server_end, listener))
    }

    /// Where the owner's window isn't the caller (the CLI, `/api/rpc`, a script, an agent that
    /// dropped `MUSTER_SESSION_ID`): a new schedule, or a change that widens one, is held for a
    /// click; settings, a note's triggers and an agent's accepted work only tighten.
    #[tokio::test]
    async fn outside_the_window_new_work_waits_for_a_click_and_limits_only_tighten() {
        let (hub, _st) = fixture();
        agent(&hub, "rpc-sam"); // Claude: takes human work only
        crate::agents::create(&hub, NewAgent {
            account_profile: None, provider: None, name: "rpc-wk".into(), kind: Some(SessionKind::Codex), model: None,
            effort: None, persona: None, accepts: None, permissions: None, autostart: false, sleep_after_min: None,
        }).unwrap();
        let mk = json!({"agent": "rpc-wk", "prompt": "p", "when": {"kind": "cron", "cron": "0 9 * * *"}});
        // `muster schedule add` (no session: as the owner, through `/api/rpc`): held.
        let r = call(&hub, "schedule.create", mk.clone()).await.unwrap();
        let id = r["id"].as_i64().unwrap();
        assert_eq!((r["status"].as_str(), r["next_run"].as_i64()), (Some("held"), None));
        assert!(r["held"]["why"].as_str().unwrap().contains("not from a click in Muster's window"));
        // Pausing applies; resuming is held; neither approves it.
        assert_eq!(call(&hub, "schedule.pause", json!({"id": id})).await.unwrap()["paused"], true);
        assert_eq!(call(&hub, "schedule.pause", json!({"id": id, "on": false})).await.unwrap()["status"], "held");
        assert!(call(&hub, "schedule.approve", json!({"id": id})).await.unwrap_err().contains("only with a click on Approve"));
        // The owner's click approves it.
        let (link, _conn) = window_link();
        hub.claim_window(9, None).unwrap();
        let r = approve_from_window(&hub, &Principal::Local, 9, &link, json!({"id": id})).await.map_err(|e| e.0).unwrap();
        assert_eq!(r["status"], "active");
        // From the window itself: as asked.
        let r = dispatch_via(&hub, &Principal::Local, "schedule.create", mk.clone(), Via::Window).await.map_err(|e| e.0).unwrap();
        assert_eq!(r["status"], "active");
        // A widening edit from outside: held; a rename: applied.
        assert_eq!(call(&hub, "schedule.update", json!({"id": id, "prompt": "more"})).await.unwrap()["status"], "held");
        let other = r["id"].as_i64().unwrap();
        assert_eq!(call(&hub, "schedule.update", json!({"id": other, "name": "n"})).await.unwrap()["status"], "active");
        // Settings: tightening applies, loosening is refused.
        call(&hub, "schedule.settings.set", json!({"unattended_cap": 5})).await.unwrap();
        let e = call(&hub, "schedule.settings.set", json!({"unattended_cap": 6})).await.unwrap_err();
        assert!(e.contains("takes a click in Muster's window"), "{e}");
        let e = call(&hub, "schedule.settings.set", json!({"watch_folders": ["/tmp"]})).await.unwrap_err();
        assert!(e.contains("watched folder"), "{e}");
        // A note's triggers: off from anywhere, on only from the window.
        let n = crate::notes::create(&hub, Some("Ideas"), "", crate::model::NoteAuthor::human(), None, None).unwrap();
        let e = call(&hub, "note.set_triggers", json!({"id": n.id, "on": true})).await.unwrap_err();
        assert!(e.contains("takes a click in Muster's window"), "{e}");
        call(&hub, "note.set_triggers", json!({"id": n.id, "on": false})).await.unwrap();
        dispatch_via(&hub, &Principal::Local, "note.set_triggers", json!({"id": n.id, "on": true}), Via::Window).await.map_err(|e| e.0).unwrap();
        // An agent's accepted work: it can shrink from anywhere, grow only from the window.
        let e = call(&hub, "agent.update", json!({"name": "rpc-sam", "accepts": ["human", "schedule"]})).await.unwrap_err();
        assert!(e.contains("takes a click in Muster's window"), "{e}");
        call(&hub, "agent.update", json!({"name": "rpc-wk", "accepts": ["human"]})).await.unwrap();
        let r = dispatch_via(&hub, &Principal::Local, "agent.update", json!({"name": "rpc-sam", "accepts": ["human", "schedule"]}), Via::Window)
            .await
            .map_err(|e| e.0)
            .unwrap();
        assert!(r["accepts_origins"].as_array().unwrap().iter().any(|o| o == "schedule"));
    }
}
