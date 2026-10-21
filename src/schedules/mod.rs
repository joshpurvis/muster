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

pub mod cron;
pub mod plan;
mod run;

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

/// When a schedule fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    /// A cron line (presets are cron lines too), in the schedule's time zone.
    Cron { cron: String },
    /// Once, at this unix ms.
    Once { at: i64 },
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
    /// Start of the 7-day window of a Claude subscription agent's recurring schedule.
    pub renewed_at: i64,
    /// The "expires tomorrow" notice was given for this window.
    pub expiry_warned: bool,
    pub created_at: i64,
    pub updated_at: i64,
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
            renewed_at: 0,
            expiry_warned: false,
            created_at: 0,
            updated_at: 0,
        }
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
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Scheduled => "scheduled",
            RunKind::CatchUp => "catch_up",
            RunKind::Manual => "manual",
        }
    }
    pub fn parse(s: &str) -> RunKind {
        match s {
            "catch_up" => RunKind::CatchUp,
            "manual" => RunKind::Manual,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            paused: false,
            quiet_start: "23:00".into(),
            quiet_end: "07:00".into(),
            unattended_cap: crate::mcp::guard::UNATTENDED_DAILY_CAP_DEFAULT,
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

/// Facts about a schedule's agent that the rules read.
pub fn facts(agent: &Agent) -> Facts {
    Facts {
        subscription_claude: agent.kind == SessionKind::Claude && agent.provider == crate::openrouter::Provider::Subscription,
    }
}

fn quiet_of(settings: &Settings) -> Quiet {
    Quiet::new(settings, jiff::tz::TimeZone::system())
}

// ----- JSON -----

/// A schedule as `schedule.list` and `schedule_upserted` carry it.
pub fn schedule_json(hub: &Hub, s: &Schedule) -> Value {
    let agent = hub.agents().by_id(s.agent_id);
    let now = hub.schedules.now();
    let f = agent.as_ref().map(facts).unwrap_or_default();
    let expires_at = plan::expires_at(s, &f);
    let expired = expires_at.is_some_and(|e| now >= e);
    let status = if expired {
        "expired"
    } else if s.next_run.is_none() {
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
    };
    let last_run = hub.store().runs(s.id, None, 1).ok().and_then(|r| r.into_iter().next()).map(|r| run_json(hub, &r));
    let note_title = s.note_id.and_then(|n| hub.store().load_note(n).ok().flatten()).map(|n| n.title);
    json!({
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
    })
}

pub fn settings_json(s: &Settings) -> Value {
    json!({
        "paused": s.paused,
        "quiet_start": s.quiet_start,
        "quiet_end": s.quiet_end,
        "unattended_cap": s.unattended_cap,
        "tz": cron::local_zone_name(),
    })
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
    pub when: Option<When>,
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
    /// The CLI inside a session (`$MUSTER_SESSION_ID`); rpc.rs refuses an agent's.
    #[serde(default)]
    #[allow(dead_code)]
    pub from_session: Option<SessionId>,
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
    }
}

/// The agent may take scheduled work (the same rule the delivery check applies).
fn check_agent(agent: &Agent) -> anyhow::Result<()> {
    if !agent.accepts_origins.contains(&Origin::Schedule) {
        let extra = if facts(agent).subscription_claude {
            " Claude subscription agents take scheduled work only if you opt in (read the warning there first); \
              a Codex or OpenRouter agent is the better fit."
        } else {
            ""
        };
        anyhow::bail!(
            "agent {} doesn't take scheduled work: tick Schedule under \"Takes messages started by\" in Edit agent \
             (or `muster agent edit {} --accepts human,schedule`).{extra}",
            agent.name,
            agent.name
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
            if let Some(n) = crate::notes::list(hub)?.into_iter().find(|n| n.title.eq_ignore_ascii_case(t)) {
                return Ok(n.id);
            }
            Ok(crate::notes::create(hub, Some(t), "", NoteAuthor::human(), None, None)?.id)
        }
    }
}

pub fn create(hub: &Hub, req: ScheduleInput) -> anyhow::Result<Value> {
    let name = req.agent.as_deref().context("agent is required")?;
    let agent = hub.agents().get(name).ok_or_else(|| crate::agents::no_agent(hub, name))?;
    check_agent(&agent)?;
    let prompt = clean_prompt(req.prompt.as_deref().context("prompt is required")?)?;
    let when = req.when.context("when is required: {\"kind\":\"cron\",\"cron\":\"0 9 * * 1-5\"} or {\"kind\":\"once\",\"at\":<unix ms>}")?;
    let tz = req.tz.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(cron::local_zone_name);
    let now = hub.schedules.now();
    let next = first_slot(&when, &tz, now)?;
    ensure!(hub.schedules.inner.lock().list.len() < MAX_SCHEDULES, "Muster already has {MAX_SCHEDULES} schedules; remove some first");
    let daily_cap = req.daily_cap.flatten().filter(|c| *c > 0);
    ensure!(daily_cap.is_none_or(|c| c <= 200), "the daily run cap is at most 200");
    let name = clean_name(req.name.as_deref(), &prompt)?;
    // Last: it may make the note.
    let note_id = match req.note.flatten() {
        Some(c) => Some(resolve_note(hub, &c)?),
        None => None,
    };
    let f = facts(&agent);
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
        renewed_at: now,
        expiry_warned: false,
        created_at: now,
        updated_at: now,
    };
    {
        let mut g = hub.schedules.inner.lock();
        s.id = hub.store().insert_schedule(&s)?;
        g.list.insert(s.id, s.clone());
    }
    hub.schedules.wake.notify_one();
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
    let s = hub.schedules.edit(id, |s| {
        if let Some(p) = &req.prompt {
            s.prompt = clean_prompt(p)?;
        }
        if req.name.is_some() || req.prompt.is_some() {
            s.name = clean_name(req.name.as_deref().or(Some(&s.name)), &s.prompt)?;
        }
        let when_changed = req.when.as_ref().is_some_and(|w| *w != s.when) || req.tz.as_ref().is_some_and(|t| t.trim() != s.tz);
        if let Some(w) = req.when.clone() {
            s.when = w;
        }
        if let Some(t) = &req.tz {
            s.tz = t.trim().to_string();
        }
        if when_changed {
            s.next_run = first_slot(&s.when, &s.tz, now)?;
            s.skip_slot = None;
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
    publish(hub, &s);
    Ok(schedule_json(hub, &s))
}

/// "Skip next" (skip the coming slot) or undo it.
pub fn set_skip(hub: &Hub, id: i64, skip: bool) -> anyhow::Result<Value> {
    let s = hub.schedules.edit(id, |s| {
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
    Ok(json!({ "schedules": list, "settings": settings_json(&hub.schedules.settings()) }))
}

pub fn runs(hub: &Hub, id: i64, before: Option<i64>, limit: Option<usize>) -> anyhow::Result<Value> {
    ensure!(hub.schedules.get(id).is_some(), "no schedule {id}");
    let limit = limit.unwrap_or(RUNS_DEFAULT).clamp(1, RUNS_MAX);
    let mut rs = hub.store().runs(id, before, limit + 1)?;
    let more = rs.len() > limit;
    rs.truncate(limit);
    Ok(json!({ "runs": rs.iter().map(|r| run_json(hub, r)).collect::<Vec<_>>(), "more": more }))
}

/// `schedule.preview`: the reading and the next runs of `when`, without saving anything.
pub fn preview(hub: &Hub, when: &When, tz: Option<&str>, quiet_hours: Option<bool>, agent: Option<&str>) -> anyhow::Result<Value> {
    let tz = tz.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(cron::local_zone_name);
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
    let f = agent.as_ref().map(facts).unwrap_or_default();
    let quiet_on = quiet_hours.unwrap_or(f.subscription_claude);
    let quiet = quiet_of(&hub.schedules.settings());
    let probe = Schedule { when: when.clone(), tz: tz.clone(), ..Default::default() };
    let reading = match when {
        When::Cron { cron } => cron::Cron::parse(cron).map_err(anyhow::Error::msg)?.describe(),
        When::Once { at } => format!("Once, {}", cron::format(&zone, *at, "%a %-d %b %Y at %H:%M")),
    };
    Ok(json!({
        "reading": reading,
        "tz": tz,
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
    #[serde(default)]
    #[allow(dead_code)]
    pub from_session: Option<SessionId>,
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
        hub.store().save_runtime(SETTINGS_KEY, &next)?;
        g.settings = next.clone();
        next
    };
    hub.schedules.wake.notify_one();
    let v = settings_json(&next);
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
    check_agent(&agent)?;
    let now = hub.schedules.now();
    let mut r = Run {
        id: 0,
        schedule_id: id,
        slot: now,
        kind: RunKind::Manual,
        started_at: now,
        msg_id: None,
        outcome: Outcome::Pending,
        reason: None,
        finished_at: None,
    };
    // The busy rule, answered now: the same schedule's previous run is still going.
    if hub.store().pending_runs()?.iter().any(|p| p.schedule_id == id)
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

/// Start the scheduler (once, at daemon start, after agents autostart is under way).
pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        run::recover(&hub);
        run::scheduler_loop(hub).await;
    });
}

#[cfg(test)]
mod tests;
