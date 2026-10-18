//! MCP server (Streamable HTTP, JSON responses). OWNER: mcp implementer. See docs/MCP.md.
//!
//! `router(hub)` serves the MCP endpoint at "/" of the returned router; the server nests it at
//! `/mcp`. Layout:
//! - `protocol`: JSON-RPC parsing, era/version handling, header validation, method dispatch.
//! - `tools`: tool schemas and implementations.
//! - `guard`: spawn/rate-limit guardrails (pure).
//! - `wait`: wait_for_state staleness rules (pure) + event waiting.
//! - `backend`: the `Backend` trait (hub surface we use) and `HubBackend`.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use crate::hub::{Hub, ServerEvent};

pub mod backend;
pub mod guard;
pub mod protocol;
pub mod tools;
pub mod wait;

#[cfg(test)]
mod tests;

use backend::{Backend, HubBackend};
use protocol::ReqHeaders;

/// Shared state for all MCP requests.
pub struct McpState<B> {
    pub backend: B,
    pub limits: guard::Limits,
    pub guard: parking_lot::Mutex<guard::GuardState>,
    guard_store: Option<crate::store::Store>,
    /// Serializes spawn_session so concurrent calls can't overshoot the spawn limits.
    pub spawn_lock: tokio::sync::Mutex<()>,
}

impl<B: Backend> McpState<B> {
    #[cfg(test)]
    pub fn new(backend: B) -> Self {
        Self::try_with_limits(backend, guard::Limits::default()).expect("fixture guard state")
    }

    pub fn try_with_limits(backend: B, limits: guard::Limits) -> anyhow::Result<Self> {
        let guard_store = backend.guard_store();
        let mut restored: guard::GuardState = match &guard_store {
            Some(store) => store.load_runtime("mcp-guards")?.unwrap_or_default(),
            None => Default::default(),
        };
        if let Some(store) = &guard_store { restored.mcp_channels.extend(store.mcp_channels()?); }
        // A spawn may have completed just before a crash. Its parent's depth was saved
        // before spawning, so removed ancestors cannot shorten the surviving lineage.
        let sessions = backend.sessions();
        for session in &sessions {
            let depth = guard::spawn_depth(&sessions, &restored.spawned_depth, session.id);
            restored.spawned_depth.insert(session.id, depth);
        }
        if let Some(store) = &guard_store { store.save_runtime("mcp-guards", &restored)?; }
        Ok(Self { backend, limits, guard: parking_lot::Mutex::new(restored),
            guard_store, spawn_lock: Default::default() })
    }

    /// Publish guard mutations only after they are durable. A failed write refuses
    /// new agent work instead of silently restoring an exhausted allowance.
    pub fn edit_guard<T>(&self, edit: impl FnOnce(&mut guard::GuardState) -> T) -> Result<T, String> {
        let mut current = self.guard.lock();
        let mut next = current.clone();
        let result = edit(&mut next);
        if next != *current {
            if let Some(store) = &self.guard_store {
                store.save_runtime("mcp-guards", &next).map_err(|error| format!("could not persist MCP limits: {error:#}"))?;
            }
            *current = next;
        }
        Ok(result)
    }
}

/// Watch session updates so wait_for_state knows when queued messages were delivered.
/// Needs a Tokio runtime; without one (never the case in the daemon) waits still work, they
/// just only see deliveries they observe themselves.
pub fn spawn_tracker<B: Backend>(st: &Arc<McpState<B>>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = st.backend.subscribe();
    let weak = Arc::downgrade(st);
    rt.spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let Some(st) = weak.upgrade() else { return };
            let result = st.edit_guard(|guard| match ev {
                ServerEvent::SessionUpserted(info) => guard.observe_session(&info, crate::model::now_ms()),
                ServerEvent::SessionRemoved { id } => guard.forget(id),
                _ => {},
            });
            if let Err(error) = result { tracing::warn!("{error}"); }
        }
    });
}

pub fn router(hub: Arc<Hub>) -> anyhow::Result<Router> {
    let limits = guard::Limits::from_env();
    if limits.allow_shell {
        tracing::warn!(
            "{}=1: agents may run shell commands through MCP",
            guard::ENV_ALLOW_SHELL
        );
    }
    if limits.cross_workspace {
        tracing::warn!(
            "{}=1: agents may reach other workspaces through MCP",
            guard::ENV_CROSS_WORKSPACE
        );
    }
    let st = Arc::new(McpState::try_with_limits(HubBackend(hub.clone()), limits)?);
    hub.set_mcp(Arc::downgrade(&st));
    spawn_tracker(&st);
    Ok(Router::new()
        .route("/", post(handle_post).get(not_allowed).delete(not_allowed))
        .with_state(st))
}

/// Run tool `name` as session `caller`, for a call that session made through the CLI (rpc.rs):
/// the same guards and rate limits as over MCP. Ok(text) or Err(the tool's error text).
pub async fn call_as(hub: &Hub, caller: crate::model::SessionId, name: &str, args: serde_json::Value) -> Result<String, String> {
    let st = hub.mcp().ok_or("the MCP server isn't running")?;
    match protocol::call_audited(&st, caller, name, args).await {
        Ok(o) if !o.is_error => Ok(o.text),
        Ok(o) => Err(o.text),
        Err(()) => Err(format!("unknown tool {name}")),
    }
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

pub fn extract_headers(h: &HeaderMap) -> ReqHeaders {
    ReqHeaders {
        authorization: header_str(h, "authorization"),
        origin: header_str(h, "origin"),
        protocol_version: header_str(h, "mcp-protocol-version"),
        mcp_method: header_str(h, "mcp-method"),
        mcp_name: header_str(h, "mcp-name"),
    }
}

async fn handle_post(
    State(st): State<Arc<McpState<HubBackend>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let reply = protocol::handle_post(&st, &extract_headers(&headers), &body).await;
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match reply.body {
        Some(v) => (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            v.to_string(),
        )
            .into_response(),
        None => status.into_response(),
    }
}

/// No standalone SSE stream (legacy GET) and no sessions to delete.
async fn not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, HeaderValue::from_static("POST"))],
    )
        .into_response()
}
