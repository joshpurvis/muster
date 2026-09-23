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

pub fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

pub fn hello(hub: &Hub) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "workspaces": hub.workspaces(),
        "channels": hub.channels(),
        "sessions": hub.sessions(),
        "windows": hub.windows(),
        "home": crate::paths::home_dir().to_string_lossy(),
        "default_shell": default_shell(),
    })
}

/// Every method except `session.attach` / `session.detach` (connection-scoped, WS only).
pub async fn dispatch(hub: &Arc<Hub>, method: &str, params: Value) -> RpcResult {
    match method {
        "hello" => Ok(hello(hub)),

        "workspace.add" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
                name: Option<String>,
            }
            let p: P = parse(params)?;
            to_value(hub.add_workspace(p.path, p.name).await?)
        }
        "workspace.remove" => {
            let p: IdParam<WorkspaceId> = parse(params)?;
            hub.remove_workspace(p.id).await?;
            Ok(json!({}))
        }

        "channel.create" => {
            let p: CreateChannel = parse(params)?;
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
            let p: SpawnSession = parse(params)?;
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
            hub.resize_session(p.id, p.cols, p.rows)?;
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
        "session.rename" => {
            let p: Rename<SessionId> = parse(params)?;
            to_value(hub.rename_session(p.id, p.name)?)
        }
        "session.move" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                channel_id: ChannelId,
            }
            let p: P = parse(params)?;
            to_value(hub.move_session(p.id, p.channel_id)?)
        }
        "session.send" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                text: String,
                #[serde(default)]
                mode: Option<SendMode>,
            }
            let p: P = parse(params)?;
            if hub.session(p.id).is_none() {
                return Err(RpcError(format!("no such session: {}", p.id)));
            }
            to_value(inject::send(hub, p.id, p.text, p.mode.unwrap_or_default(), "human".into()).await?)
        }
        "session.approve" => {
            #[derive(Deserialize)]
            struct P {
                id: SessionId,
                decision: Decision,
            }
            let p: P = parse(params)?;
            inject::approve(hub, p.id, p.decision)?;
            Ok(json!({}))
        }
        "session.interrupt" => {
            let p: IdParam<SessionId> = parse(params)?;
            inject::interrupt(hub, p.id)?;
            Ok(json!({}))
        }
        "session.clear_queue" => {
            let p: IdParam<SessionId> = parse(params)?;
            inject::clear_queue(hub, p.id);
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
            }
            let p: P = parse(params)?;
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

        "session.attach" | "session.detach" | "window.claim" => {
            Err(RpcError(format!("{method} is only available over the WebSocket")))
        }
        _ => Err(RpcError(format!("unknown method: {method}"))),
    }
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

/// `POST /api/rpc` body handler: `{method, params, id?}` -> `{id, result}` / `{id, error}`.
pub async fn http_call(hub: &Arc<Hub>, body: &[u8]) -> Value {
    match serde_json::from_slice::<Request>(body) {
        Ok(req) => {
            let r = dispatch(hub, &req.method, req.params).await;
            response(&req.id, r)
        }
        Err(e) => response(&Value::Null, Err(RpcError(format!("invalid request: {e}")))),
    }
}

type Outbox = mpsc::Sender<Message>;

/// One WebSocket client connection.
pub async fn ws_session(socket: WebSocket, hub: Arc<Hub>) {
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
    let events = {
        let out = out_tx.clone();
        let mut rx = hub.subscribe();
        tokio::spawn(async move {
            let forward = |ev: ServerEvent| serde_json::to_string(&ev).ok().map(|t| Message::Text(t.into()));
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

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { break };
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
                handle_request(&hub, conn, req, &out_tx, &resp_tx, &attachments).await;
            }
            Message::Binary(data) => {
                if data.len() >= 5 && data[0] == FRAME_INPUT {
                    let id = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
                    if let Err(e) = hub.write_input(id, &data[5..]) {
                        tracing::debug!("input for session {id}: {e}");
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
    hub.release_windows(conn);
    drop(resp_tx);
    events.abort();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_request(
    hub: &Arc<Hub>,
    conn: u64,
    req: Request,
    out: &Outbox,
    resp: &Outbox,
    attachments: &Arc<Mutex<HashMap<SessionId, JoinHandle<()>>>>,
) {
    match req.method.as_str() {
        "session.attach" => {
            let id = match parse::<IdParam<SessionId>>(req.params) {
                Ok(p) => p.id,
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
            match hub.attach(id).await {
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
                    let task = tokio::spawn(forward_output(hub.clone(), id, generation, rx, bus, out.clone()));
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
            let resp = resp.clone();
            tokio::spawn(async move {
                // A `session.wait` stops once the connection is gone. Other methods run to
                // completion (cancelling one halfway could leave state behind).
                let r = if req.method == "session.wait" {
                    tokio::select! {
                        r = dispatch(&hub, &req.method, req.params) => r,
                        _ = resp.closed() => return,
                    }
                } else {
                    dispatch(&hub, &req.method, req.params).await
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
    mut generation: Option<u64>,
    mut rx: Option<broadcast::Receiver<Bytes>>,
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
                Ok(bytes) => {
                    if out.send(Message::Binary(frame(FRAME_OUTPUT, id, &bytes))).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Slow client: start over from a fresh replay.
                    if !resend_replay(&hub, id, &mut generation, &mut rx, &out).await {
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
                    if !resend_replay(&hub, id, &mut generation, &mut rx, &out).await {
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
    generation: &mut Option<u64>,
    rx: &mut Option<broadcast::Receiver<Bytes>>,
    out: &Outbox,
) -> bool {
    let (replay, new_rx, new_gen) = match hub.attach(id).await {
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
