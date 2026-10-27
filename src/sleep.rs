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
//! of those paths), the human's `muster send` (or a phone's message), the human typing into its
//! tab (the keys are held here and typed once it is ready, `keys`), a real click in one of its
//! panels, and Start (Wake) in the agents panel. Nothing that doesn't need the agent wakes it: a
//! panel closing (or any other notice `panels::wakes` turns down), status checks, list and read
//! tools, reading its screen, the terminal's own reports (focus, mouse, cursor position),
//! showing its tab.
//!
//! Sleeping is a per-agent setting, off by default (`sleep_after_min`), plus a manual Sleep.
//! Never while it is working, waiting for approval or a question, in a turn, with prompts
//! waiting or a message on its way, with background work running (a sub-agent or a command it
//! started in the background, which would end with its CLI), with text in its input box, or
//! before its conversation id is known (it couldn't be resumed): `blocker`. The last check and
//! the switch to asleep are one step against everything that could reach the old process
//! (`State::claim`): keys typed since the check, a message or send registered meanwhile (they
//! take the start lock, which a sleep holds), a prompt Muster is typing. The stop is fenced to
//! the run that was checked.
//!
//! Waking re-reads the whole conversation; once its prompt cache has expired (src/cache.rs),
//! uncached, which costs the owner real usage. The tab says so before a wake, the wake is noted
//! in the timeline, and a message typed in after the wake gets the cold-start mark as any
//! message to a cold agent does. A wake that fails leaves the agent asleep, says why (its
//! asleep bar, a notice), and the next message or typing tries again.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
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
pub const KEYS_MAX: usize = 64 * 1024;
/// After a wake failed, how long before a panel click may try again (the human's typing, a
/// message or Wake try again at once).
const PANEL_RETRY: Duration = Duration::from_secs(60);
/// Runtime key: how much the human had typed into waking agents' tabs when the backend stopped
/// (sizes only, never the text), said in their timelines by the next backend.
const LOST_INPUT_KEY: &str = "sleep-lost-input";
/// How much of a run's transcript is searched for its background tasks' notices.
const TRANSCRIPT_TAIL: u64 = 4 * 1024 * 1024;

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

/// What the keystroke path needs, under one lock that never waits on SQLite or the hub. Lock
/// order: the hub's own lock, then this one, then the prompt queue's (`State::claim`, `flush`).
#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Asleep agents' sessions, to their agent (mirrors `Agent::asleep_at`; set first by a sleep's
    /// `claim`, so keys typed from then on are held, never typed into the run being stopped).
    asleep: HashMap<SessionId, i64>,
    /// Wakes in progress until the agent is ready: agent -> the tab being woken.
    waking: HashMap<i64, SessionId>,
    /// Wakes whose start returned: a run that ends from now on ended the wake (`settle`).
    launched: HashSet<i64>,
    /// Keys the human typed into an asleep or waking agent's tab, typed once it is ready.
    keys: HashMap<SessionId, Vec<u8>>,
    /// Tabs whose held typing went over `KEYS_MAX`: none of it is kept until the wake ends.
    overflowed: HashSet<SessionId>,
    /// Waking tabs showing a question or an approval (a resumed CLI's own startup dialog): the
    /// human's keys go straight to it, so they can answer.
    interactive: HashSet<SessionId>,
    /// Wakes asked for (keys, a panel click) whose start hasn't returned yet.
    requested: HashSet<i64>,
    /// Messages on their way to each agent (`agents::deliver`, `for_send`): a sleep waits for none.
    deliveries: HashMap<i64, u32>,
    /// Raw input to each session: how many writes were admitted so far, and how many are still
    /// on their way to its terminal, until the terminal host has settled them (`keys`, `admit`,
    /// `Admitted`). A sleep checks neither moved (`claim`).
    input: HashMap<SessionId, (u64, Arc<AtomicU32>)>,
    /// Why an agent's last wake failed, until it wakes or sleeps again (its asleep bar says so).
    failed: HashMap<i64, String>,
    /// After a failed wake: no panel click wakes the agent before this.
    panel_retry: HashMap<i64, tokio::time::Instant>,
}

impl Inner {
    fn admit(&mut self, id: SessionId) -> Admitted {
        let e = self.input.entry(id).or_default();
        e.0 += 1;
        e.1.fetch_add(1, Ordering::SeqCst);
        Admitted { on_way: e.1.clone() }
    }

    /// How many raw writes to session `id` were admitted so far, and how many are on their way.
    fn input_of(&self, id: SessionId) -> (u64, u32) {
        self.input.get(&id).map_or((0, 0), |(seq, on_way)| (*seq, on_way.load(Ordering::SeqCst)))
    }

    /// The wake of `agent` (tab `tab`) is over: stop holding keys for it. True when typed keys
    /// were dropped.
    fn end_wake(&mut self, agent: i64, tab: SessionId) -> bool {
        self.waking.remove(&agent);
        self.launched.remove(&agent);
        self.interactive.remove(&tab);
        self.overflowed.remove(&tab);
        self.keys.remove(&tab).is_some_and(|k| !k.is_empty())
    }
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

    /// Why agent `id`'s last wake failed (None: it didn't, or it has woken or slept since).
    pub fn wake_failed(&self, id: i64) -> Option<String> {
        self.inner.lock().failed.get(&id).cloned()
    }

    fn deliveries(&self, agent: i64) -> u32 {
        self.inner.lock().deliveries.get(&agent).copied().unwrap_or(0)
    }

    fn waking(&self, agent: i64) -> bool {
        let g = self.inner.lock();
        g.waking.contains_key(&agent) || g.requested.contains(&agent)
    }

    /// How many raw writes to session `id` were admitted so far, and how many of them haven't
    /// been settled with the terminal host yet.
    fn input_seq(&self, id: SessionId) -> (u64, u32) {
        self.inner.lock().input_of(id)
    }

    /// The last check of a sleep, and the switch to asleep, as one step: refused when raw input
    /// reached session `sid` since `seen` (or is on its way), a message to `agent` registered
    /// meanwhile, a wake was asked for, or a prompt is being typed or waits. Otherwise `sid` is
    /// asleep from here on: keys typed into it are held (`keys`), never typed into the run being
    /// stopped. Called with the start lock held, so no message or send can register a start.
    fn claim(&self, hub: &Hub, agent: i64, sid: SessionId, seen: u64) -> Result<(), &'static str> {
        let mut g = self.inner.lock();
        let (seq, on_way) = g.input_of(sid);
        if seq != seen || on_way > 0 {
            return Err("you typed into it just now");
        }
        if g.deliveries.get(&agent).is_some_and(|n| *n > 0) {
            return Err("a message to it is on its way");
        }
        if g.waking.contains_key(&agent) || g.requested.contains(&agent) {
            return Err("it is waking up");
        }
        let g = &mut *g;
        // Under the queue lock: no prompt starts typing in between (a delivery picks its prompt
        // under that lock).
        crate::inject::while_queue_idle(hub, sid, || {
            g.asleep.insert(sid, agent);
            g.overflowed.remove(&sid);
            g.failed.remove(&agent);
        })
        .ok_or("it has prompts waiting to be typed")
    }

    /// A claimed sleep didn't happen (the run is still there): awake, and what the human typed
    /// meanwhile goes to it after all.
    fn unclaim(&self, hub: &Hub, agent: i64, sid: SessionId) {
        let held = {
            let mut g = self.inner.lock();
            g.asleep.retain(|_, a| *a != agent);
            g.overflowed.remove(&sid);
            g.keys.remove(&sid).filter(|k| !k.is_empty()).map(|k| (k, g.admit(sid)))
        };
        if let Some((keys, admitted)) = held {
            hub.note_human_input(sid);
            if let Err(e) = hub.send_keys_admitted(sid, &keys, admitted) {
                tracing::warn!(session = sid, "typing the keys held while it was going to sleep: {e:#}");
            }
        }
    }
}

/// A message on its way to an agent, from before it is started until it is in the prompt
/// queue (`agents::deliver`, `for_send`). A sleep refuses while one is.
pub(crate) struct Delivering {
    hub: Arc<Hub>,
    agent: i64,
}

pub(crate) fn delivering(hub: &Arc<Hub>, agent: i64) -> Delivering {
    *hub.agents().sleep.inner.lock().deliveries.entry(agent).or_default() += 1;
    Delivering { hub: hub.clone(), agent }
}

/// `delivering` for something that must not wake agent `agent` (session `sid`): None when it
/// is asleep, or a sleep has claimed it. One step under the lock `State::claim` takes, so a
/// sleep either sees this and refuses, or this sees the sleep (a panel batch, `inject::enqueue_panels`).
pub(crate) fn delivering_awake(hub: &Arc<Hub>, agent: i64, sid: SessionId) -> Option<Delivering> {
    let mut g = hub.agents().sleep.inner.lock();
    if g.asleep.contains_key(&sid) {
        return None;
    }
    *g.deliveries.entry(agent).or_default() += 1;
    Some(Delivering { hub: hub.clone(), agent })
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

/// Raw input to a session, admitted until the terminal host has settled it: handed to the
/// terminal with it (`Hub::send_keys_admitted`, `write_input_admitted`), which drops it once
/// the host answered (or the input was refused). A sleep refuses while one is on its way, or
/// when one came after its check. Dropping takes no lock (the input thread drops it).
pub struct Admitted {
    on_way: Arc<AtomicU32>,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.on_way.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What became of the human's keys for a session (`keys`).
pub enum Keys {
    /// An asleep or waking agent's tab: held until it is ready (or dropped, if they were the
    /// terminal's own reports).
    Held,
    /// Type them into the session now; keep this until they are handed to the terminal.
    Pass(Admitted),
}

/// Raw input that isn't the human's (an agent's or a shell's `session.input`) for session `id`:
/// refused for an asleep or waking agent (a message wakes it; keys can't), else admitted.
pub fn admit(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<Admitted> {
    let mut g = hub.agents().sleep.inner.lock();
    if g.asleep.contains_key(&id) {
        anyhow::bail!("session {id} is an asleep agent's: send it a message (ask_agent, `muster ask`) to wake it");
    }
    if g.waking.values().any(|t| *t == id) {
        anyhow::bail!("session {id} is an agent that is waking up; try again once it is ready");
    }
    Ok(g.admit(id))
}

/// Work a Claude run started in the background (`run_in_background`: a sub-agent, a command)
/// that hasn't reported back. It runs inside the CLI's process, so a sleep would end it:
/// `blocker` refuses while any is (`SessionInfo::background`). Kept per run with its hook state
/// (`store::HookSession`), so a reload keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Background {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<BgTask>,
    /// Sub-agents that reported back. One can go on (its notice may be interim): a later hook
    /// from it counts it again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<BgTask>,
    /// The run's own transcript (its hooks' `transcript_path`), where each finished task's
    /// notice lands (`refresh_background`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BgTask {
    /// The tool call that started it (empty: the hook didn't say).
    pub tool_use: String,
    /// Its task id once known: a sub-agent's `agentId`, a command's `backgroundTaskId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// A sub-agent (the Agent/Task tool), whose end is its SubagentStop hook.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
}

/// The most background tasks (and finished sub-agents) remembered for one run.
const TASKS_MAX: usize = 256;
const DONE_KEEP: usize = 32;

fn text_of(v: Option<&serde_json::Value>) -> Option<&str> {
    v.and_then(|v| v.as_str()).filter(|v| !v.is_empty())
}

/// The id a background launch's tool result names (`agentId`, `backgroundTaskId`), structured or
/// in its text ("agentId: a1b2c3").
fn launched_id(v: Option<&serde_json::Value>) -> Option<String> {
    let v = v?;
    for k in ["agentId", "backgroundTaskId", "taskId", "task_id", "agent_id"] {
        if let Some(id) = text_of(v.get(k)) {
            return Some(id.to_string());
        }
    }
    let text = v.to_string();
    for marker in ["agentId: ", "agentId\\\":\\\"", "agentId\":\"", "backgroundTaskId\":\""] {
        if let Some(at) = text.find(marker) {
            let id: String = text[at + marker.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// A tool result whose work goes on in the background although its call didn't ask for that:
/// moved there while it ran (a command past its timeout, or the human's Ctrl+B). Only the
/// result says so: a `backgroundTaskId` (a command), `backgroundedByUser`, or a sub-agent's
/// `status: "async_launched"`.
fn moved_to_background(v: Option<&serde_json::Value>) -> bool {
    let Some(v) = v else { return false };
    text_of(v.get("backgroundTaskId")).is_some()
        || v.get("backgroundedByUser").and_then(|b| b.as_bool()) == Some(true)
        || text_of(v.get("status")) == Some("async_launched")
}

/// Every `<tag>value</tag>` in `text`.
fn tagged<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        rest = &rest[at + open.len()..];
        let Some(end) = rest.find(&close) else { break };
        out.push(rest[..end].trim());
        rest = &rest[end + close.len()..];
    }
    out
}

impl Background {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.done.is_empty() && self.transcript.is_none()
    }

    /// Background tasks still running.
    pub fn running(&self) -> u32 {
        self.tasks.len() as u32
    }

    /// The tasks `is_done` picks have reported back.
    fn finish(&mut self, is_done: impl Fn(&BgTask) -> bool) {
        let (done, left): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tasks).into_iter().partition(|t| is_done(t));
        self.tasks = left;
        for t in done.into_iter().filter(|t| t.agent && t.task.is_some()) {
            self.done.retain(|d| d.task != t.task);
            self.done.push(t);
        }
        let over = self.done.len().saturating_sub(DONE_KEEP);
        self.done.drain(..over);
    }

    /// The tasks a task notice in `text` reports on (`<task-notification>` with the launch's
    /// `<tool-use-id>` and the task's `<task-id>`) have reported back.
    pub fn notified(&mut self, text: &str) {
        if !text.contains("<task-notification>") {
            return;
        }
        let uses = tagged(text, "tool-use-id");
        let ids = tagged(text, "task-id");
        self.finish(|t| {
            (!t.tool_use.is_empty() && uses.contains(&t.tool_use.as_str())) || t.task.as_deref().is_some_and(|id| ids.contains(&id))
        });
    }

    /// What hook `event` (with `payload`) of the run changes. True when the running tasks did.
    pub fn after_hook(&mut self, event: &str, payload: &serde_json::Value) -> bool {
        let before = self.tasks.clone();
        let sub = text_of(payload.get("agent_id"));
        if sub.is_none()
            && let Some(t) = text_of(payload.get("transcript_path"))
        {
            self.transcript = Some(t.to_string());
        }
        let tool_use = text_of(payload.get("tool_use_id")).unwrap_or("");
        let launch = |t: &BgTask| t.task.is_none() && t.tool_use == tool_use;
        match event {
            // Started in the background, by the agent or one of its sub-agents (all end with the CLI).
            "PreToolUse" if payload.pointer("/tool_input/run_in_background").and_then(|v| v.as_bool()) == Some(true) => {
                let known = !tool_use.is_empty() && self.tasks.iter().any(|t| t.tool_use == tool_use);
                if !known && self.tasks.len() < TASKS_MAX {
                    let agent = matches!(text_of(payload.get("tool_name")), Some("Agent" | "Task"));
                    self.tasks.push(BgTask { tool_use: tool_use.to_string(), task: None, agent });
                }
            }
            // It started: its result names it.
            "PostToolUse" => {
                let result = payload.get("tool_response");
                if let Some(t) = self.tasks.iter_mut().find(|t| launch(t)) {
                    t.task = launched_id(result);
                } else if moved_to_background(result) && self.tasks.len() < TASKS_MAX {
                    // Started in the foreground, then moved to the background: from now on it
                    // is background work like any other.
                    let task = launched_id(result);
                    let known = self.tasks.iter().any(|t| (!tool_use.is_empty() && t.tool_use == tool_use) || (task.is_some() && t.task == task));
                    if !known {
                        let agent = matches!(text_of(payload.get("tool_name")), Some("Agent" | "Task"));
                        self.tasks.push(BgTask { tool_use: tool_use.to_string(), task, agent });
                    }
                }
            }
            // It never started.
            "PostToolUseFailure" => self.tasks.retain(|t| !launch(t)),
            "SubagentStop" => {
                if let Some(id) = sub {
                    self.finish(|t| t.agent && t.task.as_deref() == Some(id));
                }
            }
            // A task's notice, when the CLI submits it as a prompt.
            "UserPromptSubmit" => {
                if let Some(p) = text_of(payload.get("prompt")) {
                    self.notified(p);
                }
            }
            _ => {}
        }
        // A sub-agent that reported back but goes on (its notice was interim) runs again.
        if let Some(id) = sub
            && event != "SubagentStop"
            && let Some(i) = self.done.iter().position(|t| t.task.as_deref() == Some(id))
        {
            let t = self.done.remove(i);
            self.tasks.push(t);
        }
        self.tasks != before
    }
}

/// The background tasks of session `sid` whose notices are in its transcript have reported back
/// (the CLI may deliver a notice without a hook): read off the hub's locks, once the agent would
/// otherwise sleep.
async fn refresh_background(hub: &Arc<Hub>, sid: SessionId) {
    let Some((generation, bg)) = hub.background_of(sid) else { return };
    let Some(path) = bg.transcript.clone().filter(|p| p.ends_with(".jsonl")) else { return };
    if bg.tasks.is_empty() {
        return;
    }
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(TRANSCRIPT_TAIL)))?;
        let mut buf = Vec::new();
        f.take(TRANSCRIPT_TAIL).read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await;
    let Ok(Ok(text)) = read else { return };
    // Each notice is one JSON string in the transcript: its newlines are escaped there.
    let text = text.replace("\\n", "\n");
    // To the work as it is now: a hook may have registered more while the file was read.
    hub.edit_background(sid, generation, |bg| bg.notified(&text));
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
    if info.background > 0 {
        return Some(match info.background {
            1 => "it has background work still running (a sub-agent or a command it started in the background)".into(),
            n => format!("it has {n} background tasks still running (sub-agents or commands it started in the background)"),
        });
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
/// the start lock, which every wake also takes, then `State::claim`), keeping its record, tab
/// and conversation. Already asleep: nothing to do.
pub async fn sleep(hub: &Arc<Hub>, name: &str, why: Why) -> anyhow::Result<Agent> {
    let _one = hub.agents().start_lock.lock().await;
    let a = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
    if a.asleep_at.is_some() {
        return Ok(a);
    }
    if let Some(sid) = a.session_id
        && hub.session(sid).is_some_and(|i| i.background > 0)
    {
        refresh_background(hub, sid).await;
    }
    // Read before anything else: input admitted after this is caught by `claim`. Input admitted
    // before it must have reached the terminal already, or the checks below (its input box)
    // could miss it.
    let (seen, on_way) = a.session_id.map_or((0, 0), |s| hub.agents().sleep.input_seq(s));
    if on_way > 0 {
        return Err(NotNow(format!("{} can't sleep now: you typed into it just now", a.name)).into());
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
    // The run that was checked: only it is stopped.
    let Some(generation) = hub.pty_generation(info.id) else {
        return Err(NotNow(format!("{} can't sleep now: it isn't running", a.name)).into());
    };
    let state = &hub.agents().sleep;
    if let Err(no) = state.claim(hub, a.id, info.id, seen) {
        return Err(NotNow(format!("{} can't sleep now: {no}", a.name)).into());
    }
    if let Err(e) = hub.agents().set_asleep(hub.store(), a.id, Some(now), Some(generation)) {
        state.unclaim(hub, a.id, info.id);
        return Err(e);
    }
    let stopped = match hub.stop_for_sleep(info.id, generation) {
        Ok(()) => wait_exited(hub, info.id, generation).await,
        Err(e) => Err(e),
    };
    if let Err(e) = stopped {
        // Awake after all: never shown asleep while its process may still run.
        let r = hub.agents().set_asleep(hub.store(), a.id, None, None);
        state.unclaim(hub, a.id, info.id);
        r?;
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

/// Wait until run `generation` of session `id` has stopped.
async fn wait_exited(hub: &Hub, id: SessionId, generation: u64) -> anyhow::Result<()> {
    let mut rx = hub.subscribe();
    let deadline = tokio::time::Instant::now() + EXIT_WAIT;
    loop {
        let replaced = hub.pty_generation(id).is_some_and(|g| g != generation);
        let gone = replaced
            || hub.session(id).is_none_or(|s| s.state == AgentState::Exited) && !hub.live_session_ids().contains(&id);
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
/// held until it is ready (`settle`). If the start then fails, `start_failed` puts it back.
pub(crate) fn on_start(hub: &Arc<Hub>, agent: &Agent) -> anyhow::Result<Option<i64>> {
    let Some(cur) = hub.agents().by_id(agent.id).filter(|a| a.asleep_at.is_some()) else { return Ok(None) };
    let tab = cur.session_id;
    let spawn = {
        let mut g = hub.agents().sleep.inner.lock();
        g.failed.remove(&cur.id);
        g.panel_retry.remove(&cur.id);
        g.launched.remove(&cur.id);
        match tab {
            Some(tab) => g.waking.insert(cur.id, tab).is_none(),
            None => false,
        }
    };
    let before = match hub.agents().set_asleep(hub.store(), cur.id, None, None) {
        Ok(b) => b,
        Err(e) => {
            hub.agents().sleep.inner.lock().waking.remove(&cur.id);
            return Err(e);
        }
    };
    if let Some(a) = hub.agents().by_id(cur.id) {
        crate::agents::publish(hub, &a);
    }
    if spawn
        && let Some(tab) = tab
        && let Some(at) = before.asleep_at
    {
        tokio::spawn(settle(hub.clone(), cur.id, tab, at));
    }
    Ok(before.asleep_at)
}

/// The start that woke agent `agent` (asleep since `asleep_at`) has returned: from now on, a
/// run that ends has ended the wake (`settle`).
pub(crate) fn launched(hub: &Hub, agent: i64) {
    let mut g = hub.agents().sleep.inner.lock();
    if g.waking.contains_key(&agent) {
        g.launched.insert(agent);
    }
}

/// A start that returned `e` may still have launched the CLI: the terminal host's answer was
/// lost (`terminal::SpawnUncertain`), and `Hub::reconcile_pending` adopts or retires it later.
/// Not a failed wake: never put back to sleep (the CLI may run), never said as a failure.
pub(crate) fn unconfirmed(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::terminal::SpawnUncertain>().is_some()
}

/// The start that was waking agent `agent` (asleep since `asleep_at`) failed with `e`. With no
/// run going, it is asleep again (not "stopped"): its next message, or typing into its tab,
/// tries again. The keys held for it are dropped (said), and its asleep bar says why. A launch
/// that is only unconfirmed (`unconfirmed`) hasn't failed: the wake goes on, and `settle`
/// waits for the launch to be adopted (it is ready) or retired (the wake failed after all).
pub(crate) fn start_failed(hub: &Arc<Hub>, agent: i64, asleep_at: i64, e: &anyhow::Error) {
    if unconfirmed(e) {
        launched(hub, agent);
        return;
    }
    // A wake `wake_soon` asked for says so itself.
    let asked = hub.agents().sleep.inner.lock().requested.contains(&agent);
    wake_lost(hub, agent, asleep_at, e, !asked);
}

/// A wake of agent `agent` (asleep since `asleep_at`) failed with `e`, and no run of it goes
/// on: asleep again, `wake_error` says why, the held keys are dropped (`say`: and that is said).
/// True when typed keys were dropped.
fn wake_lost(hub: &Arc<Hub>, agent: i64, asleep_at: i64, e: &anyhow::Error, say: bool) -> bool {
    let Some(a) = hub.agents().by_id(agent) else { return false };
    let tab = hub.agents().sleep.inner.lock().waking.get(&agent).copied().or(a.session_id);
    let dropped = {
        let mut g = hub.agents().sleep.inner.lock();
        g.failed.insert(agent, format!("{e:#}"));
        tab.is_some_and(|t| g.end_wake(agent, t))
    };
    let live = a.session_id.is_some_and(|s| hub.live_session_ids().contains(&s));
    if !live && a.asleep_at.is_none() {
        match hub.agents().set_asleep(hub.store(), agent, Some(asleep_at), None) {
            Ok(_) => {
                if let Some(s) = a.session_id {
                    hub.mark_asleep(s);
                }
            }
            Err(err) => tracing::warn!(agent = a.name, "keeping it asleep after a failed wake: {err:#}"),
        }
    }
    if let Some(a) = hub.agents().by_id(agent) {
        crate::agents::publish(hub, &a);
    }
    if dropped && say {
        hub.notice("warn", format!("{} couldn't be woken, so what you typed into its tab wasn't sent: {e:#}", a.name));
    }
    dropped
}

/// A start of agent `agent`, asleep when it began, failed with `e`: say why on its asleep bar
/// (`wake_error`) while it is still asleep (`start_failed` has put it back when the failure came
/// after the wake began).
pub(crate) fn wake_failed(hub: &Hub, agent: i64, e: &anyhow::Error) {
    if unconfirmed(e) {
        return;
    }
    hub.agents().sleep.inner.lock().failed.insert(agent, format!("{e:#}"));
    if let Some(a) = hub.agents().by_id(agent) {
        crate::agents::publish(hub, &a);
    }
}

/// Session `id` is being restarted: if it is an asleep agent's, that agent is awake from now
/// on. Returns the agent and since when it was asleep (for `start_failed`).
pub(crate) fn on_session_start(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<Option<(i64, i64)>> {
    if !hub.agents().sleep.is_asleep(id) {
        return Ok(None);
    }
    let Some(a) = hub.agents().by_session(id) else { return Ok(None) };
    Ok(on_start(hub, &a)?.map(|at| (a.id, at)))
}

/// The wake of agent `agent` (from tab `tab`, asleep since `asleep_at`) is done once its session
/// is ready: type the keys held for it, if any, when no prompt is being typed (else wait for
/// that), then stop holding keys. While the woken CLI shows a question or an approval of its
/// own, the human's keys go straight to it. A launch whose acknowledgement was lost is waited
/// for (`Hub::reconcile_pending` adopts or retires it). A run that ends before it was ready is a
/// failed wake (`ended_early`). Gives up after `READY_WAIT`, dropping what was held (and saying
/// so); ends when something else ended the wake (`start_failed`).
async fn settle(hub: Arc<Hub>, agent: i64, tab: SessionId, asleep_at: i64) {
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    let mut rx = hub.subscribe();
    let mut started = false;
    loop {
        if !hub.agents().sleep.inner.lock().waking.contains_key(&agent) {
            return;
        }
        let cur = hub.agents().by_id(agent).and_then(|a| a.session_id);
        let info = cur.and_then(|s| hub.session(s));
        let live = cur.is_some_and(|s| hub.live_session_ids().contains(&s));
        if let Some(i) = &info
            && live
        {
            started |= i.state != AgentState::Exited;
            let dialog = matches!(i.state, AgentState::AskingQuestion | AgentState::NeedsApproval);
            {
                let mut g = hub.agents().sleep.inner.lock();
                if dialog {
                    g.interactive.insert(tab);
                } else {
                    g.interactive.remove(&tab);
                }
            }
            if adapter_for(i.kind).is_ready(i.state) && flush(&hub, agent, tab, i.id) {
                return;
            }
        }
        let now = tokio::time::Instant::now();
        // Its run ended (or never started though its start returned) before it was ready; a
        // launch still unconfirmed hasn't.
        let launched = hub.agents().sleep.inner.lock().launched.contains(&agent);
        let ended = (started || launched) && !live && !cur.is_some_and(|s| hub.may_deliver(s));
        if ended && !hub.is_shutting_down() && hub.agents().by_id(agent).is_some() {
            if ended_early(&hub, agent, tab, asleep_at).await {
                return;
            }
            continue;
        }
        if now >= deadline || hub.is_shutting_down() || hub.agents().by_id(agent).is_none() {
            give_up(&hub, agent, tab);
            return;
        }
        crate::mcp::wait::next_change(&mut rx, cur.unwrap_or(tab), (deadline - now).min(Duration::from_millis(500))).await;
    }
}

/// Type what was held for `tab` into session `to`, now ready, as the human's input. True when
/// the wake is done.
fn flush(hub: &Hub, agent: i64, tab: SessionId, to: SessionId) -> bool {
    let mut g = hub.agents().sleep.inner.lock();
    if g.keys.get(&tab).is_none_or(|k| k.is_empty()) {
        g.end_wake(agent, tab);
        return true;
    }
    // Never into a prompt Muster is typing: wait until the queue is idle (and ready again).
    let g = &mut *g;
    crate::inject::while_queue_idle(hub, to, || {
        let keys = g.keys.remove(&tab).unwrap_or_default();
        let admitted = g.admit(to);
        // The human typed them: the turn they start is the human's (whatever woke it).
        hub.note_human_input(to);
        if let Err(e) = hub.send_keys_admitted(to, &keys, admitted) {
            tracing::warn!(session = to, "typing the keys held while it woke: {e:#}");
        }
        g.end_wake(agent, tab);
    })
    .is_some()
}

/// The run that woke agent `agent` (from tab `tab`) ended before it was ready: a failed wake,
/// like a start that failed (`wake_lost`): asleep again, why on its asleep bar, typing or the
/// next message tries again. Under the start lock, checked again there: false when a start
/// meanwhile runs it again (the wake goes on).
async fn ended_early(hub: &Arc<Hub>, agent: i64, tab: SessionId, asleep_at: i64) -> bool {
    let _one = hub.agents().start_lock.lock().await;
    if hub.agents().sleep.inner.lock().waking.get(&agent) != Some(&tab) {
        return true;
    }
    let cur = hub.agents().by_id(agent).and_then(|a| a.session_id);
    if cur.is_some_and(|s| hub.live_session_ids().contains(&s) || hub.may_deliver(s)) {
        return false;
    }
    let info = cur.and_then(|s| hub.session(s));
    let why = match info.as_ref().and_then(|i| i.state_detail.clone().filter(|d| !d.is_empty() && d != ASLEEP_DETAIL)) {
        Some(d) => format!("it stopped before it was ready ({d})"),
        None => match info.and_then(|i| i.exit_code) {
            Some(code) => format!("it stopped before it was ready (exit code {code})"),
            None => "it stopped before it was ready".to_string(),
        },
    };
    let e = anyhow::anyhow!(why);
    let name = hub.agents().by_id(agent).map(|a| a.name).unwrap_or_else(|| "The agent".into());
    tracing::warn!(agent = name, "waking: {e:#}");
    let lost = if wake_lost(hub, agent, asleep_at, &e, false) { " What you typed into its tab meanwhile wasn't sent." } else { "" };
    hub.notice("warn", format!("{name} couldn't be woken: {e:#}. It is asleep again; the next message, or typing into its tab, tries again.{lost}"));
    true
}

fn give_up(hub: &Hub, agent: i64, tab: SessionId) {
    // The backend is stopping: `save_held` has recorded what is lost, for the next one to say.
    if hub.is_shutting_down() {
        return;
    }
    let dropped = hub.agents().sleep.inner.lock().end_wake(agent, tab);
    if dropped {
        let name = hub.agents().by_id(agent).map(|a| a.name).unwrap_or_else(|| "the agent".into());
        hub.notice("warn", format!("{name} didn't get ready after waking, so what you typed into its tab meanwhile wasn't sent."));
    }
}

/// The backend is stopping: record how much the human had typed into waking agents' tabs (the
/// size only, never the text), so the next backend says in their timelines that it was lost.
pub fn save_held(hub: &Hub) {
    let held: Vec<(SessionId, usize)> = hub
        .agents()
        .sleep
        .inner
        .lock()
        .keys
        .iter()
        .filter(|(_, k)| !k.is_empty())
        .map(|(id, k)| (*id, k.len()))
        .collect();
    if held.is_empty() {
        return;
    }
    if let Err(e) = hub.store().save_runtime(LOST_INPUT_KEY, &held) {
        tracing::warn!("recording the typing lost while agents woke: {e:#}");
    }
}

/// At start: say in each tab's timeline what the last backend lost of the human's typing.
fn report_lost_input(hub: &Hub) {
    let lost = match hub.store().load_runtime::<Vec<(SessionId, usize)>>(LOST_INPUT_KEY) {
        Ok(Some(l)) => l,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("reading the typing lost while agents woke: {e:#}");
            return;
        }
    };
    for (id, n) in lost {
        let Some(info) = hub.session(id) else { continue };
        let text = format!(
            "{} was waking when Muster's backend restarted: what you typed into its tab meanwhile ({n} bytes) was not sent. Type it again.",
            info.name
        );
        tracing::warn!(session = id, "{text}");
        hub.log_event(Some(info.channel_id), Some(id), EventKind::System, json!({"text": text, "level": "warn"}));
    }
    if let Err(e) = hub.store().delete_runtime(LOST_INPUT_KEY) {
        tracing::warn!("forgetting the typing lost while agents woke: {e:#}");
    }
}

/// The human's keys for session `id` (a client window or a paired phone). `Held`: taken here,
/// because it is an asleep or waking agent's tab: held until it is ready (and an asleep one
/// starts waking, if they typed something), or dropped (the terminal's own reports). `Pass`:
/// type them now (admitted for `State::claim`).
pub fn keys(hub: &Arc<Hub>, id: SessionId, bytes: &[u8]) -> Keys {
    let (wake, overflow) = {
        let mut g = hub.agents().sleep.inner.lock();
        let waking = g.waking.values().any(|t| *t == id);
        let asleep = g.asleep.get(&id).copied();
        // Not an asleep or waking agent's, or its woken CLI is asking something: typed now.
        if (!waking && asleep.is_none() && !g.keys.contains_key(&id)) || (waking && asleep.is_none() && g.interactive.contains(&id)) {
            return Keys::Pass(g.admit(id));
        }
        let (kept, typing) = typed(bytes);
        let held = g.keys.get(&id).map_or(0, Vec::len);
        let mut overflow = false;
        if !g.overflowed.contains(&id) && !kept.is_empty() && (typing || held > 0) {
            if held + kept.len() <= KEYS_MAX {
                g.keys.entry(id).or_default().extend_from_slice(&kept);
            } else {
                // Never part of it: a paste cut short is worse than none.
                g.keys.remove(&id);
                g.overflowed.insert(id);
                overflow = true;
            }
        }
        let wake = match asleep {
            Some(agent) if typing && !waking && !g.requested.contains(&agent) => Some(agent),
            _ => None,
        };
        (wake, overflow)
    };
    if overflow {
        let name = hub.agents().by_session(id).map(|a| a.name).unwrap_or_else(|| "The agent".into());
        hub.notice("warn", format!(
            "{name} is still waking: what you typed into its tab meanwhile came to more than {} KiB, so none of it was kept. Type or paste it again once it is ready.",
            KEYS_MAX / 1024
        ));
    }
    if let Some(agent) = wake {
        wake_soon(hub, agent, Some(id), Cause::Typing);
    }
    Keys::Held
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

/// What asked for a background wake (`wake_soon`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The human typed into its tab: always worth another try.
    Typing,
    /// A real click in one of its panels: retried at most every `PANEL_RETRY` after a failure
    /// (its events would ask on every panel tick).
    Panel,
}

/// Wake agent `agent` in the background (keys typed into `tab`, a panel click): once, however
/// often it is asked before the start returns.
pub(crate) fn wake_soon(hub: &Arc<Hub>, agent: i64, tab: Option<SessionId>, cause: Cause) {
    {
        let mut g = hub.agents().sleep.inner.lock();
        if cause == Cause::Panel && g.panel_retry.get(&agent).is_some_and(|t| *t > tokio::time::Instant::now()) {
            return;
        }
        if !g.requested.insert(agent) {
            return;
        }
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
                    let (what, say_cost) = match cause {
                        Cause::Typing => ("you typed into its tab", true),
                        Cause::Panel => ("you clicked in one of its panels", false),
                    };
                    note_woke(&hub, &s.agent, &s.session, at, what, say_cost);
                }
            }
            // The launch is unconfirmed, not failed: the wake goes on (`settle`).
            Err(e) if unconfirmed(&e) => tracing::warn!(agent = name.as_deref().unwrap_or("?"), "waking: {e:#}"),
            Err(e) => {
                {
                    let mut g = hub.agents().sleep.inner.lock();
                    g.failed.insert(agent, format!("{e:#}"));
                    if cause == Cause::Panel {
                        g.panel_retry.insert(agent, tokio::time::Instant::now() + PANEL_RETRY);
                    }
                    // Whether the wake got as far as holding them (`start_failed` drops them then)
                    // or not, what was typed for it goes.
                    if let Some(t) = tab {
                        g.overflowed.remove(&t);
                        g.keys.remove(&t);
                    }
                }
                let name = name.unwrap_or_else(|| "The agent".into());
                tracing::warn!(agent = name, "waking: {e:#}");
                let lost = match cause {
                    Cause::Typing => " What you typed into its tab wasn't sent; type again to try again.",
                    Cause::Panel => "",
                };
                hub.notice("warn", format!("{name} couldn't be woken: {e:#}.{lost}"));
                if let Some(a) = hub.agents().by_id(agent) {
                    crate::agents::publish(&hub, &a);
                }
            }
        }
        hub.agents().sleep.inner.lock().requested.remove(&agent);
    });
}

/// `session.send` from the human (`muster send`, a phone's message) to session `id`: if it is
/// an asleep agent's, wake it first. Returns the session to send to and the guard that keeps it
/// from sleeping until the prompt is queued (also for an awake agent). The guard is registered
/// before the start lock is taken, and the asleep check made under it, so a sleep either sees
/// this send and refuses, or has finished and this wakes it.
pub(crate) async fn for_send(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<(SessionId, Option<Delivering>)> {
    let Some(a) = hub.agents().by_session(id) else { return Ok((id, None)) };
    let guard = delivering(hub, a.id);
    match crate::agents::wake_if_asleep(hub, a.id).await? {
        Some(s) => {
            if let Some(at) = s.woke {
                note_woke(hub, &s.agent, &s.session, at, "you sent it a message", true);
            }
            Ok((s.session.id, Some(guard)))
        }
        None => Ok((id, Some(guard))),
    }
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

/// What to make of an agent saved as asleep whose session's run `live` is (`reconcile`).
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// Nothing runs: asleep, as saved.
    Asleep,
    /// The very run a sleep was stopping when the backend went away: finish stopping it.
    Stopping(u64),
    /// Another run (a wake an older backend did, a restart): awake.
    Awake,
}

fn found(live: Option<u64>, stopping: Option<u64>) -> Found {
    match (live, stopping) {
        (None, _) => Found::Asleep,
        (Some(g), Some(s)) if g == s => Found::Stopping(g),
        (Some(_), _) => Found::Awake,
    }
}

/// At start, agents this backend's records show asleep: a sleep the backend's restart cut off
/// before its run stopped is finished (the decision was made, and the run is the one it
/// checked); one whose session runs another run is awake after all; and an asleep tab says
/// "asleep" whatever an older backend wrote.
fn reconcile(hub: &Arc<Hub>) {
    for a in hub.agents().list() {
        let (Some(_), Some(sid)) = (a.asleep_at, a.session_id) else { continue };
        let live = hub.live_session_ids().contains(&sid).then(|| hub.pty_generation(sid)).flatten();
        let stopping = hub.store().agent_asleep_generation(a.id).unwrap_or_else(|e| {
            tracing::warn!(agent = a.name, "reading which run it was stopping: {e:#}");
            None
        });
        match found(live, stopping) {
            Found::Asleep => hub.mark_asleep(sid),
            Found::Stopping(generation) => {
                tracing::info!(agent = a.name, "was going to sleep when the backend restarted: stopping it");
                let hub = hub.clone();
                tokio::spawn(async move {
                    // As a sleep does: no wake or autostart slips in before it has stopped.
                    let _one = hub.agents().start_lock.lock().await;
                    let stopped = match hub.stop_for_sleep(sid, generation) {
                        Ok(()) => wait_exited(&hub, sid, generation).await,
                        Err(e) => Err(e),
                    };
                    match stopped {
                        // Stopped (or it ended on its own meanwhile): asleep, as saved.
                        Ok(()) => hub.mark_asleep(sid),
                        Err(_) if !hub.live_session_ids().contains(&sid) => hub.mark_asleep(sid),
                        Err(e) => {
                            tracing::warn!(agent = a.name, "finishing its sleep: {e:#}");
                            wake_record(&hub, &a);
                        }
                    }
                });
            }
            Found::Awake => wake_record(hub, &a),
        }
    }
}

/// Agent `a` is saved asleep but runs: awake.
fn wake_record(hub: &Hub, a: &Agent) {
    match hub.agents().set_asleep(hub.store(), a.id, None, None) {
        Ok(_) => {
            tracing::info!(agent = a.name, "was marked asleep but its process runs: awake");
            if let Some(a) = hub.agents().by_id(a.id) {
                crate::agents::publish(hub, &a);
            }
        }
        Err(e) => tracing::warn!(agent = a.name, "waking a running agent's record: {e:#}"),
    }
}

/// Put agents to sleep once their idle time runs out.
async fn sweep(hub: &Arc<Hub>) {
    let now = now_ms();
    for a in hub.agents().list() {
        if a.sleep_after_min.is_none() || a.asleep_at.is_some() {
            continue;
        }
        let Some(sid) = a.session_id else { continue };
        // Background tasks that have finished without a hook saying so, once it is otherwise due.
        if let Some(info) = hub.session(sid)
            && info.background > 0
            && due(&a, &info, human_at(hub, sid, now), now)
        {
            refresh_background(hub, sid).await;
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

/// At daemon start: fix records a restart left, then check idle agents from time to time.
pub fn spawn(hub: Arc<Hub>) {
    reconcile(&hub);
    report_lost_input(&hub);
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

    /// A hub over an in-memory store: task 1 with session 1 (Claude, no process), the agent
    /// "dozy" (id 1) in it, resumable, its home in a temporary folder; asleep when `asleep`.
    fn hub_with(asleep: bool, autostart: bool) -> Arc<Hub> {
        let store = crate::store::Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        let mut channel = crate::mcp::guard::test_channel(1);
        channel.window_id = Some(1);
        store.insert_channel(&channel).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.window_id = Some(1);
        info.kind = SessionKind::Claude;
        store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
        let home = std::env::temp_dir().join(format!("muster-sleep-{}", crate::paths::random_token()));
        store
            .insert_agent(&Agent {
                kind: SessionKind::Claude,
                session_id: Some(1),
                home_dir: home.to_string_lossy().into_owned(),
                asleep_at: asleep.then_some(5_000),
                autostart,
                ..agent()
            })
            .unwrap();
        Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(), muster_bin: "/bin/true".into(),
            terminal_host: None, openrouter_key: None }, store).unwrap()
    }

    fn hub(asleep: bool) -> Arc<Hub> {
        hub_with(asleep, false)
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(std::time::Instant::now() < end, "timed out waiting: {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn notices(rx: &mut tokio::sync::broadcast::Receiver<crate::hub::ServerEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let crate::hub::ServerEvent::Notice { text, .. } = e {
                out.push(text);
            }
        }
        out
    }

    #[test]
    fn background_work_keeps_it_awake_until_it_reports_back() {
        let mut bg = Background::default();
        let launch = |id: &str, tool: &str| json!({"tool_name": tool, "tool_use_id": id,
            "tool_input": {"run_in_background": true}, "transcript_path": "/t/main.jsonl"});
        // A foreground sub-agent is `subagents`' (it ends with its turn).
        assert!(!bg.after_hook("PreToolUse", &json!({"tool_name": "Agent", "tool_use_id": "t0", "tool_input": {}})));
        assert!(bg.after_hook("PreToolUse", &launch("t1", "Agent")));
        assert!(bg.after_hook("PreToolUse", &launch("t2", "Bash")));
        assert_eq!((bg.running(), bg.transcript.as_deref()), (2, Some("/t/main.jsonl")));
        // The launches return at once, naming their tasks (as text, or structured).
        bg.after_hook("PostToolUse", &json!({"tool_name": "Agent", "tool_use_id": "t1", "tool_response": {"content": [
            {"type": "text", "text": "Async agent launched successfully.\nagentId: a1b2 (internal ID)"}]}}));
        bg.after_hook("PostToolUse", &json!({"tool_name": "Bash", "tool_use_id": "t2",
            "tool_response": {"stdout": "", "backgroundTaskId": "bx9"}}));
        assert_eq!((bg.tasks[0].task.as_deref(), bg.tasks[1].task.as_deref()), (Some("a1b2"), Some("bx9")));
        // The agent's turn ending is no sign of them.
        assert!(!bg.after_hook("Stop", &json!({})));
        // The sub-agent's own background command counts too; its transcript isn't the run's.
        bg.after_hook("PreToolUse", &json!({"agent_id": "a1b2", "tool_name": "Bash", "tool_use_id": "t3",
            "tool_input": {"run_in_background": true}, "transcript_path": "/t/sub.jsonl"}));
        assert_eq!((bg.running(), bg.transcript.as_deref()), (3, Some("/t/main.jsonl")));
        assert!(bg.after_hook("SubagentStop", &json!({"agent_id": "a1b2"})));
        assert_eq!(bg.running(), 2);
        // Its notice was interim: it goes on, and counts again.
        assert!(bg.after_hook("PreToolUse", &json!({"agent_id": "a1b2", "tool_name": "Read", "tool_use_id": "t4", "tool_input": {}})));
        assert_eq!(bg.running(), 3);
        bg.after_hook("SubagentStop", &json!({"agent_id": "a1b2"}));
        // A prompt that only quotes the tags is no notice.
        assert!(!bg.after_hook("UserPromptSubmit", &json!({"prompt": "what is <tool-use-id>t2</tool-use-id>?"})));
        // A command's notice, submitted as a prompt.
        assert!(bg.after_hook("UserPromptSubmit", &json!({"prompt":
            "<task-notification>\n<task-id>bx9</task-id>\n<tool-use-id>t2</tool-use-id>\n<status>completed</status>\n</task-notification>"})));
        // ... or found in the transcript, where the CLI delivered it without a hook.
        bg.notified(r#"{"content":"<task-notification>\n<task-id>zz</task-id>\n<tool-use-id>t3</tool-use-id>\n<status>killed</status>"}"#);
        assert_eq!(bg.running(), 0);
        // A launch that failed never ran.
        bg.after_hook("PreToolUse", &launch("t5", "Bash"));
        assert!(bg.after_hook("PostToolUseFailure", &json!({"tool_name": "Bash", "tool_use_id": "t5"})));
        assert_eq!(bg.running(), 0);
        // Saved with the run's hook state, and read back.
        let saved: Background = serde_json::from_value(serde_json::to_value(&bg).unwrap()).unwrap();
        assert_eq!(saved, bg);
        // While any runs, it doesn't sleep.
        let mut i = info(AgentState::Idle);
        i.background = 2;
        assert!(blocker(&agent(), Some(&i), &ready()).unwrap().contains("2 background tasks"));
    }

    /// Work moved to the background while it ran (a command past its timeout, Ctrl+B) is only
    /// named by its result: it counts like work started there.
    #[test]
    fn work_moved_to_the_background_keeps_it_awake() {
        let mut bg = Background::default();
        // A foreground command that finished, even one whose output quotes a task id, is none.
        assert!(!bg.after_hook("PostToolUse", &json!({"tool_name": "Bash", "tool_use_id": "f1",
            "tool_response": {"stdout": "{\"backgroundTaskId\":\"zz\"}", "stderr": ""}})));
        // A foreground sub-agent's result names it too, but it ended.
        assert!(!bg.after_hook("PostToolUse", &json!({"tool_name": "Agent", "tool_use_id": "f2",
            "tool_response": {"status": "completed", "agentId": "a0"}})));
        assert!(bg.after_hook("PostToolUse", &json!({"tool_name": "Bash", "tool_use_id": "t1",
            "tool_response": {"stdout": "", "backgroundTaskId": "b7"}})));
        assert!(bg.after_hook("PostToolUse", &json!({"tool_name": "Agent", "tool_use_id": "t2",
            "tool_response": {"status": "async_launched", "agentId": "a9"}})));
        assert_eq!(bg.running(), 2);
        assert_eq!((bg.tasks[0].task.as_deref(), bg.tasks[1].task.as_deref(), bg.tasks[1].agent), (Some("b7"), Some("a9"), true));
        // The same result again (a replayed hook) isn't counted twice.
        assert!(!bg.after_hook("PostToolUse", &json!({"tool_name": "Bash", "tool_use_id": "t1",
            "tool_response": {"backgroundTaskId": "b7"}})));
        // They end as any background work does.
        bg.notified("<task-notification>\n<task-id>b7</task-id>\n<tool-use-id>t1</tool-use-id>\n<status>completed</status>");
        assert!(bg.after_hook("SubagentStop", &json!({"agent_id": "a9"})));
        assert_eq!(bg.running(), 0);
    }

    /// The transcript's evidence applies to the background work as it is when it is applied, so
    /// work a hook registered while the file was read is kept.
    #[test]
    fn transcript_evidence_keeps_work_registered_meanwhile() {
        let hub = hub(false);
        let (generation, _) = hub.background_of(1).unwrap();
        let launch = |id: &str| json!({"tool_name": "Bash", "tool_use_id": id, "tool_input": {"run_in_background": true}});
        hub.edit_background(1, generation, |bg| {
            bg.after_hook("PreToolUse", &launch("t1"));
        });
        // What `refresh_background` read the transcript for...
        let (_, snapshot) = hub.background_of(1).unwrap();
        assert_eq!(snapshot.running(), 1);
        // ... a hook registers more while it reads ...
        hub.edit_background(1, generation, |bg| {
            bg.after_hook("PreToolUse", &launch("t2"));
        });
        // ... and the notice it found ends only the task it names.
        let text = "<task-notification>\n<tool-use-id>t1</tool-use-id>\n</task-notification>";
        hub.edit_background(1, generation, |bg| bg.notified(text));
        let (_, now) = hub.background_of(1).unwrap();
        assert_eq!(now.tasks.iter().map(|t| t.tool_use.as_str()).collect::<Vec<_>>(), vec!["t2"]);
        assert_eq!(hub.session(1).unwrap().background, 1);
        // Another run's work is never touched.
        hub.edit_background(1, generation + 1, |bg| bg.tasks.clear());
        assert_eq!(hub.session(1).unwrap().background, 1);
    }

    /// Keys stay admitted until the terminal host has them (the input link drops the admission
    /// then), and a sleep refuses while any are on their way, before it checks the input box.
    #[tokio::test]
    async fn keys_on_their_way_to_the_terminal_keep_it_awake() {
        let hub = hub(false);
        let st = &hub.agents().sleep;
        let typing = match keys(&hub, 1, b"x") {
            Keys::Pass(a) => a,
            Keys::Held => panic!("an awake agent's keys are typed"),
        };
        // Handed to the input link, which keeps it until the host answered.
        let queued: crate::terminal::Hold = Box::new(typing);
        assert_eq!(st.input_seq(1), (1, 1));
        let e = sleep(&hub, "dozy", Why::Manual).await.unwrap_err();
        assert!(format!("{e:#}").contains("you typed into it just now"), "{e:#}");
        drop(queued);
        assert_eq!(st.input_seq(1), (1, 0));
        let e = sleep(&hub, "dozy", Why::Manual).await.unwrap_err();
        assert!(format!("{e:#}").contains("isn't running"), "{e:#}");
    }

    /// A panel batch is a message on its way until it is queued: a sleep refuses while one is,
    /// and none is queued once a sleep has claimed the tab.
    #[tokio::test]
    async fn panel_batches_and_a_sleep_exclude_each_other() {
        let hub = hub(false);
        let _lock = hub.agents().start_lock.lock().await;
        let st = &hub.agents().sleep;
        let batch = delivering_awake(&hub, 1, 1).expect("awake: it may be queued");
        assert_eq!(st.claim(&hub, 1, 1, 0), Err("a message to it is on its way"));
        drop(batch);
        assert_eq!(st.claim(&hub, 1, 1, 0), Ok(()));
        assert!(delivering_awake(&hub, 1, 1).is_none(), "claimed: its events wait for its wake");
        assert_eq!(st.deliveries(1), 0);
    }

    /// A wake whose launch is only unconfirmed (the host's answer was lost) isn't a failed one:
    /// awake, no error, typing held for it. If that launch is then retired, or the woken CLI
    /// stops before it is ready, the wake failed: asleep again, why on its asleep bar, said once,
    /// and typing tries again.
    #[tokio::test]
    async fn an_unconfirmed_launch_waits_and_one_that_stops_early_is_a_failed_wake() {
        let hub = hub(true);
        let st = &hub.agents().sleep;
        let mut rx = hub.subscribe();
        hub.test_unconfirmed_launch(1, true, "");
        let a = hub.agents().by_id(1).unwrap();
        assert_eq!(on_start(&hub, &a).unwrap(), Some(5_000));
        let lost = anyhow::Error::new(crate::terminal::SpawnUncertain("connection lost".into()));
        start_failed(&hub, 1, 5_000, &lost);
        wake_failed(&hub, 1, &lost);
        assert!(hub.agents().by_id(1).unwrap().asleep_at.is_none(), "its CLI may run: awake");
        assert_eq!(st.wake_failed(1), None);
        assert!(!st.is_asleep(1));
        assert!(matches!(keys(&hub, 1, b"hi"), Keys::Held), "held for the launch");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(st.waking(1), "waiting for the launch to be adopted or retired");
        // Retired: nothing runs.
        hub.test_unconfirmed_launch(1, false, "launch failed: no such program");
        until("the wake ends", || !st.waking(1)).await;
        let a = hub.agents().by_id(1).unwrap();
        assert!(a.asleep_at.is_some(), "asleep again");
        assert!(st.is_asleep(1));
        assert!(st.wake_failed(1).is_some_and(|w| w.contains("no such program")), "{:?}", st.wake_failed(1));
        assert!(st.inner.lock().keys.is_empty());
        let said = notices(&mut rx);
        assert_eq!(said.iter().filter(|n| n.contains("couldn't be woken")).count(), 1, "{said:?}");
        assert!(said.iter().any(|n| n.contains("wasn't sent")), "{said:?}");
        // Typing tries again.
        assert!(matches!(keys(&hub, 1, b"x"), Keys::Held));
        assert!(st.waking(1));
        until("the retry ends", || !st.waking(1)).await;
    }

    /// The last check and the switch to asleep are one step: input typed after the check, or
    /// still on its way, and a message registered meanwhile stop it; from then on the human's
    /// keys are held, never typed into the run being stopped.
    #[tokio::test]
    async fn a_sleep_claims_the_tab_against_input_and_messages() {
        let hub = hub(false);
        // As a sleep does: nothing may start the agent meanwhile.
        let _lock = hub.agents().start_lock.lock().await;
        let st = &hub.agents().sleep;
        let seen = st.input_seq(1).0;
        let typing = match keys(&hub, 1, b"x") {
            Keys::Pass(a) => a,
            Keys::Held => panic!("an awake agent's keys are typed"),
        };
        assert_eq!(st.claim(&hub, 1, 1, seen), Err("you typed into it just now"));
        drop(typing);
        assert_eq!(st.claim(&hub, 1, 1, seen), Err("you typed into it just now"), "typed since the check");
        let seen = st.input_seq(1).0;
        let message = delivering(&hub, 1);
        assert_eq!(st.claim(&hub, 1, 1, seen), Err("a message to it is on its way"));
        drop(message);
        // An agent's or a shell's raw input counts as well.
        let other = admit(&hub, 1).unwrap();
        assert!(st.claim(&hub, 1, 1, seen).is_err());
        drop(other);
        let seen = st.input_seq(1).0;
        assert_eq!(st.claim(&hub, 1, 1, seen), Ok(()));
        assert!(st.is_asleep(1));
        assert!(matches!(keys(&hub, 1, b"hi"), Keys::Held));
        assert!(admit(&hub, 1).is_err(), "keys from an agent can't reach it either");
        // The sleep didn't happen: awake, and what was typed meanwhile goes to the run after all.
        st.unclaim(&hub, 1, 1);
        assert!(!st.is_asleep(1));
        assert!(st.inner.lock().keys.is_empty());
        assert!(hub.human_input_of(1).is_some());
    }

    /// `muster send` and a tab's restart wait for the start lock a sleep holds (the send is
    /// registered before, so a sleep after it sees it).
    #[tokio::test]
    async fn sends_and_restarts_wait_for_a_sleep_in_progress() {
        let hub = hub(false);
        let lock = hub.agents().start_lock.lock().await;
        let h = hub.clone();
        let send = tokio::spawn(async move { for_send(&h, 1).await.map(|(id, guard)| (id, guard.is_some())) });
        let h = hub.clone();
        let restart = tokio::spawn(async move { crate::agents::restart_session(&h, 1).await.is_ok() });
        until("the send registers", || hub.agents().sleep.deliveries(1) == 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!send.is_finished() && !restart.is_finished());
        assert_eq!(hub.agents().sleep.deliveries(1), 1);
        drop(lock);
        assert_eq!(send.await.unwrap().unwrap(), (1, true));
        let _ = restart.await.unwrap();
        assert_eq!(hub.agents().sleep.deliveries(1), 0);
    }

    /// Autostart checks again under the start lock: an agent that fell asleep after the list was
    /// read stays asleep.
    #[tokio::test]
    async fn autostart_leaves_an_agent_that_just_fell_asleep_asleep() {
        let hub = hub_with(false, true);
        let mut rx = hub.subscribe();
        let lock = hub.agents().start_lock.lock().await;
        let h = hub.clone();
        let started = tokio::spawn(async move { crate::agents::autostart(&h).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.agents().set_asleep(hub.store(), 1, Some(now_ms()), None).unwrap();
        drop(lock);
        started.await.unwrap();
        assert!(hub.agents().by_id(1).unwrap().asleep_at.is_some());
        assert!(notices(&mut rx).iter().all(|n| !n.contains("didn't start")), "never started");
    }

    /// Typing into an asleep tab wakes it. A wake that fails leaves it asleep, says why, drops
    /// what was typed, and the next keys try again at once. A paste over the limit is never kept
    /// in part.
    #[tokio::test]
    async fn typing_wakes_it_and_a_failed_wake_can_be_retried() {
        let hub = hub(true);
        let st = &hub.agents().sleep;
        let mut rx = hub.subscribe();
        assert!(st.is_asleep(1));
        // The terminal's own reports never wake it.
        assert!(matches!(keys(&hub, 1, b"\x1b[I\x1b[<0;3;4M"), Keys::Held));
        assert!(!st.waking(1));
        assert!(matches!(keys(&hub, 1, b"hello"), Keys::Held));
        assert!(st.waking(1));
        // This hub has no terminal host: the start fails.
        until("the wake ends", || !st.waking(1)).await;
        let a = hub.agents().by_id(1).unwrap();
        assert!(a.asleep_at.is_some(), "asleep again, not stopped");
        assert!(st.is_asleep(1));
        assert!(crate::agents::agent_view(&hub, &a)["wake_error"].is_string());
        assert!(st.inner.lock().keys.is_empty());
        let said = notices(&mut rx);
        assert_eq!(said.iter().filter(|n| n.contains("couldn't be woken")).count(), 1, "{said:?}");
        assert!(said.iter().any(|n| n.contains("wasn't sent")), "{said:?}");
        // The human typing again tries again at once.
        assert!(matches!(keys(&hub, 1, b"x"), Keys::Held));
        assert!(st.waking(1));
        until("the second wake ends", || !st.waking(1)).await;
        // Over the limit: none of it is kept, and that is said.
        st.inner.lock().requested.insert(1);
        keys(&hub, 1, b"a");
        assert_eq!(st.inner.lock().keys.get(&1).map(Vec::len), Some(1));
        keys(&hub, 1, &vec![b'x'; KEYS_MAX]);
        keys(&hub, 1, b"b");
        assert!(st.inner.lock().keys.get(&1).is_none());
        assert!(notices(&mut rx).iter().any(|n| n.contains("none of it was kept")));
    }

    /// A woken CLI's own dialog (a question, an approval) takes the human's keys live, arrows
    /// included; otherwise keys wait for it to be ready, and are then typed as the human's.
    #[tokio::test]
    async fn keys_reach_a_startup_dialog_and_held_keys_are_the_humans() {
        let hub = hub(false);
        let st = &hub.agents().sleep;
        st.inner.lock().waking.insert(1, 1);
        assert!(matches!(keys(&hub, 1, b"\x1b[B"), Keys::Held));
        assert!(st.inner.lock().keys.get(&1).is_none(), "an arrow alone isn't kept");
        st.inner.lock().interactive.insert(1);
        assert!(matches!(keys(&hub, 1, b"\x1b[B"), Keys::Pass(_)));
        st.inner.lock().interactive.remove(&1);
        assert!(matches!(keys(&hub, 1, b"hi\r"), Keys::Held));
        assert!(hub.human_input_of(1).is_none());
        assert!(flush(&hub, 1, 1, 1));
        assert!(hub.human_input_of(1).is_some(), "typed as the human's, whatever woke it");
        assert!(!st.waking(1));
    }

    /// What the human typed into a waking tab when the backend stopped is said in its timeline
    /// by the next one: its size, never its text.
    #[test]
    fn typing_lost_to_a_reload_is_reported() {
        let hub = hub(false);
        hub.agents().sleep.inner.lock().keys.insert(1, b"secret".to_vec());
        save_held(&hub);
        let saved: Vec<(SessionId, usize)> = hub.store().load_runtime(LOST_INPUT_KEY).unwrap().unwrap();
        assert_eq!(saved, vec![(1, 6)]);
        report_lost_input(&hub);
        assert!(hub.store().load_runtime::<Vec<(SessionId, usize)>>(LOST_INPUT_KEY).unwrap().is_none());
    }

    /// At start, an agent saved asleep whose session runs: still the run its sleep was stopping
    /// (the backend restarted mid-stop) means finish the stop; another run means awake.
    #[test]
    fn a_sleep_cut_off_by_a_restart_is_finished_only_for_its_own_run() {
        assert_eq!(found(None, Some(4)), Found::Asleep);
        assert_eq!(found(Some(4), Some(4)), Found::Stopping(4));
        assert_eq!(found(Some(5), Some(4)), Found::Awake);
        assert_eq!(found(Some(5), None), Found::Awake);
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
