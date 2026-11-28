//! Replay a Claude turn from the injector's paste and Enter through the hook helper and
//! daemon HTTP routes. The terminal host and Claude's rendering of the paste are fixtures.

use super::*;
use crate::agents::{self, Agent, Origin, Route};
use crate::terminal::testing::{Received, serving_host};
use serde_json::{Value, json};

struct Replay {
    hub: Arc<Hub>,
    input: Arc<Received>,
    url: String,
}

impl Replay {
    async fn new() -> Self {
        let (host, directory, input) = serving_host("quiet-depth");
        let hub = fixture_with(Some(host.clone()), |store| {
            let info = crate::mcp::guard::test_session(0, None);
            store.insert_session(&StoredSession { info, extra_args: vec![] }).unwrap();
        });
        for (id, name) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
            hub.edit_session_for_test(id, |s| { s.kind = SessionKind::Claude; s.state = AgentState::Idle; });
            install_terminal(&hub, &host, id, 1, &directory);
            hub.inner.lock().tokens.insert(name.into(), id);
            let agent = agents::create(&hub, serde_json::from_value(json!({"name": name})).unwrap()).unwrap();
            hub.agents.link_session(&hub.store, agent.id, Some(id)).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::server::router(hub.clone()).unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
        });
        Self { hub, input, url }
    }

    async fn hook(&self, name: &str, event: &'static str, payload: Value) -> Vec<u8> {
        let (url, token) = (self.url.clone(), name.to_owned());
        tokio::task::spawn_blocking(move || {
            let mut printed = Vec::new();
            crate::hookcmd::forward("claude", event, url, token, &serde_json::to_vec(&payload).unwrap(), &mut printed);
            printed
        }).await.unwrap()
    }

    /// Only supply the Claude callback after the production injector actually submitted.
    async fn submitted_turn(&self, to: &Agent) -> Vec<u8> {
        let id = to.session_id.unwrap();
        let paste = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                {
                    let mut inputs = self.input.inputs.lock();
                    if inputs.last().is_some_and(|(run, bytes)| run.id == id && bytes == b"\r") {
                        assert!(inputs.iter().all(|(run, _)| run.id == id));
                        let bytes: Vec<u8> = inputs.drain(..).flat_map(|(_, bytes)| bytes).collect();
                        let text = String::from_utf8(bytes).unwrap();
                        break text.strip_prefix("\x1b[200~").unwrap().strip_suffix("\x1b[201~\r").unwrap().to_owned();
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("injector did not submit the prompt");
        let prompt = format!("\n\n<pasted_content id=\"abcd\">\n{paste}\n</pasted_content id=\"abcd\">\n");
        self.hook(&to.name, "UserPromptSubmit", json!({"prompt": prompt, "permission_mode": "default"})).await
    }

    fn earlier_question(&self, from: &Agent, to: &Agent, hop: u32) -> i64 {
        let route = Route { origin: Origin::Human, hop, reply_to: None };
        let msg = agents::message_record(to, &from.name, from.session_id, route, "Any news?");
        let id = self.hub.store.insert_agent_message(&msg).unwrap();
        self.hub.store.set_agent_message_delivered(id, now_ms() - 60_000).unwrap();
        id
    }

    async fn send(&self, from: &Agent, to: &Agent) -> anyhow::Result<(agents::Delivery, Route)> {
        let prov = self.hub.agents.send_prov(from.session_id.unwrap());
        agents::send_checked(&self.hub, to, "Next step".into(), from.name.clone(), from.session_id, prov, None).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_quiet_reply_waits_through_a_long_waking_reply() {
    let replay = Replay::new().await;
    let hub = &replay.hub;
    let [alpha, beta, gamma] = ["alpha", "beta", "gamma"].map(|name| hub.agents.get(name).unwrap());
    let row = |id| hub.store.agent_message(id).unwrap().unwrap();

    let deep_ask = replay.earlier_question(&alpha, &gamma, 7);
    let deep = crate::agent_reply::reply(hub, 3, deep_ask, "Deep note".into(), false).await.unwrap();
    let note = deep["delivered_as"].as_i64().unwrap();
    assert!(row(note).quiet && row(note).hop == 8);

    let other = replay.earlier_question(&gamma, &alpha, 1);
    let ask = replay.earlier_question(&alpha, &beta, 1);
    let (answer, context) = tokio::join!(
        crate::agent_reply::reply(hub, 2, ask, "result ".repeat(700), true),
        replay.submitted_turn(&alpha),
    );
    let wake = answer.unwrap()["delivered_as"].as_i64().unwrap();
    assert_eq!((row(wake).reply_to, row(wake).hop), (Some(ask), 2));
    assert!(hub.store.agent_message_delivered_at(wake).unwrap().is_some());
    assert!(context.is_empty(), "the hop-2 wake showed the waiting hop-8 note");
    assert_eq!(hub.agents.prov(1), Some(agents::Prov { origin: Origin::Human, hop: 2, msg_id: Some(wake) }));
    assert!(hub.agents.msg_status(&hub.store, wake).active);

    let (send, _) = tokio::join!(replay.send(&alpha, &beta), replay.submitted_turn(&beta));
    let first = row(send.unwrap().0.msg_id);
    assert_eq!((first.hop, first.reply_to), (3, Some(wake)));
    let (reply, _) = tokio::join!(
        crate::agent_reply::reply(hub, 1, other, "Follow-up".into(), true),
        replay.submitted_turn(&gamma),
    );
    let followup = row(reply.unwrap()["delivered_as"].as_i64().unwrap());
    assert_eq!((followup.hop, followup.reply_to), (3, Some(other)));

    let mut parent = first.id;
    for depth in 4..=8 {
        let (from, to) = if depth % 2 == 0 { (&beta, &alpha) } else { (&alpha, &beta) };
        replay.hook(&to.name, "Stop", json!({"last_assistant_message": "Done"})).await;
        let (send, context) = tokio::join!(replay.send(from, to), replay.submitted_turn(to));
        let m = row(send.unwrap().0.msg_id);
        assert_eq!((m.hop, m.reply_to), (depth, Some(parent)));
        parent = m.id;
        if depth < 8 {
            assert!(context.is_empty() && row(note).shown_at.is_none());
        } else {
            let context: Value = serde_json::from_slice(&context).unwrap();
            assert!(context["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("Deep note"));
            replay.hook("alpha", "PreToolUse", json!({"tool_name": "Read"})).await;
            assert!(row(note).shown_at.is_some());
        }
    }
    // Even a later Agent-tool hand-back can carry that deeper work.
    replay.hook("alpha", "Stop", json!({"last_assistant_message": "Done"})).await;
    replay.hook("alpha", "UserPromptSubmit", hand_back()).await;
    assert!(replay.send(&alpha, &beta).await.unwrap_err().to_string().contains("hop"));
    let refused = hub.store.agent_messages(Some(beta.id), None, 1).unwrap().pop().unwrap();
    assert_eq!((refused.hop, refused.reply_to), (9, Some(parent)));
    assert!(refused.refused.is_some());
}

fn hand_back() -> Value {
    json!({"prompt": "<task-notification>\n<task-id>background-task</task-id>\n<status>completed</status>\n</task-notification>"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hand_back_keeps_the_last_route_and_counts_a_note_it_actually_shows() {
    let replay = Replay::new().await;
    let hub = &replay.hub;
    let [alpha, beta, gamma] = ["alpha", "beta", "gamma"].map(|name| hub.agents.get(name).unwrap());
    let ask = replay.earlier_question(&alpha, &beta, 1);
    let (answer, _) = tokio::join!(
        crate::agent_reply::reply(hub, 2, ask, "Ready".into(), true),
        replay.submitted_turn(&alpha),
    );
    let wake = answer.unwrap()["delivered_as"].as_i64().unwrap();
    replay.hook("alpha", "Stop", json!({"last_assistant_message": "Done"})).await;
    assert!(replay.hook("alpha", "UserPromptSubmit", hand_back()).await.is_empty());
    let (send, _) = tokio::join!(replay.send(&alpha, &beta), replay.submitted_turn(&beta));
    let route = send.unwrap().1;
    assert_eq!((route.hop, route.reply_to), (3, Some(wake)));

    // With a deep human-origin note waiting, an envelope-less prompt can receive it.
    let ask = replay.earlier_question(&alpha, &gamma, 7);
    let answer = crate::agent_reply::reply(hub, 3, ask, "Deep note".into(), false).await.unwrap();
    let note = answer["delivered_as"].as_i64().unwrap();
    replay.hook("alpha", "Stop", json!({"last_assistant_message": "Done"})).await;
    assert!(!replay.hook("alpha", "UserPromptSubmit", hand_back()).await.is_empty());
    replay.hook("alpha", "PreToolUse", json!({"tool_name": "Read"})).await;
    assert!(hub.store.agent_message(note).unwrap().unwrap().shown_at.is_some());
    assert_eq!(hub.agents.send_prov(1).unwrap().hop, 8);
}
