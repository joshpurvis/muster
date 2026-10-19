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
    match app {
        "pr_decision" => check_pr(&data)?,
        "chart" => check_chart(&data)?,
        "table" => check_table(&data)?,
        _ => {}
    }
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

// ----- shape checks: the agent learns of a malformed input from its own tool call -----
// The apps validate everything again (and more) in the panel; these cover what a panel cannot
// recover from, so a mistake fails panel_open/panel_update instead of stranding the user.

fn field<'a>(o: &'a serde_json::Map<String, Value>, app: &str, key: &str) -> anyhow::Result<&'a Value> {
    o.get(key).filter(|v| !v.is_null()).with_context(|| format!("{app} data needs `{key}`"))
}

fn text<'a>(o: &'a serde_json::Map<String, Value>, app: &str, key: &str) -> anyhow::Result<&'a str> {
    let s = field(o, app, key)?.as_str().with_context(|| format!("{app} `{key}` must be a string"))?;
    ensure!(!s.trim().is_empty(), "{app} `{key}` must not be empty");
    Ok(s)
}

fn one_of(o: &serde_json::Map<String, Value>, app: &str, key: &str, options: &[&str], required: bool) -> anyhow::Result<()> {
    match o.get(key).filter(|v| !v.is_null()) {
        None if !required => Ok(()),
        None => bail!("{app} data needs `{key}` (one of {})", options.join(", ")),
        Some(v) => {
            ensure!(v.as_str().is_some_and(|s| options.contains(&s)), "{app} `{key}` must be one of {} (got {v})", options.join(", "));
            Ok(())
        }
    }
}

fn object<'a>(v: &'a Value, what: &str) -> anyhow::Result<&'a serde_json::Map<String, Value>> {
    v.as_object().with_context(|| format!("{what} must be an object"))
}

/// The fields the merge command and its safety checks are built from, exactly as the app
/// requires them (web/src/panels/apps/pr_decision/logic.ts).
pub fn check_pr(data: &Value) -> anyhow::Result<()> {
    let app = "pr_decision";
    let o = object(data, "pr_decision data")?;
    let repo = text(o, app, "repo")?;
    let part = |s: &str| (1..=100).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
    ensure!(repo.split_once('/').is_some_and(|(a, b)| part(a) && part(b)), "pr_decision `repo` must be owner/name (got {repo:?})");
    let number = field(o, app, "number")?;
    ensure!(number.as_u64().is_some_and(|n| n >= 1 && n <= (1 << 53)), "pr_decision `number` must be a positive whole number (got {number})");
    let sha = text(o, app, "head_sha")?;
    ensure!(sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "pr_decision `head_sha` must be the full 40-character lowercase commit SHA (headRefOid)");
    for key in ["base", "head"] {
        let b = text(o, app, key)?;
        ensure!(b.len() <= 255 && !b.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)),
            "pr_decision `{key}` is not a valid branch name");
    }
    let url = text(o, app, "url")?;
    let uri: axum::http::Uri = url.parse().context("pr_decision `url` is not a URL")?;
    ensure!(uri.scheme_str() == Some("https") && uri.authority().is_some_and(|a| !a.as_str().contains('@')), "pr_decision `url` must be an https URL");
    for key in ["title", "author", "summary"] { text(o, app, key)?; }
    one_of(object(field(o, app, "risk")?, "pr_decision `risk`")?, "pr_decision risk", "level", &["low", "medium", "high"], true)?;
    let checks = object(field(o, app, "checks")?, "pr_decision `checks`")?;
    one_of(checks, "pr_decision checks", "state", &["passing", "failing", "pending", "none"], true)?;
    if let Some(items) = checks.get("items").filter(|v| !v.is_null()) {
        for item in items.as_array().context("pr_decision `checks.items` must be an array")? {
            one_of(object(item, "each check")?, "pr_decision check item", "state", &["pass", "fail", "pending", "skipping", "cancel"], true)?;
        }
    }
    let changes = field(o, app, "changes")?.as_array().context("pr_decision `changes` must be an array")?;
    for c in changes {
        let c = object(c, "each change")?;
        for key in ["file", "note", "diff"] { ensure!(c.get(key).is_some_and(Value::is_string), "each pr_decision change needs a string `{key}`"); }
    }
    one_of(o, app, "merge_method_default", &["squash", "merge", "rebase"], false)?;
    one_of(o, app, "status", &["open", "merging", "queued", "merged", "failed", "requested_changes"], false)?;
    Ok(())
}

pub fn check_chart(data: &Value) -> anyhow::Result<()> {
    let app = "chart";
    let o = object(data, "chart data")?;
    one_of(o, app, "type", &["bar", "line", "scatter", "area"], true)?;
    let x = field(o, app, "x")?.get("values").and_then(Value::as_array).context("chart data needs `x.values` (an array)")?;
    ensure!((1..=2000).contains(&x.len()), "chart `x.values` needs 1 to 2000 values");
    let series = field(o, app, "series")?.as_array().context("chart `series` must be an array")?;
    ensure!((1..=12).contains(&series.len()), "chart `series` needs 1 to 12 series");
    for (i, s) in series.iter().enumerate() {
        let s = object(s, "each chart series")?;
        ensure!(s.get("name").is_some_and(Value::is_string), "chart series {i} needs a string `name`");
        let values = s.get("values").and_then(Value::as_array).with_context(|| format!("chart series {i} needs `values` (an array)"))?;
        ensure!(values.len() == x.len(), "chart series {i} has {} values but x.values has {}", values.len(), x.len());
        ensure!(values.iter().all(|v| v.is_null() || v.is_number()), "chart series {i} values must be numbers or null");
    }
    Ok(())
}

pub fn check_table(data: &Value) -> anyhow::Result<()> {
    let o = object(data, "table data")?;
    let columns = field(o, "table", "columns")?.as_array().context("table `columns` must be an array")?;
    ensure!((1..=50).contains(&columns.len()), "table `columns` needs 1 to 50 columns");
    for c in columns {
        ensure!(c.get("key").is_some_and(Value::is_string), "each table column needs a string `key`");
    }
    let rows = field(o, "table", "rows")?.as_array().context("table `rows` must be an array")?;
    ensure!(rows.len() <= 5000, "table `rows` has more than 5000 rows");
    ensure!(rows.iter().all(Value::is_object), "each table row must be an object");
    Ok(())
}

/// `https://host[:port]` for a credential-free https URL, as a CSP source.
fn https_origin(url: &str) -> anyhow::Result<String> {
    ensure!(!url.chars().any(|c| c.is_control() || c.is_whitespace()), "image url contains spaces or control characters");
    let uri: axum::http::Uri = url.parse().context("image url is not a valid URL")?;
    ensure!(uri.scheme_str() == Some("https"), "image url must be https");
    let authority = uri.authority().context("image url needs a host")?;
    ensure!(!authority.as_str().contains('@'), "image url must not contain a user name or password");
    let host = authority.host();
    // A plain host name or IP literal only: no CSP wildcards or other punctuation.
    ensure!(!host.is_empty() && host.bytes().all(|c| c.is_ascii_alphanumeric() || b".-[]:".contains(&c)) && !host.starts_with('.'),
        "image url needs a plain host name");
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

    fn pr_sample() -> Value {
        json!({"repo":"acme/api","number":812,"title":"T","author":"dana","url":"https://github.com/acme/api/pull/812",
            "base":"master","head":"dana/x","head_sha":"0123456789abcdef0123456789abcdef01234567","summary":"S",
            "risk":{"level":"low"},"checks":{"state":"passing","items":[{"name":"t","state":"pass"}]},
            "changes":[{"file":"a","note":"n","diff":"+x"}]})
    }

    fn chart_sample() -> Value {
        json!({"type":"bar","x":{"values":["a","b"]},"series":[{"name":"s","values":[1,null]}]})
    }

    #[test]
    fn built_in_inputs_are_checked_before_the_panel_opens() {
        check_pr(&pr_sample()).unwrap();
        let with = |k: &str, v: Value| { let mut d = pr_sample(); d[k] = v; d };
        for (data, why) in [
            (with("repo", json!("acme")), "owner/name"), (with("repo", json!("acme/api; rm")), "owner/name"),
            (with("number", json!(0)), "positive"), (with("number", json!("812")), "positive"), (with("number", json!(8.5)), "positive"),
            (with("head_sha", json!("0123456")), "40-character"), (with("head_sha", json!("0123456789ABCDEF0123456789abcdef01234567")), "40-character"),
            (with("head_sha", json!(format!("{}{}", "0".repeat(40), "0".repeat(24)))), "40-character"),
            (with("base", json!("ma ster")), "branch"), (with("base", Value::Null), "needs `base`"), (with("head", json!("")), "empty"),
            (with("url", json!("http://github.com/x")), "https"), (with("url", json!("https://u@github.com/x")), "https"),
            (with("risk", json!({"level":"extreme"})), "risk"), (with("checks", json!({"state":"success"})), "one of passing"),
            (with("checks", json!({"state":"passing","items":[{"name":"t","state":"SUCCESS"}]})), "check item"),
            (with("changes", json!([{"file":"a","note":"n"}])), "diff"), (with("status", json!("done")), "queued"),
            (with("merge_method_default", json!("ff")), "squash"), (json!([1]), "object"),
        ] {
            let e = format!("{:#}", check_pr(&data).unwrap_err());
            assert!(e.contains(why), "{data}: {e}");
        }
        check_chart(&chart_sample()).unwrap();
        let mut c = chart_sample();
        c["series"][0]["values"] = json!([1]);
        assert!(check_chart(&c).unwrap_err().to_string().contains("has 1 values but x.values has 2"));
        assert!(check_chart(&json!({"type":"pie","x":{"values":[1]},"series":[]})).unwrap_err().to_string().contains("one of bar"));
        assert!(check_chart(&json!({"type":"bar","x":{"values":[1]},"series":(0..13).map(|i| json!({"name":i.to_string(),"values":[1]})).collect::<Vec<_>>()})).is_err());
        check_table(&json!({"columns":[{"key":"a"}],"rows":[{"a":1}]})).unwrap();
        assert!(check_table(&json!({"columns":[],"rows":[]})).is_err());
        assert!(check_table(&json!({"columns":[{"label":"x"}],"rows":[]})).unwrap_err().to_string().contains("key"));
        assert!(check_table(&json!({"columns":[{"key":"a"}],"rows":[1]})).unwrap_err().to_string().contains("object"));
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
            (json!({"path":7}), "must be a string"), (json!({"url":"https://x.example/a b"}), "spaces"), (json!({"url":"https://*.x.example/a.png"}), "plain host"), (json!([1]), "object")] {
            let e = format!("{:#}", data("image", &cwd, bad.clone(), &mut PanelCsp::default(), true).unwrap_err());
            assert!(e.contains(why), "{bad}: {e}");
        }
        // Other apps keep the general 1 MiB input limit and take data as given.
        let big = data("table", &cwd, json!({"columns":[{"key":"a"}],"rows":[{"a": "y".repeat(crate::panels::MAX_FILE)}]}), &mut csp, true);
        assert!(big.unwrap_err().to_string().contains("1 MiB"));
        assert_eq!(data("table", &cwd, json!({"columns":[{"key":"a"}],"rows":[]}), &mut csp, true).unwrap(), br#"{"columns":[{"key":"a"}],"rows":[]}"#);
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
        for (app, data) in [("chart", chart_sample()), ("table", json!({"columns":[{"key":"a"}],"rows":[]})), ("pr_decision", pr_sample())] {
            let id = open(app, data.clone()).unwrap();
            assert_eq!(crate::panels::resource(&hub, id).unwrap()["html"], html(app).unwrap());
            // A malformed input fails the agent's own call, open or update.
            let e = open(app, json!({})).unwrap_err().to_string();
            assert!(e.contains(&format!("{app} data needs")), "{e}");
            assert!(update(id, json!({"x": 1})).is_err());
        }
        assert!(open("diff", json!({})).unwrap_err().to_string().contains("pr_decision"));
        assert!(crate::panels::open(&hub, 1, Open { title: "x".into(), app: "chart".into(), data: None,
            html: Some("<p>".into()), html_path: None, csp: None }).unwrap_err().to_string().contains("does not accept custom HTML"));
        crate::panels::archive(&hub, 1).unwrap();
        std::fs::remove_file(Path::new("/tmp").join(&rel)).unwrap();
    }
}
