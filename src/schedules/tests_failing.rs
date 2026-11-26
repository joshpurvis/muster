//! A schedule that keeps failing is told once, at its second failure in a row; an expired one is
//! told once ("expired: renew or remove").

use super::*;
use crate::hub::ServerEvent;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

/// The notices sent since the last call: (text, schedule it opens).
fn notices(rx: &mut tokio::sync::broadcast::Receiver<ServerEvent>) -> Vec<(String, Option<i64>)> {
    let mut out = vec![];
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Notice { text, schedule, .. } = ev {
            out.push((text, schedule));
        }
    }
    out
}

/// A fixture with agent alpha (Antigravity: no 7-day expiry, no default quiet hours).
fn with_alpha(now: i64) -> Fixture {
    let store = crate::store::Store::open_memory().unwrap();
    let all = vec![Origin::Human, Origin::Schedule, Origin::Webhook];
    store.insert_agent(&agent("alpha", SessionKind::Antigravity, Provider::Subscription, all)).unwrap();
    let config = crate::hub::HubConfig {
        port: 0,
        client_token: "fixture".into(),
        muster_bin: "/bin/true".into(),
        terminal_host: None,
        openrouter_key: None,
    };
    let hub = Hub::new(config, store).unwrap();
    let clock = Arc::new(FakeClock(AtomicI64::new(now)));
    hub.schedules.set_clock(clock.clone());
    Fixture { hub, clock }
}

/// Run every due slot to its end (no terminal here: nothing reaches an agent, so only runs
/// refused or skipped before sending end by themselves).
async fn run_due(f: &Fixture) -> Vec<Run> {
    let started = f.tick();
    for r in &started {
        run::spawn_execute(f.hub.clone(), r.clone());
    }
    let mut done = vec![];
    for r in started {
        let mut end = None;
        for _ in 0..400 {
            tokio::task::yield_now().await;
            let now = f.hub.store().run(r.id).unwrap().unwrap();
            if now.outcome != Outcome::Pending {
                end = Some(now);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        done.push(end.expect("the run ended"));
    }
    done
}

/// 25 Nov: an agent's half-hourly pace check was refused from 07:00 ("accepts only webhook
/// messages") and nobody noticed for 14 hours. Replayed, the notice comes at 07:30, at the
/// second refusal, once, with a button that opens the schedule; 08:00's third refusal is quiet.
#[tokio::test]
async fn the_25_nov_pace_check_is_told_at_its_second_refusal() {
    const NY: &str = "America/New_York";
    let f = with_alpha(at(NY, "2026-11-25T06:20"));
    let s = f
        .create(json!({"agent": "alpha", "name": "pace check", "prompt": "Pace check.", "tz": NY,
            "when": {"kind": "cron", "cron": "0,30 * * * *"}}))
        .unwrap();
    // 06:30's run went out and was answered.
    let alpha = f.hub.agents().get("alpha").unwrap();
    let route = crate::agents::Route { origin: Origin::Schedule, hop: 0, reply_to: None };
    let m = f.hub.store().insert_agent_message(&crate::agents::message_record(&alpha, &s.sender(), None, route, "Pace check.")).unwrap();
    f.hub.store().set_agent_message_reply(m, "pace ok", 1).unwrap();
    let mut ok = Run::new(s.id, at(NY, "2026-11-25T06:30"), RunKind::Scheduled, at(NY, "2026-11-25T06:30"));
    (ok.msg_id, ok.outcome, ok.finished_at) = (Some(m), Outcome::Replied, Some(at(NY, "2026-11-25T06:31")));
    f.hub.store().insert_run(&ok).unwrap();
    // Then alpha is set to take webhook messages only.
    crate::agents::update(&f.hub, serde_json::from_value(json!({"name": "alpha", "accepts": ["webhook"]})).unwrap(), false).unwrap();
    let mut rx = f.hub.subscribe();
    let mut told = vec![];
    for t in ["07:00", "07:30", "08:00"] {
        f.set(at(NY, &format!("2026-11-25T{t}")) + 500);
        let runs = run_due(&f).await;
        assert_eq!(runs.len(), 1, "{t}");
        assert_eq!(runs[0].outcome, Outcome::Refused, "{t}: {runs:?}");
        assert!(runs[0].reason.as_deref().unwrap().contains("accepts only webhook messages"), "{runs:?}");
        let twice: Vec<_> = notices(&mut rx).into_iter().filter(|(text, _)| text.contains("failed twice")).collect();
        told.push((t, twice));
    }
    assert!(told[0].1.is_empty(), "07:00 is the first refusal: {told:?}");
    assert_eq!(told[1].1.len(), 1, "07:30, the second refusal, is told once: {told:?}");
    let (text, opens) = &told[1].1[0];
    assert!(text.starts_with("alpha: \"pace check\" has failed twice: refused: agent alpha accepts only webhook messages"), "{text}");
    assert!(!text.contains("Claude agents take"), "the first sentence only: {text}");
    assert_eq!(*opens, Some(s.id));
    assert!(told[2].1.is_empty(), "08:00, the third, is not told again: {told:?}");
    // The window reads the row from the schedule: three refusals since 07:00.
    let j = schedule_json(&f.hub, &f.hub.schedules.get(s.id).unwrap());
    assert_eq!(j["failing"]["runs"], 3);
    assert_eq!(j["failing"]["outcome"], "refused");
    assert_eq!(j["failing"]["since"], json!(at(NY, "2026-11-25T07:00") + 500));
}

#[test]
fn a_row_of_failures_counts_what_ran() {
    let f = with_alpha(at(LONDON, "2026-10-19T08:00"));
    let s = f.create(json!({"agent": "alpha", "prompt": "p", "when": {"kind": "cron", "cron": "0 9 * * *"}})).unwrap();
    let alpha = f.hub.agents().get("alpha").unwrap();
    let mut slot = 0;
    let mut add = |outcome: Outcome, kind: RunKind, reply: Option<&str>| {
        slot += 1;
        let mut r = Run::new(s.id, slot, kind, slot);
        r.outcome = outcome;
        r.reason = Some(format!("why {slot}"));
        if let Some(text) = reply {
            let route = crate::agents::Route { origin: Origin::Schedule, hop: 0, reply_to: None };
            let m = f.hub.store().insert_agent_message(&crate::agents::message_record(&alpha, &s.sender(), None, route, "p")).unwrap();
            f.hub.store().set_agent_message_reply(m, text, 1).unwrap();
            r.msg_id = Some(m);
        }
        f.hub.store().insert_run(&r).unwrap()
    };
    let row = || failing_of(&f.hub, &f.hub.store().runs(s.id, None, 100).unwrap());
    add(Outcome::Replied, RunKind::Scheduled, None);
    add(Outcome::Blocked, RunKind::Scheduled, None);
    assert_eq!(row(), None, "one failure is not a row");
    // Skipped runs and "Then…" steps neither count nor break it.
    add(Outcome::Skipped, RunKind::Scheduled, None);
    add(Outcome::Failed, RunKind::Chain, None);
    let last = add(Outcome::TimedOut, RunKind::Manual, None);
    let r = row().expect("two in a row");
    assert_eq!((r.runs, r.outcome, r.reason.as_str(), r.since, r.last_run), (2, Outcome::TimedOut, "timed out: why 5", 2, last));
    // A run still going changes nothing.
    add(Outcome::Pending, RunKind::Scheduled, None);
    assert_eq!(row().map(|r| r.runs), Some(2));
    // A "no reply" run whose reply came late counts as replied: the row is broken.
    add(Outcome::NoReply, RunKind::Scheduled, Some("late answer"));
    assert_eq!(row(), None);
    add(Outcome::NoReply, RunKind::Scheduled, None);
    add(Outcome::Refused, RunKind::Scheduled, None);
    assert_eq!(row().map(|r| (r.runs, r.reason)), Some((2, "refused: why 9".to_string())));
    // The first sentence of a long reason, cut at 200 characters.
    let long = format!("{}. And more.", "x".repeat(300));
    let why = Failing::why(Outcome::Refused, Some(&long));
    assert!(why.starts_with("refused: xxx") && why.ends_with('…') && !why.contains("more"), "{why}");
    assert_eq!(Failing::why(Outcome::NoReply, None), "no reply");
    // A reason that already starts with the outcome doesn't say it twice.
    assert_eq!(Failing::why(Outcome::Blocked, Some("blocked on a prompt.")), "blocked on a prompt");
    assert_eq!(Failing::why(Outcome::Blocked, Some("Run this command? (rm x)")), "blocked: Run this command? (rm x)");
}

/// An expired schedule is told once per 7-day window, at expiry: not again after a restart, nor
/// at its next slot; renewing starts a new window.
#[test]
fn an_expired_schedule_is_told_once() {
    let made = at(LONDON, "2026-10-12T08:00");
    let f = Fixture::new(made);
    let s = f.create(json!({"agent": "cleo", "name": "digest", "prompt": "p", "tz": LONDON, "quiet_hours": false,
        "when": {"kind": "cron", "cron": "0 9 * * *"}})).unwrap();
    let mut rx = f.hub.subscribe();
    let expired = |rx: &mut tokio::sync::broadcast::Receiver<ServerEvent>| {
        notices(rx).into_iter().filter(|(t, _)| t.contains("expired")).collect::<Vec<_>>()
    };
    f.set(at(LONDON, "2026-10-19T08:00") - 1000);
    f.tick();
    notices(&mut rx);
    f.set(at(LONDON, "2026-10-19T08:00") + 1000);
    f.tick();
    let told = expired(&mut rx);
    assert_eq!(told, [("cleo: \"digest\" expired: renew or remove: Claude subscription agents' schedules stop after 7 days unless you renew them.".to_string(), Some(s.id))]);
    assert!(f.hub.schedules.get(s.id).unwrap().expired_told);
    // Its 09:00 slot is skipped as expired, without a second notice.
    f.set(at(LONDON, "2026-10-19T09:00") + 100);
    f.tick();
    assert_eq!(f.runs(s.id)[0].reason.as_deref(), Some(plan::EXPIRED));
    assert!(expired(&mut rx).is_empty());
    // A restart forgets what it saw, not what it told.
    f.hub.schedules.expired_seen.lock().clear();
    f.tick();
    assert!(expired(&mut rx).is_empty());
    // Renewed, it expires again 7 days later and is told again.
    renew(&f.hub, s.id, None).unwrap();
    assert!(!f.hub.schedules.get(s.id).unwrap().expired_told);
    f.set(at(LONDON, "2026-10-26T09:30"));
    f.tick();
    assert_eq!(expired(&mut rx).len(), 1);
}
