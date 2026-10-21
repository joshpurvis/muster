//! The scheduler's decisions, as plain functions of a schedule, the time and the settings (no
//! store, no hub), so tests can cover daylight saving, missed runs, clock jumps and the 7-day
//! expiry with a fake clock.

use jiff::tz::TimeZone;
use serde::Serialize;

use super::{Missed, RunKind, Schedule, Settings, When, cron, parse_hhmm};

pub const MINUTE_MS: i64 = 60_000;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 24 * HOUR_MS;
/// No schedule runs more often than this (for every agent; Claude Code's own floor is 1 minute).
pub const MIN_GAP_MS: i64 = 15 * MINUTE_MS;
/// A slot this late still counts as on time (a reload, a short suspend).
pub const GRACE_MS: i64 = 10 * MINUTE_MS;
/// A recurring schedule of a Claude subscription agent stops this long after it was made or
/// renewed (Claude Code's own limit for its recurring loops).
pub const EXPIRY_MS: i64 = 7 * DAY_MS;
/// A run waits in line behind a busy agent for at most this long.
pub const LINE_WAIT_MS: i64 = HOUR_MS;
/// How long a run waits for the agent's reply.
pub const RUN_TIMEOUT_MS: i64 = HOUR_MS;
/// Schedules faster than this skip missed runs (Auto).
pub const SKIP_MISSED_AT_OR_BELOW_MS: i64 = HOUR_MS;
/// The loop never sleeps longer than this, so it re-reads the wall clock soon after a suspend.
pub const MAX_SLEEP_MS: i64 = MINUTE_MS;
/// A gap this big between the wall clock and the monotonic clock is a jump.
pub const JUMP_MS: i64 = 2 * MINUTE_MS;

pub const PAUSED: &str = "paused";
pub const PAUSED_ALL: &str = "all schedules paused";

/// What the rules need to know about a schedule's agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    /// A Claude agent on its subscription (not OpenRouter): expiry and quiet-hours defaults.
    pub subscription_claude: bool,
}

/// When a Claude subscription agent's recurring schedule stops (None: it doesn't).
pub fn expires_at(s: &Schedule, f: &Facts) -> Option<i64> {
    (f.subscription_claude && matches!(s.when, When::Cron { .. })).then_some(s.renewed_at + EXPIRY_MS)
}

/// The quiet hours, in this machine's time zone.
pub struct Quiet {
    start: u32,
    end: u32,
    tz: TimeZone,
    pub label: String,
}

impl Quiet {
    pub fn new(s: &Settings, tz: TimeZone) -> Quiet {
        let start = parse_hhmm(&s.quiet_start).unwrap_or(23 * 60);
        let end = parse_hhmm(&s.quiet_end).unwrap_or(7 * 60);
        Quiet { start, end, tz, label: format!("{}–{}", s.quiet_start, s.quiet_end) }
    }
    pub fn contains(&self, ms: i64) -> bool {
        let Some(m) = cron::minute_of_day(&self.tz, ms) else { return false };
        match self.start.cmp(&self.end) {
            std::cmp::Ordering::Less => m >= self.start && m < self.end,
            std::cmp::Ordering::Greater => m >= self.start || m < self.end,
            std::cmp::Ordering::Equal => false,
        }
    }
}

/// What "If missed: Auto" means for this schedule: one-offs and anything slower than hourly run
/// once when Muster is back; hourly-or-faster schedules skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissedPolicy {
    RunOnce,
    Skip,
}

pub fn effective_missed(s: &Schedule, now: i64) -> MissedPolicy {
    match s.missed {
        Missed::RunOnce => MissedPolicy::RunOnce,
        Missed::Skip => MissedPolicy::Skip,
        Missed::Auto => match &s.when {
            When::Once { .. } => MissedPolicy::RunOnce,
            When::Cron { cron } => {
                let gap = cron::Cron::parse(cron)
                    .ok()
                    .zip(cron::zone(&s.tz).ok())
                    .and_then(|(c, tz)| c.min_gap_ms(&tz, now, 30));
                match gap {
                    Some(g) if g <= SKIP_MISSED_AT_OR_BELOW_MS => MissedPolicy::Skip,
                    _ => MissedPolicy::RunOnce,
                }
            }
        },
    }
}

/// What to do with a due slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Fire(RunKind),
    Skip(String),
    Refuse(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    /// The slot acted on: the newest one at or before now.
    pub slot: i64,
    /// Slots at or before now since the schedule last ran (this one included).
    pub count: u32,
    /// The schedule's next slot after this.
    pub next_run: Option<i64>,
    pub action: Action,
    /// Record a run row (false: a repeat of the same pause or expiry skip, which would only
    /// fill the history).
    pub record: bool,
}

/// Is schedule `s` due at `now`, and what happens? `runs_today`: its runs that reached the agent
/// in the last 24 hours; `last_reason`: the reason on its newest run.
pub fn due(s: &Schedule, now: i64, settings: &Settings, quiet: &Quiet, f: &Facts, runs_today: u32, last_reason: Option<&str>) -> Option<Due> {
    let first = s.next_run?;
    if first > now {
        return None;
    }
    let (slot, count, next_run) = match &s.when {
        When::Once { .. } => (first, 1, None),
        When::Cron { cron } => {
            let parsed = cron::Cron::parse(cron).ok().zip(cron::zone(&s.tz).ok());
            let Some((c, tz)) = parsed else {
                // Unreadable now (a zone removed from the system): stop rather than spin.
                return Some(Due { slot: first, count: 1, next_run: None, action: Action::Skip(format!("can't read its timing ({cron} in {})", s.tz)), record: true });
            };
            let (mut slot, mut count) = (first, 1u32);
            while count < 100_000 {
                match c.next_after(&tz, slot) {
                    Some(n) if n <= now => {
                        slot = n;
                        count += 1;
                    }
                    _ => break,
                }
            }
            (slot, count, c.next_after(&tz, now))
        }
    };
    let late = now - slot > GRACE_MS;
    let skip = |why: String, dedupe: bool| {
        let record = !(dedupe && last_reason == Some(why.as_str()));
        Some(Due { slot, count, next_run, action: Action::Skip(why), record })
    };
    if let Some(exp) = expires_at(s, f)
        && slot >= exp
    {
        return skip(EXPIRED.into(), true);
    }
    if settings.paused {
        return skip(PAUSED_ALL.into(), true);
    }
    if s.paused {
        return skip(PAUSED.into(), true);
    }
    if late && effective_missed(s, now) == MissedPolicy::Skip {
        let n = if count == 1 { "1 run".to_string() } else { format!("{count} runs") };
        return skip(format!("missed: Muster was off or asleep ({n})"), false);
    }
    if s.skip_slot.is_some_and(|k| k >= first && k <= slot) {
        return skip("skipped by you (Skip next)".into(), false);
    }
    // A catch-up runs now, so it is now that must be outside the quiet hours.
    if s.quiet_hours && quiet.contains(if late { now } else { slot }) {
        return skip(format!("quiet hours ({})", quiet.label), false);
    }
    if let Some(cap) = s.daily_cap
        && runs_today >= cap
    {
        return Some(Due {
            slot,
            count,
            next_run,
            action: Action::Refuse(format!("daily cap: {cap} runs in the last 24 hours")),
            record: true,
        });
    }
    let kind = if late { RunKind::CatchUp } else { RunKind::Scheduled };
    Some(Due { slot, count, next_run, action: Action::Fire(kind), record: true })
}

pub const EXPIRED: &str = "expired: a Claude subscription agent's schedule stops after 7 days unless you renew it";

/// Should the "expires tomorrow" notice go out now?
pub fn warn_expiry(s: &Schedule, f: &Facts, now: i64) -> bool {
    match expires_at(s, f) {
        Some(e) => !s.expiry_warned && s.next_run.is_some() && now >= e - DAY_MS && now < e,
        None => false,
    }
}

/// What a run waiting for its turn should do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Go,
    Wait,
    Skip(String),
}

/// The busy rule: skip when the same schedule's previous run is still going; otherwise wait in
/// line while the agent is busy, for at most `LINE_WAIT_MS` since the run was taken up.
pub fn gate(previous_running: bool, agent_busy: bool, waited_ms: i64) -> Gate {
    if previous_running {
        return Gate::Skip("busy: the previous run is still going".into());
    }
    if !agent_busy {
        return Gate::Go;
    }
    if waited_ms >= LINE_WAIT_MS {
        return Gate::Skip("busy: the agent was busy for an hour".into());
    }
    Gate::Wait
}

/// The clock check after a sleep: how far the wall clock moved beyond the monotonic clock
/// (positive: a suspend, or the clock set forward; negative: set back). None: no jump.
pub fn clock_jump(prev_wall: i64, wall: i64, mono_elapsed_ms: i64) -> Option<i64> {
    let drift = (wall - prev_wall) - mono_elapsed_ms;
    (drift.abs() >= JUMP_MS).then_some(drift)
}

/// How long the loop sleeps: until the earliest next slot, at most `MAX_SLEEP_MS`.
pub fn sleep_ms(now: i64, next_slots: impl Iterator<Item = i64>) -> i64 {
    next_slots.map(|t| t - now).min().unwrap_or(MAX_SLEEP_MS).clamp(0, MAX_SLEEP_MS)
}

/// The prompt with `{date}`, `{last_run}` and `{last_reply}` filled in.
pub fn fill_prompt(prompt: &str, tz: &TimeZone, slot: i64, last_run: Option<i64>, last_reply: Option<&str>) -> String {
    let mut out = prompt.replace("{date}", &cron::format(tz, slot, "%Y-%m-%d"));
    if out.contains("{last_run}") {
        let v = match last_run {
            Some(t) => cron::format(tz, t, "%Y-%m-%d %H:%M"),
            None => "never (this is the first run)".into(),
        };
        out = out.replace("{last_run}", &v);
    }
    if out.contains("{last_reply}") {
        let v = match last_reply {
            Some(r) => crate::agents::truncate_bytes(r, super::LAST_REPLY_MAX),
            None => "(no earlier reply)".into(),
        };
        out = out.replace("{last_reply}", &v);
    }
    out
}
