//! Sleep and wake for named agents (PLAN.md §10, docs/UI.md "Agents panel").
//!
//! An asleep agent's CLI process is stopped while it is idle; its record, tab and conversation
//! stay, shown as asleep (`Agent::asleep_at`, saved, so it survives a backend reload and a full
//! restart, and autostart leaves it asleep). Whatever would deliver to it wakes it: restart
//! into its saved conversation (`--resume`, `codex resume`), wait until it is ready, then
//! deliver. Every wake goes through `agents::start` under the start lock, so two messages
//! arriving together wake it once, and every message keeps the one durable path it already
//! had (`agents::deliver` and the prompt queue), so none is lost or typed twice.
//!
//! What wakes it: a message (`ask_agent`, `muster ask`, a schedule run, after every guard rail
//! of those paths), the human's `muster send`, the human typing into its tab (the keys are held
//! here and typed once it is ready, `keys`), a real click in one of its panels, and Start (Wake)
//! in the agents panel. Nothing that doesn't need the agent wakes it: a panel closing (or any
//! other notice `panels::wakes` turns down), status checks, list and read tools, reading its
//! screen, the terminal's own reports (focus, mouse, cursor position), showing its tab.
//!
//! Sleeping is a per-agent setting, off by default (`sleep_after_min`), plus a manual Sleep.
//! Never while it is working, waiting for approval or a question, in a turn, with prompts
//! waiting or a message on its way, with text in its input box, or before its conversation id
//! is known (it couldn't be resumed): `blocker`.
//!
//! Waking re-reads the whole conversation; once its prompt cache has expired (src/cache.rs),
//! uncached, which costs the owner real usage. The tab says so before a wake, the wake is noted
//! in the timeline, and a message typed in after the wake gets the cold-start mark as any
//! message to a cold agent does.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;

use crate::adapters::adapter_for;
use crate::agents::Agent;
use crate::hub::Hub;
use crate::model::*;

/// The shortest and longest idle time an agent can be set to sleep after, in minutes.
pub const AFTER_MIN: u32 = 5;
pub const AFTER_MAX: u32 = 7 * 24 * 60;
/// The detail of an asleep agent's session (its state is `exited`).
pub const ASLEEP_DETAIL: &str = "asleep";
/// How long a sleep waits for the process to stop before giving up (awake after all).
const EXIT_WAIT: Duration = Duration::from_secs(10);
/// An automatic sleep never happens within this long of the human typing into the agent.
const HUMAN_QUIET_MS: i64 = 120_000;
/// How long keys typed into a waking agent are held for it to be ready.
const READY_WAIT: Duration = Duration::from_secs(600);
/// The most typed bytes held for one waking agent.
const KEYS_MAX: usize = 64 * 1024;
/// After a wake failed, how long before a panel click or keys may try again.
const WAKE_RETRY: Duration = Duration::from_secs(60);

/// One minute of the idle time, in ms. A debug build can shorten it for the smoke test.
fn minute_ms() -> i64 {
    if cfg!(debug_assertions)
        && let Some(v) = std::env::var("MUSTER_TEST_SLEEP_MINUTE_MS").ok().and_then(|v| v.parse().ok())
    {
        return v;
    }
    60_000
}

/// How often idle agents are checked.
fn check_every() -> Duration {
    Duration::from_millis((minute_ms() / 2).clamp(200, 30_000) as u64)
}

/// A stored idle time out of range (written by hand): the nearest allowed one.
pub fn clamp_after(v: i64) -> u32 {
    v.clamp(i64::from(AFTER_MIN), i64::from(AFTER_MAX)) as u32
}

/// `sleep_after_min` as given: 0 or absent turns sleeping off.
pub fn check_after(v: Option<u32>) -> anyhow::Result<Option<u32>> {
    match v {
        None | Some(0) => Ok(None),
        Some(m) if (AFTER_MIN..=AFTER_MAX).contains(&m) => Ok(Some(m)),
        Some(_) => anyhow::bail!("sleep_after_min: {AFTER_MIN} to {AFTER_MAX} minutes, or 0 to never sleep"),
    }
}

/// What the keystroke path needs, under one lock that never waits on SQLite or the hub.
#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Asleep agents' sessions, to their agent (mirrors `Agent::asleep_at`).
    asleep: HashMap<SessionId, i64>,
    /// Wakes in progress until the agent is ready: agent -> the tab being woken.
    waking: HashMap<i64, SessionId>,
    /// Keys the human typed into an asleep or waking agent's tab, typed once it is ready.
    keys: HashMap<SessionId, Vec<u8>>,
    /// Wakes asked for (keys, a panel click) whose start hasn't returned yet.
    requested: HashSet<i64>,
    /// Messages on their way to each agent (`agents::deliver`): a sleep waits for none.
    deliveries: HashMap<i64, u32>,
}

impl State {
    pub(crate) fn set_asleep(&self, session: Option<SessionId>, agent: i64, on: bool) {
        let mut g = self.inner.lock();
        g.asleep.retain(|_, a| *a != agent);
        if on && let Some(s) = session {
            g.asleep.insert(s, agent);
        }
    }

    /// Session `id` is an asleep agent's.
    pub fn is_asleep(&self, id: SessionId) -> bool {
        self.inner.lock().asleep.contains_key(&id)
    }

    /// Some agent is asleep (a cheap check for the panel tick).
    pub fn any_asleep(&self) -> bool {
        !self.inner.lock().asleep.is_empty()
    }

    fn deliveries(&self, agent: i64) -> u32 {
        self.inner.lock().deliveries.get(&agent).copied().unwrap_or(0)
    }

    fn waking(&self, agent: i64) -> bool {
        let g = self.inner.lock();
        g.waking.contains_key(&agent) || g.requested.contains(&agent)
    }
}

/// A message on its way to an agent, from before it is started until it is in the prompt
/// queue (`agents::deliver`). A sleep refuses while one is.
pub(crate) struct Delivering {
    hub: Arc<Hub>,
    agent: i64,
}

pub(crate) fn delivering(hub: &Arc<Hub>, agent: i64) -> Delivering {
    *hub.agents().sleep.inner.lock().deliveries.entry(agent).or_default() += 1;
    Delivering { hub: hub.clone(), agent }
}

impl Drop for Delivering {
    fn drop(&mut self) {
        let mut g = self.hub.agents().sleep.inner.lock();
        if let Some(n) = g.deliveries.get_mut(&self.agent) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.deliveries.remove(&self.agent);
            }
        }
    }
}

/// What `blocker` reads about an agent's session besides its info.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Its process runs.
    pub live: bool,
    /// No prompt waits in its queue, none is being typed or paused for review.
    pub queue_idle: bool,
    /// Text sits in its input box.
    pub draft: bool,
    /// Messages on their way to it.
    pub deliveries: u32,
    /// A wake is in progress.
    pub waking: bool,
    /// When the agent message it is running was typed in (`Registry::active_delivered_at`).
    pub active_at: Option<i64>,
}

/// Why `agent` can't sleep now, in words for the human (None: it can).
pub fn blocker(agent: &Agent, info: Option<&SessionInfo>, f: &Facts) -> Option<String> {
    let Some(info) = info.filter(|i| f.live && i.state != AgentState::Exited) else {
        return Some("it isn't running".into());
    };
    if agent.resume_id.is_none() {
        return Some("its conversation isn't saved yet (it needs a finished turn first), so it couldn't pick it up again".into());
    }
    let state = match info.state {
        AgentState::Idle => None,
        AgentState::Starting => Some("it is starting"),
        AgentState::Working => Some("it is working"),
        AgentState::NeedsApproval => Some("it is waiting for approval"),
        AgentState::AskingQuestion => Some("it is asking a question"),
        AgentState::Error => Some("it shows an error"),
        AgentState::Stuck => Some("it looks stuck"),
        AgentState::Exited => Some("it isn't running"),
    };
    if let Some(s) = state {
        return Some(s.into());
    }
    if info.subagents > 0 {
        return Some("its sub-agents are still working".into());
    }
    if !f.queue_idle || info.queued > 0 {
        return Some("it has prompts waiting to be typed".into());
    }
    if f.deliveries > 0 {
        return Some("a message to it is on its way".into());
    }
    if f.waking {
        return Some("it is waking up".into());
    }
    // A prompt typed since it last came to rest: its turn is only starting.
    if f.active_at.is_some_and(|at| at >= info.state_since) {
        return Some("it is in the middle of a turn".into());
    }
    if f.draft {
        return Some("there is unsent text in its input box".into());
    }
    None
}

/// When `info` last did anything: came to rest, talked to its model, or got the human's keys.
pub fn idle_since(info: &SessionInfo, human_at: Option<i64>) -> i64 {
    info.state_since.max(info.last_turn_at.unwrap_or(0)).max(human_at.unwrap_or(0))
}

/// Has `agent` been idle long enough to sleep on its own at `now`?
pub fn due(agent: &Agent, info: &SessionInfo, human_at: Option<i64>, now: i64) -> bool {
    let Some(after) = agent.sleep_after_min else { return false };
    if human_at.is_some_and(|t| now - t < HUMAN_QUIET_MS) {
        return false;
    }
    now - idle_since(info, human_at) >= i64::from(after) * minute_ms()
}

/// When the human last typed into session `id` (Unix ms), as far as this backend saw.
fn human_at(hub: &Hub, id: SessionId, now: i64) -> Option<i64> {
    hub.human_input_of(id).map(|(_, at)| now - at.elapsed().as_millis() as i64)
}

fn facts(hub: &Hub, a: &Agent) -> (Option<SessionInfo>, Facts) {
    let Some(sid) = a.session_id else { return (None, Facts::default()) };
    let info = hub.session(sid);
    let f = Facts {
        live: hub.live_session_ids().contains(&sid),
        queue_idle: crate::inject::queue_idle(hub, sid),
        draft: info.as_ref().is_some_and(|i| i.state == AgentState::Idle) && crate::inject::has_draft(hub, sid),
        deliveries: hub.agents().sleep.deliveries(a.id),
        waking: hub.agents().sleep.waking(a.id),
        active_at: hub.agents().active_delivered_at(sid),
    };
    (info, f)
}

/// Why a sleep was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The human (Sleep in the agents panel, `muster agent sleep`).
    Manual,
    /// Its idle time ran out (`sleep_after_min`).
    Idle,
}

/// The agent can't sleep now; the text says why.
#[derive(Debug)]
pub struct NotNow(pub String);

impl std::fmt::Display for NotNow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotNow {}

/// Put agent `name` to sleep: stop its process once nothing needs it (`blocker`, checked under
/// the start lock, which every wake also takes), keeping its record, tab and conversation.
/// Already asleep: nothing to do.
pub async fn sleep(hub: &Arc<Hub>, name: &str, why: Why) -> anyhow::Result<Agent> {
    let _one = hub.agents().start_lock.lock().await;
    let a = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
    if a.asleep_at.is_some() {
        return Ok(a);
    }
    let (info, f) = facts(hub, &a);
    if let Some(no) = blocker(&a, info.as_ref(), &f) {
        return Err(NotNow(format!("{} can't sleep now: {no}", a.name)).into());
    }
    let info = info.expect("blocker checked the session");
    let now = now_ms();
    if why == Why::Idle && !due(&a, &info, human_at(hub, info.id, now), now) {
        return Err(NotNow(format!("{} was busy again", a.name)).into());
    }
    hub.agents().set_asleep(hub.store(), a.id, Some(now))?;
    let stopped = match hub.stop_for_sleep(info.id) {
        Ok(_) => wait_exited(hub, info.id).await,
        Err(e) => Err(e),
    };
    if let Err(e) = stopped {
        // Awake after all: never shown asleep while its process may still run.
        hub.agents().set_asleep(hub.store(), a.id, None)?;
        if let Some(a) = hub.agents().by_id(a.id) {
            crate::agents::publish(hub, &a);
        }
        return Err(e.context(format!("{} couldn't be put to sleep", a.name)));
    }
    let a = hub.agents().by_id(a.id).ok_or_else(|| anyhow::anyhow!("{name} was removed"))?;
    crate::agents::publish(hub, &a);
    let idle = now - idle_since(&info, human_at(hub, info.id, now));
    let text = match why {
        Why::Manual => format!("{} is asleep. The next message wakes it.", a.name),
        Why::Idle => format!(
            "{} fell asleep after {} idle. The next message wakes it.",
            a.name,
            crate::cache::idle_text(idle)
        ),
    };
    tracing::info!(agent = a.name, "{text}");
    hub.log_event(Some(info.channel_id), Some(info.id), EventKind::System, json!({
        "text": text, "level": "info", "asleep": {"agent": a.name, "idle_ms": idle, "manual": why == Why::Manual},
    }));
    Ok(a)
}

async fn wait_exited(hub: &Hub, id: SessionId) -> anyhow::Result<()> {
    let mut rx = hub.subscribe();
    let deadline = tokio::time::Instant::now() + EXIT_WAIT;
    loop {
        let gone = hub.session(id).is_none_or(|s| s.state == AgentState::Exited) && !hub.live_session_ids().contains(&id);
        if gone {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("its process didn't stop within {} s", EXIT_WAIT.as_secs());
        }
        crate::mcp::wait::next_change(&mut rx, id, (deadline - now).min(Duration::from_millis(250))).await;
    }
}

/// Agent `agent` is starting (`agents::start`, a restart of its tab): awake from now on.
/// Returns since when it was asleep, when it was. Keys typed into its tab while it wakes are
/// held until it is ready (`settle`).
pub(crate) fn on_start(hub: &Arc<Hub>, agent: &Agent) -> anyhow::Result<Option<i64>> {
    let Some(cur) = hub.agents().by_id(agent.id).filter(|a| a.asleep_at.is_some()) else { return Ok(None) };
    let tab = cur.session_id;
    let spawn = {
        let mut g = hub.agents().sleep.inner.lock();
        match tab {
            Some(tab) => g.waking.insert(cur.id, tab).is_none(),
            None => false,
        }
    };
    let before = match hub.agents().set_asleep(hub.store(), cur.id, None) {
        Ok(b) => b,
        Err(e) => {
            hub.agents().sleep.inner.lock().waking.remove(&cur.id);
            return Err(e);
        }
    };
    if let Some(a) = hub.agents().by_id(cur.id) {
        crate::agents::publish(hub, &a);
    }
    if spawn && let Some(tab) = tab {
        tokio::spawn(settle(hub.clone(), cur.id, tab));
    }
    Ok(before.asleep_at)
}

/// Session `id` is being restarted: if it is an asleep agent's, that agent is awake from now on.
pub(crate) fn on_session_start(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    if !hub.agents().sleep.is_asleep(id) {
        return Ok(());
    }
    if let Some(a) = hub.agents().by_session(id) {
        on_start(hub, &a)?;
    }
    Ok(())
}

/// The wake of agent `agent` (from tab `tab`) is done once its session is ready: type the keys
/// held for it, if any, when no prompt is being typed (else wait for that), then stop holding
/// keys. Gives up after `READY_WAIT`, or when the run ends, dropping what was held (and saying so).
async fn settle(hub: Arc<Hub>, agent: i64, tab: SessionId) {
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    let mut rx = hub.subscribe();
    let mut started = false;
    loop {
        let cur = hub.agents().by_id(agent).and_then(|a| a.session_id);
        let info = cur.and_then(|s| hub.session(s));
        let live = cur.is_some_and(|s| hub.live_session_ids().contains(&s));
        if let Some(i) = &info
            && live
        {
            started |= i.state != AgentState::Exited;
            if adapter_for(i.kind).is_ready(i.state) && flush(&hub, agent, tab, i.id) {
                return;
            }
        }
        let now = tokio::time::Instant::now();
        // Its run ended (or never started) before it was ready, or it took too long.
        let ended = started && info.as_ref().is_none_or(|i| i.state == AgentState::Exited);
        if now >= deadline || ended || hub.is_shutting_down() || hub.agents().by_id(agent).is_none() {
            give_up(&hub, agent, tab);
            return;
        }
        crate::mcp::wait::next_change(&mut rx, cur.unwrap_or(tab), (deadline - now).min(Duration::from_millis(500))).await;
    }
}

/// Type what was held for `tab` into session `to`, now ready. True when the wake is done.
fn flush(hub: &Hub, agent: i64, tab: SessionId, to: SessionId) -> bool {
    let mut g = hub.agents().sleep.inner.lock();
    if g.keys.get(&tab).is_none_or(|k| k.is_empty()) {
        g.keys.remove(&tab);
        g.waking.remove(&agent);
        return true;
    }
    // Never into a prompt Muster is typing: wait until the queue is idle (and ready again).
    crate::inject::while_queue_idle(hub, to, || {
        let keys = g.keys.remove(&tab).unwrap_or_default();
        if let Err(e) = hub.send_keys(to, &keys) {
            tracing::warn!(session = to, "typing the keys held while it woke: {e:#}");
        }
        g.waking.remove(&agent);
    })
    .is_some()
}

fn give_up(hub: &Hub, agent: i64, tab: SessionId) {
    let dropped = {
        let mut g = hub.agents().sleep.inner.lock();
        g.waking.remove(&agent);
        g.keys.remove(&tab).is_some_and(|k| !k.is_empty())
    };
    if dropped {
        let name = hub.agents().by_id(agent).map(|a| a.name).unwrap_or_else(|| "the agent".into());
        hub.notice("warn", format!("{name} didn't get ready after waking, so what you typed into its tab meanwhile wasn't sent."));
    }
}

/// The human's keys for session `id` (a client window or a paired phone). True: taken here,
/// because it is an asleep or waking agent's tab: held until it is ready (and an asleep one
/// starts waking, if they typed something), or dropped (the terminal's own reports).
pub fn keys(hub: &Arc<Hub>, id: SessionId, bytes: &[u8]) -> bool {
    let wake = {
        let mut g = hub.agents().sleep.inner.lock();
        let waking = g.waking.values().any(|t| *t == id);
        let asleep = g.asleep.get(&id).copied();
        if !waking && asleep.is_none() && !g.keys.contains_key(&id) {
            return false;
        }
        let (kept, typing) = typed(bytes);
        let held = g.keys.get(&id).map_or(0, Vec::len);
        if !kept.is_empty() && held + kept.len() <= KEYS_MAX && (typing || held > 0) {
            g.keys.entry(id).or_default().extend_from_slice(&kept);
        }
        match asleep {
            Some(agent) if typing && !waking && !g.requested.contains(&agent) => Some(agent),
            _ => None,
        }
    };
    if let Some(agent) = wake {
        wake_soon(hub, agent, Some(id), "you typed into its tab", true);
    }
    true
}

/// The human's typing in `bytes`: the bytes worth typing into a new run (the terminal's own
/// reports, cursor and function keys, and Alt chords dropped; bracketed-paste markers kept), and
/// whether there is typing at all (a printable character, Enter or Tab). Focus and mouse reports
/// never wake an agent.
pub fn typed(bytes: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut typing = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            if b >= 0x20 && b != 0x7f || matches!(b, b'\r' | b'\n' | b'\t') {
                typing = true;
            }
            out.push(b);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        match bytes.get(i) {
            Some(b'[') => {
                i += 1;
                // An X10 mouse report: ESC [ M and three bytes.
                if bytes.get(i) == Some(&b'M') {
                    i += 4;
                    continue;
                }
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                let end = (i + 1).min(bytes.len());
                let seq = &bytes[start..end];
                if seq == b"\x1b[200~" || seq == b"\x1b[201~" {
                    out.extend_from_slice(seq);
                }
                i = end;
            }
            Some(b']' | b'P' | b'_' | b'^') => {
                // OSC, DCS, APC, PM: up to BEL or ST.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // SS3 (F1-F4, cursor keys in application mode).
            Some(b'O') => i += 2,
            // Alt+key.
            Some(_) => i += 1,
            // A lone Esc.
            None => {}
        }
    }
    (out, typing)
}

/// Wake agent `agent` in the background (keys typed into `tab`, a panel click): once, however
/// often it is asked before the start returns.
pub(crate) fn wake_soon(hub: &Arc<Hub>, agent: i64, tab: Option<SessionId>, cause: &'static str, say_cost: bool) {
    if !hub.agents().sleep.inner.lock().requested.insert(agent) {
        return;
    }
    let hub = hub.clone();
    tokio::spawn(async move {
        let name = hub.agents().by_id(agent).map(|a| a.name);
        let r = match &name {
            Some(n) => crate::agents::start(&hub, n, false, None).await,
            None => Err(anyhow::anyhow!("it was removed")),
        };
        match r {
            Ok(s) => {
                if let Some(at) = s.woke {
                    note_woke(&hub, &s.agent, &s.session, at, cause, say_cost);
                }
            }
            Err(e) => {
                if let Some(tab) = tab {
                    hub.agents().sleep.inner.lock().keys.remove(&tab);
                }
                let name = name.unwrap_or_else(|| "The agent".into());
                tracing::warn!(agent = name, "waking: {e:#}");
                hub.notice("warn", format!("{name} couldn't be woken: {e:#}"));
                // Not asked again right away (a panel's events would ask every tick).
                tokio::time::sleep(WAKE_RETRY).await;
            }
        }
        hub.agents().sleep.inner.lock().requested.remove(&agent);
    });
}

/// `session.send` from the human (`muster send`) to session `id`: if it is an asleep agent's,
/// wake it first. Returns the session to send to and the guard that keeps it from sleeping
/// until the prompt is queued (also for an awake agent).
pub(crate) async fn for_send(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<(SessionId, Option<Delivering>)> {
    let Some(a) = hub.agents().by_session(id) else { return Ok((id, None)) };
    let guard = delivering(hub, a.id);
    if a.asleep_at.is_none() {
        return Ok((id, Some(guard)));
    }
    let s = crate::agents::start(hub, &a.name, false, None).await?;
    if let Some(at) = s.woke {
        note_woke(hub, &s.agent, &s.session, at, "you sent it a message", true);
    }
    Ok((s.session.id, Some(guard)))
}

/// How long `info` had been idle when its cache had likely expired at `now` (src/cache.rs),
/// else None. Unlike `cache::is_cold` it also reads an exited (asleep) session.
pub fn wake_cost(info: &SessionInfo, now: i64) -> Option<i64> {
    info.cache_cold_at.filter(|t| now >= *t).map(|_| now - info.last_turn_at.unwrap_or(now))
}

/// Say in the agent's timeline that `cause` woke it (asleep since `asleep_at`); `say_cost`: and,
/// when its cache had likely expired, what the wake costs (a message Muster types in gets the
/// cold-start note of its own instead).
pub(crate) fn note_woke(hub: &Hub, agent: &Agent, session: &SessionInfo, asleep_at: i64, cause: &str, say_cost: bool) {
    let now = now_ms();
    let cold = wake_cost(session, now);
    let mut text = format!(
        "Woke {} after {} asleep: {cause}.",
        agent.name,
        crate::cache::idle_text(now - asleep_at)
    );
    if say_cost && let Some(idle) = cold {
        text.push_str(&format!(
            " It had been idle {}: its prompt cache had likely expired, so it re-reads the whole conversation at full cost.",
            crate::cache::idle_text(idle)
        ));
    }
    tracing::info!(agent = agent.name, "{text}");
    hub.log_event(Some(session.channel_id), Some(session.id), EventKind::System, json!({
        "text": text, "level": "info",
        "woke": {"agent": agent.name, "asleep_ms": now - asleep_at, "cold_idle_ms": cold, "cause": cause},
    }));
}

/// Agents asleep in this backend's records whose process runs (a sleep a crash interrupted
/// before the stop): awake after all.
fn reconcile(hub: &Hub) {
    let live = hub.live_session_ids();
    for a in hub.agents().list() {
        if a.asleep_at.is_some() && a.session_id.is_some_and(|s| live.contains(&s)) {
            match hub.agents().set_asleep(hub.store(), a.id, None) {
                Ok(_) => {
                    tracing::info!(agent = a.name, "was marked asleep but its process runs: awake");
                    if let Some(a) = hub.agents().by_id(a.id) {
                        crate::agents::publish(hub, &a);
                    }
                }
                Err(e) => tracing::warn!(agent = a.name, "waking a running agent's record: {e:#}"),
            }
        }
    }
}

/// Put agents to sleep once their idle time runs out.
async fn sweep(hub: &Arc<Hub>) {
    let now = now_ms();
    for a in hub.agents().list() {
        if a.sleep_after_min.is_none() || a.asleep_at.is_some() {
            continue;
        }
        let (info, f) = facts(hub, &a);
        let Some(info) = info else { continue };
        if blocker(&a, Some(&info), &f).is_some() || !due(&a, &info, human_at(hub, info.id, now), now) {
            continue;
        }
        if let Err(e) = sleep(hub, &a.name, Why::Idle).await {
            tracing::debug!(agent = a.name, "not asleep: {e:#}");
        }
    }
}

/// At daemon start: fix records a crash left, then check idle agents from time to time.
pub fn spawn(hub: Arc<Hub>) {
    reconcile(&hub);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(check_every());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if hub.is_shutting_down() {
                return;
            }
            sweep(&hub).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Agent {
        Agent {
            account_profile_id: None,
            provider: crate::openrouter::Provider::Subscription,
            id: 1,
            name: "dozy".into(),
            kind: SessionKind::Codex,
            model: None,
            effort: None,
            persona: String::new(),
            home_dir: "/tmp/dozy".into(),
            permissions: Permissions::default(),
            accepts_origins: vec![crate::agents::Origin::Human],
            channel_id: Some(1),
            session_id: Some(2),
            resume_id: Some("thread-1".into()),
            created_at: 0,
            autostart: false,
            sleep_after_min: Some(30),
            asleep_at: None,
        }
    }

    fn info(state: AgentState) -> SessionInfo {
        let mut s: SessionInfo = serde_json::from_value(json!({
            "id": 2, "channel_id": 1, "kind": "codex", "name": "dozy", "cwd": "/", "state": "idle",
            "state_detail": null, "state_since": 1_000, "title": null, "parent_id": null, "pid": 7,
            "exit_code": null, "cols": 80, "rows": 24, "queued": 0, "created_at": 0,
        }))
        .unwrap();
        s.state = state;
        s
    }

    fn ready() -> Facts {
        Facts { live: true, queue_idle: true, ..Facts::default() }
    }

    #[test]
    fn only_an_idle_resumable_agent_with_nothing_pending_sleeps() {
        let a = agent();
        assert_eq!(blocker(&a, Some(&info(AgentState::Idle)), &ready()), None);
        let why = |a: &Agent, i: SessionInfo, f: Facts| blocker(a, Some(&i), &f).unwrap();
        for (state, words) in [
            (AgentState::Working, "working"),
            (AgentState::NeedsApproval, "approval"),
            (AgentState::AskingQuestion, "question"),
            (AgentState::Starting, "starting"),
            (AgentState::Error, "error"),
            (AgentState::Stuck, "stuck"),
            (AgentState::Exited, "isn't running"),
        ] {
            assert!(why(&a, info(state), ready()).contains(words), "{state:?}");
        }
        assert!(why(&a, info(AgentState::Idle), Facts { live: false, ..ready() }).contains("isn't running"));
        let fresh = Agent { resume_id: None, ..agent() };
        assert!(why(&fresh, info(AgentState::Idle), ready()).contains("couldn't pick it up again"));
        let mut queued = info(AgentState::Idle);
        queued.queued = 1;
        assert!(why(&a, queued, ready()).contains("waiting"));
        assert!(why(&a, info(AgentState::Idle), Facts { queue_idle: false, ..ready() }).contains("waiting"));
        let mut sub = info(AgentState::Idle);
        sub.subagents = 2;
        assert!(why(&a, sub, ready()).contains("sub-agents"));
        assert!(why(&a, info(AgentState::Idle), Facts { deliveries: 1, ..ready() }).contains("on its way"));
        assert!(why(&a, info(AgentState::Idle), Facts { waking: true, ..ready() }).contains("waking"));
        assert!(why(&a, info(AgentState::Idle), Facts { draft: true, ..ready() }).contains("unsent text"));
        // A message typed in since it came to rest: its turn is starting.
        assert!(why(&a, info(AgentState::Idle), Facts { active_at: Some(1_000), ..ready() }).contains("middle of a turn"));
        // One whose turn ended (it came to rest after it was typed in) no longer counts.
        assert_eq!(blocker(&a, Some(&info(AgentState::Idle)), &Facts { active_at: Some(999), ..ready() }), None);
    }

    #[test]
    fn idle_time_counts_from_the_last_sign_of_life() {
        let a = agent();
        let mut i = info(AgentState::Idle);
        i.last_turn_at = Some(5_000);
        let m = minute_ms();
        assert!(!due(&a, &i, None, 5_000 + 30 * m - 1));
        assert!(due(&a, &i, None, 5_000 + 30 * m));
        // The human's keys count as activity, and nothing sleeps within two minutes of them.
        assert!(!due(&a, &i, Some(6_000), 5_000 + 30 * m));
        assert!(due(&a, &i, Some(6_000), 6_000 + 30 * m));
        assert!(!due(&Agent { sleep_after_min: None, ..agent() }, &i, None, i64::MAX / 2));
    }

    #[test]
    fn the_setting_is_off_or_in_range() {
        assert_eq!(check_after(None).unwrap(), None);
        assert_eq!(check_after(Some(0)).unwrap(), None);
        assert_eq!(check_after(Some(60)).unwrap(), Some(60));
        assert!(check_after(Some(AFTER_MIN - 1)).is_err());
        assert!(check_after(Some(AFTER_MAX + 1)).is_err());
        assert_eq!((clamp_after(1), clamp_after(i64::MAX)), (AFTER_MIN, AFTER_MAX));
        // `muster agent new/edit --sleep-after`.
        let p = crate::cli::parse_sleep_after;
        assert_eq!((p("off").unwrap(), p("45").unwrap(), p("90m").unwrap(), p("2h").unwrap()), (0, 45, 90, 120));
        assert!(p("1").is_err() && p("soon").is_err() && p("9999h").is_err());
    }

    #[test]
    fn only_typing_wakes_and_terminal_reports_are_dropped() {
        // Focus in and out, a mouse click (SGR and X10), a cursor position report, an OSC answer.
        for report in [&b"\x1b[I"[..], b"\x1b[O", b"\x1b[<0;10;5M\x1b[<0;10;5m", b"\x1b[M #!", b"\x1b[12;40R",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\", b"\x1b[A\x1bOB", b"\x03", b"\x1b"] {
            let (kept, typing) = typed(report);
            assert!(!typing, "{report:?}");
            assert!(kept.iter().all(|b| *b != 0x1b), "{report:?}");
        }
        assert_eq!(typed(b"hi\r"), (b"hi\r".to_vec(), true));
        assert_eq!(typed(b"\r"), (b"\r".to_vec(), true));
        assert_eq!(typed("é".as_bytes()), ("é".as_bytes().to_vec(), true));
        // A paste keeps its markers, so the new run gets it as one paste; reports in between go.
        assert_eq!(typed(b"\x1b[I\x1b[200~a\nb\x1b[201~"), (b"\x1b[200~a\nb\x1b[201~".to_vec(), true));
        // Editing keys are kept with the text they edit, but don't count as typing on their own.
        assert_eq!(typed(b"\x7f"), (b"\x7f".to_vec(), false));
    }
}
