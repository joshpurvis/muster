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
//!   (ctime: set by the system on every write, rename or move in) is newer than the newest one
//!   seen is new or changed. Only inside folders the human chose in the scheduling settings
//!   (none by default), at most `FILES_SCAN_MAX` entries and `FILES_DEPTH` levels, hidden and
//!   temporary files skipped, symbolic links never followed. Changes seen while its agent is
//!   working, or any scheduled or triggered work is, and for `ABSORB_GRACE_MS` after, are taken
//!   as an agent's own and don't count (a file trigger can't feed itself or another), and
//!   changes close together are merged as a note's edits are (the quiet minutes).

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
/// minutes, `MUSTER_TEST_WATCH_EVERY_MS` the git look and the GitHub poll interval.
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
    /// Files: changes seen before this are an agent's own (it was working).
    pub absorb_until: i64,
}

#[derive(Default)]
pub struct Watchers {
    live: Mutex<HashMap<i64, Live>>,
    /// GitHub polls in flight.
    busy: Mutex<std::collections::HashSet<i64>>,
    wake: tokio::sync::Notify,
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

/// The watched folders changed: a file trigger whose folder is no longer allowed stops (its
/// waiting event goes, and its prompts still in an agent's queue are taken back).
pub fn watch_folders_changed(hub: &Hub) {
    let allowed = hub.schedules.settings().watch_folders;
    for s in hub.schedules.list() {
        let When::Files { folder, .. } = &s.when else { continue };
        if folder_allowed(&allowed, Path::new(folder)) {
            continue;
        }
        super::run::cancel_unsent(hub, Some(s.id), FOLDER_NOT_WATCHED, false);
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
/// the human allows triggers on it; the folder is still one the human allows to be watched.
/// Checked when the run is claimed, before it is sent and right before its prompt is submitted
/// (the switch may go off meanwhile). None: yes, or another kind of trigger.
pub fn event_flag(hub: &Hub, s: &Schedule) -> Option<plan::Action> {
    if let When::Files { folder, .. } = &s.when {
        let allowed = hub.schedules.settings().watch_folders;
        return (!folder_allowed(&allowed, Path::new(folder))).then(|| plan::Action::Skip(FOLDER_NOT_WATCHED.into()));
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
    if s.paused || settings.paused {
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

/// One look at a watched folder: the newest change time of a counted file, and the files
/// changed after `since` (their paths below the folder).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FolderLook {
    pub newest: u64,
    pub changed: Vec<String>,
}

/// List `folder` (and, `recursive`, up to `FILES_DEPTH` levels below), without following
/// symbolic links: an error past `FILES_SCAN_MAX` entries.
pub fn look_at_folder(folder: &Path, pattern: Option<&str>, recursive: bool, since: Option<u64>) -> anyhow::Result<FolderLook> {
    use std::os::unix::fs::MetadataExt;
    let mut out = FolderLook::default();
    let mut seen = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(folder.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let rd = std::fs::read_dir(&dir).with_context(|| format!("reading {}", tilde(&dir.to_string_lossy())))?;
        for e in rd {
            let Ok(e) = e else { continue };
            seen += 1;
            ensure!(
                seen <= FILES_SCAN_MAX,
                "{} has more than {FILES_SCAN_MAX} entries: watch a smaller folder{}",
                tilde(&folder.to_string_lossy()),
                if recursive { ", or leave out subfolders" } else { "" }
            );
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            let ft = md.file_type();
            if ft.is_symlink() || skipped_name(&name, ft.is_dir()) {
                continue;
            }
            if ft.is_dir() {
                if recursive && depth < FILES_DEPTH {
                    stack.push((e.path(), depth + 1));
                }
                continue;
            }
            if !ft.is_file() || pattern.is_some_and(|p| !glob_match(p, &name)) {
                continue;
            }
            let stamp = (md.ctime().max(0) as u64).saturating_mul(1_000_000_000).saturating_add(md.ctime_nsec().max(0) as u64);
            out.newest = out.newest.max(stamp);
            if since.is_some_and(|c| stamp > c) {
                let rel = e.path().strip_prefix(folder).map(|r| r.to_string_lossy().into_owned()).unwrap_or(name);
                out.changed.push(rel.chars().map(|c| if c.is_control() { '?' } else { c }).collect());
            }
        }
    }
    out.changed.sort();
    Ok(out)
}

/// "New or changed in ~/inbox: a.pdf, b.pdf (+3 more)".
fn files_text(folder: &str, changed: &[String]) -> String {
    let shown: Vec<&str> = changed.iter().take(LIST_MAX).map(String::as_str).collect();
    let more = if changed.len() > LIST_MAX { format!(" (+{} more)", changed.len() - LIST_MAX) } else { String::new() };
    format!("New or changed in {}: {}{more}", tilde(folder), shown.join(", "))
}

/// Working now, as far as a file trigger goes: in a turn, starting, or stopped on a prompt.
fn working(s: &SessionInfo) -> bool {
    matches!(s.state, AgentState::Starting | AgentState::Working | AgentState::NeedsApproval | AgentState::AskingQuestion | AgentState::Stuck)
}

/// Would a change seen now be an agent's own? Its agent is working, or any session is working
/// on unattended (scheduled or triggered) work.
fn agents_at_work(hub: &Hub, agent_id: i64) -> bool {
    let own = hub.agents().by_id(agent_id).and_then(|a| a.session_id);
    hub.sessions().into_iter().filter(working).any(|s| {
        Some(s.id) == own || hub.agents().prov(s.id).is_some_and(|p| p.origin != crate::agents::Origin::Human)
    })
}

/// Look at every file trigger's folder once.
pub fn check_files(hub: &Hub) {
    let settings = hub.schedules.settings();
    for s in hub.schedules.list() {
        let When::Files { folder, pattern, recursive, quiet_min } = &s.when else { continue };
        if !watching(hub, &s, &settings) {
            continue;
        }
        let now = hub.schedules.now();
        let path = Path::new(folder);
        let got = if folder_allowed(&settings.watch_folders, path) {
            look_at_folder(path, pattern.as_deref(), *recursive, s.watch.stamp)
        } else {
            Err(anyhow::anyhow!("{FOLDER_NOT_WATCHED}"))
        };
        let busy = agents_at_work(hub, s.agent_id);
        let mut absorb = false;
        let changed = hub.triggers.set(s.id, |l| {
            l.checked_at = Some(now);
            if busy {
                l.absorb_until = now + ABSORB_GRACE_MS;
            }
            absorb = busy || now < l.absorb_until;
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
        let Ok(look) = got else {
            if changed {
                super::publish(hub, &s);
            }
            continue;
        };
        if s.watch.stamp.is_some_and(|c| look.newest <= c) && look.changed.is_empty() {
            if changed {
                super::publish(hub, &s);
            }
            continue;
        }
        // The first look only records the newest change; later ones hold an event for what
        // changed (unless it was an agent's own), saved with the new cursor in one write.
        let first = s.watch.stamp.is_none();
        let text = (!first && !absorb && !look.changed.is_empty()).then(|| files_text(folder, &look.changed));
        let at = now + test_ms("MUSTER_TEST_TRIGGER_DELAY_MS").unwrap_or(*quiet_min as i64 * plan::MINUTE_MS);
        let r = hub.schedules.edit(s.id, |x| {
            ensure!(x.when == s.when && x.watch.generation == s.watch.generation && x.watch.stamp == s.watch.stamp, Obsolete);
            x.watch.stamp = Some(look.newest.max(s.watch.stamp.unwrap_or(0)));
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
pub(super) fn absorb_for_test(hub: &Hub, id: i64, until: i64) {
    hub.triggers.set(id, |l| l.absorb_until = until);
}

#[cfg(test)]
pub(super) fn hold_for_test(hub: &Hub, id: i64, text: &str, at: i64, debounce: bool) -> Schedule {
    hold(hub, id, text, None, at, debounce).unwrap()
}
