//! HTTP server (axum), bound to 127.0.0.1 only. Routes per docs/PROTOCOL.md.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade};
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
/// used or expired. A code can end up on a browser command line, which other local users can
/// read, so only a browser request (Origin present and ours) from a socket owned by the
/// daemon's own user may redeem it.
async fn exchange(
    State(hub): State<Arc<Hub>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if headers.get(header::ORIGIN).is_none() || !origin_allowed(&headers, hub.port()) {
        return forbidden("origin not allowed");
    }
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), hub.port());
    let owner = tokio::task::spawn_blocking(move || socket_owner(peer, server))
        .await
        .ok()
        .flatten();
    if owner != Some(me) {
        tracing::warn!("launch code exchange refused: connection from {peer} is owned by uid {owner:?}");
        return forbidden("invalid or expired launch code");
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

/// Uid owning the client end (`peer`) of an established loopback connection to `server`, from
/// /proc/net/tcp and /proc/net/tcp6. The whole 4-tuple must match (a local port can be shared
/// by sockets to other destinations, and TIME_WAIT rows belong to no one). None unless exactly
/// one owner is found (or without procfs).
pub fn socket_owner(peer: SocketAddr, server: SocketAddr) -> Option<u32> {
    const ESTABLISHED: &str = "01";
    let mut found: Option<u32> = None;
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 8 || f[3] != ESTABLISHED {
                continue;
            }
            let local = parse_proc_addr(f[1]).is_some_and(|a| same_endpoint(a, peer));
            let remote = parse_proc_addr(f[2]).is_some_and(|a| same_endpoint(a, server));
            if local && remote {
                let uid: u32 = f[7].parse().ok()?;
                if found.is_some_and(|u| u != uid) {
                    return None;
                }
                found = Some(uid);
            }
        }
    }
    found
}

fn same_endpoint(a: SocketAddr, b: SocketAddr) -> bool {
    let canon = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    a.port() == b.port() && canon(a.ip()) == canon(b.ip())
}

/// `0100007F:1F90` (IPv4) or 32 hex digits (IPv6) plus `:PORT`, as /proc/net/tcp* prints
/// them: each 32-bit word in host byte order.
fn parse_proc_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |h: &str| u32::from_str_radix(h, 16).ok().map(|w| w.to_ne_bytes());
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(ip)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(&ip[i * 8..i * 8 + 8])?);
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_addr_parsing() {
        // Little-endian host: 127.0.0.1:8080.
        if cfg!(target_endian = "little") {
            assert_eq!(
                parse_proc_addr("0100007F:1F90"),
                Some("127.0.0.1:8080".parse().unwrap())
            );
            assert_eq!(
                parse_proc_addr("0000000000000000FFFF00000100007F:0050"),
                Some("[::ffff:127.0.0.1]:80".parse().unwrap())
            );
            assert!(same_endpoint(
                "[::ffff:127.0.0.1]:80".parse().unwrap(),
                "127.0.0.1:80".parse().unwrap()
            ));
        }
        assert_eq!(parse_proc_addr("zz:1"), None);
    }

    #[test]
    fn peer_socket_owner_is_our_uid() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (_srv, peer) = l.accept().unwrap();
        assert_eq!(peer, c.local_addr().unwrap());
        // SAFETY: geteuid has no preconditions.
        let server = l.local_addr().unwrap();
        assert_eq!(socket_owner(peer, server), Some(unsafe { libc::geteuid() }));
        // The same client port towards another destination doesn't count.
        assert_eq!(socket_owner(peer, "127.0.0.1:1".parse().unwrap()), None);
        assert_eq!(socket_owner("127.0.0.1:1".parse().unwrap(), server), None);
    }
}
