//! HTTP server (axum), bound to 127.0.0.1 only. Routes per docs/PROTOCOL.md.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use tokio::sync::broadcast;

use crate::devices::{self, Device, Principal};
use crate::hub::{Hub, ServerEvent};

pub fn router(hub: Arc<Hub>) -> anyhow::Result<Router> {
    Ok(Router::new()
        .route("/api/health", get(health))
        // Room for a 1 MiB note or panel HTML even when every character arrives JSON-escaped.
        .route("/api/rpc", post(rpc_http).layer(axum::extract::DefaultBodyLimit::max(crate::mcp::BODY_MAX)))
        .route("/api/events", get(events_stream))
        .route("/api/launch-code", post(launch_code))
        .route("/api/exchange", post(exchange))
        .route("/api/device/pair", post(device_pair))
        .route("/api/device/me", get(device_me))
        .route("/api/device/logout", post(device_logout))
        .route("/ws", get(ws_upgrade))
        .route("/hook/{event}", post(hook))
        // Status-line usage reports (src/statusline.rs): a few numbers, never a whole payload.
        .route("/usage/{agent}", post(usage_report).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)))
        // Trigger URLs (src/schedules/triggers.rs): this machine only, each with its secret.
        .route("/api/trigger/{name}", post(trigger).layer(axum::extract::DefaultBodyLimit::max(crate::schedules::triggers::PAYLOAD_MAX + 1)))
        .with_state(hub.clone())
        .nest("/mcp", crate::mcp::router(hub.clone())?)
        .fallback(crate::assets::serve)
        .layer(axum::middleware::from_fn_with_state(hub, classify)))
}

/// This listener has no Hub, credentials, sessions, middleware or daemon routes.
pub fn panel_router(ui_port: u16, panel_port: u16) -> Router {
    Router::new()
        .route("/proxy", get(panel_proxy)).with_state(ui_port)
        .route("/preview", get(panel_preview)).with_state((ui_port, panel_port))
}

#[derive(Deserialize)]
struct PanelPreviewQuery { url: String }

/// A web_preview panel's frame (src/web_preview.rs): the dev server's URL, checked again here.
async fn panel_preview(State((ui_port, panel_port)): State<(u16, u16)>, Query(query): Query<PanelPreviewQuery>) -> Response {
    let (csp, html) = match crate::web_preview::page(&query.url, ui_port, panel_port) {
        Ok(page) => page,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
         (header::CONTENT_SECURITY_POLICY, csp),
         (header::CACHE_CONTROL, "no-store".to_string()),
         (header::REFERRER_POLICY, "no-referrer".to_string()),
         (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string())], html,
    ).into_response()
}

#[derive(Deserialize)]
struct PanelProxyQuery { csp: Option<String> }

async fn panel_proxy(State(ui_port): State<u16>, Query(query): Query<PanelProxyQuery>) -> Response {
    let csp: crate::model::PanelCsp = match query.csp.as_deref().map(serde_json::from_str).transpose() {
        Ok(csp) => csp.unwrap_or_default(),
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid CSP").into_response(),
    };
    if let Err(e) = crate::panels::validate_csp(&csp) { return (StatusCode::BAD_REQUEST, e.to_string()).into_response(); }
    let origins = json!([format!("http://127.0.0.1:{ui_port}"), format!("http://localhost:{ui_port}")]);
    let html = crate::panels::PROXY.replace("__MUSTER_UI_ORIGINS__", &origins.to_string());
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
         (header::CONTENT_SECURITY_POLICY, crate::panels::proxy_csp(&csp, ui_port)),
         (header::CACHE_CONTROL, "no-store".to_string()),
         (header::REFERRER_POLICY, "no-referrer".to_string()),
         (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string())], html,
    ).into_response()
}

/// How a request reached the daemon (docs/ANDROID.md "Security model").
#[derive(Debug, Clone)]
pub struct Peer {
    /// Came through a proxy (`tailscale serve`): the TCP peer is the proxy's uid, or it carries
    /// forwarding headers (forging those locally only restricts a request further). Proxied
    /// requests reach the phone routes only, and only with a device cookie.
    pub proxied: bool,
    /// For logs and pairing rate limits: the forwarded client IP when the proxy is trusted,
    /// else "local".
    pub source: String,
    /// `Tailscale-User-Login`, only from a trusted proxy (headers from anyone else are ignored).
    pub ts_login: Option<String>,
    /// The TCP peer (its processes are looked up for `schedule.approve`).
    pub addr: Option<SocketAddr>,
}

impl Peer {
    fn local() -> Peer {
        Peer {
            proxied: false,
            source: "local".into(),
            ts_login: None,
            addr: None,
        }
    }
}

/// Uid of the proxy whose identity headers are trusted (`MUSTER_PROXY_UID`, default 0: a
/// system tailscaled).
fn proxy_uid() -> u32 {
    std::env::var("MUSTER_PROXY_UID")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Paths a proxied request may reach at all; everything else is 404 through the proxy.
fn proxied_path_allowed(path: &str) -> bool {
    path == "/m"
        || path.starts_with("/m/")
        || path.starts_with("/api/device/")
        || path == "/ws"
        || path == "/api/rpc"
        || path == "/api/health"
}

fn has_forwarding_headers(h: &HeaderMap) -> bool {
    h.keys().any(|k| {
        let k = k.as_str();
        k == "x-forwarded-for" || k == "forwarded" || k.starts_with("x-forwarded-") || k.starts_with("tailscale-")
    })
}

/// Classify every request (`Peer` extension) and apply the proxied-request policy.
async fn classify(State(hub): State<Arc<Hub>>, mut req: Request, next: Next) -> Response {
    let h = req.headers();
    if h.contains_key("tailscale-funnel-request") {
        // Never reachable from the public internet.
        return (StatusCode::FORBIDDEN, "not available through Funnel").into_response();
    }
    let forwarded = has_forwarding_headers(h);
    let conn = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let proxy = proxy_uid();
    let trusted = match conn {
        // With the daemon itself running as the proxy's uid the socket owner proves nothing.
        Some(peer) if proxy != me => {
            let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), hub.port());
            tokio::task::spawn_blocking(move || socket_owner(peer, server))
                .await
                .ok()
                .flatten()
                == Some(proxy)
        }
        _ => false,
    };
    let mut peer = Peer::local();
    peer.addr = conn;
    peer.proxied = forwarded || trusted;
    if trusted {
        let h = req.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
        if let Some(ip) = get("x-forwarded-for").and_then(|v| v.split(',').next()) {
            peer.source = ip.trim().chars().take(64).collect();
        }
        peer.ts_login = get("tailscale-user-login").map(str::to_string);
    }
    if peer.proxied && !proxied_path_allowed(req.uri().path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    req.extensions_mut().insert(peer);
    next.run(req).await
}

fn cookie_value<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then_some(v.trim())
        })
}

fn device_cookie(token: &str, max_age_s: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_s}",
        devices::COOKIE
    ))
    .expect("cookie is ASCII")
}

/// An `Origin` a device cookie is honoured from: present, not `null`, and the tailnet origin a
/// pairing was issued for (or this daemon's own local origin).
fn device_origin_ok(hub: &Hub, h: &HeaderMap) -> bool {
    let Some(o) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let port = hub.port();
    o != "null"
        && (hub.devices().origin_allowed(o)
            || o == format!("http://127.0.0.1:{port}")
            || o == format!("http://localhost:{port}"))
}

/// Resolve a device from its cookie. `require_origin`: every state-changing route and the
/// WebSocket (the strict `Origin` check is the CSRF boundary: every host in a tailnet is
/// same-site). A request from a trusted proxy carrying a different Tailscale login than the
/// one seen at pairing is refused.
fn device_from_cookie(hub: &Hub, peer: &Peer, h: &HeaderMap, require_origin: bool) -> Result<(Device, String), Response> {
    let token = cookie_value(h, devices::COOKIE).ok_or_else(unauthorized)?;
    let dev = hub.devices().authenticate(token).ok_or_else(unauthorized)?;
    if (require_origin || h.contains_key(header::ORIGIN)) && !device_origin_ok(hub, h) {
        tracing::warn!("device {} refused: origin not allowed ({})", dev.id, peer.source);
        return Err(forbidden("origin not allowed"));
    }
    if let (Some(seen), Some(now)) = (&dev.ts_login, &peer.ts_login)
        && seen != now
    {
        tracing::warn!("device {} refused: tailscale login {now} is not {seen}", dev.id);
        return Err(unauthorized());
    }
    Ok((dev, token.to_string()))
}

fn has_x_muster(h: &HeaderMap) -> bool {
    h.get("x-muster").is_some_and(|v| v == "1")
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

/// `/ws`: the client token (`?token=`, local only) or a device cookie.
async fn ws_upgrade(
    State(hub): State<Arc<Hub>>,
    Extension(peer): Extension<Peer>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // A page of Muster's own (the desktop window, or the UI in a browser) sends one of its
    // origins; a script's WebSocket usually sends none. Only such a connection may approve an
    // agent's proposal (`rpc::approve_from_window`).
    let ui_origin = headers.contains_key(header::ORIGIN) && origin_allowed(&headers, hub.port());
    let who = if !q.token.is_empty() || headers.contains_key(header::AUTHORIZATION) {
        if peer.proxied || !hub.check_client_token(&q.token) {
            return unauthorized();
        }
        if !origin_allowed(&headers, hub.port()) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
        Principal::Local
    } else {
        match device_from_cookie(&hub, &peer, &headers, true) {
            // The cookie's sliding renewal happens on `/api/device/me` (it can set the cookie).
            Ok((d, _)) => Principal::Device { id: d.id },
            Err(r) => return r,
        }
    };
    let link = crate::rpc::WsLink { ui_origin, peer: peer.addr, server: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), hub.port())) };
    ws.max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| crate::rpc::ws_session(socket, hub, who, link))
}

/// `POST /api/rpc`: bearer client token (local only) or a device cookie plus `X-Muster: 1`.
async fn rpc_http(
    State(hub): State<Arc<Hub>>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let who = if let Some(t) = bearer(&headers) {
        if peer.proxied || !hub.check_client_token(t) {
            return unauthorized();
        }
        Principal::Local
    } else {
        if !has_x_muster(&headers) {
            return forbidden("missing X-Muster header");
        }
        match device_from_cookie(&hub, &peer, &headers, true) {
            Ok((d, _)) => Principal::Device { id: d.id },
            Err(r) => return r,
        }
    };
    axum::Json(crate::rpc::http_call(&hub, &who, &body).await).into_response()
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    name: Option<String>,
}

fn device_json(d: &Device) -> serde_json::Value {
    json!({ "device": d })
}

/// `POST /api/device/pair` `{code, name?}` -> `{device}` + the device cookie. 403 on any
/// failure (same body whatever went wrong), 429 when rate limited.
async fn device_pair(
    State(hub): State<Arc<Hub>>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    const REFUSED: &str = "invalid or expired pairing code";
    if !has_x_muster(&headers) || !device_origin_ok(&hub, &headers) {
        return forbidden(REFUSED);
    }
    let Ok(req) = serde_json::from_slice::<PairBody>(&body) else {
        return forbidden(REFUSED);
    };
    match hub
        .devices()
        .redeem(hub.store(), &req.code, req.name, &peer.source, peer.ts_login.clone())
    {
        Ok((dev, token)) => {
            let mut r = axum::Json(device_json(&dev)).into_response();
            r.headers_mut()
                .insert(header::SET_COOKIE, device_cookie(&token, devices::LIFETIME_MS / 1000));
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Err(devices::PairError::RateLimited) => {
            tracing::warn!("pairing rate limited for {}", peer.source);
            (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({ "error": { "message": "too many attempts; wait a minute" } })),
            )
                .into_response()
        }
        Err(devices::PairError::Invalid) => {
            tracing::warn!("pairing refused for {}", peer.source);
            forbidden(REFUSED)
        }
    }
}

/// `GET /api/device/me` -> `{device}` or 401. Renews the cookie when the expiry slid.
async fn device_me(State(hub): State<Arc<Hub>>, Extension(peer): Extension<Peer>, headers: HeaderMap) -> Response {
    let (dev, token) = match device_from_cookie(&hub, &peer, &headers, false) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let renewed = hub.devices().touch(hub.store(), dev.id, &peer.source);
    let mut r = axum::Json(device_json(&dev)).into_response();
    if renewed {
        r.headers_mut()
            .insert(header::SET_COOKIE, device_cookie(&token, devices::LIFETIME_MS / 1000));
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `POST /api/device/logout`: revoke this device and clear its cookie.
async fn device_logout(
    State(hub): State<Arc<Hub>>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
) -> Response {
    if !has_x_muster(&headers) {
        return forbidden("missing X-Muster header");
    }
    let (dev, _) = match device_from_cookie(&hub, &peer, &headers, true) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(e) = hub.devices().revoke(hub.store(), Some(dev.id)) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
    }
    let mut r = axum::Json(json!({})).into_response();
    r.headers_mut().insert(header::SET_COOKIE, device_cookie("", 0));
    r
}

/// Whether `GET /api/events` streams this event: tab, session, window and named-agent changes
/// (records and messages) only (no event-log entries, notices or workspaces, and never terminal
/// output).
fn streamed(ev: &ServerEvent) -> bool {
    matches!(
        ev,
        ServerEvent::SessionUpserted(_)
            | ServerEvent::SessionRemoved { .. }
            | ServerEvent::ChannelUpserted(_)
            | ServerEvent::ChannelRemoved { .. }
            | ServerEvent::WindowUpserted(_)
            | ServerEvent::WindowRemoved { .. }
            | ServerEvent::AgentUpserted(_)
            | ServerEvent::AgentRemoved { .. }
            | ServerEvent::AgentMessage(_)
    )
}

/// `GET /api/events` (client token): the event bus as NDJSON, one `{"event", "data"}` object
/// per line, as the WebSocket sends them. A reader that falls behind the bus gets
/// `{"event":"lagged","data":{"missed":n}}` and the stream goes on (re-sync with `hello`).
async fn events_stream(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !bearer(&headers).is_some_and(|t| hub.check_client_token(t)) {
        return unauthorized();
    }
    if !origin_allowed(&headers, hub.port()) {
        return forbidden("origin not allowed");
    }
    // Subscribed before the response starts, so a client that sees the headers and then calls
    // `hello` misses nothing in between.
    let rx = hub.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            let line = match rx.recv().await {
                Ok(ev) if streamed(&ev) => serde_json::to_string(&ev).ok(),
                Ok(_) => None,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    Some(json!({ "event": "lagged", "data": { "missed": n } }).to_string())
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            if let Some(mut line) = line {
                line.push('\n');
                return Some((Ok::<_, std::convert::Infallible>(Bytes::from(line)), rx));
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
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

/// `POST /api/trigger/<name>` with `Authorization: Bearer <the trigger's secret>`: fire a URL
/// trigger. Loopback only: a request through a proxy (`tailscale serve`) never gets here (and is
/// refused here too). The body, at most 16 KiB, reaches the agent as quoted untrusted data.
async fn trigger(
    State(hub): State<Arc<Hub>>,
    Extension(peer): Extension<Peer>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if peer.proxied {
        return (StatusCode::FORBIDDEN, "trigger URLs answer on this machine only").into_response();
    }
    let Some(secret) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": { "message": "send the trigger's secret: Authorization: Bearer <secret>" } })))
            .into_response();
    };
    let payload = (!body.is_empty()).then_some(&body[..]);
    match crate::schedules::triggers::fire_url(&hub, &name, Some(secret), payload, "Its trigger URL was called") {
        Ok(v) => axum::Json(v).into_response(),
        Err((code, msg)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            axum::Json(json!({ "error": { "message": msg } })),
        )
            .into_response(),
    }
}

async fn hook(State(hub): State<Arc<Hub>>, Path(event): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    // First: keystrokes recorded after this point came after the callback (`Hub::handle_hook_at`).
    let arrival = hub.human_input_seq();
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
    match hub.handle_hook_at(token, &event, payload, arrival) {
        Ok(()) => axum::Json(json!({})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": { "message": format!("{e:#}") } })),
        )
            .into_response(),
    }
}

/// `POST /usage/claude` with the run's session token: attributed by the token alone (to that
/// session, run and pinned profile), kept out of hooks, events and agent state.
async fn usage_report(State(hub): State<Arc<Hub>>, Path(agent): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    if agent != "claude" {
        return (StatusCode::NOT_FOUND, axum::Json(json!({ "error": { "message": "usage reports come from Claude" } }))).into_response();
    }
    if hub.session_by_token(token).is_none() {
        return unauthorized();
    }
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": { "message": "send JSON" } }))).into_response();
    };
    match hub.record_usage(token, &payload) {
        Ok(()) => axum::Json(json!({})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": { "message": format!("{e:#}") } }))).into_response(),
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

/// Which program a loopback connection comes from, as far as telling the owner's window from a
/// program an agent started goes (`rpc::from_window`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// Every process that holds the connection, or might (one of the owner's processes that
    /// hides its descriptors), was looked at, and none descends from a session's process.
    Outside,
    /// This process holds it, or might, and descends from a session's process.
    Inside(u32),
    /// It can't be told (no procfs, as on macOS; the connection isn't listed; nothing that holds
    /// it was found; a lookup failed). Never taken as the owner's window.
    Unknown(String),
}

/// Who holds the client end (`peer`) of an established loopback connection to `server`: the
/// socket's inode and owner from /proc/net/tcp and tcp6 (the whole 4-tuple must match), then
/// every process with a descriptor on it (`/proc/<pid>/fd`). A process of the socket's owner
/// whose descriptors can't be read (it turned off `PR_SET_DUMPABLE`, as ssh-agent does, or an
/// agent's program could) might be the one: it counts as a holder too, judged by its parents
/// (`/proc/<pid>/stat` stays readable). Inside when any of them descends from one of `roots`
/// (the sessions' processes); Unknown when anything can't be read or nothing holds it.
pub fn connection_holder(peer: SocketAddr, server: SocketAddr, roots: &std::collections::HashSet<u32>) -> Holder {
    const ESTABLISHED: &str = "01";
    let unknown = |why: &str| Holder::Unknown(why.to_string());
    let mut inodes: Vec<String> = Vec::new();
    let mut owner: Option<u32> = None;
    let mut listed = false;
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        listed = true;
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != ESTABLISHED {
                continue;
            }
            if parse_proc_addr(f[1]).is_some_and(|a| same_endpoint(a, peer))
                && parse_proc_addr(f[2]).is_some_and(|a| same_endpoint(a, server))
                && f[9] != "0"
            {
                let Ok(uid) = f[7].parse::<u32>() else { return unknown("its owner in /proc/net/tcp can't be read") };
                if owner.is_some_and(|o| o != uid) {
                    return unknown("two users hold that connection");
                }
                owner = Some(uid);
                inodes.push(format!("socket:[{}]", f[9]));
            }
        }
    }
    if !listed {
        return unknown("this system has no /proc/net/tcp to look the connection up in");
    }
    let Some(owner) = owner else { return unknown("the connection isn't listed in /proc/net/tcp") };
    let Ok(procs) = std::fs::read_dir("/proc") else { return unknown("/proc can't be listed") };
    let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let mut holders: Vec<u32> = Vec::new();
    let mut hidden: Vec<u32> = Vec::new();
    for p in procs {
        let Ok(p) = p else { return unknown("/proc can't be listed") };
        let Some(pid) = p.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        // A process of the socket's owner whose descriptors can't be read may hold it.
        let hides = |hidden: &mut Vec<u32>| {
            if proc_uids(pid).is_some_and(|u| u.contains(&owner)) {
                hidden.push(pid);
            }
        };
        match std::fs::read_dir(p.path().join("fd")) {
            Ok(fds) => {
                let mut unreadable = false;
                for fd in fds {
                    match fd.and_then(|fd| std::fs::read_link(fd.path())) {
                        Ok(l) if inodes.iter().any(|i| l.as_os_str() == i.as_str()) => {
                            holders.push(pid);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) if gone(&e) => {}
                        Err(_) => unreadable = true,
                    }
                }
                if unreadable && !holders.contains(&pid) {
                    hides(&mut hidden);
                }
            }
            Err(e) if gone(&e) => {}
            Err(_) => hides(&mut hidden),
        }
    }
    if holders.is_empty() && hidden.is_empty() {
        return unknown("no process holds the connection");
    }
    for pid in holders.iter().chain(hidden.iter()) {
        match descent(*pid, roots) {
            Some(true) => return Holder::Inside(*pid),
            Some(false) => {}
            // Gone meanwhile: it holds nothing now. Still there but its parents unreadable:
            // can't tell.
            None if !std::path::Path::new(&format!("/proc/{pid}")).exists() => {}
            None => return Holder::Unknown(format!("the parents of process {pid} can't be read")),
        }
    }
    Holder::Outside
}

/// A process's user ids (real, effective, saved, filesystem), from `/proc/<pid>/status`, which
/// stays readable when its descriptors don't. None: gone, or unreadable.
fn proc_uids(pid: u32) -> Option<Vec<u32>> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    Some(line.split_whitespace().filter_map(|u| u.parse().ok()).collect())
}

/// Is `pid`, or a process it descends from (its parent, their parent, …), one of `roots`? From
/// `/proc/<pid>/stat`. None when one of them can't be read (gone meanwhile, or no procfs). A
/// process whose parent ended was handed to init or a subreaper, so it no longer descends from
/// where it started.
pub fn descent(pid: u32, roots: &std::collections::HashSet<u32>) -> Option<bool> {
    let mut cur = pid;
    for _ in 0..256 {
        if roots.contains(&cur) {
            return Some(true);
        }
        let stat = std::fs::read_to_string(format!("/proc/{cur}/stat")).ok()?;
        match crate::gui::parse_proc_stat(&stat) {
            Some((_, ppid)) if ppid > 1 && ppid != cur => cur = ppid,
            Some(_) => return Some(false),
            None => return None,
        }
    }
    None
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
    fn events_route_streams_state_events_only() {
        assert!(streamed(&ServerEvent::SessionRemoved { id: 1 }));
        assert!(streamed(&ServerEvent::ChannelRemoved { id: 1 }));
        assert!(streamed(&ServerEvent::WindowRemoved { id: 1 }));
        assert!(!streamed(&ServerEvent::WorkspaceRemoved { id: 1 }));
        assert!(!streamed(&ServerEvent::Notice {
            level: "info".into(),
            text: "x".into()
        }));
    }

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

    /// A raw HTTP/1.1 exchange: (status, head, body). Keeps the stream for upgrades.
    fn raw(port: u16, req: &str) -> (u16, String, String, std::net::TcpStream) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read the head (and whatever body arrives with it; bodies here are small).
        loop {
            let n = s.read(&mut chunk).unwrap_or(0);
            buf.extend_from_slice(&chunk[..n]);
            if n == 0 || buf.windows(4).any(|w| w == b"\r\n\r\n") {
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < p + 4 + len {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, head.to_string(), body.to_string(), s)
    }

    fn http(port: u16, method: &str, path: &str, headers: &[&str], body: &str) -> (u16, String, String) {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for h in headers {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        req.push_str(body);
        let (st, head, body, _) = raw(port, &req);
        (st, head, body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn device_auth_end_to_end() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = crate::hub::HubConfig {
            port,
            client_token: "tok".into(),
            muster_bin: std::path::PathBuf::from("/bin/true"),
            terminal_host: None,
            openrouter_key: None,
        };
        let hub = Hub::new(cfg, crate::store::Store::open_memory().unwrap()).unwrap();
        let app = router(hub.clone()).unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap()
        });
        const ORIGIN: &str = "Origin: https://box.tail1.ts.net";
        const FWD: &str = "X-Forwarded-For: 100.64.0.9";
        let t = tokio::task::spawn_blocking(move || {
            let hello = r#"{"id":1,"method":"hello","params":{}}"#;
            // Local client token works; through the proxy it doesn't, and the rest is hidden.
            assert_eq!(http(port, "POST", "/api/rpc", &["Authorization: Bearer tok"], hello).0, 200);
            // Usage reports need a session token: not the client's, not none.
            for auth in [&["Authorization: Bearer tok"][..], &[]] {
                assert_eq!(http(port, "POST", "/usage/claude", auth, "{}").0, 401);
            }
            assert_eq!(http(port, "POST", "/api/rpc", &["Authorization: Bearer tok", FWD], hello).0, 401);
            for path in ["/api/events", "/", "/mcp", "/api/launch-code", "/hook/x", "/usage/claude"] {
                assert_eq!(http(port, "GET", path, &[FWD], "").0, 404, "{path}");
            }
            assert_eq!(http(port, "GET", "/m/", &["Tailscale-Funnel-Request: ?1"], "").0, 403);
            let (st, head, _) = http(port, "GET", "/m/", &[FWD], "");
            assert!(st == 200 || st == 404, "{st}");
            assert!(head.to_ascii_lowercase().contains("content-security-policy"), "{head}");

            // Pairing.
            let (st, _, body) = http(
                port,
                "POST",
                "/api/rpc",
                &["Authorization: Bearer tok"],
                r#"{"id":1,"method":"device.pair_code","params":{"url":"https://box.tail1.ts.net"}}"#,
            );
            assert_eq!(st, 200);
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            let code = v["result"]["code"].as_str().unwrap().to_string();
            assert_eq!(v["result"]["url"].as_str().unwrap(), format!("https://box.tail1.ts.net/m/#pair={code}"));
            let pair_body = format!(r#"{{"code":"{code}","name":"pixel"}}"#);
            // Needs X-Muster and a known origin.
            assert_eq!(http(port, "POST", "/api/device/pair", &[FWD, ORIGIN], &pair_body).0, 403);
            assert_eq!(
                http(port, "POST", "/api/device/pair", &[FWD, "X-Muster: 1", "Origin: https://evil.ts.net"], &pair_body).0,
                403
            );
            let (st, head, body) = http(port, "POST", "/api/device/pair", &[FWD, ORIGIN, "X-Muster: 1"], &pair_body);
            assert_eq!(st, 200, "{body}");
            let cookie_line = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
                .unwrap()
                .to_string();
            assert!(cookie_line.contains("HttpOnly") && cookie_line.contains("Secure") && cookie_line.contains("SameSite=Strict"));
            let kv = cookie_line.split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string();
            assert!(kv.starts_with("__Host-muster_device="));
            let cookie = format!("Cookie: {kv}");
            // Single use.
            assert_eq!(http(port, "POST", "/api/device/pair", &[FWD, ORIGIN, "X-Muster: 1"], &pair_body).0, 403);

            // The cookie authenticates `me`, RPC (with X-Muster and origin) and the socket.
            assert_eq!(http(port, "GET", "/api/device/me", &[FWD, &cookie], "").0, 200);
            assert_eq!(http(port, "GET", "/api/device/me", &[FWD], "").0, 401);
            let (st, _, body) = http(port, "POST", "/api/rpc", &[FWD, &cookie, ORIGIN, "X-Muster: 1"], hello);
            assert_eq!(st, 200);
            assert!(body.contains("\"result\""), "{body}");
            let result: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(result["result"].get("panels").is_none() && result["result"].get("panel_origin").is_none());
            let (_, _, body) = http(port, "POST", "/api/rpc", &[FWD, &cookie, ORIGIN, "X-Muster: 1"],
                r#"{"id":4,"method":"panel.resource","params":{"id":1}}"#);
            assert!(body.contains("not available to devices"), "{body}");
            let (_, _, body) = http(
                port,
                "POST",
                "/api/rpc",
                &[FWD, &cookie, ORIGIN, "X-Muster: 1"],
                r#"{"id":2,"method":"device.list","params":{}}"#,
            );
            assert!(body.contains("not available to devices"), "{body}");
            let (_, _, body) = http(
                port,
                "POST",
                "/api/rpc",
                &[FWD, &cookie, ORIGIN, "X-Muster: 1"],
                r#"{"id":3,"method":"session.approve","params":{"id":1,"decision":"approve"}}"#,
            );
            assert!(body.contains("state_seq is required"), "{body}");

            // Phone alerts: the device's own subscription, under the endpoint rules.
            let rpc = |body: &str| {
                let (st, _, b) = http(port, "POST", "/api/rpc", &[FWD, &cookie, ORIGIN, "X-Muster: 1"], body);
                assert_eq!(st, 200, "{b}");
                serde_json::from_str::<serde_json::Value>(&b).unwrap()
            };
            let (pk, auth) = crate::push::test_keys();
            let subscribe = |endpoint: &str| {
                rpc(&json!({"id": 5, "method": "push.subscribe",
                    "params": {"endpoint": endpoint, "p256dh": pk, "auth": auth}}).to_string())
            };
            let v = rpc(r#"{"id":5,"method":"push.key","params":{}}"#);
            assert_eq!(crate::push::unb64(v["result"]["public_key"].as_str().unwrap()).unwrap().len(), 65);
            for bad in ["http://fcm.googleapis.com/fcm/send/x", "https://evil.example/fcm/send/x", "https://127.0.0.1:9/x"] {
                let v = subscribe(bad);
                assert!(v["error"]["message"].as_str().unwrap().starts_with(crate::push::REFUSED), "{bad} accepted: {v}");
            }
            let v = rpc(r#"{"id":5,"method":"push.status","params":{}}"#);
            assert!(v["result"]["subscription"].is_null(), "{v}");
            let v = subscribe("https://fcm.googleapis.com/fcm/send/abc");
            assert_eq!(v["result"]["subscription"]["endpoint_host"], "fcm.googleapis.com", "{v}");
            let (_, _, body) = http(port, "POST", "/api/rpc", &["Authorization: Bearer tok"],
                r#"{"id":6,"method":"push.status","params":{}}"#);
            assert!(body.contains("for a paired phone"), "the desktop has no alerts of its own: {body}");
            let (_, _, body) = http(port, "POST", "/api/rpc", &["Authorization: Bearer tok"],
                r#"{"id":6,"method":"device.list","params":{}}"#);
            let list: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(list["result"][0]["push"]["endpoint_host"], "fcm.googleapis.com", "{list}");
            assert!(list["result"][0]["push"].get("p256dh").is_none() && !body.contains(&auth), "keys stay in the daemon");

            assert_eq!(http(port, "POST", "/api/rpc", &[FWD, &cookie, ORIGIN], hello).0, 403);
            assert_eq!(http(port, "POST", "/api/rpc", &[FWD, &cookie, "X-Muster: 1"], hello).0, 403);

            let upgrade = |extra: &str| {
                format!(
                    "GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                     Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{FWD}\r\n{cookie}\r\n{extra}\r\n"
                )
            };
            assert_eq!(raw(port, &upgrade("")).0, 403, "a cookie socket needs an Origin");
            let (st, _, _, mut ws) = raw(port, &upgrade(&format!("{ORIGIN}\r\n")));
            assert_eq!(st, 101);

            // Revoke from the desktop: the socket closes, the cookie stops working.
            let (_, _, body) = http(
                port,
                "POST",
                "/api/rpc",
                &["Authorization: Bearer tok"],
                r#"{"id":1,"method":"device.revoke","params":{"all":true}}"#,
            );
            assert!(body.contains("\"revoked\":[1]"), "{body}");
            use std::io::Read;
            let mut got = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match ws.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => got.extend_from_slice(&chunk[..n]),
                    Err(e) => panic!("socket not closed after revoke: {e}"),
                }
            }
            assert_eq!(http(port, "GET", "/api/device/me", &[FWD, &cookie], "").0, 401);
            // Revoking deleted the device's push subscription.
            let (_, _, body) = http(port, "POST", "/api/rpc", &["Authorization: Bearer tok"],
                r#"{"id":7,"method":"device.list","params":{}}"#);
            let list: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(list["result"][0]["push"].is_null(), "{list}");
        });
        t.await.unwrap();
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

    #[cfg(target_os = "linux")]
    #[test]
    fn a_connections_holder_and_what_it_descends_from() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (_srv, peer) = l.accept().unwrap();
        let me = std::process::id();
        let server = l.local_addr().unwrap();
        let none = std::collections::HashSet::new();
        // Held by this process, which descends from no session: outside; this process as a
        // session's: inside.
        assert_eq!(connection_holder(peer, server, &none), Holder::Outside);
        assert_eq!(connection_holder(peer, server, &[me].into_iter().collect()), Holder::Inside(me));
        // A connection that isn't listed: unknown, never outside.
        assert!(matches!(connection_holder(peer, "127.0.0.1:1".parse().unwrap(), &none), Holder::Unknown(_)), "another destination");
        // This process descends from itself and from its parent; not from an unrelated pid.
        let parent = std::os::unix::process::parent_id();
        assert_eq!(descent(me, &[me].into_iter().collect()), Some(true));
        assert_eq!(descent(me, &[parent].into_iter().collect()), Some(true));
        assert_eq!(descent(me, &[u32::MAX - 1].into_iter().collect()), Some(false));
        assert_eq!(descent(u32::MAX - 1, &none), None, "no such process: can't tell");
        // A child process descends from us.
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert_eq!(descent(child.id(), &[me].into_iter().collect()), Some(true));
        let _ = child.kill();
        let _ = child.wait();
        drop(c);
    }

    /// A program that hides its descriptors (`PR_SET_DUMPABLE` off) while it holds the
    /// connection: its parents still show, so one started in a session is still inside.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_holder_that_hides_its_descriptors_is_judged_by_its_parents() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = l.local_addr().unwrap();
        let me = std::process::id();
        // A python child connects, turns dumpable off and waits.
        let code = format!(
            "import ctypes,socket,sys,time\nctypes.CDLL(None).prctl(4,0,0,0,0)\ns=socket.create_connection(('127.0.0.1',{}))\nprint(s.getsockname()[1],flush=True)\ntime.sleep(10)",
            server.port()
        );
        let Ok(mut child) = std::process::Command::new("python3").args(["-I", "-c", &code]).stdout(std::process::Stdio::piped()).spawn() else {
            eprintln!("no python3: skipped");
            return;
        };
        let (_srv, peer) = l.accept().unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
        assert_eq!(line.trim().parse::<u16>().unwrap(), peer.port());
        assert!(std::fs::read_dir(format!("/proc/{}/fd", child.id())).is_err(), "its descriptors are hidden");
        // The child as a session's process (or this test as one): inside, though its
        // descriptors can't be read.
        assert_eq!(connection_holder(peer, server, &[child.id()].into_iter().collect()), Holder::Inside(child.id()));
        assert_eq!(connection_holder(peer, server, &[me].into_iter().collect()), Holder::Inside(child.id()));
        let _ = child.kill();
        let _ = child.wait();
    }
}
