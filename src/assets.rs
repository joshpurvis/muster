//! Static frontend (web/dist) embedded with rust-embed. Debug builds read from disk, so a
//! rebuilt frontend shows up without recompiling the daemon. Unknown extension-less paths
//! fall back to index.html (SPA routing).

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[allow_missing = true]
struct Dist;

const NOT_BUILT: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Muster</title>
<style>body{font:14px system-ui,sans-serif;background:#151518;color:#e4e4e8;display:grid;place-items:center;height:100vh;margin:0}code{background:#222228;padding:2px 6px;border-radius:4px}</style>
</head><body><div><h2>Muster daemon is running</h2><p>The web UI has not been built yet. Build it with <code>cd web &amp;&amp; deno task build</code>, then reload.</p></div></body></html>"#;

/// Files keep their names across builds (`app.js`), so everything but `assets/` is served
/// `no-cache` with a content-hash ETag: a window reloaded after a rebuild always gets the new
/// frontend, and an unchanged one costs a 304.
fn file(path: &str, req: &HeaderMap) -> Option<Response> {
    let f = Dist::get(path)?;
    let etag = format!("\"{}\"", hex(&f.metadata.sha256_hash()[..16]));
    let fresh = req
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut resp = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, f.data.into_owned()).into_response()
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime_guess::from_path(path).first_or_octet_stream().as_ref()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    let cache = if path.starts_with("assets/") && !cfg!(debug_assertions) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Some(resp)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn index(req: &HeaderMap) -> Response {
    file("index.html", req).unwrap_or_else(|| {
        let mut r = axum::response::Html(NOT_BUILT).into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        r
    })
}

pub async fn serve(uri: Uri, req: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        return index(&req);
    }
    if path.split('/').any(|seg| seg == "..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(r) = file(path, &req) {
        return r;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') || path.starts_with("api/") || path.starts_with("hook/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    index(&req)
}
