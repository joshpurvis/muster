//! Late replies (PLAN.md §10, docs/MCP.md "Late replies"): the agent a message was delivered to
//! answers it after the turn that received it, for example once a long job finishes (MCP `reply`;
//! `muster reply` inside its session runs that tool).
//!
//! Only that agent may reply, once per message, and only to a message Muster actually typed into
//! it. The answer is stored on the message's row (`agent_messages.answer`) before anything else,
//! then reaches the asker:
//! - an ask still waiting for that message (`ask_agent`, `agent.ask`) returns it as its reply;
//! - a named agent gets it as a new agent message (`[muster msg <new id> …]`, `reply_to` = the
//!   message answered) through `agents::send_checked`, so the origin rule, hop limit, both daily
//!   caps, the spend caps and the injection queue apply, and a stopped agent starts as for any
//!   message;
//! - another Claude or Codex session gets it typed in through its injection queue after a
//!   `[muster reply to msg <id> …]` header, under `send_message`'s Claude origin rule;
//! - the human (also through a shell tab) or a schedule sees it in the agents panel's message
//!   log, with a desktop notification when the human asked.
//!
//! Its route is the stricter of the message's chain and the replier's current one (a non-human
//! origin wins, so does the higher hop), one hop on: a reply chain ends at the hop limit like any
//! other. A restart between storing and delivering is finished at the next start (`resume`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agents::{self, Agent, AgentMessage, MsgStatus, Origin, Prov, Route, Source};
use crate::hub::Hub;
use crate::model::*;

/// How a late reply stands. `Pending` and `Delivering` are unfinished (a restart resumes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    /// Stored; nothing has taken it yet.
    Pending,
    /// Being delivered to the asker.
    Delivering,
    /// The ask that was waiting for the message returned it.
    Returned,
    /// Delivered to the asker (`msg` for an agent, `session` for where it was typed).
    Delivered,
    /// In the agents panel's log only: the human, a shell or a schedule asked.
    Shown,
    /// A guard refused the delivery (`note` says which). Still kept here.
    Refused,
    /// It couldn't be delivered (`note`: the asker is gone, an error). Still kept here.
    Undelivered,
}

/// An agent's later answer to a message, as stored on that message's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub text: String,
    /// When the agent replied (unix ms).
    pub at: i64,
    /// The session that replied.
    pub from_session: SessionId,
    /// The route the reply carries (one hop past the message's chain or the replier's).
    pub origin: Origin,
    pub hop: u32,
    pub status: AnswerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The message that carried it to an asking agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<i64>,
    /// The session it was typed into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

/// The longest a reply waits for an ask that is (or is about to be) waiting for its message to
/// take it. A waiting ask re-checks every second; one still delivering its message (typing it
/// in and confirming it, a few seconds) starts waiting once that returns; one that goes away
/// stops counting at once.
pub const WAITER_MAX: Duration = Duration::from_secs(30);
/// The question is quoted in a delivered reply up to this many characters.
const QUOTE_MAX: usize = 80;

/// What a timed-out ask says about late replies.
pub const TIMED_OUT_NOTE: &str = "no reply yet; the message stays delivered. If the agent answers later with its reply tool, the answer comes to you as a new message (\"Late reply to your message <msg_id>\"), or, if the human asked, in the agents panel's message log";

/// The asks in progress (in memory: no ask survives a restart): those waiting for a message's
/// turn, and those still delivering theirs (the message id isn't theirs to know yet).
#[derive(Default, Clone)]
pub struct Waiters(Arc<Mutex<HashMap<WaitKey, u32>>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WaitKey {
    Msg(i64),
    /// An ask from this session (None: the human) to this agent, before its delivery returned.
    Asking(i64, Option<SessionId>),
}

impl Waiters {
    fn hold(&self, key: WaitKey) -> WaitGuard {
        *self.0.lock().entry(key).or_default() += 1;
        WaitGuard { waiters: self.clone(), key }
    }

    /// An ask waits for message `msg` until the guard is dropped.
    pub fn wait(&self, msg: i64) -> WaitGuard {
        self.hold(WaitKey::Msg(msg))
    }

    /// An ask from `from` to agent `agent` is delivering its message (until the guard drops).
    pub fn asking(&self, agent: i64, from: Option<SessionId>) -> WaitGuard {
        self.hold(WaitKey::Asking(agent, from))
    }

    /// Could an ask be (about to be) waiting for message `m`?
    pub fn may_wait(&self, m: &AgentMessage) -> bool {
        let g = self.0.lock();
        g.contains_key(&WaitKey::Msg(m.id)) || g.contains_key(&WaitKey::Asking(m.to_agent, m.from_session))
    }
}

pub struct WaitGuard {
    waiters: Waiters,
    key: WaitKey,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut g = self.waiters.0.lock();
        if let Some(n) = g.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                g.remove(&self.key);
            }
        }
    }
}

/// `Registry::msg_status` for an ask waiting on message `msg`: a late reply nothing else has
/// taken yet becomes this ask's reply.
pub fn status(hub: &Hub, msg: i64) -> MsgStatus {
    let mut s = hub.agents().msg_status(hub.store(), msg);
    let taken = hub.store().update_agent_answer(msg, &[AnswerStatus::Pending, AnswerStatus::Returned], |a| {
        a.status = AnswerStatus::Returned;
        a.note = Some("returned to the ask that was waiting for it".into());
    });
    match taken {
        Ok(Some((before, after))) => {
            if before != after {
                agents::publish_message(hub, msg);
            }
            s.delivered_at.get_or_insert(0);
            s.reply = Some(after.text);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(msg, "taking a late reply: {e:#}"),
    }
    s
}

/// The route of a reply to `m` from a session whose provenance is `now`.
pub fn answer_route(m: &AgentMessage, now: Option<Prov>) -> Route {
    let (origin, hop) = now.map_or((Origin::Human, 0), |p| (p.origin, p.hop));
    agents::outgoing(Some(Prov { origin: m.origin.max(origin), hop: m.hop.max(hop), msg_id: Some(m.id) }))
}

/// Did message `m` reach its agent? Never for a refused one.
fn received(hub: &Hub, m: &AgentMessage) -> bool {
    m.refused.is_none()
        && (m.reply.is_some()
            || hub.store().agent_message_delivered_at(m.id).ok().flatten().is_some()
            || hub.agents().msg_status(hub.store(), m.id).delivered_at.is_some())
}

/// Who asked message `m`, as far as a reply can reach them.
enum Asker {
    Agent(Agent),
    Session(SessionInfo),
    /// Shown in the log only (`human`: the human asked, from a client or a shell tab).
    Log { human: bool },
    Gone(String),
}

fn asker(hub: &Hub, m: &AgentMessage) -> Asker {
    let Some(s) = m.from_session else {
        return Asker::Log { human: m.from == Origin::Human.as_str() };
    };
    // An agent's envelope names it; any other session is `session:<id>`.
    if m.from != format!("session:{s}") {
        return match hub.agents().get(&m.from) {
            Some(a) => Asker::Agent(a),
            None => Asker::Gone(format!("the agent {} that asked no longer exists", m.from)),
        };
    }
    match hub.session(s) {
        Some(i) if i.kind == SessionKind::Shell => Asker::Log { human: true },
        Some(i) if i.state != AgentState::Exited && hub.live_session_ids().contains(&s) => Asker::Session(i),
        _ => Asker::Gone(format!("session {s}, which asked, has ended")),
    }
}

/// The replying agent's name for envelopes (its record may be gone by a resume).
fn replier_name(hub: &Hub, m: &AgentMessage) -> String {
    hub.agents()
        .by_id(m.to_agent)
        .map(|a| a.name)
        .or_else(|| m.to_name.clone())
        .unwrap_or_else(|| format!("agent:{}", m.to_agent))
}

/// The text a reply is delivered with: which message it answers, then the answer.
pub fn delivered_text(m: &AgentMessage, a: &Answer) -> String {
    let quote: String = m
        .text
        .as_deref()
        .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|l| {
            let mut q: String = l.chars().take(QUOTE_MAX).collect();
            if l.chars().count() > QUOTE_MAX {
                q.push('…');
            }
            q
        })
        .unwrap_or_default();
    let quote = if quote.is_empty() { String::new() } else { format!(" (\"{quote}\")") };
    format!("Late reply to your message {}{quote}:\n{}", m.id, a.text)
}

/// The header before a reply typed into a session that isn't a named agent (it has no message
/// row of its own, so no `[muster msg <id> …]` the agents' attribution would act on).
pub fn reply_envelope(msg_id: i64, from: &str, origin: Origin, hop: u32) -> String {
    let from: String = from
        .chars()
        .map(|c| if c.is_whitespace() || c == ']' || c == '[' || c.is_control() { '_' } else { c })
        .collect();
    format!("[muster reply to msg {msg_id} from {from} origin={} hop={hop}]", origin.as_str())
}

/// MCP `reply`: the agent in session `caller` answers message `msg_id` (text already cleaned and
/// checked by the tool). Errors are refusals before anything was stored; once it is stored the
/// result says how it reached the asker, refused deliveries included.
pub async fn reply(hub: &Arc<Hub>, caller: SessionId, msg_id: i64, text: String) -> anyhow::Result<Value> {
    let me = hub.agents().by_session(caller).ok_or_else(|| {
        anyhow::anyhow!("reply answers a message Muster delivered to a named agent, and this session isn't one")
    })?;
    let m = hub
        .store()
        .agent_message(msg_id)?
        .filter(|m| m.to_agent == me.id && m.refused.is_none())
        .ok_or_else(|| anyhow::anyhow!(
            "no message {msg_id} was delivered to you (its id is on its [muster msg <id> …] line)"
        ))?;
    if m.answer.is_some() {
        anyhow::bail!("you already replied to message {msg_id}; a message takes one reply");
    }
    if !received(hub, &m) {
        anyhow::bail!("message {msg_id} hasn't reached you yet (it is still waiting in your input queue)");
    }
    if text.trim().is_empty() {
        anyhow::bail!("text must not be empty");
    }
    let route = answer_route(&m, hub.agents().prov(caller));
    let answer = Answer {
        text,
        at: now_ms(),
        from_session: caller,
        origin: route.origin,
        hop: route.hop,
        status: AnswerStatus::Pending,
        note: None,
        msg: None,
        session: None,
    };
    if !hub.store().set_agent_answer(msg_id, &answer)? {
        anyhow::bail!("you already replied to message {msg_id}; a message takes one reply");
    }
    agents::publish_message(hub, msg_id);
    // In its own task, so a caller that goes away doesn't leave it half delivered.
    let h = hub.clone();
    let done = tokio::spawn(async move {
        // An ask waiting for this message (or about to) takes it at its next check.
        let start = tokio::time::Instant::now();
        while start.elapsed() < WAITER_MAX
            && h.replies.may_wait(&m)
            && current(&h, msg_id).is_some_and(|a| a.status == AnswerStatus::Pending)
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        finish(&h, msg_id).await
    })
    .await??;
    Ok(result_json(msg_id, &done))
}

fn current(hub: &Hub, msg: i64) -> Option<Answer> {
    hub.store().agent_message(msg).ok().flatten().and_then(|m| m.answer)
}

/// What the replier is told.
fn result_json(msg_id: i64, a: &Answer) -> Value {
    json!({
        "msg_id": msg_id,
        "status": a.status,
        "delivered": matches!(a.status, AnswerStatus::Returned | AnswerStatus::Delivered | AnswerStatus::Shown),
        "note": a.note,
        "delivered_as": a.msg,
        "session_id": a.session,
        "origin": a.origin,
        "hop": a.hop,
    })
}

/// Deliver message `msg_id`'s pending late reply to its asker, unless something took it already.
/// Returns the reply as it stands afterwards.
pub async fn finish(hub: &Arc<Hub>, msg_id: i64) -> anyhow::Result<Answer> {
    let claimed = hub.store().update_agent_answer(msg_id, &[AnswerStatus::Pending], |a| a.status = AnswerStatus::Delivering)?;
    let Some((_, a)) = claimed else {
        return current(hub, msg_id).ok_or_else(|| anyhow::anyhow!("message {msg_id} is gone"));
    };
    let m = hub.store().agent_message(msg_id)?.ok_or_else(|| anyhow::anyhow!("message {msg_id} is gone"))?;
    let done = deliver(hub, &m, a).await;
    hub.store().update_agent_answer(msg_id, &[AnswerStatus::Delivering], |x| *x = done.clone())?;
    agents::publish_message(hub, msg_id);
    Ok(done)
}

/// Deliver `a` (a reply to `m`) to whoever asked `m`: the reply with its final status.
async fn deliver(hub: &Arc<Hub>, m: &AgentMessage, mut a: Answer) -> Answer {
    let from = replier_name(hub, m);
    let set = |a: &mut Answer, status: AnswerStatus, note: String| {
        a.status = status;
        a.note = Some(note);
    };
    match asker(hub, m) {
        Asker::Log { human } => {
            let who = if human { "the human".to_string() } else { m.from.clone() };
            set(&mut a, AnswerStatus::Shown, format!("{who} asked: shown in the agents panel's message log"));
            if human {
                let first = a.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                crate::notify::alert(&format!("{from} replied to message {}", m.id), first);
            }
        }
        Asker::Gone(why) => set(&mut a, AnswerStatus::Undelivered, format!("not delivered: {why}")),
        Asker::Agent(to) if to.id == m.to_agent => {
            set(&mut a, AnswerStatus::Undelivered, "not delivered: an agent can't message itself".into());
        }
        Asker::Agent(to) => {
            let text = delivered_text(m, &a);
            // send_checked routes one hop past this provenance: the reply's own route.
            let prov = Prov { origin: a.origin, hop: a.hop.saturating_sub(1), msg_id: Some(m.id) };
            let sender = hub.agents().by_id(m.to_agent).and_then(|r| r.session_id).unwrap_or(a.from_session);
            match agents::send_checked(hub, &to, text.clone(), from.clone(), Some(sender), Some(prov), None).await {
                Ok((d, _)) => {
                    a.msg = Some(d.msg_id);
                    a.session = Some(d.session_id);
                    set(&mut a, AnswerStatus::Delivered, format!("delivered to {} as message {}", to.name, d.msg_id));
                }
                Err(e) if e.downcast_ref::<agents::Refused>().is_some() => {
                    set(&mut a, AnswerStatus::Refused, format!("not delivered to {}: {e:#}", to.name));
                }
                Err(e) if e.downcast_ref::<crate::inject::DeliveryPending>().is_some() => {
                    a.msg = hub.store().find_answer_delivery(to.id, m.id, &from, a.at, &text).ok().flatten();
                    set(&mut a, AnswerStatus::Delivered, format!("queued for {}; Muster is confirming its delivery", to.name));
                }
                Err(e) => set(&mut a, AnswerStatus::Undelivered, format!("not delivered to {}: {e:#}", to.name)),
            }
        }
        Asker::Session(s) => {
            if let Err(why) = crate::mcp::guard::check_claude_origin(s.kind, a.origin) {
                set(&mut a, AnswerStatus::Refused, format!("not delivered to session {}: {why}", s.id));
                return a;
            }
            let text = format!("{}\n{}", reply_envelope(m.id, &from, a.origin, a.hop), delivered_text(m, &a));
            let source = Source::Chain(Prov { origin: a.origin, hop: a.hop, msg_id: None });
            let log_from = format!("session:{}", a.from_session);
            match crate::inject::send(hub, s.id, text, crate::inject::SendMode::Queue, log_from, source).await {
                Ok(_) => {
                    a.session = Some(s.id);
                    set(&mut a, AnswerStatus::Delivered, format!("typed into session {} ({})", s.id, s.name));
                }
                Err(e) if e.downcast_ref::<crate::inject::DeliveryPending>().is_some() => {
                    a.session = Some(s.id);
                    set(&mut a, AnswerStatus::Delivered, format!("queued for session {}; Muster is confirming its delivery", s.id));
                }
                Err(e) => set(&mut a, AnswerStatus::Undelivered, format!("not delivered to session {}: {e:#}", s.id)),
            }
        }
    }
    a
}

/// At daemon start: deliver late replies a previous backend stored but didn't finish.
pub async fn resume(hub: &Arc<Hub>) {
    let ids = match hub.store().unfinished_agent_answers() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("reading unfinished late replies: {e:#}");
            return;
        }
    };
    for id in ids {
        if let Err(e) = resume_one(hub, id).await {
            tracing::warn!(msg = id, "finishing a late reply: {e:#}");
        }
    }
}

async fn resume_one(hub: &Arc<Hub>, id: i64) -> anyhow::Result<()> {
    let Some(m) = hub.store().agent_message(id)? else { return Ok(()) };
    let Some(a) = m.answer.clone() else { return Ok(()) };
    if a.status == AnswerStatus::Delivering {
        // The restart came while it was being delivered: what got through?
        let outcome = match asker(hub, &m) {
            Asker::Agent(to) => {
                let found = hub.store().find_answer_delivery(to.id, m.id, &replier_name(hub, &m), a.at, &delivered_text(&m, &a))?;
                found.map(|n| (AnswerStatus::Delivered, Some(n), format!("delivered to {} as message {n}", to.name)))
            }
            // Its queue may have it: typing it again could show it twice.
            Asker::Session(s) => Some((AnswerStatus::Undelivered, None, format!(
                "a backend restart interrupted typing it into session {}; it may not have arrived", s.id
            ))),
            _ => None,
        };
        let changed = hub.store().update_agent_answer(id, &[AnswerStatus::Delivering], |x| match outcome {
            Some((status, msg, note)) => {
                x.status = status;
                x.msg = msg;
                x.note = Some(note);
            }
            None => x.status = AnswerStatus::Pending,
        })?;
        if changed.is_some() {
            agents::publish_message(hub, id);
        }
    }
    finish(hub, id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Arc<Hub> {
        let store = crate::store::Store::open_memory().unwrap();
        let ws = store.insert_workspace("repo", "/w/repo", false, 0).unwrap();
        let mut ch = crate::mcp::guard::test_channel(0);
        ch.workspace_id = ws;
        store.insert_channel(&ch).unwrap();
        for kind in [SessionKind::Shell, SessionKind::Claude, SessionKind::Codex, SessionKind::Codex] {
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

    /// A named agent `name` of `kind` linked to session `sid`.
    fn agent(hub: &Hub, name: &str, kind: SessionKind, sid: SessionId) -> Agent {
        let a = agents::create(hub, agents::NewAgent {
            account_profile: None,
            provider: None,
            name: name.into(),
            kind: Some(kind),
            model: None,
            effort: None,
            persona: None,
            accepts: None,
            permissions: None,
            autostart: false,
            sleep_after_min: None,
        })
        .unwrap();
        hub.agents().link_session(hub.store(), a.id, Some(sid)).unwrap();
        hub.agents().by_id(a.id).unwrap()
    }

    /// A message to `to` from `from` (label, session), delivered unless `queued`.
    fn msg(hub: &Hub, to: &Agent, from: &str, from_session: Option<SessionId>, origin: Origin, hop: u32, queued: bool) -> i64 {
        let route = Route { origin, hop, reply_to: None };
        let m = agents::message_record(to, from, from_session, route, "run the benchmarks\nand report");
        let id = hub.store().insert_agent_message(&m).unwrap();
        if !queued {
            hub.store().set_agent_message_delivered(id, now_ms() - 60_000).unwrap();
        }
        id
    }

    fn answer_of(hub: &Hub, id: i64) -> Answer {
        hub.store().agent_message(id).unwrap().unwrap().answer.unwrap()
    }

    #[tokio::test]
    async fn only_the_receiving_agent_replies_once_to_what_it_received() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let other = agent(&hub, "other", SessionKind::Codex, 4);
        let m = msg(&hub, &scout, "human", None, Origin::Human, 0, false);
        // Not an agent, another agent, an unknown id, a queued message.
        let e = reply(&hub, 2, m, "x".into()).await.unwrap_err().to_string();
        assert!(e.contains("isn't one"), "{e}");
        let e = reply(&hub, 4, m, "x".into()).await.unwrap_err().to_string();
        assert!(e.contains("no message"), "{e}");
        let e = reply(&hub, 3, 999, "x".into()).await.unwrap_err().to_string();
        assert!(e.contains("no message"), "{e}");
        let queued = msg(&hub, &scout, "human", None, Origin::Human, 0, true);
        let e = reply(&hub, 3, queued, "x".into()).await.unwrap_err().to_string();
        assert!(e.contains("hasn't reached you"), "{e}");
        // A refused message was never delivered.
        let mut r = agents::message_record(&scout, "human", None, Route { origin: Origin::Human, hop: 0, reply_to: None }, "no");
        r.to_session = Some(3);
        let refused = hub.store().insert_refused_agent_message(&r, "cap").unwrap();
        assert!(reply(&hub, 3, refused, "x".into()).await.unwrap_err().to_string().contains("no message"));
        // The human asked: the answer is kept and shown in the log.
        let v = reply(&hub, 3, m, "all green".into()).await.unwrap();
        assert_eq!(v["status"], "shown", "{v}");
        let a = answer_of(&hub, m);
        assert_eq!((a.text.as_str(), a.status, a.origin, a.hop), ("all green", AnswerStatus::Shown, Origin::Human, 1));
        let e = reply(&hub, 3, m, "again".into()).await.unwrap_err().to_string();
        assert!(e.contains("already replied"), "{e}");
        assert_eq!(answer_of(&hub, m).text, "all green");
        let _ = other;
    }

    #[test]
    fn the_route_is_the_stricter_chain_one_hop_on() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let id = msg(&hub, &scout, "ada", Some(2), Origin::Human, 2, false);
        let m = hub.store().agent_message(id).unwrap().unwrap();
        assert_eq!(answer_route(&m, None), Route { origin: Origin::Human, hop: 3, reply_to: Some(id) });
        // Replying while it handles a scheduled message: the reply is unattended work too.
        let sched = Prov { origin: Origin::Schedule, hop: 0, msg_id: Some(1) };
        assert_eq!(answer_route(&m, Some(sched)), Route { origin: Origin::Schedule, hop: 3, reply_to: Some(id) });
        let deep = Prov { origin: Origin::Human, hop: 4, msg_id: None };
        assert_eq!(answer_route(&m, Some(deep)).hop, 5);
    }

    #[tokio::test]
    async fn a_reply_to_an_agent_follows_its_policy_and_hop_limit() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let ada = agent(&hub, "ada", SessionKind::Claude, 2);
        // Ada (Claude: human only) asked; the reply comes from a scheduled chain: refused, kept.
        let m = msg(&hub, &scout, "ada", Some(2), Origin::Human, 1, false);
        hub.agents().set_prov(3, Some(Prov { origin: Origin::Schedule, hop: 0, msg_id: None })).unwrap();
        let v = reply(&hub, 3, m, "done".into()).await.unwrap();
        assert_eq!(v["status"], "refused", "{v}");
        assert!(v["note"].as_str().unwrap().contains("accepts only human"), "{v}");
        let a = answer_of(&hub, m);
        assert_eq!((a.status, a.origin, a.text.as_str()), (AnswerStatus::Refused, Origin::Schedule, "done"));
        // The refusal is in Ada's log, as any refused message, with the reply's text.
        let log = hub.store().agent_messages(Some(ada.id), None, 10).unwrap();
        assert!(log.iter().any(|x| x.refused.is_some() && x.reply_to == Some(m)
            && x.text.as_deref().is_some_and(|t| t.starts_with("Late reply to your message"))));
        // At the hop limit a reply chain ends.
        hub.agents().set_prov(3, None).unwrap();
        let deep = msg(&hub, &scout, "ada", Some(2), Origin::Human, 4, false);
        let v = reply(&hub, 3, deep, "deep".into()).await.unwrap();
        assert_eq!(v["status"], "refused", "{v}");
        assert!(v["note"].as_str().unwrap().contains("hop"), "{v}");
    }

    #[tokio::test]
    async fn a_waiting_ask_takes_the_reply_and_nothing_else_does() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let ada = agent(&hub, "ada", SessionKind::Claude, 2);
        let m = msg(&hub, &scout, "ada", Some(2), Origin::Human, 1, false);
        let waiting = hub.replies.wait(m);
        let h = hub.clone();
        let replied = tokio::spawn(async move { reply(&h, 3, m, "here it is".into()).await });
        // The ask's next check takes it.
        let mut got = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(r) = status(&hub, m).reply { got = Some(r); break; }
        }
        assert_eq!(got.as_deref(), Some("here it is"));
        drop(waiting);
        let v = replied.await.unwrap().unwrap();
        assert_eq!(v["status"], "returned", "{v}");
        // Nothing went to Ada as a new message.
        assert!(hub.store().agent_messages(Some(ada.id), None, 10).unwrap().is_empty());
        // A finished reply is never taken or delivered again.
        assert_eq!(finish(&hub, m).await.unwrap().status, AnswerStatus::Returned);
    }

    #[tokio::test]
    async fn a_reply_waits_for_the_ask_still_delivering_its_message() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        agent(&hub, "ada", SessionKind::Claude, 2);
        // Ada's ask is still delivering (typing it in, confirming it) when Scout already replies.
        let asking = hub.replies.asking(scout.id, Some(2));
        let m = msg(&hub, &scout, "ada", Some(2), Origin::Human, 1, false);
        let h = hub.clone();
        let replied = tokio::spawn(async move { reply(&h, 3, m, "fast".into()).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Its delivery returned: now it waits for the message, and takes the reply.
        let _waiting = hub.replies.wait(m);
        drop(asking);
        assert_eq!(status(&hub, m).reply.as_deref(), Some("fast"));
        assert_eq!(replied.await.unwrap().unwrap()["status"], "returned");
    }

    #[tokio::test]
    async fn a_reply_stored_before_a_restart_is_finished_at_the_next_start() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let m = msg(&hub, &scout, "human", None, Origin::Human, 0, false);
        let pending = Answer {
            text: "kept".into(), at: now_ms(), from_session: 3, origin: Origin::Human, hop: 1,
            status: AnswerStatus::Pending, note: None, msg: None, session: None,
        };
        assert!(hub.store().set_agent_answer(m, &pending).unwrap());
        assert!(!hub.store().set_agent_answer(m, &pending).unwrap(), "once");
        assert_eq!(hub.store().unfinished_agent_answers().unwrap(), vec![m]);
        resume(&hub).await;
        assert_eq!(answer_of(&hub, m).status, AnswerStatus::Shown);
        assert!(hub.store().unfinished_agent_answers().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_interrupted_delivery_to_an_agent_is_found_not_repeated() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let ada = agent(&hub, "ada", SessionKind::Codex, 2);
        let m = msg(&hub, &scout, "ada", Some(2), Origin::Human, 1, false);
        let a = Answer {
            text: "found".into(), at: now_ms(), from_session: 3, origin: Origin::Human, hop: 2,
            status: AnswerStatus::Delivering, note: None, msg: None, session: None,
        };
        hub.store().set_agent_answer(m, &a).unwrap();
        // The delivery's row got in before the restart.
        let orig = hub.store().agent_message(m).unwrap().unwrap();
        let mut d = agents::message_record(&ada, "scout", Some(3), Route { origin: Origin::Human, hop: 2, reply_to: Some(m) },
            &delivered_text(&orig, &a));
        d.created_at = a.at;
        let carried = hub.store().insert_agent_message(&d).unwrap();
        resume(&hub).await;
        let a = answer_of(&hub, m);
        assert_eq!((a.status, a.msg), (AnswerStatus::Delivered, Some(carried)));
        assert_eq!(hub.store().agent_messages(Some(ada.id), None, 10).unwrap().len(), 1);
    }

    #[test]
    fn delivered_text_names_the_message_and_quotes_its_first_line() {
        let hub = fixture();
        let scout = agent(&hub, "scout", SessionKind::Codex, 3);
        let id = msg(&hub, &scout, "ada", Some(2), Origin::Human, 1, false);
        let m = hub.store().agent_message(id).unwrap().unwrap();
        let a = Answer {
            text: "42".into(), at: 0, from_session: 3, origin: Origin::Human, hop: 2,
            status: AnswerStatus::Pending, note: None, msg: None, session: None,
        };
        assert_eq!(delivered_text(&m, &a), format!("Late reply to your message {id} (\"run the benchmarks\"):\n42"));
        // The header for a plain session is no agent envelope.
        let e = reply_envelope(id, "sc out]", Origin::Human, 2);
        assert_eq!(e, format!("[muster reply to msg {id} from sc_out_ origin=human hop=2]"));
        assert_eq!(agents::parse_envelope(&e), None);
    }
}
