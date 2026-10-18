//! Static frontend served from an independent UI release, with embedded assets as the
//! default. An atomic `ui/current` symlink switch updates assets without restarting either
//! backend or terminal host. Debug embedded builds read web/dist from disk. Unknown extension-less paths
//! fall back to index.html (SPA routing).

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

/// Bump only for incompatible UI/application protocol changes, independently of host IPC.
pub const UI_PROTOCOL: u32 = 1;

enum Source {
    Embedded,
    Directory(PathBuf),
}

impl Source {
    fn directory(path: &Path) -> anyhow::Result<Self> {
        let root = std::fs::canonicalize(path)?;
        let manifest_path = std::fs::canonicalize(root.join("muster-ui.json"))?;
        anyhow::ensure!(
            manifest_path.starts_with(&root),
            "UI manifest escapes its release directory"
        );
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest_path)?)?;
        anyhow::ensure!(
            manifest["protocol"].as_u64() == Some(UI_PROTOCOL as u64),
            "UI release is incompatible with this application backend"
        );
        anyhow::ensure!(
            root.join("index.html").is_file(),
            "UI release has no index.html"
        );
        Ok(Self::Directory(root))
    }

    fn configured() -> anyhow::Result<Self> {
        if let Some(path) = std::env::var_os("MUSTER_WEB_DIR") {
            return Self::directory(Path::new(&path));
        }
        let path = crate::paths::data_dir().join("ui/current");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Self::directory(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Embedded),
            Err(error) => Err(error.into()),
        }
    }

    fn read(&self, path: &str) -> Option<(Vec<u8>, String)> {
        if !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        match self {
            Self::Embedded => {
                let file = Dist::get(path)?;
                let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()[..16]));
                Some((file.data.into_owned(), etag))
            }
            Self::Directory(root) => {
                let full = std::fs::canonicalize(root.join(path)).ok()?;
                if !full.starts_with(root) || !full.is_file() {
                    return None;
                }
                let data = std::fs::read(full).ok()?;
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                data.hash(&mut hash);
                Some((data, format!("\"external-{:016x}\"", hash.finish())))
            }
        }
    }
}

/// Stage a complete release before one atomic switch. Existing releases remain available
/// for explicit rollback, and no running application or host process is signalled.
pub fn install(source: &Path, data: &Path) -> anyhow::Result<PathBuf> {
    let Source::Directory(source) = Source::directory(source)? else {
        unreachable!()
    };
    for required in ["index.html", "app.js", "app.css"] {
        anyhow::ensure!(
            source.join(required).is_file(),
            "UI release is missing {required}"
        );
    }
    if source.join("m/index.html").exists() {
        for required in ["m/app.js", "m/app.css", "m/sw.js", "m/manifest.webmanifest"] {
            anyhow::ensure!(
                source.join(required).is_file(),
                "UI release is missing {required}"
            );
        }
    }
    let ui = data.join("ui");
    let releases = ui.join("releases");
    crate::paths::ensure_private_dir(&releases)?;
    let name = format!("{}-{:016x}", crate::model::now_ms(), rand::random::<u64>());
    let staged = releases.join(&name);
    // A release may be installed from a previous release, but never copy a directory
    // into itself. Symlinks inside a source tree are rejected below.
    let destination = std::fs::canonicalize(&releases)?;
    anyhow::ensure!(
        !destination.starts_with(&source),
        "UI destination lies inside the source release"
    );
    std::fs::create_dir(&staged)?;
    let link = ui.join(format!("next-{name}"));
    let result = (|| {
        copy_release(&source, &staged, 0)?;
        Source::directory(&staged)?;
        // Pin JS/CSS requests to this release, even if another update happens between
        // fetching HTML and fetching its assets. Keep the old release for open windows.
        for (html, prefix) in [
            ("index.html", format!("/_ui/{name}")),
            ("m/index.html", format!("/m/_ui/{name}")),
        ] {
            let path = staged.join(html);
            if !path.exists() {
                continue;
            }
            let original = std::fs::read_to_string(&path)?;
            let updated = original
                .replace("src=\"app.js\"", &format!("src=\"{prefix}/app.js\""))
                .replace("href=\"app.css\"", &format!("href=\"{prefix}/app.css\""));
            crate::paths::write_private(&path, updated.as_bytes())?;
        }
        std::os::unix::fs::symlink(Path::new("releases").join(&name), &link)?;
        std::fs::rename(&link, ui.join("current"))?;
        Ok(staged.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&staged);
    }
    result
}

fn copy_release(source: &Path, target: &Path, depth: usize) -> anyhow::Result<()> {
    anyhow::ensure!(depth < 16, "UI directory nesting is too deep");
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let output = target.join(entry.file_name());
        if kind.is_dir() {
            crate::paths::ensure_private_dir(&output)?;
            copy_release(&entry.path(), &output, depth + 1)?;
        } else {
            anyhow::ensure!(
                kind.is_file(),
                "UI release contains a symlink or special file: {}",
                entry.path().display()
            );
            std::fs::copy(entry.path(), &output)?;
            crate::paths::make_private_file(&output);
        }
    }
    Ok(())
}

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
fn file(source: &Source, path: &str, req: &HeaderMap) -> Option<Response> {
    let (data, etag) = source.read(path)?;
    let fresh = req
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut resp = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, data).into_response()
    };
    let h = resp.headers_mut();
    if let Ok(v) =
        HeaderValue::from_str(mime_guess::from_path(path).first_or_octet_stream().as_ref())
    {
        h.insert(header::CONTENT_TYPE, v);
    }
    let cache = if matches!(source, Source::Embedded)
        && path.starts_with("assets/")
        && !cfg!(debug_assertions)
    {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Some(resp)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn index(source: &Source, req: &HeaderMap) -> Response {
    file(source, "index.html", req).unwrap_or_else(|| {
        let mut r = axum::response::Html(NOT_BUILT).into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        r
    })
}

/// The phone UI (docs/ANDROID.md) is its own app under `/m/`, served with a strict CSP (the
/// desktop page keeps an inline theme script, so it doesn't get one).
const MOBILE_CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; \
     img-src 'self' data:; manifest-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'";

fn mobile(source: &Source, path: &str, req: &HeaderMap) -> Response {
    let last = path.rsplit('/').next().unwrap_or("");
    let mut r = match file(source, path, req) {
        Some(r) => r,
        None if last.contains('.') => StatusCode::NOT_FOUND.into_response(),
        None => file(source, "m/index.html", req)
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
    };
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(MOBILE_CSP),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if path == "m/sw.js" {
        h.insert("service-worker-allowed", HeaderValue::from_static("/m/"));
    }
    r
}

pub async fn serve(uri: Uri, req: HeaderMap) -> Response {
    if let Some((mobile, release, asset)) = release_asset(uri.path()) {
        let root = crate::paths::data_dir().join("ui/releases");
        let selected = root.join(release);
        let valid_root = std::fs::canonicalize(&selected)
            .ok()
            .zip(std::fs::canonicalize(&root).ok())
            .is_some_and(|(selected, root)| selected.starts_with(root));
        return if valid_root {
            match Source::directory(&selected) {
                Ok(source) if mobile => mobile_asset(&source, asset, &req),
                Ok(source) => file(&source, asset, &req)
                    .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        } else {
            StatusCode::NOT_FOUND.into_response()
        };
    }
    match Source::configured() {
        Ok(source) => serve_from(&source, uri, req),
        Err(error) => {
            tracing::warn!("UI release unavailable: {error:#}");
            (StatusCode::SERVICE_UNAVAILABLE, [(header::CACHE_CONTROL, "no-store")],
                "This UI release is unavailable or incompatible. Install a compatible UI release and refresh this page; running terminals are unaffected.").into_response()
        }
    }
}

fn release_asset(path: &str) -> Option<(bool, &str, &str)> {
    let (mobile, rest) = if let Some(rest) = path.strip_prefix("/m/_ui/") {
        (true, rest)
    } else {
        (false, path.strip_prefix("/_ui/")?)
    };
    let (release, asset) = rest.split_once('/')?;
    if release.is_empty()
        || release.len() > 64
        || !release.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
    {
        return None;
    }
    Some((mobile, release, asset))
}

fn mobile_asset(source: &Source, asset: &str, req: &HeaderMap) -> Response {
    mobile(source, &format!("m/{asset}"), req)
}

fn serve_from(source: &Source, uri: Uri, req: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        return index(source, &req);
    }
    if path.split('/').any(|seg| seg == "..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if path == "m" {
        return axum::response::Redirect::permanent("/m/").into_response();
    }
    if let Some(rest) = path.strip_prefix("m/") {
        return mobile(
            source,
            if rest.is_empty() {
                "m/index.html"
            } else {
                path
            },
            &req,
        );
    }
    if let Some(r) = file(source, path, &req) {
        return r;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last.contains('.') || path.starts_with("api/") || path.starts_with("hook/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    index(source, &req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn external_release_switches_atomically_and_stays_within_its_root() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("muster-assets-{}", rand::random::<u64>()));
        crate::paths::ensure_private_dir(&root).unwrap();
        for name in ["one", "two", "bad"] {
            let release = root.join(name);
            std::fs::create_dir(&release).unwrap();
            std::fs::write(
                release.join("muster-ui.json"),
                if name == "bad" {
                    "{\"protocol\":999}"
                } else {
                    "{\"protocol\":1}"
                },
            )
            .unwrap();
            std::fs::write(release.join("index.html"), name).unwrap();
            std::fs::write(release.join("app.js"), name).unwrap();
            std::fs::write(release.join("app.css"), "").unwrap();
        }
        std::fs::write(root.join("private.txt"), "must not be served").unwrap();
        symlink(root.join("private.txt"), root.join("one/leak.txt")).unwrap();
        let current = root.join("current");
        symlink(root.join("one"), &current).unwrap();
        let original = Source::directory(&current).unwrap();
        let first = serve_from(&original, "/app.js".parse().unwrap(), HeaderMap::new());
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CACHE_CONTROL], "no-cache");
        let old_etag = first.headers()[header::ETAG].clone();
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, old_etag.clone());
        assert_eq!(
            serve_from(&original, "/app.js".parse().unwrap(), headers.clone()).status(),
            StatusCode::NOT_MODIFIED
        );
        assert!(original.read("../private.txt").is_none());
        assert!(original.read("leak.txt").is_none());
        assert_eq!(
            serve_from(&original, "/missing.js".parse().unwrap(), HeaderMap::new()).status(),
            StatusCode::NOT_FOUND
        );
        symlink(root.join("two"), root.join("next")).unwrap();
        std::fs::rename(root.join("next"), &current).unwrap();
        let replacement = Source::directory(&current).unwrap();
        let next = serve_from(&replacement, "/app.js".parse().unwrap(), headers);
        assert_eq!(next.status(), StatusCode::OK);
        assert_ne!(next.headers()[header::ETAG], old_etag);
        assert_eq!(
            axum::body::to_bytes(next.into_body(), 1024).await.unwrap(),
            "two"
        );
        assert_eq!(
            original.read("app.js").unwrap().0,
            b"one",
            "in-flight request retains its selected release"
        );
        assert!(Source::directory(&root.join("bad")).is_err());
        let install_data = root.join("data");
        let installed = install(&root.join("two"), &install_data).unwrap();
        assert_eq!(
            Source::directory(&install_data.join("ui/current"))
                .unwrap()
                .read("app.js")
                .unwrap()
                .0,
            b"two"
        );
        assert!(
            install(&root.join("one"), &install_data).is_err(),
            "reject symlinked content before activation"
        );
        assert!(install(&root.join("bad"), &install_data).is_err());
        assert_eq!(
            std::fs::canonicalize(install_data.join("ui/current")).unwrap(),
            installed
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
