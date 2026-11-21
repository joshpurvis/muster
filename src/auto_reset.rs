//! Idle agent resets (docs/UI.md "Idle agent resets"): with the setting on (`UiPrefs::auto_reset`,
//! off by default, changed only by a click in Muster's window), Muster starts a resting named
//! Claude agent whose conversation is very large in a new conversation by itself. Its memory/
//! notes stay; the new conversation reads them.
//!
//! - The rule is the one answers to questions follow (src/relay.rs `fresh_verdict`, read from the
//!   hub by `fresh_facts`), so both give the same reasons: resting (idle or asleep, nothing in a
//!   turn, starting, waiting on the human, queued or on its way, nobody typing), its next start
//!   not a new conversation already, no prompt of the human's own taken in by it in the last 24
//!   hours, its size known (Claude's status line), and either cold (past its cache lifetime) and
//!   at least `cache::FRESH_ANSWER_MIN_TOKENS`, or warm, resting `cache::FRESH_WARM_AFTER_MS` and
//!   at least `cache::FRESH_WARM_MIN_TOKENS`. Codex agents, agents of unknown size and plain
//!   sessions (no memory/ to come back to): never.
//! - `sweep` looks every `SWEEP_EVERY`. Each agent is decided and restarted under the cold-answer
//!   rule's per-agent lock and the start lock, the order the answer path takes them, so an answer
//!   and a reset never both restart it, and nothing else starts it in between; the start itself
//!   checks the rule once more with no await between the check and the restart
//!   (`agents::start_fresh_if_held`).
//! - A running agent is restarted the way Start fresh restarts it. An asleep one stays asleep: its
//!   conversation is forgotten, so whatever wakes it next starts a new one (no wake is spent).
//! - It leaves the new conversation one quiet message (src/quiet.rs, from `muster`, the human's
//!   origin): why, and to read memory/ first. It never wakes the agent, arrives as context when
//!   its next turn starts, and shows in the Agents panel's message log. The agent's daily message
//!   cap and accepted origins are checked before anything restarts: an agent that couldn't take
//!   the note keeps its conversation.
//! - Each reset is a `system` event in the agent's tab (`auto_reset`), saying why.
//! - A backend restart between forgetting the conversation and the new run is settled at the next
//!   start (`settle`): the reset is saved first (`PENDING_KEY`), and as for an answer
//!   (`relay::settle_cut_off`), unless a new run took over, the agent gets its conversation back.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agents::{self, Agent, Origin, Route};
use crate::hub::Hub;
use crate::model::*;
use crate::relay::{self, Fresh, Verdict};

/// How often resting agents are looked at.
pub const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);
/// Refusal for `prefs.set {auto_reset}` from anywhere but the window.
pub const BY_WINDOW: &str = "only a click in Muster's window turns idle agent resets on or off \
    (Settings and more, Idle agent resets…)";
/// The sender of the note a reset leaves.
pub const FROM: &str = "muster";
/// Runtime key: resets started and not finished, by agent id.
const PENDING_KEY: &str = "auto-reset-pending";

/// `SWEEP_EVERY`; a debug build can shorten it for the smoke test.
fn every() -> Duration {
    if cfg!(debug_assertions)
        && let Some(v) = std::env::var("MUSTER_TEST_AUTO_RESET_EVERY_MS").ok().and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(v);
    }
    SWEEP_EVERY
}

/// A reset under way, saved before the agent is restarted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    created_at: i64,
    size_k: Option<u64>,
    fresh: Fresh,
}

/// What a sweep did to one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reset {
    pub agent: String,
    pub why: String,
    /// It was asleep: its next start is the new conversation.
    pub asleep: bool,
    /// The note it left (None: refused, see the log).
    pub note: Option<i64>,
}

/// At daemon start: settle a reset a restart cut off, then sweep from time to time.
pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        settle(&hub).await;
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every(), every());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if hub.is_shutting_down() {
                return;
            }
            sweep(&hub, now_ms()).await;
        }
    });
}

/// With the setting on, reset every agent the rule picks at `now`.
pub async fn sweep(hub: &Arc<Hub>, now: i64) -> Vec<Reset> {
    let mut out = Vec::new();
    if !hub.ui_prefs().auto_reset {
        return out;
    }
    for a in hub.agents().list() {
        // A look without the locks first: most agents are kept, and a lock would wait on a start.
        if a.kind != SessionKind::Claude || !matches!(verdict(hub, &a, now), Verdict::Fresh { .. }) {
            continue;
        }
        if let Some(r) = reset_one(hub, a.id, a.created_at, now).await {
            out.push(r);
        }
    }
    out
}

/// The rule for agent `a` as it is now.
fn verdict(hub: &Hub, a: &Agent, now: i64) -> Verdict {
    let session = a.session_id.and_then(|s| hub.session(s));
    relay::fresh_verdict(&relay::fresh_facts(hub, a, session.as_ref(), now), false)
}

/// Could agent `a` take the note now (its accepted origins, its daily message cap)?
fn note_allowed(hub: &Hub, a: &Agent, now: i64) -> Result<(), String> {
    let recent = hub.store().count_agent_messages_since(a.id, now - crate::mcp::guard::AGENT_CAP_WINDOW_MS)
        .map_err(|e| format!("{e:#}"))?;
    crate::mcp::guard::check_agent_delivery(&a.name, &a.accepts_origins, Origin::Human, 0, recent)
}

fn pending(hub: &Hub) -> HashMap<i64, Pending> {
    hub.store().load_runtime(PENDING_KEY).ok().flatten().unwrap_or_default()
}

fn set_pending(hub: &Hub, id: i64, p: Option<Pending>) -> anyhow::Result<()> {
    let mut all = pending(hub);
    match p {
        Some(p) => all.insert(id, p),
        None => all.remove(&id),
    };
    hub.store().save_runtime(PENDING_KEY, &all)
}

/// Decide again under the locks and reset agent `id` (made at `created_at`).
async fn reset_one(hub: &Arc<Hub>, id: i64, created_at: i64, now: i64) -> Option<Reset> {
    let _one = relay::rule_lock(hub, id).await;
    let held = hub.agents().start_lock.lock().await;
    let at = || now.max(now_ms());
    let current = || relay::agent_then(hub, id, Some(created_at));
    let a = current()?;
    // Turned off while this waited for the locks.
    if !hub.ui_prefs().auto_reset {
        return None;
    }
    let Verdict::Fresh { why, size_k } = verdict(hub, &a, at()) else { return None };
    if let Err(no) = note_allowed(hub, &a, at()) {
        tracing::debug!(agent = a.name, "not reset: it couldn't take the note: {no}");
        return None;
    }
    let live = a.session_id.is_some_and(|s| hub.live_session_ids().contains(&s));
    let fresh = Fresh { why, replaces: a.resume_id.clone(), run: a.session_id.and_then(|s| hub.pty_generation(s)) };
    if !live {
        // Asleep: no run to restart. Forget the conversation; whatever wakes it starts a new one.
        let replaces = fresh.replaces.clone()?;
        return match hub.agents().forget_conversation(hub.store(), id, &replaces) {
            Ok(Some(b)) => {
                agents::publish(hub, &b);
                Some(record(hub, &b, &fresh, size_k, true).await)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(agent = a.name, "not reset: {e:#}");
                None
            }
        };
    }
    if let Err(e) = set_pending(hub, id, Some(Pending { created_at, size_k, fresh: fresh.clone() })) {
        tracing::warn!(agent = a.name, "not reset: saving it first failed: {e:#}");
        return None;
    }
    // Checked again by the start itself, with no await between the check and the restart.
    let ok = |s: &SessionInfo| current().is_some_and(|c| c.resume_id == fresh.replaces
        && matches!(relay::fresh_verdict(&relay::fresh_facts(hub, &c, Some(s), at()), false), Verdict::Fresh { .. }));
    let started = agents::start_fresh_if_held(hub, &a.name, &ok, &held).await;
    let done = match started {
        Ok(s) => Some(s.agent),
        Err(e) => {
            if e.downcast_ref::<agents::NotIdle>().is_none() {
                tracing::warn!(agent = a.name, "not reset: {e:#}");
            }
            // As for an answer: a new run holding a new conversation stays; otherwise a
            // conversation the failed start forgot is the agent's again.
            match current() {
                Some(c) if relay::new_run(hub, &c, &fresh) && c.resume_id != fresh.replaces => Some(c),
                Some(c) => {
                    relay::settle_cut_off(hub, &c, &fresh);
                    None
                }
                None => None,
            }
        }
    };
    if let Err(e) = set_pending(hub, id, None) {
        tracing::warn!(agent = a.name, "clearing a finished reset: {e:#}");
    }
    match done {
        Some(b) => Some(record(hub, &b, &fresh, size_k, false).await),
        None => None,
    }
}

/// A reset a backend restart cut off: done if a new run took over (recorded now), else the agent
/// gets its conversation back.
pub async fn settle(hub: &Arc<Hub>) {
    for (id, p) in pending(hub) {
        let _one = relay::rule_lock(hub, id).await;
        let _held = hub.agents().start_lock.lock().await;
        if let Some(a) = relay::agent_then(hub, id, Some(p.created_at)) {
            if relay::new_run(hub, &a, &p.fresh) && a.resume_id != p.fresh.replaces {
                record(hub, &a, &p.fresh, p.size_k, false).await;
            } else {
                relay::settle_cut_off(hub, &a, &p.fresh);
            }
        }
        if let Err(e) = set_pending(hub, id, None) {
            tracing::warn!(agent = id, "clearing a settled reset: {e:#}");
        }
    }
}

/// The agent's log line for a reset, with `why` as the rule gave it.
pub fn log_text(name: &str, why: &str, asleep: bool) -> String {
    if asleep {
        format!("Idle agent reset: {name}'s next start is a new conversation, because {why}. \
                 It stays asleep until something wakes it; its memory/ notes are kept.")
    } else {
        format!("Idle agent reset: Muster started {name} in a new conversation, because {why}. \
                 Its memory/ notes are kept.")
    }
}

/// Leave the note, then say so in the agent's tab.
async fn record(hub: &Arc<Hub>, a: &Agent, f: &Fresh, size_k: Option<u64>, asleep: bool) -> Reset {
    let out = agents::Outgoing {
        agent: a.name.clone(),
        agent_id: Some(a.id),
        text: relay::reset_line(&f.why),
        from: FROM.into(),
        from_session: None,
        route: Route { origin: Origin::Human, hop: 0, reply_to: None },
    };
    let note = match agents::quiet(hub, out).await {
        Ok(q) => Some(q.msg_id),
        Err(e) => {
            tracing::warn!(agent = a.name, "an idle agent reset couldn't leave its note: {e:#}");
            None
        }
    };
    let mut text = log_text(&a.name, &f.why, asleep);
    if note.is_none() {
        text.push_str(" Muster couldn't leave it a note saying so (see the Agents panel's message log).");
    }
    tracing::info!(agent = a.name, "{text}");
    let channel = a.session_id.and_then(|s| hub.session(s)).map(|s| s.channel_id).or(a.channel_id);
    hub.log_event(channel, a.session_id, EventKind::System, json!({
        "text": text, "level": "info",
        "auto_reset": {"agent": a.name, "agent_id": a.id, "why": f.why, "size_k": size_k, "asleep": asleep,
            "replaces": f.replaces, "note_msg": note},
    }));
    Reset { agent: a.name.clone(), why: f.why.clone(), asleep, note }
}

#[cfg(test)]
mod tests;
