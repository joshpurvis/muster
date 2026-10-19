//! Agent-owned apps and their durable return channel. The operation lock also arbitrates
//! waits against injection: an event is consumed by exactly one of those paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hub::{Hub, ServerEvent};
use crate::model::*;

pub const MAX_FILE: usize = 1024 * 1024;
pub const MAX_EVENT: usize = 16 * 1024;
pub const MARKDOWN: &str = include_str!("../web/panels/markdown.html");
pub const PROXY: &str = include_str!("../web/panels/proxy.html");

#[derive(Default)]
pub struct Panels {
    pub(crate) operations: Mutex<()>,
    waiters: Mutex<BTreeMap<(ChannelId, Option<PanelId>), usize>>,
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

fn input_html(cwd: &Path, inline: Option<String>, path: Option<String>) -> anyhow::Result<Option<String>> {
    ensure!(inline.is_none() || path.is_none(), "give html or html_path, not both");
    let html = if let Some(path) = path {
        let root = cwd.canonicalize().context("resolving task cwd")?;
        let candidate = root.join(path).canonicalize().context("resolving html_path")?;
        ensure!(candidate.starts_with(&root), "html_path must stay inside the caller's task cwd");
        // Open the resolved file without following a final symlink, then check the actual
        // opened fd on Linux as well (an ancestor could have changed since canonicalize).
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Read;
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&candidate)?;
        ensure!(file.metadata()?.is_file(), "html_path must name a regular file");
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            ensure!(actual.starts_with(&root), "html_path escaped the task cwd");
        }
        let mut text = String::new();
        file.take(MAX_FILE as u64 + 1).read_to_string(&mut text)?;
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

pub fn open(hub: &Hub, session: SessionId, args: Open) -> anyhow::Result<PanelId> {
    let _op = hub.panels.operations.lock();
    let (s, ch) = caller(hub, session)?;
    ensure!(matches!(args.app.as_str(), "markdown" | "custom"), "unknown app (use markdown or custom)");
    let supplied = input_html(Path::new(&ch.cwd), args.html, args.html_path)?;
    let html = if args.app == "custom" { supplied.context("custom apps need html or html_path")? }
        else { ensure!(supplied.is_none(), "markdown does not accept custom HTML"); MARKDOWN.to_string() };
    let csp = args.csp.unwrap_or_default();
    validate_csp(&csp)?;
    let data = data_bytes(&args.data.unwrap_or_else(|| json!({})))?;
    let now = now_ms();
    let mut panel = Panel { id: 0, channel_id: ch.id, session_id: s.id, window_id: s.window_id.or(ch.window_id),
        title: title(&args.title)?, app: args.app, csp, revision: 1, html_revision: 1,
        created_at: now, updated_at: now, closed: false, context: None, dropped_events: 0 };
    hub.store().create_panel(&mut panel, |p| {
        crate::paths::write_private(&dir(hub, p.id).join("app-1.html"), html.as_bytes())?;
        crate::paths::write_private(&dir(hub, p.id).join("data-1.json"), &data)?;
        Ok(())
    })?;
    hub.emit(ServerEvent::PanelUpserted(panel.clone()));
    Ok(panel.id)
}

pub fn update(hub: &Hub, session: SessionId, args: Update) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let mut p = own(hub, session, args.panel_id)?;
    ensure!(!p.closed, "panel is closed");
    let (_, ch) = caller(hub, session)?;
    let html = input_html(Path::new(&ch.cwd), args.html, args.html_path)?;
    ensure!(html.is_none() || p.app == "custom", "only custom apps accept HTML updates");
    if let Some(t) = args.title { p.title = title(&t)?; }
    let old = p.clone();
    let data = match args.data { Some(d) => data_bytes(&d)?, None => std::fs::read(dir(hub, p.id).join(format!("data-{}.json", p.revision)))? };
    p.revision += 1;
    p.updated_at = now_ms();
    if let Some(html) = html {
        let previous = std::fs::read_to_string(dir(hub, p.id).join(format!("app-{}.html", p.html_revision)))?;
        if html != previous {
            p.html_revision = p.revision;
            crate::paths::write_private(&dir(hub, p.id).join(format!("app-{}.html", p.html_revision)), html.as_bytes())?;
        }
    }
    crate::paths::write_private(&dir(hub, p.id).join(format!("data-{}.json", p.revision)), &data)?;
    hub.store().save_panel(&p)?;
    // The DB names complete immutable files, so a crash never publishes half an update.
    let _ = std::fs::remove_file(dir(hub, p.id).join(format!("data-{}.json", old.revision)));
    if old.html_revision != p.html_revision { let _ = std::fs::remove_file(dir(hub, p.id).join(format!("app-{}.html", old.html_revision))); }
    hub.emit(ServerEvent::PanelUpserted(p));
    Ok(())
}

pub fn resource(hub: &Hub, id: PanelId) -> anyhow::Result<Value> {
    let _op = hub.panels.operations.lock();
    let p = get(hub, id)?;
    ensure!(!p.closed, "panel is closed");
    let html = std::fs::read_to_string(dir(hub, id).join(format!("app-{}.html", p.html_revision)))?;
    let data: Value = serde_json::from_slice(&std::fs::read(dir(hub, id).join(format!("data-{}.json", p.revision)))?)?;
    Ok(json!({"panel": p, "html": html, "data": data}))
}

pub fn close(hub: &Hub, session: Option<SessionId>, id: PanelId, human: bool) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let mut p = match session { Some(s) => own(hub, s, id)?, None => get(hub, id)? };
    if p.closed { return Ok(()) }
    p.closed = true;
    p.updated_at = now_ms();
    if human {
        let event = PanelEvent { panel_id: id, kind: "closed".into(), payload: json!({}), at: now_ms(), context: None };
        hub.store().push_panel_event(&mut p, &event)?;
    } else { hub.store().save_panel(&p)?; }
    let window = p.window_id;
    hub.emit(ServerEvent::PanelUpserted(p));
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
    hub.store().push_panel_event(&mut p, &PanelEvent { panel_id: id, kind, payload, at: now, context: None })?;
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

pub fn archive(hub: &Hub, channel: ChannelId) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let mut windows = std::collections::BTreeSet::new();
    for p in list(hub)?.into_iter().filter(|p| p.channel_id == channel) {
        match std::fs::remove_dir_all(dir(hub, p.id)) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
        hub.emit(ServerEvent::PanelRemoved { id: p.id });
        hub.panels.event_rates.lock().remove(&p.id);
        if let Some(window) = p.window_id { windows.insert(window); }
    }
    hub.store().delete_channel_panels(channel)?;
    for window in windows { hub.panel_left_window(window); }
    Ok(())
}

struct Waiter<'a> { panels: &'a Panels, key: (ChannelId, Option<PanelId>) }
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut waits = self.panels.waiters.lock();
        if let Some(n) = waits.get_mut(&self.key) { *n -= 1; if *n == 0 { waits.remove(&self.key); } }
    }
}

pub async fn wait(hub: &Hub, session: SessionId, panel: Option<PanelId>, seconds: u64) -> anyhow::Result<Value> {
    ensure!(seconds <= 1800, "timeout_seconds must be at most 1800");
    let waiter = {
        let _op = hub.panels.operations.lock();
        let (_, ch) = caller(hub, session)?;
        if let Some(id) = panel { own(hub, session, id)?; }
        let key = (ch.id, panel);
        *hub.panels.waiters.lock().entry(key).or_default() += 1;
        Waiter { panels: &hub.panels, key }
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        {
            let _op = hub.panels.operations.lock();
            caller(hub, session)?;
            let panels: Vec<_> = list(hub)?.into_iter().filter(|p| p.channel_id == waiter.key.0 && panel.is_none_or(|id| id == p.id)).collect();
            let mut events = Vec::new();
            for p in &panels { events.extend(hub.store().panel_events(p)?); }
            if !events.is_empty() {
                hub.store().ack_panel_events(&panels, None)?;
                return Ok(json!({"events": events, "timed_out": false}));
            }
            if tokio::time::Instant::now() >= deadline { return Ok(json!({"events": [], "timed_out": true})); }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Called without an await under the operation lock. Queue acceptance and event deletion
/// commit together; waits never observe events which were already accepted for injection.
pub fn deliver_pending(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let _op = hub.panels.operations.lock();
    let panels = list(hub)?;
    let waits = hub.panels.waiters.lock();
    let mut batches: BTreeMap<SessionId, Vec<Panel>> = BTreeMap::new();
    for p in panels {
        if waits.contains_key(&(p.channel_id, None)) || waits.contains_key(&(p.channel_id, Some(p.id))) { continue }
        let Some(s) = hub.session(p.session_id) else { continue };
        if s.channel_id != p.channel_id || s.kind == SessionKind::Shell || s.state != AgentState::Idle || s.queued > 0 { continue }
        if hub.channel(p.channel_id).is_none_or(|c| c.archived) { continue }
        if !hub.store().panel_events(&p)?.is_empty() { batches.entry(s.id).or_default().push(p); }
    }
    drop(waits);
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
            hub.store().push_panel_event(&mut p, &PanelEvent { panel_id:id, kind:"click".into(), payload:json!(n), at:n, context:None }).unwrap();
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
        hub.store().fail_runtime_writes_for_test();
        assert!(hub.store().ack_panel_events(&[p.clone()], Some(&json!({"queue":"new"}))).is_err());
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
}
