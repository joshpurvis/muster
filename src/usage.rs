//! Claude subscription usage, as Claude Code reports it to its status line
//! (docs/ACCOUNT_PROFILES.md "Usage observations from Claude Code" and "Choosing where a new task
//! runs").
//!
//! - Source: the documented status-line JSON, `rate_limits.five_hour` / `rate_limits.seven_day`
//!   (`used_percentage` 0-100, `resets_at` Unix seconds). Muster's reporter (`muster statusline`,
//!   src/statusline.rs) is the session's status-line command: it runs the user's own status line
//!   unchanged and forwards only those numbers, authenticated by the run's session token.
//! - Attribution: the daemon maps the token to its session, run (generation) and pinned profile.
//!   Nothing in the payload names an account.
//! - Freshness: Claude Code re-runs the status line for reasons other than new provider data (a
//!   timer, a mode change, a reset passing), so a report counts as newly fetched only when the
//!   run's API time advanced, its numbers changed, or it is the run's first report. Anything else
//!   is a cached redraw: it never replaces or renews another observation.
//! - Unknown stays unknown: no observation, a stale one, an unconfirmed one or one whose reset has
//!   passed never counts as headroom. Nothing here probes an account or reads a credential.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::account_profiles::{AccountProfile, ProfileChoice, ProfileId};
use crate::model::SessionId;

/// An observation older than this no longer counts as current (the five-hour window moves).
pub const STALE_AFTER_MS: i64 = 30 * 60 * 1000;
/// An account at or above this share of either window is not suggested.
pub const NEAR_LIMIT: f64 = 90.0;
/// A fresh report with unchanged numbers renews an observation's time at most this often.
const RENEW_EVERY_MS: i64 = 30 * 1000;
/// A cached redraw bumps its own observation's "reported" time at most this often.
const REPORTED_EVERY_MS: i64 = 60 * 1000;
const DAY_S: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    FiveHour,
    SevenDay,
}

impl Window {
    pub const ALL: [Window; 2] = [Window::FiveHour, Window::SevenDay];
    pub fn key(self) -> &'static str {
        match self {
            Window::FiveHour => "five_hour",
            Window::SevenDay => "seven_day",
        }
    }
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.key() == key)
    }
    fn label(self) -> &'static str {
        match self {
            Window::FiveHour => "5-hour",
            Window::SevenDay => "weekly",
        }
    }
}

/// One window as one report gave it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub used_percentage: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

/// What a status-line callback carried, after validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub windows: BTreeMap<Window, Reading>,
    /// `cost.total_api_duration_ms`: grows with every API response of the run.
    pub api_ms: Option<f64>,
}

/// Validate a reporter body (`{"rate_limits": {...}, "api_duration_ms": n}`). Each window is
/// taken only when both of its numbers are plausible; anything else is dropped, not guessed.
pub fn parse_report(body: &Value, now_ms: i64) -> Report {
    let now_s = now_ms / 1000;
    let mut windows = BTreeMap::new();
    for w in Window::ALL {
        let Some(o) = body.get("rate_limits").and_then(|r| r.get(w.key())) else { continue };
        let used = o.get("used_percentage").and_then(Value::as_f64);
        let resets = o.get("resets_at").and_then(Value::as_f64);
        if let (Some(used), Some(resets)) = (used, resets)
            && used.is_finite() && (0.0..=100.0).contains(&used)
            && resets.is_finite() && resets > (now_s - 8 * DAY_S) as f64 && resets < (now_s + 8 * DAY_S) as f64
        {
            windows.insert(w, Reading { used_percentage: used, resets_at: resets as i64 });
        }
    }
    let api_ms = body.get("api_duration_ms").and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0);
    Report { windows, api_ms }
}

/// The run (session and process generation) a report came from.
pub type RunKey = (SessionId, u64);

/// The newest usable observation of one window for one profile key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(flatten)]
    pub reading: Reading,
    /// Unix ms when a report carried newly fetched data. None: reported, but freshness was
    /// never established (a cached redraw seen before any fresh report).
    pub observed_at: Option<i64>,
    /// Unix ms of the newest callback that carried these numbers.
    pub reported_at: i64,
    pub session_id: SessionId,
    pub generation: u64,
}

/// What a run last reported, to tell new provider data from a redraw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMemory {
    pub generation: u64,
    pub api_ms: Option<f64>,
    pub last: BTreeMap<Window, Reading>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    /// None: nothing about the run changed.
    pub run: Option<RunMemory>,
    pub updates: Vec<(Window, Observation)>,
    /// Whether anything a client shows changed.
    pub notify: bool,
}

/// Merge one report from run `run` into the stored observations of its profile key.
pub fn apply(previous: Option<&RunMemory>, stored: &BTreeMap<Window, Observation>, run: RunKey,
    report: &Report, now: i64) -> Applied {
    let previous = previous.filter(|m| m.generation == run.1);
    let api_advanced = matches!((previous.and_then(|m| m.api_ms), report.api_ms), (Some(a), Some(b)) if b > a);
    let mut memory = previous.cloned().unwrap_or(RunMemory { generation: run.1, ..Default::default() });
    let mut out = Applied::default();
    for (&w, &reading) in &report.windows {
        let last = previous.and_then(|m| m.last.get(&w));
        let fresh = last.is_none() || api_advanced || last != Some(&reading);
        let mine = |o: &Observation| (o.session_id, o.generation) == run;
        match stored.get(&w) {
            Some(o) if fresh && mine(o) && o.reading == reading
                && o.observed_at.is_some_and(|t| now - t < RENEW_EVERY_MS) => {}
            _ if fresh => {
                let o = Observation { reading, observed_at: Some(now), reported_at: now, session_id: run.0, generation: run.1 };
                out.updates.push((w, o));
                out.notify = true;
            }
            None => {
                let o = Observation { reading, observed_at: None, reported_at: now, session_id: run.0, generation: run.1 };
                out.updates.push((w, o));
                out.notify = true;
            }
            Some(o) if mine(o) && o.reading == reading && now - o.reported_at >= REPORTED_EVERY_MS => {
                out.updates.push((w, Observation { reported_at: now, ..o.clone() }));
            }
            // A redraw never replaces or renews another run's (or a newer) observation.
            Some(_) => {}
        }
        memory.last.insert(w, reading);
    }
    if report.api_ms.is_some() {
        memory.api_ms = report.api_ms;
    }
    if previous != Some(&memory) {
        out.run = Some(memory);
    }
    out
}

/// Observations are stored under the profile the reporting session was pinned to.
pub fn account_key(profile: Option<ProfileId>) -> String {
    match profile {
        Some(id) => format!("profile:{id}"),
        None => "default".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing reported for this account.
    Unknown,
    /// Newly fetched within [`STALE_AFTER_MS`] and the window has not reset.
    Current,
    /// Older than [`STALE_AFTER_MS`].
    Stale,
    /// Reported, but never shown to be newly fetched.
    Unconfirmed,
    /// Its reset time passed: the old number no longer applies and the new one is unknown.
    Reset,
}

pub fn status(o: &Observation, now: i64) -> Status {
    if o.reading.resets_at * 1000 <= now {
        return Status::Reset;
    }
    match o.observed_at {
        None => Status::Unconfirmed,
        Some(t) if now - t > STALE_AFTER_MS => Status::Stale,
        Some(_) => Status::Current,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowView {
    pub status: Status,
    pub used_percentage: Option<f64>,
    pub resets_at: Option<i64>,
    pub observed_at: Option<i64>,
    pub reported_at: Option<i64>,
    /// The profile whose session reported it, when that is another profile on the same account.
    pub reported_by: Option<String>,
}

impl WindowView {
    fn unknown() -> Self {
        WindowView { status: Status::Unknown, used_percentage: None, resets_at: None, observed_at: None,
            reported_at: None, reported_by: None }
    }
    fn current(&self) -> Option<f64> {
        (self.status == Status::Current).then_some(self.used_percentage).flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    /// `"default"` or a profile id, as a session request names it.
    pub choice: ProfileChoice,
    pub name: String,
    pub enabled: bool,
    /// Whether a new task may start here now (enabled, directory present, no conflicting
    /// inherited authentication).
    pub eligible: bool,
    pub ineligible_reason: Option<String>,
    /// Rows with the same value are one account: one set of limits, never added together.
    pub account: String,
    /// The other rows on this account, by name.
    pub same_account_as: Vec<String>,
    pub five_hour: WindowView,
    pub seven_day: WindowView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub choice: ProfileChoice,
    pub name: String,
    /// Every eligible choice on the suggested account (any of them is as good).
    pub choices: Vec<ProfileChoice>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub evaluated_at: i64,
    pub stale_after_ms: i64,
    pub rows: Vec<UsageRow>,
    pub suggestion: Option<Suggestion>,
    /// Why there is no suggestion (None when there is one).
    pub no_suggestion: Option<String>,
}

/// Claude Code's folder for a session launched with `env` and no profile: `CLAUDE_CONFIG_DIR`,
/// else `$HOME/.claude`, canonical. Only stat-like path resolution, never its contents.
pub fn default_config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let dir = match env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => crate::account_profiles::launch_home(env)?.join(".claude"),
    };
    std::fs::canonicalize(dir).ok()
}

fn find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r { r = parent[r]; }
    parent[i] = r;
    r
}

/// The rows (Default first, then every profile), each with its account's newest observation
/// per window, and a suggestion among the eligible ones.
pub fn snapshot(profiles: &[AccountProfile], observations: &[(String, Window, Observation)],
    eligible: impl Fn(Option<ProfileId>) -> Result<(), String>, default_dir: Option<&Path>, now: i64) -> UsageSnapshot {
    // Row 0 is Default. Rows on one folder, or in one group the human declared, share an account.
    let n = profiles.len() + 1;
    let mut parent: Vec<usize> = (0..n).collect();
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb { parent[ra.max(rb)] = ra.min(rb); }
    };
    for (i, p) in profiles.iter().enumerate() {
        if default_dir.is_some_and(|d| Path::new(&p.config_dir) == d) {
            union(&mut parent, 0, i + 1);
        }
        if let Some(g) = &p.account_group {
            if let Some(j) = profiles[..i].iter().position(|q| q.account_group.as_ref() == Some(g)) {
                union(&mut parent, j + 1, i + 1);
            }
        }
    }
    let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
    let name = |i: usize| if i == 0 { "Default".to_string() } else { profiles[i - 1].name.clone() };
    let key = |i: usize| if i == 0 { account_key(None) } else { account_key(Some(profiles[i - 1].id)) };
    let rank = |o: &Observation| (o.observed_at.is_some(), o.observed_at, o.reported_at);
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let members: Vec<usize> = (0..n).filter(|&j| roots[j] == roots[i]).collect();
        let view = |w: Window| {
            let best = observations.iter()
                .filter(|(k, ow, _)| *ow == w && members.iter().any(|&j| key(j) == *k))
                .max_by_key(|(_, _, o)| rank(o));
            let Some((k, _, o)) = best else { return WindowView::unknown() };
            let by = members.iter().copied().find(|&j| key(j) == *k).filter(|&j| j != i).map(name);
            WindowView { status: status(o, now), used_percentage: Some(o.reading.used_percentage),
                resets_at: Some(o.reading.resets_at), observed_at: o.observed_at, reported_at: Some(o.reported_at),
                reported_by: by }
        };
        let (choice, enabled, id) = if i == 0 {
            (ProfileChoice::pinned(None), true, None)
        } else {
            let p = &profiles[i - 1];
            (ProfileChoice::Id(p.id), p.enabled, Some(p.id))
        };
        let check = eligible(id);
        rows.push(UsageRow {
            choice, name: name(i), enabled, eligible: check.is_ok(), ineligible_reason: check.err(),
            account: format!("account-{}", roots[i]),
            same_account_as: members.iter().copied().filter(|&j| j != i).map(name).collect(),
            five_hour: view(Window::FiveHour), seven_day: view(Window::SevenDay),
        });
    }
    let (suggestion, no_suggestion) = match suggest(&rows) {
        Ok(s) => (Some(s), None),
        Err(why) => (None, Some(why)),
    };
    UsageSnapshot { evaluated_at: now, stale_after_ms: STALE_AFTER_MS, rows, suggestion, no_suggestion }
}

fn pct(v: f64) -> String {
    format!("{}%", v.round() as i64)
}

/// "A", "A and B", "A, B and C".
fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Ranking policy (docs/ACCOUNT_PROFILES.md "Choosing where a new task runs"): only eligible
/// rows; one entry per account; an account counts only when both windows are current; the one
/// whose fuller window is lowest wins, unless that is already at [`NEAR_LIMIT`]. Unknown, stale,
/// unconfirmed or reset usage never establishes that an account has more room.
pub fn suggest(rows: &[UsageRow]) -> Result<Suggestion, String> {
    let mut accounts: Vec<&str> = Vec::new();
    for r in rows.iter().filter(|r| r.eligible) {
        if !accounts.contains(&r.account.as_str()) { accounts.push(&r.account); }
    }
    if accounts.len() < 2 {
        return Err("Only one Claude account can start a task here, so there is nothing to compare.".into());
    }
    // The first eligible row of each account stands for it.
    let heads: Vec<&UsageRow> = accounts.iter()
        .filter_map(|a| rows.iter().find(|r| r.eligible && r.account == *a)).collect();
    let level = |r: &UsageRow| Some(r.five_hour.current()?.max(r.seven_day.current()?));
    let best = heads.iter().filter_map(|r| level(r).map(|l| (l, *r)))
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let Some((best_level, best)) = best else {
        return Err("No account has current usage for both its 5-hour and weekly limits. Usage appears after a session on that account gets a reply; unknown is never counted as free.".into());
    };
    if best_level >= NEAR_LIMIT {
        return Err(format!("Every account with current usage is at {} or more of a limit.", pct(NEAR_LIMIT)));
    }
    let mut reason = format!("{} used {} of its 5-hour and {} of its weekly limit.", best.name,
        pct(best.five_hour.current().unwrap_or_default()), pct(best.seven_day.current().unwrap_or_default()));
    let others = heads.iter().filter(|r| r.account != best.account);
    for r in others.clone().filter(|r| level(r).is_some()) {
        reason.push_str(&format!(" {}: {} and {}.", r.name, pct(r.five_hour.current().unwrap_or_default()),
            pct(r.seven_day.current().unwrap_or_default())));
    }
    let unknown: Vec<&str> = others.filter(|r| level(r).is_none()).map(|r| r.name.as_str()).collect();
    if !unknown.is_empty() {
        reason.push_str(&format!(" No current usage for {}, so {} not compared.", and_list(&unknown),
            if unknown.len() == 1 { "it is" } else { "they are" }));
    }
    Ok(Suggestion {
        choice: best.choice, name: best.name.clone(),
        choices: rows.iter().filter(|r| r.eligible && r.account == best.account).map(|r| r.choice).collect(),
        reason,
    })
}

// ----- composing with the user's own status line -----

/// The `statusLine` object Claude Code would use for a session launched with `env` in `cwd`,
/// were it not for Muster's own `--settings`: the first of the project's local settings, its
/// shared settings and the user's settings that sets one. Managed settings outrank `--settings`
/// and are left alone. Only the `statusLine` key is kept: the rest of each file is parsed past
/// and dropped, never stored, logged or sent.
pub fn user_status_line(env: &BTreeMap<String, String>, cwd: &Path) -> Option<Map<String, Value>> {
    let user_dir = match env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => crate::account_profiles::launch_home(env).map(|h| h.join(".claude")),
    };
    let files = [Some(cwd.join(".claude/settings.local.json")), Some(cwd.join(".claude/settings.json")),
        user_dir.map(|d| d.join("settings.json"))];
    files.into_iter().flatten().find_map(|f| status_line_of(&f))
}

fn status_line_of(file: &Path) -> Option<Map<String, Value>> {
    #[derive(Deserialize)]
    struct OnlyStatusLine {
        #[serde(rename = "statusLine")]
        status_line: Option<Value>,
    }
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(file).ok()?.take(1 << 20).read_to_string(&mut text).ok()?;
    match serde_json::from_str::<OnlyStatusLine>(&text).ok()?.status_line? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// The status-line command for a session: Muster's reporter, which runs `inner` (the user's own
/// command) with the same input and prints exactly its output. If the Muster binary is gone
/// (an old run after an update), the user's command runs directly instead.
pub fn reporter_command(muster_bin: &Path, inner: Option<&str>) -> String {
    use crate::adapters::claude::sh_quote;
    let bin = sh_quote(&muster_bin.to_string_lossy());
    match inner.filter(|c| !c.trim().is_empty()) {
        Some(cmd) => {
            let cmd = sh_quote(cmd);
            format!("if [ -x {bin} ]; then exec {bin} statusline claude {cmd}; else exec /bin/sh -c {cmd}; fi")
        }
        None => format!("[ -x {bin} ] && exec {bin} statusline claude; exit 0"),
    }
}

/// Add the reporter to the session's own Claude settings file (written by the adapter), keeping
/// every option of the user's status line (padding, refresh interval, ...) as it was. Never fails
/// a launch: usage reporting is optional, so an error is only logged.
pub fn install_reporter(settings: &Path, muster_bin: &Path, env: &BTreeMap<String, String>, cwd: &Path) {
    if std::env::var("MUSTER_USAGE_REPORTER").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no")) {
        return;
    }
    let result = (|| -> anyhow::Result<()> {
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(settings)?)?;
        let user = user_status_line(env, cwd);
        let inner = user.as_ref()
            .filter(|o| o.get("type").and_then(Value::as_str).is_none_or(|t| t == "command"))
            .and_then(|o| o.get("command")).and_then(Value::as_str).map(str::to_string);
        if user.is_some() && inner.is_none() {
            // A kind of status line Muster can't run itself: leave it showing, report nothing.
            return Ok(());
        }
        let mut line = user.unwrap_or_default();
        line.insert("type".into(), json!("command"));
        line.insert("command".into(), json!(reporter_command(muster_bin, inner.as_deref())));
        doc.as_object_mut().context("session settings are not a JSON object")?
            .insert("statusLine".into(), Value::Object(line));
        crate::adapters::claude::write_private(settings, &serde_json::to_string_pretty(&doc)?)
    })();
    if let Err(e) = result {
        tracing::warn!(settings = %settings.display(), "usage reporter not installed: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const RESET: i64 = NOW / 1000 + 4 * 3600;

    fn report(five: Option<f64>, seven: Option<f64>, api: Option<f64>) -> Report {
        let mut windows = BTreeMap::new();
        if let Some(v) = five { windows.insert(Window::FiveHour, Reading { used_percentage: v, resets_at: RESET }); }
        if let Some(v) = seven { windows.insert(Window::SevenDay, Reading { used_percentage: v, resets_at: RESET + 86_400 }); }
        Report { windows, api_ms: api }
    }

    /// Feed reports through `apply` like the store does.
    struct Account { stored: BTreeMap<Window, Observation>, runs: BTreeMap<SessionId, RunMemory> }
    impl Account {
        fn new() -> Self { Account { stored: BTreeMap::new(), runs: BTreeMap::new() } }
        fn feed(&mut self, run: RunKey, r: &Report, now: i64) -> bool {
            let a = apply(self.runs.get(&run.0), &self.stored, run, r, now);
            if let Some(m) = a.run { self.runs.insert(run.0, m); }
            for (w, o) in a.updates { self.stored.insert(w, o); }
            a.notify
        }
    }

    #[test]
    fn parsing_takes_only_plausible_documented_fields() {
        let body = json!({"rate_limits": {
            "five_hour": {"used_percentage": 23.5, "resets_at": RESET},
            "seven_day": {"used_percentage": 141.0, "resets_at": RESET},
            "spend_limit": {"used_percentage": 10, "resets_at": RESET}}, "api_duration_ms": 1200});
        let r = parse_report(&body, NOW);
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[&Window::FiveHour], Reading { used_percentage: 23.5, resets_at: RESET });
        assert_eq!(r.api_ms, Some(1200.0));
        for bad in [json!({}), json!("x"), json!({"rate_limits": {"five_hour": {"used_percentage": 5}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": "5", "resets_at": RESET}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": 12}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": -1, "resets_at": RESET}}})] {
            assert!(parse_report(&bad, NOW).windows.is_empty(), "{bad}");
        }
    }

    #[test]
    fn a_runs_first_report_is_fresh_and_redraws_do_not_renew_it() {
        let mut a = Account::new();
        assert!(a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(500.0)), NOW));
        assert_eq!(a.stored[&Window::FiveHour].observed_at, Some(NOW));
        // A timer redraw an hour later: same numbers, no new API time.
        let later = NOW + 60 * 60 * 1000;
        assert!(!a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(500.0)), later));
        let o = &a.stored[&Window::FiveHour];
        assert_eq!(o.observed_at, Some(NOW), "a redraw must not renew freshness");
        assert_eq!(o.reported_at, later);
        assert_eq!(status(o, later), Status::Stale);
        // New API time: newly fetched, even though the numbers are unchanged.
        assert!(a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(900.0)), later + 1));
        assert_eq!(a.stored[&Window::FiveHour].observed_at, Some(later + 1));
    }

    #[test]
    fn an_idle_sessions_redraw_never_replaces_a_newer_observation() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0)), NOW);
        a.feed((2, 1), &report(Some(60.0), Some(50.0), Some(100.0)), NOW + 1000);
        assert_eq!(a.stored[&Window::FiveHour].session_id, 2);
        // Session 1 redraws its old numbers: they stay ignored.
        assert!(!a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0)), NOW + 5000));
        assert_eq!(a.stored[&Window::FiveHour].reading.used_percentage, 60.0);
        assert_eq!(a.stored[&Window::FiveHour].reported_at, NOW + 1000);
        // Session 1 gets a reply: its newer numbers win.
        assert!(a.feed((1, 1), &report(Some(65.0), Some(51.0), Some(400.0)), NOW + 9000));
        assert_eq!(a.stored[&Window::FiveHour].session_id, 1);
    }

    #[test]
    fn partial_reports_keep_each_windows_own_time() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0)), NOW);
        a.feed((1, 1), &report(Some(15.0), None, Some(200.0)), NOW + 60_000);
        assert_eq!(a.stored[&Window::FiveHour].observed_at, Some(NOW + 60_000));
        assert_eq!(a.stored[&Window::SevenDay].observed_at, Some(NOW), "an absent window keeps its own time");
        assert_eq!(a.stored[&Window::SevenDay].reading.used_percentage, 30.0);
    }

    #[test]
    fn a_new_run_starts_fresh_and_a_seen_redraw_is_unconfirmed_without_evidence() {
        let mut a = Account::new();
        a.runs.insert(1, RunMemory { generation: 1, api_ms: Some(5.0),
            last: BTreeMap::from([(Window::FiveHour, Reading { used_percentage: 70.0, resets_at: RESET })]) });
        // Same run, same numbers, but nothing stored yet (e.g. cleared by a login change).
        assert!(a.feed((1, 1), &report(Some(70.0), None, Some(5.0)), NOW));
        assert_eq!(a.stored[&Window::FiveHour].observed_at, None);
        assert_eq!(status(&a.stored[&Window::FiveHour], NOW), Status::Unconfirmed);
        // The session's next run (a restart) counts as new.
        assert!(a.feed((1, 2), &report(Some(70.0), None, Some(5.0)), NOW + 10));
        assert_eq!(a.stored[&Window::FiveHour].observed_at, Some(NOW + 10));
    }

    #[test]
    fn duplicates_are_coalesced() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0)), NOW);
        // A fresh report with the same numbers within the renew interval writes nothing.
        let r = apply(a.runs.get(&1), &a.stored, (1, 1), &report(Some(10.0), Some(30.0), Some(150.0)), NOW + 1000);
        assert!(r.updates.is_empty() && !r.notify);
        assert_eq!(r.run.unwrap().api_ms, Some(150.0));
        let r = apply(a.runs.get(&1), &a.stored, (1, 1), &report(Some(10.0), Some(30.0), Some(100.0)), NOW + 1000);
        assert_eq!(r, Applied::default());
    }

    #[test]
    fn statuses_never_treat_old_data_as_current() {
        let o = Observation { reading: Reading { used_percentage: 50.0, resets_at: RESET }, observed_at: Some(NOW),
            reported_at: NOW, session_id: 1, generation: 1 };
        assert_eq!(status(&o, NOW + 1000), Status::Current);
        assert_eq!(status(&o, NOW + STALE_AFTER_MS + 1), Status::Stale);
        assert_eq!(status(&o, RESET * 1000), Status::Reset, "a passed reset is unknown, not zero");
        assert_eq!(status(&Observation { observed_at: None, ..o }, NOW), Status::Unconfirmed);
    }

    fn profile(id: i64, name: &str, dir: &str, group: Option<&str>) -> AccountProfile {
        AccountProfile { id, name: name.into(), config_dir: dir.into(), enabled: true, created_at: 0,
            account_group: group.map(str::to_string) }
    }

    fn obs(five: f64, seven: f64, at: i64, sid: SessionId) -> [(Window, Observation); 2] {
        let o = |v, r| Observation { reading: Reading { used_percentage: v, resets_at: r }, observed_at: Some(at),
            reported_at: at, session_id: sid, generation: 1 };
        [(Window::FiveHour, o(five, RESET)), (Window::SevenDay, o(seven, RESET + 86_400))]
    }

    fn with_key(key: &str, list: [(Window, Observation); 2]) -> Vec<(String, Window, Observation)> {
        list.into_iter().map(|(w, o)| (key.to_string(), w, o)).collect()
    }

    #[test]
    fn suggestion_needs_current_usage_and_never_counts_unknown_as_room() {
        let profiles = [profile(1, "Personal", "/p", None), profile(2, "Work", "/w", None)];
        let ok = |_: Option<ProfileId>| Ok(());
        let mut o = with_key("profile:1", obs(92.0, 70.0, NOW - 60_000, 1));
        o.extend(with_key("profile:2", obs(25.0, 40.0, NOW - 240_000, 2)));
        let s = snapshot(&profiles, &o, ok, None, NOW);
        assert_eq!(s.rows[0].name, "Default");
        assert_eq!(s.rows[0].five_hour.status, Status::Unknown);
        let sug = s.suggestion.unwrap();
        assert_eq!(sug.choice, ProfileChoice::Id(2));
        assert!(sug.reason.contains("Work used 25% of its 5-hour and 40% of its weekly limit"), "{}", sug.reason);
        assert!(sug.reason.contains(" Personal: 92% and 70%."), "{}", sug.reason);
        assert!(sug.reason.ends_with(" No current usage for Default, so it is not compared."), "{}", sug.reason);
        // Work's numbers went stale; Personal's are current but near its limit.
        let s = snapshot(&profiles, &o, ok, None, NOW + STALE_AFTER_MS - 100_000);
        assert_eq!(s.rows[2].five_hour.status, Status::Stale);
        assert_eq!(s.rows[1].five_hour.status, Status::Current);
        assert!(s.suggestion.is_none());
        assert!(s.no_suggestion.unwrap().contains("90%"));
        // Only one window known for Work: no claim of more room.
        let partial: Vec<_> = o.iter().filter(|(k, w, _)| k != "profile:2" || *w == Window::FiveHour).cloned().collect();
        let s = snapshot(&profiles, &partial, ok, None, NOW);
        assert!(s.suggestion.is_none(), "{:?}", s.suggestion);
        // Nothing known at all.
        let s = snapshot(&profiles, &[], ok, None, NOW);
        assert!(s.no_suggestion.unwrap().contains("unknown is never counted as free"));
    }

    #[test]
    fn shared_accounts_count_once_and_ineligible_rows_are_skipped() {
        let dir = std::env::temp_dir().join(format!("muster-usage-{}", crate::paths::random_token()));
        std::fs::create_dir(&dir).unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let profiles = [profile(1, "Main", canonical.to_str().unwrap(), None), profile(2, "Work", "/w", Some("g")),
            profile(3, "Work laptop", "/w2", Some("g"))];
        // Default and Main share a folder; Work and Work laptop were declared one account.
        let mut o = with_key("default", obs(50.0, 50.0, NOW - 1000, 1));
        o.extend(with_key("profile:3", obs(10.0, 20.0, NOW - 2000, 3)));
        let s = snapshot(&profiles, &o, |_| Ok(()), Some(&canonical), NOW);
        assert_eq!(s.rows[1].account, s.rows[0].account);
        assert_eq!(s.rows[1].five_hour.used_percentage, Some(50.0));
        assert_eq!(s.rows[1].five_hour.reported_by.as_deref(), Some("Default"));
        assert_eq!(s.rows[2].five_hour.used_percentage, Some(10.0));
        assert_eq!(s.rows[2].same_account_as, vec!["Work laptop".to_string()]);
        let sug = s.suggestion.unwrap();
        assert_eq!(sug.choice, ProfileChoice::Id(2));
        assert_eq!(sug.choices, vec![ProfileChoice::Id(2), ProfileChoice::Id(3)]);
        // Work's account can't start tasks: the shared Default/Main account is the only one left.
        let s = snapshot(&profiles, &o, |id| if matches!(id, Some(2 | 3)) { Err("disabled".into()) } else { Ok(()) },
            Some(&canonical), NOW);
        assert!(s.no_suggestion.unwrap().contains("Only one Claude account"));
        assert_eq!(s.rows[2].ineligible_reason.as_deref(), Some("disabled"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reporter_composes_with_the_users_status_line() {
        let base = std::env::temp_dir().join(format!("muster-usage-sl-{}", crate::paths::random_token()));
        let (home, proj) = (base.join("home"), base.join("proj"));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        let env = BTreeMap::from([("HOME".to_string(), home.to_string_lossy().to_string())]);
        assert_eq!(user_status_line(&env, &proj), None);
        std::fs::write(home.join(".claude/settings.json"),
            r#"{"env": {"SECRET": "x"}, "statusLine": {"type": "command", "command": "bash ~/sl.sh", "padding": 2, "refreshInterval": 5}}"#).unwrap();
        let user = user_status_line(&env, &proj).unwrap();
        assert_eq!(user["command"], "bash ~/sl.sh");
        // A profile folder replaces ~/.claude; project settings outrank both.
        let profile = base.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("settings.json"), r#"{"statusLine": {"type": "command", "command": "work-line"}}"#).unwrap();
        let mut penv = env.clone();
        penv.insert("CLAUDE_CONFIG_DIR".into(), profile.to_string_lossy().to_string());
        assert_eq!(user_status_line(&penv, &proj).unwrap()["command"], "work-line");
        std::fs::write(proj.join(".claude/settings.json"), r#"{"statusLine": {"type": "command", "command": "proj-line"}}"#).unwrap();
        std::fs::write(proj.join(".claude/settings.local.json"), "not json").unwrap();
        assert_eq!(user_status_line(&penv, &proj).unwrap()["command"], "proj-line");
        std::fs::remove_file(proj.join(".claude/settings.json")).unwrap();

        let settings = base.join("claude-settings.json");
        std::fs::write(&settings, r#"{"hooks": {}}"#).unwrap();
        let bin = Path::new("/opt/my muster/muster");
        install_reporter(&settings, bin, &env, &proj);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(doc["hooks"], json!({}));
        assert_eq!(doc["statusLine"]["padding"], 2);
        assert_eq!(doc["statusLine"]["refreshInterval"], 5);
        assert_eq!(doc["statusLine"]["command"],
            "if [ -x '/opt/my muster/muster' ]; then exec '/opt/my muster/muster' statusline claude 'bash ~/sl.sh'; else exec /bin/sh -c 'bash ~/sl.sh'; fi");
        assert!(!std::fs::read_to_string(&settings).unwrap().contains("SECRET"));
        // No status line of the user's own: the reporter alone, which prints nothing.
        std::fs::remove_file(home.join(".claude/settings.json")).unwrap();
        install_reporter(&settings, bin, &env, &proj);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(doc["statusLine"], json!({"type": "command",
            "command": "[ -x '/opt/my muster/muster' ] && exec '/opt/my muster/muster' statusline claude; exit 0"}));
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The composed command runs the user's line even when the Muster binary is missing.
    #[test]
    fn a_missing_reporter_binary_falls_back_to_the_users_command() {
        let cmd = reporter_command(Path::new("/nonexistent/muster"), Some("cat; echo ' line'"));
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&cmd)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .spawn().and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(b"input")?;
                c.wait_with_output()
            }).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "input line\n");
        let none = reporter_command(Path::new("/nonexistent/muster"), None);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&none).output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty());
    }
}
