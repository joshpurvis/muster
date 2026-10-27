//! Triggers beyond time (docs/SCHEDULING_PLAN.md feature 9, Phase 2): a schedule whose `when` is
//! an event. An event never sends anything itself: it is held on its schedule (`Schedule::event`)
//! with a time to run (`next_run`), and the scheduler takes it from there like any due slot, so
//! a triggered run goes through the same pipeline as a timed one (history, pause, quiet hours,
//! caps, the busy rule, alerts, the reply in a note, cold-cache recording). Its messages have
//! origin `webhook` (shown as "Triggered"), so an agent takes them only if it accepts that.
//!
//! Events close together are merged into one run: a note's edits until it has been quiet for a
//! few minutes (the debounce), and any event less than 15 minutes after the trigger's last run
//! waits for those 15 minutes (the floor every schedule has).
//!
//! - A note: only when the human switched "Allow triggers" on for that note (off by default;
//!   no agent can switch it). Edits made by the trigger's own agent, by unattended work (any
//!   session on a scheduled or triggered chain, which includes every run a trigger started) and
//!   Muster's own appends of a run's reply never count, so a trigger can't feed itself or
//!   another trigger. At most `NOTE_DAILY_FIRES` runs a day per note.
//! - A local git branch: the branch's ref files are read every 15 s (a stat and a small read;
//!   git runs only for the commit list once it moved, or for a reftable repository).
//! - GitHub (a PR opened, CI failed): `gh api` every few minutes (default 5) through the safe
//!   runner of src/pr.rs, with the owner's own `gh` login; Muster never sees or stores a token.
//!   Failures back off (doubling, at most an hour); "gh isn't signed in" is shown as such.
//! - A trigger URL (`POST /api/trigger/<name>` on the loopback listener, with the trigger's
//!   secret) or `muster trigger <name>`: data sent with it is cut to `PAYLOAD_MAX` and handed to
//!   the agent quoted as untrusted data.
//! - Files (Phase 3, feature 11): a folder is listed every 15 s, and a file whose change time
//!   (ctime: set by the system on every write, rename or move in) is after the last look's
//!   boundary (a little before that look began), and that wasn't reported already, is new or
//!   changed; so is everything in a subfolder made or moved in since (`files_news`). Only inside folders the human chose in the scheduling settings
//!   (none by default), at most `FILES_SCAN_MAX` entries and `FILES_DEPTH` levels, hidden and
//!   temporary files skipped, symbolic links never followed (folders are read through open
//!   handles, `pinned`), and the folder checked again at every look and before a run is sent
//!   (`folder_check`: gone, swapped for a link or no longer allowed, it stops and what it had
//!   waiting is dropped). A change made while its agent, or any scheduled or triggered work,
//!   was working, or within `ABSORB_GRACE_MS` after, is taken as an agent's own and doesn't
//!   count (a file trigger can't feed itself or another): who worked when is recorded from
//!   every session's state changes (`Span`), not seen at the looks, and kept across a reload.
//!   Changes close together are merged as a note's edits are (the quiet minutes).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{GhEvent, PendingEvent, Schedule, When, plan};
use crate::hub::Hub;
use crate::model::*;

pub const NOTE_QUIET_DEFAULT_MIN: u32 = 2;
pub const NOTE_QUIET_MAX_MIN: u32 = 120;
pub const GH_EVERY_DEFAULT_MIN: u32 = 5;
pub const GH_EVERY_MIN: u32 = 2;
pub const GH_EVERY_MAX: u32 = 60;
/// Runs a day that triggers on one note may start, together.
pub const NOTE_DAILY_FIRES: u32 = 10;
/// A trigger's own daily run cap when none is given.
pub const DAILY_CAP_DEFAULT: u32 = 10;
/// Data sent with a trigger URL or `muster trigger`, at most.
pub const PAYLOAD_MAX: usize = 16 * 1024;
/// What happened, at most (merged events drop their oldest lines past this).
pub const EVENT_TEXT_MAX: usize = 8 * 1024;
/// Commits or items listed per event.
const LIST_MAX: usize = 10;
const GIT_EVERY: Duration = Duration::from_secs(15);
/// A file trigger lists at most this many entries (files and folders) per look: a bigger tree
/// is refused (shown as an error on the trigger) rather than read.
pub const FILES_SCAN_MAX: usize = 5000;
/// With subfolders, at most this many levels below the watched folder.
pub const FILES_DEPTH: usize = 3;
/// Folders the human may allow file triggers to watch, at most.
pub const WATCH_FOLDERS_MAX: usize = 20;
/// Changes seen within this long after an agent stopped working count as its own.
pub const ABSORB_GRACE_MS: i64 = 30_000;
const GH_BACKOFF_MAX_MS: i64 = 60 * plan::MINUTE_MS;

/// Debug builds only (the smoke test): `MUSTER_TEST_TRIGGER_DELAY_MS` replaces a note's quiet
/// minutes, `MUSTER_TEST_WATCH_EVERY_MS` the git look and the GitHub poll interval,
/// `MUSTER_TEST_ABSORB_GRACE_MS` the 30 s after a turn that a file change is still its own.
fn test_ms(var: &str) -> Option<i64> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(var).ok().and_then(|v| v.parse().ok()).filter(|v: &i64| *v > 0)
}

/// A watcher's answer for a trigger that was changed (or looked at by someone else) while it
/// was working: thrown away.
#[derive(Debug)]
pub struct Obsolete;

impl std::fmt::Display for Obsolete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the trigger changed while it was being looked at")
    }
}

impl std::error::Error for Obsolete {}

// ----- live watcher state (not stored) -----

/// How a watcher is doing, for the panel (`schedule.watch`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Live {
    /// "ok", "gh_not_signed_in", "gh_missing", "error".
    pub state: &'static str,
    pub detail: Option<String>,
    pub checked_at: Option<i64>,
    /// GitHub: the next poll (backing off after errors).
    pub next_at: i64,
    pub errors: u32,
}

/// One stretch of work by a session: a turn, from entering a working state (`working`) to
/// leaving it, recorded from the session's own state changes (`Watchers::observe`, on every
/// session event), so a turn that starts, writes and ends between two looks at a folder is
/// still known. A file trigger takes a change made during one, or `ABSORB_GRACE_MS` after it,
/// as an agent's own when the span is its agent's or unattended work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub session: SessionId,
    /// The named agent the session is (filled in at the next look; None: not one).
    #[serde(default)]
    pub agent: Option<i64>,
    /// Unattended work: the session's chain was started by a schedule or trigger.
    #[serde(default)]
    pub unattended: bool,
    pub start: i64,
    /// None: still working.
    #[serde(default)]
    pub end: Option<i64>,
}

/// The spans kept in memory and saved with the runtime state (`ACTIVITY_KEY`), so a backend
/// reload (sessions keep running) doesn't forget who was working.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Activity {
    spans: Vec<Span>,
    /// When they were last saved (unix ms).
    #[serde(default)]
    saved_at: i64,
    /// The spans are complete from this time on (unix ms): every span that ended after it is
    /// kept. Before it some were dropped (too many, or from before Muster kept them), so who
    /// made a change then is unknown, and such a change counts as an agent's own (`own_change`).
    #[serde(default)]
    complete_since: i64,
    #[serde(skip)]
    dirty: bool,
}

impl Activity {
    /// Span `s` is dropped: what happened up to its end is no longer known.
    fn forget(&mut self, s: &Span, now: i64) {
        self.complete_since = self.complete_since.max(s.end.unwrap_or(now));
        self.dirty = true;
    }
}

const ACTIVITY_KEY: &str = "schedules.file_trigger_activity";
/// Spans are kept at least this long after they end, and longer while a file trigger has not
/// looked past them yet (a paused one, say: `Watchers::settled`'s `keep_from`).
const SPAN_KEEP_MS: i64 = 60 * plan::MINUTE_MS;
const SPANS_MAX: usize = 1000;
/// A change this long before a span's recorded start still counts as during it: the state is
/// detected a moment after the work begins.
const SPAN_START_SLACK_MS: i64 = 5_000;

#[derive(Default)]
pub struct Watchers {
    live: Mutex<HashMap<i64, Live>>,
    /// GitHub polls in flight.
    busy: Mutex<std::collections::HashSet<i64>>,
    wake: tokio::sync::Notify,
    /// Who worked when (file triggers' "an agent's own changes").
    activity: Mutex<Activity>,
}

/// Working, as far as a file trigger goes: in a turn, starting, or stopped on a prompt.
pub fn working_state(s: AgentState) -> bool {
    matches!(s, AgentState::Starting | AgentState::Working | AgentState::NeedsApproval | AgentState::AskingQuestion | AgentState::Stuck)
}

impl Watchers {
    /// A session's state is now `working` (or not) since `at`, and whether its provenance is
    /// unattended work now (`unattended`, `agents::Registry::unattended_now`): opens or closes
    /// its span. The span is marked unattended when its turn is seen to be (at its start, while
    /// it runs or when it ends: the provenance stays the turn's until the next prompt), so a
    /// later turn of the same session can't hide what an earlier one was. Called from
    /// `Hub::emit` for every session event (often with the hub's lock held): it takes only its
    /// own lock and calls nothing.
    pub fn observe(&self, session: SessionId, working: bool, at: i64, unattended: bool) {
        let at = if at <= 0 { now_ms() } else { at };
        let mut g = self.activity.lock();
        let open = g.spans.iter().rposition(|s| s.session == session && s.end.is_none());
        match (working, open) {
            (true, None) => {
                // Bounded between looks too (they prune): the oldest finished spans go first.
                if g.spans.len() >= 2 * SPANS_MAX
                    && let Some(i) = g.spans.iter().position(|s| s.end.is_some())
                {
                    let dropped = g.spans.remove(i);
                    g.forget(&dropped, at);
                }
                g.spans.push(Span { session, agent: None, unattended, start: at, end: None });
                g.dirty = true;
            }
            (true, Some(i)) if unattended && !g.spans[i].unattended => {
                g.spans[i].unattended = true;
                g.dirty = true;
            }
            (false, Some(i)) => {
                let start = g.spans[i].start;
                g.spans[i].end = Some(at.max(start));
                g.spans[i].unattended |= unattended;
                g.dirty = true;
            }
            _ => {}
        }
    }

    /// A session went away: its span ends now.
    pub fn session_gone(&self, session: SessionId) {
        self.observe(session, false, now_ms(), false);
    }

    /// The spans, with each one's agent filled in, and whether it was unattended for one not
    /// marked when seen (from the session's provenance while the turn is open or just ended,
    /// before a later turn begins: spans restored after a reload), and the time they are
    /// complete from (`Activity::complete_since`). A finished span is kept `SPAN_KEEP_MS`, and
    /// longer while a file trigger hasn't looked past it (`keep_from`: the oldest file trigger
    /// boundary, unix ms); past `SPANS_MAX` the oldest go. Lookups run without the lock held
    /// (they take other locks).
    fn settled(&self, hub: &Hub, now: i64, keep_from: Option<i64>) -> (Vec<Span>, i64) {
        let snapshot = self.activity.lock().spans.clone();
        let mut updates: Vec<(SessionId, i64, Option<i64>, bool)> = Vec::new();
        for (i, s) in snapshot.iter().enumerate() {
            let later = snapshot[i + 1..].iter().any(|x| x.session == s.session);
            let agent = s.agent.or_else(|| hub.agents().by_session(s.session).map(|a| a.id));
            // The provenance now is this turn's only while no later turn of it has begun.
            let unattended = s.unattended
                || (!later && hub.agents().prov(s.session).is_some_and(|p| p.origin != crate::agents::Origin::Human));
            if agent != s.agent || unattended != s.unattended {
                updates.push((s.session, s.start, agent, unattended));
            }
        }
        let mut g = self.activity.lock();
        for (session, start, agent, unattended) in updates {
            if let Some(x) = g.spans.iter_mut().find(|x| x.session == session && x.start == start) {
                x.agent = agent;
                x.unattended = unattended;
                g.dirty = true;
            }
        }
        let keep = keep_from.map_or(now - SPAN_KEEP_MS, |k| k.min(now - SPAN_KEEP_MS));
        let (kept, dropped): (Vec<Span>, Vec<Span>) = std::mem::take(&mut g.spans).into_iter().partition(|s| s.end.is_none_or(|e| e >= keep));
        g.spans = kept;
        let over = g.spans.len().saturating_sub(SPANS_MAX);
        let dropped: Vec<Span> = dropped.into_iter().chain(g.spans.drain(..over)).collect();
        for s in &dropped {
            g.forget(s, now);
        }
        (g.spans.clone(), g.complete_since)
    }

    /// Save the spans if they changed since the last save.
    fn save_activity(&self, hub: &Hub, now: i64) {
        let snapshot = {
            let mut g = self.activity.lock();
            if !g.dirty {
                return;
            }
            g.dirty = false;
            g.saved_at = now;
            g.clone()
        };
        if let Err(e) = hub.store().save_runtime(ACTIVITY_KEY, &snapshot) {
            tracing::warn!("saving who was working (file triggers): {e:#}");
            self.activity.lock().dirty = true;
        }
    }

    #[cfg(test)]
    pub(super) fn forget_activity_for_test(&self) {
        *self.activity.lock() = Activity::default();
    }

    #[cfg(test)]
    pub(super) fn spans_for_test(&self) -> Vec<Span> {
        self.activity.lock().spans.clone()
    }

    #[cfg(test)]
    pub(super) fn settled_for_test(&self, hub: &Hub, now: i64, keep_from: Option<i64>) -> (Vec<Span>, i64) {
        self.settled(hub, now, keep_from)
    }
}

/// At start (a reload keeps sessions running): the spans saved before, plus what the sessions
/// say now: one working now has an open span from when its state began; one whose last turn
/// ended after the save has a span to then (its start is unknown: from the save on), and a span
/// left open at the save that is no longer working ends at its last turn (or the save).
pub(super) fn restore_activity(hub: &Hub) {
    // Nothing saved (or unreadable): nothing is known from before now. A record from before
    // `complete_since` was kept had spans only for the hour before its save.
    let fresh = Activity { complete_since: now_ms(), ..Default::default() };
    let mut saved: Activity = match hub.store().load_runtime(ACTIVITY_KEY) {
        Ok(Some(a)) => a,
        Ok(None) => fresh,
        Err(e) => {
            tracing::warn!("reading who was working (file triggers): {e:#}");
            fresh
        }
    };
    if saved.complete_since == 0 {
        saved.complete_since = saved.saved_at - SPAN_KEEP_MS;
    }
    let sessions = hub.sessions();
    let unattended: std::collections::HashSet<SessionId> = sessions.iter().map(|i| i.id).filter(|id| hub.agents().unattended_now(*id)).collect();
    let mut g = hub.triggers.activity.lock();
    let mut spans = saved.spans;
    for sp in spans.iter_mut().filter(|s| s.end.is_none()) {
        let info = sessions.iter().find(|i| i.id == sp.session);
        if info.is_some_and(|i| working_state(i.state)) {
            continue;
        }
        let last = info.and_then(|i| i.last_turn_at).filter(|t| *t >= sp.start);
        sp.end = Some(last.unwrap_or(saved.saved_at).max(sp.start));
    }
    for i in &sessions {
        let open = spans.iter().any(|s| s.session == i.id && s.end.is_none());
        let u = unattended.contains(&i.id);
        if working_state(i.state) && !open {
            spans.push(Span { session: i.id, agent: None, unattended: u, start: i.state_since.min(now_ms()), end: None });
        } else if !working_state(i.state)
            && let Some(t) = i.last_turn_at
            && t > saved.saved_at
            && !spans.iter().any(|s| s.session == i.id && s.end.is_some_and(|e| e >= t))
        {
            spans.push(Span { session: i.id, agent: None, unattended: u, start: saved.saved_at.min(t), end: Some(t) });
        }
    }
    // Spans recorded since start (session events before this ran) stay, after the saved ones.
    for s in std::mem::take(&mut g.spans) {
        if !spans.contains(&s) {
            spans.push(s);
        }
    }
    g.spans = spans;
    g.saved_at = saved.saved_at;
    g.complete_since = g.complete_since.max(saved.complete_since);
    g.dirty = true;
}

/// Is a change made at `at` (unix ms) an agent's own, for a file trigger on agent `agent`
/// (its session `own`)? Yes during, or `ABSORB_GRACE_MS` after, a span of that agent's or of
/// unattended work; and yes when who worked then is no longer known (before `complete_since`,
/// plus the grace): a change nobody can vouch for doesn't start a run.
pub fn own_change(spans: &[Span], complete_since: i64, agent: i64, own: Option<SessionId>, at: i64, now: i64) -> bool {
    let grace = test_ms("MUSTER_TEST_ABSORB_GRACE_MS").unwrap_or(ABSORB_GRACE_MS);
    if at <= complete_since + grace {
        return true;
    }
    spans.iter().any(|s| {
        (s.agent == Some(agent) || Some(s.session) == own || s.unattended)
            && at >= s.start - SPAN_START_SLACK_MS
            && at <= s.end.unwrap_or(now.max(at)) + grace
    })
}

impl Watchers {
    pub fn live(&self, id: i64) -> Option<Live> {
        self.live.lock().get(&id).cloned()
    }
    fn set(&self, id: i64, f: impl FnOnce(&mut Live)) -> bool {
        let mut g = self.live.lock();
        let e = g.entry(id).or_default();
        let before = e.clone();
        f(e);
        *e != before
    }
    pub fn forget(&self, id: i64) {
        self.live.lock().remove(&id);
    }
}

/// `watch` in a schedule's JSON: a git or GitHub watcher's state (None for others).
pub fn watch_json(hub: &Hub, s: &Schedule) -> Value {
    if !matches!(s.when, When::Git { .. } | When::Github { .. } | When::Files { .. }) {
        return Value::Null;
    }
    let l = hub.triggers.live(s.id).unwrap_or_default();
    json!({
        "state": if l.state.is_empty() { "waiting" } else { l.state },
        "detail": l.detail,
        "checked_at": l.checked_at,
        "next_at": (l.next_at > 0).then_some(l.next_at),
    })
}

// ----- checking a trigger when it is made -----

/// A URL trigger's name: `a-z`, `0-9`, `-` and `_`, starting with a letter or digit, at most 40.
pub fn check_url_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        (1..=40).contains(&name.len())
            && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
            && name.as_bytes()[0].is_ascii_alphanumeric(),
        "a trigger name is 1-40 characters of a-z, 0-9, - and _, starting with a letter or digit (it goes in the URL)"
    );
    Ok(())
}

/// A branch name as git takes it (`git check-ref-format`'s main rules), never an option.
pub fn check_branch(b: &str) -> anyhow::Result<()> {
    ensure!(
        !b.is_empty()
            && b.len() <= 200
            && !b.starts_with('-')
            && !b.starts_with('/')
            && !b.ends_with('/')
            && !b.ends_with(".lock")
            && !b.ends_with('.')
            && !b.contains("..")
            && !b.contains("//")
            && !b.contains("@{")
            && !b.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c)),
        "`{b}` isn't a branch name"
    );
    Ok(())
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => crate::paths::home_dir().join(rest),
        None if p == "~" => crate::paths::home_dir(),
        None => PathBuf::from(p),
    }
}

/// The repository's git directories: its own (`.git`, or a worktree's) and the common one
/// where refs live.
fn git_dirs(repo: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let dot = repo.join(".git");
    let gitdir = if dot.is_dir() {
        dot
    } else if dot.is_file() {
        let text = std::fs::read_to_string(&dot)?;
        let p = text.trim().strip_prefix("gitdir:").context("its .git file names no gitdir")?.trim().to_string();
        let p = PathBuf::from(p);
        if p.is_absolute() { p } else { repo.join(p) }
    } else if repo.join("HEAD").is_file() && repo.join("refs").is_dir() {
        repo.to_path_buf() // a bare repository
    } else {
        anyhow::bail!("{} isn't a git repository", repo.display());
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_absolute() { c } else { gitdir.join(c) }
        }
        Err(_) => gitdir.clone(),
    };
    Ok((gitdir, common))
}

/// The commit `refs/heads/<branch>` points at (None: no such branch), from the loose ref or
/// `packed-refs`; a reftable repository asks git.
pub fn read_ref(repo: &Path, branch: &str) -> anyhow::Result<Option<String>> {
    let (_, common) = git_dirs(repo)?;
    if common.join("reftable").is_dir() {
        let out = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}^{{commit}}")])
            .stdin(std::process::Stdio::null())
            .output()?;
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Ok((out.status.success() && !sha.is_empty()).then_some(sha));
    }
    match std::fs::read_to_string(common.join("refs/heads").join(branch)) {
        Ok(s) => {
            let s = s.trim();
            if !s.is_empty() && !s.starts_with("ref:") {
                return Ok(Some(s.to_string()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::IsADirectory => {}
        Err(e) => return Err(e.into()),
    }
    let want = format!("refs/heads/{branch}");
    if let Ok(packed) = std::fs::read_to_string(common.join("packed-refs")) {
        for line in packed.lines() {
            if let Some((sha, name)) = line.split_once(' ')
                && name.trim() == want
                && !sha.starts_with('#')
                && !sha.starts_with('^')
            {
                return Ok(Some(sha.to_string()));
            }
        }
    }
    Ok(None)
}

/// Check and tidy an event `when` before it is saved: an existing note, a git repository (made
/// absolute) and branch, an OWNER/NAME and sane timings, a unique URL name.
pub fn check_when(hub: &Hub, when: &mut When, own_id: Option<i64>) -> anyhow::Result<()> {
    match when {
        When::Cron { .. } | When::Once { .. } => {}
        When::Note { note_id, quiet_min } => {
            crate::notes::get(hub, *note_id)?;
            ensure!((1..=NOTE_QUIET_MAX_MIN).contains(quiet_min), "wait 1 to {NOTE_QUIET_MAX_MIN} minutes of quiet after the note changes");
        }
        When::Git { repo, branch } => {
            let b = branch.trim().trim_start_matches("refs/heads/").to_string();
            check_branch(&b)?;
            let p = expand_home(repo.trim());
            ensure!(p.is_absolute(), "give the repository as a full path (or ~/…)");
            let p = std::fs::canonicalize(&p).with_context(|| format!("no folder {}", p.display()))?;
            git_dirs(&p)?;
            read_ref(&p, &b).with_context(|| format!("reading {b} in {}", p.display()))?;
            *repo = p.to_string_lossy().into_owned();
            *branch = b;
        }
        When::Github { repo, branch, every_min, .. } => {
            let r = repo.trim().trim_start_matches("https://github.com/").trim_end_matches('/').to_string();
            ensure!(crate::pr::split_repo(&r).is_some(), "`{repo}` isn't OWNER/NAME (e.g. octocat/hello-world)");
            *repo = r;
            if let Some(b) = branch {
                let t = b.trim().to_string();
                if t.is_empty() {
                    *branch = None;
                } else {
                    check_branch(&t)?;
                    *branch = Some(t);
                }
            }
            ensure!((GH_EVERY_MIN..=GH_EVERY_MAX).contains(every_min), "check GitHub every {GH_EVERY_MIN} to {GH_EVERY_MAX} minutes");
        }
        When::Url { name } => {
            let n = name.trim().to_ascii_lowercase();
            check_url_name(&n)?;
            let taken = hub.schedules.list().into_iter().any(|s| {
                Some(s.id) != own_id && matches!(&s.when, When::Url { name: o } if *o == n)
            });
            ensure!(!taken, "there is already a trigger named {n}");
            *name = n;
        }
        When::Files { folder, pattern, quiet_min, .. } => {
            let allowed = hub.schedules.settings().watch_folders;
            ensure!(
                !allowed.is_empty(),
                "no folder may be watched yet: add the folder under Scheduling settings → Watched folders first \
                 (file triggers are off until you choose a folder)"
            );
            let p = expand_home(folder.trim());
            ensure!(p.is_absolute(), "give the folder as a full path (or ~/…)");
            let p = std::fs::canonicalize(&p).with_context(|| format!("no folder {}", p.display()))?;
            ensure!(p.is_dir(), "{} isn't a folder", p.display());
            ensure!(
                folder_allowed(&allowed, &p),
                "{} isn't inside a folder you allow to be watched (Scheduling settings → Watched folders: {})",
                tilde(&p.to_string_lossy()),
                allowed.iter().map(|f| tilde(f)).collect::<Vec<_>>().join(", ")
            );
            *folder = p.to_string_lossy().into_owned();
            if let Some(pat) = pattern {
                let t = pat.trim().to_string();
                if t.is_empty() || t == "*" {
                    *pattern = None;
                } else {
                    ensure!(
                        t.len() <= 100 && !t.contains('/') && !t.chars().any(char::is_control),
                        "a file pattern is a file name with * and ?, like *.pdf (at most 100 characters, no /)"
                    );
                    *pattern = Some(t);
                }
            }
            ensure!((1..=NOTE_QUIET_MAX_MIN).contains(quiet_min), "wait 1 to {NOTE_QUIET_MAX_MIN} minutes of quiet after the files change");
        }
    }
    Ok(())
}

/// Is `path` (canonical) inside one of the `allowed` folders (canonical)?
pub fn folder_allowed(allowed: &[String], path: &Path) -> bool {
    allowed.iter().any(|a| path.starts_with(a))
}

/// The scheduling settings' watched folders, checked: existing folders, made absolute and
/// canonical (symbolic links resolved), not `/`, no repeats, at most `WATCH_FOLDERS_MAX`.
pub fn check_watch_folders(folders: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for f in folders.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
        let p = expand_home(f);
        ensure!(p.is_absolute(), "`{f}`: give a watched folder as a full path (or ~/…)");
        let p = std::fs::canonicalize(&p).with_context(|| format!("no folder {}", p.display()))?;
        ensure!(p.is_dir(), "{} isn't a folder", p.display());
        ensure!(p.parent().is_some(), "the whole disk (/) can't be watched: pick a folder");
        let s = p.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    ensure!(out.len() <= WATCH_FOLDERS_MAX, "at most {WATCH_FOLDERS_MAX} watched folders");
    Ok(out)
}

/// Is a file trigger's folder still the one the human allowed, checked afresh: it exists, is a
/// folder, resolves to the very path saved (no part of it was swapped for a symbolic link,
/// wherever that leads) and is inside a watched folder. Err: why not, in words.
pub fn folder_check(allowed: &[String], folder: &str) -> Result<(), String> {
    let real = std::fs::canonicalize(folder).map_err(|e| format!("can't read {}: {e}", tilde(folder)))?;
    if real != Path::new(folder) {
        return Err(format!(
            "{} was replaced by a link (to {}), so it is no longer looked at: make the trigger again on the real folder",
            tilde(folder),
            tilde(&real.to_string_lossy())
        ));
    }
    if !real.is_dir() {
        return Err(format!("{} isn't a folder any more", tilde(folder)));
    }
    if !folder_allowed(allowed, &real) {
        return Err(FOLDER_NOT_WATCHED.into());
    }
    Ok(())
}

/// Stop what a file trigger has waiting: its held event goes, and its prompts still in an
/// agent's queue are taken back (a run not yet sent checks `event_flag` again before it is).
fn drop_waiting(hub: &Hub, s: &Schedule, why: &str) {
    super::run::cancel_unsent(hub, Some(s.id), why, false);
    if s.event.is_some() || s.next_run.is_some() {
        match hub.schedules.edit(s.id, |x| {
            x.event = None;
            x.next_run = None;
            Ok(())
        }) {
            Ok(x) => super::publish(hub, &x),
            Err(e) => tracing::warn!(schedule = s.id, "{e:#}"),
        }
    }
}

/// The watched folders changed: a file trigger whose folder is no longer allowed stops (its
/// waiting event goes, and its prompts still in an agent's queue are taken back).
pub fn watch_folders_changed(hub: &Hub) {
    let allowed = hub.schedules.settings().watch_folders;
    for s in hub.schedules.list() {
        let When::Files { folder, .. } = &s.when else { continue };
        if let Err(why) = folder_check(&allowed, folder) {
            drop_waiting(hub, &s, &why);
        }
    }
    hub.triggers.wake.notify_one();
}

const FOLDER_NOT_WATCHED: &str = "its folder is no longer one you allow to be watched (Scheduling settings → Watched folders)";

/// How a trigger reads: "When the note “Ideas” changes (after 2 minutes of quiet)".
pub fn describe(hub: &Hub, when: &When) -> String {
    match when {
        When::Cron { .. } | When::Once { .. } => String::new(),
        When::Note { note_id, quiet_min } => {
            let title = hub.store().load_note(*note_id).ok().flatten().map(|n| n.title).unwrap_or_else(|| format!("note {note_id} (deleted)"));
            let m = if *quiet_min == 1 { "1 minute".to_string() } else { format!("{quiet_min} minutes") };
            format!("When the note “{title}” changes (after {m} of quiet)")
        }
        When::Git { repo, branch } => format!("When {branch} moves in {}", tilde(repo)),
        When::Github { repo, event: GhEvent::PrOpened, every_min, .. } => {
            format!("When a pull request is opened on {repo} (checked every {every_min} minutes)")
        }
        When::Github { repo, event: GhEvent::CiFailed, branch, every_min } => match branch {
            Some(b) => format!("When CI fails on {repo} ({b}) (checked every {every_min} minutes)"),
            None => format!("When CI fails on {repo} (checked every {every_min} minutes)"),
        },
        When::Url { name } => format!("When its trigger URL is called (muster trigger {name})"),
        When::Files { folder, pattern, recursive, quiet_min } => {
            let what = pattern.as_deref().map_or("files".to_string(), |p| format!("files named {p}"));
            let sub = if *recursive { " or its subfolders" } else { "" };
            let m = if *quiet_min == 1 { "1 minute".to_string() } else { format!("{quiet_min} minutes") };
            format!("When {what} appear or change in {}{sub} (after {m} of quiet)", tilde(folder))
        }
    }
}

fn tilde(p: &str) -> String {
    let home = crate::paths::home_dir();
    match Path::new(p).strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.to_string(),
    }
}

// ----- holding an event -----

/// Hold `text` (and `payload`) on trigger `id`, to run at `at` or later: merged with an event
/// already waiting, and never sooner than 15 minutes after the trigger's last run. A debounced
/// event (a note) moves its time on with each new edit.
fn hold(hub: &Hub, id: i64, text: &str, payload: Option<String>, at: i64, debounce: bool) -> anyhow::Result<Schedule> {
    let now = hub.schedules.now();
    let s = hub.schedules.edit(id, |s| hold_on(s, text, payload, at, debounce, now))?;
    super::publish(hub, &s);
    Ok(s)
}

/// `hold` on a schedule being edited, so a watcher saves what it has seen (its cursor) and the
/// event it found in one write: a crash can't keep the one and lose the other. (The scheduler
/// also keeps a held event until the 15-minute floor has passed.)
fn hold_on(s: &mut Schedule, text: &str, payload: Option<String>, at: i64, debounce: bool, now: i64) -> anyhow::Result<()> {
    {
        let id = s.id;
        ensure!(s.when.is_trigger(), "schedule {id} isn't a trigger");
        let floor = s.last_fired.map_or(i64::MIN, |t| t + plan::MIN_GAP_MS);
        let ev = s.event.get_or_insert_with(|| PendingEvent { at: now, ..Default::default() });
        ev.count += 1;
        const CUT: &str = "(earlier events left out)";
        let joined = if ev.text.is_empty() { text.to_string() } else { format!("{}\n{text}", ev.text) };
        let mut lines: Vec<&str> = joined.lines().filter(|l| *l != CUT).collect();
        let mut cut = joined.starts_with(CUT);
        while lines.len() > 1 && lines.iter().map(|l| l.len() + 1).sum::<usize>() > EVENT_TEXT_MAX {
            lines.remove(0);
            cut = true;
        }
        let body = crate::agents::truncate_bytes(&lines.join("\n"), EVENT_TEXT_MAX);
        ev.text = if cut { format!("{CUT}\n{body}") } else { body };
        if payload.is_some() {
            ev.payload = payload;
        }
        let want = at.max(floor);
        s.next_run = Some(match s.next_run {
            Some(cur) if !debounce => cur.min(want).max(floor),
            _ => want,
        });
        Ok(())
    }
}

// ----- notes -----

/// Does a change by `by` count for note triggers? Not Muster's own append of a run's reply (an
/// agent author with no session), and not a change made on unattended work (a session whose
/// chain a schedule or trigger started).
fn note_edit_counts(hub: &Hub, by: &NoteAuthor) -> bool {
    match by.session_id {
        None => by.kind == "human",
        Some(sid) => hub.agents().prov(sid).is_none_or(|p| p.origin == crate::agents::Origin::Human),
    }
}

/// A note's text changed (notes.rs, after the change is saved).
pub fn note_changed(hub: &Hub, note: &Note) {
    if !note.triggers || !note_edit_counts(hub, &note.updated_by) {
        return;
    }
    let editor_agent = note.updated_by.session_id.and_then(|s| hub.agents().by_session(s)).map(|a| a.id);
    let who = match (&note.updated_by.name, note.updated_by.kind.as_str()) {
        (Some(n), _) => n.clone(),
        (None, "human") => "you".into(),
        (None, k) => k.to_string(),
    };
    let now = hub.schedules.now();
    for s in hub.schedules.list() {
        let When::Note { note_id, quiet_min } = s.when else { continue };
        if note_id != note.id || editor_agent == Some(s.agent_id) {
            continue;
        }
        let text = format!("The note “{}” was edited by {who} (now at revision {}).", note.title, note.rev);
        let at = now + test_ms("MUSTER_TEST_TRIGGER_DELAY_MS").unwrap_or(quiet_min as i64 * plan::MINUTE_MS);
        if let Err(e) = hold(hub, s.id, &text, None, at, true) {
            tracing::warn!(schedule = s.id, "holding a note change: {e:#}");
        }
    }
}

/// "Allow triggers" was switched: switched off, events still waiting on this note are dropped.
pub fn note_triggers_changed(hub: &Hub, note: NoteId) {
    let on = hub.store().load_note(note).ok().flatten().is_some_and(|n| n.triggers);
    for s in hub.schedules.list() {
        if matches!(s.when, When::Note { note_id, .. } if note_id == note) {
            if !on {
                // Its prompts still waiting in an agent's queue are taken back; runs still in line
                // look at the switch again before they send.
                super::run::cancel_unsent(hub, Some(s.id), "triggers were switched off for its note", false);
            }
            if !on && s.event.is_some() {
                match hub.schedules.edit(s.id, |x| {
                    x.event = None;
                    x.next_run = None;
                    Ok(())
                }) {
                    Ok(x) => super::publish(hub, &x),
                    Err(e) => tracing::warn!(schedule = s.id, "{e:#}"),
                }
            } else {
                super::publish(hub, &s);
            }
        }
    }
}

/// The schedules whose triggers watch `note` (the per-note cap counts their runs together).
pub fn note_trigger_ids(hub: &Hub, note: NoteId) -> Vec<i64> {
    hub.schedules
        .list()
        .into_iter()
        .filter(|s| matches!(s.when, When::Note { note_id, .. } if note_id == note))
        .map(|s| s.id)
        .collect()
}

/// The per-note cap, given the runs of the note's triggers taken up in the last 24 hours (a run
/// counts from when it is claimed, so several triggers due at once can't all pass it).
pub fn note_cap(title: &str, taken: u32) -> Option<plan::Action> {
    (taken >= NOTE_DAILY_FIRES).then(|| {
        plan::Action::Refuse(format!(
            "daily cap: triggers on the note “{title}” ran {taken} times in the last 24 hours (at most {NOTE_DAILY_FIRES})"
        ))
    })
}

/// May a note or file trigger's run go, as far as its note or folder goes: the note exists and
/// the human allows triggers on it; the folder is still there, the same folder (not swapped for
/// a link) and one the human allows to be watched (`folder_check`, resolved again each time).
/// Checked when the run is claimed, before it is sent and right before its prompt is submitted
/// (the switch may go off meanwhile). None: yes, or another kind of trigger.
pub fn event_flag(hub: &Hub, s: &Schedule) -> Option<plan::Action> {
    if let When::Files { folder, .. } = &s.when {
        let allowed = hub.schedules.settings().watch_folders;
        return folder_check(&allowed, folder).err().map(plan::Action::Skip);
    }
    let When::Note { note_id, .. } = s.when else { return None };
    let Some(n) = hub.store().load_note(note_id).ok().flatten() else {
        return Some(plan::Action::Skip("its note was deleted".into()));
    };
    (!n.triggers).then(|| plan::Action::Skip(format!("triggers are off for the note “{}”", n.title)))
}

// ----- trigger URL and `muster trigger` -----

/// The trigger named `name` (a URL trigger).
pub fn by_url_name(hub: &Hub, name: &str) -> Option<Schedule> {
    let n = name.trim().to_ascii_lowercase();
    hub.schedules.list().into_iter().find(|s| matches!(&s.when, When::Url { name } if *name == n))
}

/// Fire URL trigger `name`: `secret` is checked unless the caller is the human's own client
/// (`secret: None`, `muster trigger`). The payload is kept as untrusted data.
pub fn fire_url(hub: &Hub, name: &str, secret: Option<&str>, payload: Option<&[u8]>, via: &str) -> Result<Value, (u16, String)> {
    let deny = || (403, "unknown trigger or wrong secret".to_string());
    let s = by_url_name(hub, name).ok_or_else(deny)?;
    if let Some(given) = secret {
        let ok = s.secret.as_deref().is_some_and(|want| crate::paths::token_eq(want, given.trim()));
        if !ok {
            return Err(deny());
        }
    }
    let payload = match payload {
        Some(p) if p.len() > PAYLOAD_MAX => return Err((413, format!("the data sent is {} bytes; a trigger takes at most {PAYLOAD_MAX}", p.len()))),
        Some(p) => {
            let text = crate::mcp::tools::clean_text(&String::from_utf8_lossy(p));
            (!text.trim().is_empty()).then_some(text)
        }
        None => None,
    };
    let text = format!("{via} ({} bytes of data).", payload.as_ref().map_or(0, |p| p.len()));
    let s = hold(hub, s.id, &text, payload, hub.schedules.now(), false).map_err(|e| (400, format!("{e:#}")))?;
    Ok(json!({ "queued": true, "trigger": name, "runs_at": s.next_run }))
}

/// A URL trigger's address and secret, for the human (`trigger.secret`). `rotate`: a new secret
/// (the old one stops working).
pub fn secret_json(hub: &Hub, id: i64, rotate: bool) -> anyhow::Result<Value> {
    let s = hub.schedules.get(id).with_context(|| format!("no schedule {id}"))?;
    let When::Url { name } = s.when.clone() else { anyhow::bail!("schedule {id} isn't a trigger URL") };
    let s = if rotate || s.secret.is_none() {
        let x = hub.schedules.edit(id, |x| {
            x.secret = Some(crate::paths::random_token());
            Ok(())
        })?;
        super::publish(hub, &x);
        x
    } else {
        s
    };
    let url = format!("http://127.0.0.1:{}/api/trigger/{name}", hub.port());
    let secret = s.secret.clone().unwrap_or_default();
    Ok(json!({
        "name": name,
        "url": url,
        "secret": secret,
        "curl": format!("curl -fsS -X POST -H 'Authorization: Bearer {secret}' --data-binary @- {url}"),
    }))
}

// ----- the watchers (git, GitHub) -----

fn gh_bin() -> String {
    if cfg!(debug_assertions)
        && let Ok(g) = std::env::var("MUSTER_TEST_GH")
        && !g.is_empty()
    {
        return g;
    }
    "gh".into()
}

/// Is this trigger worth watching now (not paused, not expired)? A paused or expired trigger
/// costs nothing: no reads, no `gh`.
fn watching(hub: &Hub, s: &Schedule, settings: &super::Settings) -> bool {
    if s.paused || settings.paused || s.waiting_approval() {
        return false;
    }
    let Some(agent) = hub.agents().by_id(s.agent_id) else { return false };
    let f = super::facts_for(hub, &agent, s);
    plan::expires_at(s, &f).is_none_or(|e| hub.schedules.now() < e)
}

/// Look at every git trigger's branch once.
pub fn check_git(hub: &Hub) {
    let settings = hub.schedules.settings();
    for s in hub.schedules.list() {
        let When::Git { repo, branch } = &s.when else { continue };
        if !watching(hub, &s, &settings) {
            continue;
        }
        let now = hub.schedules.now();
        let got = read_ref(Path::new(repo), branch);
        let changed = hub.triggers.set(s.id, |l| {
            l.checked_at = Some(now);
            match &got {
                Ok(Some(_)) => {
                    l.state = "ok";
                    l.detail = None;
                }
                Ok(None) => {
                    l.state = "error";
                    l.detail = Some(format!("there is no branch {branch} in {}", tilde(repo)));
                }
                Err(e) => {
                    l.state = "error";
                    l.detail = Some(format!("{e:#}"));
                }
            }
        });
        let Ok(Some(sha)) = got else {
            if changed {
                super::publish(hub, &s);
            }
            continue;
        };
        match s.watch.sha.clone() {
            Some(old) if old == sha => {}
            old => {
                // The first look only records where the branch is; a move saves the new commit
                // and the event together.
                let text = old.as_deref().map(|o| git_moved_text(Path::new(repo), branch, o, &sha));
                let r = hub.schedules.edit(s.id, |x| {
                    // Changed meanwhile (another repo or branch, or another look got there
                    // first): this look is obsolete.
                    ensure!(x.when == s.when && x.watch.generation == s.watch.generation && x.watch.sha == s.watch.sha, Obsolete);
                    x.watch.sha = Some(sha.clone());
                    match &text {
                        Some(t) => hold_on(x, t, None, now, false, now),
                        None => Ok(()),
                    }
                });
                match r {
                    Ok(x) => super::publish(hub, &x),
                    Err(e) if e.is::<Obsolete>() => tracing::debug!(schedule = s.id, "an obsolete git look was dropped"),
                    Err(e) => tracing::warn!(schedule = s.id, "holding a branch move: {e:#}"),
                }
            }
        }
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

/// "master moved in ~/x: abc → def (2 new commits)" and the commits' subject lines.
fn git_moved_text(repo: &Path, branch: &str, old: &str, new: &str) -> String {
    let head = format!("{branch} moved in {}: {} → {}", tilde(&repo.to_string_lossy()), short(old), short(new));
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--no-color", "--format=%h %s", &format!("-n{}", LIST_MAX + 1), &format!("{old}..{new}"), "--"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = text.lines().take(LIST_MAX).collect();
            if lines.is_empty() {
                return format!("{head} (no new commits: it was reset or rewound)");
            }
            let more = if text.lines().count() > LIST_MAX { "\n  …and more" } else { "" };
            format!("{head}\n{}{more}", lines.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"))
        }
        _ => format!("{head} (history rewritten)"),
    }
}

// ----- files -----

/// `*` (any run of characters) and `?` (one character) against a file name.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Hidden, temporary and partial files never count (an editor's swap file, a download still
/// coming in), nor do the usual build and dependency folders.
fn skipped_name(name: &str, dir: bool) -> bool {
    name.starts_with('.')
        || (dir && matches!(name, "node_modules" | "target" | "__pycache__"))
        || (!dir
            && (name.ends_with('~')
                || [".swp", ".swx", ".tmp", ".part", ".crdownload", ".download", ".partial"].iter().any(|e| name.ends_with(e))))
}

/// A counted file one look found: its path below the watched folder and its change time (ctime,
/// ns). `moved_in`: it is inside a folder that wasn't there at the last look (made or moved in,
/// with that folder's change time), so it is new content whatever its own change time says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSeen {
    pub rel: String,
    pub stamp: u64,
    pub moved_in: Option<u64>,
}

impl FileSeen {
    /// When it changed, as far as telling an agent's own changes goes (unix ms).
    fn changed_at_ms(&self) -> i64 {
        (self.moved_in.unwrap_or(self.stamp) / 1_000_000) as i64
    }
}

/// One look at a watched folder: every counted file, and (with subfolders) the folders seen.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FolderLook {
    pub files: Vec<FileSeen>,
    /// Folders below the watched one (device, inode), sorted; None without subfolders.
    pub dirs: Option<Vec<(u64, u64)>>,
}

/// Reading folders through open handles, never by path: each subfolder is opened relative to
/// its parent's handle with `O_NOFOLLOW | O_DIRECTORY` and each entry is looked at with
/// `AT_SYMLINK_NOFOLLOW`, so swapping a folder (or any part of the path) for a symbolic link
/// while a look runs can't lead it anywhere else.
mod pinned {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::path::{Path, PathBuf};

    pub struct Stat {
        pub dir: bool,
        pub file: bool,
        pub dev: u64,
        pub ino: u64,
        pub ctime_ns: u64,
    }

    // The casts are for other systems (st_dev is an i32 on macOS).
    #[allow(clippy::unnecessary_cast)]
    fn stat_of(st: &libc::stat) -> Stat {
        let fmt = st.st_mode & libc::S_IFMT;
        Stat {
            dir: fmt == libc::S_IFDIR,
            file: fmt == libc::S_IFREG,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            ctime_ns: (st.st_ctime.max(0) as u64).saturating_mul(1_000_000_000).saturating_add(st.st_ctime_nsec.max(0) as u64),
        }
    }

    /// An open folder.
    pub struct Dir(*mut libc::DIR);

    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: the pointer came from fdopendir and is closed once, here.
            unsafe { libc::closedir(self.0) };
        }
    }

    const FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    fn wrap(fd: libc::c_int) -> io::Result<Dir> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is an open descriptor we own; fdopendir takes it over (closedir closes it).
        let d = unsafe { libc::fdopendir(fd) };
        if d.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so fd is still ours to close.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Dir(d))
    }

    impl Dir {
        /// The watched folder itself: the last part must not be a link, and where the open
        /// handle really is must be `path` (an earlier part swapped for a link shows here).
        pub fn open_root(path: &Path) -> io::Result<Dir> {
            let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: c is a valid NUL-terminated path.
            let d = wrap(unsafe { libc::open(c.as_ptr(), FLAGS) })?;
            if let Some(real) = d.real_path()
                && real != path
            {
                return Err(io::Error::other(format!("it now leads to {}", real.display())));
            }
            Ok(d)
        }

        fn fd(&self) -> libc::c_int {
            // SAFETY: self.0 is an open DIR.
            unsafe { libc::dirfd(self.0) }
        }

        /// A subfolder, opened through this one's handle (a link there is refused).
        pub fn open_child(&self, name: &CStr) -> io::Result<Dir> {
            // SAFETY: the parent handle is open and name is NUL-terminated.
            wrap(unsafe { libc::openat(self.fd(), name.as_ptr(), FLAGS) })
        }

        pub fn stat(&self) -> io::Result<Stat> {
            // SAFETY: zeroed is a valid libc::stat; fstat fills it.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: the handle is open and st is a valid out pointer.
            if unsafe { libc::fstat(self.fd(), &mut st) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(stat_of(&st))
        }

        /// An entry of this folder, not following a link.
        pub fn stat_at(&self, name: &CStr) -> io::Result<Stat> {
            // SAFETY: as above; AT_SYMLINK_NOFOLLOW looks at a link itself.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: the handle is open, name is NUL-terminated and st is a valid out pointer.
            if unsafe { libc::fstatat(self.fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(stat_of(&st))
        }

        /// The entries' names (not `.` and `..`), at most `max`: Err as soon as there is one
        /// more, so a folder with millions of entries is never read (nor held in memory) whole.
        pub fn names(&mut self, max: usize) -> Result<Vec<CString>, ()> {
            let mut out = Vec::new();
            loop {
                // SAFETY: self.0 is an open DIR; the entry is read before the next readdir.
                let e = unsafe { libc::readdir(self.0) };
                if e.is_null() {
                    break;
                }
                // SAFETY: d_name is NUL-terminated within the entry.
                let name = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) };
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    if out.len() >= max {
                        return Err(());
                    }
                    out.push(name.to_owned());
                }
            }
            Ok(out)
        }

        /// Where the open handle is (None where the system can't say).
        fn real_path(&self) -> Option<PathBuf> {
            #[cfg(target_os = "linux")]
            {
                std::fs::read_link(format!("/proc/self/fd/{}", self.fd())).ok()
            }
            #[cfg(target_os = "macos")]
            {
                let mut buf = vec![0u8; libc::PATH_MAX as usize];
                // SAFETY: F_GETPATH writes at most PATH_MAX bytes into buf.
                if unsafe { libc::fcntl(self.fd(), libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
                    return None;
                }
                let n = buf.iter().position(|b| *b == 0)?;
                buf.truncate(n);
                Some(PathBuf::from(String::from_utf8(buf).ok()?))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                None
            }
        }
    }
}

/// List `folder` (canonical; and, `recursive`, up to `FILES_DEPTH` levels below) through open
/// handles, never following a symbolic link: the folder itself must still be where it was (not
/// swapped for a link), and a subfolder swapped for one while the look runs is left out. An
/// error past `FILES_SCAN_MAX` entries. `known_dirs`: the folders the last look saw; files in
/// any other are new content (`FileSeen::moved_in`).
pub fn look_at_folder(folder: &Path, pattern: Option<&str>, recursive: bool, known_dirs: Option<&[(u64, u64)]>) -> anyhow::Result<FolderLook> {
    let mut root = pinned::Dir::open_root(folder).map_err(|e| match e.raw_os_error() {
        // The last part is a link (ELOOP, or ENOTDIR on some systems), or an earlier one is
        // (the handle is somewhere else: `open_root`'s own error).
        Some(libc::ELOOP) | Some(libc::ENOTDIR) | None => {
            anyhow::anyhow!("{} was replaced by a link ({e}): it is no longer looked at", tilde(&folder.to_string_lossy()))
        }
        _ => anyhow::anyhow!("reading {}: {e}", tilde(&folder.to_string_lossy())),
    })?;
    let mut out = FolderLook { files: Vec::new(), dirs: recursive.then(Vec::new) };
    let mut seen = 0usize;
    let too_many = || {
        anyhow::anyhow!(
            "{} has more than {FILES_SCAN_MAX} entries: watch a smaller folder{}",
            tilde(&folder.to_string_lossy()),
            if recursive { ", or leave out subfolders" } else { "" }
        )
    };
    // Depth first, one level open at a time per depth (at most FILES_DEPTH + 1 handles).
    #[allow(clippy::too_many_arguments)]
    fn walk(
        dir: &mut pinned::Dir,
        rel: &str,
        depth: usize,
        moved_in: Option<u64>,
        pattern: Option<&str>,
        recursive: bool,
        known: Option<&[(u64, u64)]>,
        seen: &mut usize,
        out: &mut FolderLook,
    ) -> Result<(), ()> {
        // The entries this look may still read: one more and the folder is refused.
        let names = dir.names(FILES_SCAN_MAX.saturating_sub(*seen))?;
        let mut subdirs = Vec::new();
        for c in names {
            *seen += 1;
            let name = String::from_utf8_lossy(c.to_bytes()).into_owned();
            let Ok(st) = dir.stat_at(&c) else { continue };
            if !(st.dir || st.file) || skipped_name(&name, st.dir) {
                continue; // links, devices, sockets; hidden and partial files
            }
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if st.dir {
                if recursive && depth < FILES_DEPTH {
                    subdirs.push((c, path));
                }
                continue;
            }
            if pattern.is_some_and(|p| !glob_match(p, &name)) {
                continue;
            }
            out.files.push(FileSeen { rel: path.chars().map(|c| if c.is_control() { '?' } else { c }).collect(), stamp: st.ctime_ns, moved_in });
        }
        for (c, path) in subdirs {
            // Opened through the parent's handle, never followed if it became a link; one it
            // can't read is left out.
            let Ok(mut sub) = dir.open_child(&c) else { continue };
            let Ok(st) = sub.stat() else { continue };
            if let Some(d) = out.dirs.as_mut() {
                d.push((st.dev, st.ino));
            }
            // A folder the last look didn't see: everything in it is new content.
            let new_dir = known.is_some_and(|k| k.binary_search(&(st.dev, st.ino)).is_err());
            let inner = moved_in.or(new_dir.then_some(st.ctime_ns));
            walk(&mut sub, &path, depth + 1, inner, pattern, recursive, known, seen, out)?;
        }
        Ok(())
    }
    let known = known_dirs.map(|k| {
        let mut k = k.to_vec();
        k.sort_unstable();
        k
    });
    walk(&mut root, "", 0, None, pattern, recursive, known.as_deref(), &mut seen, &mut out).map_err(|_| too_many())?;
    if let Some(d) = out.dirs.as_mut() {
        d.sort_unstable();
        d.dedup();
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Looks start their boundary this long before they begin: change times come from a coarse
/// clock, and a file changed while a look runs must still be after the next look's boundary.
pub const FILES_SCAN_MARGIN_NS: u64 = 2_000_000_000;

/// What a look found that is new, and the watcher state to save (None: nothing to save: no new
/// file and the same folders). A file is new when its change time is after the boundary of the
/// last saved look and it wasn't already reported with that time, or it is inside a folder made
/// or moved in since. The new boundary is `boundary` (a little before this look began), never
/// the newest change time seen, so a file changed during a look after another file was listed
/// isn't skipped. The first look only records.
pub fn files_news(prev: &super::WatchState, look: &FolderLook, boundary: u64) -> (Vec<FileSeen>, Option<super::WatchState>) {
    let record = |cursor: u64| {
        let mut w = prev.clone();
        w.stamp = Some(cursor);
        w.recent = look.files.iter().filter(|f| f.stamp > cursor).map(|f| (f.rel.clone(), f.stamp)).collect();
        w.dirs = look.dirs.clone();
        w
    };
    let Some(cursor) = prev.stamp else {
        return (Vec::new(), Some(record(boundary)));
    };
    let fresh: Vec<FileSeen> = look
        .files
        .iter()
        .filter(|f| f.moved_in.is_some() || (f.stamp > cursor && !prev.recent.iter().any(|(r, t)| *r == f.rel && *t == f.stamp)))
        .cloned()
        .collect();
    // A state from before folders were recorded (or a look without subfolders) has none to
    // compare: it records them now.
    let dirs_changed = look.dirs != prev.dirs;
    if fresh.is_empty() && !dirs_changed {
        return (fresh, None);
    }
    (fresh, Some(record(cursor.max(boundary))))
}

/// "New or changed in ~/inbox: a.pdf, b.pdf (+3 more)".
fn files_text(folder: &str, changed: &[String]) -> String {
    let shown: Vec<&str> = changed.iter().take(LIST_MAX).map(String::as_str).collect();
    let more = if changed.len() > LIST_MAX { format!(" (+{} more)", changed.len() - LIST_MAX) } else { String::new() };
    format!("New or changed in {}: {}{more}", tilde(folder), shown.join(", "))
}

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Look at every file trigger's folder once.
pub fn check_files(hub: &Hub) {
    let settings = hub.schedules.settings();
    let files: Vec<Schedule> = hub.schedules.list().into_iter().filter(|s| matches!(s.when, When::Files { .. })).collect();
    let any = !files.is_empty();
    // Who worked when is kept until every file trigger (paused and held ones too) has looked
    // past it, so one that looks again after a long pause still knows.
    let keep_from = files.iter().filter_map(|s| s.watch.stamp).min().map(|ns| (ns / 1_000_000) as i64);
    for s in files {
        let When::Files { folder, pattern, recursive, quiet_min } = &s.when else { continue };
        if !watching(hub, &s, &settings) {
            continue;
        }
        let now = hub.schedules.now();
        let boundary = now_ns().saturating_sub(FILES_SCAN_MARGIN_NS);
        // Checked again at each look: a folder that is gone, was swapped for a link (to anywhere)
        // or is no longer allowed isn't read, and what it had waiting stops.
        let checked = folder_check(&settings.watch_folders, folder);
        let got = match &checked {
            Ok(()) => look_at_folder(Path::new(folder), pattern.as_deref(), *recursive, s.watch.dirs.as_deref()),
            Err(why) => Err(anyhow::anyhow!("{why}")),
        };
        let changed = hub.triggers.set(s.id, |l| {
            l.checked_at = Some(now);
            match &got {
                Ok(_) => {
                    l.state = "ok";
                    l.detail = None;
                }
                Err(e) => {
                    l.state = "error";
                    l.detail = Some(format!("{e:#}"));
                }
            }
        });
        #[cfg(test)]
        AFTER_LOOK.with(|h| h.borrow().as_ref().map(|f| f()));
        let look = match got {
            Ok(look) => look,
            Err(e) => {
                if checked.is_err() || e.to_string().contains("replaced by a link") {
                    drop_waiting(hub, &s, &format!("{e:#}"));
                }
                if changed {
                    super::publish(hub, &s);
                }
                continue;
            }
        };
        let (fresh, watch) = files_news(&s.watch, &look, boundary);
        let Some(watch) = watch else {
            if changed {
                super::publish(hub, &s);
            }
            continue;
        };
        // Changes made while its agent, or any unattended work, was working (or just after) are
        // an agent's own: recorded as seen, never an event. Who worked when is read after the
        // look, on the real clock (change times are real), so work that began while the folder
        // was being read counts too.
        let real_now = now_ms();
        let (spans, complete_since) = hub.triggers.settled(hub, real_now, keep_from);
        let own = hub.agents().by_id(s.agent_id).and_then(|a| a.session_id);
        let theirs: Vec<String> = fresh
            .iter()
            .filter(|f| !own_change(&spans, complete_since, s.agent_id, own, f.changed_at_ms(), real_now))
            .map(|f| f.rel.clone())
            .collect();
        let first = s.watch.stamp.is_none();
        let text = (!first && !theirs.is_empty()).then(|| files_text(folder, &theirs));
        let at = now + test_ms("MUSTER_TEST_TRIGGER_DELAY_MS").unwrap_or(*quiet_min as i64 * plan::MINUTE_MS);
        let r = hub.schedules.edit(s.id, |x| {
            ensure!(x.when == s.when && x.watch == s.watch, Obsolete);
            x.watch = watch.clone();
            match &text {
                Some(t) => hold_on(x, t, None, at, true, now),
                None => Ok(()),
            }
        });
        match r {
            Ok(x) => super::publish(hub, &x),
            Err(e) if e.is::<Obsolete>() => tracing::debug!(schedule = s.id, "an obsolete folder look was dropped"),
            Err(e) => tracing::warn!(schedule = s.id, "holding a file change: {e:#}"),
        }
    }
    // Saved only while there is a file trigger to use it (it is still recorded meanwhile).
    if any {
        hub.triggers.settled(hub, now_ms(), keep_from);
        hub.triggers.save_activity(hub, now_ms());
    }
}

/// Failed CI run ids remembered per trigger.
pub const SEEN_KEEP: usize = 200;

/// What one GitHub poll found: the new items' lines, and what the watcher has seen now.
/// - PRs (every state, newest created first, so a PR opened and closed or merged between polls
///   or while Muster was off still counts): a number above the highest seen is new.
/// - CI: a failed run whose id hasn't been seen is new, whatever its id (a run can fail after a
///   newer one did: runs are listed by when they were created, not when they failed).
///
/// The first poll (nothing seen yet) only records what is there.
pub fn gh_new_items(event: GhEvent, repo: &str, v: &Value, watch: &super::WatchState) -> (Vec<String>, super::WatchState) {
    let items: Vec<&Value> = match event {
        GhEvent::PrOpened => v.as_array().map(|a| a.iter().collect()).unwrap_or_default(),
        GhEvent::CiFailed => v.get("workflow_runs").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default(),
    };
    let id_of = |x: &Value| match event {
        GhEvent::PrOpened => x.get("number").and_then(Value::as_u64),
        GhEvent::CiFailed => x.get("id").and_then(Value::as_u64),
    };
    // A failed run's key: its id and attempt (a re-run keeps the id).
    let key = |x: &Value| format!("{}.{}", id_of(x).unwrap_or(0), x.get("run_attempt").and_then(Value::as_u64).unwrap_or(1));
    let newest = items.iter().filter_map(|x| id_of(x)).max();
    let mut next = watch.clone();
    let remember = |next: &mut super::WatchState| {
        if event == GhEvent::CiFailed {
            for x in items.iter().rev() {
                let k = key(x);
                if !next.seen.contains(&k) {
                    next.seen.push(k);
                }
            }
            let over = next.seen.len().saturating_sub(SEEN_KEEP);
            next.seen.drain(..over);
        }
    };
    let Some(seen_max) = watch.max_id else {
        next.max_id = Some(newest.unwrap_or(0));
        remember(&mut next);
        return (Vec::new(), next);
    };
    let is_new = |x: &Value| match event {
        GhEvent::PrOpened => id_of(x).is_some_and(|i| i > seen_max),
        GhEvent::CiFailed => !watch.seen.contains(&key(x)),
    };
    next.max_id = Some(newest.unwrap_or(seen_max).max(seen_max));
    remember(&mut next);
    let s = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").replace(['\n', '\r'], " ");
    let mut fresh: Vec<&&Value> = items.iter().filter(|x| is_new(x)).collect();
    fresh.sort_by_key(|x| id_of(x));
    let lines = fresh
        .iter()
        .take(LIST_MAX)
        .map(|x| match event {
            GhEvent::PrOpened => format!(
                "PR #{} opened on {repo} by {}: {} ({})",
                id_of(x).unwrap_or(0),
                x.get("user").map(|u| s(u, "login")).unwrap_or_default(),
                s(x, "title"),
                s(x, "html_url")
            ),
            GhEvent::CiFailed => format!(
                "CI failed on {repo}: {} #{} on {} ({}): {} ({})",
                s(x, "name"),
                x.get("run_number").and_then(Value::as_u64).unwrap_or(0),
                s(x, "head_branch"),
                s(x, "conclusion"),
                s(x, "display_title"),
                s(x, "html_url")
            ),
        })
        .collect::<Vec<String>>();
    let mut lines = lines;
    // Every new one is reported: past the first few, by number in one line.
    if fresh.len() > LIST_MAX {
        let rest: Vec<String> = fresh[LIST_MAX..]
            .iter()
            .map(|x| match event {
                GhEvent::PrOpened => format!("#{}", id_of(x).unwrap_or(0)),
                GhEvent::CiFailed => format!("{} #{}", s(x, "name"), x.get("run_number").and_then(Value::as_u64).unwrap_or(0)),
            })
            .collect();
        let what = if event == GhEvent::PrOpened { "PRs opened" } else { "failed runs" };
        lines.push(format!("+{} more {what} on {repo}: {}", rest.len(), rest.join(", ")));
    }
    // A whole page of new ones: there may be more than one check lists.
    if !fresh.is_empty() && fresh.len() == items.len() && items.len() >= GH_PAGE[event as usize] {
        lines.push(format!("(GitHub lists only the newest {} at a time: there may be more on {repo})", items.len()));
    }
    (lines, next)
}

/// How many PRs and failed runs one GitHub check lists (the `per_page` of `gh_path`).
const GH_PAGE: [usize; 2] = [50, 20];

/// The `gh api` path a GitHub trigger polls.
pub fn gh_path(repo: &str, event: GhEvent, branch: Option<&str>) -> String {
    match event {
        GhEvent::PrOpened => format!("repos/{repo}/pulls?state=all&sort=created&direction=desc&per_page=50"),
        GhEvent::CiFailed => match branch {
            Some(b) => format!("repos/{repo}/actions/runs?status=failure&per_page=20&branch={}", url_part(b)),
            None => format!("repos/{repo}/actions/runs?status=failure&per_page=20"),
        },
    }
}

fn url_part(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// The delay before the next poll after `errors` failures in a row: the trigger's interval,
/// doubling per failure, at most an hour.
pub fn gh_backoff_ms(every_min: u32, errors: u32) -> i64 {
    let base = test_ms("MUSTER_TEST_WATCH_EVERY_MS").unwrap_or(every_min as i64 * plan::MINUTE_MS);
    if errors == 0 {
        return base;
    }
    (base.saturating_mul(1 << errors.min(6))).min(GH_BACKOFF_MAX_MS.max(base))
}

/// Poll GitHub for trigger `id` (blocking: `gh` runs here).
pub fn poll_github(hub: &Hub, id: i64, tools: &crate::pr::Tools) {
    let Some(s) = hub.schedules.get(id) else { return };
    let When::Github { repo, event, branch, every_min } = s.when.clone() else { return };
    let got = tools.api_get(&format!("{repo} {}", if event == GhEvent::PrOpened { "PRs" } else { "workflow runs" }), &gh_path(&repo, event, branch.as_deref()));
    let now = hub.schedules.now();
    let ok = got.is_ok();
    let mut saved = None;
    match got {
        Ok(v) => {
            // What it has seen and the event it found are saved in one write (a crash can't
            // keep the one and lose the other).
            let r = hub.schedules.edit(id, |x| {
                // Edited while `gh` ran (another repo or event): this answer is obsolete.
                ensure!(x.when == s.when && x.watch.generation == s.watch.generation, Obsolete);
                let (lines, watch) = gh_new_items(event, &repo, &v, &x.watch);
                x.watch = watch;
                if lines.is_empty() { Ok(()) } else { hold_on(x, &lines.join("\n"), None, now, false, now) }
            });
            match r {
                Ok(x) => saved = Some(x),
                Err(e) if e.is::<Obsolete>() => {
                    tracing::debug!(schedule = id, "an obsolete GitHub poll was dropped");
                    return;
                }
                Err(e) => tracing::warn!(schedule = id, "saving a GitHub poll: {e:#}"),
            }
        }
        Err(e) => {
            if hub.schedules.get(id).is_none_or(|x| x.when != s.when || x.watch.generation != s.watch.generation) {
                return; // Changed while gh ran: this answer is about something else.
            }
            let (state, detail) = match e {
                crate::pr::GhError::Missing => ("gh_missing", "gh (the GitHub CLI) isn't installed or not on PATH".to_string()),
                crate::pr::GhError::NotSignedIn(d) => ("gh_not_signed_in", format!("gh isn't signed in to GitHub: run `gh auth login` ({})", crate::agents::truncate_bytes(&d, 200))),
                crate::pr::GhError::Other(d) => ("error", crate::agents::truncate_bytes(&d, 400)),
            };
            hub.triggers.set(id, |l| {
                l.state = state;
                l.detail = Some(detail);
                l.errors = l.errors.saturating_add(1);
                l.checked_at = Some(now);
                l.next_at = now + gh_backoff_ms(every_min, l.errors);
            });
        }
    }
    if ok {
        hub.triggers.set(id, |l| {
            l.state = "ok";
            l.detail = None;
            l.errors = 0;
            l.checked_at = Some(now);
            l.next_at = now + gh_backoff_ms(every_min, 0);
        });
    }
    if let Some(x) = saved.or_else(|| hub.schedules.get(id)) {
        super::publish(hub, &x);
    }
}

/// Start the GitHub polls that are due (each in a blocking task; one at a time per trigger).
fn check_github(hub: &Arc<Hub>) {
    let settings = hub.schedules.settings();
    let now = hub.schedules.now();
    for s in hub.schedules.list() {
        let When::Github { every_min, .. } = s.when else { continue };
        if !watching(hub, &s, &settings) {
            continue;
        }
        let next = hub.triggers.live(s.id).map_or(0, |l| l.next_at);
        if next > now || !hub.triggers.busy.lock().insert(s.id) {
            continue;
        }
        // Not polled again before its interval, even if this poll hangs.
        hub.triggers.set(s.id, |l| l.next_at = now + gh_backoff_ms(every_min, l.errors));
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || {
            let tools = crate::pr::Tools::new(gh_bin(), "curl", false).parallel(1);
            poll_github(&hub, s.id, &tools);
            hub.triggers.busy.lock().remove(&s.id);
        });
    }
}

/// A trigger was made or changed: look at it soon (a new git trigger records where its branch
/// is; a GitHub one polls).
pub fn changed(hub: &Hub, id: i64) {
    hub.triggers.forget(id);
    hub.triggers.wake.notify_one();
}

/// The watcher loop: git every 15 s, GitHub when each trigger's poll is due.
pub fn spawn(hub: Arc<Hub>) {
    restore_activity(&hub);
    tokio::spawn(async move {
        loop {
            {
                let h = hub.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || {
                    check_git(&h);
                    check_files(&h);
                })
                .await
                {
                    tracing::warn!("git and file triggers: {e}");
                }
            }
            check_github(&hub);
            let every = test_ms("MUSTER_TEST_WATCH_EVERY_MS").map_or(GIT_EVERY, |ms| Duration::from_millis(ms as u64));
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                _ = hub.triggers.wake.notified() => {}
            }
            if hub.is_shutting_down() {
                return;
            }
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Tests: runs right after a folder is read and before its changes are judged (work that
    /// starts while a look runs).
    pub(super) static AFTER_LOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// Tests: how many entries `names` reads from `dir` with a budget of `max` (Err: more).
#[cfg(test)]
pub(super) fn names_for_test(dir: &Path, max: usize) -> Result<usize, ()> {
    pinned::Dir::open_root(dir).map_err(|_| ())?.names(max).map(|n| n.len())
}

#[cfg(test)]
pub(super) fn hold_for_test(hub: &Hub, id: i64, text: &str, at: i64, debounce: bool) -> Schedule {
    hold(hub, id, text, None, at, debounce).unwrap()
}
