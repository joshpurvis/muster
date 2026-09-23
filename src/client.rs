//! Tiny blocking HTTP/1.1 client for the CLI (`muster status/open/ls/...`). Only talks to our
//! own daemon on 127.0.0.1, so it stays dependency-free and never needs a tokio runtime.

use std::io::{BufRead, BufReader, Read, Write};
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

/// A streaming response body (`GET /api/events`), de-chunked as it is read.
pub struct BodyStream {
    r: BufReader<TcpStream>,
    chunked: bool,
    /// Bytes left in the current chunk.
    left: usize,
    /// The CRLF after a finished chunk is still unread.
    crlf: bool,
    done: bool,
}

impl Read for BodyStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.r.read(buf);
        }
        while !self.done {
            if self.left > 0 {
                let want = buf.len().min(self.left);
                let n = self.r.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                self.left -= n;
                self.crlf = self.left == 0;
                return Ok(n);
            }
            let mut line = String::new();
            if self.crlf {
                self.r.read_line(&mut line)?;
                self.crlf = false;
                line.clear();
            }
            if self.r.read_line(&mut line)? == 0 {
                break;
            }
            let size = line.split(';').next().unwrap_or("").trim();
            match usize::from_str_radix(size, 16) {
                Ok(0) => self.done = true,
                Ok(n) => self.left = n,
                Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size")),
            }
        }
        Ok(0)
    }
}

/// `GET path` and return the body as a stream once the daemon answered 200 (no read timeout).
pub fn get_stream(port: u16, path: &str, bearer: &str) -> anyhow::Result<BodyStream> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(1000)).context("connecting to daemon")?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAuthorization: Bearer {bearer}\r\n\r\n"
    );
    s.write_all(req.as_bytes())?;
    let mut r = BufReader::new(s);
    let mut status_line = String::new();
    r.read_line(&mut status_line)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP status line")?;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            anyhow::bail!("daemon closed the connection");
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let l = h.to_ascii_lowercase();
        chunked |= l.starts_with("transfer-encoding:") && l.contains("chunked");
    }
    if status == 404 {
        anyhow::bail!("the running daemon has no {path}; after an update, restart it with `muster stop`");
    }
    if status != 200 {
        anyhow::bail!("daemon returned HTTP {status} for {path}");
    }
    r.get_ref().set_read_timeout(None)?;
    Ok(BodyStream {
        r,
        chunked,
        left: 0,
        crlf: false,
        done: false,
    })
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

/// Mint a single-use launch code (`POST /api/launch-code`). `None`: the daemon predates
/// launch codes (404).
pub fn launch_code(d: &Discovery) -> anyhow::Result<Option<String>> {
    let (status, resp) = request(
        d.port,
        "POST",
        "/api/launch-code",
        Some(&d.token),
        None,
        Duration::from_secs(10),
    )?;
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        anyhow::bail!("daemon returned HTTP {status} for a launch code");
    }
    let v: Value = serde_json::from_slice(&resp).context("invalid JSON from daemon")?;
    v["code"]
        .as_str()
        .map(|c| Some(c.to_string()))
        .context("no launch code in the daemon's reply")
}

/// Call a JSON method through `POST /api/rpc`.
pub fn rpc(d: &Discovery, method: &str, params: Value) -> anyhow::Result<Value> {
    rpc_with_timeout(d, method, params, Duration::from_secs(60))
}

/// `rpc` with a custom read timeout (long enough for `session.wait`).
pub fn rpc_with_timeout(d: &Discovery, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
    let body = serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))?;
    let (status, resp) = request(d.port, "POST", "/api/rpc", Some(&d.token), Some(&body), timeout)?;
    if status != 200 {
        anyhow::bail!("daemon returned HTTP {status}");
    }
    let v: Value = serde_json::from_slice(&resp).context("invalid JSON from daemon")?;
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown error");
        if msg.starts_with("unknown method") {
            anyhow::bail!(
                "{msg} (the running daemon, version {}, doesn't support it; after an update, restart it with `muster stop`)",
                d.version
            );
        }
        anyhow::bail!("{msg}");
    }
    Ok(v["result"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot server that answers any request with `resp`.
    fn serve(resp: &'static [u8]) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            let _ = c.write_all(resp);
        });
        port
    }

    #[test]
    fn stream_dechunks_lines() {
        let port = serve(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n6\r\n{\"a\":1\r\n3\r\n}\n{\r\n7\r\n\"b\":2}\n\r\n0\r\n\r\n",
        );
        let body = get_stream(port, "/api/events", "t").unwrap();
        let lines: Vec<String> = BufReader::new(body).lines().map(Result::unwrap).collect();
        assert_eq!(lines, [r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn stream_rejects_non_200() {
        let port = serve(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
        let e = get_stream(port, "/api/events", "bad").err().unwrap();
        assert!(format!("{e:#}").contains("401"), "{e:#}");
    }
}
