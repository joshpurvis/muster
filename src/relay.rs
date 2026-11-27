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
//! - The cold-answer rule (`FreshRule`, `fresh_verdict`), for answers to questions only: a named
//!   agent whose conversation is cold (past its cache lifetime, src/cache.rs) and large (at least
//!   `cache::FRESH_ANSWER_MIN_TOKENS`, from its status line: src/statusline.rs) gets the answer
//!   in a new conversation, started the way Start fresh starts one (`agents::start_fresh_if`).
//!   Only while it rests (idle or asleep) with nothing else waiting for it, and not when the
//!   human typed into it in the last two minutes or had a prompt of their own taken in by it in
//!   the last 24 hours. Whatever Muster can't tell keeps the current conversation. The decision
//!   is saved with the delivery (`Delivery::fresh`) before the agent is restarted, and the
//!   message says so in its first line, so a backend restart in between neither restarts the
//!   agent twice nor types the answer in twice.

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
    /// The new conversation Muster started the recipient in for it (`FreshRule`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh: Option<Fresh>,
}

/// A new conversation Muster starts a recipient in for one delivery. Saved with the delivery
/// before the agent is restarted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fresh {
    /// Why, in a few words: "the old one was cold and large, about 216k tokens and idle 3 h".
    pub why: String,
    /// The conversation it replaces (None: the agent had none Muster knew of).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The run (terminal generation) that held it, when it was running: a backend restart that
    /// finds that run still there knows the fresh start didn't happen yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<u64>,
}

impl Delivery {
    /// Not sent yet, with why it waits.
    pub fn pending(note: Option<String>) -> Delivery {
        Delivery { status: DeliveryStatus::Pending, note, to: None, msg_id: None, session_id: None, at: None, fresh: None }
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
    let me = hub.session(recorder).ok_or("the recorder's channel no longer exists")?;
    let mine = hub.channel(me.channel_id).ok_or("the recorder's channel no longer exists")?;
    let (ch, perms, kinds, what) = match t {
        Target::Agent(a, _) => {
            let session = a.session_id.and_then(|s| hub.session(s));
            let session_ch = session.as_ref().and_then(|s| hub.channel(s.channel_id));
            let own = a.channel_id.and_then(|c| hub.channel(c));
            let (ch, p) = agents::reach_profile(a, session_ch.as_ref(), own.as_ref());
            (ch, p, agents::reach_kinds(a, session.as_ref()), format!("agent {}'s tab", a.name))
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
            // The MCP reach rule (a session started under an agent's grant reaches what the agent
            // reaches, not the workspace it runs in); the plain workspace rule without MCP.
            let shared = match hub.mcp() {
                Some(st) => crate::mcp::communication::allowed_session(&st, recorder, s, &c, false),
                None => c.workspace_id == mine.workspace_id,
            };
            if !shared {
                return Err(format!(
                    "session {} is in another workspace and messaging it isn't shared with yours, so an answer can't reach it. \
                     The human can add a sharing rule in Muster's sidebar: Channels ⋯ → Session communication", s.id));
            }
            (Some(c.id), c.permissions, vec![s.kind], format!("session {}'s channel", s.id))
        }
        Target::NotNeeded(_) | Target::Gone(_) => return Ok(()),
    };
    // A recorder of another kind is compared by its own mode too (`Permissions::exceeds_from`).
    if ch != Some(mine.id) && let Some(kind) = kinds.into_iter().find(|k| perms.exceeds_from(&mine.permissions, *k, me.kind)) {
        return Err(format!(
            "{what} lets {} do more than your channel does, so an answer can't carry your words into it. \
             Ask the human to answer it there (or the asker's own lead to record it)", kind.as_str()));
    }
    Ok(())
}

pub fn outcome(status: DeliveryStatus, note: String) -> Delivery {
    Delivery { status, note: Some(note), to: None, msg_id: None, session_id: None, at: Some(now_ms()), fresh: None }
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
    /// The cold-answer rule, for the deliveries it covers (answers to questions; None for the rest).
    pub fresh: Option<FreshRule<'a>>,
}

/// The cold-answer rule for one delivery (see the module notes).
pub struct FreshRule<'a> {
    /// The human asked for a new conversation (`questions::Answer::into`): no size or cold check,
    /// the other checks still apply.
    pub asked: bool,
    /// The decision already saved with this delivery (a backend restart came since).
    pub decided: Option<&'a Fresh>,
    /// Save the decision with the delivery (None: undone), before the agent is restarted.
    pub save: &'a (dyn Fn(Option<&Fresh>) -> anyhow::Result<()> + Sync),
}

/// What every new conversation Muster starts is told first.
pub const READ_NOTES: &str = "Read your memory/ notes first";

/// The first line of a message that starts a new conversation.
pub fn fresh_line(f: &Fresh) -> String {
    format!("Muster started you in a new conversation for this message, because {}. \
             {READ_NOTES}; the message below has everything it is about.", f.why)
}

/// The note an idle agent reset leaves for the new conversation (src/auto_reset.rs), `why` as
/// `fresh_verdict` gave it.
pub fn reset_line(why: &str) -> String {
    format!("Muster started you in a new conversation while you were idle, because {why}. \
             {READ_NOTES}, before anything else.")
}

/// The words `l` carries (given `note`, as `Letter::text`), after the new-conversation line when
/// one was decided: the same text whenever it is built for the same decision, so a restart finds
/// the message it sent.
fn letter_text(l: &Letter<'_>, fresh: Option<&Fresh>, note: Option<&str>) -> String {
    let text = (l.text)(note);
    match fresh {
        Some(f) => format!("{}\n\n{text}", fresh_line(f)),
        None => text,
    }
}

/// An answer to a question waiting this long since the human's own prompt in the conversation
/// leaves that conversation alone (they worked in it lately).
pub const HUMAN_RECENT_MS: i64 = 24 * 60 * 60 * 1000;
/// Keys typed into the agent this recently: the human is at it (as for a schedule's fresh start).
const HUMAN_TYPING_MS: u128 = 120_000;

/// What `fresh_verdict` decides on, read from the hub (`fresh_facts`).
#[derive(Debug, Clone, Default)]
pub struct FreshFacts {
    pub claude: bool,
    /// Running and resting: idle, nothing queued.
    pub idle: bool,
    /// Running, but a sub-agent, a background task or a turn just typed in is under way (the
    /// screen can look idle meanwhile): a restart would cut it off.
    pub working: bool,
    /// Asleep: its run was stopped, its conversation kept.
    pub asleep: bool,
    /// How long it has been idle, once its cache has likely expired (src/sleep.rs `wake_cost`).
    pub cold_for: Option<i64>,
    /// How long it has rested since it last did anything, cache warm or not (src/sleep.rs
    /// `idle_since`; None: unknown).
    pub idle_for: Option<i64>,
    /// Its conversation's size in tokens (None: unknown).
    pub tokens: Option<u64>,
    /// Something else is waiting to reach it: prompts in its queue, another delivery under way.
    pub others_waiting: bool,
    /// The human is at it: a draft in its input box, or keys typed in the last two minutes.
    pub typing: bool,
    /// When the human last had a prompt of their own taken in by it.
    pub human_prompt_at: Option<i64>,
    /// Its next start is a new conversation already (a reset or a move the owner chose), or it
    /// asked to start itself fresh (src/self_handoff.rs) and that waits for it to rest.
    pub reset_due: bool,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The current conversation, and why.
    Keep(&'static str),
    /// A new conversation: why in a few words, and the old one's size in thousands of tokens
    /// (None: the human asked, whatever the size). The delivery's words and the Questions tab's
    /// line before an answer both come from here, so they give the same number.
    Fresh { why: String, size_k: Option<u64> },
}

/// Who asks `rule_verdict` whether to replace a resting agent's conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// An answer to one of its questions: cold and at least `cache::FRESH_ANSWER_MIN_TOKENS`.
    Answer,
    /// An answer the human asked to go into a new conversation: the resting checks only.
    Asked,
    /// An idle agent reset (src/auto_reset.rs, a setting the owner turns on): as `Answer`, or
    /// warm, resting `cache::FRESH_WARM_AFTER_MS` and at least `cache::FRESH_WARM_MIN_TOKENS`.
    Sweep,
}

/// The cold-answer rule (`rule_verdict` for an answer). `asked`: the human asked for a new
/// conversation.
pub fn fresh_verdict(f: &FreshFacts, asked: bool) -> Verdict {
    rule_verdict(f, if asked { Rule::Asked } else { Rule::Answer })
}

/// The rule for replacing a resting agent's conversation, one function for answers to its
/// questions and idle agent resets, so both give the same reasons.
pub fn rule_verdict(f: &FreshFacts, rule: Rule) -> Verdict {
    use Verdict::Keep;
    if !f.idle && !f.asleep {
        return Keep("it is in a turn, starting, or not running");
    }
    if f.working {
        return Keep("it has a sub-agent, a background task or a new turn under way");
    }
    if f.others_waiting {
        return Keep("other prompts are waiting for it");
    }
    if f.typing {
        return Keep("the human is typing into it");
    }
    if f.reset_due {
        return Keep("its next start is a new conversation already");
    }
    if rule == Rule::Asked {
        return Verdict::Fresh { why: "the human asked for a fresh start".into(), size_k: None };
    }
    if !f.claude {
        return Keep("only Claude agents report their conversation's size");
    }
    let Some(tokens) = f.tokens else { return Keep("its conversation's size is unknown") };
    match f.cold_for {
        Some(_) if tokens < crate::cache::FRESH_ANSWER_MIN_TOKENS => return Keep("its conversation is small"),
        Some(_) => {}
        None if rule != Rule::Sweep || tokens < crate::cache::FRESH_WARM_MIN_TOKENS =>
            return Keep("its prompt cache is still warm"),
        None if f.idle_for.is_none_or(|t| t < crate::cache::FRESH_WARM_AFTER_MS) =>
            return Keep("its last turn ended a few minutes ago"),
        None => {}
    }
    if f.human_prompt_at.is_some_and(|t| f.now - t < HUMAN_RECENT_MS) {
        return Keep("the human worked in this conversation in the last 24 hours");
    }
    let size_k = (tokens + 500) / 1000;
    let why = match (f.cold_for, f.idle_for) {
        (Some(idle), _) => format!("the old one was cold and large, about {size_k}k tokens and idle {}", crate::cache::idle_text(idle)),
        (None, idle) => format!("the old one was very large, about {size_k}k tokens, and idle {}",
            crate::cache::idle_text(idle.unwrap_or(0))),
    };
    Verdict::Fresh { why, size_k: Some(size_k) }
}

/// `fresh_verdict`'s facts for agent `a` (its record as it is now) at `now`, with `session` its
/// session as the caller read it (None: it has none). Resting is what a sleep needs too
/// (src/sleep.rs `blocker`): no sub-agent, background task, turn just typed in or wake under way.
pub fn fresh_facts(hub: &Hub, a: &Agent, session: Option<&SessionInfo>, now: i64) -> FreshFacts {
    let Some(info) = session else { return FreshFacts { now, ..FreshFacts::default() } };
    let live = info.state != AgentState::Exited && hub.live_session_ids().contains(&info.id);
    let keys = hub.human_input_of(info.id).is_some_and(|(_, at)| at.elapsed().as_millis() < HUMAN_TYPING_MS);
    let (_, f) = crate::sleep::facts(hub, a);
    let idle = live && agents::session_idle(info);
    FreshFacts {
        claude: a.kind == SessionKind::Claude,
        idle,
        working: live && (info.subagents > 0 || info.background > 0 || f.active_at.is_some_and(|at| at >= info.state_since)),
        asleep: !live && a.asleep_at.is_some(),
        // By the cache's lifetime, whether or not the cold warnings are shown.
        cold_for: hub.cache_gone_at(info).filter(|t| now >= *t).map(|_| now - info.last_turn_at.unwrap_or(now)),
        // Since it last came to rest or talked to its model (the human's keys count as `typing`);
        // asleep, since its last turn (stopping the process isn't activity).
        idle_for: info.last_turn_at.map(|t| (now - if live { crate::sleep::idle_since(info, None) } else { t }).max(0)),
        tokens: hub.agents().context_size(a),
        others_waiting: !f.queue_idle || f.deliveries > 0 || f.waking,
        typing: keys || (idle && f.draft),
        human_prompt_at: hub.agents().human_prompt_at(a.id),
        reset_due: a.reset_on_next_start || a.handoff.is_some() || crate::self_handoff::pending(hub, a.id),
        now,
    }
}

/// What the cold-answer rule decided for one delivery (`fresh_plan`).
enum Plan {
    /// The current conversation.
    Current,
    /// A new conversation, started before a backend restart.
    Started(Fresh),
    /// Start a new conversation now: the agent's `resume_id` must still be `expect`, and the rule
    /// must still hold (`asked`: by its resting checks only) for conversation `sized` (`expect`,
    /// or the one a start cut off by a backend restart had already forgotten).
    Start { fresh: Fresh, expect: Option<String>, sized: Option<String>, asked: bool },
}

/// `a` as the rule weighs it: resuming conversation `sized` (its size, read for that one).
fn sized_as(a: &Agent, sized: &Option<String>) -> Agent {
    Agent { resume_id: sized.clone(), ..a.clone() }
}

/// A fresh start of agent `a` that stopped before a new run took over: it resumes `f.replaces`
/// again (unless it has another conversation by now), and the replaced run's hooks count again.
fn take_back(hub: &Hub, a: &Agent, f: &Fresh) -> anyhow::Result<()> {
    let Some(old) = &f.replaces else { return Ok(()) };
    if let Some(back) = hub.agents().restore_conversation(hub.store(), a.id, old)? {
        tracing::info!(agent = a.name, "the fresh start for an answer stopped; it keeps its conversation");
        agents::publish(hub, &back);
    }
    if let Some(sid) = a.session_id {
        hub.unfence_conversation(sid, f.run);
    }
    Ok(())
}

/// Is `f`'s replaced run (the process that held the old conversation) still the agent's live run?
fn old_run_alive(hub: &Hub, a: &Agent, f: &Fresh) -> bool {
    let Some(sid) = a.session_id else { return false };
    f.run.is_some() && hub.live_session_ids().contains(&sid) && hub.pty_generation(sid) == f.run
}

/// The cold-answer rule for agent `to` (as it is now): a decision saved before a backend restart
/// is finished or undone; otherwise the rule is applied and a new decision saved before anything
/// is restarted. Err: a decision Muster couldn't undo in its record.
fn fresh_plan(hub: &Hub, to: &Agent, rule: &FreshRule<'_>) -> Result<Plan, String> {
    let undo = || (rule.save)(None).map_err(|e| format!("couldn't record that it stays in the current conversation: {e:#}"));
    let asked = rule.asked;
    let mut sized = to.resume_id.clone();
    if let Some(f) = rule.decided {
        // A fresh start forgets the old conversation before it replaces the run. Forgotten and
        // the run replaced: done. Forgotten but the old run still there (the restart was cut
        // off): decided again, on the old conversation's facts (its size, the human's prompts),
        // since the human may have worked in that run meanwhile.
        let forgotten = to.resume_id != f.replaces || f.replaces.is_none();
        if forgotten && !old_run_alive(hub, to, f) {
            return Ok(Plan::Started(f.clone()));
        }
        if to.resume_id.is_none() {
            sized.clone_from(&f.replaces);
        }
    }
    let session = to.session_id.and_then(|s| hub.session(s));
    match (fresh_verdict(&fresh_facts(hub, &sized_as(to, &sized), session.as_ref(), now_ms()), asked), rule.decided) {
        (Verdict::Keep(why), decided) => {
            tracing::debug!(agent = to.name, "an answer goes into the current conversation: {why}");
            if let Some(f) = decided {
                take_back(hub, to, f).map_err(|e| format!("couldn't give it its conversation back: {e:#}"))?;
                undo()?;
            }
            Ok(Plan::Current)
        }
        // A decision saved before a restart keeps its words (the message is found by them).
        (Verdict::Fresh { .. }, Some(f)) => Ok(Plan::Start { fresh: f.clone(), expect: to.resume_id.clone(), sized, asked }),
        (Verdict::Fresh { why, .. }, None) => {
            let fresh = Fresh { why, replaces: to.resume_id.clone(), run: to.session_id.and_then(|s| hub.pty_generation(s)) };
            if let Err(e) = (rule.save)(Some(&fresh)) {
                tracing::warn!(agent = to.name, "not starting a new conversation: saving the decision failed: {e:#}");
                return Ok(Plan::Current);
            }
            Ok(Plan::Start { fresh, expect: to.resume_id.clone(), sized, asked })
        }
    }
}

/// Serializes the deliveries the cold-answer rule covers per agent, so a second answer sees what
/// the first did (and goes into the new conversation, after the first).
pub(crate) async fn rule_lock(hub: &Hub, agent: i64) -> tokio::sync::OwnedMutexGuard<()> {
    let m = hub.agents().answer_locks.lock().entry(agent).or_default().clone();
    m.lock_owned().await
}

/// Deliver `l` to agent `to` (`note`: why it isn't the poster), with the cold-answer rule when
/// `l` has it. Every delivery check runs before anything is restarted, so a refused answer leaves
/// the conversation as it was; the fresh start and the answer's place in the new run's queue are
/// taken under one hold of the start lock, so no other message gets in first.
async fn deliver_to_agent(hub: &Arc<Hub>, l: &Letter<'_>, to: Agent, note: Option<String>) -> Delivery {
    let Some(rule) = &l.fresh else { return send_to_agent(hub, l, &to, note.as_deref(), None, None).await };
    let _one = rule_lock(hub, to.id).await;
    // Deciding, restarting, giving a conversation back and queuing the answer all happen under
    // one hold of the start lock: no other start (Start fresh, a message waking it) gets between.
    let held = hub.agents().start_lock.lock().await;
    let Some(now) = agent_then(hub, to.id, Some(to.created_at)) else {
        // Removed meanwhile: the send refuses it, saying so.
        return send_to_agent(hub, l, &to, note.as_deref(), None, Some(&held)).await;
    };
    // Refused: nothing is restarted for it. A fresh start a backend restart cut off is settled
    // first, so the refusal leaves no conversation forgotten.
    if let Err(e) = agents::precheck(hub, &now, &(l.text)(note.as_deref()), &l.from, l.from_session, l.prov, None) {
        if let Some(f) = rule.decided {
            settle_cut_off(hub, &now, f);
        }
        return failed(&now, None, e);
    }
    let (fresh, expect, sized, asked) = match fresh_plan(hub, &now, rule) {
        Err(why) => {
            // Nothing was sent: sent again later, it is decided afresh.
            let mut d = outcome(DeliveryStatus::Undelivered, format!("not delivered: {why}"));
            d.to = Some(now.name.clone());
            return d;
        }
        Ok(Plan::Current) => return send_to_agent(hub, l, &now, note.as_deref(), None, Some(&held)).await,
        Ok(Plan::Started(f)) => return send_to_agent(hub, l, &now, note.as_deref(), Some(f), Some(&held)).await,
        Ok(Plan::Start { fresh, expect, sized, asked }) => (fresh, expect, sized, asked),
    };
    let current = |hub: &Hub| agent_then(hub, to.id, Some(to.created_at));
    // Checked again here, for an asleep agent too, and once more by the start itself for a
    // running one (with no await in between): the conversation is still the one decided on, and
    // the rule still holds.
    let ok = |s: &SessionInfo| current(hub).is_some_and(|a| a.resume_id == expect
        && matches!(fresh_verdict(&fresh_facts(hub, &sized_as(&a, &sized), Some(s), now_ms()), asked), Verdict::Fresh { .. }));
    let still = now.session_id.and_then(|s| hub.session(s)).is_some_and(|s| ok(&s));
    let why = if !still {
        "it is no longer resting".to_string()
    } else {
        match agents::start_fresh_if_held(hub, &now.name, &ok, &held).await {
            Ok(_) => return send_to_agent(hub, l, &now, note.as_deref(), Some(fresh), Some(&held)).await,
            Err(e) if e.downcast_ref::<agents::NotIdle>().is_some() => "it is no longer resting".to_string(),
            Err(e) => format!("{e:#}"),
        }
    };
    // Not started. A new run that holds a new conversation stays (the answer goes to it, saying
    // so); otherwise a conversation the failed start forgot is the agent's again (its old run
    // still there, or none running), and the answer goes into it.
    if let Some(a) = current(hub) {
        if new_run(hub, &a, &fresh) {
            if a.resume_id != fresh.replaces && fresh.replaces.is_some() {
                return send_to_agent(hub, l, &now, note.as_deref(), Some(fresh), Some(&held)).await;
            }
        } else if let Err(e) = take_back(hub, &a, &fresh) {
            // Forgotten for good: its next run starts a new conversation, as the answer says.
            tracing::warn!(agent = now.name, "couldn't give it its conversation back: {e:#}");
            return send_to_agent(hub, l, &now, note.as_deref(), Some(fresh), Some(&held)).await;
        }
    }
    tracing::info!(agent = now.name, "an answer goes into the current conversation after all: {why}");
    if let Err(e) = (rule.save)(None) {
        let mut d = outcome(DeliveryStatus::Undelivered,
            format!("not delivered: couldn't record that it stays in the current conversation: {e:#}"));
        d.to = Some(now.name.clone());
        return d;
    }
    send_to_agent(hub, l, &now, note.as_deref(), None, Some(&held)).await
}

/// Is a run other than the one fresh start `f` replaces live for agent `a` now?
fn new_run(hub: &Hub, a: &Agent, f: &Fresh) -> bool {
    a.session_id.is_some_and(|s| hub.live_session_ids().contains(&s)) && !old_run_alive(hub, a, f)
}

/// An answer whose saved fresh start `f` (from before a backend restart) won't go ahead: unless a
/// new run took over, agent `a` gets its old conversation back. The caller holds the start lock.
fn settle_cut_off(hub: &Hub, a: &Agent, f: &Fresh) {
    if !new_run(hub, a, f)
        && let Err(e) = take_back(hub, a, f)
    {
        tracing::warn!(agent = a.name, "couldn't give it its conversation back: {e:#}");
    }
}

/// A delivery to agent `to` that failed with `e` (a guard's refusal, or an error).
fn failed(to: &Agent, fresh: Option<Fresh>, e: anyhow::Error) -> Delivery {
    let status = if e.downcast_ref::<agents::Refused>().is_some() { DeliveryStatus::Refused } else { DeliveryStatus::Undelivered };
    Delivery { to: Some(to.name.clone()), fresh, ..outcome(status, format!("not delivered to {}: {e:#}", to.name)) }
}

/// Send `l` to agent `to` by name (`fresh`: in the new conversation Muster started for it;
/// `held`: the caller holds the start lock).
async fn send_to_agent(hub: &Arc<Hub>, l: &Letter<'_>, to: &Agent, note: Option<&str>, fresh: Option<Fresh>,
    held: Option<&agents::StartHeld<'_>>) -> Delivery {
    let text = letter_text(l, fresh.as_ref(), note);
    let mut d = outcome(DeliveryStatus::Delivered, String::new());
    d.to = Some(to.name.clone());
    d.fresh = fresh.clone();
    let started = fresh.as_ref().map(|f| format!("started {} in a new conversation, because {}; ", to.name, f.why))
        .unwrap_or_default();
    // Delivered by name, as late replies are: an agent removed and another made with its name in
    // the moment between `target` and the start would get it (only the human removes and makes
    // agents).
    let sent = match held {
        Some(held) => agents::send_checked_held(hub, to, text.clone(), l.from.clone(), l.from_session, l.prov, held).await,
        None => agents::send_checked(hub, to, text.clone(), l.from.clone(), l.from_session, l.prov, None).await,
    };
    match sent {
        Ok((sent, _)) => {
            d.msg_id = Some(sent.msg_id);
            d.session_id = Some(sent.session_id);
            d.note = Some(if sent.delivered {
                format!("{started}typed into {} as message {}", to.name, sent.msg_id)
            } else {
                format!("{started}queued for {} as message {}: typed in when it is between steps", to.name, sent.msg_id)
            });
            d
        }
        Err(e) if e.downcast_ref::<crate::inject::DeliveryPending>().is_some() => {
            d.msg_id = hub.store().find_question_delivery(to.id, &l.from, &text).ok().flatten();
            d.note = Some(format!("{started}queued for {}; Muster is confirming its delivery", to.name));
            d
        }
        // The decision stays with it: the restart finds the agent restarted already.
        Err(_) if hub.is_shutting_down() => Delivery { fresh, ..interrupted() },
        Err(e) => failed(to, fresh, e),
    }
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
        if let Target::Agent(to, _) = &t {
            d.to = Some(to.name.clone());
            if let Some(f) = l.fresh.as_ref().and_then(|r| r.decided) {
                let _held = hub.agents().start_lock.lock().await;
                if let Some(a) = agent_then(hub, to.id, Some(to.created_at)) {
                    settle_cut_off(hub, &a, f);
                }
            }
        }
        return d;
    }
    match t {
        Target::NotNeeded(why) => outcome(DeliveryStatus::NotNeeded, why),
        Target::Gone(why) => outcome(DeliveryStatus::Undelivered, format!("not delivered: {why}")),
        Target::Agent(to, note) => deliver_to_agent(hub, l, to, note).await,
        Target::Session(s, note) => {
            // Never typed into a shell, whoever recorded it (`check_reach` says so too).
            if s.kind == SessionKind::Shell {
                return outcome(DeliveryStatus::Refused, format!("not delivered: session {} is a shell", s.id));
            }
            if let Err(why) = crate::mcp::guard::check_session_route(s.kind, route.origin, route.hop) {
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
            let fresh = l.fresh.as_ref().and_then(|r| r.decided);
            match hub.store().find_question_delivery(to.id, &l.from, &letter_text(l, fresh, note.as_deref()))? {
                None => None,
                Some(n) => match hub.store().agent_message(n)? {
                    Some(c) if c.refused.is_some() => Some(Delivery { msg_id: Some(n), to: Some(to.name.clone()), fresh: fresh.cloned(),
                        ..outcome(DeliveryStatus::Refused, format!("not delivered to {}: {}", to.name, c.refused.unwrap_or_default())) }),
                    Some(c) if c.reply.is_some() || hub.store().agent_message_delivered_at(n)?.is_some()
                        || crate::inject::holds_message(hub, n) =>
                    {
                        let started = fresh.map(|f| format!("started {} in a new conversation, because {}; ", to.name, f.why))
                            .unwrap_or_default();
                        Some(Delivery { msg_id: Some(n), to: Some(to.name.clone()), fresh: fresh.cloned(),
                            ..outcome(DeliveryStatus::Delivered, format!("{started}delivered to {} as message {n}", to.name)) })
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const HOUR: i64 = 3_600_000;

    /// A named Claude agent at rest, its cache gone 3 hours ago, 216k tokens, nothing else going on.
    fn cold_large() -> FreshFacts {
        FreshFacts {
            claude: true, idle: true, working: false, asleep: false, cold_for: Some(3 * HOUR), idle_for: Some(3 * HOUR),
            tokens: Some(216_003), others_waiting: false, typing: false, human_prompt_at: None, reset_due: false, now: NOW,
        }
    }

    fn keep(f: &FreshFacts, asked: bool) -> &'static str {
        match fresh_verdict(f, asked) {
            Verdict::Keep(why) => why,
            v => panic!("{v:?} for {f:?}"),
        }
    }

    #[test]
    fn a_cold_large_resting_conversation_is_replaced_and_says_why() {
        assert_eq!(fresh_verdict(&cold_large(), false), Verdict::Fresh {
            why: "the old one was cold and large, about 216k tokens and idle 3 h".into(), size_k: Some(216) });
        // Asleep (its run stopped) counts as resting too.
        let asleep = FreshFacts { idle: false, asleep: true, ..cold_large() };
        assert!(matches!(fresh_verdict(&asleep, false), Verdict::Fresh { .. }));
        // The line is the constant, inclusive; the human's prompt over a day ago doesn't count.
        let edge = FreshFacts { tokens: Some(crate::cache::FRESH_ANSWER_MIN_TOKENS), human_prompt_at: Some(NOW - HUMAN_RECENT_MS), ..cold_large() };
        assert!(matches!(fresh_verdict(&edge, false), Verdict::Fresh { .. }));
    }

    #[test]
    fn every_other_case_keeps_the_conversation() {
        let c = cold_large;
        assert_eq!(keep(&FreshFacts { cold_for: None, tokens: Some(199_999), ..c() }, false), "its prompt cache is still warm");
        assert_eq!(keep(&FreshFacts { tokens: Some(99_999), ..c() }, false), "its conversation is small");
        assert_eq!(keep(&FreshFacts { tokens: None, ..c() }, false), "its conversation's size is unknown");
        assert_eq!(keep(&FreshFacts { claude: false, ..c() }, false), "only Claude agents report their conversation's size");
        // Mid-turn, starting, waiting on the human, exited without being asleep: never restarted.
        assert_eq!(keep(&FreshFacts { idle: false, ..c() }, false), "it is in a turn, starting, or not running");
        assert_eq!(keep(&FreshFacts { working: true, ..c() }, false), "it has a sub-agent, a background task or a new turn under way");
        assert_eq!(keep(&FreshFacts { others_waiting: true, ..c() }, false), "other prompts are waiting for it");
        assert_eq!(keep(&FreshFacts { typing: true, ..c() }, false), "the human is typing into it");
        // A reset the owner chose is due at its next start: that start keeps it, not this rule
        // (a failed fresh start could otherwise hand the old conversation back without it).
        assert_eq!(keep(&FreshFacts { reset_due: true, ..c() }, true), "its next start is a new conversation already");
        assert_eq!(keep(&FreshFacts { human_prompt_at: Some(NOW - 23 * HOUR), ..c() }, false),
            "the human worked in this conversation in the last 24 hours");
        // No session at all.
        assert_eq!(keep(&FreshFacts { now: NOW, ..FreshFacts::default() }, false), "it is in a turn, starting, or not running");
    }

    /// Warm (its cache not expired yet) and resting `idle` since its last turn, `tokens` large.
    fn warm(tokens: u64, idle: i64) -> FreshFacts {
        FreshFacts { cold_for: None, idle_for: Some(idle), tokens: Some(tokens), ..cold_large() }
    }

    #[test]
    fn each_line_of_the_rule_sits_where_the_constants_say() {
        const MIN: i64 = 60_000;
        let cold = |t| FreshFacts { tokens: Some(t), ..cold_large() };
        // Cold: 100k and over.
        assert_eq!(keep(&cold(99_999), false), "its conversation is small");
        assert_eq!(fresh_verdict(&cold(100_000), false), Verdict::Fresh {
            why: "the old one was cold and large, about 100k tokens and idle 3 h".into(), size_k: Some(100) });
        // The same lines for an idle agent reset.
        assert_eq!(sweep_keep(&cold(99_999)), "its conversation is small");
        assert!(matches!(rule_verdict(&cold(100_000), Rule::Sweep), Verdict::Fresh { .. }));
        // Warm: 200k and over, once it has rested the grace time; idle agent resets only (an
        // answer keeps a warm conversation, as before resets existed).
        assert_eq!(sweep_keep(&warm(199_999, 20 * MIN)), "its prompt cache is still warm");
        assert_eq!(rule_verdict(&warm(200_000, 20 * MIN), Rule::Sweep), Verdict::Fresh {
            why: "the old one was very large, about 200k tokens, and idle 20 m".into(), size_k: Some(200) });
        assert_eq!(keep(&warm(500_000, 3 * HOUR), false), "its prompt cache is still warm");
        assert_eq!(crate::cache::FRESH_WARM_MIN_TOKENS, 200_000);
        let grace = crate::cache::FRESH_WARM_AFTER_MS;
        assert_eq!(sweep_keep(&warm(250_000, grace - 1)), "its last turn ended a few minutes ago");
        assert!(matches!(rule_verdict(&warm(250_000, grace), Rule::Sweep), Verdict::Fresh { .. }));
        // When it last came to rest is unknown: it may have just ended a turn.
        assert_eq!(sweep_keep(&FreshFacts { idle_for: None, ..warm(250_000, grace) }), "its last turn ended a few minutes ago");
        // The human's prompt in the last 24 hours keeps a warm one too.
        assert_eq!(sweep_keep(&FreshFacts { human_prompt_at: Some(NOW - 23 * HOUR), ..warm(250_000, grace) }),
            "the human worked in this conversation in the last 24 hours");
        // Unknown size: never, warm or cold.
        assert_eq!(sweep_keep(&FreshFacts { tokens: None, ..warm(250_000, grace) }), "its conversation's size is unknown");
        // A Codex agent: never.
        assert_eq!(sweep_keep(&FreshFacts { claude: false, ..warm(250_000, grace) }),
            "only Claude agents report their conversation's size");
        // Mid-turn, queued, typing, a reset or move already due: never.
        let w = || warm(250_000, grace);
        assert_eq!(sweep_keep(&FreshFacts { idle: false, ..w() }), "it is in a turn, starting, or not running");
        assert_eq!(sweep_keep(&FreshFacts { working: true, ..w() }), "it has a sub-agent, a background task or a new turn under way");
        assert_eq!(sweep_keep(&FreshFacts { others_waiting: true, ..w() }), "other prompts are waiting for it");
        assert_eq!(sweep_keep(&FreshFacts { typing: true, ..w() }), "the human is typing into it");
        assert_eq!(sweep_keep(&FreshFacts { reset_due: true, ..w() }), "its next start is a new conversation already");
    }

    fn sweep_keep(f: &FreshFacts) -> &'static str {
        match rule_verdict(f, Rule::Sweep) {
            Verdict::Keep(why) => why,
            v => panic!("{v:?} for {f:?}"),
        }
    }

    #[test]
    fn a_fresh_start_the_human_asked_for_skips_only_the_size_checks() {
        let warm_small = FreshFacts { cold_for: None, tokens: Some(5_000), human_prompt_at: Some(NOW - HOUR), claude: false, ..cold_large() };
        assert_eq!(fresh_verdict(&warm_small, true), Verdict::Fresh { why: "the human asked for a fresh start".into(), size_k: None });
        assert!(matches!(fresh_verdict(&FreshFacts { tokens: None, ..cold_large() }, true), Verdict::Fresh { .. }));
        // Never mid-turn, over other prompts or what the human is typing.
        assert_eq!(keep(&FreshFacts { idle: false, ..warm_small.clone() }, true), "it is in a turn, starting, or not running");
        assert_eq!(keep(&FreshFacts { others_waiting: true, ..warm_small.clone() }, true), "other prompts are waiting for it");
        assert_eq!(keep(&FreshFacts { typing: true, ..warm_small }, true), "the human is typing into it");
    }

    #[test]
    fn the_new_conversation_line_comes_first_and_the_old_text_is_unchanged_without_one() {
        let text = |n: Option<&str>| format!("Answer to your question Q1: a.{}", n.unwrap_or(""));
        let envelope = |_: &str, _: Origin, _: u32| String::new();
        let save = |_: Option<&Fresh>| Ok(());
        let poster = Who { label: "builder-2".into(), session_id: 2, kind: SessionKind::Claude, agent_id: None,
            agent_created_at: None, parent_id: None, parent_agent_id: None, parent_agent_created_at: None };
        let l = Letter { poster: &poster, recorder: None, from: "human".into(), from_session: None, prov: None,
            text: &text, envelope: &envelope, prefix: String::new(),
            fresh: Some(FreshRule { asked: false, decided: None, save: &save }) };
        assert_eq!(letter_text(&l, None, None), "Answer to your question Q1: a.");
        let f = Fresh { why: "the old one was cold and large, about 216k tokens and idle 3 h".into(), replaces: Some("c-1".into()), run: None };
        let t = letter_text(&l, Some(&f), None);
        assert!(t.starts_with("Muster started you in a new conversation for this message, because the old one was cold and large, \
            about 216k tokens and idle 3 h. Read your memory/ notes first;"), "{t}");
        assert!(t.ends_with("\n\nAnswer to your question Q1: a."), "{t}");
        // Built again for the same decision (after a restart): the same text, so it is found.
        assert_eq!(t, letter_text(&l, Some(&f.clone()), None));
    }
}
