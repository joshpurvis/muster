//! OpenRouter spend and the daily dollar caps (docs/SCHEDULING_PLAN.md feature 7, docs/PROTOCOL.md
//! "Spend cap"). An overall daily cap over every OpenRouter agent is always on (default $5), and
//! each agent may have its own. At a cap, unattended messages (any origin but the human's) to an
//! OpenRouter agent are refused with `budget: …` and one alert per cap per day; a run already
//! going finishes, and what the human types is never blocked.
//!
//! How spend is measured: OpenRouter's `GET /api/v1/key` with the daemon's own key, which
//! reports what the key has spent (`usage`, all time) and today's part of it (`usage_daily`, the
//! UTC day), as billed: cache discounts, reasoning tokens and the provider OpenRouter routed to
//! all included. The overall cap reads `usage_daily` as it is. Per agent, every rise in `usage`
//! between two readings goes to the OpenRouter agents that were working in between (split
//! evenly when several were), or to "other use of the key" when none was. The key is read only
//! while an OpenRouter agent works (at most every minute, and once when they all stop), before
//! an unattended message to one when the last reading is over a minute old, and when the
//! Scheduling settings show it: nothing at all while they are idle. The key goes to curl on its
//! stdin (a config file), never in an argument, the environment, a log or a reply.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agents::{Agent, Origin};
use crate::hub::Hub;
use crate::model::*;
use crate::openrouter::Provider;
use crate::store::Store;

/// The overall daily cap's default: $5.
pub const DEFAULT_CAP_CENTS: u32 = 500;
/// Caps are 1 cent to $1000 a day.
pub const MAX_CAP_CENTS: u32 = 100_000;
const KEY_URL: &str = "https://openrouter.ai/api/v1/key";
/// A reading older than this is refreshed before an unattended message to an OpenRouter agent.
pub const FRESH_MS: i64 = 60_000;
/// Without a reading this recent, an unattended message is refused (spend unknown).
pub const STALE_MS: i64 = 15 * 60_000;
/// While an OpenRouter agent works, the key is read at most this often.
const SAMPLE_MS: i64 = 60_000;
const TICK: Duration = Duration::from_secs(10);
const LEDGER_KEY: &str = "openrouter-spend";

/// What `GET /api/v1/key` says (credits; OpenRouter's credits are dollars).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyUsage {
    pub usage: f64,
    pub usage_daily: Option<f64>,
}

pub fn parse_key_usage(body: &[u8]) -> anyhow::Result<KeyUsage> {
    let v: Value = serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("OpenRouter's answer isn't JSON: {e}"))?;
    let d = v.get("data").unwrap_or(&v);
    let usage = d.get("usage").and_then(Value::as_f64).ok_or_else(|| anyhow::anyhow!("OpenRouter's answer has no usage"))?;
    Ok(KeyUsage { usage, usage_daily: d.get("usage_daily").and_then(Value::as_f64) })
}

pub type Fetcher = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<KeyUsage>> + Send + Sync>;

/// Spend per UTC day, kept across restarts (`runtime_state`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Ledger {
    /// The UTC day these are for, `YYYY-MM-DD`.
    day: String,
    /// Dollars per agent id today.
    agents: BTreeMap<i64, f64>,
    /// Dollars spent through the key while no OpenRouter agent of Muster's was working.
    other: f64,
    /// The key's all-time usage at the last reading, and the UTC day of that reading: a rise
    /// across midnight counts toward today only up to the new day's `usage_daily`.
    last_usage: Option<f64>,
    last_usage_day: String,
    /// Caps already alerted today: "all", or an agent id.
    alerted: BTreeSet<String>,
}

#[derive(Default)]
struct State {
    ledger: Ledger,
    /// `usage_daily` at the last reading.
    today: Option<f64>,
    /// The last good reading (daemon clock, unix ms).
    fetched_at: Option<i64>,
    /// The last reading failed: why.
    error: Option<String>,
    /// OpenRouter agents seen in a turn (or sent an unattended prompt) since the last reading.
    active: BTreeSet<i64>,
    /// OpenRouter agents in a turn now (from session events).
    working_now: BTreeSet<i64>,
}

pub struct Spend {
    store: Store,
    fetcher: parking_lot::RwLock<Option<Fetcher>>,
    clock: parking_lot::RwLock<Arc<dyn Fn() -> i64 + Send + Sync>>,
    state: Mutex<State>,
    fetching: tokio::sync::Mutex<()>,
}

fn utc_day(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// The next 00:00 UTC after `ms`.
fn next_utc_midnight(ms: i64) -> i64 {
    (ms.div_euclid(86_400_000) + 1) * 86_400_000
}

fn dollars(cents: u32) -> f64 {
    cents as f64 / 100.0
}

pub fn money(v: f64) -> String {
    format!("${v:.2}")
}

impl Spend {
    pub fn load(store: &Store, key: Option<&crate::openrouter::ApiKey>) -> Spend {
        let ledger = store.load_runtime::<Ledger>(LEDGER_KEY).ok().flatten().unwrap_or_default();
        Spend {
            store: store.clone(),
            fetcher: parking_lot::RwLock::new(key.map(|k| curl_fetcher(k.clone()))),
            clock: parking_lot::RwLock::new(Arc::new(now_ms)),
            state: Mutex::new(State { ledger, ..Default::default() }),
            fetching: Default::default(),
        }
    }

    /// Tests read the key through their own fetcher and clock.
    #[cfg(test)]
    pub fn set_fetcher(&self, f: Option<Fetcher>, clock: Arc<dyn Fn() -> i64 + Send + Sync>) {
        *self.fetcher.write() = f;
        *self.clock.write() = clock;
    }

    #[cfg(test)]
    pub fn mark_active_for_test(&self, agent: i64) {
        self.state.lock().active.insert(agent);
    }

    fn now(&self) -> i64 {
        (self.clock.read())()
    }

    pub fn key_present(&self) -> bool {
        self.fetcher.read().is_some()
    }

    /// Read the key now (one reading at a time; a caller that waited for another's reading
    /// uses it when it is fresh). Attributes the rise since the last reading.
    pub async fn refresh(&self, hub: &Hub) {
        let Some(fetch) = self.fetcher.read().clone() else { return };
        let started = self.now();
        let _one = self.fetching.lock().await;
        if self.state.lock().fetched_at.is_some_and(|t| t >= started) {
            return;
        }
        let got = fetch().await;
        let now = self.now();
        let changed = {
            let mut st = self.state.lock();
            let day = utc_day(now);
            Self::roll(&mut st, &day);
            match got {
                Ok(u) => {
                    if let Some(last) = st.ledger.last_usage {
                        let mut delta = u.usage - last;
                        // The previous reading was another day's: only what the key spent today
                        // (OpenRouter's `usage_daily`) can be today's; the rest was yesterday's.
                        if st.ledger.last_usage_day != day {
                            delta = delta.min(u.usage_daily.unwrap_or(0.0).max(0.0));
                        }
                        if delta > 0.0 {
                            let active: Vec<i64> = std::mem::take(&mut st.active).into_iter().collect();
                            if active.is_empty() {
                                st.ledger.other += delta;
                            } else {
                                let share = delta / active.len() as f64;
                                for a in active {
                                    *st.ledger.agents.entry(a).or_default() += share;
                                }
                            }
                        }
                    }
                    // Agents still working stay marked: their turn goes on into the next reading.
                    st.active = st.working_now.clone();
                    st.ledger.last_usage = Some(u.usage);
                    st.ledger.last_usage_day = day.clone();
                    let sum: f64 = st.ledger.agents.values().sum::<f64>() + st.ledger.other;
                    st.today = Some(u.usage_daily.unwrap_or(sum));
                    st.fetched_at = Some(now);
                    st.error = None;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!("openrouter spend: {why}");
                    st.error = Some(why);
                }
            }
            if let Err(e) = self.store.save_runtime(LEDGER_KEY, &st.ledger) {
                tracing::warn!("saving the OpenRouter spend: {e:#}");
            }
            true
        };
        if changed {
            crate::schedules::spend_changed(hub);
        }
    }

    /// A new UTC day: today's totals start at zero (the last reading stays, as yesterday's).
    fn roll(st: &mut State, day: &str) {
        if st.ledger.day != day {
            st.ledger = Ledger {
                day: day.to_string(),
                last_usage: st.ledger.last_usage,
                last_usage_day: std::mem::take(&mut st.ledger.last_usage_day),
                ..Default::default()
            };
            st.today = None;
        }
    }

    /// Refresh when the last reading is older than `FRESH_MS` or from another UTC day.
    pub async fn ensure_fresh(&self, hub: &Hub) {
        let now = self.now();
        let stale = self.state.lock().fetched_at.is_none_or(|t| now - t >= FRESH_MS || utc_day(t) != utc_day(now));
        if stale {
            self.refresh(hub).await;
        }
    }

    /// May an unattended message go to OpenRouter agent `agent` now? `Err(reason)` at a cap
    /// (alerting once per cap per day), or when today's spend can't be read: no key in this
    /// backend (an OpenRouter process from an earlier one may still be running with its own), no
    /// reading yet, or none in the last 15 minutes or of today.
    pub fn check(&self, hub: &Hub, agent: &Agent, settings: &crate::schedules::Settings) -> Result<(), String> {
        if !self.key_present() {
            return Err(
                "budget: this backend has no OpenRouter key, so it can't read today's spend; unattended work on OpenRouter \
                 is refused until it has one (`env OPENROUTER_API_KEY=… muster reload`)"
                    .into(),
            );
        }
        let now = self.now();
        let mut st = self.state.lock();
        let day = utc_day(now);
        Self::roll(&mut st, &day);
        let fresh = st.fetched_at.is_some_and(|t| now - t < STALE_MS && utc_day(t) == day);
        if !fresh {
            let why = st.error.clone().unwrap_or_else(|| "no reading yet".into());
            return Err(format!(
                "budget: couldn't read today's OpenRouter spend ({why}), so unattended work on OpenRouter waits until it can"
            ));
        }
        let cap = dollars(settings.spend_cap_cents);
        let today = st.today.unwrap_or(0.0);
        let resets = "OpenRouter's day ends at 00:00 UTC";
        let (key, why) = if today >= cap {
            ("all".to_string(), format!(
                "budget: OpenRouter spend today is {} of the {} daily cap for all agents ({resets}; change it in Scheduling settings)",
                money(today), money(cap)))
        } else if let Some(c) = settings.agent_spend_caps.get(&agent.id).map(|c| dollars(*c))
            && st.ledger.agents.get(&agent.id).copied().unwrap_or(0.0) >= c
        {
            (agent.id.to_string(), format!(
                "budget: {} spent {} on OpenRouter today, its daily cap is {} ({resets}; change it in Scheduling settings)",
                agent.name, money(st.ledger.agents.get(&agent.id).copied().unwrap_or(0.0)), money(c)))
        } else {
            return Ok(());
        };
        if st.ledger.alerted.insert(key) {
            let _ = self.store.save_runtime(LEDGER_KEY, &st.ledger);
            drop(st);
            let summary = "OpenRouter daily budget reached";
            let body = format!("{}. Unattended work on OpenRouter is refused until then; what you type still goes.",
                why.trim_start_matches("budget: "));
            hub.notice("warn", format!("{summary}: {body}"));
            crate::notify::alert(summary, &body);
        }
        Err(why)
    }

    /// `spend` in `schedule.list` / `schedule.settings.get`: today's spend, the caps, per agent.
    pub fn json(&self, hub: &Hub, settings: &crate::schedules::Settings) -> Value {
        let st = self.state.lock();
        let now = self.now();
        let today_ok = st.ledger.day == utc_day(now);
        let agents: Vec<Value> = hub
            .agents()
            .list()
            .into_iter()
            .filter(|a| bills_openrouter(hub, a) || settings.agent_spend_caps.contains_key(&a.id))
            .map(|a| {
                json!({
                    "id": a.id, "name": a.name,
                    "spent": if today_ok { st.ledger.agents.get(&a.id).copied().unwrap_or(0.0) } else { 0.0 },
                    "cap": settings.agent_spend_caps.get(&a.id).map(|c| dollars(*c)),
                })
            })
            .collect();
        json!({
            "key_present": self.key_present(),
            "today": st.today.filter(|_| today_ok && st.fetched_at.is_some_and(|t| utc_day(t) == utc_day(now))),
            "cap": dollars(settings.spend_cap_cents),
            "other": if today_ok { st.ledger.other } else { 0.0 },
            "agents": agents,
            "checked_at": st.fetched_at,
            "error": st.error,
            "resets_at": next_utc_midnight(now),
        })
    }

    /// One tick of the sampler: note which OpenRouter agents work, and read the key while they
    /// do (and once when they stop).
    /// A session changed (every state change is an event, so a turn of any length is seen at
    /// its start): an OpenRouter agent in a turn is marked active until the reading after it.
    /// Returns whether the key should be read now: the last OpenRouter turn just ended.
    pub fn observe(&self, hub: &Hub, info: &SessionInfo) -> bool {
        let Some(agent) = hub.agents().by_session(info.id) else { return false };
        let turn = info.provider == Provider::OpenRouter
            && matches!(info.state, AgentState::Working | AgentState::Starting | AgentState::Stuck);
        let mut st = self.state.lock();
        let was = !st.working_now.is_empty();
        if turn {
            st.working_now.insert(agent.id);
            st.active.insert(agent.id);
        } else {
            st.working_now.remove(&agent.id);
        }
        was && st.working_now.is_empty()
    }

    /// A unattended prompt was submitted to OpenRouter agent `agent` (the injector): its turn
    /// starts now, whatever the screen shows yet.
    pub fn note_submitted(&self, agent: i64) {
        let mut st = self.state.lock();
        st.active.insert(agent);
    }

    /// The periodic part: while an OpenRouter agent works or has prompts waiting, read the key
    /// at most every minute (so queued unattended prompts are judged on a fresh reading).
    async fn tick(&self, hub: &Hub) {
        if !self.key_present() {
            return;
        }
        let waiting = hub.agents().list().into_iter().any(|a| {
            a.session_id.and_then(|s| hub.session(s)).is_some_and(|s| {
                s.provider == Provider::OpenRouter
                    && s.state != AgentState::Exited
                    && (s.queued > 0 || matches!(s.state, AgentState::Working | AgentState::Starting | AgentState::Stuck))
            })
        });
        let due = waiting && self.state.lock().fetched_at.is_none_or(|t| self.now() - t >= SAMPLE_MS);
        if due {
            self.refresh(hub).await;
        }
    }
}

/// May the injector submit this prompt now? For a message on an unattended chain (any origin but
/// the human's) to a session billing OpenRouter, the spend caps are checked again right before
/// it is typed and before Enter: a prompt queued before the cap (or before the reading went stale)
/// is taken back. `Some(reason)`: don't submit.
pub fn submit_gate(hub: &Hub, session: SessionId, source: &crate::agents::Source) -> Option<String> {
    let crate::agents::Source::Chain(p) = source else { return None };
    if p.origin == Origin::Human {
        return None;
    }
    let info = hub.session(session)?;
    let agent = hub.agents().by_session(session)?;
    if info.provider != Provider::OpenRouter && !bills_openrouter(hub, &agent) {
        return None;
    }
    hub.spend.check(hub, &agent, &hub.schedules.settings()).err()
}

/// Does `agent` bill OpenRouter: its record, or its running process (a switch applies from the
/// next start)?
pub fn bills_openrouter(hub: &Hub, agent: &Agent) -> bool {
    agent.provider == Provider::OpenRouter
        || agent
            .session_id
            .and_then(|s| hub.session(s))
            .is_some_and(|s| s.state != AgentState::Exited && s.provider == Provider::OpenRouter)
}

/// Does a message on `origin` to `agent` fall under the spend caps?
pub fn applies(hub: &Hub, agent: &Agent, origin: Origin) -> bool {
    origin != Origin::Human && bills_openrouter(hub, agent)
}

/// The injector took back a prompt `submit_gate` refused: a schedule's or trigger's run ends
/// refused (`budget: …`); any other message (an agent's ask on an unattended chain) is recorded as
/// refused, so it counts toward no cap and its log entry says why.
pub fn not_submitted(hub: &Arc<Hub>, source: &crate::agents::Source, why: String) {
    let crate::agents::Source::Chain(p) = source else { return };
    let Some(msg) = p.msg_id else { return };
    let from = hub.store().agent_message(msg).ok().flatten().map(|m| m.from).unwrap_or_default();
    if crate::schedules::is_run_sender(&from) {
        crate::schedules::not_submitted(hub, source, crate::schedules::plan::Action::Refuse(why));
        return;
    }
    match hub.store().set_agent_message_refused(msg, &why) {
        Ok(()) => crate::agents::publish_message(hub, msg),
        Err(e) => tracing::warn!(msg, "recording a budget refusal: {e:#}"),
    }
}

/// The sampler (daemon start).
pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        let mut rx = hub.subscribe();
        let mut every = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(crate::hub::ServerEvent::SessionUpserted(info)) => {
                        if hub.spend.key_present() && hub.spend.observe(&hub, &info) {
                            hub.spend.refresh(&hub).await;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
                _ = every.tick() => hub.spend.tick(&hub).await,
            }
            if hub.is_shutting_down() {
                return;
            }
        }
    });
}

/// The real fetcher: curl, https only, the key in a config read from stdin (never in argv).
/// Debug builds may point it at a test server with `MUSTER_TEST_OPENROUTER_KEY_URL`.
fn curl_fetcher(key: crate::openrouter::ApiKey) -> Fetcher {
    let test_url = if cfg!(debug_assertions) { std::env::var("MUSTER_TEST_OPENROUTER_KEY_URL").ok().filter(|u| !u.is_empty()) } else { None };
    Arc::new(move || {
        let key = key.clone();
        let url = test_url.clone();
        Box::pin(async move {
            let body = curl_key(&key, url.as_deref()).await?;
            parse_key_usage(&body)
        })
    })
}

async fn curl_key(key: &crate::openrouter::ApiKey, test_url: Option<&str>) -> anyhow::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let url = test_url.unwrap_or(KEY_URL);
    let proto = if test_url.is_some() { "=http,https" } else { "=https" };
    let args = ["-q", "-sS", "-f", "--proto", proto, "--max-redirs", "0", "--max-time", "15", "--max-filesize", "65536", "-K", "-"];
    let mut child = tokio::process::Command::new("curl")
        .args(args)
        .env_remove(crate::openrouter::ENV_KEY)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("running curl: {e}"))?;
    let config = format!("url = \"{url}\"\nheader = \"Authorization: Bearer {}\"\n", key.expose());
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(config.as_bytes()).await?;
    drop(stdin);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (mut so, mut se) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let work = async {
        let _ = (&mut so).take(65_536).read_to_end(&mut out).await;
        let _ = (&mut se).take(4096).read_to_end(&mut err).await;
        child.wait().await
    };
    let status = tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .map_err(|_| anyhow::anyhow!("OpenRouter didn't answer within 20 s"))??;
    if !status.success() {
        let why = String::from_utf8_lossy(&err);
        let why = why.trim().trim_start_matches("curl: ").replace(key.expose(), "<key>");
        anyhow::bail!("couldn't read the key's usage from OpenRouter ({})", crate::agents::truncate_bytes(&why, 200));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_endpoint_is_read() {
        let u = parse_key_usage(br#"{"data":{"label":"x","usage":12.5,"usage_daily":1.25,"limit":null}}"#).unwrap();
        assert_eq!(u, KeyUsage { usage: 12.5, usage_daily: Some(1.25) });
        let u = parse_key_usage(br#"{"data":{"usage":3}}"#).unwrap();
        assert_eq!(u.usage_daily, None);
        assert!(parse_key_usage(b"<html>").is_err());
        assert!(parse_key_usage(br#"{"data":{}}"#).is_err());
    }

    #[test]
    fn days_are_utc() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(next_utc_midnight(1), 86_400_000);
        assert_eq!(money(5.0), "$5.00");
    }
}
