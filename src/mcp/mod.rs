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
    /// Serializes spawn_agent so concurrent calls can't overshoot the spawn limits.
    pub spawn_lock: tokio::sync::Mutex<()>,
}

impl<B: Backend> McpState<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            limits: guard::Limits::default(),
            guard: Default::default(),
            spawn_lock: Default::default(),
        }
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
            match ev {
                ServerEvent::SessionUpserted(info) => st
                    .guard
                    .lock()
                    .observe_session(&info, crate::model::now_ms()),
                ServerEvent::SessionRemoved { id } => st.guard.lock().forget(id),
                _ => {}
            }
        }
    });
}

pub fn router(hub: Arc<Hub>) -> Router {
    let st = Arc::new(McpState::new(HubBackend(hub)));
    spawn_tracker(&st);
    Router::new()
        .route("/", post(handle_post).get(not_allowed).delete(not_allowed))
        .with_state(st)
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
