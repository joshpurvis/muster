//! Idle agent resets (docs/PROTOCOL.md "Idle agent resets"): with the setting on
//! (`UiPrefs::auto_reset`, off by default, changed only by a click in Muster's window), Muster
//! gives a resting named Claude agent whose conversation is very large a new conversation by
//! itself. Its memory/ notes stay; the new conversation reads them.
//!
//! - The rule is the one answers to questions follow (src/relay.rs `rule_verdict`, `Rule::Sweep`,
//!   read from the hub by `fresh_facts`), so both give the same reasons: resting (idle or asleep,
//!   nothing in a turn, starting, waiting on the human, queued or on its way, nobody typing), its
//!   next start not a new conversation already, no prompt of the human's own taken in by it in the
//!   last 24 hours, its size known (Claude's status line), and either cold (past its cache
//!   lifetime) and at least `cache::FRESH_ANSWER_MIN_TOKENS`, or (resets only) warm, resting
//!   `cache::FRESH_WARM_AFTER_MS` and at least `cache::FRESH_WARM_MIN_TOKENS`. Codex agents,
//!   agents of unknown size and plain sessions (no memory/ to come back to): never.
//! - `sweep` looks every `SWEEP_EVERY`. Each agent is decided under the cold-answer rule's
//!   per-agent lock and the start lock, the order the answer path takes them, so an answer and a
//!   reset never both act on it, and nothing starts it in between.
//! - A running agent is put to sleep first, the way a sleep stops it (src/sleep.rs): its last
//!   check and the stop are one step against keys typed, messages and prompts on their way, and a
//!   restart in between finishes or undoes the stop. Then, asleep (whether it just fell asleep or
//!   already was), the rule is checked again and its conversation forgotten, while the setting is
//!   still on (`Hub::while_auto_reset_on`, which the sleep's claim also holds): whatever wakes it
//!   next starts a new conversation. No start is spent on it now; the stopped run's late hooks
//!   can't bring the old one back, also after a backend restart (`Hub::on_hook` ignores them for
//!   an asleep agent with no conversation saved). A restart between the sleep and the forgetting
//!   leaves it asleep in its old conversation, for the next sweep.
//! - After a backend restart, text in a running agent's input box that this backend didn't see
//!   typed (it can't tell a draft from Claude's suggestion) keeps it.
//! - It leaves the new conversation one quiet message (src/quiet.rs, from `muster`, the human's
//!   origin): why, and to read memory/ first. It never wakes the agent, arrives as context when
//!   its next turn starts, and shows in the Agents panel's message log. The agent's accepted
//!   origins, daily message cap and waiting quiet messages are checked before anything stops: an
//!   agent that couldn't take the note keeps its conversation.
//! - Each reset is also a `system` event in the agent's tab (`auto_reset`) and a notice in the
//!   window, saying why.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::json;

use crate::agents::{self, Agent, Origin, Route};
use crate::hub::Hub;
use crate::model::*;
use crate::relay::{self, Rule, Verdict};

/// How often resting agents are looked at.
pub const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);
/// Refusal for `prefs.set {auto_reset}` from anywhere but the window.
pub const BY_WINDOW: &str = "only a click in Muster's window turns idle agent resets on or off \
    (Settings and more, Idle agent resets…)";
/// The sender of the note a reset leaves.
pub const FROM: &str = "muster";

/// When this backend started looking (Unix ms): keys typed before it are unknown to it.
static WATCHING_SINCE: OnceLock<i64> = OnceLock::new();

/// `SWEEP_EVERY`; a debug build can shorten it for the smoke test.
fn every() -> Duration {
    if cfg!(debug_assertions)
        && let Some(v) = std::env::var("MUSTER_TEST_AUTO_RESET_EVERY_MS").ok().and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(v);
    }
    SWEEP_EVERY
}

/// What a sweep did to one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reset {
    pub agent: String,
    pub why: String,
    /// It was running, and was put to sleep for it.
    pub stopped: bool,
    /// The note it left (None: refused, see the log).
    pub note: Option<i64>,
}

/// At daemon start: sweep from time to time.
pub fn spawn(hub: Arc<Hub>) {
    let _ = WATCHING_SINCE.set(now_ms());
    tokio::spawn(async move {
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
    relay::rule_verdict(&relay::fresh_facts(hub, a, session.as_ref(), now), Rule::Sweep)
}

/// Could agent `a` take the note now (its accepted origins, its daily message cap, room among its
/// waiting quiet messages)?
fn note_allowed(hub: &Hub, a: &Agent, now: i64) -> Result<(), String> {
    let store = hub.store();
    let recent = store.count_agent_messages_since(a.id, now - crate::mcp::guard::AGENT_CAP_WINDOW_MS)
        .map_err(|e| format!("{e:#}"))?;
    crate::mcp::guard::check_agent_delivery(&a.name, &a.accepts_origins, Origin::Human, 0, recent)?;
    let waiting = store.quiet_count_agent(a.id, crate::quiet::after(now)).map_err(|e| format!("{e:#}"))?;
    if waiting >= crate::quiet::WAITING_MAX {
        return Err(format!("{waiting} quiet messages wait for it already"));
    }
    Ok(())
}

/// Text in running session `info`'s input box that this backend didn't see typed: since it came
/// to rest before this backend started, a draft can't be told from Claude's suggestion.
fn unseen_text(hub: &Hub, info: &SessionInfo) -> bool {
    WATCHING_SINCE.get().is_some_and(|since| info.state_since < *since) && crate::inject::has_any_text(hub, info.id)
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
    let (sid, replaces) = (a.session_id?, a.resume_id.clone()?);
    if let Err(no) = note_allowed(hub, &a, at()) {
        tracing::debug!(agent = a.name, "not reset: it couldn't take the note: {no}");
        return None;
    }
    let running = a.asleep_at.is_none();
    if running {
        if hub.session(sid).is_some_and(|i| unseen_text(hub, &i)) {
            tracing::debug!(agent = a.name, "not reset: text in its input box may be a draft typed before this backend started");
            return None;
        }
        if let Err(e) = crate::sleep::sleep_held(hub, &a.name, crate::sleep::Why::Reset, &held).await {
            tracing::debug!(agent = a.name, "not reset: {e:#}");
            return None;
        }
    }
    // Asleep now. Under the lock the setting is saved under: anything that came meanwhile (keys
    // held for a wake, a message on its way) keeps it, then the conversation is forgotten (once
    // forgotten, the stopped run's late hooks can't save it back: `Hub::on_hook`).
    let forgot = hub.while_auto_reset_on(|| -> anyhow::Result<Option<Agent>> {
        let Some(b) = current().filter(|b| b.asleep_at.is_some()) else { return Ok(None) };
        if !matches!(verdict(hub, &b, at()), Verdict::Fresh { .. }) {
            tracing::debug!(agent = b.name, "not reset: it is no longer resting");
            return Ok(None);
        }
        hub.agents().forget_conversation(hub.store(), id, &replaces)
    });
    let c = match forgot {
        Some(Ok(Some(c))) => c,
        None | Some(Ok(None)) => return None,
        Some(Err(e)) => {
            tracing::warn!(agent = a.name, "not reset: {e:#}");
            return None;
        }
    };
    // Its old conversation's cache clock goes with it (after: a restart in between leaves a
    // stale clock, never a saved conversation without one).
    if let Err(e) = hub.forget_run_conversation(sid) {
        tracing::warn!(agent = c.name, "clearing the old conversation's cache clock: {e:#}");
    }
    agents::publish(hub, &c);
    Some(record(hub, &c, &why, size_k, running).await)
}

/// The agent's log line for a reset, with `why` as the rule gave it.
pub fn log_text(name: &str, why: &str) -> String {
    format!("Idle agent reset: {name} is asleep, and its next start is a new conversation, because {why}. \
             Whatever wakes it starts the new one; its memory/ notes are kept.")
}

/// Leave the note, then say so in the agent's tab and the window.
async fn record(hub: &Arc<Hub>, a: &Agent, why: &str, size_k: Option<u64>, stopped: bool) -> Reset {
    let out = agents::Outgoing {
        agent: a.name.clone(),
        agent_id: Some(a.id),
        text: relay::reset_line(why),
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
    let mut text = log_text(&a.name, why);
    if note.is_none() {
        text.push_str(" Muster couldn't leave it a note saying so.");
    }
    tracing::info!(agent = a.name, "{text}");
    let channel = a.session_id.and_then(|s| hub.session(s)).map(|s| s.channel_id).or(a.channel_id);
    hub.log_event(channel, a.session_id, EventKind::System, json!({
        "text": text, "level": "info",
        "auto_reset": {"agent": a.name, "agent_id": a.id, "why": why, "size_k": size_k, "stopped": stopped, "note_msg": note},
    }));
    hub.notice("info", text);
    Reset { agent: a.name.clone(), why: why.to_string(), stopped, note }
}

#[cfg(test)]
mod tests;
