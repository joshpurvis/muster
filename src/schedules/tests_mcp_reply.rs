//! A run's reply through the agent's MCP `reply` tool (src/agent_reply.rs): an Antigravity agent's
//! turn leaves no final message, so its `reply` to the run's message is the run's reply, in the
//! turn or after the run stopped waiting, counted once, and read the same by every reader
//! (`{reply}`, `{last_reply}`, the note, the history). A reply that would bring an origin the
//! message's chain didn't have is not taken.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;

use serde_json::json;

use super::super::*;
use super::agent;
use crate::agent_reply::{self, AnswerStatus};
use crate::agents::{Agent, Origin, Prov, Route, Source};
use crate::openrouter::Provider;

/// Sessions 1 (Antigravity), 2 and 3 (Claude) and 4 (Codex), and their agents: `agy`, `cleo`,
/// `lena` (a lead) and `worker`, each taking scheduled work.
fn fixture() -> Arc<Hub> {
    let store = crate::store::Store::open_memory().unwrap();
    let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
    let mut ch = crate::mcp::guard::test_channel(0);
    ch.workspace_id = ws;
    store.insert_channel(&ch).unwrap();
    let kinds = [SessionKind::Antigravity, SessionKind::Claude, SessionKind::Claude, SessionKind::Codex];
    for kind in kinds {
        let mut info = crate::mcp::guard::test_session(0, None);
        info.kind = kind;
        store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
    }
    let all = vec![Origin::Human, Origin::Schedule, Origin::Webhook];
    let sched = vec![Origin::Human, Origin::Schedule];
    store.insert_agent(&agent("agy", SessionKind::Antigravity, Provider::Subscription, all.clone())).unwrap();
    store.insert_agent(&agent("cleo", SessionKind::Claude, Provider::Subscription, sched.clone())).unwrap();
    store.insert_agent(&Agent { lead: true, ..agent("lena", SessionKind::Claude, Provider::Subscription, sched) }).unwrap();
    store.insert_agent(&agent("worker", SessionKind::Codex, Provider::Subscription, all)).unwrap();
    let hub = Hub::new(
        crate::hub::HubConfig {
            port: 0,
            client_token: "fixture".into(),
            muster_bin: "/bin/true".into(),
            terminal_host: None,
            openrouter_key: None,
        },
        store,
    )
    .unwrap();
    hub.schedules.set_clock(Arc::new(FakeClock(AtomicI64::new(now_ms()))));
    for (name, sid) in [("agy", 1), ("cleo", 2), ("lena", 3), ("worker", 4)] {
        let a = hub.agents().get(name).unwrap();
        hub.agents().link_session(hub.store(), a.id, Some(sid)).unwrap();
    }
    hub
}

/// A daily schedule on `agent` that puts each reply in a note and asks `worker` next.
fn schedule(hub: &Hub, agent: &str) -> Schedule {
    let v = create(hub, serde_json::from_value(json!({
        "agent": agent, "prompt": "Pace check", "when": {"kind": "cron", "cron": "0 9 * * *"},
        "note": {"title": format!("{agent} pace")},
        "then": {"on_success": {"agent": "worker", "prompt": "Got: {reply}"}},
    })).unwrap()).unwrap();
    hub.schedules.get(v["id"].as_i64().unwrap()).unwrap()
}

/// Run `s`'s message (route `origin`, hop 0), typed into its agent's session 10 s ago: its run,
/// pending, and the message's id. The session is handling it (its provenance is the message's).
fn sent(hub: &Hub, s: &Schedule, origin: Origin) -> (Run, i64) {
    let a = hub.agents().by_id(s.agent_id).unwrap();
    let sid = a.session_id.unwrap();
    let at = now_ms() - 10_000;
    let m = crate::agents::message_record(&a, &s.sender(), None, Route { origin, hop: 0, reply_to: None }, "Pace check");
    let msg = hub.store().insert_agent_message(&m).unwrap();
    hub.store().set_agent_message_delivered(msg, at).unwrap();
    hub.agents().on_delivered(sid, Source::Chain(Prov { origin, hop: 0, msg_id: Some(msg) }), at).unwrap();
    let mut run = Run::new(s.id, hub.schedules.now(), RunKind::Scheduled, hub.schedules.now());
    run.msg_id = Some(msg);
    run.id = hub.store().insert_run(&run).unwrap();
    (run, msg)
}

/// The agent of session `sid` is in its turn (`true`) or went idle 5 s ago.
fn turn(hub: &Hub, sid: SessionId, working: bool) {
    hub.edit_session_for_test(sid, |i| {
        i.state = if working { AgentState::Working } else { AgentState::Idle };
        i.state_since = if working { now_ms() } else { now_ms() - 5_000 };
    });
}

/// Wait for run `run`'s reply in a task of its own, as `execute` does.
fn wait(hub: &Arc<Hub>, s: &Schedule, run: Run) -> tokio::task::JoinHandle<Run> {
    let (h, s) = (hub.clone(), s.clone());
    tokio::spawn(async move {
        let mut r = run;
        run::wait_reply(&h, &s, &mut r).await.unwrap();
        r
    })
}

fn note_body(hub: &Hub, s: &Schedule) -> String {
    crate::notes::get(hub, s.note_id.unwrap()).unwrap().body
}

fn stored(hub: &Hub, run: &Run) -> Run {
    hub.store().run(run.id).unwrap().unwrap()
}

/// The `{reply}` the run's "Then…" step is asked with.
fn step_text(hub: &Hub, s: &Schedule, run: &Run) -> String {
    let step = hub.store().runs(s.id, None, 50).unwrap().into_iter()
        .find(|r| r.kind == RunKind::Chain && r.parent == Some(run.id))
        .expect("a chain step");
    run::step_context(hub, s, &step, &jiff::tz::TimeZone::UTC).unwrap().0
}

#[tokio::test]
async fn an_antigravity_agents_reply_in_the_turn_is_the_runs_reply() {
    let hub = fixture();
    let s = schedule(&hub, "agy");
    let (run, msg) = sent(&hub, &s, Origin::Schedule);
    turn(&hub, 1, true);
    let waiting = wait(&hub, &s, run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let v = agent_reply::reply(&hub, 1, msg, "pace ok: 3 of 5 done".into(), true).await.unwrap();
    assert_eq!(v["status"], "returned", "{v}");
    let run = waiting.await.unwrap();
    assert_eq!((stored(&hub, &run).outcome, stored(&hub, &run).reason), (Outcome::Replied, None));
    let j = run_json(&hub, &run);
    assert_eq!((j["outcome"].as_str(), j["late_reply"].as_bool()), (Some("replied"), Some(false)), "{j}");
    // Every reader sees it: the note (once), `{last_reply}`, the step's `{reply}`.
    assert_eq!(note_body(&hub, &s).matches("pace ok: 3 of 5 done").count(), 1);
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1.as_deref(), Some("pace ok: 3 of 5 done"));
    assert!(step_text(&hub, &s, &run).contains("> pace ok: 3 of 5 done"));
    // Still in the turn (agy reports no turn end), but the run got its reply: not still going.
    assert!(!run::previous_running(&hub, s.id, i64::MAX).unwrap());
    // Its reply is the message's one reply; nothing in the message log says it went elsewhere.
    let m = hub.store().agent_message(msg).unwrap().unwrap();
    assert_eq!((m.reply, m.answer.unwrap().status), (None, AnswerStatus::Returned));
}

#[tokio::test]
async fn a_reply_just_after_the_turn_went_idle_is_a_late_reply() {
    let hub = fixture();
    let s = schedule(&hub, "agy");
    let (run, msg) = sent(&hub, &s, Origin::Schedule);
    turn(&hub, 1, false);
    let run = wait(&hub, &s, run).await.unwrap();
    assert_eq!(stored(&hub, &run).outcome, Outcome::NoReply);
    assert_eq!(note_body(&hub, &s), "");
    // A moment later the agent replies: the run's reply, late, as a late final message is.
    let v = agent_reply::reply(&hub, 1, msg, "pace ok".into(), true).await.unwrap();
    assert_eq!(v["status"], "returned", "{v}");
    let j = run_json(&hub, &stored(&hub, &run));
    assert_eq!((j["outcome"].as_str(), j["late_reply"].as_bool()), (Some("replied"), Some(true)), "{j}");
    assert_eq!(note_body(&hub, &s).matches("pace ok").count(), 1);
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1.as_deref(), Some("pace ok"));
    // The run itself stays as it was recorded (its failure branch, if any, already ran).
    assert_eq!(stored(&hub, &run).outcome, Outcome::NoReply);
    // A second note entry never comes (a final message arriving after it, say).
    on_reply(&hub, msg);
    assert_eq!(note_body(&hub, &s).matches("pace ok").count(), 1);
}

#[tokio::test]
async fn a_lead_agents_reply_to_its_scheduled_work_is_the_runs_reply() {
    let hub = fixture();
    let s = schedule(&hub, "lena");
    let (run, msg) = sent(&hub, &s, Origin::Schedule);
    turn(&hub, 3, true);
    let waiting = wait(&hub, &s, run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let v = agent_reply::reply(&hub, 3, msg, "lead: all on pace".into(), true).await.unwrap();
    assert_eq!((v["status"].as_str(), v["origin"].as_str()), (Some("returned"), Some("lead")), "{v}");
    let run = waiting.await.unwrap();
    assert_eq!(stored(&hub, &run).outcome, Outcome::Replied);
    // The step stays on the schedule's chain: the lead's lift doesn't pass into it.
    let step = hub.store().runs(s.id, None, 50).unwrap().into_iter().find(|r| r.kind == RunKind::Chain).unwrap();
    let (text, prov) = run::step_context(&hub, &s, &step, &jiff::tz::TimeZone::UTC).unwrap();
    assert!(text.contains("> lead: all on pace"), "{text}");
    assert_eq!(prov.origin, Origin::Schedule);
}

#[tokio::test]
async fn a_claude_agents_final_message_still_is_the_runs_reply() {
    let hub = fixture();
    let s = schedule(&hub, "cleo");
    let (run, msg) = sent(&hub, &s, Origin::Schedule);
    turn(&hub, 2, true);
    let waiting = wait(&hub, &s, run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The Stop hook's final message (`Registry::on_hook`, then `schedules::on_reply`).
    hub.store().set_agent_message_reply(msg, "final: on pace", now_ms()).unwrap();
    on_reply(&hub, msg);
    let run = waiting.await.unwrap();
    assert_eq!(stored(&hub, &run).outcome, Outcome::Replied);
    assert_eq!(note_body(&hub, &s).matches("final: on pace").count(), 1);
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1.as_deref(), Some("final: on pace"));
    assert!(step_text(&hub, &s, &run).contains("> final: on pace"));
    // A `reply` after it is shown in the log, as before: the run had its reply.
    let v = agent_reply::reply(&hub, 2, msg, "also this".into(), true).await.unwrap();
    assert_eq!(v["status"], "shown", "{v}");
    let m = hub.store().agent_message(msg).unwrap().unwrap();
    assert_eq!(m.run_reply(), Some("final: on pace"));
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1.as_deref(), Some("final: on pace"));
    assert!(!note_body(&hub, &s).contains("also this"));
}

/// A Claude agent that answers with `reply` and then ends its turn with a final message: the run
/// counts the `reply` it took, once, and every reader agrees.
#[tokio::test]
async fn a_reply_and_a_final_message_count_once() {
    let hub = fixture();
    let s = schedule(&hub, "cleo");
    let (run, msg) = sent(&hub, &s, Origin::Schedule);
    turn(&hub, 2, true);
    let waiting = wait(&hub, &s, run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let v = agent_reply::reply(&hub, 2, msg, "the answer".into(), true).await.unwrap();
    assert_eq!(v["status"], "returned", "{v}");
    let run = waiting.await.unwrap();
    hub.store().set_agent_message_reply(msg, "I sent it with the reply tool.", now_ms()).unwrap();
    on_reply(&hub, msg);
    assert_eq!(stored(&hub, &run).outcome, Outcome::Replied);
    let body = note_body(&hub, &s);
    assert_eq!((body.matches("the answer").count(), body.contains("reply tool")), (1, false), "{body}");
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1.as_deref(), Some("the answer"));
    assert!(step_text(&hub, &s, &run).contains("> the answer"));
    assert_eq!(hub.store().runs(s.id, None, 50).unwrap().iter().filter(|r| r.kind == RunKind::Chain).count(), 1);
}

/// A trigger's message (`webhook`) answered while the agent handles scheduled work: the reply is
/// rooted in both, so neither the waiting run nor a late look takes it; it is only shown.
#[tokio::test]
async fn a_reply_bringing_another_origin_is_not_the_runs_reply() {
    let hub = fixture();
    let s = schedule(&hub, "agy");
    let (run, msg) = sent(&hub, &s, Origin::Webhook);
    hub.agents().set_prov(1, Some(Prov { origin: Origin::Schedule, hop: 0, msg_id: None })).unwrap();
    turn(&hub, 1, true);
    let waiting = wait(&hub, &s, run);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let v = agent_reply::reply(&hub, 1, msg, "from elsewhere".into(), true).await.unwrap();
    assert_eq!(v["status"], "shown", "{v}");
    turn(&hub, 1, false);
    let run = waiting.await.unwrap();
    assert_eq!(stored(&hub, &run).outcome, Outcome::NoReply);
    let j = run_json(&hub, &stored(&hub, &run));
    assert_eq!((j["outcome"].as_str(), j["late_reply"].as_bool()), (Some("no_reply"), Some(false)), "{j}");
    assert_eq!(note_body(&hub, &s), "");
    assert_eq!(hub.store().last_runs_for_prompt(s.id, i64::MAX).unwrap().1, None);
    assert_eq!(hub.store().agent_message(msg).unwrap().unwrap().run_reply(), None);
}

/// A reply kept before a restart and delivered at the next start: the run's, late.
#[tokio::test]
async fn a_reply_finished_after_a_restart_is_the_runs_late_reply() {
    let hub = fixture();
    let s = schedule(&hub, "agy");
    let (mut run, msg) = sent(&hub, &s, Origin::Schedule);
    run::finish(&hub, &s, &mut run, Outcome::NoReply, Some("the turn ended without a final message for this run".into())).unwrap();
    let m = hub.store().agent_message(msg).unwrap().unwrap();
    let now = Some(Prov { origin: Origin::Schedule, hop: 0, msg_id: Some(msg) });
    let route = agent_reply::answer_route(&m, now, false);
    let a = agent_reply::Answer {
        text: "kept across".into(), at: now_ms(), from_session: 1, origin: route.origin, hop: route.hop,
        origins: agent_reply::answer_origins(&m, now, false), status: AnswerStatus::Pending, note: None, msg: None,
        session: None, quiet: false,
    };
    assert!(hub.store().set_agent_answer(msg, &a).unwrap());
    agent_reply::resume(&hub, agent_reply::unfinished(&hub)).await;
    let m = hub.store().agent_message(msg).unwrap().unwrap();
    assert_eq!(m.answer.as_ref().unwrap().status, AnswerStatus::Returned);
    assert_eq!(run_json(&hub, &stored(&hub, &run))["late_reply"], true);
    assert_eq!(note_body(&hub, &s).matches("kept across").count(), 1);
}
