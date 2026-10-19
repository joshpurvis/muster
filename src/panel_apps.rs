//! Muster's built-in panel apps (docs/MCP.md "Built-in apps"). Their HTML is generated from
//! web/src/panels/apps/ by `deno task panels` and committed; this module picks an app's HTML,
//! checks the input's shape, and prepares what Muster supplies itself: image files from the
//! worktree (the `image` app, and pictures in a `pr_decision` panel's `media`).

use std::path::Path;

use anyhow::{Context, bail, ensure};
use base64::Engine;
use serde_json::{Value, json};

use crate::model::PanelCsp;

pub const IMAGE: &str = include_str!("../web/panels/image.html");
pub const CHART: &str = include_str!("../web/panels/chart.html");
pub const TABLE: &str = include_str!("../web/panels/table.html");
pub const PR_DECISION: &str = include_str!("../web/panels/pr_decision.html");
pub const PR_OVERVIEW: &str = include_str!("../web/panels/pr_overview.html");

/// For error messages and tool descriptions.
pub const BUILTINS: &str = "markdown, image, chart, table, pr_decision, pr_overview";
/// Largest image file an `image` panel reads.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
/// Its stored input: the base64 data URI plus the agent's own short fields.
pub const MAX_IMAGE_DATA: usize = MAX_IMAGE.div_ceil(3) * 4 + 64 * 1024;
/// `pr_decision` media: at most this many pictures and charts, and this many image bytes in all.
pub const MAX_MEDIA: usize = 8;
pub const MAX_MEDIA_BYTES: usize = 6 * 1024 * 1024;
/// A `pr_decision` panel's stored input: the agent's own JSON (at most 1 MiB) plus its images
/// as data URIs.
pub const MAX_PR_DATA: usize = crate::panels::MAX_FILE + MAX_MEDIA_BYTES.div_ceil(3) * 4 + 64 * 1024;
/// The user's own actions (`actions`) in the PR apps.
pub const MAX_ACTIONS: usize = 8;
/// Single keys the PR apps use themselves (pr_decision: M R A O S H; pr_overview: J K O H N).
/// Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain letter or digit
/// never clashes with them.
pub const RESERVED_KEYS: &str = "mraoshjkn";
/// Built-in event names, which a custom action id may not reuse.
const BUILTIN_IDS: &[&str] = &["merge", "request_changes", "ask", "open", "skip", "hold", "next", "custom", "closed", "overflow"];

pub fn html(app: &str) -> Option<&'static str> {
    Some(match app {
        "markdown" => crate::panels::MARKDOWN,
        "image" => IMAGE,
        "chart" => CHART,
        "table" => TABLE,
        "pr_decision" => PR_DECISION,
        "pr_overview" => PR_OVERVIEW,
        _ => return None,
    })
}

/// The stored input for `app`. For `image` (and each picture in `pr_decision` media), reads
/// `path` from `cwd` into a data URI, or adds an https `url`'s origin to the panel's
/// resourceDomains (`opening`) or requires it there (update).
pub fn data(app: &str, cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool) -> anyhow::Result<Vec<u8>> {
    match app {
        "pr_decision" => {
            check_pr(&data)?;
            // The agent's own JSON keeps the general limit; only the images it names add to it.
            crate::panels::data_bytes(&data)?;
            let data = serde_json::to_vec(&media(cwd, data, csp, opening)?)?;
            ensure!(data.len() <= MAX_PR_DATA, "pr_decision panel data is too large");
            return Ok(data);
        }
        "pr_overview" => check_overview(&data)?,
        "chart" => check_chart(&data)?,
        "table" => check_table(&data)?,
        _ => {}
    }
    if app != "image" { return crate::panels::data_bytes(&data); }
    let data = serde_json::to_vec(&image(cwd, data, csp, opening, "image")?)?;
    ensure!(data.len() <= MAX_IMAGE_DATA, "image panel data is too large");
    Ok(data)
}

/// `what` names the input in errors: "image", or "pr_decision `media[2]`".
fn image(cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool, what: &str) -> anyhow::Result<Value> {
    let Value::Object(mut o) = data else { bail!("{what} must be an object") };
    for (k, v) in &o {
        let max = match k.as_str() { "path" | "url" => 4096, "alt" => 1000, "caption" => 2000,
            _ => bail!("{what} takes path or url, plus alt and caption (unknown field `{k}`)") };
        ensure!(v.as_str().is_some_and(|s| s.chars().count() <= max), "{what} {k} must be a string of at most {max} characters");
    }
    match (o.get("path").and_then(Value::as_str), o.get("url").and_then(Value::as_str)) {
        (Some(_), Some(_)) => bail!("give {what} path or url, not both"),
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
                ensure!(opening, "the image URL's origin {origin} is not in this panel's resourceDomains; open a new {} panel for it, or pass the image as a path",
                    if what == "image" { "image" } else { "pr_decision" });
                csp.resource_domains.push(origin);
            }
        }
        (None, None) => bail!("{what} needs path (a file in the task worktree) or url (https)"),
    }
    Ok(Value::Object(o))
}

/// `pr_decision` `media`: pictures are read like the image app's input (into `src`), charts are
/// checked like a chart panel's spec; at most [`MAX_MEDIA`] items and [`MAX_MEDIA_BYTES`] of
/// image files in all.
fn media(cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool) -> anyhow::Result<Value> {
    let Value::Object(mut o) = data else { bail!("pr_decision data must be an object") };
    let Some(list) = o.remove("media").filter(|v| !v.is_null()) else { return Ok(Value::Object(o)) };
    let Value::Array(items) = list else { bail!("pr_decision `media` must be an array") };
    ensure!(items.len() <= MAX_MEDIA, "pr_decision `media` has {} items; at most {MAX_MEDIA}", items.len());
    let mut total = 0usize;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let what = format!("pr_decision `media[{i}]`");
        let Value::Object(mut m) = item else { bail!("{what} must be an object") };
        let kind = m.remove("kind");
        match kind.as_ref().and_then(Value::as_str) {
            Some("image") => {
                let mut stored = image(cwd, Value::Object(m), csp, opening, &what)?;
                total += stored.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
                ensure!(total <= MAX_MEDIA_BYTES, "pr_decision `media` images total more than 6 MiB; use fewer or smaller pictures");
                stored["kind"] = json!("image");
                out.push(stored);
            }
            Some("chart") => {
                for k in m.keys() { ensure!(k == "spec" || k == "caption", "{what} is a chart: it takes spec and caption (unknown field `{k}`)"); }
                let spec = m.get("spec").filter(|v| !v.is_null()).with_context(|| format!("{what} needs `spec` (a chart app spec)"))?;
                check_chart(spec).with_context(|| format!("{what}.spec"))?;
                ensure!(m.get("caption").is_none_or(|c| c.as_str().is_some_and(|s| s.chars().count() <= 2000)), "{what} caption must be a string of at most 2000 characters");
                m.insert("kind".into(), json!("chart"));
                out.push(Value::Object(m));
            }
            _ => bail!("{what} needs `kind`: image or chart"),
        }
    }
    o.insert("media".into(), Value::Array(out));
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
    ensure!(repo_ok(repo), "pr_decision `repo` must be owner/name (got {repo:?})");
    let number = field(o, app, "number")?;
    ensure!(number.as_u64().is_some_and(|n| n >= 1 && n <= (1 << 53)), "pr_decision `number` must be a positive whole number (got {number})");
    let sha = text(o, app, "head_sha")?;
    ensure!(sha_ok(sha),
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
    check_actions(o, app)?;
    Ok(())
}

fn repo_ok(repo: &str) -> bool {
    let part = |s: &str| (1..=100).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
    repo.split_once('/').is_some_and(|(a, b)| part(a) && part(b))
}

fn sha_ok(sha: &str) -> bool { sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) }

/// The user's own actions (`actions`, both PR apps): buttons that send a `custom` event. Ids and
/// keys must be unique, keys one letter or digit not used by the apps, and every action must
/// say what the agent will do (`description`); `outward` ones ask the user to confirm first.
pub fn check_actions(o: &serde_json::Map<String, Value>, app: &str) -> anyhow::Result<()> {
    let Some(list) = o.get("actions").filter(|v| !v.is_null()) else { return Ok(()) };
    let list = list.as_array().with_context(|| format!("{app} `actions` must be an array"))?;
    ensure!(list.len() <= MAX_ACTIONS, "{app} `actions` has {} actions; at most {MAX_ACTIONS}", list.len());
    let (mut ids, mut keys) = (Vec::new(), Vec::new());
    for (i, a) in list.iter().enumerate() {
        let at = format!("{app} `actions[{i}]`");
        let a = object(a, &at)?;
        let id = a.get("id").and_then(Value::as_str).with_context(|| format!("{at} needs a string `id`"))?;
        ensure!((1..=40).contains(&id.len()) && id.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'),
            "{at}.id must be lowercase letters, digits, - or _ (at most 40; got {id:?})");
        ensure!(!BUILTIN_IDS.contains(&id), "{at}.id {id:?} is a built-in action");
        ensure!(!ids.contains(&id), "{at}.id {id:?} is used twice");
        ids.push(id);
        for (key, max) in [("label", 40), ("description", 300)] {
            let s = a.get(key).and_then(Value::as_str).with_context(|| format!("{at} needs a string `{key}`"))?;
            ensure!(!s.trim().is_empty() && s.chars().count() <= max, "{at}.{key} must be 1 to {max} characters");
        }
        if let Some(k) = a.get("key").filter(|v| !v.is_null()) {
            let k = k.as_str().filter(|k| k.len() == 1 && k.bytes().all(|c| c.is_ascii_alphanumeric()))
                .with_context(|| format!("{at}.key must be one letter or digit"))?.to_ascii_lowercase();
            ensure!(!RESERVED_KEYS.contains(&k), "{at}.key {k:?} is a built-in key (m r a o s h j k n are taken)");
            ensure!(!keys.contains(&k), "{at}.key {k:?} is used twice");
            keys.push(k);
        }
        ensure!(a.get("outward").is_none_or(|v| v.is_null() || v.is_boolean()), "{at}.outward must be true or false");
    }
    Ok(())
}

/// `pr_overview`: the repo and one card per open PR, as the app requires them
/// (web/src/panels/apps/pr_overview/logic.ts).
pub fn check_overview(data: &Value) -> anyhow::Result<()> {
    let app = "pr_overview";
    let o = object(data, "pr_overview data")?;
    let repo = text(o, app, "repo")?;
    ensure!(repo_ok(repo), "pr_overview `repo` must be owner/name (got {repo:?})");
    let prs = field(o, app, "prs")?.as_array().context("pr_overview `prs` must be an array")?;
    ensure!(prs.len() <= 200, "pr_overview `prs` has {} PRs; at most 200", prs.len());
    let mut numbers = Vec::with_capacity(prs.len());
    for (i, pr) in prs.iter().enumerate() {
        let at = format!("pr_overview `prs[{i}]`");
        let p = object(pr, &at)?;
        let number = p.get("number").and_then(Value::as_u64).filter(|n| *n >= 1 && *n <= 1 << 53)
            .with_context(|| format!("{at}.number must be a positive whole number"))?;
        ensure!(!numbers.contains(&number), "{at}.number #{number} is listed twice");
        numbers.push(number);
        for key in ["title", "author"] {
            ensure!(p.get(key).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()), "{at} needs a non-empty string `{key}`");
        }
        let item = format!("pr_overview prs[{i}]");
        one_of(p, &item, "checks", &["passing", "failing", "pending", "none"], false)?;
        one_of(p, &item, "risk", &["low", "medium", "high"], false)?;
        one_of(p, &item, "status", &["new", "reviewing", "merged", "queued", "held", "skipped", "changes_requested", "failed"], false)?;
        if p.get("review").and_then(Value::as_str) != Some("") {
            one_of(p, &item, "review", &["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", "NONE"], false)?;
        }
        for key in ["additions", "deletions", "files"] {
            ensure!(p.get(key).is_none_or(|v| v.is_null() || v.is_u64()), "{at}.{key} must be a whole number, 0 or more");
        }
        ensure!(p.get("draft").is_none_or(|v| v.is_null() || v.is_boolean()), "{at}.draft must be true or false");
        if let Some(sha) = p.get("head_sha").filter(|v| !v.is_null()) {
            ensure!(sha.as_str().is_some_and(sha_ok), "{at}.head_sha must be the full 40-character lowercase commit SHA");
        }
        if let Some(url) = p.get("url").filter(|v| !v.is_null()) {
            let ok = url.as_str().and_then(|u| u.parse::<axum::http::Uri>().ok())
                .is_some_and(|u| u.scheme_str() == Some("https") && u.authority().is_some_and(|a| !a.as_str().contains('@')));
            ensure!(ok, "{at}.url must be an https URL");
        }
        if let Some(labels) = p.get("labels").filter(|v| !v.is_null()) {
            let labels = labels.as_array().with_context(|| format!("{at}.labels must be an array of strings"))?;
            ensure!(labels.len() <= 20 && labels.iter().all(Value::is_string), "{at}.labels must be at most 20 strings");
        }
    }
    if let Some(current) = o.get("current").filter(|v| !v.is_null()) {
        ensure!(current.as_u64().is_some_and(|n| numbers.contains(&n)), "pr_overview `current` must be the number of a PR in `prs`");
    }
    one_of(o, app, "sort", &["risk", "quick", "oldest"], false)?;
    check_actions(o, app)?;
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

    fn overview_sample() -> Value {
        json!({"repo":"acme/api","current":812,"sort":"quick","prs":[
            {"number":812,"title":"T","author":"dana","url":"https://github.com/acme/api/pull/812","created_at":"2026-10-15T09:00:00Z",
             "draft":false,"additions":120,"deletions":14,"files":6,"checks":"passing","review":"REVIEW_REQUIRED","labels":["security"],
             "risk":"medium","summary":"S","status":"reviewing","head_sha":"0123456789abcdef0123456789abcdef01234567"},
            {"number":815,"title":"U","author":"lee","review":""}],
            "actions":[{"id":"needs-qa","label":"Needs QA","key":"q","description":"Add the needs-qa label.","outward":true}]})
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
    fn overview_and_actions_are_checked_naming_the_field() {
        check_overview(&overview_sample()).unwrap();
        check_overview(&json!({"repo":"a/b","prs":[]})).unwrap();
        let with = |f: &dyn Fn(&mut Value)| { let mut d = overview_sample(); f(&mut d); d };
        for (data, why) in [
            (with(&|d| d["repo"] = json!("acme")), "`repo` must be owner/name"),
            (with(&|d| { d.as_object_mut().unwrap().remove("prs"); }), "needs `prs`"),
            (with(&|d| d["prs"][0]["number"] = json!(0)), "prs[0]`.number"),
            (with(&|d| d["prs"][1]["number"] = json!(812)), "#812 is listed twice"),
            (with(&|d| d["prs"][0]["title"] = json!(" ")), "`prs[0]` needs a non-empty string `title`"),
            (with(&|d| d["prs"][0]["status"] = json!("done")), "prs[0] `status` must be one of new"),
            (with(&|d| d["prs"][0]["risk"] = json!("extreme")), "prs[0] `risk`"),
            (with(&|d| d["prs"][0]["checks"] = json!("green")), "prs[0] `checks`"),
            (with(&|d| d["prs"][0]["review"] = json!("LGTM")), "prs[0] `review`"),
            (with(&|d| d["prs"][0]["additions"] = json!(-1)), "additions must be a whole number"),
            (with(&|d| d["prs"][0]["head_sha"] = json!("abc")), "head_sha must be the full"),
            (with(&|d| d["prs"][0]["url"] = json!("http://x.example/1")), "url must be an https URL"),
            (with(&|d| d["prs"][0]["labels"] = json!("bug")), "labels must be an array"),
            (with(&|d| d["prs"][0]["draft"] = json!("no")), "draft must be true or false"),
            (with(&|d| d["current"] = json!(999)), "`current` must be the number of a PR"),
            (with(&|d| d["sort"] = json!("newest")), "`sort` must be one of risk"),
            (json!({"repo":"a/b","prs":(1..=201).map(|n| json!({"number":n,"title":"t","author":"a"})).collect::<Vec<_>>()}), "at most 200"),
        ] {
            let e = format!("{:#}", check_overview(&data).unwrap_err());
            assert!(e.contains(why), "{data}: {e}");
        }
        // Actions: the same rules in both PR apps.
        let action = |a: Value| { let mut d = pr_sample(); d["actions"] = json!([a]); d };
        let ok = json!({"id":"later","label":"Label later","description":"Remember it.","key":"L"});
        check_pr(&action(ok.clone())).unwrap();
        for k in RESERVED_KEYS.chars() {
            let e = check_pr(&action(json!({"id":"x","label":"X","description":"d","key":k.to_ascii_uppercase().to_string()}))).unwrap_err().to_string();
            assert!(e.contains("is a built-in key"), "{k}: {e}");
        }
        for (a, why) in [
            (json!({"id":"x","label":"X","description":"d","key":"ctrl+k"}), "one letter or digit"),
            (json!({"id":"x","label":"X","description":"d","key":"/"}), "one letter or digit"),
            (json!({"id":"merge","label":"X","description":"d"}), "is a built-in action"),
            (json!({"id":"Needs QA","label":"X","description":"d"}), "lowercase letters"),
            (json!({"id":"x","label":"X"}), "`actions[0]` needs a string `description`"),
            (json!({"id":"x","label":"X","description":" "}), "description must be 1 to 300"),
            (json!({"id":"x","label":"X".repeat(41),"description":"d"}), "label must be 1 to 40"),
            (json!({"id":"x","label":"X","description":"d","outward":"yes"}), "outward must be true or false"),
            (json!(7), "`actions[0]` must be an object"),
        ] {
            let e = format!("{:#}", check_pr(&action(a.clone())).unwrap_err());
            assert!(e.contains(why), "{a}: {e}");
        }
        let mut twice = pr_sample();
        twice["actions"] = json!([ok, {"id":"later","label":"Y","description":"d"}]);
        assert!(check_pr(&twice).unwrap_err().to_string().contains("used twice"));
        twice["actions"] = json!([{"id":"a","label":"A","description":"d","key":"x"}, {"id":"b","label":"B","description":"d","key":"X"}]);
        assert!(check_pr(&twice).unwrap_err().to_string().contains("\"x\" is used twice"));
        twice["actions"] = json!((0..9).map(|i| json!({"id":format!("a{i}"),"label":"A","description":"d"})).collect::<Vec<_>>());
        assert!(check_pr(&twice).unwrap_err().to_string().contains("at most 8"));
        let mut o = overview_sample();
        o["actions"][0]["key"] = json!("j");
        assert!(check_overview(&o).unwrap_err().to_string().contains("pr_overview `actions[0]`.key \"j\" is a built-in key"));
    }

    #[test]
    fn pr_media_pictures_are_read_like_the_image_app_and_charts_checked() {
        let (root, cwd) = scratch("media");
        std::fs::write(cwd.join("shots/a.png"), PNG).unwrap();
        std::fs::write(root.join("outside.png"), PNG).unwrap();
        let mut csp = PanelCsp::default();
        let mut d = pr_sample();
        d["media"] = json!([{"kind":"image","path":"shots/a.png","alt":"After","caption":"Login"},
            {"kind":"chart","spec":chart_sample(),"caption":"Lines per area"},
            {"kind":"image","url":"https://user-images.example/b.png"}]);
        let stored: Value = serde_json::from_slice(&data("pr_decision", &cwd, d.clone(), &mut csp, true).unwrap()).unwrap();
        let m = &stored["media"];
        assert_eq!(m[0]["kind"], "image");
        assert_eq!(m[0]["src"], format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(PNG)));
        assert_eq!((m[0]["bytes"].as_u64(), m[0]["alt"].as_str()), (Some(PNG.len() as u64), Some("After")));
        assert_eq!((m[1]["kind"].as_str(), m[1]["caption"].as_str()), (Some("chart"), Some("Lines per area")));
        assert_eq!(m[2]["url"], "https://user-images.example/b.png");
        assert_eq!(csp.resource_domains, vec!["https://user-images.example"]);
        assert_eq!(stored["repo"], "acme/api");
        // Without media the input is stored as given.
        assert_eq!(serde_json::from_slice::<Value>(&data("pr_decision", &cwd, pr_sample(), &mut csp, true).unwrap()).unwrap(), pr_sample());
        // An update may not add an origin.
        let mut u = pr_sample();
        u["media"] = json!([{"kind":"image","url":"https://other.example/c.png"}]);
        let e = data("pr_decision", &cwd, u, &mut csp.clone(), false).unwrap_err().to_string();
        assert!(e.contains("open a new pr_decision panel for it, or pass the image as a path"), "{e}");
        let bad = |media: Value| { let mut d = pr_sample(); d["media"] = media; format!("{:#}", data("pr_decision", &cwd, d, &mut PanelCsp::default(), true).unwrap_err()) };
        for (media, why) in [
            (json!([{"kind":"image","path":"../outside.png"}]), "inside"),
            (json!([{"kind":"image","path":"shots/a.png","src":"data:image/png;base64,AA"}]), "pr_decision `media[0]` takes path or url, plus alt and caption (unknown field `src`)"),
            (json!([{"kind":"image"}]), "pr_decision `media[0]` needs path"),
            (json!([{"kind":"image","url":"http://x.example/a.png"}]), "https"),
            (json!([{"kind":"video","path":"shots/a.png"}]), "needs `kind`: image or chart"),
            (json!([{"path":"shots/a.png"}]), "needs `kind`"),
            (json!([{"kind":"chart"}]), "media[0]` needs `spec`"),
            (json!([{"kind":"chart","spec":{"type":"pie","x":{"values":[1]},"series":[]}}]), "pr_decision `media[0]`.spec: chart `type` must be one of bar"),
            (json!([{"kind":"chart","spec":chart_sample(),"title":"x"}]), "unknown field `title`"),
            (json!([7]), "`media[0]` must be an object"),
            (json!({"kind":"image"}), "`media` must be an array"),
            (json!((0..9).map(|_| json!({"kind":"chart","spec":chart_sample()})).collect::<Vec<_>>()), "has 9 items; at most 8"),
        ] {
            let e = bad(media.clone());
            assert!(e.contains(why), "{media}: {e}");
        }
        // Image bytes are capped in all, not only per file.
        let big = vec![0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'].into_iter().chain(std::iter::repeat_n(0u8, 2 * 1024 * 1024)).collect::<Vec<_>>();
        for i in 0..4 { std::fs::write(cwd.join(format!("shots/big{i}.png")), &big).unwrap(); }
        let e = bad(json!((0..4).map(|i| json!({"kind":"image","path":format!("shots/big{i}.png")})).collect::<Vec<_>>()));
        assert!(e.contains("images total more than 6 MiB"), "{e}");
        let two = bad(json!([{"kind":"image","path":"shots/big0.png"},{"kind":"image","path":"shots/big1.png"},{"kind":"chart","spec":{}}]));
        assert!(two.contains("media[2]`.spec"), "two big pictures fit: {two}");
        // The agent's own JSON keeps the 1 MiB limit.
        let mut huge = pr_sample();
        huge["summary"] = json!("x".repeat(crate::panels::MAX_FILE));
        assert!(data("pr_decision", &cwd, huge, &mut PanelCsp::default(), true).unwrap_err().to_string().contains("1 MiB"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builtins_are_embedded() {
        for app in ["markdown", "image", "chart", "table", "pr_decision", "pr_overview"] {
            let html = html(app).unwrap();
            assert!(html.contains("ui/initialize") && html.contains("2026-01-26"), "{app}");
        }
        assert!(html("custom").is_none() && html("diff").is_none());
        // Generated apps are self-contained: no external scripts, styles or fetches.
        for html in [IMAGE, CHART, TABLE, PR_DECISION, PR_OVERVIEW] {
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
        for (app, data) in [("chart", chart_sample()), ("table", json!({"columns":[{"key":"a"}],"rows":[]})), ("pr_decision", pr_sample()),
            ("pr_overview", overview_sample())] {
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
