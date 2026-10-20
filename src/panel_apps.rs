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
pub const BOARD: &str = include_str!("../web/panels/board.html");

/// For error messages and tool descriptions.
pub const BUILTINS: &str = "markdown, image, chart, table, pr_decision, pr_overview, board, web_preview";
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
/// Single keys the PR apps use themselves (pr_decision: M R A O S H G; pr_overview: J K O H N M).
/// Muster's own shortcuts all need Ctrl, Alt or a function key, so a plain letter or digit
/// never clashes with them.
pub const RESERVED_KEYS: &str = "mraoshjkng";
/// Built-in event names, which a custom action id may not reuse.
const BUILTIN_IDS: &[&str] = &["merge", "request_changes", "ask", "answers", "open", "skip", "hold", "next", "custom", "closed", "overflow"];
/// `pr_decision` `questions` (web/src/panels/apps/shared/questions.ts has the same limits).
pub const MAX_QUESTIONS: usize = 20;
const MAX_QUESTION: usize = 500;
/// An old plain-string question may be longer.
const MAX_LEGACY_QUESTION: usize = 2000;
const MAX_WHY: usize = 300;
const MAX_QUESTION_OPTIONS: usize = 9;
/// A `text` question's suggested answer (the user's typed answer has the same limit).
const MAX_ANSWER: usize = 1000;
/// `verdict` and `undo` (pr_decision) and a card's `verdict` (pr_overview): one line each.
const MAX_LINE: usize = 300;

pub fn html(app: &str) -> Option<&'static str> {
    Some(match app {
        "markdown" => crate::panels::MARKDOWN,
        "image" => IMAGE,
        "chart" => CHART,
        "table" => TABLE,
        "pr_decision" => PR_DECISION,
        "pr_overview" => PR_OVERVIEW,
        "board" => BOARD,
        // Drawn by the UI, not the sandbox proxy (src/web_preview.rs).
        "web_preview" => crate::web_preview::PLACEHOLDER,
        _ => return None,
    })
}

/// The stored input for `app`. For `image`, reads `path` from `cwd` into a data URI, or adds an
/// https `url`'s origin to the panel's resourceDomains (`opening`) or requires it there
/// (update). Each picture in `pr_decision` media is read the same way, from a path only.
pub fn data(app: &str, cwd: &Path, data: Value, csp: &mut PanelCsp, opening: bool) -> anyhow::Result<Vec<u8>> {
    match app {
        "pr_decision" => {
            check_pr(&data)?;
            // The agent's own JSON keeps the general limit; only the images it names add to it.
            crate::panels::data_bytes(&data)?;
            let data = serde_json::to_vec(&media(cwd, data)?)?;
            ensure!(data.len() <= MAX_PR_DATA, "pr_decision panel data is too large");
            return Ok(data);
        }
        "pr_overview" => check_overview(&data)?,
        "chart" => check_chart(&data)?,
        "table" => check_table(&data)?,
        "board" => check_board(&data)?,
        _ => {}
    }
    if app != "image" { return crate::panels::data_bytes(&data); }
    let data = serde_json::to_vec(&image(cwd, data, Some((csp, opening)), "image", MAX_IMAGE)?)?;
    ensure!(data.len() <= MAX_IMAGE_DATA, "image panel data is too large");
    Ok(data)
}

/// `what` names the input in errors: "image", or "pr_decision `media[2]`". `web` is the panel's
/// CSP and whether it is opening, when an https `url` is allowed (the image app); without it
/// only a `path` is. `max` caps the file's size.
fn image(cwd: &Path, data: Value, web: Option<(&mut PanelCsp, bool)>, what: &str, max: usize) -> anyhow::Result<Value> {
    let Value::Object(mut o) = data else { bail!("{what} must be an object") };
    let takes = if web.is_some() { "path or url, plus alt and caption" } else { "path, plus alt and caption" };
    for (k, v) in &o {
        let limit = match k.as_str() { "path" => 4096, "url" if web.is_some() => 4096, "alt" => 1000, "caption" => 2000,
            "url" => bail!("{what} takes a path only: download the picture into your task directory and pass its path"),
            _ => bail!("{what} takes {takes} (unknown field `{k}`)") };
        ensure!(v.as_str().is_some_and(|s| s.chars().count() <= limit), "{what} {k} must be a string of at most {limit} characters");
    }
    match (o.get("path").and_then(Value::as_str), o.get("url").and_then(Value::as_str)) {
        (Some(_), Some(_)) => bail!("give {what} path or url, not both"),
        (Some(path), None) => {
            let (mime, bytes) = read_image_max(cwd, path, max)?;
            let src = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
            o.insert("src".into(), json!(src));
            o.insert("mime".into(), json!(mime));
            o.insert("bytes".into(), json!(bytes.len()));
        }
        (None, Some(url)) => {
            let Some((csp, opening)) = web else { bail!("{what} takes a path only") };
            let origin = https_origin(url)?;
            if !csp.resource_domains.contains(&origin) {
                ensure!(opening, "the image URL's origin {origin} is not in this panel's resourceDomains; open a new image panel for it");
                csp.resource_domains.push(origin);
            }
        }
        (None, None) if web.is_some() => bail!("{what} needs path (a file in the task worktree) or url (https)"),
        (None, None) => bail!("{what} needs path (a file in the task worktree)"),
    }
    Ok(Value::Object(o))
}

/// `pr_decision` `media`: pictures are files in the task worktree, read like the image app's
/// input (into `src`); charts are checked like a chart panel's spec. At most [`MAX_MEDIA`]
/// items and [`MAX_MEDIA_BYTES`] of image files in all, checked before each file is read.
/// Unknown fields are refused, as in the image app.
fn media(cwd: &Path, data: Value) -> anyhow::Result<Value> {
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
                // Never read past the budget: the file may be at most what is left of it.
                let left = MAX_MEDIA_BYTES - total;
                let mut stored = image(cwd, Value::Object(m), None, &what, left.min(MAX_IMAGE))
                    .map_err(|e| if e.to_string().contains("is larger than") {
                        anyhow::anyhow!("pr_decision `media` images total more than 6 MiB at media[{i}]; use fewer or smaller pictures")
                    } else { e })?;
                total += stored.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
                stored["kind"] = json!("image");
                out.push(stored);
            }
            Some("chart") => {
                for k in m.keys() { ensure!(k == "spec" || k == "caption", "{what} is a chart: it takes spec and caption (unknown field `{k}`)"); }
                let spec = m.get("spec").filter(|v| !v.is_null()).with_context(|| format!("{what} needs `spec` (a chart app spec)"))?;
                check_chart(spec).with_context(|| format!("{what}.spec"))?;
                opt_text(&m, &what, "caption", 2000)?;
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
// The apps validate everything again in the panel and show a problem there instead of the
// panel. So everything an app would refuse is refused here first, with the same limits:
// lengths count Unicode code points (characters) on both sides.

fn field<'a>(o: &'a serde_json::Map<String, Value>, app: &str, key: &str) -> anyhow::Result<&'a Value> {
    o.get(key).filter(|v| !v.is_null()).with_context(|| format!("{app} data needs `{key}`"))
}

fn text<'a>(o: &'a serde_json::Map<String, Value>, app: &str, key: &str) -> anyhow::Result<&'a str> {
    let s = field(o, app, key)?.as_str().with_context(|| format!("{app} `{key}` must be a string"))?;
    ensure!(!s.trim().is_empty(), "{app} `{key}` must not be empty");
    Ok(s)
}

/// A required, non-empty string of at most `max` characters.
fn req_text<'a>(o: &'a serde_json::Map<String, Value>, at: &str, key: &str, max: usize) -> anyhow::Result<&'a str> {
    let s = text(o, at, key)?;
    ensure!(s.chars().count() <= max, "{at} `{key}` is longer than {max} characters");
    Ok(s)
}

/// An optional string (absent or null) of at most `max` characters.
fn opt_text(o: &serde_json::Map<String, Value>, at: &str, key: &str, max: usize) -> anyhow::Result<()> {
    let Some(v) = o.get(key).filter(|v| !v.is_null()) else { return Ok(()) };
    let s = v.as_str().with_context(|| format!("{at} `{key}` must be a string"))?;
    ensure!(s.chars().count() <= max, "{at} `{key}` is longer than {max} characters");
    Ok(())
}

/// An optional array of at most `items` strings of at most `max` characters each.
fn opt_strings(o: &serde_json::Map<String, Value>, at: &str, key: &str, items: usize, max: usize) -> anyhow::Result<()> {
    let Some(v) = o.get(key).filter(|v| !v.is_null()) else { return Ok(()) };
    let list = v.as_array().with_context(|| format!("{at} `{key}` must be an array of strings"))?;
    ensure!(list.len() <= items, "{at} `{key}` has {} items; at most {items}", list.len());
    for (i, s) in list.iter().enumerate() {
        let s = s.as_str().with_context(|| format!("{at} `{key}[{i}]` must be a string"))?;
        ensure!(s.chars().count() <= max, "{at} `{key}[{i}]` is longer than {max} characters");
    }
    Ok(())
}

/// An optional list of objects, at most `items`.
fn opt_objects<'a>(o: &'a serde_json::Map<String, Value>, at: &str, key: &str, items: usize, required: bool)
    -> anyhow::Result<Vec<&'a serde_json::Map<String, Value>>> {
    let Some(v) = o.get(key).filter(|v| !v.is_null()) else {
        ensure!(!required, "{at} data needs `{key}`");
        return Ok(Vec::new());
    };
    let list = v.as_array().with_context(|| format!("{at} `{key}` must be an array"))?;
    ensure!(list.len() <= items, "{at} `{key}` has {} items; at most {items}", list.len());
    list.iter().enumerate().map(|(i, x)| object(x, &format!("{at} `{key}[{i}]`"))).collect()
}

fn https_ok(url: &str) -> bool {
    !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && url.parse::<axum::http::Uri>().is_ok_and(|u| u.scheme_str() == Some("https") && u.authority().is_some_and(|a| !a.as_str().contains('@') && !a.host().is_empty()))
}

fn opt_url(o: &serde_json::Map<String, Value>, at: &str, key: &str) -> anyhow::Result<()> {
    opt_text(o, at, key, 2000)?;
    if let Some(u) = o.get(key).and_then(Value::as_str) { ensure!(https_ok(u), "{at} `{key}` must be an https URL"); }
    Ok(())
}

fn opt_bool(o: &serde_json::Map<String, Value>, at: &str, key: &str) -> anyhow::Result<()> {
    ensure!(o.get(key).is_none_or(|v| v.is_null() || v.is_boolean()), "{at} `{key}` must be true or false");
    Ok(())
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

/// Everything the `pr_decision` app checks (web/src/panels/apps/pr_decision/logic.ts), above
/// all the fields the merge command and its safety checks are built from. `media` is checked
/// and read by [`media`].
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
        ensure!(branch_ok(b), "pr_decision `{key}` is not a valid branch name");
    }
    let url = text(o, app, "url")?;
    ensure!(https_ok(url) && url.chars().count() <= 2000, "pr_decision `url` must be an https URL");
    for (key, max) in [("title", 500), ("author", 100), ("summary", 20_000)] { req_text(o, app, key, max)?; }
    for key in ["verdict", "undo"] { one_line(o, app, key, MAX_LINE)?; }
    let risk = object(field(o, app, "risk")?, "pr_decision `risk`")?;
    one_of(risk, "pr_decision risk", "level", &["low", "medium", "high"], true)?;
    opt_strings(risk, "pr_decision risk", "reasons", 20, 2000)?;
    let checks = object(field(o, app, "checks")?, "pr_decision `checks`")?;
    one_of(checks, "pr_decision checks", "state", &["passing", "failing", "pending", "none"], true)?;
    for (i, item) in opt_objects(checks, "pr_decision checks", "items", 200, false)?.into_iter().enumerate() {
        let at = format!("pr_decision checks.items[{i}]");
        one_of(item, &at, "state", &["pass", "fail", "pending", "skipping", "cancel"], true)?;
        req_text(item, &at, "name", 300)?;
        opt_url(item, &at, "url")?;
    }
    if let Some(reviews) = o.get("reviews").filter(|v| !v.is_null()) {
        let reviews = object(reviews, "pr_decision `reviews`")?;
        opt_text(reviews, "pr_decision reviews", "decision", 50)?;
        for (i, r) in opt_objects(reviews, "pr_decision reviews", "items", 100, false)?.into_iter().enumerate() {
            let at = format!("pr_decision reviews.items[{i}]");
            req_text(r, &at, "author", 100)?;
            req_text(r, &at, "state", 50)?;
        }
    }
    for (i, c) in opt_objects(o, app, "changes", 50, true)?.into_iter().enumerate() {
        let at = format!("pr_decision changes[{i}]");
        for (key, max) in [("file", 1000), ("note", 5000), ("diff", 100_000)] {
            let v = c.get(key).filter(|v| !v.is_null()).with_context(|| format!("each pr_decision change needs a string `{key}` ({at})"))?;
            let v = v.as_str().with_context(|| format!("each pr_decision change needs a string `{key}` ({at})"))?;
            ensure!(!v.trim().is_empty() && v.chars().count() <= max, "{at} `{key}` must be 1 to {max} characters");
        }
    }
    opt_text(o, app, "rest_note", 2000)?;
    check_questions(o)?;
    if let Some(answer) = o.get("answer").filter(|v| !v.is_null()) {
        let answer = object(answer, "pr_decision `answer`")?;
        req_text(answer, "pr_decision answer", "question", 8000)?;
        req_text(answer, "pr_decision answer", "text", 20_000)?;
    }
    opt_text(o, app, "status_detail", 4000)?;
    opt_bool(o, app, "delete_branch_default")?;
    one_of(o, app, "merge_method_default", &["squash", "merge", "rebase"], false)?;
    one_of(o, app, "status", &["open", "merging", "queued", "merged", "failed", "requested_changes"], false)?;
    check_actions(o, app)?;
    Ok(())
}

/// An optional one-line string (no line breaks) of at most `max` characters.
fn one_line(o: &serde_json::Map<String, Value>, at: &str, key: &str, max: usize) -> anyhow::Result<()> {
    opt_text(o, at, key, max)?;
    if let Some(s) = o.get(key).and_then(Value::as_str) { ensure!(!s.contains(['\n', '\r']), "{at} `{key}` must be one line"); }
    Ok(())
}

/// Git ref rules that matter here: no spaces, control characters or ~^:?*[\.
fn branch_ok(b: &str) -> bool {
    !b.is_empty() && b.len() <= 255 && !b.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
}

fn id_ok(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// `pr_decision` `questions`: plain strings (the old form, read as kind `text` with ids q1, q2…
/// by place) or `{id, text, why?, kind: yes_no|choice|text, options?, recommended?, required?}`.
/// A `choice` has 2 to 9 options `{label, value}` with unique values; `recommended` is one of
/// the values (yes_no: "yes" or "no"), or for `text` a suggested answer.
fn check_questions(o: &serde_json::Map<String, Value>) -> anyhow::Result<()> {
    let Some(list) = o.get("questions").filter(|v| !v.is_null()) else { return Ok(()) };
    let list = list.as_array().context("pr_decision `questions` must be an array")?;
    ensure!(list.len() <= MAX_QUESTIONS, "pr_decision `questions` has {} items; at most {MAX_QUESTIONS}", list.len());
    let mut ids: Vec<String> = Vec::new();
    for (i, q) in list.iter().enumerate() {
        let at = format!("pr_decision `questions[{i}]`");
        if let Some(s) = q.as_str() {
            ensure!(s.chars().count() <= MAX_LEGACY_QUESTION, "{at} is longer than {MAX_LEGACY_QUESTION} characters");
            if s.trim().is_empty() { continue }
            let id = format!("q{}", i + 1);
            ensure!(!ids.contains(&id), "{at} id {id:?} is used twice");
            ids.push(id);
            continue;
        }
        let q = q.as_object().with_context(|| format!("{at} must be a string or an object"))?;
        let id = q.get("id").and_then(Value::as_str).with_context(|| format!("{at} needs a string `id`"))?;
        ensure!(id_ok(id), "{at}.id must be lowercase letters, digits, - or _ (at most 40; got {id:?})");
        ensure!(!ids.iter().any(|x| x == id), "{at}.id {id:?} is used twice");
        ids.push(id.to_string());
        let item = format!("pr_decision questions[{i}]");
        req_text(q, &item, "text", MAX_QUESTION)?;
        opt_text(q, &item, "why", MAX_WHY)?;
        one_of(q, &item, "kind", &["yes_no", "choice", "text"], true)?;
        opt_bool(q, &item, "required")?;
        let kind = q.get("kind").and_then(Value::as_str).unwrap_or_default();
        let mut values: Vec<&str> = Vec::new();
        if kind == "choice" {
            let options = opt_objects(q, &item, "options", MAX_QUESTION_OPTIONS, true)?;
            ensure!(options.len() >= 2, "{at}.options needs at least 2 options");
            for (j, c) in options.into_iter().enumerate() {
                let oat = format!("pr_decision questions[{i}].options[{j}]");
                req_text(c, &oat, "label", 80)?;
                let v = req_text(c, &oat, "value", 100)?;
                ensure!(!values.contains(&v), "{oat} `value` {v:?} is used twice");
                values.push(v);
            }
        } else {
            ensure!(q.get("options").is_none_or(Value::is_null), "{at}.options: {}",
                if kind == "yes_no" { "a yes_no question has its own Yes and No; use kind choice for other options" } else { "a text question takes no options" });
            if kind == "yes_no" { values = vec!["yes", "no"]; }
        }
        if let Some(r) = q.get("recommended").filter(|v| !v.is_null()) {
            let r = r.as_str().with_context(|| format!("{at}.recommended must be a string"))?;
            if kind == "text" {
                ensure!(!r.trim().is_empty() && r.chars().count() <= MAX_ANSWER, "{at}.recommended must be 1 to {MAX_ANSWER} characters");
            } else {
                ensure!(values.contains(&r), "{at}.recommended must be one of the option values ({})",
                    values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "));
            }
        }
    }
    Ok(())
}

fn repo_ok(repo: &str) -> bool {
    let part = |s: &str| (1..=100).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
    repo.split_once('/').is_some_and(|(a, b)| part(a) && part(b))
}

fn sha_ok(sha: &str) -> bool { sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) }

/// `2026-10-18T09:30:00Z` or with a fraction and/or an offset (`+02:00`): what `gh` prints and
/// what the app accepts (its `DATE` pattern).
fn date_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if b.len() < 20 || !(digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10) && b[10] == b'T'
        && digits(11..13) && b[13] == b':' && digits(14..16) && b[16] == b':' && digits(17..19)) { return false }
    let mut rest = &b[19..];
    if rest.first() == Some(&b'.') {
        let n = rest[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || n > 9 { return false }
        rest = &rest[1 + n..];
    }
    rest == b"Z" || (rest.len() == 6 && (rest[0] == b'+' || rest[0] == b'-') && rest[1..3].iter().all(u8::is_ascii_digit)
        && rest[3] == b':' && rest[4..6].iter().all(u8::is_ascii_digit))
}

/// Visible names of the apps' own buttons: a custom label may not look like one of them.
const BUILTIN_LABELS: &[&str] = &["merge", "merge anyway", "confirm merge", "request changes", "ask", "open", "open on github",
    "github", "skip", "hold", "next", "cancel", "confirm", "send answers"];

/// A label as the user reads it: trimmed, spaces collapsed, lowercase, without a trailing
/// "…", "." or "!" and without a leading "#".
pub fn label_key(label: &str) -> String {
    let words: Vec<&str> = label.split_whitespace().collect();
    words.join(" ").to_lowercase().trim_end_matches(['…', '.', '!', ':']).trim_start_matches('#').trim().to_string()
}

/// The user's own actions (`actions`, both PR apps): buttons that send a `custom` event, or in
/// pr_decision (`mode` `toggle`, the default) options that ride along with the next event. Ids and
/// keys must be unique, keys one letter or digit not used by the apps, labels must not look like
/// a built-in button, every action must say what the agent will do (`description`) and say
/// whether it reaches outside this machine (`outward`, required): those ask the user to
/// confirm first.
pub fn check_actions(o: &serde_json::Map<String, Value>, app: &str) -> anyhow::Result<()> {
    let Some(list) = o.get("actions").filter(|v| !v.is_null()) else { return Ok(()) };
    let list = list.as_array().with_context(|| format!("{app} `actions` must be an array"))?;
    ensure!(list.len() <= MAX_ACTIONS, "{app} `actions` has {} actions; at most {MAX_ACTIONS}", list.len());
    let (mut ids, mut keys) = (Vec::new(), Vec::new());
    for (i, a) in list.iter().enumerate() {
        let at = format!("{app} `actions[{i}]`");
        let a = object(a, &at)?;
        let id = a.get("id").and_then(Value::as_str).with_context(|| format!("{at} needs a string `id`"))?;
        ensure!(id_ok(id), "{at}.id must be lowercase letters, digits, - or _ (at most 40; got {id:?})");
        ensure!(!BUILTIN_IDS.contains(&id), "{at}.id {id:?} is a built-in action");
        ensure!(!ids.contains(&id), "{at}.id {id:?} is used twice");
        ids.push(id);
        for (key, max) in [("label", 40), ("description", 300)] {
            let s = a.get(key).and_then(Value::as_str).with_context(|| format!("{at} needs a string `{key}`"))?;
            ensure!(!s.trim().is_empty() && s.chars().count() <= max, "{at}.{key} must be 1 to {max} characters");
        }
        let label = a.get("label").and_then(Value::as_str).unwrap_or_default();
        ensure!(!BUILTIN_LABELS.contains(&label_key(label).as_str()), "{at}.label {label:?} looks like a built-in button; name what it does instead");
        if let Some(k) = a.get("key").filter(|v| !v.is_null()) {
            let k = k.as_str().filter(|k| k.len() == 1 && k.bytes().all(|c| c.is_ascii_alphanumeric()))
                .with_context(|| format!("{at}.key must be one letter or digit"))?.to_ascii_lowercase();
            ensure!(!RESERVED_KEYS.contains(&k), "{at}.key {k:?} is a built-in key (m r a o s h j k n g are taken)");
            ensure!(!keys.contains(&k), "{at}.key {k:?} is used twice");
            keys.push(k);
        }
        ensure!(a.get("outward").is_some_and(Value::is_boolean),
            "{at}.outward is required: true if the action changes GitHub or anything off this machine (comments, labels, closing, pushing), else false");
        one_of(a, &at, "mode", &["toggle", "now"], false)?;
    }
    Ok(())
}

/// Everything the `pr_overview` app checks (web/src/panels/apps/pr_overview/logic.ts).
pub fn check_overview(data: &Value) -> anyhow::Result<()> {
    let app = "pr_overview";
    let o = object(data, "pr_overview data")?;
    let repo = text(o, app, "repo")?;
    ensure!(repo_ok(repo), "pr_overview `repo` must be owner/name (got {repo:?})");
    opt_text(o, app, "title", 200)?;
    opt_text(o, app, "takeaway", 600)?;
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
        for (key, max) in [("title", 500), ("author", 100)] {
            let s = p.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
                .with_context(|| format!("{at} needs a non-empty string `{key}`"))?;
            ensure!(s.chars().count() <= max, "{at}.{key} is longer than {max} characters");
        }
        let item = format!("pr_overview prs[{i}]");
        one_of(p, &item, "checks", &["passing", "failing", "pending", "none"], false)?;
        one_of(p, &item, "risk", &["low", "medium", "high"], false)?;
        one_of(p, &item, "status", &["new", "ready", "reviewing", "merging", "merged", "queued", "held", "skipped", "changes_requested", "failed"], false)?;
        if p.get("review").and_then(Value::as_str) != Some("") {
            one_of(p, &item, "review", &["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", "NONE"], false)?;
        }
        for key in ["additions", "deletions", "files"] {
            ensure!(p.get(key).is_none_or(|v| v.is_null() || v.as_u64().is_some_and(|n| n <= 1 << 53)), "{at}.{key} must be a whole number, 0 or more");
        }
        ensure!(p.get("draft").is_none_or(|v| v.is_null() || v.is_boolean()), "{at}.draft must be true or false");
        if let Some(sha) = p.get("head_sha").filter(|v| !v.is_null()) {
            ensure!(sha.as_str().is_some_and(sha_ok), "{at}.head_sha must be the full 40-character lowercase commit SHA");
        }
        if let Some(url) = p.get("url").filter(|v| !v.is_null()) {
            ensure!(url.as_str().is_some_and(|u| https_ok(u) && u.chars().count() <= 2000), "{at}.url must be an https URL");
        }
        if let Some(date) = p.get("created_at").filter(|v| !v.is_null()) {
            ensure!(date.as_str().is_some_and(date_ok), "{at}.created_at must be a date such as 2026-10-18T09:30:00Z");
        }
        for (key, max) in [("summary", 500), ("status_detail", 500)] { opt_text(p, &item, key, max)?; }
        one_line(p, &item, "verdict", MAX_LINE)?;
        if let Some(base) = p.get("base").filter(|v| !v.is_null()) {
            ensure!(base.as_str().is_some_and(branch_ok), "{at}.base is not a valid branch name");
        }
        one_of(p, &item, "merge_method_default", &["squash", "merge", "rebase"], false)?;
        opt_bool(p, &item, "delete_branch_default")?;
        opt_bool(p, &item, "quick_win")?;
        if p.get("quick_win").and_then(Value::as_bool) == Some(true) {
            // Merge from the overview needs what the merge command and its checks are built from:
            // the agent marks a quick win only after the full one-snapshot review.
            for (key, what) in [("head_sha", "the head the review saw"), ("base", "the branch it merges into"), ("verdict", "one plain sentence")] {
                ensure!(p.get(key).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()), "{at}.{key} is required for a quick win ({what})");
            }
            ensure!(p.get("checks").and_then(Value::as_str) == Some("passing"), "{at}.quick_win needs checks passing");
            ensure!(p.get("draft").and_then(Value::as_bool) != Some(true), "{at}.quick_win cannot be a draft");
        }
        if let Some(labels) = p.get("labels").filter(|v| !v.is_null()) {
            let labels = labels.as_array().with_context(|| format!("{at}.labels must be an array of strings"))?;
            ensure!(labels.len() <= 20 && labels.iter().all(|l| l.as_str().is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= 100)),
                "{at}.labels must be at most 20 label names of 1 to 100 characters");
        }
    }
    if let Some(current) = o.get("current").filter(|v| !v.is_null()) {
        ensure!(current.as_u64().is_some_and(|n| numbers.contains(&n)), "pr_overview `current` must be the number of a PR in `prs`");
    }
    one_of(o, app, "sort", &["risk", "quick", "oldest"], false)?;
    check_actions(o, app)?;
    Ok(())
}

/// The kit's number formats (web/src/panels/kit/format.ts `FORMATS`).
const FORMATS: &[&str] = &["int", "pct", "usd", "ms", "s", "plain"];

/// A colour an agent may give: a kit token (`var(--mk-…)`) or a hex colour, nothing that could
/// carry other CSS (web/src/panels/kit/format.ts `COLOR`).
pub fn color_ok(c: &str) -> bool {
    let hex = |h: &str| (h.len() == 3 || h.len() == 6) && h.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(h) = c.strip_prefix('#') { return hex(h) }
    c.strip_prefix("var(--mk-").and_then(|r| r.strip_suffix(')'))
        .is_some_and(|t| (1..=30).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

fn opt_color(o: &serde_json::Map<String, Value>, at: &str) -> anyhow::Result<()> {
    if let Some(c) = o.get("color").filter(|v| !v.is_null()) {
        ensure!(c.as_str().is_some_and(color_ok), "{at} `color` must be a kit colour token such as var(--mk-c2) or a hex colour such as #6a56e6");
    }
    Ok(())
}

fn opt_number(o: &serde_json::Map<String, Value>, at: &str, key: &str) -> anyhow::Result<Option<f64>> {
    match o.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => Ok(Some(v.as_f64().with_context(|| format!("{at} `{key}` must be a number"))?)),
    }
}

/// A headline number: chart `stats` and Board `stats` items.
fn check_stat(s: &Value, at: &str) -> anyhow::Result<()> {
    let s = object(s, at)?;
    req_text(s, at, "label", 100)?;
    let v = s.get("value").filter(|v| !v.is_null()).with_context(|| format!("{at} needs `value`"))?;
    ensure!(v.is_number() || v.as_str().is_some_and(|t| t.chars().count() <= 40), "{at} `value` must be a number or a string of at most 40 characters");
    for (key, max) in [("unit", 20), ("delta", 20), ("note", 100)] { opt_text(s, at, key, max)?; }
    one_of(s, at, "tone", &["good", "bad", "flat"], false)?;
    opt_color(s, at)?;
    if let Some(t) = s.get("trend").filter(|v| !v.is_null()) {
        let t = t.as_array().with_context(|| format!("{at} `trend` must be an array of numbers"))?;
        ensure!(t.len() <= 200 && t.iter().all(Value::is_number), "{at} `trend` must be at most 200 numbers");
    }
    Ok(())
}

/// Everything the chart app checks (web/src/panels/apps/chart/logic.ts). The fields added for the
/// Briefing look (takeaway … stats) are all optional; a chart with no values is allowed, so the
/// panel can say there is nothing to show (`empty`).
pub fn check_chart(data: &Value) -> anyhow::Result<()> {
    let app = "chart";
    let o = object(data, "chart data")?;
    one_of(o, app, "type", &["bar", "line", "scatter", "area"], true)?;
    let kind = o.get("type").and_then(Value::as_str).unwrap_or_default();
    opt_text(o, app, "title", 200)?;
    let x = object(field(o, app, "x")?, "chart `x`")?;
    opt_text(x, "chart x", "label", 200)?;
    let x = x.get("values").and_then(Value::as_array).context("chart data needs `x.values` (an array)")?;
    ensure!(x.len() <= 2000, "chart `x.values` has {} values; at most 2000", x.len());
    ensure!(x.iter().all(|v| v.is_number() || v.as_str().is_some_and(|s| s.chars().count() <= 200)),
        "chart `x.values` must be numbers or strings of at most 200 characters");
    if kind == "scatter" {
        ensure!(x.iter().all(Value::is_number), "chart `x.values` must all be numbers for a scatter chart");
    }
    let series = field(o, app, "series")?.as_array().context("chart `series` must be an array")?;
    ensure!(series.len() <= 12, "chart `series` has {} series; at most 12", series.len());
    let mut names = Vec::with_capacity(series.len());
    for (i, s) in series.iter().enumerate() {
        let s = object(s, "each chart series")?;
        let name = s.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty() && n.chars().count() <= 100)
            .with_context(|| format!("chart series {i} needs a string `name` of 1 to 100 characters"))?;
        names.push(name);
        let values = s.get("values").and_then(Value::as_array).with_context(|| format!("chart series {i} needs `values` (an array)"))?;
        ensure!(values.len() == x.len(), "chart series {i} has {} values but x.values has {}", values.len(), x.len());
        ensure!(values.iter().all(|v| v.is_null() || v.is_number()), "chart series {i} values must be numbers or null");
        opt_color(s, &format!("chart series[{i}]"))?;
    }
    if let Some(y) = o.get("y").filter(|v| !v.is_null()) {
        let y = object(y, "chart `y`")?;
        opt_text(y, "chart y", "label", 200)?;
        for k in ["min", "max"] { ensure!(y.get(k).is_none_or(|v| v.is_null() || v.is_number()), "chart `y.{k}` must be a number"); }
        if let (Some(lo), Some(hi)) = (y.get("min").and_then(Value::as_f64), y.get("max").and_then(Value::as_f64)) {
            ensure!(lo < hi, "chart `y.min` must be less than `y.max`");
        }
    }
    for (key, max) in [("takeaway", 600), ("scope", 200), ("source", 300), ("empty", 200)] { opt_text(o, app, key, max)?; }
    one_of(o, app, "format", FORMATS, false)?;
    one_of(o, app, "xFormat", FORMATS, false)?;
    one_of(o, app, "labels", &["auto", "values", "end", "none"], false)?;
    if let Some(h) = o.get("highlight").filter(|v| !v.is_null()) {
        let h = h.as_str().context("chart `highlight` must be a string")?;
        ensure!(names.contains(&h), "chart `highlight` must be the name of a series (got {h:?})");
    }
    for key in ["stacked", "trend"] { opt_bool(o, app, key)?; }
    if o.get("stacked").and_then(Value::as_bool) == Some(true) { ensure!(matches!(kind, "bar" | "area"), "chart `stacked` applies to bar and area charts only"); }
    if o.get("trend").and_then(Value::as_bool) == Some(true) { ensure!(kind == "scatter", "chart `trend` applies to scatter charts only"); }
    for (i, a) in opt_objects(o, app, "annotations", 20, false)?.into_iter().enumerate() {
        let at = format!("chart annotations[{i}]");
        req_text(a, &at, "label", 60)?;
        let ax = a.get("x").filter(|v| !v.is_null()).with_context(|| format!("{at} needs `x`"))?;
        // Numbers match by value, as in the app (1 and 1.0 are the same x); strings exactly.
        let same = |v: &Value| match (v.as_f64(), ax.as_f64()) { (Some(a), Some(b)) => a == b, _ => v == ax };
        ensure!(x.iter().any(same), "{at} `x` must be one of x.values (got {ax})");
    }
    if let Some(stats) = o.get("stats").filter(|v| !v.is_null()) {
        let stats = stats.as_array().context("chart `stats` must be an array")?;
        ensure!(stats.len() <= 4, "chart `stats` has {} items; at most 4", stats.len());
        for (i, s) in stats.iter().enumerate() { check_stat(s, &format!("chart stats[{i}]"))?; }
    }
    if let Some(pin) = opt_number(o, app, "pin")? {
        ensure!(pin.fract() == 0.0 && pin >= 0.0 && (pin as usize) < x.len(), "chart `pin` must be the index of an x value");
    }
    Ok(())
}

/// Block kinds a Board takes (web/src/panels/apps/board/logic.ts `KINDS`).
pub const BOARD_KINDS: &[&str] = &["stats", "chart", "list", "meters", "callout", "actions"];
const BOARD_TONES: &[&str] = &["good", "warn", "bad", "info", "accent"];

/// An optional icon name: letters only (the panel draws "?" for a name it does not know).
fn opt_icon(o: &serde_json::Map<String, Value>, at: &str) -> anyhow::Result<()> {
    if let Some(v) = o.get("icon").filter(|v| !v.is_null()) {
        ensure!(v.as_str().is_some_and(|s| (1..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphabetic())),
            "{at} `icon` must be an icon name such as check, alert or chart");
    }
    Ok(())
}

fn opt_range(o: &serde_json::Map<String, Value>, at: &str, key: &str, lo: f64, hi: f64) -> anyhow::Result<()> {
    if let Some(n) = opt_number(o, at, key)? { ensure!((lo..=hi).contains(&n), "{at} `{key}` must be from {lo} to {hi}"); }
    Ok(())
}

/// Unknown fields: refused, naming the field and what the object takes.
fn known(o: &serde_json::Map<String, Value>, at: &str, keys: &[&str]) -> anyhow::Result<()> {
    for k in o.keys() { ensure!(keys.contains(&k.as_str()), "{at} has an unknown field `{k}` (it takes {})", keys.join(", ")); }
    Ok(())
}

/// A short shown value: a number, or a string of at most 40 characters.
fn opt_short_value(o: &serde_json::Map<String, Value>, at: &str) -> anyhow::Result<()> {
    if let Some(v) = o.get("value").filter(|v| !v.is_null()) {
        ensure!(v.is_number() || v.as_str().is_some_and(|s| s.chars().count() <= 40), "{at} `value` must be a number or a string of at most 40 characters");
    }
    Ok(())
}

/// Everything the board app checks (web/src/panels/apps/board/logic.ts), with the same limits.
/// A Board is plain JSON: no HTML and no script, so every string is shown as text and every
/// colour is a kit token or a hex colour.
pub fn check_board(data: &Value) -> anyhow::Result<()> {
    let app = "board";
    let o = object(data, "board data")?;
    known(o, "board data", &["title", "scope", "takeaway", "source", "min", "blocks"])?;
    req_text(o, app, "title", 200)?;
    for (key, max) in [("scope", 200), ("takeaway", 600), ("source", 300)] { opt_text(o, app, key, max)?; }
    opt_range(o, app, "min", 160.0, 1200.0)?;
    let blocks = opt_objects(o, app, "blocks", 40, true)?;
    ensure!(!blocks.is_empty(), "board `blocks` needs at least 1 block");
    let (mut ids, mut keys): (Vec<&str>, Vec<String>) = (Vec::new(), Vec::new());
    for (i, b) in blocks.into_iter().enumerate() {
        let at = format!("board blocks[{i}]");
        let kind = b.get("kind").and_then(Value::as_str).with_context(|| format!("{at} needs `kind` (one of {})", BOARD_KINDS.join(", ")))?;
        ensure!(BOARD_KINDS.contains(&kind), "{at} `kind` must be one of {} (got {kind:?})", BOARD_KINDS.join(", "));
        ensure!(b.get("span").is_none_or(|v| v.is_null() || v.as_str() == Some("full")), "{at} `span` must be \"full\" (or left out for one column)");
        opt_text(b, &at, "title", 200)?;
        opt_text(b, &at, "aside", 100)?;
        let base = ["kind", "span", "title", "aside"];
        let with = |extra: &[&'static str]| base.iter().copied().chain(extra.iter().copied()).collect::<Vec<_>>();
        let items = |max: usize| -> anyhow::Result<Vec<&serde_json::Map<String, Value>>> {
            let list = opt_objects(b, &at, "items", max, true)?;
            ensure!(!list.is_empty(), "{at} `items` needs at least 1 item");
            Ok(list)
        };
        match kind {
            "stats" => {
                known(b, &at, &with(&["items"]))?;
                for (j, s) in items(8)?.into_iter().enumerate() {
                    let sat = format!("{at}.items[{j}]");
                    known(s, &sat, &["label", "value", "unit", "delta", "tone", "note", "trend", "color"])?;
                    check_stat(&Value::Object(s.clone()), &sat)?;
                }
            }
            "chart" => {
                known(b, &at, &with(&["spec", "height"]))?;
                opt_range(b, &at, "height", 80.0, 800.0)?;
                let spec = b.get("spec").filter(|v| !v.is_null()).with_context(|| format!("{at} needs `spec` (the chart app's spec)"))?;
                check_chart(spec).with_context(|| format!("{at}.spec"))?;
            }
            "list" => {
                known(b, &at, &with(&["items"]))?;
                for (j, it) in items(100)?.into_iter().enumerate() {
                    let iat = format!("{at}.items[{j}]");
                    known(it, &iat, &["title", "sub", "icon", "tone", "badge", "bar", "value", "color"])?;
                    req_text(it, &iat, "title", 200)?;
                    opt_text(it, &iat, "sub", 300)?;
                    opt_text(it, &iat, "badge", 40)?;
                    opt_icon(it, &iat)?;
                    one_of(it, &iat, "tone", BOARD_TONES, false)?;
                    opt_range(it, &iat, "bar", 0.0, 1.0)?;
                    opt_short_value(it, &iat)?;
                    opt_color(it, &iat)?;
                }
            }
            "meters" => {
                known(b, &at, &with(&["note", "items"]))?;
                opt_text(b, &at, "note", 300)?;
                for (j, it) in items(20)?.into_iter().enumerate() {
                    let iat = format!("{at}.items[{j}]");
                    known(it, &iat, &["label", "value", "max", "text", "of", "mark", "markLabel", "tone", "color"])?;
                    req_text(it, &iat, "label", 100)?;
                    opt_number(it, &iat, "value")?.with_context(|| format!("{iat} needs a number `value`"))?;
                    let max = opt_number(it, &iat, "max")?.with_context(|| format!("{iat} needs a number `max`"))?;
                    ensure!(max > 0.0, "{iat} `max` must be more than 0");
                    for (key, n) in [("text", 40), ("of", 60), ("markLabel", 100)] { opt_text(it, &iat, key, n)?; }
                    opt_number(it, &iat, "mark")?;
                    one_of(it, &iat, "tone", BOARD_TONES, false)?;
                    opt_color(it, &iat)?;
                }
            }
            "callout" => {
                known(b, &at, &with(&["tone", "icon", "text"]))?;
                req_text(b, &at, "text", 1000)?;
                one_of(b, &at, "tone", &["good", "warn", "bad", "info"], false)?;
                opt_icon(b, &at)?;
            }
            _ => {
                known(b, &at, &with(&["items"]))?;
                for (j, a) in items(8)?.into_iter().enumerate() {
                    let aat = format!("{at}.items[{j}]");
                    known(a, &aat, &["id", "label", "primary", "danger", "confirm", "icon", "key", "data"])?;
                    let id = a.get("id").and_then(Value::as_str).with_context(|| format!("{aat} needs a string `id` (the event kind it sends)"))?;
                    ensure!(event_kind_ok(id) && !["closed", "overflow", "navigated"].contains(&id),
                        "{aat}.id must start with a letter and use letters, digits, _, - or / (at most 80), and not be closed, overflow or navigated (got {id:?})");
                    ensure!(!ids.contains(&id), "{aat}.id {id:?} is used twice");
                    ids.push(id);
                    let label = req_text(a, &aat, "label", 60)?;
                    for key in ["primary", "danger"] { opt_bool(a, &aat, key)?; }
                    opt_text(a, &aat, "confirm", 200)?;
                    opt_icon(a, &aat)?;
                    if let Some(k) = a.get("key").filter(|v| !v.is_null()) {
                        let k = k.as_str().filter(|k| k.len() == 1 && k.bytes().all(|c| c.is_ascii_alphanumeric()))
                            .with_context(|| format!("{aat}.key must be one letter or digit"))?.to_ascii_lowercase();
                        ensure!(!keys.contains(&k), "{aat}.key {k:?} is used twice");
                        keys.push(k);
                    }
                    if let Some(d) = a.get("data").filter(|v| !v.is_null()) {
                        let d = object(d, &format!("{aat}.data"))?;
                        ensure!(d.len() <= 10, "{aat}.data has more than 10 fields");
                        for (k, v) in d {
                            ensure!((1..=31).contains(&k.len()) && k.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
                                && k.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                                && !["send", "text", "confirm", "state", "sent", "busy", "key"].contains(&k.as_str()),
                                "{aat}.data.{k}: names must be lowercase letters, digits or _ (at most 31), and not one of the kit's own");
                            ensure!(v.is_boolean() || v.is_number() || v.as_str().is_some_and(|s| s.chars().count() <= 500),
                                "{aat}.data.{k} must be a string (at most 500 characters), number or true/false");
                        }
                    }
                    // The event a press sends (web/src/panels/kit/board.ts) must fit the event limit.
                    let data = a.get("data").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| serde_json::json!({}));
                    let event = serde_json::json!({"role": "user", "content": {"type": "text", "text": format!("Pressed \"{label}\"")},
                        "_meta": {"muster": {"kind": id, "data": data}}});
                    let size = serde_json::to_vec(&event)?.len();
                    ensure!(size <= crate::panels::MAX_EVENT, "{aat} would send an event of {size} bytes; at most 16 KiB (shorten its data)");
                }
            }
        }
    }
    Ok(())
}

/// An event kind as the panel bridge accepts it (web/src/panels/bridge.ts).
fn event_kind_ok(k: &str) -> bool {
    (1..=80).contains(&k.len()) && k.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
        && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'/')
}

/// Everything the table app checks (web/src/panels/apps/table/logic.ts). `scope`, `takeaway`,
/// `source` and a column's `format` and `tones` are optional additions for the Briefing look.
pub fn check_table(data: &Value) -> anyhow::Result<()> {
    let o = object(data, "table data")?;
    for (key, max) in [("title", 200), ("scope", 200), ("takeaway", 600), ("source", 300)] { opt_text(o, "table", key, max)?; }
    let columns = field(o, "table", "columns")?.as_array().context("table `columns` must be an array")?;
    ensure!((1..=50).contains(&columns.len()), "table `columns` needs 1 to 50 columns");
    let mut keys = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let at = format!("table columns[{i}]");
        let c = object(c, &at)?;
        let key = c.get("key").and_then(Value::as_str).filter(|k| !k.trim().is_empty() && k.chars().count() <= 200)
            .with_context(|| format!("each table column needs a string `key` of 1 to 200 characters ({at})"))?;
        ensure!(!keys.contains(&key), "{at} `key` duplicates {key:?}");
        keys.push(key);
        opt_text(c, &at, "label", 200)?;
        one_of(c, &at, "align", &["left", "right", "center"], false)?;
        one_of(c, &at, "format", &["bar", "badge", "mono"], false)?;
        if let Some(tones) = c.get("tones").filter(|v| !v.is_null()) {
            let tones = object(tones, &format!("{at} `tones`"))?;
            ensure!(tones.len() <= 50, "{at} `tones` has {} values; at most 50", tones.len());
            for (value, tone) in tones {
                ensure!(value.chars().count() <= 200, "{at} `tones` keys must be cell values of at most 200 characters");
                ensure!(tone.as_str().is_some_and(|t| ["good", "warn", "bad", "info", "accent", "plain"].contains(&t)),
                    "{at} `tones.{value}` must be one of good, warn, bad, info, accent, plain (got {tone})");
            }
        }
    }
    if let Some(sort) = o.get("sort").filter(|v| !v.is_null()) {
        let sort = object(sort, "table `sort`")?;
        let key = req_text(sort, "table sort", "key", 200)?;
        ensure!(keys.contains(&key), "table `sort.key` is not a column key ({key:?})");
        one_of(sort, "table sort", "dir", &["asc", "desc"], false)?;
    }
    let rows = field(o, "table", "rows")?.as_array().context("table `rows` must be an array")?;
    ensure!(rows.len() <= 5000, "table `rows` has more than 5000 rows");
    for (i, row) in rows.iter().enumerate() {
        let row = row.as_object().context("each table row must be an object")?;
        for (k, v) in row {
            ensure!(v.is_null() || v.is_boolean() || v.is_number() || v.as_str().is_some_and(|s| s.chars().count() <= 10_000),
                "table `rows[{i}].{k}` must be a string (at most 10000 characters), number, true, false or null");
        }
    }
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
pub fn read_image(cwd: &Path, path: &str) -> anyhow::Result<(&'static str, Vec<u8>)> { read_image_max(cwd, path, MAX_IMAGE) }

/// [`read_image`], refusing a file over `max` bytes before reading it.
pub fn read_image_max(cwd: &Path, path: &str, max: usize) -> anyhow::Result<(&'static str, Vec<u8>)> {
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
    let limit = |max: usize| if max == MAX_IMAGE { "8 MiB".to_string() } else { format!("{max} bytes") };
    ensure!(meta.len() <= max as u64, "image is larger than {}", limit(max));
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        ensure!(actual.starts_with(&root), "image path escaped the task worktree");
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "image is larger than {}", limit(max));
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
            (with("checks", json!({"state":"passing","items":[{"name":"t","state":"SUCCESS"}]})), "checks.items[0] `state`"),
            (with("changes", json!([{"file":"a","note":"n"}])), "diff"), (with("status", json!("done")), "queued"),
            (with("merge_method_default", json!("ff")), "squash"), (json!([1]), "object"),
            (with("title", json!("x".repeat(501))), "`title` is longer than 500"), (with("summary", json!(" ")), "must not be empty"),
            (with("questions", json!([1])), "`questions[0]` must be a string"), (with("risk", json!({"level":"low","reasons":["x".repeat(2001)]})), "reasons[0]"),
            (with("reviews", json!({"items":[{"author":"lee"}]})), "reviews.items[0] data needs `state`"),
            (with("answer", json!({"question":"q"})), "answer data needs `text`"), (with("delete_branch_default", json!("yes")), "true or false"),
            (with("checks", json!({"state":"passing","items":[{"name":"t","state":"pass","url":"http://x"}]})), "https"),
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
        check_table(&json!({"title":"Flaky tests","scope":"acme","takeaway":"One is **bad**.","source":"junit","columns":[{"key":"a","format":"bar"},
            {"key":"s","format":"badge","tones":{"Flaky":"warn","Fixed":"good"}},{"key":"f","format":"mono"}],"rows":[{"a":1,"s":"Flaky","f":"x.ts"}]})).unwrap();
        assert!(check_table(&json!({"columns":[],"rows":[]})).is_err());
        assert!(check_table(&json!({"columns":[{"label":"x"}],"rows":[]})).unwrap_err().to_string().contains("key"));
        assert!(check_table(&json!({"columns":[{"key":"a"}],"rows":[1]})).unwrap_err().to_string().contains("object"));
        for (t, why) in [(json!({"columns":[{"key":"a"},{"key":"a"}],"rows":[]}), "duplicates"),
            (json!({"columns":[{"key":"a"}],"rows":[{"a":[1]}]}), "rows[0].a"), (json!({"columns":[{"key":"a"}],"rows":[{"a":"x".repeat(10_001)}]}), "10000"),
            (json!({"columns":[{"key":"a"}],"rows":[],"sort":{"key":"b"}}), "not a column key"), (json!({"columns":[{"key":"a","align":"middle"}],"rows":[]}), "align"),
            (json!({"columns":[{"key":"a","format":"sparkline"}],"rows":[]}), "`format` must be one of bar, badge, mono"),
            (json!({"columns":[{"key":"a","format":"badge","tones":{"Flaky":"orange"}}],"rows":[]}), "`tones.Flaky` must be one of good, warn"),
            (json!({"columns":[{"key":"a","tones":["warn"]}],"rows":[]}), "`tones` must be an object"),
            (json!({"columns":[{"key":"a"}],"rows":[],"takeaway":"x".repeat(601)}), "`takeaway` is longer than 600")] {
            let e = format!("{:#}", check_table(&t).unwrap_err());
            assert!(e.contains(why), "{t}: {e}");
        }
        for (c, why) in [(json!({"type":"scatter","x":{"values":["a"]},"series":[{"name":"s","values":[1]}]}), "scatter"),
            (json!({"type":"bar","x":{"values":["x".repeat(201)]},"series":[{"name":"s","values":[1]}]}), "200"),
            (json!({"type":"bar","x":{"values":[1]},"series":[{"name":"","values":[1]}]}), "name"),
            (json!({"type":"bar","x":{"values":[1]},"series":[{"name":"s","values":[1]}],"y":{"min":5,"max":1}}), "y.min")] {
            let e = format!("{:#}", check_chart(&c).unwrap_err());
            assert!(e.contains(why), "{c}: {e}");
        }
        assert!(date_ok("2026-10-18T09:30:00Z") && date_ok("2026-10-18T09:30:00.123+02:00") && !date_ok("2026-10-18T09:30Z") && !date_ok("2026-10-18 09:30:00Z"));
    }

    fn board_sample() -> Value {
        json!({"title":"$4,480 spent so far this month","scope":"acme cloud · October","takeaway":"On track to finish about 6% under budget.",
            "min":300,"source":"billing export, 1–20 Oct","blocks":[
            {"kind":"stats","span":"full","items":[{"label":"Month to date","value":"$4,480","delta":"+4.1%","tone":"bad","note":"vs September"},
                {"label":"Per day","value":224,"trend":[205,212,219]}]},
            {"kind":"chart","span":"full","title":"Spend per week","aside":"by service","height":220,"spec":{"type":"bar","stacked":true,"format":"usd",
                "x":{"values":["Oct 6","Oct 13"]},"series":[{"name":"Postgres","values":[651,669]},{"name":"Compute","values":[543,539]}]}},
            {"kind":"list","title":"By service","items":[{"title":"Postgres","sub":"db-primary","bar":0.43,"value":"$1,906","icon":"db","tone":"warn","badge":"Growing","color":"var(--mk-c2)"}]},
            {"kind":"meters","title":"Budgets","note":"The tick is where spend should be today.","items":[{"label":"Whole account","value":4480,"max":7400,"text":"$4,480","of":"of $7,400","mark":4774,"markLabel":"pace","tone":"warn"}]},
            {"kind":"callout","tone":"warn","title":"Postgres is ahead of pace.","text":"It ends the month about $50 over budget."},
            {"kind":"actions","items":[{"id":"explain","label":"Why did Postgres grow?","primary":true,"icon":"sparkle","key":"w"},
                {"id":"rollback/prod","label":"Roll back","danger":true,"confirm":"Roll back production?","data":{"env":"production","to":"v2.3.2","dry_run":false}}]}]})
    }

    #[test]
    fn boards_are_checked_naming_the_field() {
        check_board(&board_sample()).unwrap();
        check_board(&json!({"title":"t","blocks":[{"kind":"callout","text":"Nothing yet."}]})).unwrap();
        let with = |f: &dyn Fn(&mut Value)| { let mut d = board_sample(); f(&mut d); d };
        for (data, why) in [
            (json!({"blocks":[{"kind":"callout","text":"x"}]}), "board data needs `title`"),
            (json!({"title":"t","blocks":[]}), "board `blocks` needs at least 1 block"),
            (json!({"title":"t"}), "board data needs `blocks`"),
            (with(&|d| d["blocks"][0]["kind"] = json!("table")), "blocks[0] `kind` must be one of stats, chart, list, meters, callout, actions"),
            (with(&|d| d["blocks"][0]["span"] = json!(2)), "blocks[0] `span` must be \"full\""),
            (with(&|d| d["titel"] = json!("typo")), "unknown field `titel`"),
            (with(&|d| d["min"] = json!(50)), "`min` must be from 160 to 1200"),
            (with(&|d| d["blocks"][0]["items"][0]["tone"] = json!("great")), "items[0] `tone` must be one of good, bad, flat"),
            (with(&|d| d["blocks"][0]["items"] = json!([])), "blocks[0] `items` needs at least 1 item"),
            (with(&|d| d["blocks"][1]["spec"]["type"] = json!("pie")), "blocks[1].spec"),
            (with(&|d| d["blocks"][1]["height"] = json!(5000)), "`height` must be from 80 to 800"),
            (with(&|d| d["blocks"][2]["items"][0]["bar"] = json!(1.5)), "`bar` must be from 0 to 1"),
            (with(&|d| d["blocks"][2]["items"][0]["color"] = json!("red; background: url(x)")), "`color` must be a kit colour token"),
            (with(&|d| d["blocks"][2]["items"][0]["icon"] = json!("<svg>")), "`icon` must be an icon name"),
            (with(&|d| d["blocks"][3]["items"][0]["max"] = json!(0)), "`max` must be more than 0"),
            (with(&|d| d["blocks"][3]["items"][0]["value"] = json!("4480")), "`value` must be a number"),
            (with(&|d| d["blocks"][4]["text"] = Value::Null), "blocks[4] data needs `text`"),
            (with(&|d| d["blocks"][5]["items"][1]["id"] = json!("closed")), "items[1].id must start with a letter"),
            (with(&|d| d["blocks"][5]["items"][1]["id"] = json!("explain")), "items[1].id \"explain\" is used twice"),
            (with(&|d| d["blocks"][5]["items"][1]["key"] = json!("W")), "items[1].key \"w\" is used twice"),
            (with(&|d| d["blocks"][5]["items"][1]["data"] = json!({"send":"x"})), "data.send: names must be lowercase"),
            (with(&|d| d["blocks"][5]["items"][1]["data"] = json!({"env":{"a":1}})), "data.env must be a string"),
        ] {
            let e = format!("{:#}", check_board(&data).unwrap_err());
            assert!(e.contains(why), "{data}: {e}");
        }
    }

    /// The same inputs pass or fail in the app (web/src/panels/apps/board/logic_test.ts reads
    /// this file too).
    #[test]
    fn boards_pass_or_fail_here_as_in_the_app() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("../web/src/panels/apps/board/parity.json")).unwrap();
        for c in cases {
            let r = check_board(&c["data"]);
            assert_eq!(r.is_ok(), c["ok"].as_bool().unwrap(), "{}: {:?}", c["name"], r.err());
        }
        let e = format!("{:#}", check_board(&json!({"title":"t","blocks":[{"kind":"actions","items":[{"id":"go","label":"Go",
            "data": (0..10).map(|i| (format!("k{i}"), json!("😀".repeat(500)))).collect::<serde_json::Map<_, _>>()}]}]})).unwrap_err());
        assert!(e.contains("items[0] would send an event of") && e.contains("at most 16 KiB"), "{e}");
    }

    #[test]
    fn chart_briefing_fields_are_optional_and_checked() {
        // Every field the Briefing look added is optional: the old shape still passes, and so does no data at all.
        check_chart(&chart_sample()).unwrap();
        check_chart(&json!({"type":"bar","x":{"values":[]},"series":[],"empty":"No deploys in this range"})).unwrap();
        let full = json!({"type":"bar","x":{"values":["Mon","Tue"]},"series":[{"name":"master","values":[42,null],"color":"var(--mk-c1)"},{"name":"PRs","values":[80,96],"color":"#41c7c1"}],
            "takeaway":"PR builds used **1.75×** the minutes.","scope":"acme-store · week of Oct 13","source":"gh run list","format":"int","xFormat":"plain",
            "highlight":"PRs","annotations":[{"x":"Tue","label":"v2.4"}],"stacked":true,"labels":"values","empty":"none yet","pin":1,
            "stats":[{"label":"master","value":287,"unit":"min","delta":"−12%","tone":"good","note":"vs last week","trend":[1,2,3],"color":"var(--mk-c1)"}]});
        check_chart(&full).unwrap();
        let with = |f: &dyn Fn(&mut Value)| { let mut d = full.clone(); f(&mut d); d };
        for (c, why) in [
            (with(&|d| d["format"] = json!("eur")), "`format` must be one of int, pct, usd, ms, s, plain"),
            (with(&|d| d["xFormat"] = json!(1)), "`xFormat` must be one of"),
            (with(&|d| d["highlight"] = json!("nope")), "`highlight` must be the name of a series"),
            (with(&|d| d["annotations"] = json!([{"x":"Sun","label":"x"}])), "annotations[0] `x` must be one of x.values"),
            (with(&|d| d["annotations"] = json!([{"x":"Mon"}])), "annotations[0] data needs `label`"),
            (with(&|d| d["stacked"] = json!("yes")), "`stacked` must be true or false"),
            (with(&|d| d["type"] = json!("line")), "`stacked` applies to bar and area"),
            (with(&|d| d["trend"] = json!(true)), "`trend` applies to scatter"),
            (with(&|d| d["labels"] = json!("some")), "`labels` must be one of auto, values, end, none"),
            (with(&|d| d["series"][0]["color"] = json!("red;background:url(x)")), "series[0] `color` must be a kit colour token"),
            (with(&|d| d["stats"][0]["tone"] = json!("great")), "stats[0] `tone` must be one of good, bad, flat"),
            (with(&|d| d["stats"][0]["value"] = Value::Null), "stats[0] needs `value`"),
            (with(&|d| d["stats"] = json!([{"label":"a","value":1},{"label":"a","value":1},{"label":"a","value":1},{"label":"a","value":1},{"label":"a","value":1}])), "at most 4"),
            (with(&|d| d["pin"] = json!(5)), "`pin` must be the index of an x value"),
            (with(&|d| d["takeaway"] = json!("x".repeat(601))), "`takeaway` is longer than 600"),
        ] {
            let e = format!("{:#}", check_chart(&c).unwrap_err());
            assert!(e.contains(why), "{c}: {e}");
        }
        for ok in ["#fff", "#6a56e6", "var(--mk-c2)", "var(--mk-add)"] { assert!(color_ok(ok), "{ok}"); }
        for bad in ["red", "#ggg", "var(--x)", "var(--mk-c2);color:red", "url(x)", "#12345", "var(--mk-)"] { assert!(!color_ok(bad), "{bad}"); }
    }

    #[test]
    fn overview_and_actions_are_checked_naming_the_field() {
        check_overview(&overview_sample()).unwrap();
        check_overview(&json!({"repo":"a/b","prs":[]})).unwrap();
        check_overview(&json!({"repo":"a/b","prs":[],"takeaway":"Six open. **One** is a quick win."})).unwrap();
        assert!(check_overview(&json!({"repo":"a/b","prs":[],"takeaway":"x".repeat(601)})).unwrap_err().to_string().contains("`takeaway` is longer than 600"));
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
            // Everything the app would refuse is refused here, with the same limits.
            (with(&|d| d["prs"][0]["summary"] = json!("x".repeat(501))), "prs[0] `summary` is longer than 500"),
            (with(&|d| d["prs"][0]["title"] = json!("😀".repeat(501))), "title is longer than 500"),
            (with(&|d| d["prs"][0]["created_at"] = json!("last week")), "created_at must be a date"),
            (with(&|d| d["prs"][0]["created_at"] = json!("2026-10-15")), "created_at must be a date"),
            (with(&|d| d["prs"][0]["labels"] = json!([""])), "labels must be at most 20 label names"),
            (with(&|d| d["title"] = json!("x".repeat(201))), "`title` is longer than 200"),
            (json!({"repo":"a/b","prs":(1..=201).map(|n| json!({"number":n,"title":"t","author":"a"})).collect::<Vec<_>>()}), "at most 200"),
        ] {
            let e = format!("{:#}", check_overview(&data).unwrap_err());
            assert!(e.contains(why), "{data}: {e}");
        }
        // Actions: the same rules in both PR apps.
        let action = |a: Value| { let mut d = pr_sample(); d["actions"] = json!([a]); d };
        let ok = json!({"id":"later","label":"Label later","description":"Remember it.","key":"L","outward":false});
        check_pr(&action(ok.clone())).unwrap();
        for k in RESERVED_KEYS.chars() {
            let e = check_pr(&action(json!({"id":"x","label":"X","description":"d","outward":false,"key":k.to_ascii_uppercase().to_string()}))).unwrap_err().to_string();
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
            (json!({"id":"x","label":"X","description":"d","outward":"yes"}), "outward is required: true if the action changes GitHub"),
            (json!({"id":"x","label":"X","description":"d"}), "outward is required"),
            (json!({"id":"x","label":"X","description":"d","outward":null}), "outward is required"),
            (json!({"id":"x","label":" Hold ","description":"d","outward":false}), "looks like a built-in button"),
            (json!({"id":"x","label":"MERGE","description":"d","outward":true}), "looks like a built-in button"),
            (json!({"id":"x","label":"Open on  GitHub…","description":"d","outward":false}), "looks like a built-in button"),
            (json!({"id":"x","label":"Request changes.","description":"d","outward":false}), "looks like a built-in button"),
            (json!({"id":"x","label":"😀".repeat(41),"description":"d","outward":false}), "label must be 1 to 40"),
            (json!(7), "`actions[0]` must be an object"),
        ] {
            let e = format!("{:#}", check_pr(&action(a.clone())).unwrap_err());
            assert!(e.contains(why), "{a}: {e}");
        }
        let mut twice = pr_sample();
        twice["actions"] = json!([ok, {"id":"later","label":"Y","description":"d","outward":false}]);
        assert!(check_pr(&twice).unwrap_err().to_string().contains("used twice"));
        twice["actions"] = json!([{"id":"a","label":"A","description":"d","key":"x","outward":false}, {"id":"b","label":"B","description":"d","key":"X","outward":false}]);
        assert!(check_pr(&twice).unwrap_err().to_string().contains("\"x\" is used twice"));
        twice["actions"] = json!((0..9).map(|i| json!({"id":format!("a{i}"),"label":"A","description":"d","outward":false})).collect::<Vec<_>>());
        assert!(check_pr(&twice).unwrap_err().to_string().contains("at most 8"));
        // Lengths count characters (code points), as the app does: 40 emoji fit, 41 do not.
        twice["actions"] = json!([{"id":"e","label":"😀".repeat(40),"description":"🚀".repeat(300),"outward":false}]);
        check_pr(&twice).unwrap();
        // Labels that only resemble a built-in are fine.
        twice["actions"] = json!([{"id":"h","label":"Hold for QA","description":"d","outward":false}]);
        check_pr(&twice).unwrap();
        let mut o = overview_sample();
        o["actions"][0]["key"] = json!("j");
        assert!(check_overview(&o).unwrap_err().to_string().contains("pr_overview `actions[0]`.key \"j\" is a built-in key"));
    }

    #[test]
    fn questions_verdict_undo_and_toggles_are_checked_like_the_app() {
        let with = |f: &dyn Fn(&mut Value)| { let mut d = pr_sample(); f(&mut d); d };
        let good = with(&|d| {
            d["verdict"] = json!("Safe to merge: small bug fix, tests pass.");
            d["undo"] = json!("If this goes wrong: revert the merge commit.");
            d["questions"] = json!([
                {"id":"limit","text":"Keep 5 per 10 minutes?","why":"Support asked for 10 per hour.","kind":"yes_no","recommended":"yes","required":true},
                {"id":"proxy","text":"Which proxies?","kind":"choice","options":[{"label":"Only ours","value":"ours"},{"label":"Any","value":"any"}],"recommended":"ours"},
                {"id":"note","text":"Anything for the author?","kind":"text","recommended":"Nice work"},
                "An old plain question", " "]);
            d["actions"] = json!([{"id":"qa","label":"Needs QA","description":"Add the label.","outward":true,"mode":"toggle"},
                {"id":"ping","label":"Ping","description":"Tell them.","outward":true,"mode":"now"}]);
        });
        check_pr(&good).unwrap();
        // 20 questions of 500 emoji fit (characters, as the app counts).
        check_pr(&with(&|d| d["questions"] = json!((0..20).map(|i| json!({"id":format!("q{i}x"),"text":"😀".repeat(500),"kind":"text"})).collect::<Vec<_>>()))).unwrap();
        let q = |patch: Value| with(&|d| {
            let mut base = json!({"id":"proxy","text":"Which proxies?","kind":"choice","options":[{"label":"Only ours","value":"ours"},{"label":"Any","value":"any"}]});
            for (k, v) in patch.as_object().unwrap() { if v.is_null() { base.as_object_mut().unwrap().remove(k); } else { base[k] = v.clone(); } }
            d["questions"] = json!([base]);
        });
        for (data, why) in [
            (with(&|d| d["verdict"] = json!("x".repeat(301))), "`verdict` is longer than 300"),
            (with(&|d| d["undo"] = json!("revert\nthen redeploy")), "`undo` must be one line"),
            (with(&|d| d["verdict"] = json!(5)), "`verdict` must be a string"),
            (with(&|d| d["questions"] = json!([1])), "`questions[0]` must be a string or an object"),
            (with(&|d| d["questions"] = json!(["x".repeat(2001)])), "`questions[0]` is longer than 2000"),
            (with(&|d| d["questions"] = json!((0..21).map(|i| json!(format!("q{i}"))).collect::<Vec<_>>())), "has 21 items; at most 20"),
            (with(&|d| d["questions"] = json!(["a", {"id":"q1","text":"t","kind":"text"}])), "\"q1\" is used twice"),
            (q(json!({"id":"Bad Id"})), "`questions[0]`.id must be lowercase"),
            (q(json!({"kind":"multi"})), "questions[0] `kind` must be one of yes_no, choice, text"),
            (q(json!({"kind":null})), "questions[0] data needs `kind`"),
            (q(json!({"text":"x".repeat(501)})), "questions[0] `text` is longer than 500"),
            (q(json!({"text":" "})), "questions[0] `text` must not be empty"),
            (q(json!({"why":"x".repeat(301)})), "questions[0] `why` is longer than 300"),
            (q(json!({"options":[{"label":"A","value":"a"}]})), "options needs at least 2"),
            (q(json!({"options":null})), "questions[0] data needs `options`"),
            (q(json!({"options":(0..10).map(|i| json!({"label":"L","value":format!("v{i}")})).collect::<Vec<_>>()})), "has 10 items; at most 9"),
            (q(json!({"options":[{"label":"A","value":"a"},{"label":"B","value":"a"}]})), "`value` \"a\" is used twice"),
            (q(json!({"options":[{"label":"x".repeat(81),"value":"a"},{"label":"B","value":"b"}]})), "`label` is longer than 80"),
            (q(json!({"recommended":"maybe"})), "recommended must be one of the option values"),
            (q(json!({"kind":"yes_no","options":null,"recommended":"Yes"})), "must be one of the option values (\"yes\", \"no\")"),
            (q(json!({"kind":"yes_no"})), "a yes_no question has its own Yes and No"),
            (q(json!({"kind":"text"})), "a text question takes no options"),
            (q(json!({"kind":"text","options":null,"recommended":"x".repeat(1001)})), "recommended must be 1 to 1000"),
            (q(json!({"required":"yes"})), "`required` must be true or false"),
            (with(&|d| d["actions"] = json!([{"id":"qa","label":"QA","description":"d","outward":false,"mode":"later"}])), "`mode` must be one of toggle, now"),
            (with(&|d| d["actions"] = json!([{"id":"answers","label":"QA","description":"d","outward":false}])), "is a built-in action"),
            (with(&|d| d["actions"] = json!([{"id":"x","label":"Send answers","description":"d","outward":false}])), "looks like a built-in button"),
        ] {
            let e = format!("{:#}", check_pr(&data).unwrap_err());
            assert!(e.contains(why), "{why}: {e}");
        }
        // Quick wins in the overview: only with what the merge is built from, green, not a draft.
        let win = json!({"number":815,"title":"Typo","author":"lee","checks":"passing","risk":"low","quick_win":true,"status":"ready",
            "head_sha":"0123456789abcdef0123456789abcdef01234567","base":"master","verdict":"Safe to merge: one word in the docs.",
            "merge_method_default":"rebase","delete_branch_default":true});
        check_overview(&json!({"repo":"a/b","prs":[win.clone()]})).unwrap();
        let card = |patch: Value| { let mut w = win.clone(); for (k, v) in patch.as_object().unwrap() {
            if v.is_null() { w.as_object_mut().unwrap().remove(k); } else { w[k] = v.clone(); } } json!({"repo":"a/b","prs":[w]}) };
        check_overview(&card(json!({"quick_win":false,"verdict":null,"base":null,"head_sha":null,"checks":"failing"}))).unwrap();
        for (data, why) in [
            (card(json!({"head_sha":null})), ".head_sha is required for a quick win"), (card(json!({"base":null})), ".base is required for a quick win"),
            (card(json!({"verdict":null})), ".verdict is required for a quick win"), (card(json!({"verdict":" "})), ".verdict is required for a quick win"),
            (card(json!({"verdict":"a\nb"})), "`verdict` must be one line"), (card(json!({"checks":"pending"})), "quick_win needs checks passing"),
            (card(json!({"draft":true})), "quick_win cannot be a draft"), (card(json!({"base":"ma ster"})), ".base is not a valid branch name"),
            (card(json!({"merge_method_default":"ff"})), "`merge_method_default` must be one of squash"), (card(json!({"quick_win":"yes"})), "`quick_win` must be true or false"),
            (card(json!({"status":"done"})), "`status` must be one of new, ready"),
        ] {
            let e = format!("{:#}", check_overview(&data).unwrap_err());
            assert!(e.contains(why), "{why}: {e}");
        }
    }

    #[test]
    fn pr_media_pictures_are_read_like_the_image_app_and_charts_checked() {
        let (root, cwd) = scratch("media");
        std::fs::write(cwd.join("shots/a.png"), PNG).unwrap();
        std::fs::write(root.join("outside.png"), PNG).unwrap();
        let mut csp = PanelCsp::default();
        let mut d = pr_sample();
        d["media"] = json!([{"kind":"image","path":"shots/a.png","alt":"After","caption":"Login"},
            {"kind":"chart","spec":chart_sample(),"caption":"Lines per area"}]);
        let stored: Value = serde_json::from_slice(&data("pr_decision", &cwd, d.clone(), &mut csp, true).unwrap()).unwrap();
        let m = &stored["media"];
        assert_eq!(m[0]["kind"], "image");
        assert_eq!(m[0]["src"], format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(PNG)));
        assert_eq!((m[0]["bytes"].as_u64(), m[0]["alt"].as_str()), (Some(PNG.len() as u64), Some("After")));
        assert_eq!((m[1]["kind"].as_str(), m[1]["caption"].as_str()), (Some("chart"), Some("Lines per area")));
        assert!(csp.resource_domains.is_empty(), "media never adds origins");
        assert_eq!(stored["repo"], "acme/api");
        // Without media the input is stored as given.
        assert_eq!(serde_json::from_slice::<Value>(&data("pr_decision", &cwd, pr_sample(), &mut csp, true).unwrap()).unwrap(), pr_sample());
        let bad = |media: Value| { let mut d = pr_sample(); d["media"] = media; format!("{:#}", data("pr_decision", &cwd, d, &mut PanelCsp::default(), true).unwrap_err()) };
        for (media, why) in [
            (json!([{"kind":"image","path":"../outside.png"}]), "inside"),
            (json!([{"kind":"image","path":"shots/a.png","src":"data:image/png;base64,AA"}]), "pr_decision `media[0]` takes path, plus alt and caption (unknown field `src`)"),
            (json!([{"kind":"image"}]), "pr_decision `media[0]` needs path"),
            (json!([{"kind":"image","url":"https://user-images.githubusercontent.com/1/a.png"}]), "takes a path only: download the picture"),
            (json!([{"kind":"image","path":"shots/a.png","url":"https://x.example/a.png"}]), "takes a path only"),
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
        assert!(e.contains("at media[2]"), "the third picture is refused before it is read: {e}");
        assert!(read_image_max(&cwd, "shots/big0.png", 1024).unwrap_err().to_string().contains("larger than 1024 bytes"));
        let mut huge = pr_sample();
        huge["changes"] = json!((0..11).map(|_| json!({"file":"a","note":"n","diff":"x".repeat(100_000)})).collect::<Vec<_>>());
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
        let rows: Vec<Value> = (0..120).map(|_| json!({"a": "y".repeat(9000)})).collect();
        let big = data("table", &cwd, json!({"columns":[{"key":"a"}],"rows":rows}), &mut csp, true);
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
            data: Some(data), html: None, html_path: None, csp: None, kit: None });
        let id = open("image", json!({"path": rel})).unwrap();
        let res = crate::panels::resource(&hub, id).unwrap();
        assert!(res["data"]["src"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(res["html"], crate::panel_kit::serve("image", false, IMAGE.to_string()));
        let web = open("image", json!({"url": "https://img.example/a.png"})).unwrap();
        assert_eq!(crate::panels::get(&hub, web).unwrap().csp.resource_domains, vec!["https://img.example"]);
        let update = |id, data| crate::panels::update(&hub, 1, Update { panel_id: id, title: None, data: Some(data), html: None, html_path: None, kit: None });
        update(web, json!({"url": "https://img.example/b.png", "caption": "B"})).unwrap();
        assert!(update(web, json!({"url": "https://evil.example/b.png"})).is_err());
        assert!(update(id, json!({"path": "../etc/passwd"})).is_err());
        for (app, data) in [("chart", chart_sample()), ("table", json!({"columns":[{"key":"a"}],"rows":[]})), ("pr_decision", pr_sample()),
            ("pr_overview", overview_sample())] {
            let id = open(app, data.clone()).unwrap();
            assert_eq!(crate::panels::resource(&hub, id).unwrap()["html"], crate::panel_kit::serve(app, false, html(app).unwrap().to_string()));
            // A malformed input fails the agent's own call, open or update.
            let e = open(app, json!({})).unwrap_err().to_string();
            assert!(e.contains(&format!("{app} data needs")), "{e}");
            assert!(update(id, json!({"x": 1})).is_err());
        }
        assert!(open("diff", json!({})).unwrap_err().to_string().contains("pr_decision"));
        assert!(crate::panels::open(&hub, 1, Open { title: "x".into(), app: "chart".into(), data: None,
            html: Some("<p>".into()), html_path: None, csp: None, kit: None }).unwrap_err().to_string().contains("does not accept custom HTML"));
        crate::panels::archive(&hub, 1).unwrap();
        std::fs::remove_file(Path::new("/tmp").join(&rel)).unwrap();
    }
}
