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
//! - Also sends `X-Muster-Agent: <agent>` (informational only).
//!
//! Host-managed runs use `$MUSTER_HOOK_DB`: append to the application-owned durable inbox
//! without requiring a live backend. The backend replays it with transactional checkpoints.
//! Legacy runs retain blocking HTTP/1.1 forwarding. Both paths have an absolute deadline.
//! Always exits 0 and prints nothing on stdout (Claude/Codex may treat hook stdout as context
//! or decisions). No-ops silently when MUSTER_URL / MUSTER_TOKEN are unset (and no token file
//! stands in for MUSTER_TOKEN, see [`TOKEN_FILE`]).

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
    let body = shrink_payload(&raw);
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
    if let Err(e) = r
        && std::env::var_os("MUSTER_HOOK_DEBUG").is_some()
    {
        eprintln!("muster hook: {e}");
    }
    0
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
