//! Agent-owned apps and their durable return channel. The operation lock also arbitrates
//! waits against injection (an event is consumed by exactly one of those paths) and against
//! session moves, which take it too: a wait or a panel prompt is checked against its task
//! under it. Panel file I/O happens outside it, so a slow or hostile path cannot stall the
//! panel system.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hub::{Hub, ServerEvent};
use crate::model::*;

pub const MAX_FILE: usize = 1024 * 1024;
pub const MAX_EVENT: usize = 16 * 1024;
/// Open panels per task, and in all, so a runaway agent cannot fill the disk.
pub const MAX_OPEN_PER_TASK: usize = 16;
pub const MAX_OPEN_TOTAL: usize = 128;
/// One wait returns whole events up to about this many bytes; the rest stay queued.
const MAX_WAIT_BYTES: usize = 256 * 1024;
pub const MARKDOWN: &str = include_str!("../web/panels/markdown.html");
pub const PROXY: &str = include_str!("../web/panels/proxy.html");

/// A wait in progress, bound to the caller's task and terminal run when it began.
#[derive(Clone, Copy)]
struct WaitReg { session: SessionId, channel: ChannelId, panel: Option<PanelId>, generation: Option<u64> }

#[derive(Default)]
pub struct Panels {
    pub(crate) operations: Mutex<()>,
    waiters: Mutex<BTreeMap<u64, WaitReg>>,
    next_waiter: AtomicU64,
    event_rates: Mutex<BTreeMap<PanelId, Vec<i64>>>,
    pub port: std::sync::atomic::AtomicU16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub title: String,
    pub app: String,
    pub data: Option<Value>,
    pub html: Option<String>,
    pub html_path: Option<String>,
    pub csp: Option<PanelCsp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub panel_id: PanelId,
    pub title: Option<String>,
    pub data: Option<Value>,
    pub html: Option<String>,
    pub html_path: Option<String>,
}

pub fn dir(hub: &Hub, id: PanelId) -> PathBuf { hub.store().panel_dir(id) }

fn app_file(hub: &Hub, p: &Panel) -> PathBuf { dir(hub, p.id).join(format!("app-{}.html", p.html_revision)) }
fn data_file(hub: &Hub, p: &Panel) -> PathBuf { dir(hub, p.id).join(format!("data-{}.json", p.revision)) }

fn title(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    ensure!(!s.is_empty() && s.len() <= 200 && !s.chars().any(char::is_control), "title must be 1–200 bytes of text");
    Ok(s.to_string())
}

/// Only origin sources, never CSP tokens, paths, credentials or arbitrary schemes.
pub fn validate_csp(csp: &PanelCsp) -> anyhow::Result<()> {
    for domains in [&csp.resource_domains, &csp.connect_domains, &csp.frame_domains, &csp.base_uri_domains] {
        ensure!(domains.len() <= 32, "at most 32 domains per CSP directive");
        for s in domains {
            ensure!(s.len() <= 255 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b":/.*-[]".contains(&c)), "invalid CSP origin: {s}");
            let uri: axum::http::Uri = s.parse().context("invalid CSP origin")?;
            ensure!(matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
                && uri.path() == "/" && uri.query().is_none(), "CSP domains must be http(s) origins: {s}");
            ensure!(!uri.host().unwrap_or("").is_empty(), "invalid CSP host");
        }
    }
    for s in &csp.frame_domains {
        let uri: axum::http::Uri = s.parse()?;
        ensure!(uri.scheme_str() == Some("http") && matches!(uri.host(), Some("localhost" | "127.0.0.1")),
            "frameDomains only supports http://localhost or http://127.0.0.1 dev servers");
    }
    ensure!(csp.base_uri_domains.is_empty(), "baseUriDomains is not supported");
    Ok(())
}

/// `srcdoc` inherits this policy; the inner frame additionally receives the strict app CSP.
/// connectDomains are retained for a future per-panel approval; they grant no access today.
pub fn proxy_csp(csp: &PanelCsp, ui_port: u16) -> String {
    let resources = csp.resource_domains.join(" ");
    format!("default-src 'none'; script-src 'self' 'unsafe-inline' {resources}; style-src 'self' 'unsafe-inline' {resources}; img-src 'self' data: {resources}; font-src 'self' {resources}; media-src 'self' data: {resources}; connect-src 'none'; frame-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors http://127.0.0.1:{ui_port} http://localhost:{ui_port}")
}

/// Read the app HTML. Called without the operation lock. `html_path` must stay inside the
/// task cwd and name a regular file: it is opened without blocking (a FIFO or device never
/// stalls the call) and without following a final symlink, then checked by descriptor.
fn input_html(cwd: &Path, inline: Option<String>, path: Option<String>) -> anyhow::Result<Option<String>> {
    ensure!(inline.is_none() || path.is_none(), "give html or html_path, not both");
    let html = if let Some(path) = path {
        let root = cwd.canonicalize().context("resolving task cwd")?;
        let candidate = root.join(path).canonicalize().context("resolving html_path")?;
        ensure!(candidate.starts_with(&root), "html_path must stay inside the caller's task cwd");
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Read;
        let file = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&candidate)
            .context("opening html_path")?;
        ensure!(file.metadata()?.is_file(), "html_path must name a regular file");
        // Linux checks the opened descriptor too (an ancestor could have been swapped for a
        // symlink since canonicalize). Known gap: macOS has no such check (docs/AGENT_PANELS.md).
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            ensure!(actual.starts_with(&root), "html_path escaped the task cwd");
        }
        let mut text = String::new();
        file.take(MAX_FILE as u64 + 1).read_to_string(&mut text).context("html_path must be UTF-8 text")?;
        Some(text)
    } else { inline };
    if let Some(html) = &html { ensure!(html.len() <= MAX_FILE, "HTML exceeds 1 MiB"); }
    Ok(html)
}

fn data_bytes(data: &Value) -> anyhow::Result<Vec<u8>> {
    ensure!(data.is_object(), "panel data must be an object");
    let data = serde_json::to_vec(data)?;
    ensure!(data.len() <= MAX_FILE, "data exceeds 1 MiB");
    Ok(data)
}

fn caller(hub: &Hub, session: SessionId) -> anyhow::Result<(SessionInfo, Channel)> {
    let s = hub.session(session).context("calling session no longer exists")?;
    let c = hub.channel(s.channel_id).filter(|c| !c.archived).context("task is archived or missing")?;
    Ok((s, c))
}

/// The caller under the lock, still in the task its unlocked file work was done for.
fn caller_in(hub: &Hub, session: SessionId, channel: ChannelId) -> anyhow::Result<(SessionInfo, Channel)> {
    let (s, c) = caller(hub, session)?;
    ensure!(c.id == channel, "the calling session moved to another task; try again");
    Ok((s, c))
}

pub fn list(hub: &Hub) -> anyhow::Result<Vec<Panel>> { hub.store().load_panels() }

pub fn get(hub: &Hub, id: PanelId) -> anyhow::Result<Panel> {
    list(hub)?.into_iter().find(|p| p.id == id).context("panel not found")
}

fn own(hub: &Hub, session: SessionId, id: PanelId) -> anyhow::Result<Panel> {
    let (_, ch) = caller(hub, session)?;
    let p = get(hub, id)?;
    ensure!(p.channel_id == ch.id, "panel belongs to another task");
    Ok(p)
}

/// A unique name for a file or directory written before its final name is known.
fn staging_name(prefix: &str) -> String { format!("{prefix}{:016x}", rand::random::<u64>()) }

fn remove_dir(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        // The startup sweep retries it.
        Err(e) => tracing::warn!("removing panel files {}: {e}", path.display()),
    }
}

pub fn open(hub: &Hub, session: SessionId, args: Open) -> anyhow::Result<PanelId> {
    let (_, ch) = caller(hub, session)?;
    ensure!(matches!(args.app.as_str(), "markdown" | "custom"), "unknown app (use markdown or custom)");
    let supplied = input_html(Path::new(&ch.cwd), args.html, args.html_path)?;
    let html = if args.app == "custom" { supplied.context("custom apps need html or html_path")? }
        else { ensure!(supplied.is_none(), "markdown does not accept custom HTML"); MARKDOWN.to_string() };
    let csp = args.csp.unwrap_or_default();
    validate_csp(&csp)?;
    let data = data_bytes(&args.data.unwrap_or_else(|| json!({})))?;
    let title = title(&args.title)?;
    // Write the files under a temporary name first; the row commit names them.
    let staging = hub.store().panels_root().join(staging_name(".staging-"));
    let staged = (|| -> anyhow::Result<()> {
        crate::paths::write_private(&staging.join("app-1.html"), html.as_bytes())?;
        crate::paths::write_private(&staging.join("data-1.json"), &data)?;
        Ok(())
    })();
    if let Err(e) = staged { remove_dir(&staging); return Err(e) }
    let result = (|| -> anyhow::Result<Panel> {
        let _op = hub.panels.operations.lock();
        let (s, ch) = caller_in(hub, session, ch.id)?;
        let open: Vec<Panel> = list(hub)?.into_iter().filter(|p| !p.closed).collect();
        ensure!(open.iter().filter(|p| p.channel_id == ch.id).count() < MAX_OPEN_PER_TASK,
            "this task already has {MAX_OPEN_PER_TASK} open panels; close one with panel_close first");
        ensure!(open.len() < MAX_OPEN_TOTAL, "Muster already has {MAX_OPEN_TOTAL} open panels; close some first");
        let now = now_ms();
        let mut panel = Panel { id: 0, channel_id: ch.id, session_id: s.id, window_id: s.window_id.or(ch.window_id),
            title, app: args.app, csp, revision: 1, html_revision: 1,
            created_at: now, updated_at: now, closed: false, context: None, dropped_events: 0 };
        let mut placed = None;
        let created = hub.store().create_panel(&mut panel, |p| {
            // No committed row names this id, so anything there is a leftover of a failed
            // open (an id rolls back with its insert); a crash before the commit leaves the
            // directory to the startup sweep.
            let target = dir(hub, p.id);
            remove_dir(&target);
            std::fs::rename(&staging, &target)?;
            placed = Some(target);
            Ok(())
        });
        if let Err(e) = created {
            if let Some(target) = placed { remove_dir(&target); }
            return Err(e);
        }
        hub.emit(ServerEvent::PanelUpserted(panel.clone()));
        Ok(panel)
    })();
    match result {
        Ok(panel) => Ok(panel.id),
        Err(e) => {
            remove_dir(&staging);
            Err(e)
        }
    }
}

pub fn update(hub: &Hub, session: SessionId, args: Update) -> anyhow::Result<()> {
    // Read and stage everything without the lock; the lock only renames and commits.
    let seen = own(hub, session, args.panel_id)?;
    ensure!(!seen.closed, "panel is closed");
    let (_, ch) = caller(hub, session)?;
    let html = input_html(Path::new(&ch.cwd), args.html, args.html_path)?;
    ensure!(html.is_none() || seen.app == "custom", "only custom apps accept HTML updates");
    let title = args.title.as_deref().map(title).transpose()?;
    let data = args.data.as_ref().map(data_bytes).transpose()?;
    // Unchanged HTML keeps its revision (the app keeps running), if nobody changed it meanwhile.
    let same_html = html.as_ref().is_some_and(|html| std::fs::read_to_string(app_file(hub, &seen)).is_ok_and(|old| old == *html));
    let folder = dir(hub, seen.id);
    let staged_html = folder.join(staging_name("stage-"));
    let staged_data = folder.join(staging_name("stage-"));
    let cleanup = || { let _ = std::fs::remove_file(&staged_html); let _ = std::fs::remove_file(&staged_data); };
    let staged = (|| -> anyhow::Result<()> {
        if let Some(html) = &html { crate::paths::write_private(&staged_html, html.as_bytes())?; }
        if let Some(data) = &data { crate::paths::write_private(&staged_data, data)?; }
        Ok(())
    })();
    if let Err(e) = staged { cleanup(); return Err(e.context("writing panel files (is the panel closed?)")) }
    let result = (|| -> anyhow::Result<(Panel, Panel)> {
        let _op = hub.panels.operations.lock();
        let mut p = own(hub, session, args.panel_id)?;
        caller_in(hub, session, ch.id)?;
        ensure!(!p.closed, "panel is closed");
        let old = p.clone();
        if let Some(t) = title { p.title = t; }
        p.revision += 1;
        p.updated_at = now_ms();
        let new_html = html.is_some() && !(same_html && old.html_revision == seen.html_revision);
        if new_html {
            p.html_revision = p.revision;
            std::fs::rename(&staged_html, app_file(hub, &p))?;
        }
        if data.is_some() { std::fs::rename(&staged_data, data_file(hub, &p))?; }
        else { std::fs::hard_link(data_file(hub, &old), data_file(hub, &p))?; }
        if let Err(e) = hub.store().save_panel(&p) {
            let _ = std::fs::remove_file(data_file(hub, &p));
            if new_html { let _ = std::fs::remove_file(app_file(hub, &p)); }
            return Err(e);
        }
        hub.emit(ServerEvent::PanelUpserted(p.clone()));
        Ok((old, p))
    })();
    cleanup();
    let (old, p) = result?;
    // The DB names complete immutable files, so a crash never publishes half an update.
    let _ = std::fs::remove_file(data_file(hub, &old));
    if old.html_revision != p.html_revision { let _ = std::fs::remove_file(app_file(hub, &old)); }
    Ok(())
}

pub fn resource(hub: &Hub, id: PanelId) -> anyhow::Result<Value> {
    // Files are read without the lock; an update may replace them meanwhile, so retry.
    let mut attempt = 0;
    loop {
        let p = { let _op = hub.panels.operations.lock(); get(hub, id)? };
        ensure!(!p.closed, "panel is closed");
        let read = || -> std::io::Result<(String, Vec<u8>)> {
            Ok((std::fs::read_to_string(app_file(hub, &p))?, std::fs::read(data_file(hub, &p))?))
        };
        match read() {
            Ok((html, data)) => return Ok(json!({"panel": p, "html": html, "data": serde_json::from_slice::<Value>(&data)?})),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && attempt < 3 => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn close(hub: &Hub, session: Option<SessionId>, id: PanelId, human: bool) -> anyhow::Result<()> {
    let window = {
        let _op = hub.panels.operations.lock();
        let mut p = match session { Some(s) => own(hub, s, id)?, None => get(hub, id)? };
        if p.closed { return Ok(()) }
        p.closed = true;
        p.updated_at = now_ms();
        if human {
            let event = PanelEvent { id: 0, panel_id: id, kind: "closed".into(), payload: json!({}), at: now_ms(), context: None };
            hub.store().push_panel_event(&mut p, &event)?;
        } else { hub.store().save_panel(&p)?; }
        hub.panels.event_rates.lock().remove(&id);
        let window = p.window_id;
        hub.emit(ServerEvent::PanelUpserted(p));
        window
    };
    // Its app and data can no longer be shown; queued events and metadata stay for delivery.
    remove_dir(&dir(hub, id));
    if let Some(window) = window { hub.panel_left_window(window); }
    Ok(())
}

pub fn event(hub: &Hub, id: PanelId, kind: String, payload: Value) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let mut p = get(hub, id)?;
    ensure!(!p.closed, "panel is closed");
    ensure!(!kind.is_empty() && kind.len() <= 80 && !kind.chars().any(char::is_control) && kind != "closed" && kind != "overflow", "invalid event kind");
    ensure!(serde_json::to_vec(&payload)?.len() <= MAX_EVENT, "event exceeds 16 KiB");
    let now = now_ms();
    let mut rates = hub.panels.event_rates.lock();
    let rate = rates.entry(id).or_default();
    rate.retain(|t| *t > now - 60_000);
    ensure!(rate.len() < 30, "panel event rate limit (30 per minute)");
    rate.push(now);
    hub.store().push_panel_event(&mut p, &PanelEvent { id: 0, panel_id: id, kind, payload, at: now, context: None })?;
    Ok(())
}

pub fn context(hub: &Hub, id: PanelId, context: Value) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let mut p = get(hub, id)?;
    ensure!(!p.closed && context.is_object(), "invalid panel context");
    ensure!(serde_json::to_vec(&context)?.len() <= MAX_EVENT, "context exceeds 16 KiB");
    p.context = Some(context);
    hub.store().save_panel(&p)
}

pub fn move_to(hub: &Hub, id: PanelId, window: WindowId) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    ensure!(hub.windows().iter().any(|w| w.id == window), "window not found");
    let mut p = get(hub, id)?;
    ensure!(!p.closed, "panel is closed");
    let old = p.window_id;
    p.window_id = Some(window);
    hub.store().save_panel(&p)?;
    hub.emit(ServerEvent::PanelUpserted(p));
    if let Some(old) = old.filter(|old| *old != window) { hub.panel_left_window(old); }
    Ok(())
}

pub fn adopt_orphans(hub: &Hub, window: WindowId) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let windows = hub.windows();
    for mut p in list(hub)? {
        if !p.closed && p.window_id.is_none_or(|id| !windows.iter().any(|w| w.id == id)) {
            p.window_id = Some(window);
            hub.store().save_panel(&p)?;
            hub.emit(ServerEvent::PanelUpserted(p));
        }
    }
    Ok(())
}

/// Remove a task's panels, events and files. Every task-archive path calls this (it is
/// idempotent); the startup sweep removes files a failed attempt left behind.
pub fn archive(hub: &Hub, channel: ChannelId) -> anyhow::Result<()> {
    let (ids, windows) = {
        let _op = hub.panels.operations.lock();
        let panels: Vec<Panel> = list(hub)?.into_iter().filter(|p| p.channel_id == channel).collect();
        hub.store().delete_channel_panels(channel)?;
        let mut windows = BTreeSet::new();
        for p in &panels {
            hub.emit(ServerEvent::PanelRemoved { id: p.id });
            hub.panels.event_rates.lock().remove(&p.id);
            if let Some(window) = p.window_id { windows.insert(window); }
        }
        (panels.into_iter().map(|p| p.id).collect::<Vec<_>>(), windows)
    };
    for id in ids { remove_dir(&dir(hub, id)); }
    for window in windows { hub.panel_left_window(window); }
    Ok(())
}

/// At startup, before any UI or delivery: finish interrupted archives, forget closed panels
/// with nothing left to deliver, and remove files no committed record names (staged writes,
/// old revisions, closed or deleted panels).
pub fn recover(hub: &Hub) -> anyhow::Result<()> {
    for panel in list(hub)? {
        if hub.channel(panel.channel_id).is_none_or(|c| c.archived) { archive(hub, panel.channel_id)?; }
    }
    let queued = crate::inject::queued_panel_ids(hub);
    for id in hub.store().idle_closed_panels()? {
        if !queued.contains(&id) { hub.store().delete_panel(id)?; }
    }
    let panels: BTreeMap<PanelId, Panel> = list(hub)?.into_iter().map(|p| (p.id, p)).collect();
    let Ok(entries) = std::fs::read_dir(hub.store().panels_root()) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        let live = entry.file_name().to_str().and_then(|n| n.parse::<PanelId>().ok())
            .and_then(|id| panels.get(&id)).filter(|p| !p.closed);
        let Some(p) = live else {
            if path.is_dir() { remove_dir(&path) } else { let _ = std::fs::remove_file(&path); }
            continue;
        };
        let keep = [app_file(hub, p), data_file(hub, p)];
        for file in std::fs::read_dir(&path).into_iter().flatten().flatten() {
            if !keep.contains(&file.path()) && let Err(e) = std::fs::remove_file(file.path()) {
                tracing::warn!("removing stale panel file {}: {e}", file.path().display());
            }
        }
    }
    Ok(())
}

struct Waiter<'a> { panels: &'a Panels, key: u64 }
impl Drop for Waiter<'_> {
    fn drop(&mut self) { self.panels.waiters.lock().remove(&self.key); }
}

/// Is the wait still the caller's own: same session, task and terminal run as when it began?
fn wait_valid(hub: &Hub, w: &WaitReg) -> anyhow::Result<()> {
    let s = hub.session(w.session).context("calling session no longer exists")?;
    ensure!(s.channel_id == w.channel, "wait cancelled: the calling session moved to another task");
    ensure!(hub.channel(w.channel).is_some_and(|c| !c.archived), "wait cancelled: the task was archived");
    ensure!(hub.pty_generation(w.session) == w.generation, "wait cancelled: the calling session restarted");
    Ok(())
}

/// Whole events in order, up to `budget` bytes (always at least one), with the latest
/// app context on each panel's last returned event.
fn take_events(panels: &[Panel], queued: Vec<Vec<PanelEvent>>, budget: usize) -> (Vec<PanelEvent>, usize) {
    let (mut out, mut used, mut left) = (Vec::new(), 0usize, 0usize);
    for (p, events) in panels.iter().zip(queued) {
        let start = out.len();
        for mut e in events {
            e.context = None;
            let size = serde_json::to_vec(&e).map_or(0, |v| v.len());
            if left > 0 || (!out.is_empty() && used + size > budget) { left += 1; continue }
            used += size;
            out.push(e);
        }
        if out.len() > start && let Some(last) = out.last_mut() { last.context = p.context.clone(); }
    }
    (out, left)
}

pub async fn wait(hub: &Hub, session: SessionId, panel: Option<PanelId>, seconds: u64) -> anyhow::Result<Value> {
    ensure!(seconds <= 1800, "timeout_seconds must be at most 1800");
    let waiter = {
        let _op = hub.panels.operations.lock();
        let (_, ch) = caller(hub, session)?;
        if let Some(id) = panel { own(hub, session, id)?; }
        let reg = WaitReg { session, channel: ch.id, panel, generation: hub.pty_generation(session) };
        let key = hub.panels.next_waiter.fetch_add(1, Ordering::Relaxed);
        hub.panels.waiters.lock().insert(key, reg);
        Waiter { panels: &hub.panels, key }
    };
    let reg = hub.panels.waiters.lock()[&waiter.key];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        {
            let _op = hub.panels.operations.lock();
            // Checked with the acknowledgement under one lock, which session moves also take.
            wait_valid(hub, &reg)?;
            let panels: Vec<_> = list(hub)?.into_iter().filter(|p| p.channel_id == reg.channel && panel.is_none_or(|id| id == p.id)).collect();
            let queued = panels.iter().map(|p| hub.store().panel_events(p)).collect::<anyhow::Result<Vec<_>>>()?;
            let (events, remaining) = take_events(&panels, queued, MAX_WAIT_BYTES);
            if !events.is_empty() {
                // Known gap: the events are deleted as this result is built. If the backend
                // dies, or the caller's connection drops, before the caller reads it, they are
                // lost; chosen over redelivering events the agent may already have acted on.
                hub.store().ack_panel_events(&events)?;
                return Ok(json!({"events": events, "remaining": remaining, "timed_out": false}));
            }
            if tokio::time::Instant::now() >= deadline { return Ok(json!({"events": [], "remaining": 0, "timed_out": true})); }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `panel_event`: the full payload of an event a panel prompt showed only as a placeholder.
/// Fetching acknowledges it.
pub fn fetch_event(hub: &Hub, session: SessionId, panel: PanelId, event: i64) -> anyhow::Result<Value> {
    let _op = hub.panels.operations.lock();
    own(hub, session, panel)?;
    if let Some(found) = crate::inject::take_in_flight_panel_event(hub, panel, event)? {
        return Ok(json!({"event": found}));
    }
    match hub.store().panel_event(panel, event)? {
        Some((found, true)) => {
            hub.store().ack_panel_events(std::slice::from_ref(&found))?;
            Ok(json!({"event": found}))
        }
        Some(_) => anyhow::bail!("event {event} is still queued: it comes with the next panel prompt or wait_for_panel_event"),
        None => anyhow::bail!("panel {panel} has no event {event} to fetch (fetched already, or never shown as a placeholder)"),
    }
}

/// Called without an await under the operation lock. Queue acceptance and event removal
/// commit together; waits never observe events which were already accepted for injection.
pub fn deliver_pending(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let panels = list(hub)?;
    // Only a wait still bound to its caller's task and run holds events back.
    let waits: Vec<WaitReg> = hub.panels.waiters.lock().values().copied().collect();
    let waits: Vec<WaitReg> = waits.into_iter().filter(|w| wait_valid(hub, w).is_ok()).collect();
    let mut batches: BTreeMap<SessionId, Vec<(Panel, Vec<PanelEvent>)>> = BTreeMap::new();
    for p in panels {
        if waits.iter().any(|w| w.channel == p.channel_id && w.panel.is_none_or(|id| id == p.id)) { continue }
        let Some(s) = hub.session(p.session_id) else { continue };
        if s.channel_id != p.channel_id || s.kind == SessionKind::Shell || s.state != AgentState::Idle || s.queued > 0 { continue }
        if hub.channel(p.channel_id).is_none_or(|c| c.archived) { continue }
        let events = hub.store().panel_events(&p)?;
        if !events.is_empty() { batches.entry(s.id).or_default().push((p, events)); }
    }
    for (session, panels) in batches { crate::inject::enqueue_panels(hub, session, &panels)?; }
    Ok(())
}

pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            if hub.is_shutting_down() { return }
            if let Err(e) = deliver_pending(&hub) { tracing::warn!("panel delivery: {e:#}"); }
        }
    });
}

pub async fn tool(hub: &Arc<Hub>, session: SessionId, name: &str, args: Value) -> anyhow::Result<Value> {
    match name {
        "panel_open" => Ok(json!({"panel_id": open(hub, session, serde_json::from_value(args)?)?})),
        "panel_update" => { update(hub, session, serde_json::from_value(args)?)?; Ok(json!({})) },
        "panel_close" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { panel_id: PanelId }
            let p: P = serde_json::from_value(args)?;
            close(hub, Some(session), p.panel_id, false)?;
            Ok(json!({}))
        },
        "list_panels" => {
            ensure!(args.as_object().is_some_and(|o| o.is_empty()), "list_panels takes no arguments");
            let (_, ch) = caller(hub, session)?;
            Ok(json!({"panels": list(hub)?.into_iter().filter(|p| p.channel_id == ch.id && !p.closed).collect::<Vec<_>>()}))
        },
        "wait_for_panel_event" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { panel_id: Option<PanelId>, timeout_seconds: Option<u64> }
            let p: P = serde_json::from_value(args)?;
            wait(hub, session, p.panel_id, p.timeout_seconds.unwrap_or(300)).await
        },
        "panel_event" => {
            #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct P { panel_id: PanelId, event_id: i64 }
            let p: P = serde_json::from_value(args)?;
            fetch_event(hub, session, p.panel_id, p.event_id)
        },
        _ => anyhow::bail!("unknown panel tool"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StoredSession};

    fn fixture() -> Arc<Hub> {
        let store = Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        for ch in 1..=2 {
            let mut channel = crate::mcp::guard::test_channel(ch);
            channel.window_id = Some(1);
            store.insert_channel(&channel).unwrap();
            let mut info = crate::mcp::guard::test_session(0, None);
            info.channel_id = ch;
            info.window_id = Some(1);
            store.insert_session(&StoredSession { info, extra_args: vec![] }).unwrap();
        }
        Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None }, store).unwrap()
    }

    fn markdown(hub: &Hub) -> PanelId {
        open(hub, 1, Open { title: "Review".into(), app: "markdown".into(), data: Some(json!({"markdown":"# Hello"})),
            html: None, html_path: None, csp: None }).unwrap()
    }

    #[test]
    fn files_are_private_and_revisions_survive_reconstruction() {
        use std::os::unix::fs::PermissionsExt;
        let hub = fixture();
        let id = markdown(&hub);
        for file in ["app-1.html", "data-1.json"] {
            assert_eq!(std::fs::metadata(dir(&hub, id).join(file)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        update(&hub, 1, Update { panel_id: id, data: Some(json!({"markdown":"# New"})), title: Some("Changed".into()), html: None, html_path: None }).unwrap();
        let res = resource(&hub, id).unwrap();
        assert_eq!(res["panel"]["revision"], 2);
        assert_eq!(res["panel"]["html_revision"], 1);
        assert_eq!(res["data"]["markdown"], "# New");
        assert!(!dir(&hub, id).join("data-1.json").exists());
        let rebuilt = Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None }, hub.store().clone()).unwrap();
        assert_eq!(resource(&rebuilt, id).unwrap(), res);
        archive(&hub, 1).unwrap();
        assert!(list(&hub).unwrap().is_empty() && !dir(&hub, id).exists());
    }

    #[test]
    fn only_changed_html_advances_html_revision() {
        let hub = fixture();
        let id = open(&hub, 1, Open { title:"Custom".into(), app:"custom".into(), data:None, html:Some("a".into()), html_path:None, csp:None }).unwrap();
        for (html, expected) in [("a", 1), ("b", 3)] {
            update(&hub, 1, Update { panel_id:id, title:None, data:None, html:Some(html.into()), html_path:None }).unwrap();
            assert_eq!(get(&hub, id).unwrap().html_revision, expected);
        }
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn panel_only_windows_survive_release_until_the_panel_leaves() {
        let hub = fixture();
        let id = markdown(&hub);
        let window = hub.create_window(None).unwrap().id;
        move_to(&hub, id, window).unwrap();
        hub.claim_window(99, Some(window)).unwrap();
        hub.release_windows(99);
        assert!(hub.windows().iter().any(|w| w.id == window));
        move_to(&hub, id, 1).unwrap();
        assert!(!hub.windows().iter().any(|w| w.id == window));
        let window = hub.create_window(None).unwrap().id;
        move_to(&hub, id, window).unwrap();
        close(&hub, None, id, true).unwrap();
        assert!(!hub.windows().iter().any(|w| w.id == window));
        archive(&hub, 1).unwrap();
    }

    #[tokio::test]
    async fn scope_close_context_wait_and_cancellation() {
        let hub = fixture();
        let id = markdown(&hub);
        assert!(close(&hub, Some(2), id, false).unwrap_err().to_string().contains("another task"));
        assert!(wait(&hub, 2, Some(id), 0).await.unwrap_err().to_string().contains("another task"));
        assert!(update(&hub, 2, Update { panel_id:id, title:None, data:Some(json!({})), html:None, html_path:None }).is_err());
        context(&hub, id, json!({"structuredContent":{"selected":2}})).unwrap();
        event(&hub, id, "approve".into(), json!({"hunk":2})).unwrap();
        let out = wait(&hub, 1, Some(id), 0).await.unwrap();
        assert_eq!(out["events"][0]["kind"], "approve");
        assert_eq!(out["events"][0]["context"]["structuredContent"]["selected"], 2);
        assert_eq!(wait(&hub, 1, None, 0).await.unwrap()["timed_out"], true);
        tokio::select! {
            _ = wait(&hub, 1, None, 300) => panic!("wait completed early"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {},
        }
        assert!(hub.panels.waiters.lock().is_empty(), "cancelled waits must not suppress injection");
        close(&hub, None, id, true).unwrap();
        close(&hub, None, id, true).unwrap();
        let out = wait(&hub, 1, Some(id), 0).await.unwrap();
        assert_eq!(out["events"].as_array().unwrap().len(), 1);
        assert_eq!(out["events"][0]["kind"], "closed");
        assert!(event(&hub, id, "late".into(), json!({})).is_err());
        archive(&hub, 1).unwrap();
    }

    #[tokio::test]
    async fn queued_events_survive_restart_and_are_bounded_with_a_note() {
        let hub = fixture();
        let id = markdown(&hub);
        let mut p = get(&hub, id).unwrap();
        for n in 0..205 {
            hub.store().push_panel_event(&mut p, &PanelEvent { id:0, panel_id:id, kind:"click".into(), payload:json!(n), at:n, context:None }).unwrap();
        }
        let loaded = get(&hub, id).unwrap();
        let events = hub.store().panel_events(&loaded).unwrap();
        assert_eq!(events.len(), 201);
        assert_eq!(events[0].kind, "overflow");
        assert_eq!(events[0].payload["dropped"], 5);
        assert_eq!(events[1].payload, 5);
        assert_eq!(events[200].payload, 204);
        // No live agent: neither a shell nor an exited opener receives injection.
        deliver_pending(&hub).unwrap();
        assert_eq!(hub.store().panel_events(&get(&hub,id).unwrap()).unwrap().len(), 201);
        assert_eq!(wait(&hub, 1, None, 0).await.unwrap()["events"].as_array().unwrap().len(), 201);
        assert_eq!(get(&hub, id).unwrap().dropped_events, 0);
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn queue_transfer_is_atomic_on_persistence_failure() {
        let hub = fixture();
        let id = markdown(&hub);
        event(&hub,id,"click".into(),json!({})).unwrap();
        let p = get(&hub,id).unwrap();
        let events = hub.store().panel_events(&p).unwrap();
        hub.store().fail_runtime_writes_for_test();
        let moves = crate::store::PanelEventMoves { claim: events, ..Default::default() };
        assert!(hub.store().save_prompt_queues(&json!({"queue":"new"}), &moves).is_err());
        assert_eq!(hub.store().panel_events(&p).unwrap().len(), 1);
        assert!(hub.store().load_runtime::<Value>("prompt-queues").is_err());
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn limits_reject_large_files_and_event_floods() {
        let hub = fixture();
        assert!(input_html(Path::new("/tmp"), Some("x".repeat(MAX_FILE + 1)), None).is_err());
        assert!(data_bytes(&json!({"markdown":"x".repeat(MAX_FILE)})).is_err());
        assert!(data_bytes(&json!("not an object")).is_err());
        let id = markdown(&hub);
        assert!(event(&hub,id,"click".into(),json!("x".repeat(MAX_EVENT))).is_err());
        for _ in 0..30 { event(&hub,id,"click".into(),json!({})).unwrap(); }
        assert!(event(&hub,id,"click".into(),json!({})).unwrap_err().to_string().contains("rate limit"));
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn html_paths_reject_escapes_and_external_symlinks() {
        let hub = fixture();
        let root = hub.store().panel_dir(0);
        let cwd = root.join("task");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("outside.html"), "outside").unwrap();
        std::fs::write(cwd.join("inside.html"), "inside").unwrap();
        std::os::unix::fs::symlink(root.join("outside.html"), cwd.join("escape.html")).unwrap();
        std::os::unix::fs::symlink(cwd.join("inside.html"), cwd.join("ok.html")).unwrap();
        for path in ["../outside.html", "escape.html"] { assert!(input_html(&cwd,None,Some(path.into())).is_err()); }
        assert_eq!(input_html(&cwd,None,Some("ok.html".into())).unwrap().unwrap(), "inside");
        assert!(input_html(&cwd,Some("a".into()),Some("inside.html".into())).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn csp_is_strict_and_sources_cannot_inject_directives() {
        for domain in ["https://cdn.example; connect-src *", "https://cdn.example/a", "'unsafe-eval'", "data:", "https://user@cdn.example", "https://cdn.example?x=1"] {
            assert!(validate_csp(&PanelCsp { resource_domains:vec![domain.into()], ..Default::default() }).is_err(), "{domain}");
        }
        let csp = PanelCsp { resource_domains:vec!["https://cdn.example".into()], connect_domains:vec!["https://api.example".into()], frame_domains:vec!["http://localhost:5173".into()], ..Default::default() };
        validate_csp(&csp).unwrap();
        let policy = proxy_csp(&csp,7433);
        assert!(policy.contains("connect-src 'none'") && policy.contains("https://cdn.example") && !policy.contains("api.example"));
        assert!(policy.contains("frame-src 'self'") && policy.contains("frame-ancestors http://127.0.0.1:7433"));
        assert!(validate_csp(&PanelCsp { frame_domains:vec!["https://example.com".into()], ..Default::default() }).is_err());
        assert!(PROXY.contains("event.source === frame.contentWindow") && PROXY.contains("uiOrigins.includes(event.origin)"));
    }

    #[test]
    fn closing_deletes_files_but_keeps_queued_events() {
        let hub = fixture();
        let id = markdown(&hub);
        event(&hub, id, "approve".into(), json!({})).unwrap();
        close(&hub, Some(1), id, false).unwrap();
        assert!(!dir(&hub, id).exists(), "a closed panel's app and data are deleted");
        assert!(resource(&hub, id).is_err());
        let p = get(&hub, id).unwrap();
        assert_eq!(hub.store().panel_events(&p).unwrap()[0].kind, "approve");
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn open_panels_per_task_are_bounded() {
        let hub = fixture();
        for _ in 0..MAX_OPEN_PER_TASK { markdown(&hub); }
        let err = open(&hub, 1, Open { title: "One more".into(), app: "markdown".into(), data: None, html: None, html_path: None, csp: None }).unwrap_err();
        assert!(err.to_string().contains("open panels"), "{err}");
        // Another task has its own allowance; closing one frees a slot.
        open(&hub, 2, Open { title: "Other".into(), app: "markdown".into(), data: None, html: None, html_path: None, csp: None }).unwrap();
        close(&hub, Some(1), 1, false).unwrap();
        markdown(&hub);
        archive(&hub, 1).unwrap();
        archive(&hub, 2).unwrap();
    }

    #[test]
    fn a_fifo_html_path_fails_without_blocking() {
        let hub = fixture();
        let cwd = hub.store().panel_dir(0).join("task");
        std::fs::create_dir_all(&cwd).unwrap();
        let fifo = std::ffi::CString::new(cwd.join("pipe.html").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = cwd.clone();
        std::thread::spawn(move || { let _ = tx.send(input_html(&dir, None, Some("pipe.html".into())).map(|_| ())); });
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("opening a FIFO must not block");
        assert!(result.unwrap_err().to_string().contains("regular file"));
        std::fs::remove_dir_all(hub.store().panel_dir(0)).unwrap();
    }

    #[tokio::test]
    async fn a_wait_ends_when_its_caller_moves_to_another_task() {
        let hub = fixture();
        let id = markdown(&hub);
        let waiting = { let hub = hub.clone(); tokio::spawn(async move { wait(&hub, 1, None, 300).await }) };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.panels.waiters.lock().len(), 1);
        hub.move_session(1, 2).unwrap();
        // The user's click in task 1 must not go to a caller now scoped to task 2.
        event(&hub, id, "approve".into(), json!({})).unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap().unwrap_err();
        assert!(err.to_string().contains("moved to another task"), "{err}");
        assert!(hub.panels.waiters.lock().is_empty());
        assert_eq!(hub.store().panel_events(&get(&hub, id).unwrap()).unwrap().len(), 1);
        assert!(wait(&hub, 1, Some(id), 0).await.unwrap_err().to_string().contains("another task"));
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn closing_an_emptied_task_removes_its_panels() {
        let hub = fixture();
        let id = markdown(&hub);
        event(&hub, id, "approve".into(), json!({})).unwrap();
        // Moving the last session out (close_empty_source) archives through archive_if_empty.
        hub.move_session(1, 2).unwrap();
        assert!(hub.archive_if_empty(1).unwrap());
        assert!(list(&hub).unwrap().is_empty() && !dir(&hub, id).exists());
        assert!(archive(&hub, 1).is_ok(), "cleanup is idempotent");
    }

    #[test]
    fn startup_sweeps_stale_files_and_spent_closed_panels() {
        let hub = fixture();
        let id = markdown(&hub);
        update(&hub, 1, Update { panel_id: id, data: Some(json!({"markdown":"# 2"})), title: None, html: None, html_path: None }).unwrap();
        let folder = dir(&hub, id);
        for junk in ["data-1.tmp4242", "stage-0123456789abcdef", "app-9.html"] { std::fs::write(folder.join(junk), "x").unwrap(); }
        let root = hub.store().panels_root();
        std::fs::create_dir_all(root.join(".staging-00")).unwrap();
        std::fs::create_dir_all(root.join("999")).unwrap();
        let spent = markdown(&hub);
        let kept = markdown(&hub);
        close(&hub, Some(1), spent, false).unwrap();
        event(&hub, kept, "approve".into(), json!({})).unwrap();
        close(&hub, Some(1), kept, false).unwrap();
        std::fs::create_dir_all(dir(&hub, spent)).unwrap();
        recover(&hub).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&folder).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["app-1.html", "data-2.json"]);
        assert!(!root.join(".staging-00").exists() && !root.join("999").exists() && !dir(&hub, spent).exists());
        assert!(get(&hub, spent).is_err(), "a closed panel with nothing to deliver is forgotten");
        assert!(get(&hub, kept).is_ok(), "a closed panel with queued events stays");
        assert_eq!(resource(&hub, id).unwrap()["data"]["markdown"], "# 2");
        archive(&hub, 1).unwrap();
    }

    #[test]
    fn waits_return_whole_events_within_a_budget() {
        let hub = fixture();
        let id = markdown(&hub);
        context(&hub, id, json!({"selected": 1})).unwrap();
        for n in 0..3 { event(&hub, id, "comment".into(), json!({"text": "x".repeat(1000), "n": n})).unwrap(); }
        let p = get(&hub, id).unwrap();
        let queued = vec![hub.store().panel_events(&p).unwrap()];
        let (events, remaining) = take_events(std::slice::from_ref(&p), queued.clone(), 2500);
        assert_eq!((events.len(), remaining), (2, 1));
        assert!(events[0].context.is_none() && events[1].context.as_ref().unwrap()["selected"] == 1);
        assert!(events.iter().all(|e| e.id > 0));
        // A single event larger than the budget still goes out whole.
        let (events, remaining) = take_events(std::slice::from_ref(&p), queued, 10);
        assert_eq!((events.len(), remaining), (1, 2));
        archive(&hub, 1).unwrap();
    }
}
