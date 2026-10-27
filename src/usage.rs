//! Claude subscription usage, as Claude Code reports it to its status line
//! (docs/ACCOUNT_PROFILES.md "Usage observations from Claude Code" and "Choosing where a new task
//! runs").
//!
//! - Source: the documented status-line JSON, `rate_limits.five_hour` / `rate_limits.seven_day`
//!   (`used_percentage` 0-100, `resets_at` Unix seconds). The session's status-line command runs
//!   the user's own status line itself and hands a copy of the JSON to Muster's reporter
//!   (`muster statusline`, src/statusline.rs), which forwards only those numbers, authenticated by
//!   the run's session token.
//! - Attribution: the daemon maps the token to its session and run (generation); the run's launch
//!   record names the configuration folder it really used. Observations are filed per folder (one
//!   folder, one login). Nothing in the payload names an account or a folder.
//! - Freshness: Claude Code re-runs the status line for reasons other than new provider data (a
//!   timer, a mode change, a reset passing). A report is newly fetched data only when something
//!   changed for its run (its first report, more API time, other numbers) and it carries the time
//!   Claude Code last wrote the conversation; it is dated by that time, not by its arrival.
//!   Anything else never replaces or renews another observation.
//! - Unknown stays unknown: no observation, a stale one, an unconfirmed one or one whose reset has
//!   passed never counts as headroom. Nothing here probes an account or reads a credential; the
//!   only file contents read are settings files, without following links (see `read_beneath`).

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
    /// Unix ms when Claude Code last wrote the conversation (its transcript's modification
    /// time, read by the reporter): no earlier than the API response these numbers came with.
    /// The only evidence of when they were fetched; None: unknown.
    pub data_at: Option<i64>,
}

/// Validate a reporter body (`{"rate_limits": {...}, "api_duration_ms": n, "data_at": ms}`).
/// Each window is taken only when both of its numbers are plausible; anything else is dropped,
/// not guessed. A `data_at` in the future (beyond clock skew) or more than 8 days old is dropped.
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
    let data_at = body.get("data_at").and_then(Value::as_f64)
        .filter(|t| t.is_finite() && *t > (now_ms - 8 * DAY_S * 1000) as f64 && *t <= (now_ms + 5_000) as f64)
        .map(|t| (t as i64).min(now_ms));
    Report { windows, api_ms, data_at }
}

/// The run (session and process generation) a report came from.
pub type RunKey = (SessionId, u64);

/// The newest usable observation of one window for one Claude configuration folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(flatten)]
    pub reading: Reading,
    /// Unix ms when these numbers were newly fetched, as far as the evidence shows. None:
    /// reported, but when they were fetched was never established.
    pub observed_at: Option<i64>,
    /// Unix ms of the newest callback that carried these numbers.
    pub reported_at: i64,
    pub session_id: SessionId,
    pub generation: u64,
}

/// One run of a Claude session: the configuration folder it was launched with (recorded at
/// launch, so reports are filed under the folder that run really used) and what it last
/// reported, to tell new provider data from a redraw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMemory {
    pub generation: u64,
    /// Canonical; None when it could not be resolved at launch (its reports are then ignored).
    #[serde(default)]
    pub config_dir: Option<String>,
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

/// Merge one report from run `run` (whose launch record is `memory`) into the stored
/// observations of its folder.
///
/// A window's report is newly fetched data only with both kinds of evidence: something changed
/// for this run (its first report of the window, more API time, or different numbers) and the
/// reporter's `data_at` says when. It is then dated `data_at`, not when it arrived, so a report
/// delivered late (a lost callback, then a redraw an hour later) is as old as its data, and it
/// replaces an observation only if not older than it. Without that evidence a report is kept
/// only where nothing better exists, as "not confirmed fresh".
pub fn apply(memory: &RunMemory, stored: &BTreeMap<Window, Observation>, run: RunKey, report: &Report, now: i64)
    -> Applied {
    let mut next = memory.clone();
    let api_advanced = matches!((memory.api_ms, report.api_ms), (Some(a), Some(b)) if b > a);
    let mut out = Applied::default();
    for (&w, &reading) in &report.windows {
        let last = memory.last.get(&w);
        let changed = last.is_none() || api_advanced || last != Some(&reading);
        let mine = |o: &Observation| (o.session_id, o.generation) == run;
        let fetched = report.data_at.filter(|_| changed);
        let observation = |observed_at| Observation { reading, observed_at, reported_at: now, session_id: run.0, generation: run.1 };
        match (stored.get(&w), fetched) {
            // Coalesce: the same numbers from the same run, dated within the renew interval.
            (Some(o), Some(at)) if mine(o) && o.reading == reading
                && o.observed_at.is_some_and(|t| (at - t).abs() < RENEW_EVERY_MS) => {}
            (Some(o), Some(at)) if o.observed_at.is_some_and(|t| t > at) => {}
            (_, Some(at)) => {
                out.updates.push((w, observation(Some(at))));
                out.notify = true;
            }
            (None, None) => {
                out.updates.push((w, observation(None)));
                out.notify = true;
            }
            // This run's own newer numbers, without a date: newer than what it said before.
            (Some(o), None) if mine(o) && o.reading != reading && changed => {
                out.updates.push((w, observation(None)));
                out.notify = true;
            }
            (Some(o), None) if mine(o) && o.reading == reading && now - o.reported_at >= REPORTED_EVERY_MS => {
                out.updates.push((w, Observation { reported_at: now, ..o.clone() }));
            }
            // A redraw never replaces or renews another run's (or a newer) observation.
            (Some(_), None) => {}
        }
        next.last.insert(w, reading);
    }
    if report.api_ms.is_some() {
        next.api_ms = report.api_ms;
    }
    if next != *memory {
        out.run = Some(next);
    }
    out
}

/// Observations are filed under the configuration folder the reporting run used: one folder,
/// one login, whichever profile (or Default) reached it.
pub fn folder_key(config_dir: &str) -> String {
    format!("dir:{config_dir}")
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
    /// The profile whose folder it was reported for, when that is another profile on the same
    /// account.
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

/// The configuration folder Claude Code uses for a session launched with `env`:
/// `CLAUDE_CONFIG_DIR`, else `$HOME/.claude`, canonical. Path resolution only, never contents.
/// With the daemon's environment it is the folder of Default.
pub fn effective_config_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
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
/// per window, and a suggestion among the eligible ones. `default_dir` is the folder new
/// Default sessions would use now; observations filed under a folder no row uses are not shown.
pub fn snapshot(profiles: &[AccountProfile], observations: &[(String, Window, Observation)],
    eligible: impl Fn(Option<ProfileId>) -> Result<(), String>, default_dir: Option<&Path>, now: i64) -> UsageSnapshot {
    // Row 0 is Default. Rows on one folder, or in one group the human declared, share an account.
    let n = profiles.len() + 1;
    let dirs: Vec<Option<String>> = std::iter::once(default_dir.map(|d| d.to_string_lossy().into_owned()))
        .chain(profiles.iter().map(|p| Some(p.config_dir.clone()))).collect();
    let mut parent: Vec<usize> = (0..n).collect();
    let union = |parent: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb { parent[ra.max(rb)] = ra.min(rb); }
    };
    for i in 1..n {
        if let Some(j) = (0..i).find(|&j| dirs[j].is_some() && dirs[j] == dirs[i]) {
            union(&mut parent, j, i);
        }
        if let Some(g) = &profiles[i - 1].account_group
            && let Some(j) = profiles[..i - 1].iter().position(|q| q.account_group.as_ref() == Some(g))
        {
            union(&mut parent, j + 1, i);
        }
    }
    let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
    let name = |i: usize| if i == 0 { "Default".to_string() } else { profiles[i - 1].name.clone() };
    let key = |i: usize| dirs[i].as_deref().map(folder_key);
    let rank = |o: &Observation| (o.observed_at.is_some(), o.observed_at, o.reported_at);
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let members: Vec<usize> = (0..n).filter(|&j| roots[j] == roots[i]).collect();
        let view = |w: Window| {
            let best = observations.iter()
                .filter(|(k, ow, _)| *ow == w && members.iter().any(|&j| key(j).as_deref() == Some(k.as_str())))
                .max_by_key(|(_, _, o)| rank(o));
            let Some((k, _, o)) = best else { return WindowView::unknown() };
            let by = (key(i).as_deref() != Some(k.as_str()))
                .then(|| members.iter().copied().find(|&j| key(j).as_deref() == Some(k.as_str())).map(name))
                .flatten();
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

/// The largest settings file read (Claude Code's own are a few KiB).
const MAX_SETTINGS: u64 = 1 << 20;

/// Read `rel` (plain names, no separators) beneath the canonical folder `base` without following
/// a link anywhere below `base`: each folder and the file are opened with `O_NOFOLLOW`, the file
/// must be a regular file with a single link and at most `max` bytes. `Ok(None)`: it does not
/// exist. `Err`: it exists but could not be read safely (a link, a special file, too large ...):
/// the caller must not guess what it says. A name that looks like a credentials file is refused
/// outright, whatever it points at.
fn read_beneath(base: &Path, rel: &[&str], max: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::{ErrorKind, Read};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let shown = || rel.iter().fold(base.to_path_buf(), |p, c| p.join(c)).display().to_string();
    if !base.is_absolute()
        || rel.is_empty()
        || rel.iter().any(|c| c.is_empty() || *c == "." || *c == ".." || c.contains('/') || c.contains('\0')
            || c.to_ascii_lowercase().contains("credential"))
    {
        return Err(format!("refusing to read {}", shown()));
    }
    let at = |dir: libc::c_int, name: &[u8], flags: libc::c_int| -> std::io::Result<OwnedFd> {
        let c = std::ffi::CString::new(name).map_err(|_| std::io::Error::from(ErrorKind::InvalidInput))?;
        // SAFETY: a valid NUL-terminated path and flags; the result is checked before use.
        let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };
    let fail = |e: std::io::Error| -> Result<Option<Vec<u8>>, String> {
        match e.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => Err(format!("{} is or passes through a link", shown())),
            _ => Err(format!("{}: {e}", shown())),
        }
    };
    let mut dir = match at(libc::AT_FDCWD, base.as_os_str().as_bytes(), libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    let (last, folders) = rel.split_last().expect("not empty");
    for folder in folders {
        dir = match at(dir.as_raw_fd(), folder.as_bytes(), libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
    }
    // Non-blocking, so a FIFO planted under the name can't hang the launch.
    let file = match at(dir.as_raw_fd(), last.as_bytes(), libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(f) => std::fs::File::from(f),
        Err(e) => return fail(e),
    };
    let meta = file.metadata().map_err(|e| format!("{}: {e}", shown()))?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > max {
        return Err(format!("{} is not a plain file of at most {max} bytes", shown()));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).map_err(|e| format!("{}: {e}", shown()))?;
    if bytes.len() as u64 > max {
        return Err(format!("{} grew past {max} bytes", shown()));
    }
    Ok(Some(bytes))
}

/// What one settings file says about the status line. `Err`: it exists but could not be read
/// safely or parsed, so what Claude Code will show is unknown.
fn status_line_in(base: &Path, rel: &[&str]) -> Result<Option<Map<String, Value>>, String> {
    #[derive(Deserialize)]
    struct OnlyStatusLine {
        #[serde(rename = "statusLine")]
        status_line: Option<Value>,
    }
    let Some(bytes) = read_beneath(base, rel, MAX_SETTINGS)? else { return Ok(None) };
    let parsed: OnlyStatusLine = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not valid settings JSON: {e}", base.join(rel.join("/")).display()))?;
    match parsed.status_line {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o)),
        Some(_) => Err(format!("{}: statusLine is not an object", base.join(rel.join("/")).display())),
    }
}

/// Where Claude Code keeps the project's local settings for a session started in `cwd`
/// (docs: settings, "Where Claude Code keeps the local file in a git repository", read
/// 2026-10-26): the repository root, or for a linked worktree the main checkout's root; but
/// `cwd` itself outside a git repository, when that root is the home folder, or when the root,
/// its `.git` or its `.claude` is not owned by this user. Found by reading `.git` entries, never
/// by running git (a repository's own git config could run commands). `Err`: unsure.
fn local_settings_root(cwd: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    let Some((top, git)) = cwd.ancestors().find_map(|d| std::fs::symlink_metadata(d.join(".git")).ok().map(|m| (d, m)))
    else {
        return Ok(cwd.to_path_buf());
    };
    let root = if git.is_dir() {
        top.to_path_buf()
    } else if git.is_file() {
        // A linked worktree (or a submodule): "gitdir: <path>"; a worktree's git folder names
        // the shared one in `commondir`.
        let text = read_beneath(top, &[".git"], 4096)?.unwrap_or_default();
        let text = String::from_utf8_lossy(&text);
        let gitdir = text.trim().strip_prefix("gitdir:").map(str::trim).filter(|g| !g.is_empty())
            .ok_or_else(|| format!("{} is not a gitdir file", top.join(".git").display()))?;
        let gitdir = std::fs::canonicalize(top.join(gitdir)).map_err(|e| format!("{gitdir}: {e}"))?;
        match read_beneath(&gitdir, &["commondir"], 4096)? {
            None => top.to_path_buf(),
            Some(common) => {
                let common = String::from_utf8_lossy(&common);
                let common = std::fs::canonicalize(gitdir.join(common.trim())).map_err(|e| format!("commondir: {e}"))?;
                // A worktree of a bare repository has no main checkout: unsure.
                if common.file_name() != Some(std::ffi::OsStr::new(".git")) {
                    return Err(format!("{} belongs to a repository without a main checkout", cwd.display()));
                }
                common.parent().map(Path::to_path_buf).ok_or("no main checkout")?
            }
        }
    } else {
        return Err(format!("{} is neither a folder nor a file", top.join(".git").display()));
    };
    let root = std::fs::canonicalize(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let owned = |p: &Path, required: bool| match std::fs::symlink_metadata(p) {
        Ok(m) => m.uid() == me,
        Err(_) => !required,
    };
    if home.is_some_and(|h| h == root) || !owned(&root, true) || !owned(&root.join(".git"), true)
        || !owned(&root.join(".claude"), false)
    {
        return Ok(cwd.to_path_buf());
    }
    Ok(root)
}

/// The `statusLine` object Claude Code would use for a session launched with `env` in `cwd`,
/// were it not for Muster's own `--settings`, in Claude Code's precedence (docs: settings,
/// "Settings precedence", read 2026-10-26): project local settings (at the repository root,
/// then a file an older version left in `cwd`), shared project settings in `cwd`, then the
/// user settings of the session's configuration folder. Managed settings outrank `--settings`
/// and are left alone. Only the `statusLine` key is kept: the rest of each file is parsed past
/// and dropped, never stored, logged or sent. Nothing is read through a link (see
/// [`read_beneath`]). `Ok(None)`: no status line of the user's own. `Err`: unsure, so Muster
/// must leave the session's status line alone.
pub fn user_status_line(env: &BTreeMap<String, String>, cwd: &Path) -> Result<Option<Map<String, Value>>, String> {
    let cwd = std::fs::canonicalize(cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
    let home = crate::account_profiles::launch_home(env).and_then(|h| std::fs::canonicalize(h).ok());
    let local_root = local_settings_root(&cwd, home.as_deref())?;
    let mut files: Vec<(PathBuf, [&str; 2])> = vec![(local_root.clone(), [".claude", "settings.local.json"])];
    if local_root != cwd {
        files.push((cwd.clone(), [".claude", "settings.local.json"]));
    }
    files.push((cwd.clone(), [".claude", "settings.json"]));
    for (base, rel) in &files {
        if let Some(line) = status_line_in(base, rel)? {
            return Ok(Some(line));
        }
    }
    // A configuration folder that doesn't exist has no settings for Claude Code either.
    match effective_config_dir(env) {
        Some(dir) => status_line_in(&dir, &["settings.json"]),
        None => Ok(None),
    }
}

/// The status-line command for a session. The shell runs the user's own command itself, with
/// the same input, as the last step (`exec`, so its output and exit code are Claude Code's), and
/// hands a copy of the input to Muster's reporter in the background with its output discarded.
/// Whatever happens to the reporter (an older Muster binary that has no `statusline`, a missing
/// binary, a crash, a slow backend), the user's line is unaffected. Without a user command
/// only the reporter runs, and the line is empty.
pub fn reporter_command(muster_bin: &Path, inner: Option<&str>) -> String {
    use crate::adapters::claude::sh_quote;
    let bin = sh_quote(&muster_bin.to_string_lossy());
    match inner.filter(|c| !c.trim().is_empty()) {
        // The here-document's body is the literal `$input`, expanded once: a payload line can't
        // end it early or be run.
        Some(cmd) => format!(
            "input=$(cat)\nprintf '%s\\n' \"$input\" | {bin} statusline claude >/dev/null 2>&1 &\n\
             exec /bin/sh -c {} <<MUSTER_STATUS_INPUT\n$input\nMUSTER_STATUS_INPUT\n",
            sh_quote(cmd)
        ),
        None => format!("{bin} statusline claude >/dev/null 2>&1; exit 0"),
    }
}

/// Add the reporter to the session's own Claude settings file (written by the adapter), keeping
/// every option of the user's status line (padding, refresh interval, ...) as it was. When the
/// user's status line can't be determined safely, or is not a command, the session's status
/// line is left entirely to Claude Code (no usage from it). Never fails a launch: an error is
/// only logged.
pub fn install_reporter(settings: &Path, muster_bin: &Path, env: &BTreeMap<String, String>, cwd: &Path) {
    if std::env::var("MUSTER_USAGE_REPORTER").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no")) {
        return;
    }
    let result = (|| -> anyhow::Result<()> {
        let user = match user_status_line(env, cwd) {
            Ok(user) => user,
            Err(why) => {
                tracing::info!("leaving the status line to Claude Code: {why}");
                return Ok(());
            }
        };
        let inner = user.as_ref()
            .filter(|o| o.get("type").and_then(Value::as_str).is_none_or(|t| t == "command"))
            .and_then(|o| o.get("command")).and_then(Value::as_str).map(str::to_string);
        if user.is_some() && inner.is_none() {
            // A kind of status line Muster can't run itself: leave it showing, report nothing.
            return Ok(());
        }
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(settings)?)?;
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
    const HOUR: i64 = 60 * 60 * 1000;

    fn report(five: Option<f64>, seven: Option<f64>, api: Option<f64>, data_at: Option<i64>) -> Report {
        let mut windows = BTreeMap::new();
        if let Some(v) = five { windows.insert(Window::FiveHour, Reading { used_percentage: v, resets_at: RESET }); }
        if let Some(v) = seven { windows.insert(Window::SevenDay, Reading { used_percentage: v, resets_at: RESET + 86_400 }); }
        Report { windows, api_ms: api, data_at }
    }

    /// Feed reports through `apply` like the store does (each run's launch record first).
    struct Account { stored: BTreeMap<Window, Observation>, runs: BTreeMap<SessionId, RunMemory> }
    impl Account {
        fn new() -> Self { Account { stored: BTreeMap::new(), runs: BTreeMap::new() } }
        fn feed(&mut self, run: RunKey, r: &Report, now: i64) -> bool {
            let memory = self.runs.entry(run.0).or_default();
            if memory.generation != run.1 {
                *memory = RunMemory { generation: run.1, config_dir: Some("/c".into()), ..Default::default() };
            }
            let a = apply(memory, &self.stored, run, r, now);
            if let Some(m) = a.run { self.runs.insert(run.0, m); }
            for (w, o) in a.updates { self.stored.insert(w, o); }
            a.notify
        }
        fn five(&self) -> &Observation { &self.stored[&Window::FiveHour] }
    }

    #[test]
    fn parsing_takes_only_plausible_documented_fields() {
        let body = json!({"rate_limits": {
            "five_hour": {"used_percentage": 23.5, "resets_at": RESET},
            "seven_day": {"used_percentage": 141.0, "resets_at": RESET},
            "spend_limit": {"used_percentage": 10, "resets_at": RESET}}, "api_duration_ms": 1200, "data_at": NOW - 5});
        let r = parse_report(&body, NOW);
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[&Window::FiveHour], Reading { used_percentage: 23.5, resets_at: RESET });
        assert_eq!((r.api_ms, r.data_at), (Some(1200.0), Some(NOW - 5)));
        for bad in [json!({}), json!("x"), json!({"rate_limits": {"five_hour": {"used_percentage": 5}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": "5", "resets_at": RESET}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": 12}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": -1, "resets_at": RESET}}})] {
            assert!(parse_report(&bad, NOW).windows.is_empty(), "{bad}");
        }
        // A data time from the future (beyond skew) or long ago is no evidence; slight skew is clamped.
        for (at, want) in [(NOW + 60_000, None), (NOW - 9 * 24 * HOUR, None), (NOW + 2000, Some(NOW))] {
            assert_eq!(parse_report(&json!({"data_at": at}), NOW).data_at, want, "{at}");
        }
    }

    #[test]
    fn fresh_reports_are_dated_by_their_data_and_redraws_do_not_renew_them() {
        let mut a = Account::new();
        assert!(a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(500.0), Some(NOW - 2000)), NOW));
        assert_eq!(a.five().observed_at, Some(NOW - 2000), "dated by the data, not by arrival");
        // A timer redraw an hour later: same numbers, no new API time, though the transcript moved.
        let later = NOW + HOUR;
        assert!(!a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(500.0), Some(later)), later));
        assert_eq!(a.five().observed_at, Some(NOW - 2000), "a redraw must not renew freshness");
        assert_eq!(a.five().reported_at, later);
        assert_eq!(status(a.five(), later), Status::Stale);
        // New API time: newly fetched, even though the numbers are unchanged.
        assert!(a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(900.0), Some(later + 1)), later + 1));
        assert_eq!(a.five().observed_at, Some(later + 1));
    }

    /// A run's first report lost (a backend reload), then a redraw an hour later: its numbers are
    /// an hour old and must read as such, never as current.
    #[test]
    fn a_first_report_delivered_late_is_as_old_as_its_data() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(20.0), Some(40.0), Some(500.0), Some(NOW)), NOW + HOUR);
        assert_eq!(a.five().observed_at, Some(NOW));
        assert_eq!(status(a.five(), NOW + HOUR), Status::Stale);
        // Without any date, a first report is kept but never current.
        let mut b = Account::new();
        assert!(b.feed((1, 1), &report(Some(20.0), None, Some(500.0), None), NOW));
        assert_eq!(status(b.five(), NOW), Status::Unconfirmed);
        // A later dated report from the same run confirms it.
        b.feed((1, 1), &report(Some(21.0), None, Some(600.0), Some(NOW + 10)), NOW + 10);
        assert_eq!(status(b.five(), NOW + 10), Status::Current);
    }

    #[test]
    fn an_idle_sessions_report_never_replaces_a_newer_observation() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0), Some(NOW)), NOW);
        a.feed((2, 1), &report(Some(60.0), Some(50.0), Some(100.0), Some(NOW + 1000)), NOW + 1000);
        assert_eq!(a.five().session_id, 2);
        // Session 1 redraws its old numbers: they stay ignored.
        assert!(!a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0), Some(NOW + 5000)), NOW + 5000));
        assert_eq!(a.five().reading.used_percentage, 60.0);
        assert_eq!(a.five().reported_at, NOW + 1000);
        // Session 3's first report arrives now but its data is older than session 2's: kept out.
        assert!(!a.feed((3, 1), &report(Some(5.0), Some(5.0), Some(1.0), Some(NOW + 500)), NOW + 6000));
        assert_eq!(a.five().session_id, 2);
        // Session 1 gets a reply: its newer numbers win.
        assert!(a.feed((1, 1), &report(Some(65.0), Some(51.0), Some(400.0), Some(NOW + 9000)), NOW + 9000));
        assert_eq!(a.five().session_id, 1);
    }

    #[test]
    fn partial_reports_keep_each_windows_own_time() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0), Some(NOW)), NOW);
        a.feed((1, 1), &report(Some(15.0), None, Some(200.0), Some(NOW + 60_000)), NOW + 60_000);
        assert_eq!(a.five().observed_at, Some(NOW + 60_000));
        assert_eq!(a.stored[&Window::SevenDay].observed_at, Some(NOW), "an absent window keeps its own time");
        assert_eq!(a.stored[&Window::SevenDay].reading.used_percentage, 30.0);
    }

    #[test]
    fn a_seen_redraw_is_unconfirmed_and_a_new_run_starts_over() {
        let mut a = Account::new();
        a.runs.insert(1, RunMemory { generation: 1, config_dir: Some("/c".into()), api_ms: Some(5.0),
            last: BTreeMap::from([(Window::FiveHour, Reading { used_percentage: 70.0, resets_at: RESET })]) });
        // Same run, same numbers, but nothing stored (e.g. cleared by a login change).
        assert!(a.feed((1, 1), &report(Some(70.0), None, Some(5.0), Some(NOW)), NOW));
        assert_eq!(a.five().observed_at, None);
        assert_eq!(status(a.five(), NOW), Status::Unconfirmed);
        // The session's next run (a restart) has its own first report.
        assert!(a.feed((1, 2), &report(Some(70.0), None, Some(5.0), Some(NOW + 10)), NOW + 10));
        assert_eq!(a.five().observed_at, Some(NOW + 10));
    }

    #[test]
    fn duplicates_are_coalesced() {
        let mut a = Account::new();
        a.feed((1, 1), &report(Some(10.0), Some(30.0), Some(100.0), Some(NOW)), NOW);
        let memory = a.runs[&1].clone();
        // A fresh report with the same numbers within the renew interval writes nothing.
        let r = apply(&memory, &a.stored, (1, 1), &report(Some(10.0), Some(30.0), Some(150.0), Some(NOW + 1000)), NOW + 1000);
        assert!(r.updates.is_empty() && !r.notify);
        assert_eq!(r.run.unwrap().api_ms, Some(150.0));
        let r = apply(&memory, &a.stored, (1, 1), &report(Some(10.0), Some(30.0), Some(100.0), Some(NOW)), NOW + 1000);
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

    fn filed(dir: &str, list: [(Window, Observation); 2]) -> Vec<(String, Window, Observation)> {
        list.into_iter().map(|(w, o)| (folder_key(dir), w, o)).collect()
    }

    #[test]
    fn suggestion_needs_current_usage_and_never_counts_unknown_as_room() {
        let profiles = [profile(1, "Personal", "/p", None), profile(2, "Work", "/w", None)];
        let ok = |_: Option<ProfileId>| Ok(());
        let mut o = filed("/p", obs(92.0, 70.0, NOW - 60_000, 1));
        o.extend(filed("/w", obs(25.0, 40.0, NOW - 240_000, 2)));
        let s = snapshot(&profiles, &o, ok, Some(Path::new("/d")), NOW);
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
        let partial: Vec<_> = o.iter().filter(|(k, w, _)| k != "dir:/w" || *w == Window::FiveHour).cloned().collect();
        let s = snapshot(&profiles, &partial, ok, None, NOW);
        assert!(s.suggestion.is_none(), "{:?}", s.suggestion);
        // Nothing known at all.
        let s = snapshot(&profiles, &[], ok, None, NOW);
        assert!(s.no_suggestion.unwrap().contains("unknown is never counted as free"));
    }

    #[test]
    fn shared_accounts_count_once_and_ineligible_rows_are_skipped() {
        let profiles = [profile(1, "Main", "/home/u/.claude", None), profile(2, "Work", "/w", Some("g")),
            profile(3, "Work laptop", "/w2", Some("g"))];
        // Default and Main share a folder; Work and Work laptop were declared one account.
        let mut o = filed("/home/u/.claude", obs(50.0, 50.0, NOW - 1000, 1));
        o.extend(filed("/w2", obs(10.0, 20.0, NOW - 2000, 3)));
        let default = Some(Path::new("/home/u/.claude"));
        let s = snapshot(&profiles, &o, |_| Ok(()), default, NOW);
        assert_eq!(s.rows[1].account, s.rows[0].account);
        assert_eq!((s.rows[0].five_hour.used_percentage, s.rows[1].five_hour.used_percentage), (Some(50.0), Some(50.0)));
        assert_eq!(s.rows[1].five_hour.reported_by, None, "the same folder: its own figure");
        assert_eq!(s.rows[2].five_hour.used_percentage, Some(10.0));
        assert_eq!(s.rows[2].five_hour.reported_by.as_deref(), Some("Work laptop"));
        assert_eq!(s.rows[2].same_account_as, vec!["Work laptop".to_string()]);
        let sug = s.suggestion.unwrap();
        assert_eq!(sug.choice, ProfileChoice::Id(2));
        assert_eq!(sug.choices, vec![ProfileChoice::Id(2), ProfileChoice::Id(3)]);
        // Work's account can't start tasks: the shared Default/Main account is the only one left.
        let s = snapshot(&profiles, &o, |id| if matches!(id, Some(2 | 3)) { Err("disabled".into()) } else { Ok(()) },
            default, NOW);
        assert!(s.no_suggestion.unwrap().contains("Only one Claude account"));
        assert_eq!(s.rows[2].ineligible_reason.as_deref(), Some("disabled"));
    }

    /// Default's figures belong to the folder its sessions ran with: after the backend restarts
    /// with another CLAUDE_CONFIG_DIR, Default shows the new folder's (unknown), not the old one's.
    #[test]
    fn default_shows_only_the_folder_it_uses_now() {
        let o = filed("/old", obs(50.0, 50.0, NOW - 1000, 1));
        let s = snapshot(&[], &o, |_| Ok(()), Some(Path::new("/old")), NOW);
        assert_eq!(s.rows[0].five_hour.used_percentage, Some(50.0));
        let s = snapshot(&[], &o, |_| Ok(()), Some(Path::new("/new")), NOW);
        assert_eq!(s.rows[0].five_hour.status, Status::Unknown);
        let s = snapshot(&[], &o, |_| Ok(()), None, NOW);
        assert_eq!(s.rows[0].five_hour.status, Status::Unknown);
    }

    // ----- reading the user's status line -----

    struct Tree(PathBuf);
    impl Tree {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("muster-usage-sl-{}", crate::paths::random_token()));
            std::fs::create_dir_all(&p).unwrap();
            Tree(std::fs::canonicalize(p).unwrap())
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn line(cmd: &str) -> String {
        format!(r#"{{"statusLine": {{"type": "command", "command": "{cmd}"}}}}"#)
    }

    fn cmd_of(r: Result<Option<Map<String, Value>>, String>) -> Option<String> {
        r.unwrap().map(|o| o["command"].as_str().unwrap().to_string())
    }

    #[test]
    fn the_users_status_line_is_found_where_claude_code_looks() {
        let t = Tree::new();
        let home = t.dir("home");
        let env = BTreeMap::from([("HOME".to_string(), home.to_string_lossy().to_string())]);
        let proj = t.dir("proj");
        assert_eq!(cmd_of(user_status_line(&env, &proj)), None);
        t.write("home/.claude/settings.json",
            r#"{"env": {"SECRET": "x"}, "statusLine": {"type": "command", "command": "user-line", "padding": 2}}"#);
        assert_eq!(cmd_of(user_status_line(&env, &proj)).as_deref(), Some("user-line"));
        // A profile folder replaces ~/.claude.
        let profile = t.dir("profile");
        t.write("profile/settings.json", &line("work-line"));
        let mut penv = env.clone();
        penv.insert("CLAUDE_CONFIG_DIR".into(), profile.to_string_lossy().to_string());
        assert_eq!(cmd_of(user_status_line(&penv, &proj)).as_deref(), Some("work-line"));
        // Outside git, both project files are in cwd; local outranks shared, which outranks user.
        t.write("proj/.claude/settings.json", &line("proj-line"));
        assert_eq!(cmd_of(user_status_line(&penv, &proj)).as_deref(), Some("proj-line"));
        t.write("proj/.claude/settings.local.json", &line("local-line"));
        assert_eq!(cmd_of(user_status_line(&penv, &proj)).as_deref(), Some("local-line"));
        // A local file Claude Code would reject: unsure, so Muster stays out of it.
        t.write("proj/.claude/settings.local.json", "not json");
        assert!(user_status_line(&penv, &proj).is_err());
    }

    #[test]
    fn local_settings_live_at_the_repository_root_or_the_main_checkout() {
        let t = Tree::new();
        let env = BTreeMap::from([("HOME".to_string(), t.dir("home").to_string_lossy().to_string())]);
        t.dir("repo/.git");
        let sub = t.dir("repo/src/deep");
        t.write("repo/src/deep/.claude/settings.json", &line("shared-in-cwd"));
        t.write("repo/.claude/settings.json", &line("shared-at-root"));
        // The shared file comes from cwd, never the root.
        assert_eq!(cmd_of(user_status_line(&env, &sub)).as_deref(), Some("shared-in-cwd"));
        // A local file an older version left in cwd still applies, below the root's.
        t.write("repo/src/deep/.claude/settings.local.json", &line("old-local"));
        assert_eq!(cmd_of(user_status_line(&env, &sub)).as_deref(), Some("old-local"));
        t.write("repo/.claude/settings.local.json", &line("root-local"));
        assert_eq!(cmd_of(user_status_line(&env, &sub)).as_deref(), Some("root-local"));
        // A linked worktree reads the main checkout's local file.
        let main = t.0.join("repo");
        t.write("repo/.git/worktrees/wt/commondir", "../..\n");
        t.write("wt/.git", &format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()));
        let wt = t.0.join("wt");
        assert_eq!(cmd_of(user_status_line(&env, &wt)).as_deref(), Some("root-local"));
        t.write("wt/.claude/settings.local.json", &line("wt-local"));
        assert_eq!(cmd_of(user_status_line(&env, &wt)).as_deref(), Some("root-local"), "the main checkout's wins");
        // A worktree of a bare repository: Claude Code's choice is unknown, so unsure.
        t.write("bare.git/worktrees/b/commondir", "../..\n");
        t.write("b/.git", &format!("gitdir: {}\n", t.0.join("bare.git/worktrees/b").display()));
        assert!(user_status_line(&env, &t.0.join("b")).is_err());
        // A repository whose root is the home folder keeps the local file in cwd.
        let henv = BTreeMap::from([("HOME".to_string(), main.to_string_lossy().to_string())]);
        assert_eq!(cmd_of(user_status_line(&henv, &sub)).as_deref(), Some("old-local"));
    }

    /// A repository can't make Muster read through a link: not a linked settings file (say, to a
    /// credentials file), not a linked `.claude` folder, not a FIFO, nothing oversized.
    #[test]
    fn settings_are_never_read_through_links() {
        use std::os::unix::fs::symlink;
        let t = Tree::new();
        let env = BTreeMap::from([("HOME".to_string(), t.dir("home").to_string_lossy().to_string())]);
        let secret = t.write("secrets/.credentials.json", r#"{"statusLine": {"type": "command", "command": "leaked"}}"#);
        let proj = t.dir("proj");
        symlink(&secret, t.dir("proj/.claude").join("settings.json")).unwrap();
        let err = user_status_line(&env, &proj).unwrap_err();
        assert!(err.contains("link") && !err.contains("leaked"), "{err}");
        // A linked .claude folder.
        let proj2 = t.dir("proj2");
        symlink(t.0.join("secrets"), proj2.join(".claude")).unwrap();
        assert!(user_status_line(&env, &proj2).unwrap_err().contains("link"));
        // A FIFO under the name doesn't hang, a hard link or a huge file is refused.
        let proj3 = t.dir("proj3/.claude");
        let fifo = std::ffi::CString::new(proj3.join("settings.json").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(user_status_line(&env, &t.0.join("proj3")).is_err());
        std::fs::remove_file(proj3.join("settings.json")).unwrap();
        std::fs::hard_link(&secret, proj3.join("settings.json")).unwrap();
        assert!(user_status_line(&env, &t.0.join("proj3")).is_err());
        std::fs::remove_file(proj3.join("settings.json")).unwrap();
        std::fs::write(proj3.join("settings.json"), " ".repeat(MAX_SETTINGS as usize + 1)).unwrap();
        assert!(user_status_line(&env, &t.0.join("proj3")).is_err());
        // Names that look like credentials are refused outright.
        assert!(read_beneath(&t.0, &["secrets", ".credentials.json"], 1024).is_err());
        assert!(read_beneath(&t.0, &["..", "x"], 1024).is_err());

        // Unsure means the session's status line is left to Claude Code entirely.
        let settings = t.write("claude-settings.json", r#"{"hooks": {}}"#);
        install_reporter(&settings, Path::new("/opt/muster"), &env, &proj);
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), r#"{"hooks": {}}"#);
    }

    #[test]
    fn the_reporter_keeps_every_option_of_the_users_line() {
        let t = Tree::new();
        let home = t.dir("home");
        let env = BTreeMap::from([("HOME".to_string(), home.to_string_lossy().to_string())]);
        let proj = t.dir("proj");
        t.write("home/.claude/settings.json",
            r#"{"env": {"SECRET": "x"}, "statusLine": {"type": "command", "command": "bash ~/sl.sh", "padding": 2, "refreshInterval": 5}}"#);
        let settings = t.write("claude-settings.json", r#"{"hooks": {}}"#);
        let bin = Path::new("/opt/my muster/muster");
        install_reporter(&settings, bin, &env, &proj);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(doc["hooks"], json!({}));
        assert_eq!((&doc["statusLine"]["padding"], &doc["statusLine"]["refreshInterval"]), (&json!(2), &json!(5)));
        assert_eq!(doc["statusLine"]["command"], reporter_command(bin, Some("bash ~/sl.sh")));
        assert!(!std::fs::read_to_string(&settings).unwrap().contains("SECRET"));
        // No status line of the user's own: the reporter alone, which prints nothing.
        std::fs::remove_file(home.join(".claude/settings.json")).unwrap();
        install_reporter(&settings, bin, &env, &proj);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(doc["statusLine"], json!({"type": "command", "command": reporter_command(bin, None)}));
        // A status line that isn't a command is left showing as it is.
        t.write("home/.claude/settings.json", r#"{"statusLine": {"type": "something-else"}}"#);
        std::fs::write(&settings, r#"{"hooks": {}}"#).unwrap();
        install_reporter(&settings, bin, &env, &proj);
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), r#"{"hooks": {}}"#);
    }

    /// Run the composed command like Claude Code: `/bin/sh -c`, the payload on stdin.
    fn run_line(cmd: &str, input: &str) -> std::process::Output {
        use std::io::Write;
        let mut c = std::process::Command::new("/bin/sh").arg("-c").arg(cmd)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        c.wait_with_output().unwrap()
    }

    /// Whatever the reporter binary does (missing, an older Muster that rejects `statusline`, a
    /// crash), the user's line shows exactly as it would without Muster, exit code included.
    #[test]
    fn the_users_line_never_depends_on_the_reporter() {
        let t = Tree::new();
        let got = t.0.join("reporter-got");
        let older = t.write("older-muster", "#!/bin/sh\necho \"error: unrecognized subcommand 'statusline'\" >&2\nexit 2\n");
        let working = t.write("muster", &format!("#!/bin/sh\ncat > {}\n", got.display()));
        for p in [&older, &working] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let user = "cat; echo ' line'; exit 3";
        // A payload that tries to end the here-document early or run something.
        let input = "{\"a\": \"MUSTER_STATUS_INPUT\\n$(touch pwned) `touch pwned2` \\\\ \"}";
        for bin in [Path::new("/nonexistent/muster"), older.as_path(), working.as_path()] {
            let out = run_line(&reporter_command(bin, Some(user)), input);
            assert_eq!(out.status.code(), Some(3), "{bin:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{input}\n line\n"), "{bin:?}");
            assert!(out.stderr.is_empty(), "{bin:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
        assert!(!Path::new("pwned").exists() && !Path::new("pwned2").exists());
        // The working reporter got the same payload.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&got).map_or(true, |s| s.is_empty()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&got).unwrap(), format!("{input}\n"));
        // Without a user command: an empty line that never fails.
        for bin in [Path::new("/nonexistent/muster"), older.as_path()] {
            let out = run_line(&reporter_command(bin, None), input);
            assert!(out.status.success() && out.stdout.is_empty(), "{bin:?}");
        }
    }
}
