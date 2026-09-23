//! HTTP server (axum), bound to 127.0.0.1 only. Routes per docs/PROTOCOL.md.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use crate::hub::Hub;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/rpc", post(rpc_http))
        .route("/api/launch-code", post(launch_code))
        .route("/api/exchange", post(exchange))
        .route("/ws", get(ws_upgrade))
        .route("/hook/{event}", post(hook))
        .with_state(hub.clone())
        .nest("/mcp", crate::mcp::router(hub))
        .fallback(crate::assets::serve)
}

async fn health() -> impl IntoResponse {
    axum::Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let v = v.trim();
    v.strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
        .map(str::trim)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": { "message": "unauthorized" } })),
    )
        .into_response()
}

/// Browsers always send Origin on WebSocket upgrades; only our own UI origins are allowed.
fn origin_allowed(headers: &HeaderMap, port: u16) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin == format!("http://127.0.0.1:{port}")
        || origin == format!("http://localhost:{port}")
        || origin.starts_with("tauri://")
        || origin.starts_with("wry://")
    {
        return true;
    }
    // Development escape hatch (e.g. a frontend dev server on another port).
    std::env::var("MUSTER_ALLOW_ORIGIN").is_ok_and(|o| !o.is_empty() && o.split(',').any(|o| o.trim() == origin))
}

#[derive(Deserialize)]
struct WsQuery {
    #[serde(default)]
    token: String,
}

async fn ws_upgrade(
    State(hub): State<Arc<Hub>>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !hub.check_client_token(&q.token) {
        return unauthorized();
    }
    if !origin_allowed(&headers, hub.port()) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| crate::rpc::ws_session(socket, hub))
}

async fn rpc_http(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    if !bearer(&headers).is_some_and(|t| hub.check_client_token(t)) {
        return unauthorized();
    }
    axum::Json(crate::rpc::http_call(&hub, &body).await).into_response()
}

fn forbidden(msg: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({ "error": { "message": msg } })),
    )
        .into_response()
}

/// `POST /api/launch-code` (client token): mint a single-use code for opening the UI without
/// putting the token on a browser command line.
async fn launch_code(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !bearer(&headers).is_some_and(|t| hub.check_client_token(t)) {
        return unauthorized();
    }
    if !origin_allowed(&headers, hub.port()) {
        return forbidden("origin not allowed");
    }
    axum::Json(json!({ "code": hub.mint_launch_code() })).into_response()
}

#[derive(Deserialize)]
struct ExchangeBody {
    code: String,
}

/// `POST /api/exchange` `{"code"}` -> `{"token"}`; the code is consumed. 403 if it is unknown,
/// used or expired.
async fn exchange(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    if !origin_allowed(&headers, hub.port()) {
        return forbidden("origin not allowed");
    }
    let Ok(req) = serde_json::from_slice::<ExchangeBody>(&body) else {
        return forbidden("invalid or expired launch code");
    };
    match hub.exchange_launch_code(req.code.trim()) {
        Some(token) => axum::Json(json!({ "token": token })).into_response(),
        None => forbidden("invalid or expired launch code"),
    }
}

async fn hook(State(hub): State<Arc<Hub>>, Path(event): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    let payload = if body.iter().all(|b| b.is_ascii_whitespace()) {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    if hub.session_by_token(token).is_none() {
        return unauthorized();
    }
    match hub.handle_hook(token, &event, payload) {
        Ok(()) => axum::Json(json!({})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": { "message": format!("{e:#}") } })),
        )
            .into_response(),
    }
}
