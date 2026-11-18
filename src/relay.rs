//! Delivering Muster's word to an agent that posted something for the human: the answer to a
//! question (src/questions.rs) or the result of a pull request card (src/prs.rs).
//!
//! - Who: the session that posted, as it was then (`Who`), and the same poster later (`same`).
//! - Where: the posting agent by name (its current session, whatever happened to the one that
//!   posted: a restart, a fresh start, sleep), else the session that posted, else the lead that
//!   started that session (`target`).
//! - The rule `ask_agent` and `send_message` apply, when someone other than the human recorded
//!   the words (`check_reach`).
//! - One delivery (`deliver`), and, after a restart that came while one was under way, whether it
//!   got through (`found_after_restart`), so it is neither lost nor typed in twice.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agents::{self, Agent, Origin, Prov, Source};
use crate::hub::Hub;
use crate::model::*;

/// A session acting on a question or a card (the poster, a lead passing it on, a recorder), as it
/// was then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Who {
    /// For people: the named agent's name, else "Claude (task name)".
    pub label: String,
    pub session_id: SessionId,
    pub kind: SessionKind,
    /// The named agent, when it is one: answers go to it by name. `agent_created_at` tells it
    /// apart from a newer agent that took the name of a removed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_created_at: Option<i64>,
    /// The session that started it (its lead), and that session's named agent if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_created_at: Option<i64>,
}

/// How a delivery stands. `Pending` and `Delivering` are unfinished (a restart resumes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Delivering,
    /// In the recipient's input queue, or typed in.
    Delivered,
    /// The recorder is the one it would go to: it has the words already.
    NotNeeded,
    /// A guard refused it (`note` says which). What it carried is still kept.
    Refused,
    /// Nobody to deliver it to, or an error (`note`). What it carried is still kept.
    Undelivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Where it went: an agent's name or `session <id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The agent message that carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

impl Delivery {
    /// Not sent yet, with why it waits.
    pub fn pending(note: Option<String>) -> Delivery {
        Delivery { status: DeliveryStatus::Pending, note, to: None, msg_id: None, session_id: None, at: None }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn unfinished(&self) -> bool {
        matches!(self.status, DeliveryStatus::Pending | DeliveryStatus::Delivering)
    }
}

/// Session `id` as a question or card records it (None: no such session).
pub fn who(hub: &Hub, id: SessionId) -> Option<Who> {
    let s = hub.session(id)?;
    let agent = hub.agents().by_session(id);
    let label = crate::notes::author(hub, Some(id)).ok().and_then(|a| a.name).unwrap_or_else(|| format!("session {id}"));
    let parent_agent = s.parent_id.and_then(|p| hub.agents().by_session(p));
    Some(Who {
        label,
        session_id: id,
        kind: s.kind,
        agent_id: agent.as_ref().map(|a| a.id),
        agent_created_at: agent.as_ref().map(|a| a.created_at),
        parent_id: s.parent_id,
        parent_agent_id: parent_agent.as_ref().map(|a| a.id),
        parent_agent_created_at: parent_agent.as_ref().map(|a| a.created_at),
    })
}

/// The same poster: the same named agent (whatever its session now), else the same session.
pub fn same(a: &Who, b: &Who) -> bool {
    match (a.agent_id, b.agent_id) {
        (Some(x), Some(y)) => x == y && a.agent_created_at == b.agent_created_at,
        (None, None) => a.session_id == b.session_id,
        _ => false,
    }
}

/// Named agent `id` if it is still the one recorded (not a newer agent with its id or name).
pub fn agent_then(hub: &Hub, id: i64, created_at: Option<i64>) -> Option<Agent> {
    hub.agents().by_id(id).filter(|a| Some(a.created_at) == created_at)
}

/// Who a delivery goes to.
#[derive(Debug)]
pub enum Target {
    /// A named agent, by name. `note`: why it isn't the poster (the lead of a session that ended).
    Agent(Agent, Option<String>),
    /// A session that isn't a named agent (`note` as above).
    Session(SessionInfo, Option<String>),
    /// The recorder is where it would go.
    NotNeeded(String),
    Gone(String),
}

/// Where words for `poster` go when `recorder` recorded them (None: the human or Muster), with
/// `live` the running sessions: the posting agent by name; else the posting session; else the lead
/// that started it.
pub fn target(hub: &Hub, poster: &Who, recorder: Option<&Who>, live: &HashSet<SessionId>) -> Target {
    if recorder.is_some_and(|r| same(poster, r)) {
        return Target::NotNeeded(format!("not sent: {} asked it and recorded the answer itself", poster.label));
    }
    let is_recorder = |agent: Option<i64>, session: SessionId| recorder.is_some_and(|r|
        r.session_id == session || (agent.is_some() && r.agent_id == agent));
    if let Some(id) = poster.agent_id {
        return match agent_then(hub, id, poster.agent_created_at) {
            Some(a) => Target::Agent(a, None),
            None if hub.agents().by_id(id).is_some() || hub.agents().get(&poster.label).is_some() =>
                Target::Gone(format!("the agent {} that asked was removed (a newer agent has its name)", poster.label)),
            None => Target::Gone(format!("the agent {} that asked no longer exists", poster.label)),
        };
    }
    let running = |id: SessionId| hub.session(id).filter(|s| s.state != AgentState::Exited && live.contains(&id));
    if let Some(s) = running(poster.session_id) {
        return Target::Session(s, None);
    }
    let ended = format!("session {} ({}), which asked, has ended", poster.session_id, poster.label);
    let Some(p) = poster.parent_id else { return Target::Gone(ended) };
    if is_recorder(poster.parent_agent_id, p) {
        return Target::NotNeeded(format!("not sent: {ended}, and you started it, so you have the answer"));
    }
    let note = Some(format!("{ended}; you started it, so its answer comes to you"));
    if let Some(a) = poster.parent_agent_id.and_then(|id| agent_then(hub, id, poster.parent_agent_created_at)) {
        return Target::Agent(a, note);
    }
    match running(p) {
        Some(s) => Target::Session(s, note),
        None => Target::Gone(format!("{ended}, and so has session {p}, which started it")),
    }
}

/// The rule `ask_agent` and `send_message` apply: the recorder can't reach, through an answer,
/// an agent or session that runs with more than the recorder's own channel allows (its words
/// would steer an agent with wider permissions).
pub fn check_reach(hub: &Hub, recorder: SessionId, t: &Target) -> Result<(), String> {
    let mine = hub.session(recorder).and_then(|s| hub.channel(s.channel_id)).ok_or("the recorder's channel no longer exists")?;
    let (ch, perms, kind, what) = match t {
        Target::Agent(a, _) => {
            let session_ch = a.session_id.and_then(|s| hub.session(s)).and_then(|s| hub.channel(s.channel_id));
            let own = a.channel_id.and_then(|c| hub.channel(c));
            let (ch, p) = agents::reach_profile(a, session_ch.as_ref(), own.as_ref());
            (ch, p, a.kind, format!("agent {}'s tab", a.name))
        }
        Target::Session(s, _) => {
            // What `send_message` refuses, an answer may not do either.
            let c = hub.channel(s.channel_id).ok_or_else(|| format!("session {}'s channel no longer exists", s.id))?;
            if s.kind == SessionKind::Shell {
                return Err(format!("session {} is a shell: an answer typed there would run as commands", s.id));
            }
            if s.state == AgentState::Exited || c.archived {
                return Err(format!("session {} has ended", s.id));
            }
            let shared = c.workspace_id == mine.workspace_id
                || hub.mcp().is_some_and(|st| crate::mcp::communication::allowed(&st, recorder, &c, false));
            if !shared {
                return Err(format!(
                    "session {} is in another workspace and messaging it isn't shared with yours, so an answer can't reach it. \
                     The human can add a sharing rule in the Agents panel under Session communication", s.id));
            }
            (Some(c.id), c.permissions, s.kind, format!("session {}'s channel", s.id))
        }
        Target::NotNeeded(_) | Target::Gone(_) => return Ok(()),
    };
    if ch != Some(mine.id) && perms.exceeds_for(&mine.permissions, kind) {
        return Err(format!(
            "{what} lets {} do more than your channel does, so an answer can't carry your words into it. \
             Ask the human to answer it there (or the asker's own lead to record it)", kind.as_str()));
    }
    Ok(())
}

pub fn outcome(status: DeliveryStatus, note: String) -> Delivery {
    Delivery { status, note: Some(note), to: None, msg_id: None, session_id: None, at: Some(now_ms()) }
}

/// A delivery the backend's shutdown cut off before it reached a queue: pending again, so the
/// next backend sends it.
pub fn interrupted() -> Delivery {
    Delivery::pending(Some("a backend restart interrupted its delivery; it is sent when Muster is back".into()))
}

/// One message for a poster.
pub struct Letter<'a> {
    pub poster: &'a Who,
    /// Who recorded the words (None: the human's own click, or Muster).
    pub recorder: Option<&'a Who>,
    /// The sender label its message carries: the recorder's agent name, `session:<id>`, `human`
    /// or `muster`. Fixed when the words were stored, so a restart looks for the message under
    /// the label it was sent with.
    pub from: String,
    pub from_session: Option<SessionId>,
    /// The recorder's provenance (None: the human's own, origin human, hop 0).
    pub prov: Option<Prov>,
    /// The words, given why they go to someone other than the poster (None: they go to it).
    pub text: &'a (dyn Fn(Option<&str>) -> String + Sync),
    /// The first line before the words typed into a session that isn't a named agent (no agent
    /// message row, so no `[muster msg …]` envelope), given the sender, origin and hop.
    pub envelope: &'a (dyn Fn(&str, Origin, u32) -> String + Sync),
    /// How that first line starts, whatever the sender (to find it in a queue).
    pub prefix: String,
}

/// Deliver `l`: the delivery with its final status.
pub async fn deliver(hub: &Arc<Hub>, l: &Letter<'_>) -> Delivery {
    let live: HashSet<SessionId> = hub.live_session_ids().into_iter().collect();
    let route = agents::outgoing(l.prov);
    let t = target(hub, l.poster, l.recorder, &live);
    // Checked again now (the poster, its lead or their tabs may have changed since): with the
    // recorder's current session (a named agent's, whatever happened to the one it recorded
    // from). A recorder Muster can no longer find refuses: it can't be checked.
    let recorder = l.recorder.map(|r| r.agent_id
        .and_then(|id| agent_then(hub, id, r.agent_created_at)).and_then(|ag| ag.session_id).unwrap_or(r.session_id));
    if let Some(r) = recorder
        && let Err(why) = check_reach(hub, r, &t)
    {
        let mut d = outcome(DeliveryStatus::Refused, format!("not delivered: {why}"));
        if let Target::Agent(to, _) = &t { d.to = Some(to.name.clone()); }
        return d;
    }
    match t {
        Target::NotNeeded(why) => outcome(DeliveryStatus::NotNeeded, why),
        Target::Gone(why) => outcome(DeliveryStatus::Undelivered, format!("not delivered: {why}")),
        Target::Agent(to, note) => {
            let text = (l.text)(note.as_deref());
            let mut d = outcome(DeliveryStatus::Delivered, String::new());
            d.to = Some(to.name.clone());
            // Delivered by name, as late replies are: an agent removed and another made with its
            // name in the moment between `target` and the start would get it (only the human
            // removes and makes agents).
            match agents::send_checked(hub, &to, text.clone(), l.from.clone(), l.from_session, l.prov, None).await {
                Ok((sent, _)) => {
                    d.msg_id = Some(sent.msg_id);
                    d.session_id = Some(sent.session_id);
                    d.note = Some(if sent.delivered {
                        format!("typed into {} as message {}", to.name, sent.msg_id)
                    } else {
                        format!("queued for {} as message {}: typed in when it is between steps", to.name, sent.msg_id)
                    });
                }
                Err(e) if e.downcast_ref::<agents::Refused>().is_some() => {
                    d.status = DeliveryStatus::Refused;
                    d.note = Some(format!("not delivered to {}: {e:#}", to.name));
                }
                Err(e) if e.downcast_ref::<crate::inject::DeliveryPending>().is_some() => {
                    d.msg_id = hub.store().find_question_delivery(to.id, &l.from, &text).ok().flatten();
                    d.note = Some(format!("queued for {}; Muster is confirming its delivery", to.name));
                }
                Err(_) if hub.is_shutting_down() => return interrupted(),
                Err(e) => {
                    d.status = DeliveryStatus::Undelivered;
                    d.note = Some(format!("not delivered to {}: {e:#}", to.name));
                }
            }
            d
        }
        Target::Session(s, note) => {
            // Never typed into a shell, whoever recorded it (`check_reach` says so too).
            if s.kind == SessionKind::Shell {
                return outcome(DeliveryStatus::Refused, format!("not delivered: session {} is a shell", s.id));
            }
            if let Err(why) = crate::mcp::guard::check_claude_origin(s.kind, route.origin) {
                return outcome(DeliveryStatus::Refused, format!("not delivered to session {}: {why}", s.id));
            }
            let text = format!("{}\n{}", (l.envelope)(&l.from, route.origin, route.hop), (l.text)(note.as_deref()));
            let source = Source::Chain(Prov { origin: route.origin, hop: route.hop, msg_id: None });
            // The human's own words (a tab) are logged as theirs.
            let log_from = l.from_session.map_or_else(|| l.from.clone(), |s| format!("session:{s}"));
            let mut d = outcome(DeliveryStatus::Delivered, String::new());
            d.to = Some(format!("session {}", s.id));
            d.session_id = Some(s.id);
            match crate::inject::send(hub, s.id, text, crate::inject::SendMode::Queue, log_from, source).await {
                Ok(o) => d.note = Some(if o.delivered {
                    format!("typed into session {} ({})", s.id, s.name)
                } else {
                    format!("queued for session {} ({}): typed in when it is between steps", s.id, s.name)
                }),
                Err(e) if e.downcast_ref::<crate::inject::DeliveryPending>().is_some() => {
                    d.note = Some(format!("queued for session {}; Muster is confirming its delivery", s.id));
                }
                Err(_) if hub.is_shutting_down() => return interrupted(),
                Err(e) => {
                    d.status = DeliveryStatus::Undelivered;
                    d.note = Some(format!("not delivered to session {}: {e:#}", s.id));
                }
            }
            d
        }
    }
}

/// A restart came while `l` was being delivered: what got through? Some(delivery) when it reached
/// the recipient (or was refused there); None when it must be sent again. A message row recorded
/// but never queued is dropped first, as a failed delivery's is.
pub fn found_after_restart(hub: &Arc<Hub>, l: &Letter<'_>) -> anyhow::Result<Option<Delivery>> {
    let live: HashSet<SessionId> = hub.live_session_ids().into_iter().collect();
    Ok(match target(hub, l.poster, l.recorder, &live) {
        Target::Agent(to, note) => {
            match hub.store().find_question_delivery(to.id, &l.from, &(l.text)(note.as_deref()))? {
                None => None,
                Some(n) => match hub.store().agent_message(n)? {
                    Some(c) if c.refused.is_some() => Some(Delivery { msg_id: Some(n), to: Some(to.name.clone()),
                        ..outcome(DeliveryStatus::Refused, format!("not delivered to {}: {}", to.name, c.refused.unwrap_or_default())) }),
                    Some(c) if c.reply.is_some() || hub.store().agent_message_delivered_at(n)?.is_some()
                        || crate::inject::holds_message(hub, n) =>
                    {
                        Some(Delivery { msg_id: Some(n), to: Some(to.name.clone()),
                            ..outcome(DeliveryStatus::Delivered, format!("delivered to {} as message {n}", to.name)) })
                    }
                    // Recorded, but the restart came before it was queued: nothing will type it
                    // in. Drop that row (as a failed delivery does) and send it again.
                    _ => {
                        hub.store().delete_agent_message(n)?;
                        None
                    }
                },
            }
        }
        // Still in its queue: it arrives. Otherwise it is sent again (if the restart came just
        // after it was typed in, the session sees it twice, rather than never).
        Target::Session(s, _) if crate::inject::holds_prompt(hub, s.id, &l.prefix) =>
            Some(Delivery { to: Some(format!("session {}", s.id)), session_id: Some(s.id),
                ..outcome(DeliveryStatus::Delivered, format!("queued for session {} before a backend restart", s.id)) }),
        _ => None,
    })
}
