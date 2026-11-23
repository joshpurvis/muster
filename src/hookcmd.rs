//! Client side of `muster hook <agent> <event> [payload]`: runs inside agent CLIs' hook
//! callbacks, forwards the payload to the daemon, and must never block or fail the agent.
//! OWNER: agents implementer.
//!
//! Wire contract (matches PROTOCOL.md `POST /hook/<event>`):
//! - URL: `$MUSTER_URL/hook/<event>`, where `<event>` is exactly the agent's event name
//!   (`PreToolUse`, `Stop`, `notify`, ...). That same string reaches `Hub::handle_hook` and the
//!   adapter's `on_hook`. The agent kind is NOT in the path: the daemon already knows it from
//!   the session token.
//! - Auth: `Authorization: Bearer $MUSTER_TOKEN` (the per-session token).
//! - Body: the hook's JSON payload: the argv payload if given (Codex `notify` appends it as the
//!   last argument), else stdin (Claude and Codex hooks). Oversized strings are truncated so a
//!   huge tool result doesn't bloat the event log.
//! - Antigravity's title callback accepts only `state`. Its stdin includes account data.
//!   Only validated state, approval status and conversation id may reach either transport.
//!   The title command has no stdout.
//! - Also sends `X-Muster-Agent: <agent>` (informational only).
//!
//! Host-managed runs use `$MUSTER_HOOK_DB`: append to the application-owned durable inbox
//! without requiring a live backend. The backend replays it with transactional checkpoints.
//! Legacy runs retain blocking HTTP/1.1 forwarding. Both paths have an absolute deadline.
//! Always exits 0. Prints nothing on stdout (Claude/Codex treat hook stdout as context or
//! decisions), except Claude's `UserPromptSubmit`: it first asks the backend for the quiet
//! messages waiting for this turn (`POST /quiet/take` with the prompt's envelope message id and
//! the payload's `permission_mode`, src/quiet.rs; never for a subagent's prompt) and prints them
//! as the turn's `additionalContext`, then names them in the payload it saves (`quiet::PAYLOAD_KEY`): the backend keeps them pending for the run, and marks
//! them shown once a later hook of the run's main agent (`PreToolUse`, `PostToolUse`, `Stop`)
//! shows the turn ran. No backend (a reload) means no context: they wait for a later turn. No-ops silently when MUSTER_URL / MUSTER_TOKEN are
//! unset (and no token file stands in for MUSTER_TOKEN, see [`TOKEN_FILE`]).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(1500);
const OVERALL: Duration = Duration::from_millis(2000);
const STDIN_WAIT: Duration = Duration::from_millis(1000);
const MAX_STDIN: usize = 8 << 20;
const MAX_STRING: usize = 4000;
/// Payload keys holding a turn's final message (Claude Stop, Codex notify), and their limit.
const REPLY_KEYS: &[&str] = &["last_assistant_message", "last-assistant-message"];
const MAX_REPLY: usize = 64 * 1024;
/// How long `UserPromptSubmit` waits for the quiet messages before going on without them.
const QUIET_WAIT: Duration = Duration::from_millis(1000);
/// The most of the backend's answer read for them (a turn's context is under 8,000 characters).
const QUIET_MAX_RESPONSE: usize = 256 * 1024;

/// Where a run whose CLI strips credentials from its hooks' environment finds its session token
/// (`<session dir>/hook-token`, private, rewritten at each start): an OpenRouter Claude session
/// runs with `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, which removes `MUSTER_TOKEN` (src/openrouter.rs).
pub const TOKEN_FILE: &str = "hook-token";

/// `$MUSTER_TOKEN`, else the token file of `$MUSTER_SESSION_ID`'s session.
pub(crate) fn session_token() -> Option<String> {
    session_token_from(|k| std::env::var(k).ok())
}

fn session_token_from(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(t) = var("MUSTER_TOKEN").filter(|t| !t.is_empty()) {
        return Some(t);
    }
    let id: crate::model::SessionId = var("MUSTER_SESSION_ID")?.trim().parse().ok()?;
    let t = std::fs::read_to_string(crate::paths::session_dir(id).join(TOKEN_FILE)).ok()?;
    Some(t.trim().to_string()).filter(|t| !t.is_empty() && t.len() <= 256)
}

/// Returns the process exit code (always 0 unless the adapter wants to block the hook).
pub fn run(agent: &str, event: &str, payload_arg: Option<String>) -> i32 {
    let (Ok(url), Some(token)) = (std::env::var("MUSTER_URL"), session_token()) else {
        return 0;
    };
    if !valid_name(event) || url.is_empty() || token.is_empty() {
        return 0;
    }
    let raw = match payload_arg {
        Some(p) => p.into_bytes(),
        None => read_stdin(),
    };
    let Some(mut body) = callback_body(agent, event, &raw) else { return 0 };
    let last = (agent == "antigravity").then(|| {
        let id = std::env::var("MUSTER_SESSION_ID").ok()?.parse().ok()?;
        Some(crate::paths::session_dir(id).join(crate::adapters::antigravity::LAST_STATE))
    }).flatten();
    if last.as_ref().is_some_and(|p| crate::adapters::antigravity::read_last_state(p).as_ref() == Some(&body)) {
        return 0;
    }
    let remember = last.map(|path| {
        let reduced = body.clone();
        move || { let _ = crate::paths::write_private(&path, &reduced); }
    });
    if agent == "claude" && event == "UserPromptSubmit" {
        body = quiet_messages(&url, &token, agent, body, &mut std::io::stdout());
    }
    if let Some(path) = std::env::var_os("MUSTER_HOOK_DB") {
        let (tx, rx) = std::sync::mpsc::channel();
        let event = event.to_string();
        std::thread::spawn(move || {
            let saved = crate::hook_inbox::Inbox::open(std::path::Path::new(&path))
                .and_then(|mut inbox| inbox.enqueue(&token, &event, &body));
            let _ = tx.send(saved);
        });
        let saved = rx.recv_timeout(OVERALL).unwrap_or_else(|_| Err(anyhow::anyhow!("callback inbox timed out")));
        if let Err(error) = saved {
            // No HTTP fallback: an uncertain durable write must not become a duplicate.
            eprintln!("muster hook: callback could not be saved: {error:#}");
        } else if let Some(remember) = remember {
            remember();
        }
        return 0;
    }
    // One absolute deadline over resolve + connect + write + read: the POST runs on a thread
    // we simply stop waiting for (the process exits right after).
    let (tx, rx) = std::sync::mpsc::channel();
    let path = format!("/hook/{event}");
    let agent = agent.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(post(&url, &path, &token, &agent, &body));
    });
    let r = rx
        .recv_timeout(OVERALL)
        .unwrap_or_else(|_| Err("timed out".into()));
    if matches!(r, Ok(200..=299)) && let Some(remember) = remember {
        remember();
    }
    if let Err(e) = r
        && std::env::var_os("MUSTER_HOOK_DEBUG").is_some()
    {
        eprintln!("muster hook: {e}");
    }
    0
}

/// Claude's `UserPromptSubmit` (src/quiet.rs): fetch the quiet messages waiting for this turn,
/// write them to `out` as the hook's `additionalContext`, and return the payload naming them.
/// Any failure (no backend, a slow answer, a slash command) leaves `body` as it was and writes
/// nothing: the messages keep waiting.
fn quiet_messages(url: &str, token: &str, agent: &str, body: Vec<u8>, out: &mut impl Write) -> Vec<u8> {
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(&body) else { return body };
    // A slash command (/clear, /compact) may never reach the model: they wait for a real prompt.
    if payload.get("prompt").and_then(|p| p.as_str()).is_some_and(|p| p.trim_start().starts_with('/')) {
        return body;
    }
    // A subagent's prompt (the human typing while viewing one) never takes the main agent's notes.
    if crate::quiet::subagent_hook(&payload) {
        return body;
    }
    // The prompt's own message, when Muster typed it in (its envelope's id): the backend judges
    // the turn by it (src/quiet.rs `take_run`); any other prompt is the human's turn.
    let msg = payload.get("prompt").and_then(|p| p.as_str()).and_then(crate::agents::parse_envelope);
    // The permission mode the process runs in now (it may differ from the tab's setting): notes
    // checked for a narrower mode wait (`quiet::mode_width`).
    let mode = payload.get("permission_mode").and_then(|m| m.as_str());
    let request = serde_json::json!({"msg": msg, "permission_mode": mode}).to_string().into_bytes();
    let (tx, rx) = std::sync::mpsc::channel();
    let (url, token, agent) = (url.to_string(), token.to_string(), agent.to_string());
    std::thread::spawn(move || {
        let _ = tx.send(post_read(&url, "/quiet/take", &token, &agent, &request, QUIET_MAX_RESPONSE));
    });
    let Ok(Ok((200, answer))) = rx.recv_timeout(QUIET_WAIT) else { return body };
    let Ok(answer) = serde_json::from_slice::<serde_json::Value>(&answer) else { return body };
    let Some(context) = answer.get("context").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) else { return body };
    let hook = serde_json::json!({
        "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": context}
    });
    if writeln!(out, "{hook}").and_then(|()| out.flush()).is_err() {
        return body;
    }
    if let Some(o) = payload.as_object_mut() {
        o.insert(crate::quiet::PAYLOAD_KEY.into(), answer.get("quiet").cloned().unwrap_or_default());
    }
    serde_json::to_vec(&payload).unwrap_or(body)
}

/// `post`, reading the answer: its status and body (at most `max` bytes; `Connection: close`,
/// so the body runs to the end, by `Content-Length` when given).
fn post_read(url: &str, path: &str, token: &str, agent: &str, body: &[u8], max: usize) -> Result<(u16, Vec<u8>), String> {
    let (host, port, base) = parse_url(url)?;
    let addr = loopback_addr(&host, port)?;
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("connect {addr}: {e}"))?;
    s.set_read_timeout(Some(TIMEOUT)).ok();
    s.set_write_timeout(Some(TIMEOUT)).ok();
    let agent = if valid_name(agent) { agent } else { "unknown" };
    let head = format!(
        "POST {base}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nX-Muster-Agent: {agent}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).map_err(|e| format!("write: {e}"))?;
    s.write_all(body).map_err(|e| format!("write: {e}"))?;
    s.flush().ok();
    let mut got = Vec::new();
    s.take(max as u64 + 4096).read_to_end(&mut got).map_err(|e| format!("read: {e}"))?;
    let split = got.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad response")?;
    let head = String::from_utf8_lossy(&got[..split]).to_string();
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).ok_or("bad response")?;
    let mut rest = got[split + 4..].to_vec();
    let header = |name: &str| head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_ascii_lowercase())
    });
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        rest = unchunk(&rest).ok_or("bad chunked body")?;
    } else if let Some(n) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if rest.len() < n {
            return Err("short body".into());
        }
        rest.truncate(n);
    }
    if rest.len() > max {
        return Err("answer too large".into());
    }
    Ok((status, rest))
}

/// A chunked HTTP/1.1 body, decoded (None when malformed).
fn unchunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&b[..eol]).ok()?.split(';').next()?.trim();
        let n = usize::from_str_radix(size, 16).ok()?;
        b = &b[eol + 2..];
        if n == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..n)?);
        b = b.get(n + 2..)?;
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c))
}

/// Read stdin with a deadline, never from a terminal (that would steal the user's keystrokes).
pub(crate) fn read_stdin() -> Vec<u8> {
    #[cfg(unix)]
    if unsafe { libc::isatty(0) } == 1 {
        return Vec::new();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::stdin()
            .lock()
            .take(MAX_STDIN as u64)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(STDIN_WAIT).unwrap_or_default()
}

fn callback_body(agent: &str, event: &str, raw: &[u8]) -> Option<Vec<u8>> {
    if agent == "antigravity" {
        return if event == "state" {
            let value = serde_json::from_slice(raw).ok()?;
            serde_json::to_vec(&antigravity_state(&value)?).ok()
        } else { None };
    }
    Some(shrink_payload(raw))
}

/// Build a fresh object before logging, saving or forwarding anything. The raw payload
/// includes the signed-in email, quota and workspace details, and may acquire more fields.
pub(crate) fn antigravity_state(value: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::{Map, Value};
    let input = value.as_object()?;
    let mut state = Map::new();
    if let Some(s) = input.get("agent_state").and_then(Value::as_str)
        && !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        state.insert("agent_state".into(), s.into());
    }
    if let Some(pending) = input.get("tool_confirmation_pending").and_then(Value::as_bool) {
        state.insert("tool_confirmation_pending".into(), pending.into());
    }
    if let Some(id) = input.get("conversation_id").or_else(|| input.get("session_id")).and_then(Value::as_str)
        && crate::agents::valid_resume_id(id)
    {
        state.insert("session_id".into(), id.into());
    }
    Some(Value::Object(state))
}

/// Parse as JSON and truncate long strings; non-JSON input is sent as a JSON string.
pub(crate) fn shrink_payload(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return b"{}".to_vec();
    }
    let mut v = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(trimmed.to_string()),
    };
    truncate_strings(&mut v);
    serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec())
}

fn truncate_strings(v: &mut serde_json::Value) {
    truncate_strings_max(v, MAX_STRING);
}

fn truncate_strings_max(v: &mut serde_json::Value, max: usize) {
    match v {
        serde_json::Value::String(s) if s.len() > max => {
            let mut cut = max;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            let dropped = s.len() - cut;
            s.truncate(cut);
            s.push_str(&format!("…[{dropped} bytes truncated]"));
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(truncate_strings),
        serde_json::Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                // A turn's final message is an agent's reply (agents.rs): keep much more of it.
                let max = if REPLY_KEYS.contains(&k.as_str()) { MAX_REPLY } else { MAX_STRING };
                truncate_strings_max(v, max);
            }
        }
        _ => {}
    }
}

/// The daemon is always on this machine (hub::daemon_url is 127.0.0.1), but MUSTER_URL reaches
/// us through the agent's environment, which a project's settings can override. The bearer
/// token goes only to a loopback address: an IP literal, or `localhost` resolving to one. No
/// other name is even looked up.
fn loopback_addr(host: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip
            .is_loopback()
            .then(|| std::net::SocketAddr::new(ip, port))
            .ok_or_else(|| format!("MUSTER_URL host {host} is not loopback"));
    }
    if !host.eq_ignore_ascii_case("localhost") {
        return Err(format!("MUSTER_URL host {host:?} is not loopback"));
    }
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .find(|a| a.ip().is_loopback())
        .ok_or_else(|| format!("resolve {host}: no loopback address"))
}

/// `http://host:port[/base]` -> (host, port, base path without trailing slash).
fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported MUSTER_URL {url:?} (need http://)"))?;
    let (hostport, base) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in {url:?}"))?,
        ),
        _ => (hostport.to_string(), 80),
    };
    Ok((
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        port,
        base.to_string(),
    ))
}

pub(crate) fn post(url: &str, path: &str, token: &str, agent: &str, body: &[u8]) -> Result<u16, String> {
    let (host, port, base) = parse_url(url)?;
    let addr = loopback_addr(&host, port)?;
    let mut s =
        TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("connect {addr}: {e}"))?;
    s.set_read_timeout(Some(TIMEOUT)).ok();
    s.set_write_timeout(Some(TIMEOUT)).ok();
    let agent = if valid_name(agent) { agent } else { "unknown" };
    let head = format!(
        "POST {base}{path} HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nX-Muster-Agent: {agent}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    s.write_all(body).map_err(|e| format!("write: {e}"))?;
    s.flush().ok();
    // Read just the status line; the daemon's answer carries nothing we need.
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    while got.len() < 16 {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let line = String::from_utf8_lossy(&got);
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "bad response".to_string())?;
    if status >= 400 {
        return Err(format!("daemon answered {status}"));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrubbed_hook_finds_its_token_in_the_session_folder() {
        let dir = crate::paths::session_dir(987_654);
        crate::paths::ensure_private_dir(&dir).unwrap();
        crate::paths::write_private(&dir.join(TOKEN_FILE), b"run-token-1\n").unwrap();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let from = |p| session_token_from(env(p));
        assert_eq!(from(&[("MUSTER_TOKEN", "from-env"), ("MUSTER_SESSION_ID", "987654")]).as_deref(), Some("from-env"));
        assert_eq!(from(&[("MUSTER_SESSION_ID", "987654")]).as_deref(), Some("run-token-1"));
        assert_eq!(from(&[("MUSTER_TOKEN", ""), ("MUSTER_SESSION_ID", "987654")]).as_deref(), Some("run-token-1"));
        assert_eq!(from(&[("MUSTER_SESSION_ID", "987655")]), None);
        assert_eq!(from(&[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::net::TcpListener;

    #[test]
    fn antigravity_state_only_forwards_allowed_fields() {
        use serde_json::{Value, json};
        let raw = json!({
            "email": "someone@example.invalid", "plan_tier": "sample", "quota": 42,
            "cwd": "/demo-app", "unknown": {"account": "someone@example.invalid"},
            "agent_state": "tool_use", "tool_confirmation_pending": false,
            "conversation_id": "conv-demo-1", "session_id": "older"
        });
        let bytes = callback_body("antigravity", "state", &serde_json::to_vec(&raw).unwrap()).unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(!wire.contains("email") && !wire.contains("example.invalid"));
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), json!({
            "agent_state":"tool_use", "tool_confirmation_pending":false, "session_id":"conv-demo-1"
        }));
    }

    #[test]
    fn antigravity_state_rejects_invalid_values_and_other_events() {
        for raw in [br#"{"agent_state":"bad@state","conversation_id":"--evil","tool_confirmation_pending":1}"#.as_slice(),
                    br#"{"agent_state":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#] {
            assert_eq!(callback_body("antigravity", "state", raw).unwrap(), b"{}");
        }
        assert_eq!(callback_body("antigravity", "state", b"[]"), None);
        assert_eq!(callback_body("antigravity", "state", b"invalid"), None);
        assert_eq!(callback_body("antigravity", "other", b"{}"), None);
        assert_eq!(callback_body("antigravity", "state", br#"{"session_id":"conv-demo-2"}"#).unwrap(),
                   br#"{"session_id":"conv-demo-2"}"#);
    }

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_url("http://127.0.0.1:7433").unwrap(),
            ("127.0.0.1".into(), 7433, "".into())
        );
        assert_eq!(
            parse_url("http://localhost:9/x/").unwrap(),
            ("localhost".into(), 9, "/x".into())
        );
        assert_eq!(
            parse_url("http://[::1]:5").unwrap(),
            ("::1".into(), 5, "".into())
        );
        assert!(parse_url("https://a:1").is_err());
    }

    #[test]
    fn shrinks_payload() {
        let big = "x".repeat(10_000);
        let raw =
            serde_json::to_vec(&serde_json::json!({"tool_response": {"stdout": big}, "n": 1}))
                .unwrap();
        let out: serde_json::Value = serde_json::from_slice(&shrink_payload(&raw)).unwrap();
        let s = out["tool_response"]["stdout"].as_str().unwrap();
        assert!(s.len() < 4100 && s.ends_with("bytes truncated]"));
        assert_eq!(out["n"], 1);
        assert_eq!(shrink_payload(b"  "), b"{}");
        assert_eq!(shrink_payload(b"not json"), b"\"not json\"");
    }

    #[test]
    fn the_token_goes_only_to_loopback() {
        for host in ["127.0.0.1", "127.8.9.10", "::1"] {
            assert!(loopback_addr(host, 7433).is_ok(), "{host}");
        }
        for host in ["10.0.0.1", "0.0.0.0", "192.168.1.5", "::", "fe80::1", "example.com", "localhost.evil.com", "127.0.0.1.nip.io"] {
            assert!(loopback_addr(host, 7433).is_err(), "{host}");
        }
        // Refused before connecting or sending anything.
        let err = post("http://example.com:80", "/hook/Stop", "secret", "claude", b"{}").unwrap_err();
        assert!(err.contains("not loopback"), "{err}");
    }

    #[test]
    fn posts_to_local_listener() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = c.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&req);
                if let Some(i) = s.find("\r\n\r\n") {
                    let len: usize = s
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if req.len() >= i + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            String::from_utf8(req).unwrap()
        });
        let body = shrink_payload(br#"{"type":"agent-turn-complete"}"#);
        let status = post(
            &format!("http://127.0.0.1:{port}"),
            "/hook/notify",
            "tok",
            "codex",
            &body,
        )
        .unwrap();
        assert_eq!(status, 200);
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /hook/notify HTTP/1.1\r\n"));
        assert!(req.contains("Authorization: Bearer tok\r\n"));
        assert!(req.contains("X-Muster-Agent: codex\r\n"));
        assert!(req.ends_with(r#"{"type":"agent-turn-complete"}"#));
    }

    /// A one-request server answering `response`; returns what it was sent.
    fn answer_once(response: &'static [u8]) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = c.read(&mut buf) {
                if n == 0 { break }
                req.extend_from_slice(&buf[..n]);
                if req.ends_with(b"}") { break }
            }
            c.write_all(response).unwrap();
            String::from_utf8(req).unwrap()
        });
        (port, h)
    }

    #[test]
    fn a_prompt_gets_the_waiting_quiet_messages_as_context_and_names_them() {
        let (port, h) = answer_once(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 74\r\n\r\n{\"context\":\"Messages for you\",\"quiet\":{\"agent\":[7],\"session\":[]},\"left\":0}",
        );
        let mut out = Vec::new();
        let body = quiet_messages(&format!("http://127.0.0.1:{port}"), "tok", "claude", br#"{"prompt":"hi","permission_mode":"default"}"#.to_vec(), &mut out);
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /quiet/take HTTP/1.1\r\n") && req.contains("Authorization: Bearer tok\r\n"), "{req}");
        // A typed prompt names no message: the backend takes it as the human's turn. The mode the
        // process runs in goes along.
        assert!(req.ends_with("\r\n\r\n{\"msg\":null,\"permission_mode\":\"default\"}"), "{req}");
        // Claude Code's hook output: one JSON object on stdout.
        let printed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, serde_json::json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "Messages for you"}}));
        let saved: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(saved["prompt"], "hi");
        assert_eq!(saved[crate::quiet::PAYLOAD_KEY], serde_json::json!({"agent": [7], "session": []}));
    }

    #[test]
    fn a_prompt_muster_typed_in_names_its_message() {
        let (port, h) = answer_once(b"HTTP/1.1 200 OK\r\ncontent-length: 25\r\n\r\n{\"context\":null,\"left\":0}");
        let mut out = Vec::new();
        let prompt = br#"{"prompt":"[muster msg 12 from alpha origin=human hop=1]\nrun the tests"}"#.to_vec();
        quiet_messages(&format!("http://127.0.0.1:{port}"), "tok", "claude", prompt, &mut out);
        let req = h.join().unwrap();
        // No mode in the payload: null (the backend counts it as the tab's mode).
        assert!(req.ends_with("\r\n\r\n{\"msg\":12,\"permission_mode\":null}"), "{req}");
    }

    #[test]
    fn a_chunked_answer_is_decoded() {
        // Two chunks whose declared sizes match their bytes: the context reaches the output.
        let (port, h) = answer_once(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n15\r\n{\"context\":\"Chunked\",\r\n2c\r\n\"quiet\":{\"agent\":[4],\"session\":[]},\"left\":0}\r\n0\r\n\r\n",
        );
        let mut out = Vec::new();
        let body = quiet_messages(&format!("http://127.0.0.1:{port}"), "t", "claude", br#"{"prompt":"hi"}"#.to_vec(), &mut out);
        h.join().unwrap();
        let printed: serde_json::Value = serde_json::from_slice(&out).expect("the chunked answer decoded");
        assert_eq!(printed["hookSpecificOutput"]["additionalContext"], "Chunked");
        let saved: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(saved[crate::quiet::PAYLOAD_KEY], serde_json::json!({"agent": [4], "session": []}));
    }

    #[test]
    fn quiet_messages_wait_when_there_is_nothing_to_show_or_no_backend() {
        let payload = br#"{"prompt":"hi"}"#.to_vec();
        // None waiting.
        let (port, h) = answer_once(
            b"HTTP/1.1 200 OK\r\ncontent-length: 25\r\n\r\n{\"context\":null,\"left\":0}",
        );
        let mut out = Vec::new();
        assert_eq!(quiet_messages(&format!("http://127.0.0.1:{port}"), "t", "claude", payload.clone(), &mut out), payload);
        h.join().unwrap();
        assert!(out.is_empty());
        // No backend (a reload): quick, nothing printed, payload as it was.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t = std::time::Instant::now();
        assert_eq!(quiet_messages(&format!("http://127.0.0.1:{closed}"), "t", "claude", payload.clone(), &mut out), payload);
        assert!(out.is_empty() && t.elapsed() < Duration::from_secs(2));
        // An error answer.
        let (port, h) = answer_once(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\n\r\n{}");
        assert_eq!(quiet_messages(&format!("http://127.0.0.1:{port}"), "t", "claude", payload.clone(), &mut out), payload);
        h.join().unwrap();
        assert!(out.is_empty());
        // A slash command never asks: nothing listens on this port, and nothing is tried.
        let slash = br#"{"prompt":"  /clear"}"#.to_vec();
        assert_eq!(quiet_messages(&format!("http://127.0.0.1:{closed}"), "t", "claude", slash.clone(), &mut out), slash);
        assert!(out.is_empty());
        // Nor does a subagent's prompt: the main agent's notes wait for the main agent.
        let sub = br#"{"prompt":"hi","agent_id":"a1"}"#.to_vec();
        assert_eq!(quiet_messages(&format!("http://127.0.0.1:{closed}"), "t", "claude", sub.clone(), &mut out), sub);
        assert!(out.is_empty());
        assert_eq!(unchunk(b"3\r\nabc\r\n0\r\n\r\n").unwrap(), b"abc");
        assert!(unchunk(b"zz\r\n").is_none());
    }

    #[test]
    fn unreachable_daemon_is_quick_and_harmless() {
        // Bind then drop to get a closed port.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let t = std::time::Instant::now();
        assert!(
            post(
                &format!("http://127.0.0.1:{port}"),
                "/hook/x",
                "t",
                "claude",
                b"{}"
            )
            .is_err()
        );
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!valid_name("../x"));
        assert!(valid_name("PreToolUse"));
    }
}
