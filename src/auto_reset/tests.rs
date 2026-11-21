//! Idle agent resets: one test per line of the rule, at the sweep (the verdict's own lines are
//! src/relay.rs's tests). The fixture has no terminal host, so its agents rest asleep: a running
//! agent's restart is the same start the cold-answer rule uses (its tests in src/questions.rs).

use super::*;
use crate::agents::{NewAgent, Source};

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

/// Session 1 Claude and 2 Codex, both in channel 1.
fn fixture() -> Arc<Hub> {
    let store = crate::store::Store::open_memory().unwrap();
    let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
    let mut ch = crate::mcp::guard::test_channel(0);
    ch.workspace_id = ws;
    store.insert_channel(&ch).unwrap();
    for kind in [SessionKind::Claude, SessionKind::Codex] {
        let mut info = crate::mcp::guard::test_session(0, None);
        info.kind = kind;
        store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
    }
    let cfg = crate::hub::HubConfig {
        port: 0,
        client_token: "t".into(),
        muster_bin: std::path::PathBuf::from("/bin/true"),
        terminal_host: None,
        openrouter_key: None,
    };
    Hub::new(cfg, store).unwrap()
}

fn turn_on(hub: &Hub, on: bool) {
    hub.set_ui_prefs(UiPrefsChange { auto_reset: Some(on), ..Default::default() }).unwrap();
}

/// Agent `name` of `kind` on session `sid`, asleep in conversation "conv-old", its last turn
/// `idle` before `now`, its 60-minute cache gone once that is over an hour, with `tokens`
/// reported for that conversation (None: never reported).
fn resting(hub: &Arc<Hub>, name: &str, kind: SessionKind, sid: SessionId, tokens: Option<u64>, idle: i64, now: i64) -> Agent {
    let a = crate::agents::create(hub, NewAgent {
        working_location: None, account_profile: None, provider: None, name: name.into(), kind: Some(kind),
        model: None, effort: None, persona: None, accepts: None, permissions: None, autostart: false, sleep_after_min: None,
    })
    .unwrap();
    hub.agents().link_session(hub.store(), a.id, Some(sid)).unwrap();
    hub.agents().update(hub.store(), a.id, |b| b.resume_id = Some("conv-old".into())).unwrap();
    hub.agents().set_asleep(hub.store(), a.id, Some(now - idle), None).unwrap();
    if let Some(t) = tokens {
        hub.agents().note_context(a.id, "conv-old", t, 1, 1.0).unwrap();
    }
    hub.edit_session_for_test(sid, |i| {
        i.state = AgentState::Exited;
        i.state_since = now - idle;
        i.last_turn_at = Some(now - idle);
        i.cache_cold_at = Some(now - idle + 60 * MIN);
    });
    hub.agents().by_id(a.id).unwrap()
}

fn claude(hub: &Arc<Hub>, tokens: Option<u64>, idle: i64, now: i64) -> Agent {
    resting(hub, "alpha", SessionKind::Claude, 1, tokens, idle, now)
}

fn resume_of(hub: &Hub, a: &Agent) -> Option<String> {
    hub.agents().by_id(a.id).unwrap().resume_id
}

/// Sweep once at `now`: whether `a` was reset (and, when it was, that its conversation is gone).
async fn reset(hub: &Arc<Hub>, a: &Agent, now: i64) -> bool {
    let done = sweep(hub, now).await;
    let hit = done.iter().any(|r| r.agent == a.name);
    assert_eq!(resume_of(hub, a).is_none(), hit, "{done:?}");
    hit
}

#[tokio::test]
async fn cold_resets_from_100k() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let a = claude(&hub, Some(99_999), 3 * HOUR, now);
    assert!(!reset(&hub, &a, now).await);
    hub.agents().note_context(a.id, "conv-old", 100_000, 1, 2.0).unwrap();
    assert!(reset(&hub, &a, now).await);
}

#[tokio::test]
async fn warm_resets_from_200k_after_the_grace() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    // 20 minutes idle: the 60-minute cache is still warm.
    let a = claude(&hub, Some(199_999), 20 * MIN, now);
    assert!(!reset(&hub, &a, now).await);
    hub.agents().note_context(a.id, "conv-old", 200_000, 1, 2.0).unwrap();
    // Within the grace after its last turn: kept, then reset once it has passed.
    let early = now - 20 * MIN + crate::cache::FRESH_WARM_AFTER_MS - MIN;
    assert!(!reset(&hub, &a, early).await);
    assert!(reset(&hub, &a, now).await);
}

#[tokio::test]
async fn the_humans_prompt_in_the_last_24_hours_keeps_it() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let a = claude(&hub, Some(216_003), 3 * HOUR, now);
    hub.agents().on_delivered(1, Source::Human, now - 23 * HOUR).unwrap();
    assert!(!reset(&hub, &a, now).await);
    // A day and more ago no longer counts.
    hub.agents().on_delivered(1, Source::Human, now - 25 * HOUR).unwrap();
    assert!(reset(&hub, &a, now).await);
}

#[tokio::test]
async fn a_message_on_its_way_keeps_it() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let a = claude(&hub, Some(216_003), 3 * HOUR, now);
    let on_its_way = crate::sleep::delivering(&hub, a.id);
    assert!(!reset(&hub, &a, now).await);
    drop(on_its_way);
    assert!(reset(&hub, &a, now).await);
}

#[tokio::test]
async fn a_codex_agent_and_an_unknown_size_are_kept() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let codex = resting(&hub, "beta", SessionKind::Codex, 2, Some(500_000), 3 * HOUR, now);
    let unknown = claude(&hub, None, 3 * HOUR, now);
    assert!(sweep(&hub, now).await.is_empty());
    assert_eq!(resume_of(&hub, &codex).as_deref(), Some("conv-old"));
    assert_eq!(resume_of(&hub, &unknown).as_deref(), Some("conv-old"));
    // A size reported for another conversation is unknown for this one.
    hub.agents().note_context(unknown.id, "conv-other", 500_000, 1, 2.0).unwrap();
    assert!(!reset(&hub, &unknown, now).await);
}

#[tokio::test]
async fn with_the_setting_off_nothing_is_reset() {
    let now = now_ms();
    let hub = fixture();
    assert!(!hub.ui_prefs().auto_reset, "off by default");
    let a = claude(&hub, Some(216_003), 3 * HOUR, now);
    assert!(!reset(&hub, &a, now).await);
    turn_on(&hub, true);
    turn_on(&hub, false);
    assert!(!reset(&hub, &a, now).await);
}

#[tokio::test]
async fn a_full_daily_cap_keeps_it_and_records_nothing() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let a = claude(&hub, Some(216_003), 3 * HOUR, now);
    let route = Route { origin: Origin::Human, hop: 0, reply_to: None };
    for _ in 0..crate::mcp::guard::AGENT_DAILY_PROMPT_CAP {
        hub.store().insert_agent_message(&agents::message_record(&a, "lead", None, route, "x")).unwrap();
    }
    let before = hub.store().count_agent_messages_since(a.id, 0).unwrap();
    assert!(!reset(&hub, &a, now).await);
    // Swept again and again, it adds no refused rows to the log.
    assert!(!reset(&hub, &a, now).await);
    assert_eq!(hub.store().count_agent_messages_since(a.id, 0).unwrap(), before);
}

#[tokio::test]
async fn a_reset_leaves_a_note_and_a_log_line_once() {
    let now = now_ms();
    let hub = fixture();
    turn_on(&hub, true);
    let a = claude(&hub, Some(182_400), 10 * HOUR, now);
    let mut rx = hub.subscribe();
    let done = sweep(&hub, now).await;
    let why = "the old one was cold and large, about 182k tokens and idle 10 h";
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!((done[0].why.as_str(), done[0].asleep), (why, true));
    // The note: a quiet message from Muster, the first thing the new conversation reads.
    let m = hub.store().agent_message(done[0].note.expect("a note")).unwrap().unwrap();
    assert!(m.quiet && m.from == FROM && m.origin == Origin::Human && m.hop == 0, "{m:?}");
    assert_eq!(m.text.as_deref(), Some(format!("Muster started you in a new conversation while you were idle, because {why}. \
        Read your memory/ notes first, before anything else.").as_str()));
    // The log line in its tab.
    let ev = loop {
        if let crate::hub::ServerEvent::Event(e) = rx.recv().await.unwrap()
            && e.data.get("auto_reset").is_some()
        {
            break e;
        }
    };
    assert_eq!(ev.session_id, Some(1));
    assert_eq!(ev.data["text"], format!("Idle agent reset: alpha's next start is a new conversation, because {why}. \
        It stays asleep until something wakes it; its memory/ notes are kept."));
    assert_eq!(ev.data["auto_reset"]["size_k"], 182);
    assert_eq!(ev.data["auto_reset"]["replaces"], "conv-old");
    // Its next sweep finds nothing to do: the new conversation's size is unknown until reported.
    assert!(sweep(&hub, now + HOUR).await.is_empty());
}

#[tokio::test]
async fn a_reset_a_restart_cut_off_gives_the_conversation_back() {
    let now = now_ms();
    let hub = fixture();
    let a = claude(&hub, Some(216_003), 3 * HOUR, now);
    // Saved, then the start forgot the conversation, then the backend stopped: no new run.
    let fresh = Fresh { why: "the old one was cold and large".into(), replaces: Some("conv-old".into()), run: Some(7) };
    set_pending(&hub, a.id, Some(Pending { created_at: a.created_at, size_k: Some(216), fresh })).unwrap();
    hub.agents().update(hub.store(), a.id, |b| b.resume_id = None).unwrap();
    settle(&hub).await;
    assert_eq!(resume_of(&hub, &a).as_deref(), Some("conv-old"));
    assert!(pending(&hub).is_empty());
}

#[test]
fn the_rule_lines_are_named_constants() {
    assert_eq!((crate::cache::FRESH_ANSWER_MIN_TOKENS, crate::cache::FRESH_WARM_MIN_TOKENS), (100_000, 200_000));
    assert_eq!(crate::cache::FRESH_WARM_AFTER_MS, 15 * MIN);
    assert_eq!(SWEEP_EVERY, Duration::from_secs(300));
}
