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

#[test]
fn a_folder_look_finds_new_and_changed_files_only() {
    let d = temp_dir("look");
    std::fs::write(d.join("old.pdf"), "a").unwrap();
    std::fs::write(d.join(".hidden.pdf"), "a").unwrap();
    std::fs::write(d.join("note.txt"), "a").unwrap();
    let first = triggers::look_at_folder(&d, Some("*.pdf"), false, None).unwrap();
    assert!(first.changed.is_empty() && first.newest > 0);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(d.join("new.pdf"), "b").unwrap();
    std::fs::write(d.join("other.txt"), "b").unwrap();
    std::fs::write(d.join("dl.pdf.part"), "b").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/deep.pdf"), "b").unwrap();
    let next = triggers::look_at_folder(&d, Some("*.pdf"), false, Some(first.newest)).unwrap();
    assert_eq!(next.changed, vec!["new.pdf".to_string()]);
    let rec = triggers::look_at_folder(&d, Some("*.pdf"), true, Some(first.newest)).unwrap();
    assert_eq!(rec.changed, vec!["new.pdf".to_string(), "sub/deep.pdf".to_string()]);
    // A file moved in keeps its old contents' times but gets a new change time.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let elsewhere = temp_dir("elsewhere");
    std::fs::write(elsewhere.join("moved.pdf"), "c").unwrap();
    std::fs::rename(elsewhere.join("moved.pdf"), d.join("moved.pdf")).unwrap();
    let moved = triggers::look_at_folder(&d, Some("*.pdf"), false, Some(rec.newest)).unwrap();
    assert_eq!(moved.changed, vec!["moved.pdf".to_string()]);
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
fn file_changes_are_debounced_and_an_agents_own_are_ignored() {
    let t0 = at(LONDON, "2026-10-19T10:00");
    let f = Fixture::new(t0);
    let d = temp_dir("trig");
    allow(&f, &[&d]);
    let s = f.create(json!({"agent": "worker", "prompt": "File it", "when": {"kind": "files", "folder": d, "quiet_min": 3}})).unwrap();
    triggers::check_files(&f.hub);
    let base = f.hub.schedules.get(s.id).unwrap();
    assert!(base.watch.stamp.is_some() && base.event.is_none(), "the first look only records");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(d.join("a.pdf"), "1").unwrap();
    triggers::check_files(&f.hub);
    let held = f.hub.schedules.get(s.id).unwrap();
    assert_eq!(held.next_run, Some(t0 + 3 * MIN));
    assert!(held.event.as_ref().unwrap().text.contains("New or changed in") && held.event.as_ref().unwrap().text.contains("a.pdf"));
    // Another change a minute later moves the run on (one run for both).
    f.set(t0 + MIN);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(d.join("b.pdf"), "2").unwrap();
    triggers::check_files(&f.hub);
    let held = f.hub.schedules.get(s.id).unwrap();
    assert_eq!((held.next_run, held.event.as_ref().unwrap().count), (Some(t0 + 4 * MIN), 2));
    f.set(t0 + 4 * MIN);
    let started = f.tick();
    assert_eq!((started.len(), started[0].kind), (1, RunKind::Triggered));
    // While its agent works (or just after), a change is the agent's own: absorbed, no event.
    triggers::absorb_for_test(&f.hub, s.id, f.hub.schedules.now() + triggers::ABSORB_GRACE_MS);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(d.join("written-by-agent.pdf"), "3").unwrap();
    triggers::check_files(&f.hub);
    let after = f.hub.schedules.get(s.id).unwrap();
    assert!(after.event.is_none(), "an agent's own change doesn't count");
    assert!(after.watch.stamp > held.watch.stamp, "but it is recorded as seen");
    // The folder taken off the list: nothing is looked at or held.
    let other = temp_dir("trig-other");
    allow(&f, &[&other]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(d.join("c.pdf"), "4").unwrap();
    f.set(t0 + 5 * MIN + triggers::ABSORB_GRACE_MS);
    triggers::check_files(&f.hub);
    assert!(f.hub.schedules.get(s.id).unwrap().event.is_none());
    assert_eq!(f.hub.triggers.live(s.id).unwrap().state, "error");
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&other);
}
