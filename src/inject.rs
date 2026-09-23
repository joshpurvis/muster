//! Intelligent injection: per-session input queue and reliable prompt submission.
//! OWNER: agents implementer. Used by rpc.rs and mcp.rs through the functions below.
//!
//! Delivery: wait until the adapter says the session is ready (normally Idle), clear leftover
//! text muster itself put in the input box (e.g. Claude restores an interrupted prompt), write
//! `prompt_bytes`, wait, write `submit_bytes`, then verify the agent took it (state left Idle,
//! or the input box no longer holds the text). If not, submit once more; if the text is still
//! sitting there, clear it and requeue (at most 3 attempts), so no half-typed input remains.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::agents::{Adapter, adapter_for};
use crate::hub::{Hub, ServerEvent};
use crate::model::{AgentState, Decision, EventKind, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Deliver when the agent is ready (default).
    #[default]
    Queue,
    /// Interrupt the current turn, then deliver.
    Interrupt,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendOutcome {
    /// True if it was written immediately; false if queued.
    pub delivered: bool,
    /// Position in queue after enqueueing (0 when delivered).
    pub queued: u32,
}

const SETTLE: Duration = Duration::from_millis(350);
const SUBMIT_DELAY_MIN: Duration = Duration::from_millis(120);
const VERIFY_FOR: Duration = Duration::from_millis(3000);
const VERIFY_RETRY_FOR: Duration = Duration::from_millis(1500);
const MAX_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone)]
struct Item {
    text: String,
    /// Clear whatever is in the input box first (set for Interrupt-mode sends).
    clear_first: bool,
    attempts: u8,
    /// The human was told this prompt waits behind their unsent draft.
    draft_noticed: bool,
}

#[derive(Default)]
struct SessQ {
    items: VecDeque<Item>,
    /// A delivery is in progress (only one per session at a time).
    busy: bool,
    /// The last text muster typed, to recognize it if the CLI puts it back in the box.
    last_sent: Option<String>,
}

static QUEUES: LazyLock<Mutex<HashMap<SessionId, SessQ>>> = LazyLock::new(Default::default);

fn queue_len(id: SessionId) -> u32 {
    QUEUES
        .lock()
        .get(&id)
        .map(|q| q.items.len() as u32)
        .unwrap_or(0)
}

fn sync_queued(hub: &Hub, id: SessionId) {
    hub.set_queued(id, queue_len(id));
}

fn log_prompt(hub: &Hub, id: SessionId, text: &str, from: &str, queued: bool) {
    let channel = hub.session(id).map(|s| s.channel_id);
    hub.log_event(
        channel,
        Some(id),
        EventKind::Prompt,
        serde_json::json!({"text": text, "session_id": id, "from": from, "queued": queued}),
    );
}

fn is_live(hub: &Hub, id: SessionId) -> bool {
    hub.session(id)
        .is_some_and(|s| s.state != AgentState::Exited)
        && hub.live_session_ids().contains(&id)
}

fn ready(hub: &Hub, id: SessionId) -> bool {
    hub.session(id)
        .is_some_and(|s| adapter_for(s.kind).is_ready(s.state))
}

/// Send a prompt. `from` is "human" or "session:<id>" (for the timeline).
pub async fn send(
    hub: &Arc<Hub>,
    id: SessionId,
    text: String,
    mode: SendMode,
    from: String,
) -> anyhow::Result<SendOutcome> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    if !is_live(hub, id) {
        anyhow::bail!("session {id} is not running");
    }
    if text.trim().is_empty() {
        anyhow::bail!("empty prompt");
    }
    let adapter = adapter_for(info.kind);
    let item = Item {
        text: text.clone(),
        clear_first: mode == SendMode::Interrupt,
        attempts: 0,
        draft_noticed: false,
    };

    if mode == SendMode::Interrupt {
        let busy_now = !adapter.is_ready(info.state);
        if busy_now {
            hub.write_input(id, &adapter.interrupt_bytes())?;
        }
        let n = {
            let mut g = QUEUES.lock();
            let q = g.entry(id).or_default();
            q.items.push_front(item);
            q.items.len() as u32
        };
        sync_queued(hub, id);
        log_prompt(hub, id, &text, &from, true);
        kick(hub.clone(), id);
        return Ok(SendOutcome {
            delivered: false,
            queued: n,
        });
    }

    // Deliver right away if nothing is ahead of us and the agent is ready.
    let deliver_now = {
        let mut g = QUEUES.lock();
        let q = g.entry(id).or_default();
        if q.items.is_empty() && !q.busy && adapter.is_ready(info.state) {
            q.busy = true;
            true
        } else {
            q.items.push_back(item.clone());
            false
        }
    };
    if !deliver_now {
        let n = queue_len(id);
        sync_queued(hub, id);
        log_prompt(hub, id, &text, &from, true);
        kick(hub.clone(), id);
        return Ok(SendOutcome {
            delivered: false,
            queued: n,
        });
    }

    log_prompt(hub, id, &text, &from, false);
    // Run in its own task so a dropped request (client gone) can't abandon a half-typed
    // prompt or leave the queue marked busy.
    let task = {
        let hub = hub.clone();
        tokio::spawn(async move {
            let r = deliver(&hub, id, item).await;
            finish(&hub, id);
            r
        })
    };
    let r = task
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("delivery task failed: {e}")));
    match r {
        Ok(true) => Ok(SendOutcome {
            delivered: true,
            queued: 0,
        }),
        Ok(false) => Ok(SendOutcome {
            delivered: false,
            queued: queue_len(id),
        }),
        Err(e) => Err(e),
    }
}

fn finish(hub: &Arc<Hub>, id: SessionId) {
    if let Some(q) = QUEUES.lock().get_mut(&id) {
        q.busy = false;
    }
    sync_queued(hub, id);
    kick(hub.clone(), id);
}

/// Start delivering the head of the queue if the session is ready and idle-handed.
fn kick(hub: Arc<Hub>, id: SessionId) {
    if !is_live(&hub, id) {
        drop_queue(&hub, id);
        return;
    }
    if !ready(&hub, id) {
        return;
    }
    {
        let mut g = QUEUES.lock();
        let Some(q) = g.get_mut(&id) else { return };
        if q.busy || q.items.is_empty() {
            return;
        }
        q.busy = true;
    }
    tokio::spawn(async move {
        // Let the TUI finish rendering after the state flip, then re-check.
        tokio::time::sleep(SETTLE).await;
        let is_ready = ready(&hub, id);
        let item = {
            let mut g = QUEUES.lock();
            let q = g.entry(id).or_default();
            let next = if is_ready { q.items.pop_front() } else { None };
            if next.is_none() {
                // Not ready any more, or the queue was cleared meanwhile.
                q.busy = false;
            }
            next
        };
        if let Some(item) = item {
            sync_queued(&hub, id);
            if let Err(e) = deliver(&hub, id, item).await {
                hub.notice(
                    "error",
                    format!("Could not deliver a queued prompt to session {id}: {e:#}"),
                );
            }
            finish(&hub, id);
        }
    });
}

fn drop_queue(hub: &Hub, id: SessionId) {
    let had = QUEUES
        .lock()
        .remove(&id)
        .is_some_and(|q| !q.items.is_empty());
    if had {
        hub.set_queued(id, 0);
    }
}

fn composer(hub: &Hub, id: SessionId, adapter: &dyn Adapter) -> Option<String> {
    hub.screen(id).and_then(|s| adapter.composer_text(&s))
}

/// First non-empty line of `text`, shortened, for spotting it in the input box.
fn probe(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(24).collect()
}

fn box_holds(composer: &str, text: &str) -> bool {
    let p = probe(text);
    (!p.is_empty() && composer.contains(&p)) || composer.contains("[Pasted")
}

/// Why typing must stop right now, if it must.
fn unsafe_to_type(
    hub: &Hub,
    id: SessionId,
    adapter: &dyn Adapter,
    pid: Option<u32>,
) -> Option<String> {
    let Some(info) = hub.session(id) else {
        return Some("the session was removed".into());
    };
    if info.state == AgentState::Exited || info.pid != pid {
        return Some("the process exited or was restarted".into());
    }
    let screen = hub.screen(id)?;
    // A dialog (permission, question, folder trust) took over: keys would answer it.
    match adapter
        .detect_from_screen(&screen, info.state)
        .map(|u| u.state)
    {
        Some(AgentState::NeedsApproval | AgentState::AskingQuestion) => {
            Some("a dialog is open in the terminal".into())
        }
        _ => None,
    }
}

/// Type and submit one prompt. Ok(true) = accepted; Ok(false) = put back in the queue.
async fn deliver(hub: &Arc<Hub>, id: SessionId, mut item: Item) -> anyhow::Result<bool> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    let adapter = adapter_for(info.kind);
    let pid = info.pid;
    let requeue = |item: Item| {
        QUEUES.lock().entry(id).or_default().items.push_front(item);
        Ok(false)
    };
    if let Some(why) = unsafe_to_type(hub, id, adapter, pid) {
        tracing::debug!(session = id, "not typing: {why}");
        return requeue(item);
    }

    // 1. Leftover text in the input box. Clear it if muster put it there (Claude restores an
    //    interrupted prompt) or on interrupt-mode sends; otherwise it's the human's draft:
    //    wait rather than append to it and submit both.
    if let Some(current) = composer(hub, id, adapter).filter(|c| !c.trim().is_empty()) {
        let ours = QUEUES
            .lock()
            .get(&id)
            .and_then(|q| q.last_sent.clone())
            .is_some_and(|last| box_holds(&current, &last));
        match adapter.clear_input_bytes() {
            Some(clear) if ours || item.clear_first => {
                hub.write_input(id, &clear)?;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            _ => {
                if !item.draft_noticed {
                    item.draft_noticed = true;
                    hub.notice(
                        "info",
                        format!("Session {id} has unsent text in its input box; the queued prompt waits until it's sent or cleared."),
                    );
                }
                return requeue(item);
            }
        }
    }
    item.attempts += 1;

    // 2. Type, pause, submit.
    let bytes = adapter.prompt_bytes(&item.text);
    hub.write_input(id, &bytes)?;
    QUEUES.lock().entry(id).or_default().last_sent = Some(item.text.clone());
    let extra = Duration::from_millis((bytes.len() as u64 / 50).min(900));
    tokio::time::sleep(SUBMIT_DELAY_MIN + extra).await;
    if let Some(why) = unsafe_to_type(hub, id, adapter, pid) {
        // Something took over while we typed; don't press Enter into it.
        anyhow::bail!("prompt typed but not submitted: {why}");
    }
    let seq_before = hub.output_seq(id);
    hub.write_input(id, &adapter.submit_bytes())?;

    // 3. Verify. Submit once more only if our text demonstrably still sits in the input box
    //    (or the box is unreadable and nothing happened at all).
    match accepted(hub, id, adapter, &item.text, seq_before, VERIFY_FOR).await {
        Outcome::Accepted => return Ok(true),
        Outcome::Gone => anyhow::bail!("the session exited"),
        Outcome::Pending => {}
    }
    if unsafe_to_type(hub, id, adapter, pid).is_some() {
        return Ok(true);
    }
    tracing::debug!(session = id, "prompt not accepted yet; submitting again");
    let seq_before = hub.output_seq(id);
    hub.write_input(id, &adapter.submit_bytes())?;
    match accepted(hub, id, adapter, &item.text, seq_before, VERIFY_RETRY_FOR).await {
        Outcome::Accepted => return Ok(true),
        Outcome::Gone => anyhow::bail!("the session exited"),
        Outcome::Pending => {}
    }

    // 4. Still sitting in the box: take it back out and retry later.
    let still_there = composer(hub, id, adapter).is_some_and(|c| box_holds(&c, &item.text));
    if still_there && unsafe_to_type(hub, id, adapter, pid).is_none() {
        if let Some(clear) = adapter.clear_input_bytes() {
            hub.write_input(id, &clear)?;
        }
        if item.attempts < MAX_ATTEMPTS {
            return requeue(item);
        }
        anyhow::bail!("the agent did not accept the prompt after {MAX_ATTEMPTS} attempts");
    }
    // No readable input box and no visible reaction (e.g. a silent shell command): assume it
    // landed rather than typing it twice.
    Ok(true)
}

enum Outcome {
    Accepted,
    Pending,
    Gone,
}

async fn accepted(
    hub: &Hub,
    id: SessionId,
    adapter: &dyn Adapter,
    text: &str,
    seq_before: u64,
    within: Duration,
) -> Outcome {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let Some(info) = hub.session(id) else {
            return Outcome::Gone;
        };
        match info.state {
            AgentState::Exited => return Outcome::Gone,
            AgentState::Idle | AgentState::Starting => {}
            // Working, or the agent reacted with a prompt/question/error: it took the input.
            _ => return Outcome::Accepted,
        }
        match composer(hub, id, adapter) {
            // Readable input box: accepted once our text left it and the screen moved on.
            Some(c) => {
                if !box_holds(&c, text) && hub.output_seq(id) > seq_before {
                    return Outcome::Accepted;
                }
            }
            // Unknown input box (shells): any output after the submit counts.
            None => {
                if hub.output_seq(id) > seq_before {
                    return Outcome::Accepted;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Outcome::Pending;
        }
    }
}

pub fn approve(hub: &Arc<Hub>, id: SessionId, decision: Decision) -> anyhow::Result<()> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    let adapter = adapter_for(info.kind);
    let screen = hub
        .screen(id)
        .ok_or_else(|| anyhow::anyhow!("session {id} has no screen"))?;
    let Some(bytes) = adapter.approve_bytes(decision, &screen) else {
        if decision == Decision::ApproveAlways
            && adapter.approve_bytes(Decision::Approve, &screen).is_some()
        {
            anyhow::bail!(
                "the prompt in session {id} has no \"don't ask again\" option; approve once instead"
            );
        }
        anyhow::bail!(
            "session {id} shows no permission prompt muster can answer (state: {}); answer it in the terminal",
            info.state.as_str()
        );
    };
    hub.write_input(id, &bytes)?;
    hub.log_event(
        Some(info.channel_id),
        Some(id),
        EventKind::Approval,
        serde_json::json!({"session_id": id, "decision": decision}),
    );
    Ok(())
}

pub fn interrupt(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    hub.write_input(id, &adapter_for(info.kind).interrupt_bytes())
}

pub fn clear_queue(hub: &Arc<Hub>, id: SessionId) {
    if let Some(q) = QUEUES.lock().get_mut(&id) {
        q.items.clear();
    }
    hub.set_queued(id, 0);
}

/// Background tasks (queue draining). Called once at daemon start.
pub fn spawn_tasks(hub: Arc<Hub>) {
    let mut rx = hub.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(ServerEvent::SessionUpserted(info)) => {
                        let has_items = QUEUES.lock().get(&info.id).is_some_and(|q| !q.items.is_empty());
                        if has_items {
                            kick(hub.clone(), info.id);
                        }
                    }
                    Ok(ServerEvent::SessionRemoved { id }) => {
                        QUEUES.lock().remove(&id);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    let ids: Vec<SessionId> = QUEUES
                        .lock()
                        .iter()
                        .filter(|(_, q)| !q.items.is_empty() && !q.busy)
                        .map(|(id, _)| *id)
                        .collect();
                    for id in ids {
                        kick(hub.clone(), id);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_matching() {
        assert!(box_holds("first line\nsecond", "first line\nsecond"));
        assert!(box_holds("[Pasted text #1 +20 lines]", "anything"));
        assert!(!box_holds("", "hello"));
        assert!(!box_holds("other text", "hello"));
        assert_eq!(
            probe("\n  hello world, this is a long first line\nx"),
            "hello world, this is a l"
        );
    }
}
