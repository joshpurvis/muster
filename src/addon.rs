//! Window-owned add-on registry and the narrow agent-message bridge (docs/ADDONS.md).
use std::{collections::{HashMap, VecDeque}, path::PathBuf, sync::Arc, time::{Duration, Instant}};
use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use crate::{agents::{self, Origin}, hub::Hub};

pub const BY_WINDOW: &str = "Add-on changes and messages are allowed only from Muster's window. Open Add-on tabs there.";
pub const CHANGED: &str = "Changed in another window. Reopen Add-on tabs to see it.";
pub const NOT_ALLOWED: &str = "Not sent: this agent isn't allowed.";
pub const LIMIT: &str = "Not sent: limit of 30 an hour.";
pub const TEXT: &str = "Not sent: message must be 1-300 characters.";
pub const NOT_SENT: &str = "Not sent.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Addon {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default = "default_icon")]
    pub icon: String,
    #[serde(default)]
    pub agents: Vec<String>,
}
fn default_icon() -> String { "globe".into() }

// Numeric layout ids stay below Questions; collisions are refused at registration.
pub fn tab_hash(id: &str) -> u32 { id.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32) }
pub fn slug(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.as_bytes()[0].is_ascii_alphanumeric() && !id.ends_with('-')
}

/// A bounded, copyable origin, carried through the existing provenance and reply paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identity([u8; 46]);
impl Identity {
    pub fn parse(s: &str) -> Option<Self> {
        let id = s.strip_prefix("addon:")?;
        if !slug(id) { return None }
        let mut bytes = [0; 46];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Some(Self(bytes))
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0[..self.0.iter().position(|b| *b == 0).unwrap_or(46)]).unwrap()
    }
}

impl Addon {
    pub fn checked(mut self, ports: &[u16]) -> anyhow::Result<Self> {
        ensure!(slug(&self.id), "Use a short lower-case slug for the add-on id.");
        ensure!(!self.name.trim().is_empty() && self.name.chars().count() <= 40 && !self.name.chars().any(char::is_control),
            "Name must be 1–40 characters.");
        let target = crate::web_preview::parse(&self.url, &[])
            .map_err(|_| anyhow::anyhow!("Use a local address: http://localhost:<port> or http://127.0.0.1:<port>."))?;
        ensure!(!ports.contains(&target.port), "That's Muster's own address. Use your app's port.");
        self.url = target.url();
        ensure!(ICONS.contains(&self.icon.as_str()), "Choose an icon from the list.");
        for name in &self.agents { agents::check_name(name)?; }
        self.agents.sort(); self.agents.dedup();
        Ok(self)
    }
}

#[derive(Default)]
struct Data { addons: Vec<Addon>, revision: String, sent: HashMap<String, VecDeque<Instant>> }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot { pub addons: Vec<Addon>, pub revision: String }
pub struct Registry { path: PathBuf, data: Mutex<Data> }
impl Registry {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let read = || -> anyhow::Result<Vec<Addon>> {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut addons: Vec<Addon> = serde_json::from_slice(&bytes)?;
            let mut hashes = std::collections::HashSet::new();
            for a in &mut addons {
                *a = a.clone().checked(&[])?;
                ensure!(hashes.insert(tab_hash(&a.id)), "Duplicate add-on id or tab");
            }
            Ok(addons)
        };
        let addons = read().unwrap_or_else(|e| {
            let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let bad = path.with_file_name(format!("{}.bad-{timestamp}", path.file_name().unwrap_or_default().to_string_lossy()));
            tracing::warn!("Could not load add-on registry: {e}; starting with no add-ons and moving it to {}", bad.display());
            if let Err(e) = std::fs::rename(&path, &bad) { tracing::warn!("Could not set aside add-on registry: {e}"); }
            Vec::new()
        });
        Ok(Self { path, data: Mutex::new(Data { addons, revision: new_revision(), ..Default::default() }) })
    }
    pub fn list(&self, ports: &[u16]) -> Vec<Addon> {
        self.snapshot(ports).addons
    }
    pub fn snapshot(&self, ports: &[u16]) -> Snapshot {
        let data = self.data.lock();
        Snapshot { addons: data.addons.iter().filter_map(|a| a.clone().checked(ports).ok()).collect(), revision: data.revision.clone() }
    }
    pub fn save(&self, addon: Option<Addon>, id: &str, revision: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock();
        ensure!(revision == data.revision, CHANGED);
        let mut next = data.addons.clone();
        next.retain(|a| a.id != id);
        if let Some(a) = addon {
            ensure!(!next.iter().any(|b| tab_hash(&a.id) == tab_hash(&b.id)), "Try saving again to choose another add-on id.");
            next.push(a);
        }
        crate::paths::ensure_private_dir(self.path.parent().context("Registry needs a directory")?)?;
        crate::paths::write_private(&self.path, &serde_json::to_vec(&next)?)?;
        data.addons = next;
        data.revision = new_revision();
        Ok(())
    }
    fn claim(&self, id: &str, framed_origin: &str, agent: &str, text: &str, ports: &[u16], now: Instant) -> anyhow::Result<Origin> {
        let mut data = self.data.lock();
        // Count refusals too, before registration, origin, grant, text or agent lookup.
        let times = data.sent.entry(id.into()).or_default();
        while times.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600)) { times.pop_front(); }
        ensure!(times.len() < 30, LIMIT);
        times.push_back(now);
        let a = data.addons.iter().find(|a| a.id == id).context(NOT_ALLOWED)?;
        let target = crate::web_preview::parse(&a.url, ports).map_err(|_| anyhow::anyhow!(NOT_ALLOWED))?;
        // URL::origin in the window omits default ports.
        let mut origin = target.origin();
        if (target.https && target.port == 443) || (!target.https && target.port == 80) {
            origin.truncate(origin.rfind(':').unwrap());
        }
        ensure!(framed_origin == origin && a.agents.iter().any(|n| n == agent), NOT_ALLOWED);
        ensure!((1..=300).contains(&text.chars().count()), TEXT);
        Ok(Origin::Addon(Identity::parse(&format!("addon:{id}")).unwrap()))
    }
}
fn new_revision() -> String { format!("{:032x}", rand::random::<u128>()) }

pub async fn message(hub: &Arc<Hub>, id: &str, framed_origin: &str, name: &str, text: String) -> anyhow::Result<()> {
    let origin = hub.addons.claim(id, framed_origin, name, &text, &crate::panels::own_ports(hub), Instant::now())?;
    let mut agent = hub.agents().get(name).context(NOT_SENT)?;
    // The window's tick grants this first delivery. Later messages keep their origin and use
    // the usual recipient policy, hop, spend and daily limits.
    agent.accepts_origins.push(origin);
    agents::send_checked(hub, &agent, text, origin.as_str().into(), None, None, Some(origin)).await
        .map_err(|_| anyhow::anyhow!(NOT_SENT))?;
    Ok(())
}

const ICONS: &[&str] = &["check", "x", "alert", "info", "clock", "refresh", "external", "branch", "merge", "search", "rocket", "undo", "shield", "zap", "play", "pause", "dot", "chart", "table", "image", "doc", "pr", "list", "globe", "lock", "server", "db", "dollar", "flask", "sparkle", "plus", "minus", "arrowUp", "arrowDown", "arrowRight", "chevron", "maximize", "eye", "flag", "users", "terminal", "copy", "filter", "sort", "help"];

#[cfg(test)]
mod tests {
    use super::*;
    fn counter() -> Addon {
        Addon { id: "counter".into(), name: "Counter".into(), url: "HTTP://LOCALHOST:5173?count=0".into(), icon: default_icon(), agents: vec!["alpha".into()] }
    }
    fn registry() -> Registry {
        Registry::load(std::env::temp_dir().join(format!("muster-addon-{}/addons.json", rand::random::<u64>()))).unwrap()
    }
    #[test]
    fn addon_url_and_shape_rules() {
        assert_eq!(counter().checked(&[7433, 7434]).unwrap().url, "http://localhost:5173/?count=0");
        for url in ["http://[::1]:5173/", "http://127.1:5173", "http://127.0.0.1:7433", "https://localhost:7434", "file:///tmp/counter", "http://user@localhost:5173", "http://localhost:5173\\@elsewhere.test", "http://elsewhere.test:5173"] {
            assert!(Addon { url: url.into(), ..counter() }.checked(&[7433, 7434]).is_err(), "{url}");
        }
        for name in ["".into(), " ".into(), "a".repeat(41)] {
            assert!(Addon { name, ..counter() }.checked(&[]).is_err());
        }
        assert!(Addon { icon: "unknown".into(), ..counter() }.checked(&[]).is_err());
        assert!(serde_json::from_value::<Addon>(serde_json::json!({"id":"counter","name":"Counter","url":"http://localhost:5173","extra":true})).is_err());
        assert!(slug("counter")); assert!(!slug("../counter"));
    }
    #[test]
    fn addon_registry_round_trip_and_agent_grants() {
        let r = registry();
        let a = counter().checked(&[]).unwrap();
        r.save(Some(a.clone()), &a.id, &r.snapshot(&[]).revision).unwrap();
        assert_eq!(Registry::load(r.path.clone()).unwrap().list(&[]), vec![a.clone()]);
        assert!(r.claim("counter", "http://localhost:5173", "beta", "hello", &[], Instant::now()).is_err());
        assert!(r.claim("counter", "http://localhost:5173", "alpha", &"x".repeat(301), &[], Instant::now()).is_err());
        assert!(r.claim("counter", "http://localhost:5173", "alpha", "", &[], Instant::now()).is_err());
        assert!(r.claim("counter", "http://localhost:5173", "alpha", "hello", &[5173], Instant::now()).is_err());
        r.save(None, &a.id, &r.snapshot(&[]).revision).unwrap();
        assert!(r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], Instant::now()).is_err());
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_rolling_hour_and_origin_survive_replies() {
        let r = registry(); let a = counter().checked(&[]).unwrap(); r.save(Some(a), "counter", &r.snapshot(&[]).revision).unwrap();
        let now = Instant::now();
        let origin = r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now).unwrap();
        assert_eq!(origin.as_str(), "addon:counter");
        let route = agents::proposed_route(None, Some(origin));
        assert_eq!(route.hop, 0);
        let next = agents::outgoing(Some(route.prov(Some(1))));
        assert_eq!(next.hop, 1); assert_eq!(next.origin, origin);
        let json = serde_json::to_string(&route.prov(Some(1))).unwrap();
        assert_eq!(serde_json::from_str::<agents::Prov>(&json).unwrap(), route.prov(Some(1)));
        for _ in 1..30 { r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now).unwrap(); }
        assert!(r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now + Duration::from_secs(3599)).is_err());
        r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now + Duration::from_secs(3600)).unwrap();
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_stale_origin_cannot_use_new_grants() {
        let r = registry();
        let a = counter().checked(&[]).unwrap();
        r.save(Some(a.clone()), &a.id, &r.snapshot(&[]).revision).unwrap();
        let changed = Addon { url: "http://localhost:5174/".into(), agents: vec!["beta".into()], ..a };
        r.save(Some(changed), "counter", &r.snapshot(&[]).revision).unwrap();
        let now = Instant::now();
        assert_eq!(r.claim("counter", "http://localhost:5173", "beta", "hello", &[], now).unwrap_err().to_string(), NOT_ALLOWED);
        assert_eq!(r.claim("counter", "http://localhost:5174", "alpha", "hello", &[], now).unwrap_err().to_string(), NOT_ALLOWED);
        assert!(r.claim("counter", "http://localhost:5174", "beta", "hello", &[], now).is_ok());
        let default_port = Addon { url: "https://localhost:443/".into(), ..counter() }.checked(&[]).unwrap();
        r.save(Some(default_port), "counter", &r.snapshot(&[]).revision).unwrap();
        assert!(r.claim("counter", "https://localhost", "alpha", "hello", &[], now).is_ok());
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_every_refusal_consumes_the_hourly_allowance() {
        let r = registry();
        r.save(Some(counter().checked(&[]).unwrap()), "counter", &r.snapshot(&[]).revision).unwrap();
        let now = Instant::now();
        for i in 0..30 {
            let (origin, agent, text, expected) = match i % 4 {
                0 => ("http://localhost:5174", "alpha", "hello", NOT_ALLOWED),
                1 => ("http://localhost:5173", "beta", "hello", NOT_ALLOWED),
                2 => ("http://localhost:5173", "alpha", "", TEXT),
                _ => ("http://localhost:5173", "alpha", "hello", NOT_ALLOWED),
            };
            if i == 3 { r.save(None, "counter", &r.snapshot(&[]).revision).unwrap(); }
            let expected = if i >= 3 { NOT_ALLOWED } else { expected };
            assert_eq!(r.claim("counter", origin, agent, text, &[], now).unwrap_err().to_string(), expected);
        }
        r.save(Some(counter().checked(&[]).unwrap()), "counter", &r.snapshot(&[]).revision).unwrap();
        assert_eq!(r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now).unwrap_err().to_string(), LIMIT);
        assert!(r.claim("counter", "http://localhost:5173", "alpha", "hello", &[], now + Duration::from_secs(3600)).is_ok());
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_revision_prevents_restoring_grants_or_deleted_addons() {
        let r = registry();
        let a = counter().checked(&[]).unwrap();
        let initial = r.snapshot(&[]).revision;
        r.save(Some(a.clone()), &a.id, &initial).unwrap();
        let loaded = r.snapshot(&[]).revision;
        r.save(Some(Addon { agents: vec![], ..a.clone() }), &a.id, &loaded).unwrap();
        assert_eq!(r.save(Some(a.clone()), &a.id, &loaded).unwrap_err().to_string(), CHANGED);
        assert_eq!(r.save(None, &a.id, &loaded).unwrap_err().to_string(), CHANGED);
        assert!(r.list(&[])[0].agents.is_empty());
        let revoked = r.snapshot(&[]).revision;
        r.save(None, &a.id, &revoked).unwrap();
        assert_eq!(r.save(Some(a.clone()), &a.id, &revoked).unwrap_err().to_string(), CHANGED);
        assert!(r.list(&[]).is_empty());
        let reloaded = Registry::load(r.path.clone()).unwrap();
        assert_ne!(reloaded.snapshot(&[]).revision, r.snapshot(&[]).revision);
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_bad_registry_is_set_aside_and_loads_empty() {
        for bad in ["{", "{}", r#"[{"id":"../counter","name":"Counter","url":"http://localhost:5173"}]"#] {
            let r = registry();
            std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
            std::fs::write(&r.path, bad).unwrap();
            let loaded = Registry::load(r.path.clone()).unwrap();
            assert!(loaded.list(&[]).is_empty());
            assert!(!r.path.exists());
            let files: Vec<_> = std::fs::read_dir(r.path.parent().unwrap()).unwrap().map(|e| e.unwrap().path()).collect();
            assert_eq!(files.len(), 1);
            assert!(files[0].file_name().unwrap().to_str().unwrap().starts_with("addons.json.bad-"));
            assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), bad);
            std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn addon_address_errors_belong_to_the_dialog() {
        for url in ["file:///tmp/counter", "https://elsewhere.test", "http://127.1:5173"] {
            assert_eq!(Addon { url: url.into(), ..counter() }.checked(&[]).unwrap_err().to_string(),
                "Use a local address: http://localhost:<port> or http://127.0.0.1:<port>.");
        }
        assert_eq!(counter().checked(&[5173]).unwrap_err().to_string(), "That's Muster's own address. Use your app's port.");
    }
}
