//! JSON-RPC + MCP protocol handling for the Streamable HTTP transport, independent of axum.
//!
//! Dual-era server (see modelcontextprotocol.io/specification/2026-07-28/basic/versioning):
//! - **Modern** (2026-07-28): stateless. Every request carries
//!   `params._meta["io.modelcontextprotocol/protocolVersion"]`; HTTP headers
//!   `MCP-Protocol-Version`, `Mcp-Method` and (for tools/call) `Mcp-Name` must mirror the body.
//!   Methods: server/discover, tools/list, tools/call. Results carry `resultType`.
//! - **Legacy** (2025-11-25, 2025-06-18, 2025-03-26): `initialize` handshake,
//!   `notifications/initialized`, `ping`, tools/list, tools/call. We never mint
//!   `Mcp-Session-Id` (auth is the bearer token; the server is stateless), which the legacy
//!   spec allows. JSON-RPC batches (2025-03-26 only) are accepted.
//! Responses are always `application/json`; we never open SSE streams.

use base64::Engine;
use serde_json::{Map, Value, json};

use super::McpState;
use super::backend::Backend;
use super::tools;
use crate::model::{EventKind, SessionId};

pub const SERVER_NAME: &str = "muster";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Newest first; the first entry is our answer when a client asks for something unknown.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const HEADER_MISMATCH: i64 = -32020;
pub const UNSUPPORTED_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "muster runs many coding agents and shells side by side, grouped into channels \
(one task each, usually its own git worktree). You are one of those sessions. Use whoami to orient, \
list_channels/list_sessions to see what's running, spawn_session to delegate, send_message + wait_for_state \
to drive another agent, read_screen/get_recent_output to inspect it, post_to_channel to tell the human \
something, and get_diff to review work. Notes (note_list, note_read, note_write, note_append, note_open) are \
plain-text notes the human and every agent share: append to add without conflicts; a full write needs the rev \
from note_read. You cannot approve permission prompts; the human does that.";

/// The HTTP headers we look at, already extracted (case-insensitively) by the caller.
#[derive(Debug, Default, Clone)]
pub struct ReqHeaders {
    pub authorization: Option<String>,
    pub origin: Option<String>,
    pub protocol_version: Option<String>,
    pub mcp_method: Option<String>,
    pub mcp_name: Option<String>,
}

impl ReqHeaders {
    pub fn bearer(&self) -> Option<&str> {
        let a = self.authorization.as_deref()?.trim();
        let (scheme, tok) = a.split_once(' ')?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| tok.trim())
            .filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    /// None = empty body (202 / 405).
    pub body: Option<Value>,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body: Some(body),
        }
    }
    fn accepted() -> Self {
        Self {
            status: 202,
            body: None,
        }
    }
}

pub fn error_obj(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut err = json!({"code": code, "message": message.into()});
    if let Some(d) = data {
        err["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": err})
}

fn result_obj(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Allow only loopback origins (DNS-rebinding protection). Absent Origin is fine (CLI clients).
pub fn origin_allowed(origin: &str) -> bool {
    let rest = match origin.split_once("://") {
        Some((scheme, rest)) if matches!(scheme, "http" | "https" | "tauri" | "wry") => rest,
        _ => return false,
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = if let Some(h) = host.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
        || origin.starts_with("tauri://")
        || origin.starts_with("wry://")
}

/// Decode an `Mcp-Name`/`Mcp-Param-*` value that may use the `=?base64?...?=` sentinel.
pub fn decode_header_value(v: &str) -> Option<String> {
    match v
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|b| String::from_utf8(b).ok()),
        None => Some(v.to_string()),
    }
}

fn all_versions() -> Vec<&'static str> {
    MODERN_VERSIONS
        .iter()
        .chain(LEGACY_VERSIONS)
        .copied()
        .collect()
}

fn server_info() -> Value {
    json!({"name": SERVER_NAME, "title": "muster", "version": SERVER_VERSION})
}

fn capabilities() -> Value {
    json!({"tools": {"listChanged": false}})
}

/// Handle one POST body. `caller` resolution happens here so it is covered by tests.
pub async fn handle_post<B: Backend>(st: &McpState<B>, h: &ReqHeaders, body: &[u8]) -> Reply {
    if let Some(o) = &h.origin
        && !origin_allowed(o)
    {
        return Reply::json(
            403,
            json!({"jsonrpc": "2.0", "error": {"code": INVALID_REQUEST, "message": "Forbidden origin"}}),
        );
    }
    let Some(caller) = h.bearer().and_then(|t| st.backend.session_by_token(t)) else {
        return Reply::json(
            401,
            json!({"jsonrpc": "2.0", "error": {"code": INVALID_REQUEST, "message": "Unauthorized: send `Authorization: Bearer <MUSTER_TOKEN>` (this session's token)"}}),
        );
    };
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Reply::json(
                400,
                error_obj(Value::Null, PARSE_ERROR, format!("Parse error: {e}"), None),
            );
        }
    };
    match msg {
        Value::Array(items) => {
            if items.is_empty() {
                return Reply::json(
                    400,
                    error_obj(Value::Null, INVALID_REQUEST, "Empty batch", None),
                );
            }
            let mut out = Vec::new();
            for item in items {
                if let Some(body) = handle_message(st, h, caller, item, true).await.body {
                    out.push(body);
                }
            }
            if out.is_empty() {
                Reply::accepted()
            } else {
                Reply::json(200, Value::Array(out))
            }
        }
        other => handle_message(st, h, caller, other, false).await,
    }
}

async fn handle_message<B: Backend>(
    st: &McpState<B>,
    h: &ReqHeaders,
    caller: SessionId,
    msg: Value,
    in_batch: bool,
) -> Reply {
    let Value::Object(m) = msg else {
        return Reply::json(
            400,
            error_obj(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request: expected a JSON-RPC object",
                None,
            ),
        );
    };
    let id = m.get("id").cloned();
    if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Reply::json(
            400,
            error_obj(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: jsonrpc must be \"2.0\"",
                None,
            ),
        );
    }
    let Some(method) = m.get("method").and_then(Value::as_str).map(str::to_string) else {
        // A JSON-RPC response from the client: we never send requests, so just accept it.
        if id.is_some() && (m.contains_key("result") || m.contains_key("error")) {
            return Reply::accepted();
        }
        return Reply::json(
            400,
            error_obj(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: missing method",
                None,
            ),
        );
    };
    let Some(id) = id else {
        // Notification (notifications/initialized, notifications/cancelled, ...): accept.
        return Reply::accepted();
    };
    if !(id.is_string() || id.is_number()) {
        return Reply::json(
            400,
            error_obj(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request: id must be a string or number",
                None,
            ),
        );
    }
    let empty = Map::new();
    let params = match m.get("params") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(p)) => p,
        Some(_) => {
            return Reply::json(
                200,
                error_obj(id, INVALID_PARAMS, "params must be an object", None),
            );
        }
    };
    let modern = params
        .get("_meta")
        .and_then(|meta| meta.get(META_VERSION))
        .and_then(Value::as_str)
        .map(str::to_string);

    match modern {
        // Batching was removed after 2025-03-26; modern requests must come one per POST so
        // their mirrored headers can be checked.
        Some(_) if in_batch => Reply::json(
            200,
            error_obj(
                id,
                INVALID_REQUEST,
                "JSON-RPC batches are not supported for protocol 2026-07-28; send one request per POST",
                None,
            ),
        ),
        Some(ver) if method != "initialize" => {
            handle_modern(st, h, caller, id, &method, params, &ver).await
        }
        _ => handle_legacy(st, h, caller, id, &method, params).await,
    }
}

async fn handle_modern<B: Backend>(
    st: &McpState<B>,
    h: &ReqHeaders,
    caller: SessionId,
    id: Value,
    method: &str,
    params: &Map<String, Value>,
    ver: &str,
) -> Reply {
    if !MODERN_VERSIONS.contains(&ver) {
        return Reply::json(
            400,
            error_obj(
                id,
                UNSUPPORTED_VERSION,
                "Unsupported protocol version",
                Some(json!({"supported": all_versions(), "requested": ver})),
            ),
        );
    }
    {
        let mismatch = |what: String| {
            Reply::json(
                400,
                error_obj(
                    id.clone(),
                    HEADER_MISMATCH,
                    format!("Header mismatch: {what}"),
                    None,
                ),
            )
        };
        match h.protocol_version.as_deref() {
            Some(v) if v == ver => {}
            Some(v) => {
                return mismatch(format!(
                    "MCP-Protocol-Version header '{v}' does not match body '{ver}'"
                ));
            }
            None => return mismatch("missing MCP-Protocol-Version header".into()),
        }
        match h.mcp_method.as_deref() {
            Some(v) if v == method => {}
            Some(v) => {
                return mismatch(format!(
                    "Mcp-Method header '{v}' does not match body '{method}'"
                ));
            }
            None => return mismatch("missing Mcp-Method header".into()),
        }
        if method == "tools/call" {
            let body_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match h.mcp_name.as_deref().map(decode_header_value) {
                Some(Some(v)) if v == body_name => {}
                Some(Some(v)) => {
                    return mismatch(format!(
                        "Mcp-Name header '{v}' does not match body '{body_name}'"
                    ));
                }
                Some(None) => return mismatch("malformed Mcp-Name header".into()),
                None => return mismatch("missing Mcp-Name header".into()),
            }
        }
    }
    let meta = json!({META_SERVER_INFO: server_info()});
    let result = match method {
        "server/discover" => json!({
            "resultType": "complete",
            "supportedVersions": all_versions(),
            "capabilities": capabilities(),
            "instructions": INSTRUCTIONS,
            "ttlMs": 300_000,
            "cacheScope": "private",
            "_meta": meta,
        }),
        "tools/list" => json!({
            "resultType": "complete",
            "tools": tools::tool_definitions(),
            "ttlMs": 300_000,
            "cacheScope": "private",
            "_meta": meta,
        }),
        "tools/call" => match call(st, caller, params).await {
            Ok(mut r) => {
                r["resultType"] = json!("complete");
                r["_meta"] = meta;
                r
            }
            Err((code, msg)) => return Reply::json(200, error_obj(id, code, msg, None)),
        },
        _ => {
            return Reply::json(
                404,
                error_obj(
                    id,
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                    None,
                ),
            );
        }
    };
    Reply::json(200, result_obj(id, result))
}

async fn handle_legacy<B: Backend>(
    st: &McpState<B>,
    h: &ReqHeaders,
    caller: SessionId,
    id: Value,
    method: &str,
    params: &Map<String, Value>,
) -> Reply {
    if let Some(v) = h.protocol_version.as_deref()
        && MODERN_VERSIONS.contains(&v)
    {
        return Reply::json(
            400,
            error_obj(
                id,
                HEADER_MISMATCH,
                format!(
                    "Header mismatch: MCP-Protocol-Version is '{v}' but the body has no \
                     params._meta[\"{META_VERSION}\"]"
                ),
                None,
            ),
        );
    }
    if let Some(v) = h.protocol_version.as_deref()
        && method != "initialize"
        && !LEGACY_VERSIONS.contains(&v)
    {
        return Reply::json(
            400,
            error_obj(
                id,
                INVALID_REQUEST,
                format!("Unsupported MCP-Protocol-Version: {v}"),
                Some(json!({"supported": all_versions()})),
            ),
        );
    }
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = if LEGACY_VERSIONS.contains(&requested) {
                requested
            } else {
                LEGACY_VERSIONS[0]
            };
            json!({
                "protocolVersion": version,
                "capabilities": capabilities(),
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools::tool_definitions()}),
        "server/discover" => json!({
            "supportedVersions": all_versions(),
            "capabilities": capabilities(),
            "serverInfo": server_info(),
            "instructions": INSTRUCTIONS,
        }),
        "tools/call" => match call(st, caller, params).await {
            Ok(r) => r,
            Err((code, msg)) => return Reply::json(200, error_obj(id, code, msg, None)),
        },
        _ => {
            return Reply::json(
                200,
                error_obj(
                    id,
                    METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                    None,
                ),
            );
        }
    };
    Reply::json(200, result_obj(id, result))
}

/// Writes the McpCall audit event exactly once: on `finish`, or on drop if the request was
/// cancelled mid-call (client disconnected during a long wait_for_state, for example).
struct Audit<'a, B: Backend> {
    st: &'a McpState<B>,
    caller: SessionId,
    tool: String,
    args: Value,
    done: bool,
}

impl<B: Backend> Audit<'_, B> {
    fn write(&mut self, ok: bool, cancelled: bool) {
        self.done = true;
        let channel = self.st.backend.session(self.caller).map(|s| s.channel_id);
        let mut data = json!({
            "session_id": self.caller,
            "tool": self.tool,
            "args": tools::truncate_args(&self.args),
            "ok": ok,
        });
        if cancelled {
            data["cancelled"] = json!(true);
        }
        self.st
            .backend
            .log_event(channel, Some(self.caller), EventKind::McpCall, data);
    }
}

impl<B: Backend> Drop for Audit<'_, B> {
    fn drop(&mut self) {
        if !self.done {
            self.write(false, true);
        }
    }
}

/// A tool call made for a session outside the MCP transport (its `muster send`/`ask` through
/// the CLI, rpc.rs): the same tool code and the same McpCall audit event (`via: "cli"`).
pub async fn call_audited<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    name: &str,
    mut args: Value,
) -> Result<tools::ToolOutput, ()> {
    let out = tools::call_tool(st, caller, name, &args).await;
    if let Value::Object(m) = &mut args {
        m.insert("via".into(), json!("cli"));
    }
    let mut audit = Audit {
        st,
        caller,
        tool: name.to_string(),
        args,
        done: false,
    };
    audit.write(matches!(&out, Ok(o) if !o.is_error), false);
    out
}

/// tools/call: run the tool, log an McpCall event in the caller's channel.
async fn call<B: Backend>(
    st: &McpState<B>,
    caller: SessionId,
    params: &Map<String, Value>,
) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str);
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let mut audit = Audit {
        st,
        caller,
        tool: name.unwrap_or("").to_string(),
        args,
        done: false,
    };
    let Some(name) = name else {
        audit.write(false, false);
        return Err((INVALID_PARAMS, "tools/call requires params.name".into()));
    };
    let out = tools::call_tool(st, caller, name, &audit.args).await;
    audit.write(matches!(&out, Ok(o) if !o.is_error), false);
    match out {
        Ok(o) => Ok(json!({"content": [{"type": "text", "text": o.text}], "isError": o.is_error})),
        Err(()) => Err((INVALID_PARAMS, format!("Unknown tool: {name}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(origin_allowed("http://127.0.0.1:7433"));
        assert!(origin_allowed("http://localhost"));
        assert!(origin_allowed("http://[::1]:9000"));
        assert!(origin_allowed("tauri://localhost"));
        assert!(!origin_allowed("http://evil.com"));
        assert!(!origin_allowed("http://127.0.0.1.evil.com:80"));
        assert!(!origin_allowed("null"));
    }

    #[test]
    fn header_decoding() {
        assert_eq!(decode_header_value("whoami").as_deref(), Some("whoami"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(decode_header_value("=?base64?!!!?="), None);
    }

    #[test]
    fn bearer_parsing() {
        let h = |a: &str| ReqHeaders {
            authorization: Some(a.into()),
            ..Default::default()
        };
        assert_eq!(h("Bearer abc").bearer(), Some("abc"));
        assert_eq!(h("bearer  abc ").bearer(), Some("abc"));
        assert_eq!(h("Basic abc").bearer(), None);
        assert_eq!(h("Bearer ").bearer(), None);
    }
}
