//! Phase 3 with a fake clock: agents' proposals (nothing runs before the human approves, and the
//! approval checks again), reminders (one-off, self, 24 hours, the limits that stop chains and
//! bursts), what an agent may pause or delete, the estimate, and the file-change trigger (the
//! owner's folders only, ctime cursor, debounce, an agent's own changes ignored).

use serde_json::{Value, json};

use super::super::plan::{self, Action};
use super::super::*;
use super::{Fixture, LONDON, at};
use crate::agents::{Origin, Prov};

const MIN: i64 = 60_000;

/// Link agent `name` to session `sid`, so tool calls from `sid` are that agent's.
fn as_agent(f: &Fixture, name: &str, sid: SessionId) {
    let a = f.hub.agents().get(name).unwrap();
    f.hub.agents().link_session(f.hub.store(), a.id, Some(sid)).unwrap();
}

fn tool(f: &Fixture, sid: SessionId, name: &str, args: Value) -> anyhow::Result<Value> {
    agent::tool(&f.hub, sid, name, args)
}

fn err(r: anyhow::Result<Value>) -> String {
    format!("{:#}", r.unwrap_err())
}

fn no_quiet_hours(f: &Fixture) {
    set_settings(&f.hub, serde_json::from_value(json!({"quiet_start": "00:00", "quiet_end": "00:00"})).unwrap()).unwrap();
}

// ----- proposals -----

#[test]
fn a_proposal_never_runs_until_the_human_approves_it() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let r = tool(&f, 41, "propose_schedule", json!({
        "prompt": "Summarise the Ideas note", "cron": "0 16 * * 5", "tz": LONDON,
        "reason": "You asked for a weekly summary",
    }))
    .unwrap();
    assert_eq!(r["status"], "proposed");
    let id = r["schedule_id"].as_i64().unwrap();
    let s = f.hub.schedules.get(id).unwrap();
    assert!(s.waiting_approval() && s.next_run.is_none(), "no time to run while it waits");
    let j = schedule_json(&f.hub, &s);
    assert_eq!((j["status"].as_str(), j["made_by"]["agent"].as_str(), j["made_by"]["kind"].as_str()), (Some("proposed"), Some("worker"), Some("proposal")));
    assert_eq!(j["made_by"]["reason"], "You asked for a weekly summary");
    assert_eq!(j["estimate"]["runs_per_week"], 1);
    assert!(j["estimate"]["text"].as_str().unwrap().contains("About 1 run in the coming week"), "{}", j["estimate"]["text"]);
    // Nothing makes it due: not the clock, not Run now, not Skip next, not an edit of its timing.
    f.set(at(LONDON, "2026-10-23T16:00"));
    assert!(f.tick().is_empty());
    assert!(plan::due(&Schedule { next_run: Some(0), ..s.clone() }, f.hub.schedules.now(), &Settings::default(), &super::quiet_london(), &Facts::default(), 0, None).is_none());
    assert!(run_now(&f.hub, id).unwrap_err().to_string().contains("approve it"));
    assert!(set_skip(&f.hub, id, true).is_err());
    update(&f.hub, serde_json::from_value(json!({"id": id, "when": {"kind": "cron", "cron": "0 17 * * 5"}})).unwrap()).unwrap();
    assert!(f.hub.schedules.get(id).unwrap().next_run.is_none(), "the human's edit keeps it waiting");
    assert!(matches!(plan::still_ok(&s, 0, &Settings::default(), &super::quiet_london(), &Facts::default(), 0, None), Some(Action::Skip(_))));
    // Approved: it runs like any schedule, from now on.
    let j = approve(&f.hub, id).unwrap();
    assert_eq!(j["status"], "active");
    let s = f.hub.schedules.get(id).unwrap();
    assert_eq!(s.next_run, Some(at(LONDON, "2026-10-23T17:00")));
    assert!(s.made_by.as_ref().unwrap().approved_at.is_some());
    assert!(approve(&f.hub, id).is_err(), "only once");
}

#[test]
fn approving_checks_the_proposal_again() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    let r = tool(&f, 41, "propose_schedule", json!({"prompt": "p", "in_minutes": 30, "reason": "follow up"})).unwrap();
    let id = r["schedule_id"].as_i64().unwrap();
    // Its time passed before the human looked: it can't be approved as it is.
    f.set(at(LONDON, "2026-10-19T11:00"));
    let e = approve(&f.hub, id).unwrap_err().to_string();
    assert!(e.contains("passed") && e.contains("dismiss"), "{e}");
    // A proposal for cleo (a Claude subscription agent that opted in), then the human takes the
    // opt-in back: approving is refused.
    let r = tool(&f, 41, "propose_schedule", json!({"agent": "cleo", "prompt": "p", "cron": "0 9 * * 1-5", "reason": "r"})).unwrap();
    let id = r["schedule_id"].as_i64().unwrap();
    let j = schedule_json(&f.hub, &f.hub.schedules.get(id).unwrap());
    assert_eq!(j["expires_at"], Value::Null, "the 7 days start at approval");
    assert!(j["estimate"]["text"].as_str().unwrap().contains("Claude subscription"), "{}", j["estimate"]["text"]);
    let edit: crate::agents::AgentUpdate = serde_json::from_value(json!({"name": "cleo", "accepts": ["human"]})).unwrap();
    crate::agents::update(&f.hub, edit).unwrap();
    assert!(approve(&f.hub, id).unwrap_err().to_string().contains("doesn't take scheduled work"));
}

#[test]
fn proposals_are_refused_from_unattended_work_and_for_agents_that_dont_take_it() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    no_quiet_hours(&f);
    as_agent(&f, "worker", 41);
    // Handling a scheduled run (or anything a schedule or trigger started): no new schedules.
    for origin in [Origin::Schedule, Origin::Webhook] {
        f.hub.agents().set_prov(41, Some(Prov { origin, hop: 0, msg_id: Some(1) })).unwrap();
        let e = err(tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "0 9 * * *", "reason": "r"})));
        assert!(e.contains("nothing started by a schedule or trigger may make schedules"), "{e}");
        let e = err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 20})));
        assert!(e.contains("nothing started by a schedule or trigger"), "{e}");
    }
    // An ask from another agent the human started is the human's work.
    f.hub.agents().set_prov(41, Some(Prov { origin: Origin::Human, hop: 1, msg_id: Some(2) })).unwrap();
    assert!(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 20})).is_ok());
    // ada (Claude subscription) never opted in: no reminder for it, no proposal for it.
    as_agent(&f, "ada", 42);
    let e = err(tool(&f, 42, "set_reminder", json!({"prompt": "p", "in_minutes": 20})));
    assert!(e.contains("doesn't take scheduled work") && e.contains("Only the human"), "{e}");
    let e = err(tool(&f, 41, "propose_schedule", json!({"agent": "ada", "prompt": "p", "cron": "0 9 * * *", "reason": "r"})));
    assert!(e.contains("doesn't take scheduled work"), "{e}");
    // A session that isn't a named agent has no schedules.
    assert!(err(tool(&f, 99, "list_schedules", json!({}))).contains("only named agents"));
    // A reason is required, and triggers and steps aren't the agent's to set.
    assert!(err(tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "0 9 * * *", "reason": " "}))).contains("say why"));
    assert!(tool(&f, 41, "propose_schedule", json!({"prompt": "p", "when": {"kind": "url", "name": "x"}, "reason": "r"})).is_err());
    assert!(tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "0 9 * * *", "reason": "r", "then": {}})).is_err());
    // The cron floor applies as for the human's schedules.
    assert!(err(tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "*/5 * * * *", "reason": "r"}))).contains("15 minutes"));
}

#[test]
fn proposal_limits_count_waiting_live_and_daily() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    let p = |n: u32| json!({"prompt": format!("p{n}"), "cron": format!("{n} 9 * * *"), "reason": "r"});
    let ids: Vec<i64> = (0..3).map(|n| tool(&f, 41, "propose_schedule", p(n)).unwrap()["schedule_id"].as_i64().unwrap()).collect();
    assert!(err(tool(&f, 41, "propose_schedule", p(3))).contains("already waiting"));
    // Approved ones still count toward the 5 live schedules it made.
    for id in &ids {
        approve(&f.hub, *id).unwrap();
    }
    tool(&f, 41, "propose_schedule", p(4)).unwrap();
    tool(&f, 41, "propose_schedule", p(5)).unwrap();
    assert!(err(tool(&f, 41, "propose_schedule", p(6))).contains("5 schedules you made"));
    // Deleting gives the room back, but not the day's allowance.
    for n in 0..5 {
        let id = f.hub.schedules.list().into_iter().find(|s| s.made_by.is_some()).unwrap().id;
        tool(&f, 41, "delete_schedule", json!({"schedule_id": id})).unwrap();
        let new = tool(&f, 41, "propose_schedule", p(10 + n)).unwrap()["schedule_id"].as_i64().unwrap();
        approve(&f.hub, new).unwrap();
    }
    let id = f.hub.schedules.list().into_iter().find(|s| s.made_by.is_some()).unwrap().id;
    tool(&f, 41, "delete_schedule", json!({"schedule_id": id})).unwrap();
    assert!(err(tool(&f, 41, "propose_schedule", p(20))).contains("10 proposals in the last 24 hours"));
}

#[test]
fn stale_proposals_and_done_reminders_are_tidied() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let p = tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "0 9 * * *", "reason": "r"})).unwrap()["schedule_id"].as_i64().unwrap();
    let r = tool(&f, 41, "set_reminder", json!({"prompt": "check CI", "in_minutes": 20})).unwrap()["schedule_id"].as_i64().unwrap();
    f.set(t0 + 20 * MIN);
    let started = f.tick();
    assert_eq!(started.len(), 1, "the reminder runs");
    // It ran (the fixture delivers nothing: end it by hand) and is done.
    let mut run = started[0].clone();
    run::finish(&f.hub, &f.hub.schedules.get(r).unwrap(), &mut run, Outcome::Replied, None).unwrap();
    f.set(t0 + 20 * MIN + agent::DONE_REMINDER_KEEP_MS);
    f.tick();
    assert!(f.hub.schedules.get(r).is_none(), "a reminder goes a day after it ran");
    assert!(f.hub.schedules.get(p).is_some());
    f.set(t0 + agent::PROPOSAL_KEEP_MS);
    f.tick();
    assert!(f.hub.schedules.get(p).is_none(), "a proposal nobody approved goes after 7 days");
}

// ----- reminders -----

#[test]
fn a_reminder_runs_without_approval_within_its_limits() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let r = tool(&f, 41, "set_reminder", json!({"prompt": "Check CI again", "in_minutes": 20})).unwrap();
    assert_eq!(r["status"], "active");
    let s = f.hub.schedules.get(r["schedule_id"].as_i64().unwrap()).unwrap();
    assert_eq!((s.when.clone(), s.next_run, s.agent_id), (When::Once { at: t0 + 20 * MIN }, Some(t0 + 20 * MIN), f.hub.agents().get("worker").unwrap().id));
    assert_eq!(s.made_by.as_ref().unwrap().kind, MadeKind::Reminder);
    assert!(!s.waiting_approval());
    assert_eq!(s.name, "Reminder: Check CI again");
    assert_eq!(s.conversation, Conversation::Continue);
    // Too soon, too late, recurring, for another agent: refused.
    assert!(err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 2}))).contains("5 minutes to 24 hours"));
    assert!(err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 24 * 60 + 1}))).contains("propose_schedule"));
    assert!(tool(&f, 41, "set_reminder", json!({"prompt": "p", "cron": "0 9 * * *"})).is_err());
    assert!(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 60, "agent": "orla"})).is_err());
    // 15 minutes apart, at most 3 waiting.
    assert!(err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 30}))).contains("15 minutes apart"));
    tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 40})).unwrap();
    tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 60})).unwrap();
    assert!(err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 90}))).contains("3 reminders waiting"));
    // When it runs, it is an ordinary scheduled run, and while the agent handles it, it can't set
    // the next one (its session's provenance is the schedule's).
    f.set(t0 + 20 * MIN);
    let started = f.tick();
    assert_eq!((started.len(), started[0].kind), (1, RunKind::Scheduled));
    assert_eq!(s.origin(), Origin::Schedule);
}

#[test]
fn reminders_per_day_count_deleted_ones() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    for _ in 0..agent::REMINDERS_PER_DAY {
        let id = tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 30})).unwrap()["schedule_id"].as_i64().unwrap();
        tool(&f, 41, "delete_schedule", json!({"schedule_id": id})).unwrap();
    }
    assert!(err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 30}))).contains("10 reminders in the last 24 hours"));
    f.set(t0 + plan::DAY_MS);
    assert!(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 30})).is_ok());
}

#[test]
fn a_claude_reminder_keeps_quiet_hours() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "cleo", 43);
    // Quiet hours nearly all day: a reminder that would only be skipped is refused.
    set_settings(&f.hub, serde_json::from_value(json!({"quiet_start": "00:00", "quiet_end": "23:59"})).unwrap()).unwrap();
    let e = err(tool(&f, 43, "set_reminder", json!({"prompt": "p", "in_minutes": 20})));
    assert!(e.contains("quiet hours"), "{e}");
    no_quiet_hours(&f);
    let r = tool(&f, 43, "set_reminder", json!({"prompt": "p", "in_minutes": 20})).unwrap();
    let s = f.hub.schedules.get(r["schedule_id"].as_i64().unwrap()).unwrap();
    assert!(s.quiet_hours, "on for a Claude subscription agent, and not the agent's to turn off");
}

// ----- pausing and deleting -----

#[test]
fn an_agent_pauses_its_own_or_its_schedules_and_deletes_only_what_it_made() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    as_agent(&f, "orla", 44);
    let human_on_worker = f.create(json!({"agent": "worker", "prompt": "nightly", "when": {"kind": "cron", "cron": "0 2 * * *"}})).unwrap();
    let human_on_orla = f.create(json!({"agent": "orla", "prompt": "x", "when": {"kind": "cron", "cron": "0 3 * * *"}})).unwrap();
    let list = tool(&f, 41, "list_schedules", json!({})).unwrap();
    let ids: Vec<i64> = list["schedules"].as_array().unwrap().iter().map(|s| s["schedule_id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![human_on_worker.id], "its own, not orla's");
    assert_eq!(list["takes_scheduled_work"], true);
    // Pause: one that runs on it, yes; another agent's, no; resuming isn't a tool.
    assert_eq!(tool(&f, 41, "pause_schedule", json!({"schedule_id": human_on_worker.id})).unwrap()["status"], "paused");
    assert!(err(tool(&f, 41, "pause_schedule", json!({"schedule_id": human_on_orla.id}))).contains("only the human"));
    // Delete: the human's, no; its proposal for orla, yes.
    assert!(err(tool(&f, 41, "delete_schedule", json!({"schedule_id": human_on_worker.id}))).contains("didn't make"));
    let p = tool(&f, 41, "propose_schedule", json!({"agent": "orla", "prompt": "p", "cron": "0 9 * * *", "reason": "r"})).unwrap()["schedule_id"].as_i64().unwrap();
    assert!(err(tool(&f, 44, "delete_schedule", json!({"schedule_id": p}))).contains("didn't make"), "orla didn't make it");
    assert_eq!(tool(&f, 44, "list_schedules", json!({})).unwrap()["schedules"].as_array().unwrap().len(), 2, "orla sees the proposal for it");
    tool(&f, 41, "delete_schedule", json!({"schedule_id": p})).unwrap();
    assert!(f.hub.schedules.get(p).is_none());
}

// ----- the estimate -----

#[test]
fn the_estimate_counts_runs_after_quiet_hours_and_caps() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    no_quiet_hours(&f);
    let r = tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "*/15 * * * *", "reason": "r"})).unwrap();
    let s = f.hub.schedules.get(r["schedule_id"].as_i64().unwrap()).unwrap();
    let worker = f.hub.agents().get("worker").unwrap();
    let e = agent::estimate(&f.hub, &worker, &s, f.hub.schedules.now());
    // 96 a day by its timing, but the agent takes at most 24 unattended messages a day.
    assert_eq!(e["max_per_day"], 24);
    assert!(e["runs_per_week"].as_u64().unwrap() <= 7 * 24 + 24, "{e}");
    assert_eq!(e["billing"], "codex");
    let orla = f.hub.agents().get("orla").unwrap();
    let e = agent::estimate(&f.hub, &orla, &s, f.hub.schedules.now());
    assert_eq!(e["billing"], "openrouter");
    assert!(e["text"].as_str().unwrap().contains("$5.00 a day"), "{e}");
}

// ----- the file-change trigger -----

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("muster-files-{tag}-{}-{}", std::process::id(), crate::paths::random_token()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

fn allow(f: &Fixture, dirs: &[&std::path::Path]) {
    let v: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
    set_settings(&f.hub, serde_json::from_value(json!({"watch_folders": v})).unwrap()).unwrap();
}

#[test]
fn file_triggers_are_off_until_the_human_chooses_a_folder() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    let root = temp_dir("off");
    let inbox = root.join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let other = temp_dir("other");
    let w = json!({"agent": "worker", "prompt": "File it", "when": {"kind": "files", "folder": inbox, "pattern": "*.pdf"}});
    let e = f.create(w.clone()).unwrap_err().to_string();
    assert!(e.contains("Watched folders"), "{e}");
    allow(&f, &[&other]);
    let e = f.create(w.clone()).unwrap_err().to_string();
    assert!(e.contains("isn't inside a folder you allow"), "{e}");
    allow(&f, &[&root]);
    let s = f.create(w).unwrap();
    assert!(matches!(&s.when, When::Files { pattern: Some(p), recursive: false, quiet_min: 2, .. } if p == "*.pdf"));
    assert_eq!(s.origin(), Origin::Webhook);
    assert_eq!(s.daily_cap, Some(triggers::DAILY_CAP_DEFAULT));
    assert!(schedule_json(&f.hub, &s)["reading"].as_str().unwrap().contains("files named *.pdf appear or change in"));
    // Taking the folder away stops it.
    allow(&f, &[&other]);
    assert!(matches!(triggers::event_flag(&f.hub, &s), Some(Action::Skip(w)) if w.contains("no longer")));
    // The settings check their folders.
    assert!(set_settings(&f.hub, serde_json::from_value(json!({"watch_folders": ["/"]})).unwrap()).is_err());
    assert!(set_settings(&f.hub, serde_json::from_value(json!({"watch_folders": ["relative/x"]})).unwrap()).is_err());
    assert!(set_settings(&f.hub, serde_json::from_value(json!({"watch_folders": [root.join("missing")]})).unwrap()).is_err());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&other);
}

fn rels(v: &[triggers::FileSeen]) -> Vec<String> {
    v.iter().map(|f| f.rel.clone()).collect()
}

/// One look and what it found new, as `check_files` does (boundary: a little before it began).
fn look_news(d: &std::path::Path, pattern: Option<&str>, recursive: bool, w: &WatchState) -> (Vec<String>, WatchState) {
    let boundary = now_ns().saturating_sub(triggers::FILES_SCAN_MARGIN_NS);
    let look = triggers::look_at_folder(d, pattern, recursive, w.dirs.as_deref()).unwrap();
    let (fresh, next) = triggers::files_news(w, &look, boundary);
    (rels(&fresh), next.unwrap_or_else(|| w.clone()))
}

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}

#[test]
fn a_folder_look_finds_new_and_changed_files_only() {
    let d = temp_dir("look");
    std::fs::write(d.join("old.pdf"), "a").unwrap();
    std::fs::write(d.join(".hidden.pdf"), "a").unwrap();
    std::fs::write(d.join("note.txt"), "a").unwrap();
    let (first, w) = look_news(&d, Some("*.pdf"), false, &WatchState::default());
    assert!(first.is_empty() && w.stamp.is_some(), "the first look only records");
    std::fs::write(d.join("new.pdf"), "b").unwrap();
    std::fs::write(d.join("other.txt"), "b").unwrap();
    std::fs::write(d.join("dl.pdf.part"), "b").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/deep.pdf"), "b").unwrap();
    let (next, w2) = look_news(&d, Some("*.pdf"), false, &w);
    assert_eq!(next, vec!["new.pdf".to_string()]);
    // Seen once, not again (the overlap between looks reports nothing twice).
    assert!(look_news(&d, Some("*.pdf"), false, &w2).0.is_empty());
    let look = triggers::look_at_folder(&d, Some("*.pdf"), true, None).unwrap();
    assert_eq!(rels(&look.files), vec!["new.pdf", "old.pdf", "sub/deep.pdf"]);
    // A file moved in keeps its old contents' times but gets a new change time.
    let elsewhere = temp_dir("elsewhere");
    std::fs::write(elsewhere.join("moved.pdf"), "c").unwrap();
    std::fs::rename(elsewhere.join("moved.pdf"), d.join("moved.pdf")).unwrap();
    assert_eq!(look_news(&d, Some("*.pdf"), false, &w2).0, vec!["moved.pdf".to_string()]);
    // Too big a tree is refused, not read.
    let big = temp_dir("big");
    for i in 0..=triggers::FILES_SCAN_MAX {
        std::fs::write(big.join(format!("f{i}")), "").unwrap();
    }
    assert!(triggers::look_at_folder(&big, None, false, None).unwrap_err().to_string().contains("more than"));
    assert!(triggers::glob_match("*.pdf", "a.pdf") && !triggers::glob_match("*.pdf", "a.pdfx") && triggers::glob_match("r?port*", "report-1"));
    for p in [d, elsewhere, big] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[test]
fn a_file_changed_during_a_look_is_not_skipped() {
    // A look lists b (old) then a (just changed); b changes while it runs. The old cursor (the
    // newest change seen, a's) would skip b's change; the boundary (before the look began) and
    // the per-file record don't.
    let f = |rel: &str, stamp: u64| triggers::FileSeen { rel: rel.into(), stamp, moved_in: None };
    let prev = WatchState { stamp: Some(100), ..Default::default() };
    let look1 = triggers::FolderLook { files: vec![f("a", 300), f("b", 50)], dirs: None };
    let (fresh, w) = triggers::files_news(&prev, &look1, 250);
    assert_eq!(rels(&fresh), vec!["a"]);
    let w = w.unwrap();
    assert_eq!((w.stamp, w.recent.clone()), (Some(250), vec![("a".to_string(), 300)]));
    // Next look: b changed at 280 (after the boundary, before a's time): reported; a isn't again.
    let look2 = triggers::FolderLook { files: vec![f("a", 300), f("b", 280)], dirs: None };
    let (fresh, w) = triggers::files_news(&w, &look2, 400);
    assert_eq!(rels(&fresh), vec!["b"]);
    // Nothing new: nothing to save.
    let w = w.unwrap();
    assert!(triggers::files_news(&w, &look2, 500).1.is_none());
    // a changed again (a new time): reported.
    let look3 = triggers::FolderLook { files: vec![f("a", 450), f("b", 280)], dirs: None };
    assert_eq!(rels(&triggers::files_news(&w, &look3, 600).0), vec!["a"]);
}

#[test]
fn a_folder_moved_in_counts_its_files_as_new() {
    let d = temp_dir("movedir");
    std::fs::create_dir(d.join("kept")).unwrap();
    std::fs::write(d.join("kept/x.pdf"), "x").unwrap();
    let (_, w) = look_news(&d, None, true, &WatchState::default());
    assert_eq!(w.dirs.as_ref().map(Vec::len), Some(1));
    // A folder full of old files, moved in: only the folder's change time is new.
    let elsewhere = temp_dir("movedir-src");
    std::fs::create_dir_all(elsewhere.join("batch/inner")).unwrap();
    std::fs::write(elsewhere.join("batch/a.pdf"), "a").unwrap();
    std::fs::write(elsewhere.join("batch/inner/b.pdf"), "b").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let (_, w) = look_news(&d, None, true, &w); // the boundary moves past the files' own times
    let w = WatchState { stamp: Some(now_ns()), ..w };
    std::fs::rename(elsewhere.join("batch"), d.join("batch")).unwrap();
    let (fresh, w2) = look_news(&d, None, true, &w);
    assert_eq!(fresh, vec!["batch/a.pdf".to_string(), "batch/inner/b.pdf".to_string()], "everything in it is new");
    assert_eq!(w2.dirs.as_ref().map(Vec::len), Some(3));
    assert!(look_news(&d, None, true, &w2).0.is_empty(), "once");
    // Renamed within the watched folder: not new content.
    std::fs::rename(d.join("batch"), d.join("batch2")).unwrap();
    let w3 = WatchState { stamp: Some(now_ns()), recent: vec![], ..w2 };
    assert!(look_news(&d, None, true, &w3).0.is_empty(), "the same folders, moved: nothing new");
    for p in [d, elsewhere] {
        let _ = std::fs::remove_dir_all(p);
    }
}

#[test]
fn a_look_never_follows_a_folder_swapped_for_a_link() {
    let d = temp_dir("swap");
    let secret = temp_dir("swap-secret");
    std::fs::write(secret.join("secret.pdf"), "s").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/mine.pdf"), "m").unwrap();
    // A subfolder that is a link from the start: never read.
    std::os::unix::fs::symlink(&secret, d.join("link")).unwrap();
    let look = triggers::look_at_folder(&d, None, true, None).unwrap();
    assert_eq!(rels(&look.files), vec!["sub/mine.pdf"]);
    // Swapped for a link again and again while looks run: no look ever lists the other folder.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapper = {
        let (d, secret, stop) = (d.clone(), secret.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::rename(d.join("sub"), d.join(".sub-real"));
                let _ = std::os::unix::fs::symlink(&secret, d.join("sub"));
                std::thread::yield_now();
                let _ = std::fs::remove_file(d.join("sub"));
                let _ = std::fs::rename(d.join(".sub-real"), d.join("sub"));
            }
        })
    };
    for _ in 0..400 {
        if let Ok(look) = triggers::look_at_folder(&d, None, true, None) {
            assert!(look.files.iter().all(|f| !f.rel.contains("secret")), "{:?}", rels(&look.files));
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    swapper.join().unwrap();
    // The watched folder itself swapped for a link: refused, wherever the link leads.
    let watched = temp_dir("swap-root");
    std::fs::write(watched.join("w.pdf"), "w").unwrap();
    std::fs::rename(&watched, watched.with_extension("real")).unwrap();
    std::os::unix::fs::symlink(&secret, &watched).unwrap();
    let e = triggers::look_at_folder(&watched, None, false, None).unwrap_err().to_string();
    assert!(e.contains("replaced by a link"), "{e}");
    // An earlier part of its path swapped: the open handle's real place shows it.
    let parent = temp_dir("swap-parent");
    std::fs::create_dir(parent.join("in")).unwrap();
    let inner = parent.join("in");
    std::fs::rename(&parent, parent.with_extension("real")).unwrap();
    std::fs::create_dir_all(secret.join("in")).unwrap();
    std::os::unix::fs::symlink(&secret, &parent).unwrap();
    assert!(triggers::look_at_folder(&inner, None, false, None).is_err(), "a path through a link is refused");
    for p in [d, secret, watched.with_extension("real"), parent.with_extension("real")] {
        let _ = std::fs::remove_dir_all(p);
    }
    let _ = std::fs::remove_file(&watched);
    let _ = std::fs::remove_file(&parent);
}

#[test]
fn a_held_event_stops_when_its_folder_is_swapped_for_a_link() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    let root = temp_dir("held");
    let d = root.join("inbox");
    std::fs::create_dir(&d).unwrap();
    let elsewhere_allowed = root.join("other");
    std::fs::create_dir(&elsewhere_allowed).unwrap();
    allow(&f, &[&root]);
    let s = f.create(json!({"agent": "worker", "prompt": "File it", "when": {"kind": "files", "folder": d, "quiet_min": 3}})).unwrap();
    triggers::check_files(&f.hub);
    std::fs::write(d.join("a.pdf"), "1").unwrap();
    triggers::check_files(&f.hub);
    assert!(f.hub.schedules.get(s.id).unwrap().event.is_some(), "an event waits out its quiet minutes");
    // The folder is swapped for a link (even to another allowed folder) while it waits.
    std::fs::rename(&d, root.join("inbox-real")).unwrap();
    std::os::unix::fs::symlink(&elsewhere_allowed, &d).unwrap();
    let held = f.hub.schedules.get(s.id).unwrap();
    assert!(matches!(triggers::event_flag(&f.hub, &held), Some(Action::Skip(w)) if w.contains("replaced by a link")), "checked again before it runs");
    triggers::check_files(&f.hub);
    let after = f.hub.schedules.get(s.id).unwrap();
    assert!(after.event.is_none() && after.next_run.is_none(), "what it had waiting is dropped");
    assert!(f.hub.triggers.live(s.id).unwrap().detail.unwrap().contains("replaced by a link"));
    // The folder gone altogether: the same.
    std::fs::remove_file(&d).unwrap();
    assert!(matches!(triggers::event_flag(&f.hub, &after), Some(Action::Skip(w)) if w.contains("can't read")));
    let _ = std::fs::remove_dir_all(&root);
}

/// `worker` is session 41; a span of its work, or of unattended work, from `start` to `end`
/// (unix ms, real clock).
fn worked(f: &Fixture, sid: SessionId, start: i64, end: Option<i64>) {
    f.hub.triggers.observe(sid, true, start);
    if let Some(e) = end {
        f.hub.triggers.observe(sid, false, e);
    }
}

#[test]
fn file_changes_are_debounced_and_an_agents_own_are_ignored() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let d = temp_dir("trig");
    allow(&f, &[&d]);
    let s = f.create(json!({"agent": "worker", "prompt": "File it", "when": {"kind": "files", "folder": d, "quiet_min": 3}})).unwrap();
    triggers::check_files(&f.hub);
    let base = f.hub.schedules.get(s.id).unwrap();
    assert!(base.watch.stamp.is_some() && base.event.is_none(), "the first look only records");
    std::fs::write(d.join("a.pdf"), "1").unwrap();
    triggers::check_files(&f.hub);
    let held = f.hub.schedules.get(s.id).unwrap();
    assert_eq!(held.next_run, Some(t0 + 3 * MIN));
    assert!(held.event.as_ref().unwrap().text.contains("New or changed in") && held.event.as_ref().unwrap().text.contains("a.pdf"));
    // Another change a minute later moves the run on (one run for both).
    f.set(t0 + MIN);
    std::fs::write(d.join("b.pdf"), "2").unwrap();
    triggers::check_files(&f.hub);
    let held = f.hub.schedules.get(s.id).unwrap();
    assert_eq!((held.next_run, held.event.as_ref().unwrap().count), (Some(t0 + 4 * MIN), 2));
    f.set(t0 + 4 * MIN);
    let started = f.tick();
    assert_eq!((started.len(), started[0].kind), (1, RunKind::Triggered));
    // Its agent's turn started, wrote and ended between two looks: still its own (the turn is
    // recorded from the session's state changes, not seen at a look).
    let real = now_ms();
    f.hub.triggers.observe(41, true, real - 2_000);
    std::fs::write(d.join("written-by-agent.pdf"), "3").unwrap();
    f.hub.triggers.observe(41, false, now_ms());
    triggers::check_files(&f.hub);
    let after = f.hub.schedules.get(s.id).unwrap();
    assert!(after.event.is_none(), "an agent's own change doesn't count");
    assert!(after.watch.recent.iter().any(|(r, _)| r == "written-by-agent.pdf"), "but it is recorded as seen");
    // Unattended work by any session (here 77, on a scheduled chain): its changes don't count.
    f.hub.agents().set_prov(77, Some(Prov { origin: Origin::Schedule, hop: 0, msg_id: Some(1) })).unwrap();
    worked(&f, 77, now_ms() - 1_000, None);
    std::fs::write(d.join("by-a-schedule.pdf"), "4").unwrap();
    triggers::check_files(&f.hub);
    assert!(f.hub.schedules.get(s.id).unwrap().event.is_none(), "unattended work's change doesn't count");
    f.hub.triggers.observe(77, false, now_ms());
    // Turns that ended well over 30 s ago don't make a change now anyone's own.
    f.hub.triggers.forget_activity_for_test();
    worked(&f, 41, now_ms() - 120_000, Some(now_ms() - 60_000));
    std::fs::write(d.join("by-you.pdf"), "5").unwrap();
    triggers::check_files(&f.hub);
    let e = f.hub.schedules.get(s.id).unwrap().event.expect("a change of yours fires it");
    assert!(e.text.contains("by-you.pdf") && !e.text.contains("by-a-schedule.pdf"), "{}", e.text);
    // The folder taken off the list: nothing is looked at or held.
    let other = temp_dir("trig-other");
    allow(&f, &[&other]);
    std::fs::write(d.join("c.pdf"), "4").unwrap();
    triggers::check_files(&f.hub);
    assert!(f.hub.schedules.get(s.id).unwrap().event.is_none());
    assert_eq!(f.hub.triggers.live(s.id).unwrap().state, "error");
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn who_was_working_is_kept_across_a_reload() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    // Kept only while there is a file trigger to use it.
    let d = temp_dir("reload");
    allow(&f, &[&d]);
    f.create(json!({"agent": "worker", "prompt": "File it", "when": {"kind": "files", "folder": d}})).unwrap();
    let real = now_ms();
    worked(&f, 41, real - 5_000, Some(real - 1_000));
    worked(&f, 42, real - 500, None);
    triggers::check_files(&f.hub); // settles (worker's agent id) and saves
    let before = f.hub.triggers.spans_for_test();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].agent, Some(f.hub.agents().get("worker").unwrap().id));
    // A reload: the backend's memory is gone, the saved spans come back.
    f.hub.triggers.forget_activity_for_test();
    triggers::restore_activity(&f.hub);
    let after = f.hub.triggers.spans_for_test();
    assert_eq!(after[0], before[0]);
    // Session 42's span was open at the save and the session isn't working now (it is gone):
    // it is closed, at the save at the latest.
    assert!(after[1].session == 42 && after[1].end.is_some_and(|e| e >= after[1].start), "{after:?}");
    assert!(triggers::own_change(&after, -1, Some(42), real, now_ms()), "a change then is still its own");
    let _ = std::fs::remove_dir_all(&d);
}

// ----- limits under concurrency, pending runs, the CLI's relative times -----

#[test]
fn reminders_made_at_once_still_keep_every_limit() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    // Eight calls at once for the same time: one gets it (15 minutes apart).
    let ok = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 20})).is_ok())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).filter(|x| *x).count()
    });
    assert_eq!(ok, 1, "no two reminders at the same time");
    // Eight at once, each at its own time: only 3 may wait.
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let ok = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let f = &f;
                sc.spawn(move || tool(f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 20 + 20 * i})).is_ok())
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).filter(|x| *x).count()
    });
    assert_eq!(ok, agent::REMINDERS_WAITING_MAX);
    // Proposals at once: at most 3 waiting.
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let ok = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let f = &f;
                sc.spawn(move || tool(f, 41, "propose_schedule", json!({"prompt": "p", "cron": format!("{i} 9 * * *"), "reason": "r"})).is_ok())
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).filter(|x| *x).count()
    });
    assert_eq!(ok, agent::PROPOSALS_WAITING_MAX);
}

#[test]
fn a_reminder_whose_run_is_still_to_happen_still_counts() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    let r = tool(&f, 41, "set_reminder", json!({"prompt": "check CI", "in_minutes": 20})).unwrap()["schedule_id"].as_i64().unwrap();
    // Its time comes: the run is claimed (no next run any more) but waits, say behind a turn of
    // the human's.
    f.set(t0 + 20 * MIN);
    assert_eq!(f.tick().len(), 1);
    assert!(f.hub.schedules.get(r).unwrap().next_run.is_none());
    // Still 15 minutes apart from it, and still one of the 3 waiting.
    let e = err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 5})));
    assert!(e.contains("15 minutes apart"), "{e}");
    tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 20})).unwrap();
    tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 40})).unwrap();
    let e = err(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 60})));
    assert!(e.contains("3 reminders waiting"), "{e}");
    // Exactly 5 minutes ahead is allowed (what `muster schedule remind --at +5m` sends).
    let f = Fixture::new(t0);
    as_agent(&f, "worker", 41);
    assert!(tool(&f, 41, "set_reminder", json!({"prompt": "p", "in_minutes": 5})).is_ok());
}

#[test]
fn a_proposals_timing_says_its_time_zone_when_it_isnt_this_computers() {
    let f = Fixture::new(at(LONDON, "2026-10-19T10:00"));
    as_agent(&f, "worker", 41);
    let here = cron::local_zone_name();
    let tz = if here == "America/Los_Angeles" { "Asia/Tokyo" } else { "America/Los_Angeles" };
    let r = tool(&f, 41, "propose_schedule", json!({"prompt": "p", "cron": "30 8 * * 1-5", "tz": tz, "reason": "r"})).unwrap();
    let zone = tz.replace('_', " ");
    assert!(r["estimate"].as_str().unwrap().contains(&format!("at {zone} times")), "{}", r["estimate"]);
    let r = tool(&f, 41, "propose_schedule", json!({"prompt": "p", "in_minutes": 120, "tz": tz, "reason": "r"})).unwrap();
    let e = r["estimate"].as_str().unwrap();
    assert!(e.contains(&format!("{zone} time")) && e.contains("here)"), "{e}");
}
