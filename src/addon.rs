//! Window-owned add-on registry and the narrow agent-message bridge (docs/ADDONS.md).
use std::{collections::{HashMap, VecDeque}, path::PathBuf, sync::Arc, time::{Duration, Instant}};
use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use crate::{agents::{self, Origin}, hub::Hub};

pub const BY_WINDOW: &str = "Add-on changes and messages are allowed only from Muster's window. Open Add-on tabs there.";

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
        self.url = crate::web_preview::parse(&self.url, ports)
            .map_err(|e| anyhow::anyhow!(crate::web_preview::for_user(&e.to_string())))?.url();
        ensure!(ICONS.contains(&self.icon.as_str()), "Choose an icon from the list.");
        for name in &self.agents { agents::check_name(name)?; }
        self.agents.sort(); self.agents.dedup();
        Ok(self)
    }
}

#[derive(Default)]
struct Data { addons: Vec<Addon>, sent: HashMap<String, VecDeque<Instant>> }
pub struct Registry { path: PathBuf, data: Mutex<Data> }
impl Registry {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let addons = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, data: Mutex::new(Data { addons, ..Default::default() }) })
    }
    pub fn list(&self, ports: &[u16]) -> Vec<Addon> {
        self.data.lock().addons.iter().filter_map(|a| a.clone().checked(ports).ok()).collect()
    }
    pub fn save(&self, addon: Option<Addon>, id: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock();
        let mut next = data.addons.clone();
        next.retain(|a| a.id != id);
        if let Some(a) = addon {
            ensure!(!next.iter().any(|b| tab_hash(&a.id) == tab_hash(&b.id)), "Try saving again to choose another add-on id.");
            next.push(a);
        }
        crate::paths::ensure_private_dir(self.path.parent().context("Registry needs a directory")?)?;
        crate::paths::write_private(&self.path, &serde_json::to_vec(&next)?)?;
        data.addons = next;
        Ok(())
    }
    fn claim(&self, id: &str, agent: &str, text: &str, ports: &[u16], now: Instant) -> anyhow::Result<Origin> {
        let mut data = self.data.lock();
        let a = data.addons.iter().find(|a| a.id == id).context("This add-on was removed.")?;
        a.clone().checked(ports)?;
        ensure!(a.agents.iter().any(|n| n == agent), "This agent is not ticked for the add-on.");
        ensure!((1..=300).contains(&text.chars().count()), "Message must be 1–300 characters.");
        let times = data.sent.entry(id.into()).or_default();
        while times.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600)) { times.pop_front(); }
        ensure!(times.len() < 30, "This add-on has sent 30 messages in the last hour. Try later.");
        times.push_back(now);
        Ok(Origin::Addon(Identity::parse(&format!("addon:{id}")).unwrap()))
    }
}

pub async fn message(hub: &Arc<Hub>, id: &str, name: &str, text: String) -> anyhow::Result<()> {
    let mut agent = hub.agents().get(name).context("This agent was removed.")?;
    let origin = hub.addons.claim(id, name, &text, &crate::panels::own_ports(hub), Instant::now())?;
    // The window's tick grants this first delivery. Later messages keep their origin and use
    // the usual recipient policy, hop, spend and daily limits.
    agent.accepts_origins.push(origin);
    agents::send_checked(hub, &agent, text, origin.as_str().into(), None, None, Some(origin)).await?;
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
        r.save(Some(a.clone()), &a.id).unwrap();
        assert_eq!(Registry::load(r.path.clone()).unwrap().list(&[]), vec![a.clone()]);
        assert!(r.claim("counter", "beta", "hello", &[], Instant::now()).is_err());
        assert!(r.claim("counter", "alpha", &"x".repeat(301), &[], Instant::now()).is_err());
        assert!(r.claim("counter", "alpha", "", &[], Instant::now()).is_err());
        assert!(r.claim("counter", "alpha", "hello", &[5173], Instant::now()).is_err());
        r.save(None, &a.id).unwrap();
        assert!(r.claim("counter", "alpha", "hello", &[], Instant::now()).is_err());
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
    #[test]
    fn addon_rolling_hour_and_origin_survive_replies() {
        let r = registry(); let a = counter().checked(&[]).unwrap(); r.save(Some(a), "counter").unwrap();
        let now = Instant::now();
        let origin = r.claim("counter", "alpha", "hello", &[], now).unwrap();
        assert_eq!(origin.as_str(), "addon:counter");
        let route = agents::proposed_route(None, Some(origin));
        assert_eq!(route.hop, 0);
        let next = agents::outgoing(Some(route.prov(Some(1))));
        assert_eq!(next.hop, 1); assert_eq!(next.origin, origin);
        let json = serde_json::to_string(&route.prov(Some(1))).unwrap();
        assert_eq!(serde_json::from_str::<agents::Prov>(&json).unwrap(), route.prov(Some(1)));
        for _ in 1..30 { r.claim("counter", "alpha", "hello", &[], now).unwrap(); }
        assert!(r.claim("counter", "alpha", "hello", &[], now + Duration::from_secs(3599)).is_err());
        r.claim("counter", "alpha", "hello", &[], now + Duration::from_secs(3600)).unwrap();
        std::fs::remove_dir_all(r.path.parent().unwrap()).unwrap();
    }
}
