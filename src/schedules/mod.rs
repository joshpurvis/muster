//! Schedules (docs/SCHEDULING_PLAN.md, Phase 1): recurring or one-off messages to a named agent.
//! Each run is an ordinary agent message with origin `schedule` at hop 0, sent through the same
//! path as `agent.ask` (`agents::send_checked`), so the agent's accepted origins, the hop limit and
//! the daily caps all apply.
//!
//! State lives in the store (`schedules`, `schedule_runs`; migration v19) and in memory here. The
//! scheduler is one daemon task (`spawn`): it sleeps on the monotonic clock until the next due
//! slot (at most a minute at a time), reads the wall clock when it wakes, and notices when the
//! two disagree (a suspend, or the clock being set). A slot is claimed in the store (the run row,
//! unique per schedule and slot, together with the schedule's next slot) before anything is
//! delivered, so no slot ever fires twice: not after a crash, a reload or a clock change.
//!
//! Rules (the owner's decisions, docs/SCHEDULING_PLAN.md "Decisions"):
//! - Missed runs (Muster off or asleep at the time): daily and weekly schedules run once when
//!   Muster is back; hourly-or-faster ones skip (`plan::effective_missed`). Catch-ups go one at a
//!   time per agent, spread out.
//! - A busy agent: a run is skipped when the same schedule's previous run is still going,
//!   otherwise it waits in line for up to an hour (`plan::LINE_WAIT_MS`), then is skipped.
//! - Claude subscription agents: scheduled only if they accept the schedule origin; their
//!   recurring schedules expire 7 days after they were made or renewed; quiet hours default on.
//!
//! Phase 2 adds triggers (a `when` that is an event, `triggers.rs`; runs with origin `webhook`),
//! "Then…" steps (`Schedule::then`: after a run, ask another agent one hop further down the same
//! chain, or just notify), and the OpenRouter spend caps (src/spend.rs, checked with every other
//! delivery rule in `agents::precheck`).

pub mod cron;
pub mod plan;
mod run;
pub mod triggers;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agents::{Agent, Origin};
use crate::hub::{Hub, ServerEvent};
use crate::model::*;
use crate::store::Store;

pub use plan::{Facts, Quiet};

// ----- the clock -----

/// Wall-clock time for the scheduler, injectable so tests can move it.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The system clock, plus a fixed offset that only debug builds read from
/// `MUSTER_TEST_CLOCK_OFFSET_MS` (the smoke test's way to be "two days later").
pub struct SystemClock {
    offset_ms: i64,
}

impl SystemClock {
    pub fn from_env() -> Self {
        let offset_ms = if cfg!(debug_assertions) {
            std::env::var("MUSTER_TEST_CLOCK_OFFSET_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
        } else {
            0
        };
        if offset_ms != 0 {
            tracing::warn!("schedules: test clock offset {offset_ms} ms");
        }
        SystemClock { offset_ms }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        now_ms() + self.offset_ms
    }
}

/// A clock tests set by hand.
#[cfg(test)]
#[derive(Default)]
pub struct FakeClock(pub AtomicI64);

#[cfg(test)]
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ----- records -----

/// When a schedule fires: at a time (cron, once), or when something happens (a trigger,
/// Phase 2: its runs have origin `webhook`, shown as "Triggered").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    /// A cron line (presets are cron lines too), in the schedule's time zone.
    Cron { cron: String },
    /// Once, at this unix ms.
    Once { at: i64 },
    /// A note's text changed (only while the human allows triggers on it), then stayed quiet
    /// for `quiet_min` minutes.
    Note {
        note_id: NoteId,
        #[serde(default = "default_note_quiet")]
        quiet_min: u32,
    },
    /// A branch moved in a local git repository (polled from its ref files).
    Git { repo: String, branch: String },
    /// GitHub, polled through `gh` every `every_min` minutes: a PR opened, or a workflow run
    /// failed (on `branch` only, when given).
    Github {
        repo: String,
        event: GhEvent,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default = "default_gh_every")]
        every_min: u32,
    },
    /// A loopback URL (`POST /api/trigger/<name>` with its secret) or `muster trigger <name>`.
    Url { name: String },
}

fn default_note_quiet() -> u32 {
    triggers::NOTE_QUIET_DEFAULT_MIN
}
fn default_gh_every() -> u32 {
    triggers::GH_EVERY_DEFAULT_MIN
}

impl When {
    /// Fired by events, not the clock.
    pub fn is_trigger(&self) -> bool {
        !matches!(self, When::Cron { .. } | When::Once { .. })
    }
    /// Recurs (expires for Claude subscription agents): cron lines and triggers.
    pub fn recurs(&self) -> bool {
        !matches!(self, When::Once { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhEvent {
    PrOpened,
    CiFailed,
}

/// The "Then…" steps of a schedule: what runs after a run (or a step) ends, on success (a
/// reply) or on failure. Each step is one hop further down the same chain, so the 4-hop limit
/// bounds chains.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Then {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_success: Option<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<Step>,
}

/// One step: ask an agent (its prompt may quote the previous reply as `{reply}`), or, on
/// failure only, just notify the human.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notify: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<Box<Then>>,
}

/// An event waiting for its trigger's run (debounced, or held by the 15-minute floor).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEvent {
    /// When the first of these events happened.
    pub at: i64,
    /// What happened, one line per event (outside text included: quoted to the agent as data).
    pub text: String,
    /// Data sent with a trigger URL or `muster trigger` (untrusted, size-capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Events merged into this one.
    pub count: u32,
}

/// What a watcher has already seen, kept with the schedule so a restart notices what changed
/// while Muster was off (and fires once for it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchState {
    /// Git: the branch's commit when last looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// GitHub: the newest PR number or workflow run id seen (None: not looked yet; the first
    /// look only records it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_id: Option<u64>,
    /// GitHub CI: the failed runs already seen, as `<run id>.<attempt>` (the latest
    /// `triggers::SEEN_KEEP`), so a run that fails after a newer one did, or fails again on a
    /// re-run, is still noticed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen: Vec<String>,
    /// Raised each time the trigger is changed to watch something else: a look started before
    /// (a slow `gh` call) whose answer comes back after is thrown away, not saved.
    #[serde(default)]
    pub generation: u64,
}

/// "If missed": what a slot missed while Muster was off or asleep does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Missed {
    /// Daily and weekly (and one-off) schedules run once; hourly-or-faster skip.
    #[default]
    Auto,
    RunOnce,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Conversation {
    /// The agent's current conversation.
    #[default]
    Continue,
    /// A new conversation for each run (only once the agent is between turns).
    Fresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    #[default]
    OnFailure,
    Always,
    Never,
}

/// A schedule as stored (the `record` column, with `id` and `agent_id` also in columns).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub id: i64,
    pub agent_id: i64,
    pub name: String,
    pub prompt: String,
    pub when: When,
    /// IANA time zone name the cron line (and `{date}`) is read in.
    pub tz: String,
    pub missed: Missed,
    pub conversation: Conversation,
    pub notify: Notify,
    /// Replies are also appended to this note, under a date heading.
    pub note_id: Option<NoteId>,
    /// Runs in the quiet hours (settings) are skipped.
    pub quiet_hours: bool,
    /// At most this many runs reach the agent per rolling 24 hours.
    pub daily_cap: Option<u32>,
    pub paused: bool,
    /// "Skip next": this slot is skipped when it comes.
    pub skip_slot: Option<i64>,
    /// The next slot (None: a one-off that ran, or no time ever comes again).
    pub next_run: Option<i64>,
    /// The newest slot handled (run or skipped).
    pub last_slot: Option<i64>,
    /// The newest slot that ran (the 15-minute floor counts from it).
    pub last_fired: Option<i64>,
    /// When this schedule's newest message was submitted to the agent (unix ms, daemon clock):
    /// the floor also counts from it, so a late run doesn't let the next slot follow at once.
    pub last_sent: Option<i64>,
    /// Start of the 7-day window of a Claude subscription agent's recurring schedule.
    pub renewed_at: i64,
    /// The "expires tomorrow" notice was given for this window.
    pub expiry_warned: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// The "Then…" steps after a run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub then: Option<Then>,
    /// A trigger's event waiting to run at `next_run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<PendingEvent>,
    /// A watcher's memory (git, GitHub).
    pub watch: WatchState,
    /// A URL trigger's secret (never in `schedule.list`: `trigger.secret` gives it to the human).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            id: 0,
            agent_id: 0,
            name: String::new(),
            prompt: String::new(),
            when: When::Once { at: 0 },
            tz: "UTC".into(),
            missed: Missed::Auto,
            conversation: Conversation::Continue,
            notify: Notify::OnFailure,
            note_id: None,
            quiet_hours: false,
            daily_cap: None,
            paused: false,
            skip_slot: None,
            next_run: None,
            last_slot: None,
            last_fired: None,
            last_sent: None,
            renewed_at: 0,
            expiry_warned: false,
            created_at: 0,
            updated_at: 0,
            then: None,
            event: None,
            watch: WatchState::default(),
            secret: None,
        }
    }
}

impl Schedule {
    /// The origin of its runs: `schedule` for timed ones, `webhook` ("Triggered") for triggers.
    pub fn origin(&self) -> Origin {
        if self.when.is_trigger() { Origin::Webhook } else { Origin::Schedule }
    }
    /// The envelope's sender for its own runs.
    pub fn sender(&self) -> String {
        run::sender_of(self.id, self.when.is_trigger())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// At its time.
    Scheduled,
    /// Late: its time passed while Muster was off or asleep.
    CatchUp,
    /// Run now.
    Manual,
    /// Started by its trigger's event.
    Triggered,
    /// A "Then…" step after another run.
    Chain,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Scheduled => "scheduled",
            RunKind::CatchUp => "catch_up",
            RunKind::Manual => "manual",
            RunKind::Triggered => "triggered",
            RunKind::Chain => "chain",
        }
    }
    pub fn parse(s: &str) -> RunKind {
        match s {
            "catch_up" => RunKind::CatchUp,
            "manual" => RunKind::Manual,
            "triggered" => RunKind::Triggered,
            "chain" => RunKind::Chain,
            _ => RunKind::Scheduled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Waiting in line, or sent and waiting for the reply.
    Pending,
    Replied,
    /// Stopped on an approval prompt, a question or an error.
    Blocked,
    /// The turn ended without a reply attributed to the message.
    NoReply,
    /// The agent's process ended.
    Ended,
    TimedOut,
    /// The agent's policy refused it (origin, a daily cap).
    Refused,
    /// Not sent: paused, busy, quiet hours, missed, skipped by the human.
    Skipped,
    /// Couldn't be sent (the agent didn't start, ...).
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Replied => "replied",
            Outcome::Blocked => "blocked",
            Outcome::NoReply => "no_reply",
            Outcome::Ended => "ended",
            Outcome::TimedOut => "timed_out",
            Outcome::Refused => "refused",
            Outcome::Skipped => "skipped",
            Outcome::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Outcome {
        match s {
            "pending" => Outcome::Pending,
            "replied" => Outcome::Replied,
            "blocked" => Outcome::Blocked,
            "no_reply" => Outcome::NoReply,
            "ended" => Outcome::Ended,
            "timed_out" => Outcome::TimedOut,
            "refused" => Outcome::Refused,
            "skipped" => Outcome::Skipped,
            _ => Outcome::Failed,
        }
    }
    /// What "Notify me: on failure" alerts on.
    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Blocked | Outcome::NoReply | Outcome::Ended | Outcome::TimedOut | Outcome::Refused | Outcome::Failed)
    }
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Pending => "running",
            Outcome::Replied => "replied",
            Outcome::Blocked => "blocked",
            Outcome::NoReply => "no reply",
            Outcome::Ended => "agent ended",
            Outcome::TimedOut => "timed out",
            Outcome::Refused => "refused",
            Outcome::Skipped => "skipped",
            Outcome::Failed => "failed",
        }
    }
}

/// One run of a schedule (`schedule_runs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: i64,
    pub schedule_id: i64,
    /// The planned time (now, for Run now).
    pub slot: i64,
    pub kind: RunKind,
    /// When the scheduler took it up.
    pub started_at: i64,
    /// The agent message, once sent.
    pub msg_id: Option<i64>,
    pub outcome: Outcome,
    pub reason: Option<String>,
    pub finished_at: Option<i64>,
    /// A chain step: the run it follows, its place (`s`/`f` per level) and its agent.
    pub parent: Option<i64>,
    pub step: Option<String>,
    pub agent_id: Option<i64>,
    /// A triggered run: what happened (`PendingEvent` as JSON).
    pub event: Option<String>,
}

impl Run {
    /// A new pending run of `schedule` at `slot`.
    pub fn new(schedule_id: i64, slot: i64, kind: RunKind, now: i64) -> Run {
        Run {
            id: 0,
            schedule_id,
            slot,
            kind,
            started_at: now,
            msg_id: None,
            outcome: Outcome::Pending,
            reason: None,
            finished_at: None,
            parent: None,
            step: None,
            agent_id: None,
            event: None,
        }
    }
    pub fn pending_event(&self) -> Option<PendingEvent> {
        self.event.as_deref().and_then(|e| serde_json::from_str(e).ok())
    }
}

/// The scheduling settings (Agents panel → Scheduling settings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "Pause all schedules".
    pub paused: bool,
    /// Quiet hours, `HH:MM` in this machine's time zone; start == end: none.
    pub quiet_start: String,
    pub quiet_end: String,
    /// Unattended messages one agent takes per rolling 24 hours.
    pub unattended_cap: u32,
    /// OpenRouter: the daily dollar cap over every agent, in cents (always on).
    pub spend_cap_cents: u32,
    /// OpenRouter: optional per-agent daily caps, in cents, by agent id.
    pub agent_spend_caps: BTreeMap<i64, u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            paused: false,
            quiet_start: "23:00".into(),
            quiet_end: "07:00".into(),
            unattended_cap: crate::mcp::guard::UNATTENDED_DAILY_CAP_DEFAULT,
            spend_cap_cents: crate::spend::DEFAULT_CAP_CENTS,
            agent_spend_caps: BTreeMap::new(),
        }
    }
}

/// `HH:MM` -> minutes since midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

// ----- limits -----

pub const PROMPT_MAX: usize = 16 * 1024;
pub const NAME_MAX: usize = 80;
pub const MAX_SCHEDULES: usize = 200;
/// Runs kept per schedule.
pub const RUNS_KEEP: usize = 500;
pub const RUNS_DEFAULT: usize = 20;
pub const RUNS_MAX: usize = 200;
/// `{last_reply}` is cut to this many bytes.
pub const LAST_REPLY_MAX: usize = 4000;
/// A one-off schedule is at most this far ahead.
pub const ONCE_MAX_AHEAD_MS: i64 = 366 * 86_400_000;
const SETTINGS_KEY: &str = "schedule-settings";

// ----- the registry -----

#[derive(Default)]
struct Inner {
    list: BTreeMap<i64, Schedule>,
    settings: Settings,
}

pub struct Schedules {
    store: Store,
    clock: parking_lot::RwLock<Arc<dyn Clock>>,
    inner: Mutex<Inner>,
    /// Wakes the scheduler loop (a schedule changed).
    wake: tokio::sync::Notify,
    /// One schedule run at a time per agent (`run::execute`).
    agent_lines: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// Catch-ups are spread out: when the last one started (scheduler clock).
    catchup: tokio::sync::Mutex<Option<i64>>,
    /// When this backend's scheduler started (scheduler clock).
    started_at: std::sync::atomic::AtomicI64,
    /// Schedules already announced as expired (`run::tick`), so clients hear of it once.
    pub(crate) expired_seen: Mutex<std::collections::HashSet<i64>>,
    /// Runs waiting for their reply, woken when the run is ended elsewhere (its prompt was
    /// taken back before it was submitted).
    pub(crate) waiters: Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>,
}

impl Schedules {
    pub fn load(store: &Store, clock: Arc<dyn Clock>) -> anyhow::Result<Self> {
        let settings = store.load_runtime::<Settings>(SETTINGS_KEY)?.unwrap_or_default();
        let list = store.load_schedules()?.into_iter().map(|s| (s.id, s)).collect();
        let started_at = clock.now_ms();
        Ok(Schedules {
            store: store.clone(),
            clock: parking_lot::RwLock::new(clock),
            inner: Mutex::new(Inner { list, settings }),
            wake: Default::default(),
            agent_lines: Default::default(),
            catchup: Default::default(),
            started_at: started_at.into(),
            expired_seen: Default::default(),
            waiters: Default::default(),
        })
    }

    pub fn now(&self) -> i64 {
        self.clock.read().now_ms()
    }

    pub(crate) fn started_at(&self) -> i64 {
        self.started_at.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Tests drive the scheduler with their own clock.
    #[cfg(test)]
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        self.started_at.store(clock.now_ms(), std::sync::atomic::Ordering::Relaxed);
        *self.clock.write() = clock;
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().settings.clone()
    }

    pub fn get(&self, id: i64) -> Option<Schedule> {
        self.inner.lock().list.get(&id).cloned()
    }

    pub fn list(&self) -> Vec<Schedule> {
        self.inner.lock().list.values().cloned().collect()
    }

    fn line(&self, agent: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.agent_lines.lock().entry(agent).or_default().clone()
    }

    /// Edit schedule `id` under the lock; saved only when `f` succeeds.
    fn edit(&self, id: i64, f: impl FnOnce(&mut Schedule) -> anyhow::Result<()>) -> anyhow::Result<Schedule> {
        let mut g = self.inner.lock();
        let mut s = g.list.get(&id).cloned().with_context(|| format!("no schedule {id} (see `muster schedule ls`)"))?;
        f(&mut s)?;
        s.updated_at = self.now();
        self.store.save_schedule(&s)?;
        g.list.insert(id, s.clone());
        drop(g);
        self.wake.notify_one();
        Ok(s)
    }
}

/// Facts about a schedule's agent that the rules read. A Claude agent counts as on its
/// subscription when either its record or its running process says so: a switch to OpenRouter
/// only takes effect at the next start, and until then the subscription's rules (expiry, quiet
/// hours by default) still apply.
pub fn facts(hub: &Hub, agent: &Agent) -> Facts {
    use crate::openrouter::Provider;
    let running = agent
        .session_id
        .and_then(|s| hub.session(s))
        .filter(|s| s.state != AgentState::Exited)
        .map(|s| s.provider);
    Facts {
        subscription_claude: agent.kind == SessionKind::Claude
            && (agent.provider == Provider::Subscription || running == Some(Provider::Subscription)),
    }
}

/// The facts for schedule `s` (of `agent`): a Claude subscription agent anywhere in it, its own
/// or a "Then…" step's, brings the subscription rules (7-day expiry, quiet hours by default).
pub fn facts_for(hub: &Hub, agent: &Agent, s: &Schedule) -> Facts {
    let mut f = facts(hub, agent);
    if !f.subscription_claude {
        f.subscription_claude = step_agents(s.then.as_ref())
            .into_iter()
            .filter_map(|id| hub.agents().by_id(id))
            .any(|a| facts(hub, &a).subscription_claude);
    }
    f
}

/// Every agent a schedule's "Then…" steps ask.
pub fn step_agents(then: Option<&Then>) -> Vec<i64> {
    let mut out = Vec::new();
    if let Some(t) = then {
        for st in [&t.on_success, &t.on_failure].into_iter().flatten() {
            out.extend(st.agent_id);
            out.extend(step_agents(st.then.as_deref()));
        }
    }
    out
}

/// The step at `path` (`s`/`f` per level) of `then`.
pub fn step_at<'a>(then: Option<&'a Then>, path: &str) -> Option<&'a Step> {
    let mut cur = then?;
    let mut found = None;
    for (i, c) in path.chars().enumerate() {
        let st = match c {
            's' => cur.on_success.as_ref(),
            'f' => cur.on_failure.as_ref(),
            _ => None,
        }?;
        found = Some(st);
        if i + 1 < path.len() {
            cur = st.then.as_deref()?;
        }
    }
    found
}

fn then_json(hub: &Hub, t: &Then) -> Value {
    let step = |st: &Step| {
        json!({
            "notify": st.notify,
            "agent": st.agent_id.and_then(|a| hub.agents().by_id(a)).map(|a| a.name),
            "agent_id": st.agent_id,
            "prompt": st.prompt,
            "then": st.then.as_deref().map(|t| then_json(hub, t)),
        })
    };
    json!({
        "on_success": t.on_success.as_ref().map(step),
        "on_failure": t.on_failure.as_ref().map(step),
    })
}

fn quiet_of(settings: &Settings) -> Quiet {
    Quiet::new(settings, jiff::tz::TimeZone::system())
}

// ----- JSON -----

/// A schedule as `schedule.list` and `schedule_upserted` carry it.
pub fn schedule_json(hub: &Hub, s: &Schedule) -> Value {
    let agent = hub.agents().by_id(s.agent_id);
    let now = hub.schedules.now();
    let f = agent.as_ref().map(|a| facts_for(hub, a, s)).unwrap_or_default();
    let expires_at = plan::expires_at(s, &f);
    let expired = expires_at.is_some_and(|e| now >= e);
    let trigger = s.when.is_trigger();
    let status = if expired {
        "expired"
    } else if s.next_run.is_none() && !trigger {
        "done"
    } else if s.paused {
        "paused"
    } else {
        "active"
    };
    let tz = cron::zone(&s.tz).ok();
    let reading = match (&s.when, &tz) {
        (When::Cron { cron }, _) => cron::Cron::parse(cron).map(|c| c.describe()).unwrap_or_else(|e| e),
        (When::Once { at }, Some(tz)) => format!("Once, {}", cron::format(tz, *at, "%a %-d %b %Y at %H:%M")),
        (When::Once { .. }, None) => "Once".into(),
        (w, _) => triggers::describe(hub, w),
    };
    // Its own newest run ("Then…" steps are other agents' runs, shown in the history).
    let last_run = hub
        .store()
        .runs(s.id, None, 10)
        .ok()
        .and_then(|r| r.into_iter().find(|r| r.kind != RunKind::Chain))
        .map(|r| run_json(hub, &r));
    let note_title = s.note_id.and_then(|n| hub.store().load_note(n).ok().flatten()).map(|n| n.title);
    // A note trigger's note: its title, and whether the human allows triggers on it.
    let watched_note = match &s.when {
        When::Note { note_id, .. } => hub.store().load_note(*note_id).ok().flatten().map(|n| json!({"id": n.id, "title": n.title, "triggers": n.triggers})),
        _ => None,
    };
    let url = match &s.when {
        When::Url { name } => Some(format!("http://127.0.0.1:{}/api/trigger/{name}", hub.port())),
        _ => None,
    };
    json!({
        "trigger": trigger,
        "origin": s.origin(),
        "then": s.then.as_ref().map(|t| then_json(hub, t)),
        "pending": s.event.as_ref().map(|e| json!({"at": e.at, "text": e.text, "count": e.count, "payload": e.payload.is_some()})),
        "watch": triggers::watch_json(hub, s),
        "note": watched_note,
        "url": url,
        "id": s.id,
        "agent_id": s.agent_id,
        "agent": agent.as_ref().map(|a| a.name.clone()),
        "name": s.name,
        "prompt": s.prompt,
        "when": s.when,
        "tz": s.tz,
        "reading": reading,
        "missed": s.missed,
        "missed_effective": plan::effective_missed(s, now),
        "conversation": s.conversation,
        "notify": s.notify,
        "note_id": s.note_id,
        "note_title": note_title,
        "quiet_hours": s.quiet_hours,
        "daily_cap": s.daily_cap,
        "paused": s.paused,
        "skip_next": s.skip_slot.is_some() && s.skip_slot == s.next_run,
        "next_run": s.next_run,
        "last_slot": s.last_slot,
        "status": status,
        "expires_at": expires_at,
        "expires_soon": expires_at.is_some_and(|e| !expired && e - now <= plan::DAY_MS),
        "created_at": s.created_at,
        "updated_at": s.updated_at,
        "last_run": last_run,
    })
}

/// A run as `schedule.runs` and `schedule_run` carry it, with its message (text and reply).
pub fn run_json(hub: &Hub, r: &Run) -> Value {
    let msg = r.msg_id.and_then(|m| hub.store().agent_message(m).ok().flatten());
    // A reply that came after the wait ended still counts as one.
    let (outcome, late) = match (&msg, r.outcome) {
        (Some(m), Outcome::TimedOut | Outcome::NoReply) if m.reply.is_some() => (Outcome::Replied, true),
        _ => (r.outcome, false),
    };
    json!({
        "id": r.id,
        "schedule_id": r.schedule_id,
        "slot": r.slot,
        "kind": r.kind,
        "started_at": r.started_at,
        "finished_at": r.finished_at,
        "outcome": outcome,
        "late_reply": late,
        "reason": r.reason,
        "msg_id": r.msg_id,
        "message": msg.as_ref().map(crate::agents::message_json),
        "parent_run": r.parent,
        "step": r.step,
        "agent": r.agent_id.and_then(|a| hub.agents().by_id(a)).map(|a| a.name),
        "event": r.pending_event().map(|e| json!({"at": e.at, "text": e.text, "count": e.count, "payload": e.payload.is_some()})),
    })
}

pub fn settings_json(hub: &Hub, s: &Settings) -> Value {
    json!({
        "paused": s.paused,
        "quiet_start": s.quiet_start,
        "quiet_end": s.quiet_end,
        "unattended_cap": s.unattended_cap,
        "tz": cron::local_zone_name(),
        "spend": hub.spend.json(hub, s),
    })
}

/// The OpenRouter spend was read again: clients get it with the settings.
pub fn spend_changed(hub: &Hub) {
    hub.emit(ServerEvent::ScheduleSettings(settings_json(hub, &hub.schedules.settings())));
}

fn publish(hub: &Hub, s: &Schedule) {
    hub.emit(ServerEvent::ScheduleUpserted(schedule_json(hub, s)));
}

fn publish_run(hub: &Hub, r: &Run) {
    hub.emit(ServerEvent::ScheduleRun(run_json(hub, r)));
    if let Some(s) = hub.schedules.get(r.schedule_id) {
        publish(hub, &s);
    }
}

// ----- operations (RPC and CLI) -----

/// `when` as clients send it: a cron line, or once at a unix ms or at a wall-clock time
/// (`YYYY-MM-DDTHH:MM`) read in the schedule's time zone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WhenInput {
    Cron { cron: String },
    Once {
        #[serde(default)]
        at: Option<i64>,
        #[serde(default)]
        local: Option<String>,
    },
    /// A note by id, or by its title (`note`).
    Note {
        #[serde(default)]
        note_id: Option<NoteId>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        quiet_min: Option<u32>,
    },
    Git { repo: String, branch: String },
    Github {
        repo: String,
        event: GhEvent,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        every_min: Option<u32>,
    },
    Url { name: String },
}

impl WhenInput {
    pub fn is_trigger(&self) -> bool {
        !matches!(self, WhenInput::Cron { .. } | WhenInput::Once { .. })
    }

    /// The stored `When`, checked: a time read in `tz`, or an event (its note found, its repo
    /// made absolute, …; `own`: the schedule being edited, for the URL name's uniqueness).
    pub fn resolve_in(&self, hub: &Hub, tz: &str, own: Option<i64>) -> anyhow::Result<When> {
        let mut w = match self {
            WhenInput::Cron { .. } | WhenInput::Once { .. } => return self.resolve(tz),
            WhenInput::Note { note_id, note, quiet_min } => {
                let id = match (note_id, note.as_deref().map(str::trim).filter(|t| !t.is_empty())) {
                    (Some(id), _) => *id,
                    (None, Some(t)) => {
                        let found: Vec<NoteId> =
                            crate::notes::list(hub)?.into_iter().filter(|n| n.title.eq_ignore_ascii_case(t)).map(|n| n.id).collect();
                        match found.as_slice() {
                            [id] => *id,
                            [] => match t.parse::<NoteId>() {
                                Ok(id) => id,
                                Err(_) => anyhow::bail!("no note named “{t}” (a note trigger watches an existing note)"),
                            },
                            many => anyhow::bail!(
                                "{} notes are named “{t}” (ids {}): give the one to watch by its id (note_id)",
                                many.len(),
                                many.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
                            ),
                        }
                    }
                    (None, None) => anyhow::bail!("a note trigger needs the note (note_id, or its title as note)"),
                };
                When::Note { note_id: id, quiet_min: quiet_min.unwrap_or(triggers::NOTE_QUIET_DEFAULT_MIN) }
            }
            WhenInput::Git { repo, branch } => When::Git { repo: repo.clone(), branch: branch.clone() },
            WhenInput::Github { repo, event, branch, every_min } => When::Github {
                repo: repo.clone(),
                event: *event,
                branch: branch.clone(),
                every_min: every_min.unwrap_or(triggers::GH_EVERY_DEFAULT_MIN),
            },
            WhenInput::Url { name } => When::Url { name: name.clone() },
        };
        triggers::check_when(hub, &mut w, own)?;
        Ok(w)
    }
    /// The stored `When`, with a local time converted in `tz` (a time skipped by a
    /// daylight-saving change moves after it, as cron times do).
    pub fn resolve(&self, tz: &str) -> anyhow::Result<When> {
        match self {
            WhenInput::Cron { cron } => Ok(When::Cron { cron: cron.clone() }),
            WhenInput::Once { at: Some(at), local: None } => Ok(When::Once { at: *at }),
            WhenInput::Once { at: None, local: Some(l) } => {
                let zone = cron::zone(tz).map_err(anyhow::Error::msg)?;
                let dt: jiff::civil::DateTime = l.trim().replacen(' ', "T", 1).parse()
                    .map_err(|_| anyhow::anyhow!("`{l}` isn't a date and time: use YYYY-MM-DDTHH:MM"))?;
                let at = zone.to_ambiguous_timestamp(dt).compatible()
                    .map_err(|e| anyhow::anyhow!("`{l}` in {tz}: {e}"))?;
                Ok(When::Once { at: at.as_millisecond() })
            }
            WhenInput::Once { .. } => anyhow::bail!("a one-off needs exactly one of `at` (unix ms) or `local` (YYYY-MM-DDTHH:MM)"),
            _ => anyhow::bail!("a trigger isn't a time"),
        }
    }
}

/// Two timings that fire at the same times in `tz` (`@daily` is `0 0 * * *`).
fn same_timing(a: &When, b: &When) -> bool {
    match (a, b) {
        (When::Cron { cron: x }, When::Cron { cron: y }) => {
            x.trim() == y.trim() || matches!((cron::Cron::parse(x), cron::Cron::parse(y)), (Ok(p), Ok(q)) if p == q)
        }
        _ => a == b,
    }
}

/// Where replies also go: an existing note, or one found (or made) by its title.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum NoteChoice {
    Id { id: NoteId },
    Title { title: String },
}

/// `schedule.create`, and `schedule.update` (every field optional there).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInput {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub when: Option<WhenInput>,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub missed: Option<Missed>,
    #[serde(default)]
    pub conversation: Option<Conversation>,
    #[serde(default)]
    pub notify: Option<Notify>,
    /// A note, or `null` for none.
    #[serde(default, deserialize_with = "some")]
    pub note: Option<Option<NoteChoice>>,
    #[serde(default)]
    pub quiet_hours: Option<bool>,
    /// A number, or `null` (or 0) for no cap.
    #[serde(default, deserialize_with = "some")]
    pub daily_cap: Option<Option<u32>>,
    #[serde(default)]
    pub paused: Option<bool>,
    /// The "Then…" steps, or `null` for none.
    #[serde(default, deserialize_with = "some")]
    pub then: Option<Option<ThenInput>>,
    /// The CLI inside a session (`$MUSTER_SESSION_ID`); rpc.rs refuses an agent's.
    #[serde(default)]
    #[allow(dead_code)]
    pub from_session: Option<SessionId>,
}

/// "Then…" as clients send it: agents by name.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThenInput {
    #[serde(default)]
    pub on_success: Option<StepInput>,
    #[serde(default)]
    pub on_failure: Option<StepInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepInput {
    /// On failure only: just notify the human.
    #[serde(default)]
    pub notify: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub then: Option<Box<ThenInput>>,
}

/// Check "Then…" steps: each asks an agent that takes work of `origin` with a prompt (or, on
/// failure, notifies), at most `MAX_AGENT_HOPS` levels deep (each level is one hop further).
fn resolve_then(hub: &Hub, t: &ThenInput, origin: Origin, depth: u32) -> anyhow::Result<Option<Then>> {
    let max = crate::mcp::guard::MAX_AGENT_HOPS;
    ensure!(depth <= max, "a chain is at most {max} steps deep (each step is one hop, and messages stop after {max} hops)");
    let step = |s: &StepInput, failure: bool| -> anyhow::Result<Option<Step>> {
        if s.notify {
            ensure!(failure, "only the failure branch can just notify you (use Notify me: Always for successes)");
            ensure!(s.agent.is_none() && s.prompt.is_none() && s.then.is_none(), "a step either notifies you or asks an agent, not both");
            return Ok(Some(Step { notify: true, ..Default::default() }));
        }
        let Some(name) = s.agent.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
            ensure!(s.prompt.as_deref().is_none_or(|p| p.trim().is_empty()), "a step's prompt needs an agent to ask");
            return Ok(None);
        };
        let agent = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
        check_agent(hub, &agent, origin)?;
        let prompt = clean_prompt(s.prompt.as_deref().unwrap_or(""))?;
        let then = match &s.then {
            Some(t) => resolve_then(hub, t, origin, depth + 1)?.map(Box::new),
            None => None,
        };
        Ok(Some(Step { notify: false, agent_id: Some(agent.id), prompt, then }))
    };
    let out = Then {
        on_success: t.on_success.as_ref().map(|s| step(s, false)).transpose()?.flatten(),
        on_failure: t.on_failure.as_ref().map(|s| step(s, true)).transpose()?.flatten(),
    };
    Ok((out.on_success.is_some() || out.on_failure.is_some()).then_some(out))
}

/// A field that is present (even `null`) is `Some`.
fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn clean_name(name: Option<&str>, prompt: &str) -> anyhow::Result<String> {
    let n = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => {
            let first = prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Schedule");
            crate::agents::truncate_bytes(first, 60)
        }
    };
    ensure!(n.len() <= NAME_MAX && !n.chars().any(char::is_control), "a schedule name is one line of at most {NAME_MAX} bytes");
    Ok(n)
}

fn clean_prompt(p: &str) -> anyhow::Result<String> {
    let p = crate::mcp::tools::clean_text(p);
    ensure!(!p.trim().is_empty(), "the prompt must not be empty");
    ensure!(p.len() <= PROMPT_MAX, "the prompt is {} bytes; the limit is {PROMPT_MAX}", p.len());
    Ok(p)
}

/// Check `when` in `tz` and give its first slot after `now`.
pub fn first_slot(when: &When, tz: &str, now: i64) -> anyhow::Result<Option<i64>> {
    let zone = cron::zone(tz).map_err(anyhow::Error::msg)?;
    match when {
        When::Once { at } => {
            ensure!(*at > now, "that time has passed: pick a time in the future");
            ensure!(*at - now <= ONCE_MAX_AHEAD_MS, "a one-off schedule can be at most a year ahead");
            Ok(Some(*at))
        }
        When::Cron { cron } => {
            let c = cron::Cron::parse(cron).map_err(anyhow::Error::msg)?;
            if let Some(gap) = c.min_gap_ms(&zone, now, 200)
                && gap < plan::MIN_GAP_MS
            {
                anyhow::bail!(
                    "`{cron}` runs {} minutes apart at times; schedules run at most every {} minutes",
                    gap / 60_000,
                    plan::MIN_GAP_MS / 60_000
                );
            }
            let next = c.next_after(&zone, now);
            ensure!(next.is_some(), "`{cron}` never runs");
            Ok(next)
        }
        // A trigger has no time of its own: an event sets one.
        _ => Ok(None),
    }
}

/// The agent may take work of `origin` (the same rule the delivery check applies): scheduled
/// work for a timed schedule, triggered (`webhook`) work for a trigger.
fn check_agent(hub: &Hub, agent: &Agent, origin: Origin) -> anyhow::Result<()> {
    if !agent.accepts_origins.contains(&origin) {
        let (what, tick) = if origin == Origin::Webhook { ("triggered", "Webhook (triggered)") } else { ("scheduled", "Schedule") };
        let extra = if facts(hub, agent).subscription_claude {
            format!(
                " Claude subscription agents take {what} work only if you opt in (read the warning there first); \
                  a Codex or OpenRouter agent is the better fit."
            )
        } else {
            String::new()
        };
        anyhow::bail!(
            "agent {} doesn't take {what} work: tick {tick} under \"Takes messages started by\" in Edit agent \
             (or `muster agent edit {} --accepts human,{}`).{extra}",
            agent.name,
            agent.name,
            origin.as_str()
        );
    }
    Ok(())
}

fn resolve_note(hub: &Hub, choice: &NoteChoice) -> anyhow::Result<NoteId> {
    match choice {
        NoteChoice::Id { id } => Ok(crate::notes::get(hub, *id)?.id),
        NoteChoice::Title { title } => {
            let t = title.trim();
            ensure!(!t.is_empty(), "give the note a title");
            let found: Vec<NoteId> = crate::notes::list(hub)?.into_iter().filter(|n| n.title.eq_ignore_ascii_case(t)).map(|n| n.id).collect();
            match found.as_slice() {
                [id] => return Ok(*id),
                [] => {}
                many => anyhow::bail!(
                    "{} notes are named “{t}” (ids {}): pick the one for the replies by its id",
                    many.len(),
                    many.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
                ),
            }
            Ok(crate::notes::create(hub, Some(t), "", NoteAuthor::human(), None, None)?.id)
        }
    }
}

pub fn create(hub: &Hub, req: ScheduleInput) -> anyhow::Result<Value> {
    let name = req.agent.as_deref().context("agent is required")?;
    let agent = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
    let when = req.when.context("when is required: {\"kind\":\"cron\",\"cron\":\"0 9 * * 1-5\"} or {\"kind\":\"once\",\"at\":<unix ms>}")?;
    let origin = if when.is_trigger() { Origin::Webhook } else { Origin::Schedule };
    check_agent(hub, &agent, origin)?;
    let prompt = clean_prompt(req.prompt.as_deref().context("prompt is required")?)?;
    let tz = req.tz.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(cron::local_zone_name);
    cron::zone(&tz).map_err(anyhow::Error::msg)?;
    let when = when.resolve_in(hub, &tz, None)?;
    let now = hub.schedules.now();
    let next = first_slot(&when, &tz, now)?;
    ensure!(hub.schedules.inner.lock().list.len() < MAX_SCHEDULES, "Muster already has {MAX_SCHEDULES} schedules; remove some first");
    // A trigger has a daily cap unless one is given (a busy repo or note can't run it all day).
    let daily_cap = match req.daily_cap {
        Some(c) => c.filter(|c| *c > 0),
        None => when.is_trigger().then_some(triggers::DAILY_CAP_DEFAULT),
    };
    ensure!(daily_cap.is_none_or(|c| c <= 200), "the daily run cap is at most 200");
    let name = clean_name(req.name.as_deref(), &prompt)?;
    let then = match req.then.flatten() {
        Some(t) => resolve_then(hub, &t, origin, 1)?,
        None => None,
    };
    // Last: it may make the note.
    let note_id = match req.note.flatten() {
        Some(c) => Some(resolve_note(hub, &c)?),
        None => None,
    };
    let probe = Schedule { then: then.clone(), ..Default::default() };
    let f = facts_for(hub, &agent, &probe);
    let secret = matches!(when, When::Url { .. }).then(crate::paths::random_token);
    let mut s = Schedule {
        id: 0,
        agent_id: agent.id,
        name,
        prompt,
        when,
        tz,
        missed: req.missed.unwrap_or_default(),
        conversation: req.conversation.unwrap_or_default(),
        notify: req.notify.unwrap_or_default(),
        note_id,
        // Default on for Claude subscription agents, off for Codex and OpenRouter.
        quiet_hours: req.quiet_hours.unwrap_or(f.subscription_claude),
        daily_cap,
        paused: req.paused.unwrap_or(false),
        skip_slot: None,
        next_run: next,
        last_slot: None,
        last_fired: None,
        last_sent: None,
        renewed_at: now,
        expiry_warned: false,
        created_at: now,
        updated_at: now,
        then,
        event: None,
        watch: WatchState::default(),
        secret,
    };
    {
        let mut g = hub.schedules.inner.lock();
        s.id = hub.store().insert_schedule(&s)?;
        g.list.insert(s.id, s.clone());
    }
    hub.schedules.wake.notify_one();
    if s.when.is_trigger() {
        triggers::changed(hub, s.id);
    }
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

pub fn update(hub: &Hub, req: ScheduleInput) -> anyhow::Result<Value> {
    let id = req.id.context("id is required")?;
    ensure!(req.agent.is_none(), "a schedule's agent can't change: make a new schedule for the other agent");
    let note_id = match &req.note {
        Some(Some(c)) => Some(Some(resolve_note(hub, c)?)),
        Some(None) => Some(None),
        None => None,
    };
    let now = hub.schedules.now();
    let before = hub.schedules.get(id).with_context(|| format!("no schedule {id} (see `muster schedule ls`)"))?;
    let agent = hub.agents().by_id(before.agent_id).context("this schedule's agent was removed")?;
    // Checked before the edit (they read the hub): the new timing or trigger, and the steps.
    let tz_in = req.tz.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&before.tz).to_string();
    let new_when = match &req.when {
        Some(w) => Some(w.resolve_in(hub, &tz_in, Some(id))?),
        None => None,
    };
    let origin = if new_when.as_ref().unwrap_or(&before.when).is_trigger() { Origin::Webhook } else { Origin::Schedule };
    if origin != before.origin() {
        check_agent(hub, &agent, origin)?;
    }
    let new_then = match &req.then {
        Some(Some(t)) => Some(resolve_then(hub, t, origin, 1)?),
        Some(None) => Some(None),
        // The origin changed: the steps' agents must take the new one too.
        None if origin != before.origin() => {
            for a in step_agents(before.then.as_ref()).into_iter().filter_map(|a| hub.agents().by_id(a)) {
                check_agent(hub, &a, origin)?;
            }
            None
        }
        None => None,
    };
    let s = hub.schedules.edit(id, |s| {
        if let Some(p) = &req.prompt {
            s.prompt = clean_prompt(p)?;
        }
        if req.name.is_some() || req.prompt.is_some() {
            s.name = clean_name(req.name.as_deref().or(Some(&s.name)), &s.prompt)?;
        }
        let tz = req.tz.as_deref().map(str::trim).unwrap_or(&s.tz).to_string();
        cron::zone(&tz).map_err(anyhow::Error::msg)?;
        let when = new_when.clone().unwrap_or_else(|| s.when.clone());
        // Compared by what it does: re-sending the same timing (or `@daily` for `0 0 * * *`)
        // keeps the next run and Skip next as they are.
        let when_changed = !same_timing(&when, &s.when) || tz != s.tz;
        if when_changed {
            let was_trigger = s.when.is_trigger();
            s.when = when;
            s.tz = tz;
            s.next_run = first_slot(&s.when, &s.tz, now)?;
            s.skip_slot = None;
            // A trigger that watches something else starts afresh: no held event, no memory.
            if s.when.is_trigger() || was_trigger {
                s.event = None;
                s.watch = WatchState { generation: s.watch.generation + 1, ..WatchState::default() };
            }
            if matches!(s.when, When::Url { .. }) {
                s.secret.get_or_insert_with(crate::paths::random_token);
            } else {
                s.secret = None;
            }
        }
        if let Some(t) = &new_then {
            s.then = t.clone();
        }
        if let Some(m) = req.missed {
            s.missed = m;
        }
        if let Some(c) = req.conversation {
            s.conversation = c;
        }
        if let Some(n) = req.notify {
            s.notify = n;
        }
        if let Some(n) = note_id {
            s.note_id = n;
        }
        if let Some(q) = req.quiet_hours {
            s.quiet_hours = q;
        }
        if let Some(c) = req.daily_cap {
            let c = c.filter(|c| *c > 0);
            ensure!(c.is_none_or(|c| c <= 200), "the daily run cap is at most 200");
            s.daily_cap = c;
        }
        if let Some(p) = req.paused {
            s.paused = p;
        }
        Ok(())
    })?;
    if s.when.is_trigger() {
        triggers::changed(hub, s.id);
    }
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

pub fn remove(hub: &Hub, id: i64) -> anyhow::Result<Value> {
    let s = {
        let mut g = hub.schedules.inner.lock();
        let s = g.list.get(&id).cloned().with_context(|| format!("no schedule {id}"))?;
        hub.store().delete_schedule(id)?;
        g.list.remove(&id);
        s
    };
    hub.emit(ServerEvent::ScheduleRemoved { id, agent_id: s.agent_id });
    hub.triggers.forget(id);
    run::cancel_unsent(hub, Some(id), "its schedule was removed", true);
    Ok(json!({ "removed": id }))
}

/// An agent was removed: so are its schedules.
pub fn remove_for_agent(hub: &Hub, agent_id: i64) {
    let ids: Vec<i64> = hub.schedules.list().into_iter().filter(|s| s.agent_id == agent_id).map(|s| s.id).collect();
    for id in ids {
        if let Err(e) = remove(hub, id) {
            tracing::warn!(schedule = id, "removing a removed agent's schedule: {e:#}");
        }
    }
}

/// An agent's record changed (its provider: whether its schedules expire): tell clients.
pub fn agent_changed(hub: &Hub, agent_id: i64) {
    for s in hub.schedules.list().into_iter().filter(|s| s.agent_id == agent_id) {
        publish(hub, &s);
    }
    hub.schedules.wake.notify_one();
}

pub fn set_paused(hub: &Hub, id: i64, paused: bool) -> anyhow::Result<Value> {
    let s = hub.schedules.edit(id, |s| {
        s.paused = paused;
        Ok(())
    })?;
    if paused {
        run::cancel_unsent(hub, Some(id), plan::PAUSED, false);
    }
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

/// "Skip next" (skip the coming slot) or undo it.
pub fn set_skip(hub: &Hub, id: i64, skip: bool) -> anyhow::Result<Value> {
    let s = hub.schedules.edit(id, |s| {
        ensure!(!s.when.is_trigger(), "a trigger has no next time to skip: pause it instead");
        if skip {
            s.skip_slot = Some(s.next_run.context("this schedule has no next run to skip")?);
        } else {
            s.skip_slot = None;
        }
        Ok(())
    })?;
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

/// Renew a Claude subscription agent's recurring schedule for 7 days from now. An expired one
/// starts again from its next time (the slots it missed stay skipped).
pub fn renew(hub: &Hub, id: i64) -> anyhow::Result<Value> {
    let now = hub.schedules.now();
    let s = hub.schedules.edit(id, |s| {
        s.renewed_at = now;
        s.expiry_warned = false;
        if let When::Cron { .. } = s.when {
            s.next_run = first_slot(&s.when, &s.tz, now)?;
            s.skip_slot = None;
        }
        Ok(())
    })?;
    hub.schedules.expired_seen.lock().remove(&id);
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

pub fn list(hub: &Hub, agent: Option<&str>) -> anyhow::Result<Value> {
    let only = match agent {
        Some(n) => Some(hub.agents().get(n).ok_or_else(|| crate::agents::no_agent(hub, n))?.id),
        None => None,
    };
    let list: Vec<Value> = hub
        .schedules
        .list()
        .iter()
        .filter(|s| only.is_none_or(|a| s.agent_id == a))
        .map(|s| schedule_json(hub, s))
        .collect();
    Ok(json!({ "schedules": list, "settings": settings_json(hub, &hub.schedules.settings()) }))
}

pub fn runs(hub: &Hub, id: i64, before: Option<i64>, limit: Option<usize>) -> anyhow::Result<Value> {
    ensure!(hub.schedules.get(id).is_some(), "no schedule {id}");
    let limit = limit.unwrap_or(RUNS_DEFAULT).clamp(1, RUNS_MAX);
    let mut rs = hub.store().runs(id, before, limit + 1)?;
    let more = rs.len() > limit;
    rs.truncate(limit);
    Ok(json!({ "runs": rs.iter().map(|r| run_json(hub, r)).collect::<Vec<_>>(), "more": more }))
}

/// The preview's facts: its agent and its "Then…" steps' agents (a Claude subscription agent
/// anywhere brings quiet hours by default and the 7-day expiry, as `facts_for` does once saved).
fn preview_facts(hub: &Hub, agent: Option<&Agent>, then_agents: &[String]) -> Facts {
    let mut f = agent.map(|a| facts(hub, a)).unwrap_or_default();
    if !f.subscription_claude {
        f.subscription_claude = then_agents.iter().filter_map(|n| hub.agents().get(n)).any(|a| facts(hub, &a).subscription_claude);
    }
    f
}

/// `schedule.preview`: the reading and the next runs of `when`, without saving anything.
pub fn preview(hub: &Hub, when: &WhenInput, tz: Option<&str>, quiet_hours: Option<bool>, agent: Option<&str>, own: Option<i64>, then_agents: &[String]) -> anyhow::Result<Value> {
    let tz = tz.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(cron::local_zone_name);
    let when = &when.resolve_in(hub, &tz, own)?;
    if when.is_trigger() {
        let agent = match agent {
            Some(n) => Some(hub.agents().get(n).ok_or_else(|| crate::agents::no_agent(hub, n))?),
            None => None,
        };
        let f = preview_facts(hub, agent.as_ref(), then_agents);
        let note_off = match when {
            When::Note { note_id, .. } => hub.store().load_note(*note_id).ok().flatten().is_some_and(|n| !n.triggers),
            _ => false,
        };
        return Ok(json!({
            "reading": triggers::describe(hub, when),
            "tz": tz,
            "when": when,
            "next": [],
            "quiet": [],
            "quiet_hours_default": f.subscription_claude,
            "missed_default": "run_once",
            "expires": f.subscription_claude,
            "trigger": true,
            "note_triggers_off": note_off,
            "daily_cap_default": triggers::DAILY_CAP_DEFAULT,
        }));
    }
    let now = hub.schedules.now();
    let first = first_slot(when, &tz, now)?;
    let zone = cron::zone(&tz).map_err(anyhow::Error::msg)?;
    let mut next = Vec::new();
    if let Some(t) = first {
        next.push(t);
        if let When::Cron { cron } = when {
            let c = cron::Cron::parse(cron).map_err(anyhow::Error::msg)?;
            while next.len() < 3 {
                match c.next_after(&zone, *next.last().unwrap()) {
                    Some(n) => next.push(n),
                    None => break,
                }
            }
        }
    }
    let agent = match agent {
        Some(n) => Some(hub.agents().get(n).ok_or_else(|| crate::agents::no_agent(hub, n))?),
        None => None,
    };
    let f = preview_facts(hub, agent.as_ref(), then_agents);
    let quiet_on = quiet_hours.unwrap_or(f.subscription_claude);
    let quiet = quiet_of(&hub.schedules.settings());
    let probe = Schedule { when: when.clone(), tz: tz.clone(), ..Default::default() };
    let reading = match when {
        When::Cron { cron } => cron::Cron::parse(cron).map_err(anyhow::Error::msg)?.describe(),
        When::Once { at } => format!("Once, {}", cron::format(&zone, *at, "%a %-d %b %Y at %H:%M")),
        w => triggers::describe(hub, w),
    };
    Ok(json!({
        "reading": reading,
        "tz": tz,
        "when": when,
        "next": next,
        "quiet": next.iter().map(|t| quiet_on && quiet.contains(*t)).collect::<Vec<_>>(),
        "quiet_hours_default": f.subscription_claude,
        "missed_default": plan::effective_missed(&probe, now),
        "expires": f.subscription_claude && matches!(when, When::Cron { .. }),
    }))
}

/// `schedule.settings.set`: only the fields given change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChange {
    pub paused: Option<bool>,
    pub quiet_start: Option<String>,
    pub quiet_end: Option<String>,
    pub unattended_cap: Option<u32>,
    /// OpenRouter: the overall daily cap in dollars (always on: it can't be cleared).
    pub spend_cap: Option<f64>,
    /// OpenRouter: per-agent daily caps in dollars by agent name, `null` to clear one.
    pub agent_spend_caps: Option<BTreeMap<String, Option<f64>>>,
    #[serde(default)]
    #[allow(dead_code)]
    pub from_session: Option<SessionId>,
}

fn cents(dollars: f64) -> anyhow::Result<u32> {
    let c = (dollars * 100.0).round();
    ensure!(
        dollars.is_finite() && c >= 1.0 && c <= crate::spend::MAX_CAP_CENTS as f64,
        "a daily spend cap is $0.01 to ${}",
        crate::spend::MAX_CAP_CENTS / 100
    );
    Ok(c as u32)
}

pub fn set_settings(hub: &Hub, c: SettingsChange) -> anyhow::Result<Value> {
    let next = {
        let mut g = hub.schedules.inner.lock();
        let mut next = g.settings.clone();
        if let Some(p) = c.paused {
            next.paused = p;
        }
        for (field, v) in [(&mut next.quiet_start, c.quiet_start), (&mut next.quiet_end, c.quiet_end)] {
            if let Some(v) = v {
                let m = parse_hhmm(&v).with_context(|| format!("`{v}` isn't a time: use HH:MM, e.g. 23:00"))?;
                *field = format!("{:02}:{:02}", m / 60, m % 60);
            }
        }
        if let Some(n) = c.unattended_cap {
            ensure!(
                (1..=crate::mcp::guard::AGENT_DAILY_PROMPT_CAP as u32).contains(&n),
                "the unattended cap is 1 to {} messages a day",
                crate::mcp::guard::AGENT_DAILY_PROMPT_CAP
            );
            next.unattended_cap = n;
        }
        if let Some(d) = c.spend_cap {
            next.spend_cap_cents = cents(d)?;
        }
        if let Some(caps) = &c.agent_spend_caps {
            for (name, cap) in caps {
                let a = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
                match cap {
                    Some(d) => {
                        next.agent_spend_caps.insert(a.id, cents(*d)?);
                    }
                    None => {
                        next.agent_spend_caps.remove(&a.id);
                    }
                }
            }
        }
        // Caps of agents that are gone go too.
        next.agent_spend_caps.retain(|id, _| hub.agents().by_id(*id).is_some());
        hub.store().save_runtime(SETTINGS_KEY, &next)?;
        g.settings = next.clone();
        next
    };
    hub.schedules.wake.notify_one();
    if next.paused {
        run::cancel_unsent(hub, None, plan::PAUSED_ALL, false);
    }
    let v = settings_json(hub, &next);
    hub.emit(ServerEvent::ScheduleSettings(v.clone()));
    // Every schedule's status may read differently (paused for all).
    for s in hub.schedules.list() {
        publish(hub, &s);
    }
    Ok(v)
}

/// `schedule.run`: run it now (origin schedule, like any run), whatever its timing. Returns the
/// run: pending, or skipped when its previous run is still going.
pub fn run_now(hub: &Arc<Hub>, id: i64) -> anyhow::Result<Value> {
    let s = hub.schedules.get(id).with_context(|| format!("no schedule {id}"))?;
    let agent = hub.agents().by_id(s.agent_id).context("this schedule's agent was removed")?;
    check_agent(hub, &agent, s.origin())?;
    let now = hub.schedules.now();
    let mut r = Run::new(id, now, RunKind::Manual, now);
    // The busy rule, answered now: the same schedule's previous run is still going.
    if run::previous_running(hub, id, i64::MAX)?
        && let plan::Gate::Skip(why) = plan::gate(true, false, 0)
    {
        r.outcome = Outcome::Skipped;
        r.reason = Some(why);
        r.finished_at = Some(now);
    }
    r.id = hub.store().insert_run(&r)?;
    publish_run(hub, &r);
    if r.outcome == Outcome::Pending {
        run::spawn_execute(hub.clone(), r.clone());
    }
    Ok(run_json(hub, &r))
}

/// The injector is about to submit a prompt from `source` (and checks again right before
/// Enter). For a schedule's message: may it still go? `Some(action)`: no, and why. The run must
/// still be pending (not timed out, its schedule not removed) and, for a timed run, its rules
/// must still hold now (`plan::still_ok`), as when it was taken up. Allowed: the time is kept
/// for the 15-minute floor. Any other prompt: `None`.
pub fn submit_gate(hub: &Hub, source: &crate::agents::Source, commit: bool) -> Option<plan::Action> {
    let crate::agents::Source::Chain(p) = source else { return None };
    let msg = p.msg_id?;
    if p.origin == Origin::Human {
        return None;
    }
    let m = hub.store().agent_message(msg).ok().flatten()?;
    run::parse_sender(&m.from)?;
    let deny = |why: &str| Some(plan::Action::Skip(why.to_string()));
    let Some(r) = run::run_for_msg(hub, msg, &m.from) else { return deny("its schedule was removed") };
    if r.outcome != Outcome::Pending {
        return deny("its run had already ended before the prompt could be submitted");
    }
    let Some(s) = hub.schedules.get(r.schedule_id) else { return deny("its schedule was removed") };
    let Some(agent) = hub.agents().by_id(s.agent_id) else { return deny("its agent was removed") };
    if r.kind != RunKind::Manual {
        let settings = hub.schedules.settings();
        // A step is part of its run: the schedule's daily cap and floor were its run's.
        let (today, since) = if r.kind == RunKind::Chain {
            (0, None)
        } else {
            (
                hub.store().count_delivered_runs_since(s.id, now_ms() - plan::DAY_MS, Some(msg)).unwrap_or(0),
                s.last_sent.map(|t| now_ms() - t),
            )
        };
        if let Some(a) = plan::still_ok(&s, hub.schedules.now(), &settings, &quiet_of(&settings), &facts_for(hub, &agent, &s), today, since) {
            return Some(a);
        }
        // A note trigger's prompt goes only while its note still allows triggers.
        if r.kind == RunKind::Triggered
            && let Some(a) = triggers::note_flag(hub, &s)
        {
            return Some(a);
        }
    }
    // Recorded at the last check, right before Enter, for timed runs (Run now is the human's).
    if commit
        && r.kind != RunKind::Manual
        && r.kind != RunKind::Chain
        && let Err(e) = hub.schedules.edit(s.id, |x| {
            x.last_sent = Some(now_ms());
            Ok(())
        })
    {
        tracing::warn!(schedule = s.id, "recording when its prompt went out: {e:#}");
    }
    None
}

/// Is `from` a schedule's, trigger's or "Then…" step's sender?
pub fn is_run_sender(from: &str) -> bool {
    run::parse_sender(from).is_some()
}

/// The injector took a schedule's prompt back without submitting it (`submit_gate` said no).
pub fn not_submitted(hub: &Arc<Hub>, source: &crate::agents::Source, action: plan::Action) {
    let crate::agents::Source::Chain(p) = source else { return };
    let Some(msg) = p.msg_id else { return };
    run::unsent(hub, msg, action);
}

/// A reply arrived for agent message `msg` (the hook handler): a schedule run's reply goes to
/// its note even when the run had stopped waiting for it, once.
pub fn on_reply(hub: &Hub, msg: i64) {
    let Ok(Some(r)) = hub.store().run_by_msg(msg) else { return };
    let Some(s) = hub.schedules.get(r.schedule_id) else { return };
    if r.kind != RunKind::Chain {
        run::reply_to_note_once(hub, &s, &r);
    }
    if r.outcome != Outcome::Pending {
        // Its history line now reads "replied" (late).
        publish_run(hub, &r);
    }
}

/// Start the scheduler (once, at daemon start, after agents autostart is under way).
pub fn spawn(hub: Arc<Hub>) {
    triggers::spawn(hub.clone());
    tokio::spawn(async move {
        run::recover(&hub);
        run::scheduler_loop(hub).await;
    });
}

/// `trigger.fire` (`muster trigger <name>`): the human's own client, no secret needed.
pub fn fire(hub: &Hub, name: &str, payload: Option<&str>) -> anyhow::Result<Value> {
    triggers::fire_url(hub, name, None, payload.map(str::as_bytes), &format!("`muster trigger {name}` was run"))
        .map_err(|(_, why)| anyhow::anyhow!(if why.starts_with("unknown") { format!("no trigger named {name} (see `muster schedule ls`)") } else { why }))
}

#[cfg(test)]
mod tests;
