//! The `web_preview` panel (docs/MCP.md "Built-in apps"): a local dev server, live, beside the
//! agent's terminal. Unlike the other built-ins it is not an MCP App in the sandbox proxy (whose
//! frames are opaque-origin and may not nest frames, docs/AGENT_PANELS.md "Security"); the UI
//! draws its own toolbar and frames the panel server's `/preview` page, which frames exactly the
//! dev server's URL under a `frame-src` naming only that origin. So the framed page can navigate
//! or redirect only within its own origin, never to Muster's own ports or off loopback, and
//! agent-written apps gain nothing: nested frames stay blocked in the sandbox.
//!
//! Only loopback http(s) URLs are accepted (`localhost` or `127.0.0.1`, any port but Muster's
//! own), checked here at `panel_open`/`panel_update` and again when `/preview` is served. IPv6
//! literals (`[::1]`) are refused: browsers cannot name an IPv6 host in a CSP source, so such a
//! frame could not be held to its origin; `localhost` reaches a server on ::1 too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};

/// Stored as the panel's app HTML; never served (the UI draws a web preview itself).
pub const PLACEHOLDER: &str = "<!doctype html><!-- web_preview: drawn by the Muster UI around /preview -->";
/// The panel server's `/preview` page.
pub const PAGE: &str = include_str!("../web/panels/preview.html");
const MAX_URL: usize = 2048;

/// Where any refusal points the agent instead.
const ELSEWHERE: &str = "web_preview frames only local dev servers: http(s)://localhost or 127.0.0.1, \
    with a port. To show any other page, open it in the user's system browser instead (xdg-open URL on Linux, open URL on macOS)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host { Localhost, V4 }

impl Host {
    fn as_str(self) -> &'static str {
        match self { Host::Localhost => "localhost", Host::V4 => "127.0.0.1" }
    }
    /// Where the check connects: `localhost` may be a server listening on ::1 only.
    fn addrs(self, port: u16) -> Vec<SocketAddr> {
        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
        match self { Host::Localhost => vec![v4, v6], Host::V4 => vec![v4] }
    }
}

/// A checked preview URL, rebuilt from its parts so the browser reads exactly what was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    pub host: Host,
    pub port: u16,
    /// Path, query and fragment; always starts with `/`.
    pub rest: String,
}

impl Target {
    pub fn scheme(&self) -> &'static str { if self.https { "https" } else { "http" } }
    pub fn origin(&self) -> String { format!("{}://{}:{}", self.scheme(), self.host.as_str(), self.port) }
    pub fn url(&self) -> String { format!("{}{}", self.origin(), self.rest) }
    /// The `frame-src` that holds the frame to this origin: always the exact origin (which is
    /// why no IPv6 literal is ever a Target).
    pub fn frame_src(&self) -> String { self.origin() }
}

/// Parse a loopback dev-server URL. `reserved` are Muster's own ports (the UI and the panel
/// server), refused on every loopback host.
pub fn parse(url: &str, reserved: &[u16]) -> anyhow::Result<Target> {
    ensure!(!url.is_empty(), "web_preview needs a url, e.g. http://localhost:5173");
    ensure!(url.len() <= MAX_URL, "web_preview url is longer than {MAX_URL} characters");
    // Browsers drop tabs and newlines inside URLs and map some Unicode dots to `.`: anything
    // outside printable ASCII could read differently there than here.
    ensure!(url.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "web_preview url contains spaces, control characters or non-ASCII text; percent-encode them");
    ensure!(!url.contains('\\'), "web_preview url must not contain a backslash");
    let Some((scheme, after)) = url.split_once("://") else {
        bail!("web_preview url needs http:// or https:// (e.g. http://localhost:5173). {ELSEWHERE}")
    };
    let https = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        other => bail!("web_preview url must be http or https, not {other}. {ELSEWHERE}"),
    };
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(end);
    ensure!(!authority.contains('@'), "web_preview url must not contain a user name or password. {ELSEWHERE}");
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        // A browser cannot hold a frame to an IPv6 address (CSP sources cannot name one), so
        // the frame could leave the dev server: refuse every IPv6 literal, [::1] included.
        let ip = inner.split_once(']').map_or(inner, |(ip, _)| ip);
        match ip.parse::<Ipv6Addr>() {
            Ok(ip) if ip.is_loopback() || ip.is_unspecified() =>
                bail!("web_preview cannot show an IPv6 address like [{ip}]; use http://localhost:PORT instead, which reaches the same server"),
            _ => bail!("web_preview url host [{ip}] is not this computer. {ELSEWHERE}"),
        }
    } else {
        let (name, port) = match authority.rsplit_once(':') { Some((h, p)) => (h, Some(p)), None => (authority, None) };
        let host = match name.to_ascii_lowercase().as_str() {
            "localhost" => Host::Localhost,
            "127.0.0.1" => Host::V4,
            "" => bail!("web_preview url needs a host, e.g. http://localhost:5173"),
            "0.0.0.0" | "0" => bail!("0.0.0.0 means \"every interface\" to a server; use http://localhost:PORT instead"),
            other => {
                // 127.1, 2130706433, 0x7f.0.0.1 and 0177.0.0.1 all reach 127.0.0.1 in a browser;
                // ask for the plain spelling rather than guess at every variant.
                let numeric = !other.is_empty() && other.split('.').all(|p| !p.is_empty()
                    && (p.bytes().all(|c| c.is_ascii_digit()) || p.starts_with("0x") && p[2..].bytes().all(|c| c.is_ascii_hexdigit())));
                if numeric || other.starts_with("127.") {
                    bail!("write a loopback address as exactly 127.0.0.1 (or localhost), not {other}");
                }
                bail!("{other} is not this computer. {ELSEWHERE}")
            }
        };
        (host, port)
    };
    let port = match port {
        None => if https { 443 } else { 80 },
        Some(p) => {
            ensure!(!p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit()), "web_preview url port must be a number");
            let p: u32 = p.parse()?;
            ensure!((1..=65535).contains(&p), "web_preview url port must be 1–65535");
            p as u16
        }
    };
    ensure!(!reserved.contains(&port),
        "port {port} is Muster's own (its window or its panel server); a preview cannot show Muster itself");
    let rest = match rest.chars().next() { None => "/".to_string(), Some('/') => rest.to_string(), Some(_) => format!("/{rest}") };
    Ok(Target { https, host, port, rest })
}

/// A `web_preview` panel's stored input: `{url}`, rebuilt from its checked parts.
pub fn data(data: Value, reserved: &[u16]) -> anyhow::Result<Vec<u8>> {
    let Value::Object(o) = data else { bail!("web_preview data must be an object, e.g. {{\"url\": \"http://localhost:5173\"}}") };
    for k in o.keys() { ensure!(k == "url", "web_preview takes only `url` (unknown field `{k}`)"); }
    let url = o.get("url").filter(|v| !v.is_null())
        .context("web_preview data needs `url`, e.g. {\"url\": \"http://localhost:5173\"}")?
        .as_str().context("web_preview `url` must be a string")?;
    crate::panels::data_bytes(&json!({"url": parse(url, reserved)?.url()}))
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('\'', "&#39;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `/preview` page for `url`, and its CSP: no scripts, and a frame held to the dev server.
pub fn page(url: &str, ui_port: u16, panel_port: u16) -> anyhow::Result<(String, String)> {
    let t = parse(url, &[ui_port, panel_port])?;
    let csp = format!("default-src 'none'; style-src 'unsafe-inline'; frame-src {}; object-src 'none'; base-uri 'none'; \
        form-action 'none'; frame-ancestors http://127.0.0.1:{ui_port} http://localhost:{ui_port}", t.frame_src());
    Ok((csp, PAGE.replace("__MUSTER_PREVIEW_URL__", &escape_attr(&t.url()))))
}

// ----- the best-effort check behind the panel's notices -----

const CONNECT: Duration = Duration::from_millis(1500);
const ANSWER: Duration = Duration::from_secs(3);
const MAX_HEAD: usize = 64 * 1024;

/// What the dev server says, as far as Muster can tell without a browser: is anything
/// listening, and for http, does the response refuse framing or redirect elsewhere.
/// `ancestors` are the origins that will frame it (the panel server, the UI).
pub async fn check(t: &Target, ancestors: &[String]) -> Value {
    let mut stream = None;
    for addr in t.host.addrs(t.port) {
        if let Ok(Ok(s)) = tokio::time::timeout(CONNECT, tokio::net::TcpStream::connect(addr)).await {
            stream = Some(s);
            break;
        }
    }
    let Some(mut stream) = stream else { return json!({"reachable": false}) };
    // TLS is the browser's business; a connection is all Muster checks for https.
    if t.https { return json!({"reachable": true, "answered": null}) }
    let path = t.rest.split('#').next().unwrap_or("/");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: Muster-preview-check\r\nAccept: text/html,*/*\r\n\
        Sec-Fetch-Dest: iframe\r\nConnection: close\r\n\r\n", t.host.as_str(), t.port);
    let head = tokio::time::timeout(ANSWER, async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(request.as_bytes()).await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_HEAD {
            let n = stream.read(&mut chunk).await?;
            if n == 0 { break }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(buf)
    }).await;
    let Ok(Ok(head)) = head else { return json!({"reachable": true, "answered": false}) };
    let Some((status, headers)) = parse_head(&head) else { return json!({"reachable": true, "answered": false}) };
    let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.as_str());
    let leaves_to = location.filter(|_| (300..400).contains(&status)).and_then(|loc| redirect_away(t, loc));
    json!({"reachable": true, "answered": true, "status": status,
        "refused": frame_refusal(&headers, &t.origin(), ancestors), "leaves_to": leaves_to})
}

/// Status and lower-cased header names of a response head.
fn parse_head(head: &[u8]) -> Option<(u16, Vec<(String, String)>)> {
    let text = String::from_utf8_lossy(head);
    let text = text.split("\r\n\r\n").next()?;
    let mut lines = text.split("\r\n");
    let status = lines.next()?.strip_prefix("HTTP/1.")?.get(2..)?.trim_start().get(..3)?.parse().ok()?;
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    Some((status, headers))
}

/// A redirect's target, when it leaves the preview's origin (the frame cannot follow it).
fn redirect_away(t: &Target, location: &str) -> Option<String> {
    let location = location.trim();
    let absolute = if location.starts_with("//") { format!("{}:{location}", t.scheme()) }
        else if location.contains("://") { location.to_string() }
        else { return None };
    let same = parse(&absolute, &[]).is_ok_and(|to| to.origin() == t.origin());
    (!same).then(|| absolute.chars().take(300).collect())
}

/// Why a browser would refuse to show this response in Muster's frame, if it would.
/// CSP `frame-ancestors` wins over `X-Frame-Options` when present, as in browsers.
pub fn frame_refusal(headers: &[(String, String)], own: &str, ancestors: &[String]) -> Option<String> {
    let mut has_ancestors = false;
    for csp in headers.iter().filter(|(k, _)| k == "content-security-policy").map(|(_, v)| v) {
        for policy in csp.split(',') {
            let Some(directive) = policy.split(';').map(str::trim)
                .find(|d| d.split_ascii_whitespace().next().is_some_and(|n| n.eq_ignore_ascii_case("frame-ancestors"))) else { continue };
            has_ancestors = true;
            let sources: Vec<&str> = directive.split_ascii_whitespace().skip(1).collect();
            if !ancestors.iter().all(|a| sources.iter().any(|s| source_matches(s, a, own))) {
                return Some(format!("Content-Security-Policy {}", directive.chars().take(200).collect::<String>()));
            }
        }
    }
    if has_ancestors { return None }
    for value in headers.iter().filter(|(k, _)| k == "x-frame-options").flat_map(|(_, v)| v.split(',')) {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny" => return Some("X-Frame-Options: DENY".into()),
            "sameorigin" => return Some("X-Frame-Options: SAMEORIGIN".into()),
            _ => {}
        }
    }
    None
}

/// Does a CSP source expression allow `origin` (`scheme://host:port`, port explicit)? A
/// simplified CSP3 match: enough for `'none'`, `'self'`, `*`, schemes and host sources.
fn source_matches(source: &str, origin: &str, own: &str) -> bool {
    let Some((scheme, hostport)) = origin.split_once("://") else { return false };
    let Some((host, port)) = hostport.rsplit_once(':') else { return false };
    let source = source.to_ascii_lowercase();
    match source.as_str() {
        "'self'" => return origin == own,
        "*" => return matches!(scheme, "http" | "https"),
        s if s.starts_with('\'') => return false,
        s if s.ends_with(':') && !s.contains('/') => {
            let want = &s[..s.len() - 1];
            return want == scheme || (want == "http" && scheme == "https");
        }
        _ => {}
    }
    let (src_scheme, rest) = match source.split_once("://") { Some((s, r)) => (Some(s), r), None => (None, source.as_str()) };
    if let Some(s) = src_scheme && s != scheme && !(s == "http" && scheme == "https") { return false }
    if src_scheme.is_none() && !matches!(scheme, "http" | "https") { return false }
    let rest = rest.split('/').next().unwrap_or("");
    let (src_host, src_port) = match rest.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit() || c == '*') => (h, Some(p)),
        _ => (rest, None),
    };
    let host_ok = src_host == "*" || src_host == host
        || src_host.strip_prefix("*.").is_some_and(|suffix| host.ends_with(&format!(".{suffix}")));
    let default = if scheme == "https" { "443" } else { "80" };
    let port_ok = match src_port { Some("*") => true, Some(p) => p == port, None => port == default };
    host_ok && port_ok
}

/// One plain sentence for the agent when the check found a problem (`panel_open`/`panel_update`).
pub fn agent_note(url: &str, check: &Value) -> Option<String> {
    if check["reachable"] == false {
        return Some(format!("Nothing answers at {url} yet. The panel says so and reloads by itself once the \
            server answers (it keeps checking for 2 minutes); start the dev server, or check the port."));
    }
    if let Some(reason) = check["refused"].as_str() {
        return Some(format!("{url} refuses to be shown inside another page ({reason}), so the panel shows a notice \
            instead. Allow framing in the dev server's config, or open the page in the user's browser."));
    }
    if let Some(to) = check["leaves_to"].as_str() {
        return Some(format!("{url} redirects to {to}, which the preview cannot follow (it stays on the dev server's \
            origin). Pass the final URL if it is local, or open it in the user's browser."));
    }
    if check["answered"] == false {
        return Some(format!("The server at {url} accepted the connection but sent no answer within 3 seconds."));
    }
    None
}

/// Check a `web_preview` panel's current URL (the UI's `panel.preview_check`, and the agent's
/// note). Only a stored, checked URL is ever fetched: never one supplied with the request.
pub async fn check_panel(hub: &crate::hub::Hub, id: crate::model::PanelId) -> anyhow::Result<(String, Value)> {
    let res = crate::panels::resource(hub, id)?;
    ensure!(res["panel"]["app"] == "web_preview", "panel is not a web_preview");
    let url = res["data"]["url"].as_str().context("web_preview panel has no url")?.to_string();
    let panel_port = hub.panels.port.load(std::sync::atomic::Ordering::Relaxed);
    let t = parse(&url, &[hub.port(), panel_port])?;
    let ancestors = [format!("http://127.0.0.1:{panel_port}"), format!("http://127.0.0.1:{}", hub.port())];
    Ok((url, check(&t, &ancestors).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESERVED: &[u16] = &[7433, 40001];

    #[test]
    fn loopback_urls_are_accepted_and_rebuilt() {
        for (input, want) in [
            ("http://localhost:5173", "http://localhost:5173/"),
            ("http://LOCALHOST:5173/app?x=1#top", "http://localhost:5173/app?x=1#top"),
            ("HTTP://127.0.0.1:8000/", "http://127.0.0.1:8000/"),
            ("https://localhost:8443/a/b", "https://localhost:8443/a/b"),
            ("http://localhost", "http://localhost:80/"),
            ("https://127.0.0.1", "https://127.0.0.1:443/"),
            ("http://localhost:5173?q=1", "http://localhost:5173/?q=1"),
            ("http://localhost:5173#x", "http://localhost:5173/#x"),
            ("http://localhost:05173/", "http://localhost:5173/"),
            ("http://localhost:5173//evil.example/", "http://localhost:5173//evil.example/"),
        ] {
            assert_eq!(parse(input, RESERVED).unwrap().url(), want, "{input}");
        }
        // IPv6 literals cannot be held to their origin by a CSP: refused, pointing at localhost.
        for v6 in ["http://[::1]:3000/", "http://[0:0:0:0:0:0:0:1]:3000", "https://[::1]:8443/", "http://[::]:3000/"] {
            let err = parse(v6, RESERVED).unwrap_err().to_string();
            assert!(err.contains("http://localhost:PORT"), "{v6}: {err}");
        }
        assert_eq!(parse("http://localhost:5173/x", RESERVED).unwrap().frame_src(), "http://localhost:5173");
    }

    #[test]
    fn tricks_and_other_sites_are_refused() {
        for bad in [
            // Not loopback, or only looks like it.
            "https://example.com/", "http://localhost.evil.com:5173/", "http://127.0.0.1.nip.io:5173/",
            "http://evil.com#@localhost:5173", "http://evil.com?localhost:5173", "http://localhost.:5173/",
            "http://192.168.1.10:5173/", "http://[::ffff:127.0.0.1]:5173/", "http://[fe80::1]:5173/",
            // Credentials: the host is what follows the @.
            "http://localhost:5173@evil.com/", "http://user:pass@localhost:5173/", "http://localhost@evil.com",
            // Other spellings of 127.0.0.1, and "every interface".
            "http://2130706433:5173/", "http://0177.0.0.1:5173/", "http://127.1:5173/", "http://0x7f.0.0.1:5173/",
            "http://127.0.0.2:5173/", "http://0.0.0.0:5173/", "http://[::]:5173/",
            // Hidden characters browsers drop or reinterpret.
            "http://local\thost:5173/", "http://localhost:5173/\n", " http://localhost:5173/", "http://localhost\u{3002}evil.com/",
            "http://localhost:5173\\@evil.com/", "http:\\\\localhost:5173",
            // Schemes and shapes.
            "localhost:5173", "//localhost:5173", "ftp://localhost:21/", "javascript://localhost:5173/%0aalert(1)",
            "file:///etc/passwd", "data:text/html,x", "http:/localhost:5173", "http://:5173/", "http://[::1/",
            "http://localhost:/", "http://localhost:0/", "http://localhost:65536/", "http://localhost:5173:80/",
            "http://localhost:+80/", "",
            // Muster's own window and panel server.
            "http://127.0.0.1:7433/", "http://localhost:7433/#token=x", "http://[::1]:40001/proxy",
        ] {
            assert!(parse(bad, RESERVED).is_err(), "{bad:?} should be refused");
        }
        let err = parse("https://github.com/x/y", RESERVED).unwrap_err().to_string();
        assert!(err.contains("system browser"), "{err}");
        let err = parse("http://localhost:7433/", RESERVED).unwrap_err().to_string();
        assert!(err.contains("Muster's own"), "{err}");
        let err = parse("http://0.0.0.0:8000/", RESERVED).unwrap_err().to_string();
        assert!(err.contains("http://localhost:PORT"), "{err}");
        assert!(parse(&format!("http://localhost:1/{}", "a".repeat(MAX_URL)), RESERVED).is_err());
    }

    #[test]
    fn data_is_exactly_one_url() {
        let stored: Value = serde_json::from_slice(&data(json!({"url": "http://LOCALHOST:5173"}), RESERVED).unwrap()).unwrap();
        assert_eq!(stored, json!({"url": "http://localhost:5173/"}));
        for bad in [json!({}), json!({"url": 5}), json!({"url": "http://localhost:5173", "title": "x"}), json!("http://localhost:5173"),
            json!({"url": "https://example.com"})] {
            assert!(data(bad.clone(), RESERVED).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_preview_page_frames_only_the_dev_origin() {
        let (csp, html) = page("http://localhost:5173/a?b=1&c=\"<x>'", 7433, 40001).unwrap();
        assert!(csp.contains("frame-src http://localhost:5173;") && csp.contains("default-src 'none'"), "{csp}");
        assert!(!csp.contains("script-src"), "the page runs no script: {csp}");
        assert!(csp.contains("frame-ancestors http://127.0.0.1:7433 http://localhost:7433"), "{csp}");
        assert!(html.contains(r#"src="http://localhost:5173/a?b=1&amp;c=&quot;&lt;x&gt;&#39;""#), "{html}");
        assert!(!html.contains("__MUSTER_PREVIEW_URL__"));
        assert!(html.contains("sandbox=\"") && !html.contains("allow-top-navigation"));
        assert!(page("http://127.0.0.1:40001/proxy", 7433, 40001).is_err(), "the panel server itself");
        assert!(page("https://example.com/", 7433, 40001).is_err());
    }

    fn headers(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn framing_refusals_follow_browser_rules() {
        let own = "http://localhost:5173";
        let anc = ["http://127.0.0.1:40001".to_string(), "http://127.0.0.1:7433".to_string()];
        let refused = |h: &[(&str, &str)]| frame_refusal(&headers(h), own, &anc);
        assert_eq!(refused(&[("x-frame-options", "DENY")]).unwrap(), "X-Frame-Options: DENY");
        assert!(refused(&[("x-frame-options", "sameorigin")]).is_some());
        assert!(refused(&[("x-frame-options", "ALLOW-FROM http://127.0.0.1:7433")]).is_none(), "browsers ignore ALLOW-FROM");
        assert!(refused(&[]).is_none());
        for blocks in ["frame-ancestors 'none'", "frame-ancestors 'self'", "default-src 'self'; frame-ancestors https://example.com",
            "frame-ancestors http://127.0.0.1:7433", "frame-ancestors https:", "frame-ancestors http://localhost:*"] {
            assert!(refused(&[("content-security-policy", blocks)]).is_some(), "{blocks}");
        }
        for allows in ["frame-ancestors *", "frame-ancestors http:", "frame-ancestors http://127.0.0.1:*",
            "frame-ancestors 'self' http://127.0.0.1:40001 http://127.0.0.1:7433", "default-src 'self'", "frame-ancestors http://*.0.0.1:* http://127.0.0.1:*"] {
            assert!(refused(&[("content-security-policy", allows)]).is_none(), "{allows}");
        }
        // frame-ancestors overrides X-Frame-Options; report-only policies never block.
        assert!(refused(&[("x-frame-options", "DENY"), ("content-security-policy", "frame-ancestors *")]).is_none());
        assert!(refused(&[("content-security-policy-report-only", "frame-ancestors 'none'")]).is_none());
    }

    #[test]
    fn responses_and_redirects_are_read() {
        let (status, h) = parse_head(b"HTTP/1.1 302 Found\r\nLocation: https://accounts.example/login\r\nX-Frame-Options: deny\r\n\r\nbody").unwrap();
        assert_eq!(status, 302);
        assert!(h.contains(&("location".into(), "https://accounts.example/login".into())));
        assert!(parse_head(b"garbage\r\n\r\n").is_none());
        let t = parse("http://localhost:5173/", RESERVED).unwrap();
        assert_eq!(redirect_away(&t, "https://accounts.example/login").unwrap(), "https://accounts.example/login");
        assert_eq!(redirect_away(&t, "//evil.example/x").unwrap(), "http://evil.example/x");
        assert_eq!(redirect_away(&t, "http://127.0.0.1:5173/").unwrap(), "http://127.0.0.1:5173/", "another origin, even on loopback");
        for same in ["/login", "login", "http://localhost:5173/next", "?a=1"] { assert!(redirect_away(&t, same).is_none(), "{same}"); }
    }

    #[tokio::test]
    async fn the_check_reports_down_refusing_and_redirecting_servers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn serve(answer: &'static str) -> u16 {
            let l = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(answer.as_bytes()).await;
                }
            });
            port
        }
        let anc = ["http://127.0.0.1:1".to_string()];
        let at = |port: u16| parse(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        let ok = serve("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\nhi").await;
        let c = check(&at(ok), &anc).await;
        assert_eq!((c["reachable"].as_bool(), c["status"].as_u64(), c["refused"].is_null()), (Some(true), Some(200), true), "{c}");
        assert!(agent_note("u", &c).is_none());
        let deny = serve("HTTP/1.1 200 OK\r\nX-Frame-Options: DENY\r\n\r\n").await;
        let c = check(&at(deny), &anc).await;
        assert_eq!(c["refused"], "X-Frame-Options: DENY");
        assert!(agent_note("u", &c).unwrap().contains("refuses"));
        let away = serve("HTTP/1.1 302 Found\r\nLocation: https://login.example/\r\n\r\n").await;
        assert_eq!(check(&at(away), &anc).await["leaves_to"], "https://login.example/");
        // A port nothing listens on: bind, note the port, close.
        let closed = { let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap(); l.local_addr().unwrap().port() };
        let c = check(&at(closed), &anc).await;
        assert_eq!(c["reachable"], false);
        assert!(agent_note("http://127.0.0.1/", &c).unwrap().contains("Nothing answers"));
    }
}
