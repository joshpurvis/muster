//! Muster's built-in panel apps (docs/MCP.md "Built-in apps"). Their HTML is generated from
//! web/src/panels/apps/ by `deno task panels` and committed; this module picks an app's HTML and
//! prepares the input Muster supplies itself, which today is an image file from the worktree.

use std::path::Path;

use anyhow::{Context, bail, ensure};
use base64::Engine;
use serde_json::{Value, json};

use crate::model::PanelCsp;

pub const IMAGE: &str = include_str!("../web/panels/image.html");
pub const CHART: &str = include_str!("../web/panels/chart.html");
pub const TABLE: &str = include_str!("../web/panels/table.html");
pub const PR_DECISION: &str = include_str!("../web/panels/pr_decision.html");

/// For error messages and tool descriptions.
pub const BUILTINS: &str = "markdown, image, chart, table, pr_decision";
/// Largest image file an `image` panel reads.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
/// Its stored input: the base64 data URI plus the agent's own short fields.
pub const MAX_IMAGE_DATA: usize = MAX_IMAGE.div_ceil(3) * 4 + 64 * 1024;

pub fn html(app: &str) -> Option<&'static str> {
    Some(match app {
        "markdown" => crate::panels::MARKDOWN,
        "image" => IMAGE,
        "chart" => CHART,
        "table" => TABLE,
        "pr_decision" => PR_DECISION,
        _ => return None,
    })
}

/// The stored input for `app`. For `image`, reads `path` from `cwd` into a data URI, or adds an
/// https `url`'s origin to the panel's resourceDomains (`opening`) or requires it there (update).
pub fn data(app: &str, cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool) -> anyhow::Result<Vec<u8>> {
    if app != "image" { return crate::panels::data_bytes(&data); }
    let data = serde_json::to_vec(&image(cwd, data, csp, opening)?)?;
    ensure!(data.len() <= MAX_IMAGE_DATA, "image panel data is too large");
    Ok(data)
}

fn image(cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool) -> anyhow::Result<Value> {
    let Value::Object(mut o) = data else { bail!("panel data must be an object") };
    for (k, v) in &o {
        let max = match k.as_str() { "path" | "url" => 4096, "alt" => 1000, "caption" => 2000,
            _ => bail!("image data takes path or url, plus alt and caption (unknown field `{k}`)") };
        ensure!(v.as_str().is_some_and(|s| s.chars().count() <= max), "image {k} must be a string of at most {max} characters");
    }
    match (o.get("path").and_then(Value::as_str), o.get("url").and_then(Value::as_str)) {
        (Some(_), Some(_)) => bail!("give image path or url, not both"),
        (Some(path), None) => {
            let (mime, bytes) = read_image(cwd, path)?;
            let src = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
            o.insert("src".into(), json!(src));
            o.insert("mime".into(), json!(mime));
            o.insert("bytes".into(), json!(bytes.len()));
        }
        (None, Some(url)) => {
            let origin = https_origin(url)?;
            if !csp.resource_domains.contains(&origin) {
                ensure!(opening, "the image URL's origin {origin} is not in this panel's resourceDomains; open a new image panel for it");
                csp.resource_domains.push(origin);
            }
        }
        (None, None) => bail!("image data needs path (a file in the task worktree) or url (https)"),
    }
    Ok(Value::Object(o))
}

/// `https://host[:port]` for a credential-free https URL, as a CSP source.
fn https_origin(url: &str) -> anyhow::Result<String> {
    ensure!(!url.chars().any(|c| c.is_control() || c.is_whitespace()), "image url contains spaces or control characters");
    let uri: axum::http::Uri = url.parse().context("image url is not a valid URL")?;
    ensure!(uri.scheme_str() == Some("https"), "image url must be https");
    let authority = uri.authority().context("image url needs a host")?;
    ensure!(!authority.as_str().contains('@'), "image url must not contain a user name or password");
    ensure!(!authority.host().is_empty(), "image url needs a host");
    Ok(format!("https://{}", authority.as_str().to_ascii_lowercase()))
}

/// Reads an image file inside `cwd`: no escape through `..` or symlinks, regular files only,
/// at most [`MAX_IMAGE`], and only PNG, JPEG, GIF, WebP or SVG (by content; SVG by extension
/// too). SVG is later shown through `<img>`, where its scripts never run.
pub fn read_image(cwd: &Path, path: &str) -> anyhow::Result<(&'static str, Vec<u8>)> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(!path.is_empty() && !path.contains('\0'), "invalid image path");
    let root = cwd.canonicalize().context("resolving the task worktree")?;
    let candidate = root.join(path).canonicalize().with_context(|| format!("image not found: {path}"))?;
    ensure!(candidate.starts_with(&root), "image path must stay inside the task worktree");
    // O_NONBLOCK: opening a FIFO must not hang the daemon; regular files ignore it.
    let file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&candidate)
        .with_context(|| format!("opening image {path}"))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "image path must name a regular file");
    ensure!(meta.len() <= MAX_IMAGE as u64, "image is larger than 8 MiB");
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        ensure!(actual.starts_with(&root), "image path escaped the task worktree");
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_IMAGE, "image is larger than 8 MiB");
    let mime = sniff(&bytes, &candidate).context("unsupported image type: use PNG, JPEG, GIF, WebP or SVG")?;
    Ok((mime, bytes))
}

fn sniff(b: &[u8], path: &Path) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") { return Some("image/png") }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) { return Some("image/jpeg") }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { return Some("image/gif") }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" { return Some("image/webp") }
    let svg = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    if svg && std::str::from_utf8(b).is_ok_and(|t| t.contains("<svg")) { return Some("image/svg+xml") }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("muster-panel-apps-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("task");
        std::fs::create_dir_all(cwd.join("shots")).unwrap();
        (root, cwd)
    }

    #[test]
    fn builtins_are_embedded() {
        for app in ["markdown", "image", "chart", "table", "pr_decision"] {
            let html = html(app).unwrap();
            assert!(html.contains("ui/initialize") && html.contains("2026-01-26"), "{app}");
        }
        assert!(html("custom").is_none() && html("diff").is_none());
        // Generated apps are self-contained: no external scripts, styles or fetches.
        for html in [IMAGE, CHART, TABLE, PR_DECISION] {
            assert!(!html.contains("<script src") && !html.contains("<link") && !html.contains("fetch("));
        }
    }

    #[test]
    fn image_paths_are_read_safely() {
        let (root, cwd) = scratch("paths");
        std::fs::write(cwd.join("shots/a.png"), PNG).unwrap();
        std::fs::write(cwd.join("shots/b.svg"), "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        std::fs::write(cwd.join("shots/fake.svg"), "<html><script>alert(1)</script>").unwrap();
        std::fs::write(cwd.join("notes.txt"), "hello").unwrap();
        std::fs::write(root.join("outside.png"), PNG).unwrap();
        std::os::unix::fs::symlink(root.join("outside.png"), cwd.join("escape.png")).unwrap();
        std::os::unix::fs::symlink(&root, cwd.join("up")).unwrap();
        std::os::unix::fs::symlink(cwd.join("shots/a.png"), cwd.join("inside.png")).unwrap();
        std::fs::write(cwd.join("big.png"), vec![0u8; MAX_IMAGE + 1]).unwrap();
        assert_eq!(read_image(&cwd, "shots/a.png").unwrap(), ("image/png", PNG.to_vec()));
        assert_eq!(read_image(&cwd, "inside.png").unwrap().0, "image/png");
        assert_eq!(read_image(&cwd, "shots/b.svg").unwrap().0, "image/svg+xml");
        let fifo = std::ffi::CString::new(cwd.join("pipe.png").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for (path, why) in [("../outside.png", "inside"), ("escape.png", "inside"), ("up/outside.png", "inside"),
            ("/etc/passwd", "inside"), ("shots", "regular"), ("pipe.png", "regular"), ("big.png", "8 MiB"),
            ("notes.txt", "unsupported"), ("shots/fake.svg", "unsupported"), ("missing.png", "not found"), ("", "invalid")] {
            let e = format!("{:#}", read_image(&cwd, path).unwrap_err());
            assert!(e.contains(why), "{path}: {e}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_data_becomes_a_data_uri_or_a_declared_origin() {
        let (root, cwd) = scratch("data");
        std::fs::write(cwd.join("shots/a.png"), PNG).unwrap();
        let mut csp = PanelCsp::default();
        let stored: Value = serde_json::from_slice(&data("image", &cwd, json!({"path":"shots/a.png","alt":"A"}), &mut csp, true).unwrap()).unwrap();
        assert_eq!(stored["src"], format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(PNG)));
        assert_eq!((stored["mime"].as_str(), stored["bytes"].as_u64(), stored["alt"].as_str()), (Some("image/png"), Some(PNG.len() as u64), Some("A")));
        assert!(csp.resource_domains.is_empty());

        data("image", &cwd, json!({"url":"https://Img.Example:8443/a.png?x=1"}), &mut csp, true).unwrap();
        assert_eq!(csp.resource_domains, vec!["https://img.example:8443"]);
        crate::panels::validate_csp(&csp).unwrap();
        data("image", &cwd, json!({"url":"https://img.example:8443/b.png"}), &mut csp, false).unwrap();
        let e = data("image", &cwd, json!({"url":"https://other.example/b.png"}), &mut csp, false).unwrap_err().to_string();
        assert!(e.contains("open a new image panel"), "{e}");

        for (bad, why) in [(json!({}), "needs path"), (json!({"path":"shots/a.png","url":"https://x.example/a"}), "not both"),
            (json!({"url":"http://x.example/a.png"}), "https"), (json!({"url":"https://u:p@x.example/a.png"}), "user name"),
            (json!({"url":"javascript:alert(1)"}), "https"), (json!({"path":"shots/a.png","src":"data:image/png;base64,AA"}), "unknown field `src`"),
            (json!({"path":7}), "must be a string"), (json!({"url":"https://x.example/a b"}), "spaces"), (json!([1]), "object")] {
            let e = format!("{:#}", data("image", &cwd, bad.clone(), &mut PanelCsp::default(), true).unwrap_err());
            assert!(e.contains(why), "{bad}: {e}");
        }
        // Other apps keep the general 1 MiB input limit and take data as given.
        assert!(data("chart", &cwd, json!({"x": "y".repeat(crate::panels::MAX_FILE)}), &mut csp, true).is_err());
        assert_eq!(data("table", &cwd, json!({"rows":[]}), &mut csp, true).unwrap(), br#"{"rows":[]}"#);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_panels_open_and_update_through_the_panel_core() {
        use crate::panels::{Open, Update};
        let store = crate::store::Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        let mut channel = crate::mcp::guard::test_channel(1);
        channel.window_id = Some(1);
        store.insert_channel(&channel).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.channel_id = 1;
        info.window_id = Some(1);
        store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
        let hub = crate::hub::Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None }, store).unwrap();
        // The test channel's cwd is /tmp.
        let rel = format!("muster-panel-apps-core-{}.png", std::process::id());
        std::fs::write(Path::new("/tmp").join(&rel), PNG).unwrap();
        let open = |app: &str, data: Value| crate::panels::open(&hub, 1, Open { title: "Shot".into(), app: app.into(),
            data: Some(data), html: None, html_path: None, csp: None });
        let id = open("image", json!({"path": rel})).unwrap();
        let res = crate::panels::resource(&hub, id).unwrap();
        assert!(res["data"]["src"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(res["html"], IMAGE);
        let web = open("image", json!({"url": "https://img.example/a.png"})).unwrap();
        assert_eq!(crate::panels::get(&hub, web).unwrap().csp.resource_domains, vec!["https://img.example"]);
        let update = |id, data| crate::panels::update(&hub, 1, Update { panel_id: id, title: None, data: Some(data), html: None, html_path: None });
        update(web, json!({"url": "https://img.example/b.png", "caption": "B"})).unwrap();
        assert!(update(web, json!({"url": "https://evil.example/b.png"})).is_err());
        assert!(update(id, json!({"path": "../etc/passwd"})).is_err());
        for app in ["chart", "table", "pr_decision"] {
            let id = open(app, json!({})).unwrap();
            assert_eq!(crate::panels::resource(&hub, id).unwrap()["html"], html(app).unwrap());
        }
        assert!(open("diff", json!({})).unwrap_err().to_string().contains("pr_decision"));
        assert!(crate::panels::open(&hub, 1, Open { title: "x".into(), app: "chart".into(), data: None,
            html: Some("<p>".into()), html_path: None, csp: None }).unwrap_err().to_string().contains("does not accept custom HTML"));
        crate::panels::archive(&hub, 1).unwrap();
        std::fs::remove_file(Path::new("/tmp").join(&rel)).unwrap();
    }
}
