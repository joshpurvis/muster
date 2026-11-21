//! Pull requests for the human to merge (docs/MCP.md "Pull requests to merge", docs/UI.md "PRs to
//! merge"): an agent posts a card for a pull request it wants merged; the human merges it with a
//! click in Muster's window, and Muster's own code (never a model) does the merge.
//!
//! - `post_pr` checks the card (every problem listed at once), copies its pictures into Muster's
//!   data directory (a rebuild in the agent's folder can't swap them later), and stores it. It
//!   doesn't call GitHub: posting stays fast and works offline. Posting the same pull request again
//!   replaces its open card, keeping the number.
//! - Muster reads each open card's pull request from GitHub when the tab asks (`refresh`: when it
//!   is shown, every 2 minutes while it is, and with Check again). `verdict` turns a read into the
//!   four rules on the card (open and not a draft, the commit the agent posted, fits onto the base,
//!   checks). A new head marks the card stale and tells the poster once.
//! - `merge` (only from a click in Muster's window: `rpc::pr_from_window`) claims the card in one
//!   transaction for the commit, base and post the human saw and returns; in its own task it reads
//!   the pull request again and refuses unless every rule passes, the base is unchanged and the base
//!   branch has no merge queue, then runs the one write, `gh pr merge --merge --match-head-commit=<the
//!   posted commit>` (src/pr.rs `Gh`), and reads it back: merged as commit X, refused with GitHub's
//!   words, or not known yet (it may still merge; the next read settles it). A restart in the middle
//!   reads GitHub again at the next start (`resume`).
//! - Every outcome the poster should know (merged, refused, changes asked, stale) is a message in
//!   the card's `outbox`, stored first, then delivered as an answer to a question is
//!   (src/relay.rs): to the posting agent by name, else its session, else its lead; a restart
//!   finishes it.

use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agents::{self, Origin};
use crate::hub::Hub;
use crate::model::*;
use crate::pr::{Gh, GhError};
use crate::relay::{Delivery, DeliveryStatus, Who};

/// Open cards one poster may have, and in all.
pub const MAX_OPEN_PER_POSTER: usize = 20;
pub const MAX_OPEN: usize = 100;
pub const MAX_PICTURES: usize = 6;
/// All of one card's pictures together.
pub const MAX_CARD_BYTES: usize = 24 << 20;
/// Merged and closed cards a listing returns (the newest).
pub const DONE_LIMIT: usize = 50;
/// A card's pull request is read again at most this often (Check again: `FORCED_REFRESH_MS`).
pub const REFRESH_MS: i64 = 60_000;
const FORCED_REFRESH_MS: i64 = 3_000;
/// GitHub reads at once, in one refresh.
const PARALLEL_READS: usize = 4;
/// Reads of a merge's result before Muster takes GitHub's word that it didn't merge.
const RESULT_READS: u32 = 3;
const PICTURE_EXT: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "svg"];
/// A picture more than this many times as tall as it is wide is left out of a card: fitted to the
/// window it is a sliver too narrow to read (a full-page phone screenshot, 390 x 8000). One phone
/// screen (390 x 844, about 2.2) and anything wider are shown.
pub const MAX_TALL: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// In the tab, waiting for the human.
    Waiting,
    /// The human's click claimed it; Muster is reading and merging it.
    Merging,
    Merged,
    /// GitHub's newest commit isn't the one posted: Merge is off until it is posted again.
    Stale,
    /// The human asked for changes: out of the list until it is posted again.
    ChangesAsked,
    /// Its poster took it back.
    Withdrawn,
    /// Closed on GitHub, or merged there by something other than this card.
    Closed,
}

/// The statuses of an open card: one per pull request (`prs_open`), posting again replaces it.
pub const OPEN_STATUSES: [Status; 4] = [Status::Waiting, Status::Merging, Status::Stale, Status::ChangesAsked];

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::Merging => "merging",
            Status::Merged => "merged",
            Status::Stale => "stale",
            Status::ChangesAsked => "changes_asked",
            Status::Withdrawn => "withdrawn",
            Status::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picture {
    /// Where the agent had it (shown in Details).
    pub path: String,
    pub caption: String,
    pub side: Side,
    /// Muster's copy, inside the card's directory (`<post>/<index>.<ext>`); empty when it was
    /// left out.
    pub file: String,
    /// Left out for being too tall to read (`MAX_TALL`): its size. Muster keeps no copy, and the
    /// window shows a line in its place, so the other pictures keep their pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub too_tall: Option<Size>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// The picture's size when it is too tall to read in the window (`MAX_TALL`); `None` for one that
/// is shown, or whose header doesn't give a size.
pub fn too_tall(mime: &str, bytes: &[u8]) -> Option<Size> {
    let (width, height) = crate::image_size::size(mime, bytes)?;
    (height as u64 > MAX_TALL as u64 * width as u64).then_some(Size { width, height })
}

/// One check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Pass,
    Fail,
    /// Not a failure, but Merge is off (new commits).
    Warn,
    /// GitHub is still working it out (checks running, mergeability not computed yet).
    Wait,
    /// Nothing to check (no checks at all).
    None,
}

/// A pull request's checks, counted from GitHub's status check rollup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub running: u32,
    /// The first few failing and running ones, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<String>,
}

/// What Muster read from GitHub about a card's pull request (`gh pr view`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Read {
    pub at: i64,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub draft: bool,
    pub head_sha: String,
    pub base: String,
    /// MERGEABLE, CONFLICTING or UNKNOWN (GitHub still computing).
    pub mergeable: String,
    pub merge_state: String,
    pub title: String,
    pub url: String,
    pub author: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit: Option<String>,
    /// Auto-merge is on for it on GitHub (GitHub would merge it by itself, its own way).
    #[serde(default)]
    pub auto_merge: bool,
    /// GitHub's reply lacked a field the rules rest on, or had a value Muster doesn't know.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    pub checks: Checks,
}

/// The four rules on the card, from a read, against the commit that was posted. `ok`: Merge may
/// go; `reason`: why not, in plain words ("it conflicts with master").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub ok: bool,
    pub state: Mark,
    pub commit: Mark,
    pub base: Mark,
    pub checks: Mark,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadError {
    pub at: i64,
    pub text: String,
}

/// A merge the human started that didn't happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub at: i64,
    /// In plain words, after "Not merged: " ("it conflicts with master").
    pub reason: String,
    /// GitHub's (gh's) own words, when it said something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_said: Option<String>,
    /// Muster asked GitHub to merge but couldn't confirm the result (gh timed out, the read back
    /// failed, or GitHub left it open after accepting): it may still merge. The next read settles it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
    /// The poster wasn't told (a restart cut the merge off before any result).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_told: bool,
}

/// The human's click and what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeTry {
    /// When the human's click claimed it.
    pub at: i64,
    /// The commit it was for (the posted one), the post the human saw, and the base branch the
    /// window showed: the merge goes ahead only if GitHub's read before it shows that base.
    pub sha: String,
    #[serde(default)]
    pub post: u32,
    #[serde(default)]
    pub base: String,
    /// When the write (`gh pr merge`) was started: only then can a merge found later be this click's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<i64>,
    /// The merge commit GitHub made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub at: i64,
    pub words: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub at: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Merged,
    Refused,
    ChangesAsked,
    Stale,
}

/// A message for the poster, stored before it is delivered (src/relay.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub kind: NoteKind,
    pub at: i64,
    /// The sender label: `muster` for Muster's results, `human` for the human's own words.
    pub from: String,
    /// The words, as they go to the poster itself (`for_lead` adds why they reach its lead).
    pub text: String,
    pub delivery: Delivery,
    /// Who it is for and the provenance it travels with, fixed when it was written (a later post
    /// of the card, by its poster or its lead, never redirects or relabels it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Who>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::agents::Prov>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pr {
    /// The card's number (P<id>), from the row.
    #[serde(default)]
    pub id: i64,
    pub status: Status,
    pub created_at: i64,
    pub updated_at: i64,
    pub poster: Who,
    pub origin: Origin,
    pub hop: u32,
    /// OWNER/NAME as the agent gave it; GitHub's own address is in `github.url`.
    pub repo: String,
    pub number: u64,
    /// The commit the agent posted the card for: the only one Merge merges.
    pub head_sha: String,
    pub summary: String,
    pub who_notices: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pictures: Vec<Picture>,
    pub tests: String,
    pub undo: String,
    pub after_merge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// How many times it was posted (1: once). Pictures of post N are in directory N.
    pub post: u32,
    /// "Not now": set aside at this time (it moves down the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub later: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<Read>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The last read failed (the read before it, if any, is kept in `github`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<ReadError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeTry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Changes>,
    /// The poster was told its card went stale (once per post).
    #[serde(default)]
    pub stale_told: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn: Option<Ended>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Ended>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbox: Vec<Note>,
}

/// Why `Store::post_pr` refused a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRefusal {
    /// Another poster has this pull request's open card (its label, its number).
    Theirs(String, i64),
    /// Being merged right now (its number).
    Merging(i64),
    PosterFull(usize),
    Full(usize),
    /// The open card changed while this post was being checked.
    Changed,
}

// ----- GitHub's answer -----

fn s<'a>(v: &'a Value, k: &str) -> &'a str { v.get(k).and_then(Value::as_str).unwrap_or("") }
fn n(v: &Value, k: &str) -> u64 { v.get(k).and_then(Value::as_u64).unwrap_or(0) }

/// One entry of a status check rollup: passed, failed, or still running. A CheckRun passes when
/// it is COMPLETED with SUCCESS, NEUTRAL or SKIPPED (gh gives a running one an empty
/// conclusion); a StatusContext passes with SUCCESS and runs while PENDING or EXPECTED. Anything
/// else (FAILURE, CANCELLED, TIMED_OUT, ACTION_REQUIRED, STALE, ERROR, a kind Muster doesn't
/// know) counts as failed: Merge is never on for a check Muster can't read as passed.
fn check_mark(c: &Value) -> Mark {
    match s(c, "__typename") {
        "CheckRun" => match (s(c, "status"), s(c, "conclusion")) {
            ("COMPLETED", "SUCCESS" | "NEUTRAL" | "SKIPPED") => Mark::Pass,
            ("COMPLETED", _) => Mark::Fail,
            _ => Mark::Wait,
        },
        "StatusContext" => match s(c, "state") {
            "SUCCESS" => Mark::Pass,
            "PENDING" | "EXPECTED" => Mark::Wait,
            _ => Mark::Fail,
        },
        _ => Mark::Fail,
    }
}

fn check_name(c: &Value) -> String {
    let name = if s(c, "__typename") == "StatusContext" { s(c, "context") } else { s(c, "name") };
    clip(&plain(name), 80)
}

fn clip(t: &str, max: usize) -> String {
    match t.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

/// `gh pr view --json VIEW_FIELDS` output as a `Read` at time `at`. GitHub's text (title, names)
/// is cleaned to plain text; it is only ever shown.
pub fn read_view(v: &Value, at: i64) -> Read {
    let mut checks = Checks::default();
    for c in v.get("statusCheckRollup").and_then(Value::as_array).into_iter().flatten() {
        checks.total += 1;
        match check_mark(c) {
            Mark::Pass => checks.passed += 1,
            Mark::Wait => {
                checks.running += 1;
                if checks.waiting.len() < 5 { checks.waiting.push(check_name(c)); }
            }
            _ => {
                checks.failed += 1;
                if checks.failing.len() < 5 { checks.failing.push(check_name(c)); }
            }
        }
    }
    let clean = |t: &str, max: usize| clip(&plain(t), max);
    // Every field the four rules rest on, in a form gh gives it: else the read counts as failed.
    // The base branch exactly as GitHub names it (compared with what the human saw, and shown):
    // a name that cleaning would change can't be shown faithfully, so the read counts as failed.
    let base_raw = s(v, "baseRefName");
    let base_ok = (1..=255).contains(&base_raw.len()) && plain(base_raw) == base_raw && !base_raw.chars().any(char::is_whitespace);
    let complete = matches!(s(v, "state"), "OPEN" | "CLOSED" | "MERGED")
        && v.get("isDraft").is_some_and(Value::is_boolean)
        && matches!(s(v, "mergeable"), "MERGEABLE" | "CONFLICTING" | "UNKNOWN")
        && v.get("statusCheckRollup").is_some_and(|c| c.is_array() || c.is_null())
        && v.get("autoMergeRequest").is_some_and(|a| a.is_null() || a.is_object())
        && base_ok;
    let sha = s(v, "headRefOid").to_ascii_lowercase();
    Read {
        at,
        state: clean(s(v, "state"), 20),
        draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        head_sha: if crate::pr::sha_ok(&sha) { sha } else { String::new() },
        base: if base_ok { base_raw.to_string() } else { clean(base_raw, 200) },
        mergeable: clean(s(v, "mergeable"), 20),
        merge_state: clean(s(v, "mergeStateStatus"), 20),
        title: clean(s(v, "title"), 300),
        url: crate::pr::https_url(s(v, "url")).unwrap_or("").to_string(),
        author: clean(v.get("author").and_then(|a| a.get("login")).and_then(Value::as_str).unwrap_or(""), 100),
        additions: n(v, "additions"),
        deletions: n(v, "deletions"),
        files: n(v, "changedFiles"),
        merge_commit: v.get("mergeCommit").and_then(|m| m.get("oid")).and_then(Value::as_str)
            .map(str::to_ascii_lowercase).filter(|o| crate::pr::sha_ok(o)),
        auto_merge: v.get("autoMergeRequest").is_some_and(|a| !a.is_null()),
        incomplete: !complete,
        checks,
    }
}

/// The four rules for read `r` of a card posted for `posted` (pure; tested against real gh output).
/// GitHub still computing whether it fits (UNKNOWN) doesn't refuse on its own: the merge call
/// decides, and its refusal is shown. No checks at all passes ("GitHub runs no checks here").
pub fn verdict(r: &Read, posted: &str) -> Verdict {
    let state = if r.state == "OPEN" && !r.draft && !r.auto_merge && !r.incomplete { Mark::Pass } else { Mark::Fail };
    let commit = if !r.head_sha.is_empty() && r.head_sha == posted { Mark::Pass } else { Mark::Warn };
    let base = match r.mergeable.as_str() {
        "MERGEABLE" => Mark::Pass,
        "CONFLICTING" => Mark::Fail,
        _ => Mark::Wait,
    };
    let c = &r.checks;
    let checks = if c.failed > 0 { Mark::Fail } else if c.running > 0 { Mark::Wait } else if c.total == 0 { Mark::None } else { Mark::Pass };
    let base_name = if r.base.is_empty() { "the base branch".to_string() } else { r.base.clone() };
    let reason = if state == Mark::Fail {
        Some(match r.state.as_str() {
            "MERGED" => "it is merged on GitHub already".to_string(),
            "CLOSED" => "it is closed on GitHub".to_string(),
            _ if r.incomplete => "GitHub's reply lacked something Muster checks; Check again".to_string(),
            "OPEN" if r.draft => "it is still a draft".to_string(),
            "OPEN" => "auto-merge is on for it on GitHub, so GitHub would merge it by itself".to_string(),
            other => format!("GitHub says it is {}", if other.is_empty() { "in no state Muster knows" } else { other }),
        })
    } else if commit != Mark::Pass {
        Some("it has new commits since it was posted".to_string())
    } else if base == Mark::Fail {
        Some(format!("it conflicts with {base_name}"))
    } else if checks == Mark::Fail {
        Some(format!("{} of {} {} failed", c.failed, c.total, if c.total == 1 { "check" } else { "checks" }))
    } else if checks == Mark::Wait {
        Some("a check is still running".to_string())
    } else {
        None
    };
    Verdict { ok: reason.is_none(), state, commit, base, checks, reason }
}

fn gh_words(e: &GhError) -> String {
    match e {
        GhError::Missing => "gh (the GitHub CLI) isn't installed or not on Muster's PATH".to_string(),
        GhError::NotSignedIn(d) => format!("gh isn't signed in to GitHub: run `gh auth login` ({})", clip(d, 300)),
        GhError::Other(d) => clip(d, 600),
    }
}

// ----- posting -----

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PictureIn {
    pub path: String,
    pub caption: String,
    pub side: Side,
}

/// `post_pr`'s arguments.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPr {
    pub repo: String,
    pub number: u64,
    pub head_sha: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub who_notices: String,
    #[serde(default)]
    pub pictures: Vec<PictureIn>,
    #[serde(default)]
    pub tests: String,
    #[serde(default)]
    pub undo: String,
    #[serde(default)]
    pub after_merge: String,
    #[serde(default)]
    pub details: Option<String>,
}

/// A card that passed the check, its pictures read, not yet stored.
#[derive(Debug, Clone)]
pub struct Checked {
    pub repo: String,
    pub number: u64,
    pub head_sha: String,
    pub summary: String,
    pub who_notices: String,
    pub pictures: Vec<CheckedPicture>,
    pub tests: String,
    pub undo: String,
    pub after_merge: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CheckedPicture {
    pub pic: PictureIn,
    pub mime: &'static str,
    /// Empty when it is left out (`too_tall`).
    pub data: Vec<u8>,
    pub too_tall: Option<Size>,
}

/// Plain text from an agent or GitHub: control characters cleaned (`clean_text`), and Unicode's
/// direction controls dropped, so text can't be shown reordered.
fn plain(t: &str) -> String {
    crate::mcp::tools::clean_text(t).chars()
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}'))
        .collect()
}

fn clean(t: &str) -> String {
    plain(t).trim().to_string()
}

fn field(p: &mut Vec<String>, name: &str, value: &str, min: usize, max: usize, one_line: bool, need: &str) {
    let n = value.chars().count();
    if n == 0 {
        p.push(format!("{name} is missing: {need}"));
    } else if n < min {
        p.push(format!("{name} is too short ({n} characters, at least {min}): {need}"));
    } else if n > max {
        p.push(format!("{name} is too long ({n} characters, at most {max})"));
    }
    if one_line && value.contains('\n') {
        p.push(format!("{name} must be one line"));
    }
}

fn ext_of(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "svg",
    }
}

/// The check every card passes before it is stored: which pull request and commit, what changes
/// and who notices, the tests the agent ran, how to undo it, what comes after the merge, and
/// pictures that are images. Every problem is listed, so one retry can fix them all.
pub fn check(c: NewPr) -> Result<Checked, Vec<String>> {
    let mut p = Vec::new();
    let repo = c.repo.trim().to_string();
    if crate::pr::split_repo(&repo).is_none() {
        p.push(format!("repo: OWNER/NAME on GitHub, such as acme/notes-app (got {:?})", clip(&repo, 120)));
    }
    if !(1..=1u64 << 53).contains(&c.number) {
        p.push("number: the pull request's number on GitHub".into());
    }
    let head_sha = c.head_sha.trim().to_ascii_lowercase();
    if !crate::pr::sha_ok(&head_sha) {
        p.push("head_sha: the full 40-character id of the commit you mean (git rev-parse HEAD after you pushed); Merge merges only that commit".into());
    }
    let summary = clean(&c.summary);
    let who_notices = clean(&c.who_notices);
    let tests = clean(&c.tests);
    let undo = clean(&c.undo);
    let after_merge = clean(&c.after_merge);
    let details = Some(clean(c.details.as_deref().unwrap_or(""))).filter(|d| !d.is_empty());
    field(&mut p, "summary", &summary, 20, 600, false,
        "one to three plain sentences on what changes, for someone who knows the product but not the code");
    field(&mut p, "who_notices", &who_notices, 10, 300, false, "who notices the change, and where (\"anyone who opens the settings page\")");
    field(&mut p, "tests", &tests, 10, 1000, false, "the tests you ran and their result, in your own words");
    field(&mut p, "undo", &undo, 10, 500, false, "how to undo it if it goes wrong (\"revert the merge commit\")");
    field(&mut p, "after_merge", &after_merge, 5, 500, false,
        "what has to happen after the merge, and who does it (\"I pull and rebuild; then reload the window\"), or \"nothing\"");
    if let Some(d) = &details {
        field(&mut p, "details", d, 1, 2000, false, "");
    }
    if c.pictures.len() > MAX_PICTURES {
        p.push(format!("at most {MAX_PICTURES} pictures (it has {})", c.pictures.len()));
    }
    let mut pictures = Vec::new();
    let mut bytes = 0usize;
    for (i, pic) in c.pictures.into_iter().take(MAX_PICTURES).enumerate() {
        let caption = clean(&pic.caption);
        field(&mut p, &format!("picture {}'s caption", i + 1), &caption, 3, 200, true, "say what it shows");
        let path = pic.path.trim().to_string();
        let ext = std::path::Path::new(&path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        if path.chars().any(char::is_control) {
            p.push(format!("picture {}: a path without control characters", i + 1));
            continue;
        }
        if !path.starts_with('/') {
            p.push(format!("picture {}: give an absolute path", i + 1));
            continue;
        }
        if !ext.as_deref().is_some_and(|e| PICTURE_EXT.contains(&e)) {
            p.push(format!("picture {}: png, jpg, gif, webp or svg only", i + 1));
            continue;
        }
        // Read as the image panel reads a file: a regular file, at most 8 MiB, and an image by
        // its content. Not a symlink: the picture is the file the agent names, nothing it points to.
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            p.push(format!("picture {}: a file, not a symlink", i + 1));
            continue;
        }
        match crate::panel_apps::read_image(std::path::Path::new("/"), path.trim_start_matches('/')) {
            Ok((mime, data)) => {
                let pic = PictureIn { path, caption, side: pic.side };
                // Too tall to read: left out, its place kept (the poster is told; `post`).
                match too_tall(mime, &data) {
                    Some(size) => pictures.push(CheckedPicture { pic, mime, data: Vec::new(), too_tall: Some(size) }),
                    None => {
                        bytes += data.len();
                        pictures.push(CheckedPicture { pic, mime, data, too_tall: None });
                    }
                }
            }
            Err(e) => p.push(format!("picture {}: {e:#}", i + 1)),
        }
    }
    if bytes > MAX_CARD_BYTES {
        p.push(format!("the pictures come to {} MiB; at most {} MiB in all", bytes >> 20, MAX_CARD_BYTES >> 20));
    }
    if !p.is_empty() {
        return Err(p);
    }
    Ok(Checked { repo, number: c.number, head_sha, summary, who_notices, pictures, tests, undo, after_merge, details })
}

/// MCP `post_pr` for session `caller`. Returns at once, without calling GitHub.
pub fn post(hub: &Hub, caller: SessionId, args: Value) -> anyhow::Result<Value> {
    let poster = crate::relay::who(hub, caller).context("calling session no longer exists")?;
    anyhow::ensure!(poster.kind != SessionKind::Shell, "only agent sessions post pull requests");
    let new: NewPr = serde_json::from_value(args).map_err(|e| anyhow::anyhow!("{e}"))?;
    let c = check(new).map_err(|problems| anyhow::anyhow!(
        "Muster refused the card. Fix all of these and post it again:\n- {}", problems.join("\n- ")))?;
    let route = agents::outgoing(hub.agents().prov(caller));
    let now = now_ms();
    let pictures_of = |post: u32| c.pictures.iter().enumerate().map(|(i, x)| Picture {
        path: x.pic.path.clone(), caption: x.pic.caption.clone(), side: x.pic.side, too_tall: x.too_tall,
        file: if x.too_tall.is_some() { String::new() } else { format!("{post}/{i}.{}", ext_of(x.mime)) },
    }).collect::<Vec<_>>();
    let card = Pr {
        id: 0, status: Status::Waiting, created_at: now, updated_at: now, poster: poster.clone(), origin: route.origin.clone(),
        hop: route.hop, repo: c.repo.clone(), number: c.number, head_sha: c.head_sha.clone(), summary: c.summary.clone(),
        who_notices: c.who_notices.clone(), pictures: pictures_of(1), tests: c.tests.clone(), undo: c.undo.clone(),
        after_merge: c.after_merge.clone(), details: c.details.clone(), post: 1, later: None, github: None, verdict: None,
        read_error: None, refused: None, merge: None, changes: None, stale_told: false, withdrawn: None, closed: None,
        outbox: Vec::new(),
    };
    // The pictures go into the post's own directory before the card is stored (the store calls
    // `write` inside its transaction): a card never shows without them, and a failed copy leaves
    // the card it would have replaced as it was.
    let write = |stored: &Pr| store_pictures(hub, stored, &c);
    // Whether the caller may post over the open card is worked out before the store's lock is
    // taken (it looks up sessions and agents, whose locks are taken in the other order elsewhere),
    // then checked to be for the same card, post and poster inside the transaction.
    let allowed = hub.store().open_pr(&c.repo, c.number)?.map(|o| (o.id, o.post, o.poster.clone(), may_act(hub, &o.poster, &poster)));
    let stored = hub.store().post_pr(&card, MAX_OPEN_PER_POSTER, MAX_OPEN, |old| {
        if old.status == Status::Merging {
            return Err(PostRefusal::Merging(old.id));
        }
        let Some((id, post, who, may)) = &allowed else { return Err(PostRefusal::Changed) };
        if *id != old.id || *post != old.post || !crate::relay::same(who, &old.poster) {
            return Err(PostRefusal::Changed);
        }
        if !may {
            return Err(PostRefusal::Theirs(old.poster.label.clone(), old.id));
        }
        // Posted again: the new commit, words and pictures; what Muster read for the old one,
        // and what the human did with it, start over. Messages still on their way keep going.
        let post = old.post + 1;
        Ok(Pr { id: old.id, created_at: now, pictures: pictures_of(post), post, outbox: old.outbox.clone(), ..card.clone() })
    }, write).map_err(|e| anyhow::anyhow!("the card was not posted: {e:#}"))?;
    let (pr, replaced) = match stored {
        Ok(x) => x,
        Err(PostRefusal::Merging(n)) => anyhow::bail!(
            "the human is merging {} #{} right now (card P{n}); wait for the result message before posting it again", c.repo, c.number),
        Err(PostRefusal::Theirs(who, n)) => anyhow::bail!(
            "{who} posted {} #{} already (card P{n}); only the agent that posted a card posts it again (or, once that \
            session has ended, the session or agent that started it)", c.repo, c.number),
        Err(PostRefusal::PosterFull(n)) => anyhow::bail!(
            "you have {n} cards waiting (at most {MAX_OPEN_PER_POSTER}); withdraw some with withdraw_pr first"),
        Err(PostRefusal::Full(n)) => anyhow::bail!("{n} cards are waiting in Muster (at most {MAX_OPEN}); try again once some are merged"),
        Err(PostRefusal::Changed) => anyhow::bail!("{} #{}'s card changed while you posted; post it again", c.repo, c.number),
    };
    forget_old_pictures(hub, &pr);
    publish(hub, &pr);
    let to = match (pr.poster.agent_id, hub.agents().by_session(caller)) {
        (Some(_), Some(a)) => format!("you, the agent {}, by name: also after a restart or a fresh start", a.name),
        _ => match pr.poster.parent_id {
            Some(p) => format!("this session ({caller}); if it has ended by then, the session that started it ({p})"),
            None => format!("this session ({caller})"),
        },
    };
    let left_out = c.pictures.iter().enumerate().filter_map(|(i, x)| x.too_tall.map(|s| format!(
        "picture {} ({}): {} × {} px, too tall to read in the human's window (more than {MAX_TALL} times as tall as wide), \
        so it is left out of the card. Post again with a crop of one screen (a phone: about 390 × 844) or a few such crops.",
        i + 1, x.pic.path, s.width, s.height))).collect::<Vec<_>>();
    let mut out = json!({
        "card": format!("P{}", pr.id),
        "repo": pr.repo,
        "number": pr.number,
        "head_sha": pr.head_sha,
        "status": pr.status,
        "replaced": replaced,
        "result_goes_to": to,
        "note": format!("{} as card P{} in the human's PRs to merge tab. Nothing waits for it: carry on. Muster reads the \
            pull request from GitHub when the human opens the tab, and merges only commit {} with the human's click. You get a \
            message that starts \"Merge result for {} #{}\" (merged, or not and why), or \"Changes asked on\" with the human's \
            words. If you push new commits, the card goes stale and Merge is off: post it again with post_pr (new commit, new \
            pictures); withdraw_pr takes it back.",
            if replaced { "Posted again" } else { "Posted" }, pr.id, &pr.head_sha[..7], pr.repo, pr.number),
    });
    if !left_out.is_empty() {
        out["note"] = json!(format!("{} {} of your pictures left out (see left_out): the human sees a line saying so in \
            its place.", out["note"].as_str().unwrap_or_default(), left_out.len()));
        out["left_out"] = json!(left_out);
    }
    Ok(out)
}

/// Write post `pr.post`'s pictures into its own directory (a later post never overwrites them).
/// On failure the directory goes again.
fn store_pictures(hub: &Hub, pr: &Pr, c: &Checked) -> anyhow::Result<()> {
    if c.pictures.is_empty() {
        return Ok(());
    }
    let dir = hub.store().pr_dir(pr.id).join(pr.post.to_string());
    let written = (|| {
        crate::paths::ensure_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for (pic, x) in pr.pictures.iter().zip(&c.pictures).filter(|(p, _)| p.too_tall.is_none()) {
            let data = &x.data;
            let file = hub.store().pr_dir(pr.id).join(&pic.file);
            crate::paths::write_private(&file, data).with_context(|| format!("writing {}", file.display()))?;
        }
        anyhow::Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    written.context("Muster couldn't keep the pictures")
}

/// `me` may post `poster`'s card again or withdraw it: it is the poster, or the poster is gone (a
/// session that has ended, or a named agent that was removed) and `me` is what started it (the
/// session, or its named agent), which is where the poster's messages went.
fn may_act(hub: &Hub, poster: &Who, me: &Who) -> bool {
    if crate::relay::same(poster, me) {
        return true;
    }
    let gone = match poster.agent_id {
        Some(id) => crate::relay::agent_then(hub, id, poster.agent_created_at).is_none(),
        None => hub.session(poster.session_id).is_none_or(|s| s.state == AgentState::Exited),
    };
    gone && (poster.parent_id == Some(me.session_id)
        || (poster.parent_agent_id.is_some() && poster.parent_agent_id == me.agent_id
            && poster.parent_agent_created_at == me.agent_created_at))
}

/// The pictures of earlier posts of `pr` (best effort: they are only shown for the current post).
fn forget_old_pictures(hub: &Hub, pr: &Pr) {
    let Ok(entries) = std::fs::read_dir(hub.store().pr_dir(pr.id)) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_str().and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n < pr.post)
            && e.file_type().is_ok_and(|t| t.is_dir())
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// MCP `withdraw_pr`: only the poster, only while its card is open (not while it is merged).
pub fn withdraw(hub: &Hub, caller: SessionId, repo: &str, number: u64, reason: &str) -> anyhow::Result<Value> {
    let me = crate::relay::who(hub, caller).context("calling session no longer exists")?;
    let reason = clean(reason);
    anyhow::ensure!((5..=500).contains(&reason.chars().count()), "reason: say in a few words why (5 to 500 characters)");
    let open = hub.store().open_pr(repo.trim(), number)?
        .with_context(|| format!("no card for {} #{number} is open", repo.trim()))?;
    // Outside the store's lock, as in `post`; then checked to be the same card and poster.
    let may = may_act(hub, &open.poster, &me);
    let (pr, ()) = hub.store().update_pr(open.id, |p| {
        anyhow::ensure!(p.post == open.post && crate::relay::same(&p.poster, &open.poster),
            "the card for {} #{} changed meanwhile; try again", p.repo, p.number);
        anyhow::ensure!(may,
            "{} posted that card, not you: only its poster withdraws it (or, once that session has ended, what started it)", p.poster.label);
        anyhow::ensure!(p.status != Status::Merging, "the human is merging it right now; wait for the result message");
        anyhow::ensure!(OPEN_STATUSES.contains(&p.status), "card P{} is {} already", p.id, p.status.as_str());
        p.status = Status::Withdrawn;
        p.withdrawn = Some(Ended { at: now_ms(), reason });
        Ok(())
    })?;
    // A withdrawn card is never shown again: its pictures go (posting and withdrawing again and
    // again can't fill the disk).
    let _ = std::fs::remove_dir_all(hub.store().pr_dir(pr.id));
    publish(hub, &pr);
    Ok(json!({"card": format!("P{}", pr.id), "repo": pr.repo, "number": pr.number, "status": pr.status}))
}

/// The MCP pull request tools, run as session `caller` (src/mcp/tools.rs checks the rate limit).
pub fn tool(hub: &Arc<Hub>, caller: SessionId, name: &str, args: Value) -> anyhow::Result<Value> {
    match name {
        "post_pr" => post(hub, caller, args),
        "withdraw_pr" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct P {
                repo: String,
                number: u64,
                reason: String,
            }
            let p: P = serde_json::from_value(args)?;
            withdraw(hub, caller, &p.repo, p.number, &p.reason)
        }
        _ => anyhow::bail!("unknown pull request tool {name}"),
    }
}

// ----- the window -----

/// Tell the human's windows that `pr` changed (`pr_upserted`, the card as `pr.get` returns it).
/// Devices don't get it (`rpc::forwarded`), as with questions.
pub fn publish(hub: &Hub, pr: &Pr) {
    hub.emit(crate::hub::ServerEvent::PrUpserted(view(pr)));
}

/// A card as the window shows it: the record, with its number, without the words sent to the
/// poster (their delivery stays).
pub fn view(pr: &Pr) -> Value {
    let mut v = serde_json::to_value(pr).unwrap_or(Value::Null);
    v["card"] = json!(format!("P{}", pr.id));
    if let Some(out) = v.get_mut("outbox").and_then(Value::as_array_mut) {
        for o in out {
            if let Some(m) = o.as_object_mut() { m.remove("text"); }
        }
    }
    v
}

/// `pr.list`: the open cards the tab shows (waiting, merging, stale; oldest first) and the newest
/// `DONE_LIMIT` merged or closed ones.
pub fn list(hub: &Hub) -> anyhow::Result<Value> {
    let open = hub.store().prs(&[Status::Waiting, Status::Merging, Status::Stale], MAX_OPEN)?;
    let done = hub.store().prs(&[Status::Merged, Status::Closed], DONE_LIMIT)?;
    Ok(json!({"prs": open.iter().chain(done.iter()).map(view).collect::<Vec<_>>()}))
}

pub fn get(hub: &Hub, id: i64) -> anyhow::Result<Value> {
    let pr = hub.store().pr(id)?.with_context(|| format!("no pull request card P{id}"))?;
    Ok(view(&pr))
}

/// `pr.picture`: picture `index` (from 0) of post `post` of card `id` (the one the window shows),
/// as a data URI, from Muster's own copy.
pub fn picture(hub: &Hub, id: i64, post: u32, index: usize) -> anyhow::Result<Value> {
    use base64::Engine;
    let pr = hub.store().pr(id)?.with_context(|| format!("no pull request card P{id}"))?;
    anyhow::ensure!(pr.post == post, "card P{id} was posted again since: look at it again");
    let p = pr.pictures.get(index).with_context(|| format!("card P{id} has no picture {}", index + 1))?;
    // Too tall to read: never sent, whether post_pr left it out or the card was stored before that
    // rule (its copy is measured here).
    let tall = |size: Size| json!({"too_tall": size, "caption": p.caption});
    if let Some(size) = p.too_tall {
        return Ok(tall(size));
    }
    let (mime, bytes) = crate::panel_apps::read_image(&hub.store().pr_dir(id), &p.file)?;
    if let Some(size) = too_tall(mime, &bytes) {
        return Ok(tall(size));
    }
    Ok(json!({
        "src": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)),
        "caption": p.caption,
    }))
}

/// The `gh` the daemon runs: `gh` on its PATH (`MUSTER_TEST_GH` in a debug build's tests).
pub fn gh(hub: &Hub) -> Arc<Gh> {
    let mut program: std::ffi::OsString = "gh".into();
    if cfg!(debug_assertions)
        && let Ok(g) = std::env::var("MUSTER_TEST_GH")
        && !g.is_empty()
    {
        program = g.into();
    }
    let cwd = hub.store().prs_root();
    let _ = crate::paths::ensure_private_dir(&cwd);
    Arc::new(Gh::new(program, cwd))
}

/// Read card `repo` #`number` from GitHub. The read's time (`at`, kept with what it found or
/// with its error) is when it started, so a slow read never overwrites one that started later.
async fn read_gh(gh: &Arc<Gh>, repo: &str, number: u64) -> Result<Read, ReadError> {
    let (gh, repo, at) = (gh.clone(), repo.to_string(), now_ms());
    match tokio::task::spawn_blocking(move || gh.view(&repo, number)).await {
        Ok(Ok(v)) => Ok(read_view(&v, at)),
        Ok(Err(e)) => Err(ReadError { at, text: gh_words(&e) }),
        Err(e) => Err(ReadError { at, text: format!("the GitHub read stopped: {e}") }),
    }
}

/// GitHub reads under way, by hub and card: a card is read by one refresh at a time, whichever
/// window asked, and at most `PARALLEL_READS` cards at once in all.
fn reading() -> &'static std::sync::Mutex<std::collections::HashSet<(usize, i64)>> {
    static READING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(usize, i64)>>> = std::sync::OnceLock::new();
    READING.get_or_init(Default::default)
}

fn read_gate() -> Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(PARALLEL_READS))).clone()
}

/// `pr.refresh`: read card `id`'s pull request again (None: every waiting, stale or changes-asked
/// card), when its last read is older than `REFRESH_MS` (`force`, Check again: `FORCED_REFRESH_MS`)
/// and no read of it is under way. Returns what `get` or `list` returns.
pub async fn refresh(hub: &Arc<Hub>, gh: Arc<Gh>, id: Option<i64>, force: bool) -> anyhow::Result<Value> {
    let cards = match id {
        Some(id) => vec![hub.store().pr(id)?.with_context(|| format!("no pull request card P{id}"))?],
        None => hub.store().prs(&[Status::Waiting, Status::Stale, Status::ChangesAsked], MAX_OPEN)?,
    };
    let now = now_ms();
    let gap = if force { FORCED_REFRESH_MS } else { REFRESH_MS };
    let key = Arc::as_ptr(hub) as usize;
    let due: Vec<Pr> = {
        let mut busy = reading().lock().unwrap_or_else(|e| e.into_inner());
        cards.into_iter().filter(|p| matches!(p.status, Status::Waiting | Status::Stale | Status::ChangesAsked)
            && { let last = p.github.as_ref().map_or(0, |r| r.at).max(p.read_error.as_ref().map_or(0, |e| e.at));
                 last > now || now - last >= gap }
            && busy.insert((key, p.id))).collect()
    };
    let mut tasks = Vec::new();
    for p in due {
        let (hub, gh) = (hub.clone(), gh.clone());
        // Each in its own task, so a window that goes away doesn't leave a read half recorded.
        tasks.push(tokio::spawn(async move {
            let gate = read_gate();
            let r = {
                let _slot = gate.acquire().await;
                read_gh(&gh, &p.repo, p.number).await
            };
            if let Err(e) = apply_read(&hub, p.id, p.post, r).await {
                tracing::warn!(card = p.id, "recording a GitHub read: {e:#}");
            }
            reading().lock().unwrap_or_else(|e| e.into_inner()).remove(&(key, p.id));
        }));
    }
    match id {
        // Check again: this card's read, waited for.
        Some(id) => {
            for t in tasks {
                let _ = t.await;
            }
            get(hub, id)
        }
        // Every card: the reads go on, their results come as `pr_upserted`.
        None => {
            prune_done_pictures(hub);
            list(hub)
        }
    }
}

/// The pictures of merged and closed cards past the newest `DONE_LIMIT` (no window shows them).
fn prune_done_pictures(hub: &Hub) {
    let Ok(done) = hub.store().prs(&[Status::Merged, Status::Closed], 100_000) else { return };
    for p in done.iter().take(done.len().saturating_sub(DONE_LIMIT)) {
        let _ = std::fs::remove_dir_all(hub.store().pr_dir(p.id));
    }
}

/// Record read `r` of card `id` as it was posted at `post`: the rules, and what it says about the
/// card (merged by this card's click, merged or closed elsewhere, new commits). Dropped when it
/// is of an earlier post, or older than what the card holds (a merge does its own reads). Only
/// while the card waits or is stale; a card the human asked changes on is only closed by it.
async fn apply_read(hub: &Arc<Hub>, id: i64, post: u32, r: Result<Read, ReadError>) -> anyhow::Result<()> {
    let (pr, notes) = hub.store().update_pr(id, |p| {
        if p.post != post || !matches!(p.status, Status::Waiting | Status::Stale | Status::ChangesAsked) {
            return Ok(false);
        }
        let at = match &r { Ok(read) => read.at, Err(e) => e.at };
        let newest = p.github.as_ref().map_or(0, |g| g.at).max(p.read_error.as_ref().map_or(0, |e| e.at))
            .max(p.refused.as_ref().map_or(0, |f| f.at));
        // (A newest time ahead of the clock, after the clock was set back, never blocks a read.)
        if at <= newest && newest <= now_ms() {
            return Ok(false);
        }
        let before = p.outbox.len();
        match r {
            Err(e) => p.read_error = Some(e),
            Ok(read) => {
                let v = verdict(&read, &p.head_sha);
                // A merge of exactly the commit this card's click sent to GitHub, whose result
                // Muster never read back (gh timed out, the read failed, a restart): that click's.
                let ours = p.merge.as_ref().is_some_and(|m| m.sent_at.is_some() && m.sha == read.head_sha && m.commit.is_none())
                    && p.refused.as_ref().is_none_or(|f| f.unknown);
                if read.state == "MERGED" && ours && p.status != Status::ChangesAsked {
                    let commit = read.merge_commit.clone();
                    p.read_error = None;
                    p.github = Some(read);
                    p.verdict = Some(v);
                    merged(p, commit);
                } else if read.state == "MERGED" || read.state == "CLOSED" {
                    p.status = Status::Closed;
                    p.closed = Some(Ended { at: read.at, reason: if read.state == "MERGED" {
                        "merged on GitHub, not from this card".into()
                    } else {
                        "closed on GitHub".into()
                    } });
                    p.read_error = None;
                    p.github = Some(read);
                    p.verdict = Some(v);
                } else if p.status == Status::ChangesAsked {
                    // Still open: it waits for its poster to post it again (kept, so the next
                    // read waits its turn).
                    p.read_error = None;
                    p.github = Some(read);
                } else {
                    p.read_error = None;
                    let moved = !read.head_sha.is_empty() && read.head_sha != p.head_sha;
                    if moved {
                        p.status = Status::Stale;
                        if !p.stale_told {
                            p.stale_told = true;
                            push(p, NoteKind::Stale, "muster", stale_text(p, &read.head_sha));
                        }
                    } else if p.status == Status::Stale {
                        p.status = Status::Waiting;
                    }
                    // A refused merge stays shown until a later read passes.
                    if v.ok && p.refused.as_ref().is_some_and(|f| f.at < read.at) {
                        p.refused = None;
                    }
                    p.github = Some(read);
                    p.verdict = Some(v);
                }
            }
        }
        Ok(p.outbox.len() > before)
    })?;
    publish(hub, &pr);
    if notes {
        send_notes(hub, id).await;
    }
    Ok(())
}

fn push(p: &mut Pr, kind: NoteKind, from: &str, text: String) {
    // Numbered, so two notes with the same words are never taken for one after a restart.
    let text = format!("{text}\n(Muster's note {} on card P{}.)", p.outbox.len() + 1, p.id);
    // A result of the human's click (merged, refused, changes asked) is the human's own (no route:
    // origin human, hop 0); a stale notice, which no click caused, carries the poster's own
    // provenance from when it posted.
    let route = (kind == NoteKind::Stale).then_some(crate::agents::Prov { origin: p.origin, hop: p.hop, msg_id: None });
    p.outbox.push(Note { kind, at: now_ms(), from: from.into(), text, delivery: Delivery::pending(None), to: Some(p.poster.clone()), route });
}

fn short(sha: &str) -> &str { &sha[..sha.len().min(7)] }

fn local_time(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%-d %b %H:%M").to_string())
        .unwrap_or_else(|_| ms.to_string())
}

/// The card, as the end of every message to the poster: enough for a poster with a new
/// conversation to know which one it was.
fn card_lines(p: &Pr) -> String {
    format!("Card P{}, posted {} by {} for commit {}. What it changes: {}\nAfter the merge, it said: {}\n",
        p.id, local_time(p.created_at), p.poster.label, p.head_sha, p.summary, p.after_merge)
}

fn stale_text(p: &Pr, newest: &str) -> String {
    format!("Changed since you posted it: {} #{} has commits you didn't post (GitHub's newest is {}; the card is for {}). \
        Merge is off for it, so nothing is merged. Post it again with post_pr (the new head_sha, new pictures), or withdraw_pr \
        if it isn't meant to be merged.\n\n{}", p.repo, p.number, short(newest), short(&p.head_sha), card_lines(p))
}

/// Mark `p` merged by its click, as commit `commit`, and tell its poster.
fn merged(p: &mut Pr, commit: Option<String>) {
    let at = now_ms();
    p.status = Status::Merged;
    p.refused = None;
    let m = p.merge.get_or_insert(MergeTry { at, sha: p.head_sha.clone(), post: p.post, base: String::new(), sent_at: Some(at),
        done_at: None, commit: None });
    m.done_at = Some(at);
    m.commit = commit.clone();
    let base = p.github.as_ref().map(|r| r.base.clone()).filter(|b| !b.is_empty()).unwrap_or_else(|| "the base branch".into());
    let clicked = local_time(p.merge.as_ref().map_or(at, |m| m.at));
    let text = format!("Merge result for {} #{}: merged.\nThe human clicked Merge in Muster's PRs to merge tab ({clicked}). \
        Muster merged commit {} into {base} as a merge commit{}; Muster didn't delete the branch (GitHub does if the repo is set \
        to delete merged branches). Do what comes after the merge now: \"{}\"\n\n{}",
        p.repo, p.number, p.head_sha, commit.map(|c| format!(" {c}")).unwrap_or_default(), p.after_merge, card_lines(p));
    push(p, NoteKind::Merged, "muster", text);
}

/// GitHub's address for the read is `repo` #`number` (names ignore case): a renamed or moved repo
/// gets a card of its own, posted again under its new name.
fn same_pr(url: &str, repo: &str, number: u64) -> bool {
    url.eq_ignore_ascii_case(&format!("https://github.com/{repo}/pull/{number}"))
}

/// gh's words as the poster gets them: the first line, without gh's suggestions of other flags
/// (`--admin`, `--auto`: only the human's click merges). The card keeps them whole for the human.
fn said_to_poster(said: &str) -> String {
    let first = said.lines().map(str::trim).find(|l| !l.is_empty() && !l.contains("--admin") && !l.contains("--auto")).unwrap_or("");
    clip(first, 300)
}

/// How a merge the human started ended without merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// It didn't merge: Muster refused before the write, or GitHub refused it.
    Refused,
    /// Muster asked GitHub to merge but couldn't confirm the result: it may still merge.
    Unknown,
}

/// Mark `p`'s merge as not merged (`ending`) for `reason`, settle its status from `read` (when
/// there is one), and tell its poster.
fn refused(p: &mut Pr, ending: Ending, reason: String, github_said: Option<String>, read: Option<Read>) {
    let at = now_ms();
    if let Some(m) = p.merge.as_mut() {
        m.done_at = Some(at);
        // Settled: it didn't merge, so a merge found later is someone else's.
        if ending == Ending::Refused { m.sent_at = None; }
    }
    p.status = Status::Waiting;
    if let Some(r) = read {
        let v = verdict(&r, &p.head_sha);
        if (r.state == "MERGED" || r.state == "CLOSED") && ending == Ending::Refused {
            p.status = Status::Closed;
            p.closed = Some(Ended { at, reason: if r.state == "MERGED" { "merged on GitHub, not from this card".into() } else { "closed on GitHub".into() } });
        } else if !r.head_sha.is_empty() && r.head_sha != p.head_sha {
            p.status = Status::Stale;
            // This message says so: no second one.
            p.stale_told = true;
        }
        p.github = Some(r);
        p.verdict = Some(v);
    }
    let said = github_said.as_deref().map(said_to_poster).filter(|s| !s.is_empty())
        .map(|s| format!(" GitHub said: \"{s}\"")).unwrap_or_default();
    let clicked = local_time(p.merge.as_ref().map_or(at, |m| m.at));
    let text = match ending {
        Ending::Refused => format!("Merge result for {} #{}: not merged: {reason}.\nThe human clicked Merge in Muster's PRs to merge \
            tab ({clicked}).{said} Nothing was merged. Don't merge it yourself: only the human's click in Muster merges. Once it is \
            fixed, post the card again with post_pr (the same repo and number replace it).\n\n{}",
            p.repo, p.number, card_lines(p)),
        Ending::Unknown => format!("Merge result for {} #{}: not known yet: {reason}.\nThe human clicked Merge in Muster's PRs to \
            merge tab ({clicked}) and Muster asked GitHub to merge commit {}, but couldn't confirm the result.{said} It may still be \
            merged. Don't push to its branch or merge it yourself. Muster reads it again when the human opens the tab or clicks \
            Check again; if it merged you get \"Merge result for {} #{}: merged\".\n\n{}",
            p.repo, p.number, p.head_sha, p.repo, p.number, card_lines(p)),
    };
    p.refused = Some(Refusal { at, reason, github_said, unknown: ending == Ending::Unknown, not_told: false });
    push(p, NoteKind::Refused, "muster", text);
}

/// Refusal for `pr.merge` and the other card actions from anywhere but a click in the window.
pub const MERGE_BY_CLICK: &str = "only a click in Muster's window merges a pull request, asks for changes or sets one aside: \
    open the PRs to merge tab there";

/// `pr.merge` (only a click in Muster's window gets here: `rpc::pr_from_window`): merge card `id`,
/// which the human saw at post `post` for commit `sha` into branch `base`. Claimed in one
/// transaction (a second click, or another window, is refused); returns the card, merging, at
/// once. The merge itself runs in its own task (a window that goes away can't stop it halfway),
/// and its result comes as `pr_upserted`.
pub fn merge(hub: &Arc<Hub>, gh: Arc<Gh>, id: i64, sha: &str, base: &str, post: u32) -> anyhow::Result<Value> {
    let sha = sha.trim().to_ascii_lowercase();
    let (pr, ()) = hub.store().update_pr(id, |p| {
        match p.status {
            Status::Waiting => {}
            Status::Merging => anyhow::bail!("{} #{} is being merged already", p.repo, p.number),
            Status::Merged => anyhow::bail!("{} #{} was merged already", p.repo, p.number),
            Status::Stale => anyhow::bail!("{} #{} has new commits since it was posted: Merge is off until it is posted again",
                p.repo, p.number),
            Status::ChangesAsked => anyhow::bail!("you asked for changes on {} #{}: it comes back when it is posted again",
                p.repo, p.number),
            Status::Withdrawn | Status::Closed => anyhow::bail!("{} #{} is no longer waiting ({})", p.repo, p.number, p.status.as_str()),
        }
        anyhow::ensure!(p.post == post && p.head_sha == sha,
            "the card for {} #{} changed since you looked (it was posted again): look at it again", p.repo, p.number);
        let seen = p.github.as_ref().map(|r| r.base.as_str()).unwrap_or("");
        anyhow::ensure!(!seen.is_empty() && seen == base,
            "the card for {} #{} changed since you looked (GitHub's base branch is {:?} now): look at it again", p.repo, p.number, seen);
        p.status = Status::Merging;
        p.merge = Some(MergeTry { at: now_ms(), sha: sha.clone(), post, base: base.to_string(), sent_at: None, done_at: None, commit: None });
        Ok(())
    })?;
    publish(hub, &pr);
    let h = hub.clone();
    tokio::spawn(async move {
        let h2 = h.clone();
        let done = tokio::spawn(async move { run_merge(&h2, &gh, id).await }).await;
        let why = match done {
            Ok(Ok(_)) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(e) => format!("the merge stopped: {e}"),
        };
        // Never left in Merging (an error, or the task itself failed): back to waiting, with why.
        // Had gh been started, the result is unknown, and the next read finds out.
        let ended = h.store().update_pr(id, |p| {
            if p.status != Status::Merging { return Ok(false) }
            let ending = if p.merge.as_ref().is_some_and(|m| m.sent_at.is_some()) { Ending::Unknown } else { Ending::Refused };
            refused(p, ending, format!("Muster couldn't finish it ({why})"), None, None);
            Ok(true)
        });
        match ended {
            Ok((pr, notes)) => {
                publish(&h, &pr);
                if notes { send_notes(&h, id).await; }
            }
            Err(e) => tracing::warn!(card = id, "recording a merge that stopped: {e:#}"),
        }
    });
    Ok(view(&pr))
}

/// The merge itself: read, check (the four rules, the base the human saw, no merge queue), `gh
/// pr merge`, read back. The card is Merging throughout.
async fn run_merge(hub: &Arc<Hub>, gh: &Arc<Gh>, id: i64) -> anyhow::Result<Pr> {
    let pr = hub.store().pr(id)?.context("the card is gone")?;
    let tried = pr.merge.clone().context("the card has no merge under way")?;
    let sha = tried.sha.clone();
    let finish = |hub: &Arc<Hub>, edit: Box<dyn FnOnce(&mut Pr) + Send>| -> anyhow::Result<Pr> {
        let (pr, ()) = hub.store().update_pr(id, |p| {
            if p.status == Status::Merging { edit(p) }
            Ok(())
        })?;
        publish(hub, &pr);
        Ok(pr)
    };
    let refuse = |reason: String, said: Option<String>, read: Option<Read>| -> Box<dyn FnOnce(&mut Pr) + Send> {
        Box::new(move |p| refused(p, Ending::Refused, reason, said, read))
    };
    // 1. Read it again: every rule must pass now, whatever the tab showed.
    let read = match read_gh(gh, &pr.repo, pr.number).await {
        Err(e) => {
            let pr = finish(hub, refuse("Muster couldn't read the pull request on GitHub".into(), Some(e.text), None))?;
            send_notes(hub, id).await;
            return Ok(pr);
        }
        Ok(read) => read,
    };
    let v = verdict(&read, &sha);
    let blocked = if let Some(reason) = v.reason.clone() {
        Some((reason, None))
    } else if !same_pr(&read.url, &pr.repo, pr.number) {
        Some((format!("GitHub knows it by another name now ({}); post the card again with that name", read.url), None))
    } else if read.base != tried.base {
        Some((format!("its base branch changed to {} since you looked (you saw {})", read.base, tried.base), None))
    } else {
        // 2. A pull request that merges through a merge queue: gh would queue it or turn on
        // auto-merge (GitHub then merges it later, its own way), not merge it now.
        let (g, repo, number) = (gh.clone(), pr.repo.clone(), pr.number);
        let unread = "Muster couldn't check on GitHub whether it merges through a merge queue".to_string();
        match tokio::task::spawn_blocking(move || g.queue(&repo, number)).await {
            Ok(Ok((false, false))) => None,
            Ok(Ok(_)) => Some(("it merges through a merge queue on GitHub, which Muster doesn't use: merge it there".to_string(), None)),
            Ok(Err(e)) => Some((unread, Some(gh_words(&e)))),
            Err(e) => Some((unread, Some(e.to_string()))),
        }
    };
    if let Some((reason, said)) = blocked {
        let pr = finish(hub, refuse(reason, said, Some(read)))?;
        send_notes(hub, id).await;
        return Ok(pr);
    }
    let (stored, ()) = hub.store().update_pr(id, |p| {
        anyhow::ensure!(p.status == Status::Merging, "the card is no longer being merged");
        p.github = Some(read);
        p.verdict = Some(v);
        if let Some(m) = p.merge.as_mut() { m.sent_at = Some(now_ms()); }
        Ok(())
    })?;
    publish(hub, &stored);
    // 3. The one write.
    let (g, repo, number, s) = (gh.clone(), stored.repo.clone(), stored.number, sha.clone());
    let out = tokio::task::spawn_blocking(move || g.merge(&repo, number, &s)).await
        .unwrap_or_else(|e| Err((GhError::Other(format!("the merge call stopped: {e}")), true)));
    // 4. Read it back (GitHub may take a moment to show it merged).
    let mut back = read_gh(gh, &stored.repo, stored.number).await;
    for _ in 1..RESULT_READS {
        if matches!(out, Err((_, false))) || matches!(&back, Ok(r) if r.state == "MERGED") { break }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        back = read_gh(gh, &stored.repo, stored.number).await;
    }
    let pr = match back {
        Ok(r) if r.state == "MERGED" && r.head_sha == sha => {
            let commit = r.merge_commit.clone();
            finish(hub, Box::new(move |p| {
                p.github = Some(r);
                merged(p, commit);
            }))?
        }
        Ok(r) => match out {
            // gh said no, and GitHub shows it unmerged: a refusal, in GitHub's words.
            Err((e, false)) => {
                let reason = match verdict(&r, &sha).reason {
                    Some(why) if r.state != "MERGED" => why,
                    _ => "GitHub refused the merge".to_string(),
                };
                finish(hub, refuse(reason, Some(gh_words(&e)), Some(r)))?
            }
            // gh timed out, or said yes and GitHub still shows it open: it may merge later.
            other => {
                let said = other.err().map(|(e, _)| gh_words(&e));
                let reason = if said.is_some() { "gh didn't answer in time" } else {
                    "GitHub accepted the merge but hasn't merged it (it may have queued it or turned on auto-merge)"
                }.to_string();
                finish(hub, Box::new(move |p| refused(p, Ending::Unknown, reason, said, Some(r))))?
            }
        },
        Err(e) => {
            // gh may have merged it: the next read of the card (Check again) finds out, and a
            // merge of this commit counts as this card's (`apply_read`).
            let said = Some(out.err().map(|(o, _)| gh_words(&o)).unwrap_or(e.text));
            finish(hub, Box::new(move |p| refused(p, Ending::Unknown,
                "Muster couldn't read the result back from GitHub; Check again shows whether it merged".into(), said, None)))?
        }
    };
    send_notes(hub, id).await;
    Ok(hub.store().pr(id)?.unwrap_or(pr))
}

/// `pr.ask_changes`: the human's words go to the poster; the card leaves the list until it is
/// posted again.
pub async fn ask_changes(hub: &Arc<Hub>, id: i64, post: u32, words: &str) -> anyhow::Result<Value> {
    let words = clean(words);
    anyhow::ensure!(!words.is_empty() && words.chars().count() <= 4000, "say what to change (1 to 4000 characters)");
    let (pr, ()) = hub.store().update_pr(id, |p| {
        anyhow::ensure!(p.post == post, "{} #{} was posted again since you looked: look at it again", p.repo, p.number);
        anyhow::ensure!(matches!(p.status, Status::Waiting | Status::Stale),
            "{} #{} is {}: changes are asked of a waiting card", p.repo, p.number, p.status.as_str());
        let at = now_ms();
        p.status = Status::ChangesAsked;
        p.changes = Some(Changes { at, words: words.clone() });
        let text = format!("Changes asked on {} #{}: the human's words, from Muster's PRs to merge tab ({}): \"{words}\"\n\
            The card left the list and nothing was merged. Once it is changed and pushed, post it again with post_pr (the new \
            head_sha, new pictures).\n\n{}", p.repo, p.number, local_time(at), card_lines(p));
        push(p, NoteKind::ChangesAsked, "human", text);
        Ok(())
    })?;
    publish(hub, &pr);
    let h = hub.clone();
    tokio::spawn(async move { send_notes(&h, id).await });
    Ok(view(&pr))
}

/// `pr.later`: set a waiting card aside ("Not now": it moves down the list), or back.
pub fn later(hub: &Hub, id: i64, post: u32, on: bool) -> anyhow::Result<Value> {
    let (pr, ()) = hub.store().update_pr(id, |p| {
        anyhow::ensure!(p.post == post, "{} #{} was posted again since you looked: look at it again", p.repo, p.number);
        anyhow::ensure!(matches!(p.status, Status::Waiting | Status::Stale), "{} #{} is {}", p.repo, p.number, p.status.as_str());
        p.later = on.then(now_ms);
        Ok(())
    })?;
    publish(hub, &pr);
    Ok(view(&pr))
}

// ----- messages to the poster -----

/// The prefix of note `index`'s first line in a session that isn't a named agent.
fn note_prefix(id: i64, index: usize) -> String {
    format!("[muster pull request P{id} note {index} from ")
}

fn note_envelope(id: i64, index: usize, from: &str, origin: Origin, hop: u32) -> String {
    let from: String = from.chars()
        .map(|c| if c.is_whitespace() || c == ']' || c == '[' || c.is_control() { '_' } else { c })
        .collect();
    format!("{}{from} origin={} hop={hop}]", note_prefix(id, index), origin.as_str())
}

/// The provenance note `n` travels with (`push`; a note from before `route` was kept: the card's).
fn note_prov(pr: &Pr, n: &Note) -> Option<crate::agents::Prov> {
    n.route.or_else(|| (n.kind == NoteKind::Stale && n.to.is_none())
        .then_some(crate::agents::Prov { origin: pr.origin, hop: pr.hop, msg_id: None }))
}

/// Who note `n` is for.
fn note_to<'a>(pr: &'a Pr, n: &'a Note) -> &'a Who {
    n.to.as_ref().unwrap_or(&pr.poster)
}

/// Note `n`'s words for whoever gets them (`note`: why that is the poster's lead).
fn note_text(pr: &Pr, n: &Note, note: Option<&str>) -> String {
    let t = match note {
        Some(why) => format!("{why}. The pull request below was posted by {}, not you.\n{}", note_to(pr, n).label, n.text),
        None => n.text.clone(),
    };
    plain(&t)
}

fn letter<T>(pr: &Pr, index: usize, run: impl FnOnce(&crate::relay::Letter<'_>) -> T) -> T {
    let n = &pr.outbox[index];
    let text = |note: Option<&str>| note_text(pr, n, note);
    let id = pr.id;
    let envelope = move |from: &str, origin: Origin, hop: u32| note_envelope(id, index, from, origin, hop);
    run(&crate::relay::Letter {
        poster: note_to(pr, n), recorder: None, from: n.from.clone(), from_session: None, prov: note_prov(pr, n),
        text: &text, envelope: &envelope, prefix: note_prefix(pr.id, index),
        // The cold-answer rule covers answers to questions only (src/relay.rs).
        fresh: None,
    })
}

/// Deliver card `id`'s pending notes, each claimed in one transaction first.
pub async fn send_notes(hub: &Arc<Hub>, id: i64) {
    loop {
        let claimed = hub.store().update_pr(id, |p| {
            Ok(p.outbox.iter().position(|n| n.delivery.status == DeliveryStatus::Pending).inspect(|&i| {
                p.outbox[i].delivery.status = DeliveryStatus::Delivering;
                p.outbox[i].delivery.note = None;
            }))
        });
        let (pr, index) = match claimed {
            Ok((pr, Some(i))) => (pr, i),
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(card = id, "claiming a message for a card's poster: {e:#}");
                return;
            }
        };
        let done = {
            let n = &pr.outbox[index];
            let text = |note: Option<&str>| note_text(&pr, n, note);
            let envelope = |from: &str, origin: Origin, hop: u32| note_envelope(id, index, from, origin, hop);
            let l = crate::relay::Letter {
                poster: note_to(&pr, n), recorder: None, from: n.from.clone(), from_session: None, prov: note_prov(&pr, n),
                text: &text, envelope: &envelope, prefix: note_prefix(id, index), fresh: None,
            };
            // A stale notice goes to the poster itself or nowhere: it carries the poster's own words,
            // and no click of the human's sent it, so it is never passed on to a lead.
            let live: std::collections::HashSet<SessionId> = hub.live_session_ids().into_iter().collect();
            let elsewhere = n.kind == NoteKind::Stale && match crate::relay::target(hub, l.poster, None, &live) {
                crate::relay::Target::Agent(_, note) | crate::relay::Target::Session(_, note) => note.is_some(),
                _ => false,
            };
            if elsewhere {
                crate::relay::outcome(DeliveryStatus::Undelivered,
                    "not delivered: the poster has ended, and Muster doesn't pass a stale notice on to its lead".into())
            } else {
                crate::relay::deliver(hub, &l).await
            }
        };
        let interrupted = done.status == DeliveryStatus::Pending;
        match hub.store().update_pr(id, |p| {
            if let Some(n) = p.outbox.get_mut(index)
                && n.delivery.status == DeliveryStatus::Delivering
            {
                n.delivery = done.clone();
            }
            Ok(())
        }) {
            Ok((pr, ())) => publish(hub, &pr),
            Err(e) => {
                tracing::warn!(card = id, "recording a message's delivery: {e:#}");
                return;
            }
        }
        // A shutdown cut it off: the next backend sends it.
        if interrupted {
            return;
        }
    }
}

// ----- restarts -----

/// What a previous backend left unfinished on a card: the messages it was delivering, and the
/// merge it was running (its click's time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    pub id: i64,
    pub delivering: Vec<usize>,
    pub merging: Option<i64>,
}

/// Cards a previous backend left unfinished (a merge under way, a message on its way). Read at
/// daemon start before the server takes requests, so none of this backend's own is among them.
pub fn unfinished(hub: &Hub) -> Vec<Unfinished> {
    let ids = hub.store().unfinished_prs().unwrap_or_else(|e| {
        tracing::warn!("reading unfinished pull request cards: {e:#}");
        Vec::new()
    });
    ids.into_iter().filter_map(|id| {
        let p = hub.store().pr(id).ok().flatten()?;
        Some(Unfinished {
            id,
            delivering: p.outbox.iter().enumerate()
                .filter(|(_, n)| n.delivery.status == DeliveryStatus::Delivering).map(|(i, _)| i).collect(),
            merging: (p.status == Status::Merging).then(|| p.merge.as_ref().map_or(0, |m| m.at)),
        })
    }).collect()
}

/// At daemon start: finish the cards in `cards` (`unfinished`).
pub async fn resume(hub: &Arc<Hub>, cards: Vec<Unfinished>) {
    let gh = gh(hub);
    for u in cards {
        if let Err(e) = resume_one(hub, &gh, &u).await {
            tracing::warn!(card = u.id, "finishing a pull request card: {e:#}");
        }
    }
}

async fn resume_one(hub: &Arc<Hub>, gh: &Arc<Gh>, u: &Unfinished) -> anyhow::Result<()> {
    let id = u.id;
    let Some(pr) = hub.store().pr(id)? else { return Ok(()) };
    // Only the merge the previous backend was running (a click on this backend since is its own).
    let ours = |p: &Pr| p.status == Status::Merging && u.merging.is_some_and(|at| p.merge.as_ref().map_or(0, |m| m.at) == at);
    if ours(&pr) {
        // A restart came during the merge: did it merge? GitHub knows.
        let sha = pr.merge.as_ref().map_or(pr.head_sha.clone(), |m| m.sha.clone());
        let sent = pr.merge.as_ref().is_some_and(|m| m.sent_at.is_some());
        let read = read_gh(gh, &pr.repo, pr.number).await;
        let (stored, ()) = hub.store().update_pr(id, |p| {
            if !ours(p) { return Ok(()) }
            match read {
                Ok(r) if r.state == "MERGED" && r.head_sha == sha && sent => {
                    let commit = r.merge_commit.clone();
                    p.github = Some(r);
                    merged(p, commit);
                }
                other if sent => {
                    // The write had gone to GitHub: the poster is told the result isn't known yet
                    // (the next read settles it, and a merge then found is this click's).
                    let (said, read) = match other { Ok(r) => (None, Some(r)), Err(e) => (Some(e.text), None) };
                    refused(p, Ending::Unknown, "Muster restarted while GitHub was merging it, and couldn't confirm the result".into(),
                        said, read);
                }
                other => {
                    // Cut off before the write: nothing was sent to GitHub, and the poster never
                    // heard of the click, so it isn't told.
                    p.status = Status::Waiting;
                    if let Some(m) = p.merge.as_mut() { m.done_at = Some(now_ms()); }
                    let (said, read) = match other { Ok(r) => (None, Some(r)), Err(e) => (Some(e.text), None) };
                    if let Some(r) = read {
                        p.verdict = Some(verdict(&r, &p.head_sha));
                        if r.state == "MERGED" || r.state == "CLOSED" {
                            p.status = Status::Closed;
                            p.closed = Some(Ended { at: r.at, reason: if r.state == "MERGED" {
                                "merged on GitHub, not from this card".into() } else { "closed on GitHub".into() } });
                        } else if !r.head_sha.is_empty() && r.head_sha != p.head_sha {
                            p.status = Status::Stale;
                        }
                        p.github = Some(r);
                    }
                    p.refused = Some(Refusal { at: now_ms(), reason: "the merge didn't start: Muster restarted first".into(),
                        github_said: said, unknown: false, not_told: true });
                }
            }
            Ok(())
        })?;
        publish(hub, &stored);
    }
    // Messages the previous backend was delivering when it stopped: what got through? (Only
    // those: one this backend has claimed since is its own.)
    let pr = hub.store().pr(id)?.context("the card is gone")?;
    for &i in &u.delivering {
        if pr.outbox.get(i).is_none_or(|n| n.delivery.status != DeliveryStatus::Delivering) { continue }
        let found = letter(&pr, i, |l| crate::relay::found_after_restart(hub, l))?;
        hub.store().update_pr(id, |p| {
            if let Some(n) = p.outbox.get_mut(i)
                && n.delivery.status == DeliveryStatus::Delivering
            {
                match found.clone() {
                    Some(d) => n.delivery = d,
                    None => n.delivery.status = DeliveryStatus::Pending,
                }
            }
            Ok(())
        })?;
    }
    send_notes(hub, id).await;
    Ok(())
}

#[cfg(test)]
#[path = "prs_tests.rs"]
mod tests;
