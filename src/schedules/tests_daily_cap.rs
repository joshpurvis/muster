//! A schedule's daily cap ("max per day") counts runs per calendar day in the schedule's time
//! zone, by the day of each run's slot: a daily schedule whose run went out a few seconds after
//! its slot yesterday still runs today, a second run on the same day is refused, daylight-saving
//! days are as long as they are, and a catch-up counts toward the day it stands for.

use super::super::*;
use super::{Fixture, LONDON, at};
use crate::agents::Origin;
use serde_json::json;

/// A run of `s` at `slot` whose message reached the agent `late_ms` after the slot (stamped on
/// the daemon's real clock, as agent messages are; the fixture's clock is the scheduler's).
fn delivered(f: &Fixture, s: &Schedule, slot: i64, late_ms: i64) -> i64 {
    let worker = f.hub.agents().get("worker").unwrap();
    let route = crate::agents::Route { origin: Origin::Schedule, hop: 0, reply_to: None };
    let mut m = crate::agents::message_record(&worker, &s.sender(), None, route, "x");
    m.created_at = crate::model::now_ms() - (f.hub.schedules.now() - (slot + late_ms));
    let mut r = Run::new(s.id, slot, RunKind::Scheduled, slot);
    r.msg_id = Some(f.hub.store().insert_agent_message(&m).unwrap());
    r.outcome = Outcome::Replied;
    r.finished_at = Some(slot + late_ms + 60_000);
    f.hub.store().insert_run(&r).unwrap()
}

/// The run the tick just claimed at `slot`, sent and answered `late_ms` after it.
fn deliver_claimed(f: &Fixture, s: &Schedule, slot: i64, late_ms: i64) {
    let mut r = f.runs(s.id).into_iter().find(|r| r.slot == slot && r.outcome == Outcome::Pending).expect("a claimed run");
    let worker = f.hub.agents().get("worker").unwrap();
    let route = crate::agents::Route { origin: Origin::Schedule, hop: 0, reply_to: None };
    let mut m = crate::agents::message_record(&worker, &s.sender(), None, route, "x");
    m.created_at = crate::model::now_ms() - (f.hub.schedules.now() - (slot + late_ms));
    r.msg_id = Some(f.hub.store().insert_agent_message(&m).unwrap());
    r.outcome = Outcome::Replied;
    r.finished_at = Some(slot + late_ms + 60_000);
    f.hub.store().update_run(&r).unwrap();
}

fn daily(f: &Fixture, cron: &str, cap: u32) -> Schedule {
    f.create(json!({"agent": "worker", "prompt": "p", "tz": LONDON, "when": {"kind": "cron", "cron": cron}, "daily_cap": cap, "notify": "never"}))
        .unwrap()
}

#[test]
fn a_daily_cap_of_one_runs_every_day_when_yesterdays_run_went_out_a_few_seconds_late() {
    let f = Fixture::new(at(LONDON, "2026-11-22T09:00"));
    let s = daily(&f, "30 9 * * *", 1);
    // Yesterday's 09:30 run reached the agent at 09:30:05.
    delivered(&f, &s, at(LONDON, "2026-11-21T09:30"), 5_000);
    f.set(at(LONDON, "2026-11-22T09:30") + 500);
    let started = f.tick();
    let rs = f.runs(s.id);
    assert_eq!(started.len(), 1, "today's 09:30 run goes: {rs:?}");
    assert_eq!(started[0].kind, RunKind::Scheduled);
}

#[test]
fn a_second_run_on_the_same_day_is_still_refused() {
    let f = Fixture::new(at(LONDON, "2026-11-22T09:00"));
    let s = daily(&f, "30 9,15 * * *", 1);
    f.set(at(LONDON, "2026-11-22T09:30") + 500);
    assert_eq!(f.tick().len(), 1, "the first run of the day goes");
    deliver_claimed(&f, &s, at(LONDON, "2026-11-22T09:30"), 5_000);
    f.set(at(LONDON, "2026-11-22T15:30") + 500);
    assert!(f.tick().is_empty());
    let rs = f.runs(s.id);
    let refused = rs.iter().find(|r| r.slot == at(LONDON, "2026-11-22T15:30")).expect("the 15:30 run is recorded");
    assert_eq!(refused.outcome, Outcome::Refused);
    assert_eq!(refused.reason.as_deref(), Some("daily cap: it already ran once today (at most 1 a day)"));
    // The next morning is a new day.
    f.set(at(LONDON, "2026-11-23T09:30") + 500);
    assert_eq!(f.tick().len(), 1, "the next day's first run goes");
}

#[test]
fn a_daylight_saving_day_is_one_calendar_day_however_long() {
    // Spring forward (29 Mar 2026 in London, a 23-hour day): 09:30 on the 28th to 09:30 on the
    // 29th is 23 hours, and the 29th's run still goes.
    let f = Fixture::new(at(LONDON, "2026-03-29T09:00"));
    let s = daily(&f, "30 9 * * *", 1);
    delivered(&f, &s, at(LONDON, "2026-03-28T09:30"), 0);
    f.set(at(LONDON, "2026-03-29T09:30") + 500);
    assert_eq!(f.tick().len(), 1, "the run after the 23-hour night goes");

    // Fall back (25 Oct 2026, a 25-hour day): 00:30 and 23:30 that day are 24 hours apart but
    // one calendar day, so the second is refused; the next day's 00:30 goes.
    let f = Fixture::new(at(LONDON, "2026-10-25T00:00"));
    let s = daily(&f, "30 0,23 * * *", 1);
    f.set(at(LONDON, "2026-10-25T00:30") + 500);
    assert_eq!(f.tick().len(), 1);
    deliver_claimed(&f, &s, at(LONDON, "2026-10-25T00:30"), 0);
    f.set(at(LONDON, "2026-10-25T23:30") + 500);
    assert!(f.tick().is_empty(), "23:30 on the 25-hour day is the same day");
    let last = f.runs(s.id).into_iter().find(|r| r.slot == at(LONDON, "2026-10-25T23:30")).unwrap();
    assert_eq!(last.outcome, Outcome::Refused, "{last:?}");
    f.set(at(LONDON, "2026-10-26T00:30") + 500);
    assert_eq!(f.tick().len(), 1, "the 26th is a new day");
}

#[test]
fn a_catch_up_after_midnight_counts_toward_the_day_it_stands_for() {
    // 23:30 daily: the 21st's run went; Muster was off at 23:30 on the 22nd and back at 00:20.
    let f = Fixture::new(at(LONDON, "2026-11-21T23:00"));
    let s = daily(&f, "30 23 * * *", 1);
    delivered(&f, &s, at(LONDON, "2026-11-21T23:30"), 5_000);
    f.hub.schedules.edit(s.id, |x| {
        x.next_run = Some(at(LONDON, "2026-11-22T23:30"));
        x.last_fired = Some(at(LONDON, "2026-11-21T23:30"));
        Ok(())
    }).unwrap();
    f.set(at(LONDON, "2026-11-23T00:20"));
    let started = f.tick();
    assert_eq!(started.len(), 1, "the catch-up for the 22nd goes: {:?}", f.runs(s.id));
    assert_eq!((started[0].kind, started[0].slot), (RunKind::CatchUp, at(LONDON, "2026-11-22T23:30")));
    deliver_claimed(&f, &s, at(LONDON, "2026-11-22T23:30"), 50 * 60_000);
    // It went out at 00:20 on the 23rd, but stood for the 22nd: the 23rd's own run still goes.
    f.set(at(LONDON, "2026-11-23T23:30") + 500);
    assert_eq!(f.tick().len(), 1, "the 23rd's run goes: {:?}", f.runs(s.id));
}

#[test]
fn a_refusal_for_another_day_names_that_day() {
    // A catch-up refused for a day that already had its run says which day.
    let f = Fixture::new(at(LONDON, "2026-11-22T09:00"));
    let s = daily(&f, "30 9,15 * * *", 1);
    delivered(&f, &s, at(LONDON, "2026-11-22T09:30"), 5_000);
    f.hub.schedules.edit(s.id, |x| {
        x.next_run = Some(at(LONDON, "2026-11-22T15:30"));
        x.last_fired = Some(at(LONDON, "2026-11-22T09:30"));
        Ok(())
    }).unwrap();
    f.set(at(LONDON, "2026-11-23T08:00"));
    assert!(f.tick().is_empty());
    let r = f.runs(s.id).into_iter().find(|r| r.slot == at(LONDON, "2026-11-22T15:30")).unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{r:?}");
    assert_eq!(r.reason.as_deref(), Some("daily cap: it already ran once on Sun 22 Nov (at most 1 a day)"));
}
