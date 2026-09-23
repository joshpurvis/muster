//! Tiny blocking HTTP/1.1 client for the CLI (`muster status/open/...`). Only talks to our
//! own daemon on 127.0.0.1, so it stays dependency-free and never needs a tokio runtime.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::paths::Discovery;

pub fn request(
    port: u16,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(1000)).context("connecting to daemon")?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let body = body.unwrap_or(&[]);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    if let Some(t) = bearer {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP status line")?;
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked")
    });
    if chunked {
        body = dechunk(&body);
    }
    Ok((status, body))
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = b.windows(2).position(|w| w == b"\r\n") {
        let size_s = String::from_utf8_lossy(&b[..eol]);
        let Ok(size) = usize::from_str_radix(size_s.split(';').next().unwrap_or("").trim(), 16) else {
            break;
        };
        b = &b[eol + 2..];
        if size == 0 || b.len() < size {
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
    out
}

pub fn healthy(port: u16) -> bool {
    matches!(
        request(port, "GET", "/api/health", None, None, Duration::from_secs(2)),
        Ok((200, body)) if serde_json::from_slice::<Value>(&body).is_ok_and(|v| v["ok"] == true)
    )
}

/// Call a JSON method through `POST /api/rpc`.
pub fn rpc(d: &Discovery, method: &str, params: Value) -> anyhow::Result<Value> {
    let body = serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))?;
    let (status, resp) = request(
        d.port,
        "POST",
        "/api/rpc",
        Some(&d.token),
        Some(&body),
        Duration::from_secs(60),
    )?;
    if status != 200 {
        anyhow::bail!("daemon returned HTTP {status}");
    }
    let v: Value = serde_json::from_slice(&resp).context("invalid JSON from daemon")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("{}", err["message"].as_str().unwrap_or("unknown error"));
    }
    Ok(v["result"].clone())
}
