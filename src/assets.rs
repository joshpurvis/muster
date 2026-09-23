//! Static frontend (web/dist) embedded with rust-embed. Debug builds read from disk, so a
//! rebuilt frontend shows up without recompiling the daemon. Unknown extension-less paths
//! fall back to index.html (SPA routing).

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[allow_missing = true]
struct Dist;

const NOT_BUILT: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Muster</title>
<style>body{font:14px system-ui,sans-serif;background:#151518;color:#e4e4e8;display:grid;place-items:center;height:100vh;margin:0}code{background:#222228;padding:2px 6px;border-radius:4px}</style>
</head><body><div><h2>Muster daemon is running</h2><p>The web UI has not been built yet. Build it with <code>cd web &amp;&amp; deno task build</code>, then reload.</p></div></body></html>"#;

fn file(path: &str) -> Option<Response> {
    let f = Dist::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = (StatusCode::OK, f.data.into_owned()).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    let cache = if cfg!(debug_assertions) || path == "index.html" {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Some(resp)
}

fn index() -> Response {
    file("index.html").unwrap_or_else(|| {
        let mut r = axum::response::Html(NOT_BUILT).into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        r
    })
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        return index();
    }
    if path.split('/').any(|seg| seg == "..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(r) = file(path) {
        return r;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') || path.starts_with("api/") || path.starts_with("hook/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    index()
}
