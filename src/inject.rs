//! Intelligent injection: per-session input queue and reliable prompt submission.
//! OWNER: agents implementer. Used by rpc.rs and mcp.rs through the functions below.
//!
//! Delivery: wait until the adapter says the session is ready (normally Idle), clear leftover
//! text muster itself put in the input box (e.g. Claude restores an interrupted prompt), write
//! `prompt_bytes`, wait, write `submit_bytes`, then verify the agent took it (state left Idle,
//! or the input box no longer holds the text). If not, submit once more; if the text is still
//! sitting there, clear it and requeue (at most 3 attempts), so no half-typed input remains.
//! If a dialog (permission prompt, question) opens while the text is being typed, Enter is not
//! pressed and the prompt goes back to the front of the queue; the typed text is cleared by the
//! next attempt once the dialog is gone (keys sent now would answer the dialog).
//!
//! Shells: the adapter can't read the input line, so shell deliveries start with a line clear
//! (Ctrl-E, Ctrl-U; see `agents/shell.rs`). A half-typed command of the human is discarded
//! (bash and zsh keep it in the kill ring: Ctrl-Y brings it back).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::adapters::{Adapter, adapter_for};
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

/// The prompt was accepted into the durable queue. An error after that point is not
/// evidence that no input ran; retain its attribution and accounting until review.
#[derive(Debug)]
pub struct DeliveryPending;
impl std::fmt::Display for DeliveryPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("prompt retained in the durable queue; inspect delivery status before retrying")
    }
}
impl std::error::Error for DeliveryPending {}

const SETTLE: Duration = Duration::from_millis(350);
const SUBMIT_DELAY_MIN: Duration = Duration::from_millis(120);
const VERIFY_FOR: Duration = Duration::from_millis(3000);
const VERIFY_RETRY_FOR: Duration = Duration::from_millis(1500);
const MAX_ATTEMPTS: u8 = 3;
/// After a prompt could not be typed (a human draft, a dialog, a screen that isn't ready),
/// wait this long before trying again instead of looping on our own queue updates.
const REQUEUE_BACKOFF: Duration = Duration::from_millis(1000);
/// The longest one delivery may take (typing, the checks, verification take seconds). Past it the
/// attempt is stopped and settled like a failed one, so the queue is never left marked busy.
const DELIVER_LIMIT: Duration = Duration::from_secs(120);
/// An idle session whose prompts have waited this long, untyped, is a stall: one notice when the
/// human must act (`Wait::needs_human`), else the queue is kicked again (`watch_stalls`).
const STALL_AFTER: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Item {
    generation: Option<u64>,
    text: String,
    /// Clear whatever is in the input box first (set for Interrupt-mode sends).
    clear_first: bool,
    attempts: u8,
    /// The human was told this prompt waits behind their unsent draft.
    draft_noticed: bool,
    /// Who it comes from: becomes the session's provenance when it is submitted (agents.rs).
    source: crate::agents::Source,
    /// The panel events this prompt carries. Optional and skipped when absent, so the queue
    /// format stays readable by a backend without panels (it ignores unknown fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    panel: Option<PanelBatch>,
    /// Part of a self handoff (src/self_handoff.rs). Optional and skipped when absent, like
    /// `panel`: an older backend after a rollback types it as an ordinary prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<HandoffPart>,
}

/// The two prompts of a self handoff, queued together (`enqueue_handoff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HandoffPart {
    /// `/clear`.
    Clear,
    /// The prompt naming the handoff file. While `held` it stays at the head of the queue and
    /// nothing behind it is typed: it waits for Claude's new conversation (`release_handoff`).
    Prompt { held: bool },
}

fn held(item: &Item) -> bool {
    matches!(item.handoff, Some(HandoffPart::Prompt { held: true }))
}

/// After a backend restart nothing drives a self handoff any more: one whose `/clear` was never
/// typed is dropped whole (it never clears the conversation out of turn, without its prompt);
/// one whose `/clear` was typed lets its prompt go. True when something changed.
fn settle_orphaned_handoff(queue: &mut SessQ) -> bool {
    if queue.items.iter().any(|item| item.handoff == Some(HandoffPart::Clear)) {
        queue.items.retain(|item| item.handoff.is_none());
        return true;
    }
    let mut changed = false;
    for item in queue.items.iter_mut().filter(|item| held(item)) {
        item.handoff = Some(HandoffPart::Prompt { held: false });
        changed = true;
    }
    changed
}

/// Panel events admitted to a prompt. They live here, not in `panel_events`, until the
/// prompt's fate is known: dropped before typing, they return to their panels; once typing
/// began they are never replayed (see `panel_moves`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PanelBatch {
    id: u64,
    /// The task the panels belong to: the prompt is typed only while its opener is in it.
    channel_id: crate::model::ChannelId,
    events: Vec<crate::model::PanelEvent>,
    /// Events shown only as a placeholder (too long for a prompt): kept for `panel_event`.
    #[serde(default)]
    fetch: Vec<i64>,
}

/// What happens to the panel events of a batch removed from a queue before it was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsent {
    /// Back to the panel queue (the opener exited, restarted, moved, or the task closed).
    Release,
    /// Gone (the human cleared the session's prompt queue).
    Discard,
}

fn batches(queues: &HashMap<SessionId, SessQ>) -> HashMap<u64, (&PanelBatch, bool)> {
    let mut out = HashMap::new();
    for queue in queues.values() {
        for item in &queue.items {
            if let Some(batch) = &item.panel { out.insert(batch.id, (batch, false)); }
        }
        if let Some(active) = &queue.active
            && let Some(batch) = &active.item.panel
        {
            out.insert(batch.id, (batch, active.stage != Stage::Selected));
        }
    }
    out
}

/// The panel event moves implied by a queue change, saved in the same transaction. A new
/// batch claims its events; a batch removed before typing started releases them (or
/// discards them); a batch removed after typing started (submitted, reviewed as uncertain,
/// or lost with its process) keeps only its placeholder events, for `panel_event`.
fn panel_moves(previous: &HashMap<SessionId, SessQ>, next: &HashMap<SessionId, SessQ>, unsent: Unsent) -> crate::store::PanelEventMoves {
    let (before, after) = (batches(previous), batches(next));
    let mut moves = crate::store::PanelEventMoves::default();
    for (id, (batch, _)) in &after {
        if !before.contains_key(id) { moves.claim.extend(batch.events.iter().cloned()); }
    }
    for (id, (batch, started)) in &before {
        if after.contains_key(id) { continue }
        if *started {
            moves.park.extend(batch.events.iter().filter(|e| batch.fetch.contains(&e.id)).cloned());
        } else if unsent == Unsent::Release {
            moves.release.extend(batch.events.iter().cloned());
        }
    }
    moves
}

/// The opener may still receive this batch: it is in the batch's task, which is open.
/// Callers hold the panels operation lock, which session moves also take.
fn panel_bound(hub: &Hub, id: SessionId, batch: &PanelBatch) -> bool {
    hub.session(id).is_some_and(|s| s.channel_id == batch.channel_id)
        && hub.channel(batch.channel_id).is_some_and(|c| !c.archived)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage { Selected, Typing, Submitting }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Active { item: Item, stage: Stage }

#[derive(Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessQ {
    items: VecDeque<Item>,
    #[serde(skip)]
    busy: bool,
    last_sent: Option<String>,
    active: Option<Active>,
    blocked: bool,
    generation: Option<u64>,
    /// The session is being restarted for a fresh start (`carry_over`): its exit keeps the
    /// prompts never typed, and the next run types them (`rebase`).
    #[serde(default)]
    carry: bool,
}

/// Point queue `q` at run `generation`. A queue for another run is dropped, unless it carries
/// prompts across a restart (`carry_over`): those stay, now for this run.
fn rebase(q: &mut SessQ, generation: Option<u64>) {
    if q.generation == generation {
        // Still the same run (a restart that never happened): nothing to carry.
        q.carry = false;
        return;
    }
    let mut items = if q.carry { std::mem::take(&mut q.items) } else { VecDeque::new() };
    for item in &mut items { item.generation = generation; }
    *q = SessQ { items, generation, ..SessQ::default() };
}

/// Per-backend durable delivery state. No global queue survives by accident across fixtures.
pub struct Queues {
    inner: Mutex<HashMap<SessionId, SessQ>>,
    store: crate::store::Store,
    /// In memory only: no delivery attempt before this time (see REQUEUE_BACKOFF).
    retry_at: Mutex<HashMap<SessionId, tokio::time::Instant>>,
    /// In memory only: sessions whose human was told their queue waits while a subagent's
    /// conversation is on screen. Cleared once the main conversation is back.
    view_noticed: Mutex<std::collections::HashSet<SessionId>>,
    /// In memory only: since when each idle session's prompts have waited untyped, and whether
    /// the human was told (`watch_stalls`).
    stalls: Mutex<HashMap<SessionId, (tokio::time::Instant, bool)>>,
}

impl Queues {
    pub fn in_progress(&self) -> bool { self.inner.lock().values().any(|queue| queue.busy) }

    pub fn load(store: &crate::store::Store) -> anyhow::Result<Self> {
        let mut queues: HashMap<SessionId, SessQ> = store.load_runtime("prompt-queues")?.unwrap_or_default();
        for queue in queues.values_mut() {
            if let Some(active) = queue.active.take() {
                if active.stage == Stage::Selected {
                    queue.items.push_front(active.item);
                } else {
                    queue.active = Some(active);
                    queue.blocked = true;
                }
            }
            queue.busy = false;
            if settle_orphaned_handoff(queue) {
                tracing::warn!("a self handoff was waiting when the backend stopped: its prompts were settled");
            }
        }
        store.save_runtime("prompt-queues", &queues)?;
        Ok(Self { inner: Mutex::new(queues), store: store.clone(), retry_at: Mutex::new(HashMap::new()), view_noticed: Mutex::default(), stalls: Mutex::default() })
    }

    fn edit<T>(&self, id: SessionId, change: impl FnOnce(&mut SessQ) -> T) -> anyhow::Result<T> {
        self.edit_with(id, Unsent::Release, change)
    }

    fn persist(&self, previous: &HashMap<SessionId, SessQ>, next: &HashMap<SessionId, SessQ>, unsent: Unsent) -> anyhow::Result<()> {
        self.store.save_prompt_queues(next, &panel_moves(previous, next, unsent))
    }

    fn edit_with<T>(&self, id: SessionId, unsent: Unsent, change: impl FnOnce(&mut SessQ) -> T) -> anyhow::Result<T> {
        let mut guard = self.inner.lock();
        let previous = guard.get(&id).cloned().unwrap_or_default();
        let mut queue = previous.clone();
        let result = change(&mut queue);
        if queue != previous {
            // `busy` is not persisted: flipping it alone needs no database write.
            let durable = |q: &SessQ| SessQ { busy: false, ..q.clone() };
            let persist = durable(&queue) != durable(&previous);
            let mut next = guard.clone();
            next.insert(id, queue);
            if persist { self.persist(&guard, &next, unsent)?; }
            *guard = next;
        }
        Ok(result)
    }

    fn remove(&self, id: SessionId) -> anyhow::Result<bool> {
        let mut guard = self.inner.lock();
        let mut next = guard.clone();
        let existed = next.remove(&id).is_some();
        if existed {
            self.persist(&guard, &next, Unsent::Release)?;
            *guard = next;
        }
        Ok(existed)
    }

    fn back_off(&self, id: SessionId) {
        self.retry_at.lock().insert(id, tokio::time::Instant::now() + REQUEUE_BACKOFF);
    }

    fn backing_off(&self, id: SessionId) -> bool {
        let mut retry = self.retry_at.lock();
        match retry.get(&id) {
            Some(at) if *at > tokio::time::Instant::now() => true,
            Some(_) => { retry.remove(&id); false }
            None => false,
        }
    }

    fn requeue(&self, id: SessionId, item: Item) -> anyhow::Result<()> {
        self.edit(id, |queue| {
            if queue.generation != item.generation { return }
            queue.active = None;
            queue.items.push_front(item);
        })
    }
}

/// Waiting prompts plus the one being delivered (or paused for review). Counting the
/// active one keeps the number steady while an attempt pops and requeues it.
fn queue_len(hub: &Hub, id: SessionId) -> u32 {
    hub.injection.inner.lock().get(&id).map(waiting).unwrap_or(0)
}

fn waiting(q: &SessQ) -> u32 {
    q.items.len() as u32 + u32::from(q.active.is_some())
}

fn sync_queued(hub: &Hub, id: SessionId) {
    hub.set_queued(id, queue_len(hub, id));
}

/// Called after host reconciliation and before any automatic delivery tasks start.
pub fn restore(hub: &Hub) -> anyhow::Result<()> {
    let queues = hub.injection.inner.lock().clone();
    for (id, queue) in queues {
        if !is_live(hub, id) || queue.generation != hub.pty_generation(id) {
            note_lost_panel_batch(hub, id, &queue);
            hub.injection.remove(id)?;
        } else {
            // Untyped batches an older backend queued with only notices go back to their panels.
            let _op = hub.panels.operations.lock();
            release_unbound_panel_batches(hub, id);
        }
        sync_queued(hub, id);
    }
    Ok(())
}

/// Take agent message `msg`'s prompt out of every queue where it still waits (not one being
/// typed: the submission check stops that). True when one was removed.
pub fn drop_message(hub: &Hub, msg: i64) -> anyhow::Result<bool> {
    let is = |item: &Item| matches!(item.source, crate::agents::Source::Chain(p) if p.msg_id == Some(msg));
    let sessions: Vec<SessionId> = hub.injection.inner.lock().iter()
        .filter(|(_, q)| q.items.iter().any(is)).map(|(id, _)| *id).collect();
    let mut dropped = false;
    for id in sessions {
        dropped |= hub.injection.edit_with(id, Unsent::Discard, |q| {
            let before = q.items.len();
            q.items.retain(|item| !is(item));
            q.items.len() != before
        })?;
        sync_queued(hub, id);
    }
    Ok(dropped)
}

/// Queue a self handoff for session `id` in run `generation`: `/clear`, then `prompt` held at
/// the head of the queue until `release_handoff` (src/self_handoff.rs). Refused when the run
/// changed or another handoff is queued.
pub(crate) fn enqueue_handoff(hub: &Arc<Hub>, id: SessionId, generation: Option<u64>, prompt: String,
    source: crate::agents::Source, from: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!hub.is_shutting_down(), "application backend is stopping");
    anyhow::ensure!(is_live(hub, id), "session {id} is not running");
    let item = |text: &str, part| Item { generation, text: text.into(), clear_first: false, attempts: 0,
        draft_noticed: false, source, panel: None, handoff: Some(part) };
    let current = hub.pty_generation(id);
    hub.injection.edit(id, |q| {
        anyhow::ensure!(current == generation, "the terminal run changed");
        rebase(q, generation);
        let queued = q.items.iter().chain(q.active.as_ref().map(|a| &a.item)).any(|i| i.handoff.is_some());
        anyhow::ensure!(!queued, "a fresh start is already queued");
        q.items.push_back(item("/clear", HandoffPart::Clear));
        q.items.push_back(item(&prompt, HandoffPart::Prompt { held: true }));
        Ok(())
    })??;
    sync_queued(hub, id);
    log_prompt(hub, id, "/clear", from, true);
    kick(hub.clone(), id);
    Ok(())
}

/// Let session `id`'s held handoff prompt go (Claude started the new conversation). True when
/// one was held.
pub(crate) fn release_handoff(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<bool> {
    let released = hub.injection.edit(id, |q| {
        let mut any = false;
        for item in q.items.iter_mut().filter(|i| held(i)) {
            item.handoff = Some(HandoffPart::Prompt { held: false });
            any = true;
        }
        any
    })?;
    if let Some(text) = released.then(|| handoff_prompt_text(hub, id)).flatten() {
        log_prompt(hub, id, &text, "self-handoff", true);
    }
    kick(hub.clone(), id);
    Ok(released)
}

fn handoff_prompt_text(hub: &Hub, id: SessionId) -> Option<String> {
    hub.injection.inner.lock().get(&id)?.items.iter()
        .find(|i| matches!(i.handoff, Some(HandoffPart::Prompt { .. }))).map(|i| i.text.clone())
}

/// Where session `id`'s self handoff stands in its queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HandoffQueued {
    /// `/clear` waits in the queue.
    pub clear_waiting: bool,
    /// `/clear` is being typed (or paused for review).
    pub clear_active: bool,
    /// The prompt waits or is being typed.
    pub prompt: bool,
}

pub(crate) fn handoff_queued(hub: &Hub, id: SessionId) -> HandoffQueued {
    let guard = hub.injection.inner.lock();
    let Some(q) = guard.get(&id) else { return HandoffQueued::default() };
    let active = q.active.as_ref().map(|a| &a.item);
    HandoffQueued {
        clear_waiting: q.items.iter().any(|i| i.handoff == Some(HandoffPart::Clear)),
        clear_active: active.is_some_and(|i| i.handoff == Some(HandoffPart::Clear)),
        prompt: q.items.iter().chain(active).any(|i| matches!(i.handoff, Some(HandoffPart::Prompt { .. }))),
    }
}

/// Take session `id`'s self handoff back out of its queue, as far as it still waits (a prompt
/// being typed stays). (`/clear` was taken back, its prompt was taken back).
pub(crate) fn withdraw_handoff(hub: &Hub, id: SessionId) -> anyhow::Result<(bool, bool)> {
    let removed = hub.injection.edit_with(id, Unsent::Discard, |q| {
        let clear = q.items.iter().any(|i| i.handoff == Some(HandoffPart::Clear));
        let prompt = q.items.iter().any(|i| matches!(i.handoff, Some(HandoffPart::Prompt { .. })));
        q.items.retain(|i| i.handoff.is_none());
        (clear, prompt)
    })?;
    sync_queued(hub, id);
    Ok(removed)
}

/// Is agent message `msg` still in a prompt queue (waiting, or being typed)?
pub fn holds_message(hub: &Hub, msg: i64) -> bool {
    let is = |item: &Item| matches!(item.source, crate::agents::Source::Chain(p) if p.msg_id == Some(msg));
    hub.injection.inner.lock().values()
        .any(|q| q.items.iter().any(is) || q.active.as_ref().is_some_and(|a| is(&a.item)))
}

/// Does session `id`'s prompt queue hold a prompt (waiting, or being typed) that starts with
/// `prefix`? (src/questions.rs: an answer typed into a plain session has no message id.)
pub fn holds_prompt(hub: &Hub, id: SessionId, prefix: &str) -> bool {
    hub.injection.inner.lock().get(&id).is_some_and(|q|
        q.items.iter().any(|i| i.text.starts_with(prefix)) || q.active.as_ref().is_some_and(|a| a.item.text.starts_with(prefix)))
}

/// A waiting item that was never typed into its session: not being typed, and not one typed
/// and then requeued (a dialog or a subagent's view came up before Enter), whose text may still
/// sit in the input box (`last_sent`), where Enter would send it after all.
fn untyped<'a>(q: &'a SessQ) -> impl Iterator<Item = &'a Item> + 'a {
    q.items.iter().filter(move |i| q.last_sent.as_deref() != Some(i.text.as_str()))
}

/// Is agent message `msg` waiting in a prompt queue, never typed in yet (`take_back_message`
/// could still take it out)? (src/questions.rs: the human may take an answer back until then.)
pub fn waits_message(hub: &Hub, msg: i64) -> bool {
    let is = |item: &Item| matches!(item.source, crate::agents::Source::Chain(p) if p.msg_id == Some(msg));
    hub.injection.inner.lock().values().any(|q| untyped(q).any(is))
}

/// Does session `id`'s queue hold a prompt starting with `prefix` that waits, never typed in yet
/// (`drop_prompt` could still take it out)?
pub fn waits_prompt(hub: &Hub, id: SessionId, prefix: &str) -> bool {
    hub.injection.inner.lock().get(&id).is_some_and(|q| untyped(q).any(|i| i.text.starts_with(prefix)))
}

/// Take agent message `msg`'s prompt out of the queue where it waits, only while it was never
/// typed in (`untyped`): one typed and requeued may still be in the input box. True when taken.
pub fn take_back_message(hub: &Hub, msg: i64) -> anyhow::Result<bool> {
    let is = |item: &Item| matches!(item.source, crate::agents::Source::Chain(p) if p.msg_id == Some(msg));
    take_back(hub, is)
}

/// Take the prompts that start with `prefix` out of session `id`'s queue, as `take_back_message`.
/// (src/questions.rs: an answer typed into a session that isn't a named agent has no message id.)
pub fn drop_prompt(hub: &Hub, id: SessionId, prefix: &str) -> anyhow::Result<bool> {
    if !waits_prompt(hub, id, prefix) {
        return Ok(false);
    }
    take_back(hub, |item: &Item| item.text.starts_with(prefix))
}

fn take_back(hub: &Hub, is: impl Fn(&Item) -> bool) -> anyhow::Result<bool> {
    let sessions: Vec<SessionId> = hub.injection.inner.lock().iter()
        .filter(|(_, q)| untyped(q).any(&is)).map(|(id, _)| *id).collect();
    let mut taken = false;
    for id in sessions {
        taken |= hub.injection.edit_with(id, Unsent::Discard, |q| {
            let before = q.items.len();
            let typed = q.last_sent.clone();
            q.items.retain(|item| !is(item) || typed.as_deref() == Some(item.text.as_str()));
            q.items.len() != before
        })?;
        sync_queued(hub, id);
    }
    Ok(taken)
}

/// Session `id` has no prompt waiting, being typed, or paused for review (src/sleep.rs).
pub(crate) fn queue_idle(hub: &Hub, id: SessionId) -> bool {
    hub.injection.inner.lock().get(&id).is_none_or(|q| q.items.is_empty() && q.active.is_none() && !q.busy && !q.blocked)
}

/// Run `f` only while session `id`'s queue is idle, under the queue lock: no delivery can
/// start typing in between (a delivery picks its prompt under the same lock). Never call it
/// with the queue lock held, or take it inside `f`.
pub(crate) fn while_queue_idle<T>(hub: &Hub, id: SessionId, f: impl FnOnce() -> T) -> Option<T> {
    let guard = hub.injection.inner.lock();
    guard.get(&id).is_none_or(|q| q.items.is_empty() && q.active.is_none() && !q.busy && !q.blocked).then(f)
}

/// Text sits in session `id`'s input box (a draft of the human's, as far as Muster can tell).
pub(crate) fn has_draft(hub: &Hub, id: SessionId) -> bool {
    hub.session(id).is_some_and(|s| composer(hub, id, adapter_for(s.kind)).is_some_and(|c| !c.trim().is_empty()))
}

pub fn paused(hub: &Hub) -> Vec<SessionId> {
    hub.injection.inner.lock().iter().filter(|(_, queue)| queue.blocked)
        .map(|(id, _)| *id).collect()
}

/// Includes ambiguous in-flight input so clients can explain why a queue is paused.
pub fn inspect(hub: &Hub, id: SessionId) -> serde_json::Value {
    let mut v = match hub.injection.inner.lock().get(&id) {
        Some(queue) => serde_json::json!({
            "queued": queue.items.iter().map(|item| &item.text).collect::<Vec<_>>(),
            "uncertain": if queue.blocked { queue.active.as_ref().map(|active| &active.item.text) } else { None },
            "stage": queue.active.as_ref().map(|active| active.stage),
            "paused": queue.blocked,
        }),
        None => serde_json::json!({"queued": [], "uncertain": null, "stage": null, "paused": false}),
    };
    // After the queue lock is let go: `wait_reason` takes it.
    v["waiting"] = serde_json::json!(wait_reason(hub, id).map(|w| w.text));
    v
}

/// Why session `id`'s queued prompts are not being typed now, in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub text: String,
    /// Only the human can end the wait (review a delivery, send or clear a draft, answer a
    /// dialog, switch the view back); otherwise it ends by itself.
    pub needs_human: bool,
}

/// What `wait_reason` reads from the queue, the session and its screen.
#[derive(Debug, Default, Clone)]
struct WaitFacts {
    /// Prompts waiting, or one paused for review.
    pending: bool,
    blocked: bool,
    carry: bool,
    busy: bool,
    live: bool,
    same_run: bool,
    interrupted: bool,
    head_held: bool,
    state: Option<AgentState>,
    ready: bool,
    screen: Option<Unsafe>,
    draft: bool,
}

fn wait_from(id: SessionId, f: &WaitFacts) -> Option<Wait> {
    let human = |text: String| Some(Wait { text, needs_human: true });
    let by_itself = |text: &str| Some(Wait { text: text.into(), needs_human: false });
    if !f.pending { return None }
    if f.blocked {
        return human(format!(
            "a delivery needs review: it may already be in the terminal. Check it, then run `muster queue {id} --resume`"));
    }
    if f.carry { return by_itself("it waits for the agent's restart") }
    if f.busy { return by_itself("a prompt is being typed now") }
    if !f.live { return by_itself("the session is not running") }
    if !f.same_run { return by_itself("it was queued for the session's previous run") }
    if f.interrupted {
        return human("typing is paused because some input may not have reached the terminal: check it, then choose Continue typing".into());
    }
    if f.head_held { return by_itself("a fresh start the agent asked for goes first") }
    match f.state {
        Some(AgentState::NeedsApproval) => return human("a permission prompt is open in the terminal".into()),
        Some(AgentState::AskingQuestion) => return human("a question is open in the terminal".into()),
        Some(state) if !f.ready => return Some(Wait {
            text: format!("it is typed once the agent is idle (now {})", state.as_str()), needs_human: false }),
        _ => {}
    }
    match &f.screen {
        Some(Unsafe::Dialog) => return human("a dialog is open in the terminal".into()),
        Some(Unsafe::OtherView(name)) => {
            return human(format!("the terminal shows subagent {name}: switch back to the main conversation"));
        }
        Some(Unsafe::Gone(why)) => return by_itself(why),
        None => {}
    }
    if f.draft { return human("unsent text is in the input box: send it or clear it".into()) }
    None
}

/// Why session `id`'s queued prompts wait, if they do (`muster queue`, `list_sessions`, the agents
/// list, a refused sleep, the stall notice). None: nothing waits, or the next is about to be typed.
pub fn wait_reason(hub: &Hub, id: SessionId) -> Option<Wait> {
    let generation = hub.pty_generation(id);
    let (mut f, clear_first) = {
        let guard = hub.injection.inner.lock();
        let q = guard.get(&id)?;
        (WaitFacts {
            pending: !q.items.is_empty() || q.blocked,
            blocked: q.blocked,
            carry: q.carry,
            busy: q.busy,
            same_run: q.generation == generation,
            head_held: q.items.front().is_some_and(held),
            ..WaitFacts::default()
        }, q.items.front().is_some_and(|i| i.clear_first))
    };
    if !f.pending || f.blocked || f.carry || f.busy { return wait_from(id, &f) }
    f.live = is_live(hub, id);
    if f.live && f.same_run {
        f.interrupted = hub.input_interrupted(id);
        if let Some(info) = hub.session(id) {
            let adapter = adapter_for(info.kind);
            f.state = Some(info.state);
            f.ready = adapter.is_ready(info.state);
            if f.ready && !f.interrupted && !f.head_held {
                f.screen = unsafe_to_type(hub, id, adapter, generation);
                f.draft = f.screen.is_none() && draft_in_box(hub, id, adapter, clear_first);
            }
        }
    }
    wait_from(id, &f)
}

/// The input box holds text that isn't Muster's own leftover: the human's draft, which a
/// prompt must not be appended to (a `clear_first` prompt clears the box instead).
fn draft_in_box(hub: &Hub, id: SessionId, adapter: &dyn Adapter, clear_first: bool) -> bool {
    let Some(current) = composer(hub, id, adapter).filter(|c| !c.trim().is_empty()) else { return false };
    let ours = hub.injection.inner.lock().get(&id)
        .and_then(|q| q.last_sent.clone())
        .is_some_and(|last| box_holds(&current, &last));
    adapter.clear_input_bytes().is_none() || !(ours || clear_first)
}

/// A fresh start is about to restart session `id`: keep the prompts never typed for the new run
/// (`drop_queue` on its exit, then `rebase`). Returns how many wait now.
pub(crate) fn carry_over(hub: &Hub, id: SessionId) -> anyhow::Result<usize> {
    let n = hub.injection.edit(id, |q| {
        let n = q.items.iter().filter(|i| i.panel.is_none() && i.handoff.is_none()).count()
            + usize::from(q.active.as_ref().is_some_and(|a| a.stage == Stage::Selected && a.item.panel.is_none()));
        if n > 0 { q.carry = true; }
        n
    })?;
    Ok(n)
}

/// After the restart of a fresh start: the carried prompts are now the new run's; type them once
/// it is ready (behind the start's own first prompt). On a failed restart they go (the session
/// stopped) or, if the old run never stopped, simply stay where they were.
pub(crate) fn after_restart(hub: &Arc<Hub>, id: SessionId, started: bool) {
    let generation = hub.pty_generation(id);
    let live = is_live(hub, id);
    let settled = hub.injection.edit(id, |q| {
        if !q.carry { return false }
        if live { rebase(q, generation) } else if !started { q.items.clear(); q.carry = false; }
        true
    });
    match settled {
        Ok(true) if !live && !started => {
            hub.notice("warn", format!("Session {id} did not start again; the prompts queued for it were dropped."));
            drop_queue(hub, id);
        }
        Ok(_) => {}
        Err(error) => hub.notice("error", format!("Could not keep the prompt queue across the restart: {error:#}")),
    }
    sync_queued(hub, id);
    kick(hub.clone(), id);
}

/// Called every second: an idle session whose prompts have waited untyped for `STALL_AFTER`
/// gets one notice saying why, when only the human can end the wait; otherwise its queue is
/// kicked again (and the wait logged), so it never sits silently.
fn watch_stalls(hub: &Arc<Hub>) {
    let now = tokio::time::Instant::now();
    let waiting: Vec<SessionId> = hub.injection.inner.lock().iter()
        .filter(|(_, q)| !q.items.is_empty() || q.blocked).map(|(id, _)| *id).collect();
    hub.injection.stalls.lock().retain(|id, _| waiting.contains(id));
    for id in waiting {
        // Only a session at rest: one that works will be typed into when its turn ends.
        let idle = is_live(hub, id) && hub.session(id).is_some_and(|s| s.state != AgentState::Working);
        if !idle {
            hub.injection.stalls.lock().remove(&id);
            continue;
        }
        let (since, noticed) = *hub.injection.stalls.lock().entry(id).or_insert((now, false));
        if noticed || now.duration_since(since) < STALL_AFTER { continue }
        let wait = wait_reason(hub, id);
        let n = queue_len(hub, id);
        match &wait {
            Some(w) if w.needs_human => {
                let prompts = if n == 1 { "1 queued prompt has".to_string() } else { format!("{n} queued prompts have") };
                hub.notice("warn", format!("Session {id}: {prompts} waited over {} minutes: {}.", STALL_AFTER.as_secs() / 60, w.text));
                hub.injection.stalls.lock().insert(id, (since, true));
            }
            _ => {
                tracing::warn!(session = id, waiting = n, why = wait.as_ref().map(|w| w.text.as_str()).unwrap_or("none found"),
                    "queued prompts waited untyped; kicking the queue again");
                // Again after another STALL_AFTER if they still wait.
                hub.injection.stalls.lock().insert(id, (now, false));
                kick(hub.clone(), id);
            }
        }
    }
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

/// Send a prompt. `from` is "human" or "session:<id>" (for the timeline). `source` is its
/// provenance, applied to the session when the prompt is actually submitted (so a queued
/// prompt doesn't relabel the turn that is still running).
pub async fn send(
    hub: &Arc<Hub>,
    id: SessionId,
    text: String,
    mode: SendMode,
    from: String,
    source: crate::agents::Source,
) -> anyhow::Result<SendOutcome> {
    anyhow::ensure!(!hub.is_shutting_down(), "application backend is stopping; prompt was not queued");
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    if !is_live(hub, id) {
        anyhow::bail!("session {id} is not running");
    }
    if text.trim().is_empty() {
        anyhow::bail!("empty prompt");
    }
    // The first prompt to an auto-named channel names it.
    hub.note_prompt(id, &text);
    let adapter = adapter_for(info.kind);
    let item = Item {
        generation: hub.pty_generation(id),
        text: text.clone(),
        clear_first: mode == SendMode::Interrupt,
        attempts: 0,
        draft_noticed: false,
        source,
        panel: None,
        handoff: None,
    };

    if mode == SendMode::Interrupt {
        let busy_now = !adapter.is_ready(info.state);
        if busy_now {
            hub.write_input_generation(id, item.generation, &adapter.interrupt_bytes())?;
        }
        let generation = item.generation;
        // Read before the queue lock: a start holds the hub lock while it locks the queue.
        let current = hub.pty_generation(id);
        let n = hub.injection.edit(id, |q| {
            anyhow::ensure!(current == generation, "the terminal run changed");
            rebase(q, generation);
            // Never ahead of a held self-handoff prompt: the new conversation reads it first.
            let at = q.items.iter().take_while(|i| held(i)).count();
            q.items.insert(at, item);
            Ok(q.items.len() as u32)
        })??;
        sync_queued(hub, id);
        log_prompt(hub, id, &text, &from, true);
        kick(hub.clone(), id);
        return Ok(SendOutcome {
            delivered: false,
            queued: n,
        });
    }

    // Deliver right away if nothing is ahead of us and the agent is ready.
    let generation = item.generation;
    let current = hub.pty_generation(id);
    let deliver_now = hub.injection.edit(id, |q| {
        anyhow::ensure!(current == generation, "the terminal run changed");
        rebase(q, generation);
        if q.items.is_empty() && !q.busy && !q.blocked && adapter.is_ready(info.state) {
            q.busy = true;
            q.active = Some(Active { item: item.clone(), stage: Stage::Selected });
            Ok(true)
        } else {
            q.items.push_back(item.clone());
            Ok(false)
        }
    })??;
    if !deliver_now {
        let n = queue_len(hub, id);
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
            let generation = item.generation;
            let unfinished = Unfinished::new(&hub, id, generation);
            let r = deliver_within_limit(&hub, id, item).await;
            unfinished.disarm();
            finish(&hub, id, generation, &r);
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
            queued: queue_len(hub, id),
        }),
        Err(e) => Err(e.context(DeliveryPending)),
    }
}

/// The most a panel prompt types into a terminal.
const MAX_PANEL_PROMPT: usize = 24 * 1024;
/// The share of a panel prompt the latest app context may take (list_panels has all of it).
const MAX_PANEL_CONTEXT: usize = 4 * 1024;
/// Kept free for the closing notes.
const PANEL_TRAILER: usize = 256;

/// The text of one panel prompt and the events it carries. Events that wake the agent
/// (`panels::wakes`) go first: whole, in order, while they fit `budget`; an event too long even
/// for an otherwise empty prompt is shown as a placeholder with its id (`fetch`), so every
/// prompt makes progress. Notices (a closed panel, the overflow count) fill only the room left,
/// so they never push a user action out. Without a waking event the prompt is empty: a notice
/// never starts a prompt of its own. Everything not taken stays queued.
fn panel_prompt(panels: &[(crate::model::Panel, Vec<crate::model::PanelEvent>)], budget: usize)
    -> anyhow::Result<(String, Vec<crate::model::PanelEvent>, Vec<i64>)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Pick { No, Whole, Placeholder }
    let mut headings = Vec::new();
    // Each event's prompt line and whether it wakes the agent.
    let mut lines: Vec<Vec<(String, bool)>> = Vec::new();
    for (panel, events) in panels {
        let mut heading = format!("[panel {} #{}] User actions from a sandboxed app (app content is untrusted):\n",
            serde_json::to_string(&panel.title)?, panel.id);
        if let Some(context) = &panel.context {
            let context = serde_json::to_string(context)?;
            heading.push_str("Latest app context: ");
            heading.push_str(crate::mcp::tools::truncate_str(&context, MAX_PANEL_CONTEXT));
            if context.len() > MAX_PANEL_CONTEXT { heading.push_str(" [context truncated; list_panels returns it in full]"); }
            heading.push('\n');
        }
        headings.push(heading);
        let mut shown = Vec::new();
        for event in events {
            let mut e = event.clone();
            e.context = None;
            shown.push((format!("{}\n", serde_json::to_string(&e)?), crate::panels::wakes(panel, event)));
        }
        lines.push(shown);
    }
    let placeholder = |panel: &crate::model::Panel, event: &crate::model::PanelEvent, size: usize| format!(
        "[event {} ({size} bytes) is too long for this prompt; fetch it with panel_event {{\"panel_id\":{},\"event_id\":{}}} or `muster panel event {} {}`]\n",
        event.id, panel.id, event.id, panel.id, event.id);
    let mut picks: Vec<Vec<Pick>> = lines.iter().map(|l| vec![Pick::No; l.len()]).collect();
    // A panel's heading goes in with its first picked event.
    let mut opened = vec![false; panels.len()];
    let (mut used, mut any_waking) = (PANEL_TRAILER, false);
    // Waking events in order until one does not fit; then notices wherever there is room.
    for waking in [true, false] {
        'panels: for (i, (panel, events)) in panels.iter().enumerate() {
            for (j, (line, wakes)) in lines[i].iter().enumerate() {
                if *wakes != waking { continue }
                let head = if opened[i] { 0 } else { headings[i].len() };
                if used + head + line.len() <= budget {
                    picks[i][j] = Pick::Whole;
                    used += head + line.len();
                } else if waking && !any_waking {
                    picks[i][j] = Pick::Placeholder;
                    used += head + placeholder(panel, &events[j], line.len()).len();
                } else if waking {
                    break 'panels;
                } else { continue }
                opened[i] = true;
                any_waking |= waking;
            }
        }
    }
    if !any_waking { return Ok((String::new(), Vec::new(), Vec::new())) }
    let mut text = String::new();
    let (mut taken, mut fetch, mut left, mut notices_left) = (Vec::new(), Vec::new(), 0usize, 0usize);
    for (i, (panel, events)) in panels.iter().enumerate() {
        if opened[i] { text.push_str(&headings[i]); }
        for (j, event) in events.iter().enumerate() {
            match picks[i][j] {
                Pick::Whole => text.push_str(&lines[i][j].0),
                Pick::Placeholder => {
                    text.push_str(&placeholder(panel, event, lines[i][j].0.len()));
                    fetch.push(event.id);
                }
                Pick::No => {
                    if lines[i][j].1 { left += 1 } else { notices_left += 1 }
                    continue;
                }
            }
            taken.push(event.clone());
        }
    }
    if left > 0 {
        text.push_str(&format!("[{left} more panel events are queued; they follow in the next panel prompt, or call wait_for_panel_event]\n"));
    }
    if notices_left > 0 {
        text.push_str(&format!("[{notices_left} more panel notices (such as a closed panel) are queued; they come with a later panel prompt, or call wait_for_panel_event]\n"));
    }
    Ok((crate::mcp::tools::clean_text(&text), taken, fetch))
}

/// Called with the panels operation lock held (session moves take it too), with each panel's
/// queued events. Admits the events that fit one prompt to the durable queue: the queue save
/// and their removal from `panel_events` are one SQLite transaction, before any typing.
pub fn enqueue_panels(hub: &Arc<Hub>, id: SessionId, panels: &[(crate::model::Panel, Vec<crate::model::PanelEvent>)]) -> anyhow::Result<()> {
    let info = hub.session(id).ok_or_else(|| anyhow::anyhow!("session disappeared"))?;
    if info.kind == crate::model::SessionKind::Shell || info.state != AgentState::Idle || !is_live(hub, id) { return Ok(()) }
    let Some((first, _)) = panels.first() else { return Ok(()) };
    anyhow::ensure!(panels.iter().all(|(p, _)| p.channel_id == info.channel_id && p.session_id == id),
        "panel batch does not belong to session {id}'s task");
    // A named agent's batch is a message on its way until it is queued (src/sleep.rs), and none
    // goes to one asleep or being put to sleep: its events wait for its wake (`panels::deliver_pending`).
    let _delivering = match hub.agents().by_session(id) {
        Some(a) => match crate::sleep::delivering_awake(hub, a.id, id) {
            Some(d) => Some(d),
            None => return Ok(()),
        },
        None => None,
    };
    let (text, events, fetch) = panel_prompt(panels, MAX_PANEL_PROMPT)?;
    if events.is_empty() { return Ok(()) }
    let generation = hub.pty_generation(id);
    let batch = PanelBatch { id: rand::random(), channel_id: info.channel_id, events, fetch };
    // Human-origin at hop 0, as a `Chain`: older backends can still read this queue.
    let source = crate::agents::Source::Chain(crate::agents::Prov { origin: crate::agents::Origin::Human, hop: 0, msg_id: None });
    let item = Item { generation, text: text.clone(), clear_first: false, attempts: 0, draft_noticed: false,
        source, panel: Some(batch), handoff: None };
    hub.injection.edit(id, |queue| {
        rebase(queue, generation);
        queue.items.push_back(item);
    })?;
    sync_queued(hub, id);
    log_prompt(hub, id, &text, &format!("panel:{}", first.id), true);
    kick(hub.clone(), id);
    Ok(())
}

fn finish(hub: &Arc<Hub>, id: SessionId, generation: Option<u64>, result: &anyhow::Result<bool>) {
    let ok = result.is_ok();
    let settle = move |q: &mut SessQ| {
        if q.generation != generation { return }
        q.busy = false;
        if ok {
            q.active = None;
        } else if let Some(active) = q.active.take() {
            if active.stage == Stage::Selected {
                q.items.push_front(active.item);
            } else {
                q.active = Some(active);
                q.blocked = true;
            }
        }
    };
    if let Err(error) = hub.injection.edit(id, settle) {
        // Settled in memory all the same: a queue left busy would never be typed again (the
        // saved record still shows the attempt, which a backend restart reviews).
        if let Some(q) = hub.injection.inner.lock().get_mut(&id) { settle(q); }
        hub.notice("error", format!("Could not save prompt delivery status for {id}: {error:#}"));
        return;
    }
    sync_queued(hub, id);
    kick(hub.clone(), id);
}

/// `deliver`, stopped after `DELIVER_LIMIT` (dropping it stops its typing at the next await).
async fn deliver_within_limit(hub: &Arc<Hub>, id: SessionId, item: Item) -> anyhow::Result<bool> {
    match tokio::time::timeout(DELIVER_LIMIT, deliver(hub, id, item)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("the delivery did not finish within {} s", DELIVER_LIMIT.as_secs())),
    }
}

/// A delivery task that ends without settling its attempt (a panic, the task cancelled) settles
/// it as failed on the way out, so the queue is never left busy with nothing typing.
struct Unfinished { hub: Arc<Hub>, id: SessionId, generation: Option<u64>, armed: bool }

impl Unfinished {
    fn new(hub: &Arc<Hub>, id: SessionId, generation: Option<u64>) -> Self {
        Self { hub: hub.clone(), id, generation, armed: true }
    }
    fn disarm(mut self) { self.armed = false; }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(session = self.id, "a prompt delivery stopped unexpectedly");
            finish(&self.hub, self.id, self.generation, &Err(anyhow::anyhow!("the delivery stopped unexpectedly")));
        }
    }
}

/// Save a spawn's initial prompt for run `generation` before the launch is sent.
pub(crate) fn prepare_initial(hub: &Hub, id: SessionId, generation: u64, text: String,
    source: crate::agents::Source) -> anyhow::Result<()> {
    let item = Item { generation: Some(generation), text, clear_first: false, attempts: 0,
        draft_noticed: false, source, panel: None, handoff: None };
    hub.injection.edit(id, |q| {
        // The start's own prompt goes ahead of prompts carried over from the previous run.
        let carried = q.carry;
        rebase(q, Some(generation));
        if carried { q.items.push_front(item) } else { q.items.push_back(item) }
    })
}

/// The launch carrying a prepared initial prompt was acknowledged.
pub(crate) fn kick_initial(hub: &Arc<Hub>, id: SessionId) {
    sync_queued(hub, id);
    kick(hub.clone(), id);
}

fn kick(hub: Arc<Hub>, id: SessionId) {
    if hub.is_shutting_down() { return }
    if !is_live(&hub, id) {
        // A prepared launch (its acknowledgement may still be on the way) keeps its queue.
        if !hub.may_deliver(id) { drop_queue(&hub, id); }
        return;
    }
    note_other_view(&hub, id);
    if !ready(&hub, id) || hub.injection.backing_off(id) { return }
    let generation = hub.pty_generation(id);
    let should_start = hub.injection.edit(id, |q| {
        if q.generation != generation || q.busy || q.blocked || q.items.is_empty() { return false }
        q.busy = true;
        true
    });
    match should_start {
        Ok(true) => {},
        Ok(false) => return,
        Err(error) => { hub.notice("error", format!("Could not save prompt queue: {error:#}")); return; }
    }
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        let is_ready = ready(&hub, id) && !hub.is_shutting_down() && !deferred(&hub, id, generation);
        // Session moves take this lock: a panel batch is checked against its task as it is taken.
        let selected = {
            let _op = hub.panels.operations.lock();
            let unbound = unbound_panel_batches(&hub, id);
            hub.injection.edit(id, |q| {
                if q.generation != generation { return None }
                // A panel batch whose opener left its task returns to its panels (`panel_moves`).
                q.items.retain(|item| item.panel.as_ref().is_none_or(|b| !unbound.contains(&b.id)));
                // A held handoff prompt keeps its place, and everything behind it waits.
                let item = if is_ready && !q.items.front().is_some_and(held) { q.items.pop_front() } else { None };
                if let Some(item) = &item {
                    q.active = Some(Active { item: item.clone(), stage: Stage::Selected });
                } else { q.busy = false; }
                item
            })
        };
        match selected {
            Ok(Some(item)) => {
                sync_queued(&hub, id);
                let generation = item.generation;
                let unfinished = Unfinished::new(&hub, id, generation);
                let result = deliver_within_limit(&hub, id, item).await;
                unfinished.disarm();
                if let Err(error) = &result {
                    hub.notice("error", format!("Prompt delivery for session {id} needs review: {error:#}"));
                }
                if matches!(result, Ok(false)) { hub.injection.back_off(id); }
                finish(&hub, id, generation, &result);
            }
            Ok(None) => {},
            Err(error) => {
                // Nothing was taken: the queue is free again (left busy, it would never be typed).
                if let Some(q) = hub.injection.inner.lock().get_mut(&id) && q.generation == generation { q.busy = false; }
                hub.notice("error", format!("Could not select queued prompt: {error:#}"));
            }
        }
    });
}

/// A panel prompt that was being typed or submitted when its run ended is not resent (the
/// agent may have it), like any uncertain prompt; unlike one, say so, as its events are gone.
fn note_lost_panel_batch(hub: &Hub, id: SessionId, queue: &SessQ) {
    let Some(active) = queue.active.as_ref().filter(|a| a.stage != Stage::Selected) else { return };
    let Some(batch) = &active.item.panel else { return };
    let mut panels: Vec<_> = batch.events.iter().map(|e| e.panel_id).collect();
    panels.dedup();
    let panels = panels.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(", ");
    hub.notice("warn", format!(
        "Session {id} ended while a prompt with {} user actions from panel {panels} was being delivered; it may not have arrived and is not resent.",
        batch.events.len()));
}

/// Waiting panel batches of the session that may no longer be typed: the opener left the
/// batch's task (`panel_bound`), or the batch holds only notices (`panels::wakes`), as an older
/// backend could queue (a close-only prompt waiting behind a draft across `muster reload`).
/// Removed untyped, their events go back to their panels, where a notice waits for a real
/// event's prompt or a wait. Called under the panels operation lock and outside the queue lock
/// (which a start takes while holding the hub lock), so the answer holds until the caller
/// edits the queue.
fn unbound_panel_batches(hub: &Hub, id: SessionId) -> Vec<u64> {
    let waiting: Vec<PanelBatch> = hub.injection.inner.lock().get(&id)
        .map(|q| q.items.iter().filter_map(|i| i.panel.clone()).collect()).unwrap_or_default();
    if waiting.is_empty() { return Vec::new() }
    // Unreadable panel records: judge by task only, never release a batch on a read error.
    let panels: Option<HashMap<crate::model::PanelId, crate::model::Panel>> =
        crate::panels::list(hub).ok().map(|all| all.into_iter().map(|p| (p.id, p)).collect());
    let notices_only = |b: &PanelBatch| panels.as_ref().is_some_and(|panels| !b.events.iter()
        .any(|e| panels.get(&e.panel_id).is_some_and(|p| crate::panels::wakes(p, e))));
    waiting.into_iter().filter(|b| !panel_bound(hub, id, b) || notices_only(b)).map(|b| b.id).collect()
}

/// Return the session's not-yet-typed panel batches whose task it no longer is in to their
/// panels. Called under the panels operation lock right after a session moves.
pub(crate) fn release_unbound_panel_batches(hub: &Hub, id: SessionId) {
    let unbound = unbound_panel_batches(hub, id);
    if unbound.is_empty() { return }
    match hub.injection.edit(id, |q| q.items.retain(|item| item.panel.as_ref().is_none_or(|b| !unbound.contains(&b.id)))) {
        Ok(()) => sync_queued(hub, id),
        Err(error) => hub.notice("error", format!("Could not return panel events queued for session {id}: {error:#}")),
    }
}

/// An event announced by a placeholder whose prompt is still in delivery (`panel_event` may
/// race the injector's acknowledgement). Fetching it means it need not be parked afterwards.
pub(crate) fn take_in_flight_panel_event(hub: &Hub, panel: crate::model::PanelId, event: i64) -> anyhow::Result<Option<crate::model::PanelEvent>> {
    let found = hub.injection.inner.lock().iter().find_map(|(sid, q)| {
        let batch = q.active.as_ref()?.item.panel.as_ref()?;
        let e = batch.events.iter().find(|e| e.panel_id == panel && e.id == event && batch.fetch.contains(&event))?;
        Some((*sid, e.clone()))
    });
    let Some((sid, found)) = found else { return Ok(None) };
    hub.injection.edit(sid, |q| {
        if let Some(batch) = q.active.as_mut().and_then(|a| a.item.panel.as_mut()) { batch.fetch.retain(|id| *id != event); }
    })?;
    Ok(Some(found))
}

/// Panels with events inside some prompt queue (their records must stay for a release).
pub(crate) fn queued_panel_ids(hub: &Hub) -> std::collections::BTreeSet<crate::model::PanelId> {
    batches(&hub.injection.inner.lock()).values()
        .flat_map(|(batch, _)| batch.events.iter().map(|e| e.panel_id)).collect()
}

pub(crate) fn drop_queue(hub: &Hub, id: SessionId) {
    hub.injection.view_noticed.lock().remove(&id);
    hub.injection.stalls.lock().remove(&id);
    let queue = hub.injection.inner.lock().get(&id).cloned();
    if let Some(queue) = &queue { note_lost_panel_batch(hub, id, queue); }
    if queue.is_some_and(|q| q.carry) {
        // A fresh start's restart: keep what was never typed for the next run (`rebase`). A
        // prompt being typed may have arrived, and isn't resent; panel events go back to their
        // panels and a self handoff's prompts are dropped, as on any exit.
        let kept = hub.injection.edit(id, |q| {
            if let Some(active) = q.active.take() && active.stage == Stage::Selected { q.items.push_front(active.item); }
            q.items.retain(|item| item.panel.is_none() && item.handoff.is_none());
            *q = SessQ { items: std::mem::take(&mut q.items), carry: true, ..SessQ::default() };
        });
        match kept {
            Ok(()) => hub.set_queued(id, 0),
            Err(error) => hub.notice("error", format!("Could not keep the prompt queue across the restart: {error:#}")),
        }
        return;
    }
    match hub.injection.remove(id) {
        Ok(true) => hub.set_queued(id, 0),
        Ok(false) => {},
        Err(error) => hub.notice("error", format!("Could not clear prompt queue: {error:#}")),
    }
}

/// Would the next delivery be put straight back (a dialog, an unreadable screen, the
/// human's unsent draft, or typing paused after an input interruption)? Checked before taking the prompt off the queue, so waiting costs
/// no queue writes and no queue-count updates. Tells the human once about a draft.
fn deferred(hub: &Hub, id: SessionId, generation: Option<u64>) -> bool {
    // Typing is paused after an input interruption: nothing is typed until the user has
    // checked the terminal and continues (`Hub::continue_input` kicks the queue again).
    if hub.input_interrupted(id) { return true }
    let Some(info) = hub.session(id) else { return false };
    let adapter = adapter_for(info.kind);
    let head = hub.injection.inner.lock().get(&id)
        .filter(|q| q.generation == generation)
        .and_then(|q| q.items.front().map(|item| (item.clear_first, item.draft_noticed)));
    let Some((clear_first, noticed)) = head else { return false };
    if hub.injection.inner.lock().get(&id).and_then(|q| q.items.front().map(held)).unwrap_or(false) { return true }
    let blocked = if unsafe_to_type(hub, id, adapter, generation).is_some() {
        true
    } else if draft_in_box(hub, id, adapter, clear_first) {
        if !noticed {
            let marked = hub.injection.edit(id, |q| {
                if q.generation != generation { return false }
                q.items.front_mut().map(|item| item.draft_noticed = true).is_some()
            });
            if matches!(marked, Ok(true)) {
                hub.notice(
                    "info",
                    format!("Session {id} has unsent text in its input box; the queued prompt waits until it's sent or cleared."),
                );
            }
        }
        true
    } else {
        false
    };
    if blocked { hub.injection.back_off(id); }
    blocked
}

/// Tell the human once per spell that a queued prompt waits while a subagent's conversation
/// is on screen. Checked on every kick, before readiness: the viewed subagent may be working,
/// which keeps the session from looking ready, and the prompt waits all the same.
fn note_other_view(hub: &Hub, id: SessionId) {
    let Some(info) = hub.session(id) else { return };
    let view = hub.screen(id).and_then(|s| adapter_for(info.kind).other_view(&s));
    let queued = hub.injection.inner.lock().get(&id).is_some_and(|q| !q.items.is_empty());
    match view.filter(|_| queued) {
        Some(name) => if hub.injection.view_noticed.lock().insert(id) {
            hub.notice("info", format!(
                "Session {id} is showing subagent {name}; the queued prompt waits until you switch back to the main conversation."));
        },
        None => { hub.injection.view_noticed.lock().remove(&id); }
    }
}

/// The input box's text, with the agent's own dim suggestion read as empty. Claude draws a
/// suggested next prompt in the empty box after a turn; read as the human's draft, it held
/// queued messages until the human happened to send something (4 Nov: ~45 min). It counts
/// as a suggestion only while the session is idle and the human hasn't typed into it since
/// it went idle: a real draft (even with the cursor moved back to its start) follows keys.
fn composer(hub: &Hub, id: SessionId, adapter: &dyn Adapter) -> Option<String> {
    let screen = hub.screen(id)?;
    let text = adapter.composer_text(&screen)?;
    let untouched = || hub.session(id).is_some_and(|info| {
        let now = crate::model::now_ms();
        let typed_at = hub.human_input_of(id).map(|(_, at)| now - at.elapsed().as_millis() as i64);
        untouched_since_idle(info.state, info.state_since, typed_at)
    });
    if !text.trim().is_empty() && adapter.composer_may_be_suggestion(&screen) && untouched() {
        return Some(String::new());
    }
    Some(text)
}

/// Idle, with no keystroke from the human since it went idle (`typed_at`, Unix ms). Key
/// times live in memory only, so a backend restart forgets keys typed before it.
fn untouched_since_idle(state: AgentState, state_since: i64, typed_at: Option<i64>) -> bool {
    state == AgentState::Idle && typed_at.is_none_or(|t| t < state_since)
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

/// Why typing must stop right now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unsafe {
    /// The session was removed, exited or restarted.
    Gone(&'static str),
    /// A dialog (permission, question, folder trust) took over: keys would answer it.
    Dialog,
    /// The terminal shows a subagent's conversation (Claude Code's subagent view): keys would
    /// be sent to that subagent instead of the agent itself.
    OtherView(String),
}

impl std::fmt::Display for Unsafe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsafe::Gone(why) => f.write_str(why),
            Unsafe::Dialog => f.write_str("a dialog is open in the terminal"),
            Unsafe::OtherView(name) => write!(f, "the terminal shows subagent {name}, not the main conversation"),
        }
    }
}

/// Why typing must stop right now, if it must. `generation` identifies the process the
/// delivery started with (compared instead of the pid, which can be reused).
fn unsafe_to_type(
    hub: &Hub,
    id: SessionId,
    adapter: &dyn Adapter,
    generation: Option<u64>,
) -> Option<Unsafe> {
    let Some(info) = hub.session(id) else {
        return Some(Unsafe::Gone("the session was removed"));
    };
    if info.state == AgentState::Exited || hub.pty_generation(id) != generation {
        return Some(Unsafe::Gone("the process exited or was restarted"));
    }
    let Some(screen) = hub.screen(id) else {
        return Some(Unsafe::Gone("the terminal host is unavailable"));
    };
    screen_unsafe(adapter, &screen, info.state)
}

/// What on the screen itself forbids typing: a dialog, or another conversation in view.
fn screen_unsafe(adapter: &dyn Adapter, screen: &crate::adapters::ScreenSnapshot, state: AgentState) -> Option<Unsafe> {
    match adapter.detect_from_screen(screen, state).map(|u| u.state) {
        Some(AgentState::NeedsApproval | AgentState::AskingQuestion) => Some(Unsafe::Dialog),
        _ => adapter.other_view(screen).map(Unsafe::OtherView),
    }
}

/// What to do after typing a prompt, before pressing Enter.
#[derive(Debug, PartialEq, Eq)]
enum AfterTyping {
    Submit,
    /// A dialog opened: don't submit; put the prompt back at the front of the queue. Its text
    /// stays in the input box until the next attempt clears it (it matches `last_sent`).
    Requeue,
    Fail(String),
}

fn after_typing(state: Option<Unsafe>, attempts: u8) -> AfterTyping {
    match state {
        None => AfterTyping::Submit,
        // The human opened a subagent while it was typed: the text sits in the main box,
        // which the next attempt clears (it matches `last_sent`) once main is back. Never
        // counts against the attempts: waiting for the human is not a failed delivery.
        Some(Unsafe::OtherView(_)) => AfterTyping::Requeue,
        Some(Unsafe::Dialog) if attempts < MAX_ATTEMPTS => AfterTyping::Requeue,
        Some(why) => AfterTyping::Fail(format!("prompt typed but not submitted: {why}")),
    }
}

/// What one delivery is, across its retries: its agent message, its panel batch, else its text.
fn delivery_identity(item: &Item) -> String {
    use std::hash::{Hash, Hasher};
    if let crate::agents::Source::Chain(crate::agents::Prov { msg_id: Some(m), .. }) = item.source {
        return format!("msg:{m}");
    }
    if let Some(batch) = &item.panel {
        return format!("panel:{}", batch.id);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (item.generation, &item.text).hash(&mut h);
    format!("text:{:016x}", h.finish())
}

/// Type and submit one prompt. Ok(true) = accepted; Ok(false) = put back in the queue.
async fn deliver(hub: &Arc<Hub>, id: SessionId, mut item: Item) -> anyhow::Result<bool> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    let adapter = adapter_for(info.kind);
    let generation = item.generation;
    let requeue = |item: Item| {
        hub.injection.requeue(id, item)?;
        Ok(false)
    };
    if let Some(why) = unsafe_to_type(hub, id, adapter, generation) {
        tracing::debug!(session = id, "not typing: {why}");
        return requeue(item);
    }
    // A schedule's prompt that may no longer go (paused, expired, removed, timed out while it
    // waited here) is taken back, never typed (src/schedules/ `submit_gate`).
    if let Some(no) = crate::schedules::submit_gate(hub, &item.source, false) {
        crate::schedules::not_submitted(hub, &item.source, no);
        return Ok(true);
    }
    // A self handoff's /clear whose right was taken back meanwhile is never typed.
    if item.handoff == Some(HandoffPart::Clear) && !crate::self_handoff::still_allowed(hub, id) {
        return Ok(true);
    }
    // Unattended work for an OpenRouter session past a spend cap (or with today's spend
    // unknown) is taken back too, whoever queued it (src/spend.rs).
    if let Some(why) = crate::spend::submit_gate_fresh(hub, id, &item.source).await {
        crate::spend::not_submitted(hub, &item.source, why);
        return Ok(true);
    }

    // 1. Leftover text in the input box. Clear it if muster put it there (Claude restores an
    //    interrupted prompt) or on interrupt-mode sends; otherwise it's the human's draft:
    //    wait rather than append to it and submit both.
    if let Some(current) = composer(hub, id, adapter).filter(|c| !c.trim().is_empty()) {
        let ours = hub.injection.inner
            .lock()
            .get(&id)
            .and_then(|q| q.last_sent.clone())
            .is_some_and(|last| box_holds(&current, &last));
        match adapter.clear_input_bytes() {
            Some(clear) if ours || item.clear_first => {
                mark_stage(hub, id, generation, Stage::Typing)?;
                hub.write_input_generation(id, generation, &clear)?;
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
    // A shell's encoding follows the shell this run started, not the shell preference now.
    let launched = generation.and_then(|g| hub.launched_program(id, g));
    let bytes = match launched.filter(|_| info.kind == crate::model::SessionKind::Shell) {
        Some(shell) => crate::adapters::shell::prompt_bytes_for(&shell, &item.text),
        None => adapter.prompt_bytes(&item.text),
    };
    {
        // A panel batch is typed only while its opener is in the batch's task: checked under
        // the lock session moves take, so a move cannot slip in before the keys are written.
        let _op = item.panel.as_ref().map(|_| hub.panels.operations.lock());
        if let Some(batch) = &item.panel && !panel_bound(hub, id, batch) {
            mark_stage(hub, id, generation, Stage::Selected)?;
            return Ok(false);
        }
        mark_stage(hub, id, generation, Stage::Typing)?;
        hub.write_input_generation(id, generation, &bytes)?;
    }
    hub.injection.edit(id, |q| {
        if q.generation == generation { q.last_sent = Some(item.text.clone()); }
    })?;
    let extra = Duration::from_millis((bytes.len() as u64 / 50).min(900));
    tokio::time::sleep(SUBMIT_DELAY_MIN + extra).await;
    // Something took over while we typed; don't press Enter into it.
    let why = unsafe_to_type(hub, id, adapter, generation);
    if matches!(why, Some(Unsafe::OtherView(_))) { item.attempts = item.attempts.saturating_sub(1); }
    match after_typing(why, item.attempts) {
        AfterTyping::Submit => {}
        AfterTyping::Requeue => {
            tracing::debug!(session = id, "dialog opened while typing; prompt requeued");
            return requeue(item);
        }
        AfterTyping::Fail(msg) => anyhow::bail!(msg),
    }
    let seq_before = {
        let _op = item.panel.as_ref().map(|_| hub.panels.operations.lock());
        // Checked last thing before any key below (Ctrl-C, Enter): one switch to a subagent
        // after the check above would send it there (Ctrl-C would stop that subagent). The text
        // stays in the main box and the next attempt clears it as ours. The human's own keys
        // don't pass through Muster, so a switch inside this last instant can't be excluded.
        if hub.screen(id).is_some_and(|s| adapter.other_view(&s).is_some()) {
            item.attempts = item.attempts.saturating_sub(1);
            return requeue(item);
        }
        if let Some(batch) = &item.panel && !panel_bound(hub, id, batch) {
            // Typed but not submitted: take the text back out; the events return to the panel.
            if let Some(clear) = adapter.clear_input_bytes() { hub.write_input_generation(id, generation, &clear)?; }
            mark_stage(hub, id, generation, Stage::Selected)?;
            return Ok(false);
        }
        // Checked again right before Enter (the rules may have changed while it was typed).
        if let Some(no) = crate::schedules::submit_gate(hub, &item.source, true) {
            if let Some(clear) = adapter.clear_input_bytes() { hub.write_input_generation(id, generation, &clear)?; }
            crate::schedules::not_submitted(hub, &item.source, no);
            return Ok(true);
        }
        if let Some(why) = crate::spend::submit_gate(hub, id, &item.source) {
            if let Some(clear) = adapter.clear_input_bytes() { hub.write_input_generation(id, generation, &clear)?; }
            crate::spend::not_submitted(hub, &item.source, why);
            return Ok(true);
        }
        // A self handoff's /clear whose right was taken back while it was typed: taken back out.
        if item.handoff == Some(HandoffPart::Clear) && !crate::self_handoff::still_allowed(hub, id) {
            if let Some(clear) = adapter.clear_input_bytes() { hub.write_input_generation(id, generation, &clear)?; }
            return Ok(true);
        }
        // Its turn is OpenRouter spend from now on, even before the screen shows it working.
        if let Some(a) = hub.agents().by_session(id)
            && hub.session(id).is_some_and(|s| s.provider == crate::openrouter::Provider::OpenRouter)
        {
            hub.spend.note_submitted(a.id);
        }
        // The prompt runs from here on: the session now handles a message from `source`.
        let now = crate::model::now_ms();
        hub.note_prompt_delivery(id, generation, item.source, now)?;
        // Muster's own sends (not the human's) into an agent whose cache has likely expired are
        // recorded before Enter, so a crash after it can't leave one unmarked; never held back
        // (src/cache.rs). A retry of the same delivery is recorded once.
        if let crate::agents::Source::Chain(prov) = item.source
            && let Some(idle) = hub.cache_cold(id, now)
        {
            hub.note_cold_send(id, prov, item.panel.is_some(), &delivery_identity(&item), idle);
        }
        mark_stage(hub, id, generation, Stage::Submitting)?;
        let seq_before = hub.output_seq(id);
        hub.write_input_generation(id, generation, &adapter.submit_bytes())?;
        // Enter reached the terminal: a self handoff's part counts as typed from here on.
        if let Some(part) = item.handoff {
            crate::self_handoff::submitting(hub, id, part);
        }
        seq_before
    };

    // 3. Verify. Submit once more only if our text demonstrably still sits in the input box
    //    (or the box is unreadable and nothing happened at all).
    match accepted(hub, id, generation, adapter, &item.text, seq_before, VERIFY_FOR).await {
        Outcome::Accepted => { crate::mcp::communication::accepted(hub, id, &item.text); return Ok(true); },
        Outcome::Gone => anyhow::bail!("the session exited"),
        Outcome::Pending => {}
    }
    if unsafe_to_type(hub, id, adapter, generation).is_some() {
        return Ok(true);
    }
    tracing::debug!(session = id, "prompt not accepted yet; submitting again");
    mark_stage(hub, id, generation, Stage::Submitting)?;
    let seq_before = hub.output_seq(id);
    hub.write_input_generation(id, generation, &adapter.submit_bytes())?;
    match accepted(hub, id, generation, adapter, &item.text, seq_before, VERIFY_RETRY_FOR).await {
        Outcome::Accepted => { crate::mcp::communication::accepted(hub, id, &item.text); return Ok(true); },
        Outcome::Gone => anyhow::bail!("the session exited"),
        Outcome::Pending => {}
    }

    // 4. Still sitting in the box: take it back out and retry later.
    let still_there = composer(hub, id, adapter).is_some_and(|c| box_holds(&c, &item.text));
    if still_there && unsafe_to_type(hub, id, adapter, generation).is_none() {
        if let Some(clear) = adapter.clear_input_bytes() {
            mark_stage(hub, id, generation, Stage::Typing)?;
            hub.write_input_generation(id, generation, &clear)?;
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

fn mark_stage(hub: &Hub, id: SessionId, generation: Option<u64>, stage: Stage) -> anyhow::Result<()> {
    hub.injection.edit(id, |q| {
        if q.generation != generation { return Err(anyhow::anyhow!("the terminal run changed")) }
        let active = q.active.as_mut().ok_or_else(|| anyhow::anyhow!("the prompt delivery was cancelled"))?;
        active.stage = stage;
        Ok(())
    })?
}

enum Outcome {
    Accepted,
    Pending,
    Gone,
}

async fn accepted(
    hub: &Hub,
    id: SessionId,
    generation: Option<u64>,
    adapter: &dyn Adapter,
    text: &str,
    seq_before: u64,
    within: Duration,
) -> Outcome {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if hub.pty_generation(id) != generation { return Outcome::Gone }
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

/// Answer the permission prompt on screen. `from` is logged ("human", "device:<id>").
/// `seen` = the `(state_seq, state_since)` of the prompt the caller saw; if the session has
/// moved on since (another prompt, none, or this one already answered), refuse rather than
/// answer something the caller never saw.
pub fn approve(
    hub: &Arc<Hub>,
    id: SessionId,
    decision: Decision,
    from: &str,
    seen: (Option<u64>, Option<i64>),
) -> anyhow::Result<()> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    let adapter = adapter_for(info.kind);
    let generation = hub.pty_generation(id);
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
    // Claim only once the keys are chosen (a prompt muster can't answer isn't consumed), then
    // make sure the screen they were chosen from is still the one on display: the detector
    // lags the screen, so a new prompt can be up before `state_seq` moves.
    hub.claim_prompt(id, seen.0, seen.1)?;
    if seen.0.is_some() && hub.screen(id).is_none_or(|now| now.lines != screen.lines) {
        anyhow::bail!("stale: session {id}'s screen changed while answering; check it again");
    }
    hub.write_input_generation(id, generation, &bytes)?;
    hub.log_event(
        Some(info.channel_id),
        Some(id),
        EventKind::Approval,
        serde_json::json!({"session_id": id, "decision": decision, "from": from}),
    );
    Ok(())
}

pub fn interrupt(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    let info = hub
        .session(id)
        .ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    hub.write_input(id, &adapter_for(info.kind).interrupt_bytes())
}

/// Queued panel batches are discarded with the rest: the human chose to drop them.
pub fn clear_queue(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    hub.injection.edit_with(id, Unsent::Discard, |q| {
        q.items.clear();
        if !q.busy { q.active = None; q.blocked = false; }
    })?;
    sync_queued(hub, id);
    Ok(())
}

/// The human reviewed an uncertain delivery: forget that record and resume the prompts
/// waiting behind it. Never resends the uncertain prompt or undoes typed input.
pub fn resume_queue(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    let resumed = hub.injection.edit(id, |q| {
        if !q.blocked || q.busy { return false }
        q.active = None;
        q.blocked = false;
        true
    })?;
    anyhow::ensure!(resumed, "session {id} has no paused delivery to resume");
    sync_queued(hub, id);
    kick(hub.clone(), id);
    Ok(())
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
                        let has_items = hub.injection.inner.lock().get(&info.id).is_some_and(|q| !q.items.is_empty());
                        if has_items {
                            kick(hub.clone(), info.id);
                        }
                    }
                    Ok(ServerEvent::SessionRemoved { id }) => {
                        drop_queue(&hub, id);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    let ids: Vec<SessionId> = hub.injection.inner
                        .lock()
                        .iter()
                        .filter(|(_, q)| !q.items.is_empty() && !q.busy)
                        .map(|(id, _)| *id)
                        .collect();
                    for id in ids {
                        kick(hub.clone(), id);
                    }
                    watch_stalls(&hub);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported bug: the human viewed the lead's subagent and a queued message for the lead
    /// was typed into it. Real Claude Code screens: subagent on screen (idle or working) blocks
    /// typing; the main conversation, with the same agent list, does not.
    #[test]
    fn a_subagent_on_screen_blocks_typing() {
        use crate::adapters::tui::testutil::snap;
        let claude = adapter_for(crate::model::SessionKind::Claude);
        for (fixture, state) in [
            (include_str!("adapters/fixtures/claude_subagent_view.txt"), AgentState::Idle),
            (include_str!("adapters/fixtures/claude_subagent_view.txt"), AgentState::Working),
            (include_str!("adapters/fixtures/claude_subagent_typed.txt"), AgentState::Idle),
        ] {
            assert_eq!(screen_unsafe(claude, &snap(fixture), state), Some(Unsafe::OtherView("sleeper".into())));
        }
        let main = snap(include_str!("adapters/fixtures/claude_main_with_agents.txt"));
        assert_eq!(screen_unsafe(claude, &main, AgentState::Idle), None);
    }

    /// A possible suggestion counts as an empty box only while idle and untouched since: a
    /// draft the human typed after the turn ended (cursor moved back to its start with Home)
    /// stays a draft. 4 Nov: the lead went idle at 15:01:38 with no keys after, so the queued
    /// messages should have gone; the human's 15:24 question came after the 15:22 idle.
    #[test]
    fn suggestion_only_when_untouched_since_idle() {
        let idle_at = 1_000_000;
        assert!(untouched_since_idle(AgentState::Idle, idle_at, None));
        assert!(untouched_since_idle(AgentState::Idle, idle_at, Some(idle_at - 60_000)));
        assert!(!untouched_since_idle(AgentState::Idle, idle_at, Some(idle_at + 5_000)));
        assert!(!untouched_since_idle(AgentState::Idle, idle_at, Some(idle_at)));
        assert!(!untouched_since_idle(AgentState::Working, idle_at, None));
    }

    /// A hub over an in-memory store with one Claude session (1) and no terminal host.
    fn hub() -> Arc<Hub> {
        let store = crate::store::Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.insert_channel(&crate::mcp::guard::test_channel(1)).unwrap();
        let mut info = crate::mcp::guard::test_session(0, None);
        info.kind = crate::model::SessionKind::Claude;
        store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
        Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(), muster_bin: "/bin/true".into(),
            terminal_host: None, openrouter_key: None }, store).unwrap()
    }

    fn plain(generation: u64, text: &str) -> Item {
        Item { generation: Some(generation), text: text.into(), clear_first: false, attempts: 0, draft_noticed: false,
            source: crate::agents::Source::Human, panel: None, handoff: None }
    }

    /// Issue #24: an idle agent's queue stopped for hours. Claude Code turns terminal focus
    /// reporting on, so a tab gaining or losing focus sends `ESC [ I` / `ESC [ O`; counted as the
    /// human's typing, it made Claude's dim suggestion read as a draft, and every queued prompt
    /// waited behind it. Focus and mouse reports are not typing; a key still is.
    #[test]
    fn a_focus_change_never_turns_the_suggestion_into_a_draft() {
        let hub = hub();
        let idle_since = crate::model::now_ms() - 1_000;
        let typed_at = |hub: &Hub| hub.human_input_of(1).map(|(_, at)| crate::model::now_ms() - at.elapsed().as_millis() as i64);
        for report in [&b"\x1b[O"[..], b"\x1b[I", b"\x1b[O\x1b[I", b"\x1b[<0;10;5M\x1b[<0;10;5m"] {
            hub.note_human_keys(1, report);
            assert!(untouched_since_idle(AgentState::Idle, idle_since, typed_at(&hub)), "{report:?}");
        }
        hub.note_human_keys(1, b"\x1b[Iy");
        assert!(!untouched_since_idle(AgentState::Idle, idle_since, typed_at(&hub)), "a key after the turn is a draft");
    }

    /// Each reason a queue waits, in the order they are checked, and whether only the human can
    /// end it.
    #[test]
    fn a_waiting_queue_says_why() {
        let ready = WaitFacts { pending: true, live: true, same_run: true, state: Some(AgentState::Idle), ready: true, ..WaitFacts::default() };
        let w = |f: WaitFacts| wait_from(41, &f).map(|w| (w.text, w.needs_human));
        assert_eq!(w(ready.clone()), None, "nothing in the way: it is about to be typed");
        assert_eq!(w(WaitFacts { pending: false, ..ready.clone() }), None);
        let human = |f: WaitFacts, says: &str| {
            let (text, needs_human) = w(f).unwrap();
            assert!(needs_human && text.contains(says), "{text}");
        };
        let by_itself = |f: WaitFacts, says: &str| {
            let (text, needs_human) = w(f).unwrap();
            assert!(!needs_human && text.contains(says), "{text}");
        };
        human(WaitFacts { blocked: true, busy: true, ..ready.clone() }, "`muster queue 41 --resume`");
        by_itself(WaitFacts { carry: true, live: false, ..ready.clone() }, "restart");
        by_itself(WaitFacts { busy: true, ..ready.clone() }, "being typed now");
        by_itself(WaitFacts { live: false, ..ready.clone() }, "not running");
        by_itself(WaitFacts { same_run: false, ..ready.clone() }, "previous run");
        human(WaitFacts { interrupted: true, ..ready.clone() }, "Continue typing");
        by_itself(WaitFacts { head_held: true, ..ready.clone() }, "fresh start");
        human(WaitFacts { state: Some(AgentState::NeedsApproval), ready: false, ..ready.clone() }, "permission prompt");
        human(WaitFacts { state: Some(AgentState::AskingQuestion), ready: false, ..ready.clone() }, "question");
        by_itself(WaitFacts { state: Some(AgentState::Working), ready: false, ..ready.clone() }, "once the agent is idle (now working)");
        human(WaitFacts { screen: Some(Unsafe::Dialog), ..ready.clone() }, "dialog");
        human(WaitFacts { screen: Some(Unsafe::OtherView("sleeper".into())), ..ready.clone() }, "subagent sleeper");
        human(WaitFacts { draft: true, ..ready.clone() }, "unsent text is in the input box");
    }

    /// Start fresh on a running agent: prompts never typed survive its restart, now for the new
    /// run, behind the start's own first prompt; a panel batch and a self handoff go as on any exit.
    #[test]
    fn a_fresh_start_keeps_the_prompts_never_typed() {
        let hub = hub();
        let handoff = Item { handoff: Some(HandoffPart::Clear), ..plain(5, "/clear") };
        hub.injection.edit(1, |q| {
            q.generation = Some(5);
            q.items.extend([plain(5, "answer Q3: a"), handoff, plain(5, "msg 12")]);
            q.active = Some(Active { item: plain(5, "msg 11"), stage: Stage::Selected });
        }).unwrap();
        assert_eq!(carry_over(&hub, 1).unwrap(), 3);
        // The old run exits.
        drop_queue(&hub, 1);
        let q = hub.injection.inner.lock().get(&1).cloned().unwrap();
        assert!(q.carry && q.generation.is_none() && q.active.is_none() && !q.busy);
        assert_eq!(q.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["msg 11", "answer Q3: a", "msg 12"]);
        // Kept across a backend restart too (saved with the queue).
        let reloaded = Queues::load(hub.store()).unwrap();
        assert!(reloaded.inner.lock().get(&1).is_some_and(|q| q.carry && q.items.len() == 3));
        // The new run (6) starts with a prompt of its own: it goes first.
        prepare_initial(&hub, 1, 6, "read memory/ first".into(), crate::agents::Source::Human).unwrap();
        let q = hub.injection.inner.lock().get(&1).cloned().unwrap();
        assert!(!q.carry && q.generation == Some(6));
        assert_eq!(q.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["read memory/ first", "msg 11", "answer Q3: a", "msg 12"]);
        assert!(q.items.iter().all(|i| i.generation == Some(6)));
    }

    /// Without a fresh start an exit drops the queue as before; a restart that never stopped the
    /// old run leaves its queue as it was; a failed one drops what it carried, with a notice.
    #[test]
    fn carried_prompts_stay_only_across_a_restart() {
        let hub = hub();
        let fill = |hub: &Hub| hub.injection.edit(1, |q| { q.generation = Some(5); q.items.push_back(plain(5, "msg 12")); }).unwrap();
        fill(&hub);
        drop_queue(&hub, 1);
        assert!(hub.injection.inner.lock().get(&1).is_none());
        // Same run still there: carry is simply forgotten.
        fill(&hub);
        carry_over(&hub, 1).unwrap();
        hub.injection.edit(1, |q| rebase(q, Some(5))).unwrap();
        let q = hub.injection.inner.lock().get(&1).cloned().unwrap();
        assert!(!q.carry && q.items.len() == 1);
        // The restart failed and the session is down: dropped.
        carry_over(&hub, 1).unwrap();
        drop_queue(&hub, 1);
        let mut notices = hub.subscribe();
        after_restart(&hub, 1, false);
        assert!(hub.injection.inner.lock().get(&1).is_none());
        let mut said = false;
        while let Ok(event) = notices.try_recv() {
            said |= matches!(event, ServerEvent::Notice { text, .. } if text.contains("prompts queued for it were dropped"));
        }
        assert!(said);
    }

    #[test]
    fn dialog_while_typing_requeues_at_front() {
        // Decision: a dialog means "requeue" (until attempts run out), a dead process fails.
        assert_eq!(after_typing(None, 1), AfterTyping::Submit);
        assert_eq!(after_typing(Some(Unsafe::Dialog), 1), AfterTyping::Requeue);
        assert_eq!(after_typing(Some(Unsafe::Dialog), 2), AfterTyping::Requeue);
        assert!(matches!(
            after_typing(Some(Unsafe::Dialog), MAX_ATTEMPTS),
            AfterTyping::Fail(_)
        ));
        // The human opened a subagent's view mid-typing: don't press Enter into it.
        assert_eq!(after_typing(Some(Unsafe::OtherView("sleeper".into())), 1), AfterTyping::Requeue);
        assert_eq!(after_typing(Some(Unsafe::OtherView("sleeper".into())), MAX_ATTEMPTS), AfterTyping::Requeue);
        assert!(matches!(
            after_typing(Some(Unsafe::Gone("the session was removed")), 1),
            AfterTyping::Fail(m) if m.contains("removed")
        ));
        // Requeue puts it ahead of what was queued meanwhile.
        let id = 4_000_000_001;
        let item = |t: &str| Item {
            generation: Some(1),
            text: t.into(),
            clear_first: false,
            attempts: 1,
            draft_noticed: false,
            source: crate::agents::Source::Human, panel: None, handoff: None,
        };
        let store = crate::store::Store::open_memory().unwrap();
        let queues = Queues::load(&store).unwrap();
        queues.edit(id, |q| { q.generation = Some(1); q.items.push_back(item("later")); }).unwrap();
        queues.requeue(id, item("interrupted")).unwrap();
        let restored = Queues::load(&store).unwrap();
        let texts: Vec<String> = restored.inner.lock().remove(&id).unwrap()
            .items.into_iter().map(|item| item.text).collect();
        assert_eq!(texts, ["interrupted", "later"]);
        // The next attempt recognizes the leftover text as ours (so it clears it first).
        assert!(box_holds("interrupted", "interrupted"));
    }

    #[test]
    fn recovery_requeues_only_work_that_never_started_typing() {
        for stage in [Stage::Selected, Stage::Typing, Stage::Submitting] {
            let store = crate::store::Store::open_memory().unwrap();
            let queues = Queues::load(&store).unwrap();
            let item = Item { generation: Some(7), text: "run-once".into(), clear_first: false, attempts: 1,
                draft_noticed: false, source: crate::agents::Source::Human, panel: None, handoff: None };
            queues.edit(1, |q| {
                q.busy = true;
                q.generation = Some(7);
                q.active = Some(Active { item, stage });
            }).unwrap();
            drop(queues);
            let replacement = Queues::load(&store).unwrap();
            let state = replacement.inner.lock();
            let q = &state[&1];
            assert!(!q.busy);
            assert_eq!(q.generation, Some(7));
            if stage == Stage::Selected {
                assert_eq!(q.items[0].text, "run-once");
                assert!(q.active.is_none() && !q.blocked);
            } else {
                assert!(q.items.is_empty(), "ambiguous input must not be automatically replayed");
                assert!(q.blocked);
                assert_eq!(q.active.as_ref().unwrap().item.text, "run-once");
            }
        }
    }

    #[test]
    fn failed_queue_write_does_not_accept_work_in_memory() {
        let store = crate::store::Store::open_memory().unwrap();
        let queues = Queues::load(&store).unwrap();
        store.fail_runtime_writes_for_test();
        assert!(queues.edit(1, |q| q.last_sent = Some("must-not-be-accepted".into())).is_err());
        assert!(queues.inner.lock().is_empty());
    }

    #[test]
    fn obsolete_delivery_cannot_requeue_into_replacement_run() {
        let store = crate::store::Store::open_memory().unwrap();
        let queues = Queues::load(&store).unwrap();
        queues.edit(1, |q| q.generation = Some(2)).unwrap();
        queues.requeue(1, Item { generation: Some(1), text: "obsolete".into(),
            clear_first: false, attempts: 1, draft_noticed: false,
            source: crate::agents::Source::Human, panel: None, handoff: None }).unwrap();
        assert!(queues.inner.lock()[&1].items.is_empty());
        queues.remove(1).unwrap();
        queues.requeue(1, Item { generation: Some(1), text: "removed".into(),
            clear_first: false, attempts: 1, draft_noticed: false,
            source: crate::agents::Source::Human, panel: None, handoff: None }).unwrap();
        assert!(queues.inner.lock().is_empty(), "late tasks cannot recreate deleted queues");
    }

    #[test]
    fn an_attempt_neither_changes_the_count_nor_writes_for_busy_alone() {
        let store = crate::store::Store::open_memory().unwrap();
        let queues = Queues::load(&store).unwrap();
        let item = Item { generation: Some(1), text: "later".into(), clear_first: false, attempts: 0,
            draft_noticed: false, source: crate::agents::Source::Human, panel: None, handoff: None };
        queues.edit(1, |q| { q.generation = Some(1); q.items.push_back(item.clone()); }).unwrap();
        let count = || waiting(&queues.inner.lock()[&1]);
        assert_eq!(count(), 1);
        // Taking the prompt for an attempt, then putting it back (a draft or dialog), keeps
        // the count steady, so the tab's queued badge doesn't flicker.
        queues.edit(1, |q| { let item = q.items.pop_front().unwrap(); q.active = Some(Active { item, stage: Stage::Selected }); }).unwrap();
        assert_eq!(count(), 1);
        queues.requeue(1, item).unwrap();
        assert_eq!(count(), 1);
        // `busy` is in memory only: flipping it never needs the database.
        store.fail_runtime_writes_for_test();
        queues.edit(1, |q| q.busy = true).unwrap();
        queues.edit(1, |q| q.busy = false).unwrap();
        assert!(queues.edit(1, |q| q.last_sent = Some("x".into())).is_err());
    }

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

    use crate::model::{Panel, PanelEvent};
    use serde_json::json;

    fn panel_store() -> (crate::store::Store, Panel) {
        let store = crate::store::Store::open_memory().unwrap();
        let mut panel = Panel { id: 0, channel_id: 1, session_id: 1, window_id: None, title: "Review".into(), kit: false, notify_on_close: false,
            app: "markdown".into(), csp: Default::default(), revision: 1, data_revision: 1, html_revision: 1, created_at: 0,
            updated_at: 0, closed: false, context: None, dropped_events: 0 };
        store.create_panel(&mut panel, |_| Ok(())).unwrap();
        for n in 0..3 { push(&store, &mut panel, n); }
        (store, panel)
    }

    fn push(store: &crate::store::Store, panel: &mut Panel, n: i64) {
        store.push_panel_event(panel, &PanelEvent { id: 0, panel_id: panel.id, kind: "click".into(),
            payload: json!(n), at: n, context: None }).unwrap();
    }

    fn panel_item(events: Vec<PanelEvent>, fetch: Vec<i64>) -> Item {
        Item { generation: Some(1), text: "[panel]".into(), clear_first: false, attempts: 0, draft_noticed: false,
            source: crate::agents::Source::Chain(crate::agents::Prov { origin: crate::agents::Origin::Human, hop: 0, msg_id: None }),
            panel: Some(PanelBatch { id: rand::random(), channel_id: 1, events, fetch }), handoff: None }
    }

    fn ids(store: &crate::store::Store, panel: &Panel) -> Vec<i64> {
        let panel = store.load_panels().unwrap().into_iter().find(|p| p.id == panel.id).unwrap();
        store.panel_events(&panel).unwrap().iter().map(|e| e.id).collect()
    }

    #[test]
    fn panel_batches_return_to_their_panel_unless_typing_began() {
        let (store, mut panel) = panel_store();
        let queues = Queues::load(&store).unwrap();
        let events = store.panel_events(&panel).unwrap();
        assert_eq!(events.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2, 3]);
        queues.edit(1, |q| { q.generation = Some(1); q.items.push_back(panel_item(events, vec![])); }).unwrap();
        assert!(ids(&store, &panel).is_empty(), "admitted events leave the panel queue");
        push(&store, &mut panel, 3);
        // The opener exited before the prompt was typed: the batch goes back, in order.
        queues.remove(1).unwrap();
        assert_eq!(ids(&store, &panel), [1, 2, 3, 4]);
        // Typing began, then the prompt was resolved: only the placeholder event stays, parked.
        let events = store.panel_events(&panel).unwrap();
        queues.edit(1, |q| { q.generation = Some(1); q.items.push_back(panel_item(events, vec![2])); }).unwrap();
        queues.edit(1, |q| { let item = q.items.pop_front().unwrap(); q.active = Some(Active { item, stage: Stage::Typing }); }).unwrap();
        queues.edit(1, |q| q.active = None).unwrap();
        assert!(ids(&store, &panel).is_empty(), "typed events are never replayed");
        assert!(store.panel_event(panel.id, 2).unwrap().is_some_and(|(e, parked)| parked && e.payload == json!(1)));
        assert!(store.panel_event(panel.id, 1).unwrap().is_none());
        // A cleared queue discards its batches.
        push(&store, &mut panel, 5);
        let events = store.panel_events(&panel).unwrap();
        queues.edit(1, |q| q.items.push_back(panel_item(events, vec![]))).unwrap();
        queues.edit_with(1, Unsent::Discard, |q| q.items.clear()).unwrap();
        assert!(ids(&store, &panel).is_empty());
    }

    #[test]
    fn overflow_counts_follow_their_batch() {
        let (store, mut panel) = panel_store();
        panel.dropped_events = 3;
        store.save_panel(&panel).unwrap();
        let queues = Queues::load(&store).unwrap();
        let events = store.panel_events(&panel).unwrap();
        assert_eq!((events[0].id, &events[0].kind), (0, &"overflow".to_string()));
        queues.edit(1, |q| q.items.push_back(panel_item(events, vec![]))).unwrap();
        assert_eq!(store.load_panels().unwrap()[0].dropped_events, 0);
        queues.remove(1).unwrap();
        assert_eq!(store.load_panels().unwrap()[0].dropped_events, 3);
        assert_eq!(ids(&store, &panel), [0, 1, 2, 3]);
    }

    #[test]
    fn a_claim_fails_whole_when_an_event_was_taken_meanwhile() {
        let (store, panel) = panel_store();
        let queues = Queues::load(&store).unwrap();
        let events = store.panel_events(&panel).unwrap();
        store.ack_panel_events(&events[1..2]).unwrap();
        assert!(queues.edit(1, |q| q.items.push_back(panel_item(events, vec![]))).is_err());
        assert!(queues.inner.lock().is_empty());
        assert_eq!(ids(&store, &panel), [1, 3]);
    }

    /// The queue as the backend before panels (master, 2026-10-18) declares it.
    mod before_panels {
        use std::collections::VecDeque;
        use serde::Deserialize;
        #[derive(Deserialize)] pub enum Source { Human, Chain(crate::agents::Prov) }
        #[derive(Deserialize)] #[allow(dead_code)]
        pub struct Item { pub generation: Option<u64>, pub text: String, pub clear_first: bool, pub attempts: u8, pub draft_noticed: bool, pub source: Source }
        #[derive(Deserialize)] #[serde(rename_all = "snake_case")] #[allow(dead_code)] pub enum Stage { Selected, Typing, Submitting }
        #[derive(Deserialize)] #[allow(dead_code)] pub struct Active { pub item: Item, pub stage: Stage }
        #[derive(Deserialize)] #[allow(dead_code)]
        pub struct SessQ { pub items: VecDeque<Item>, #[serde(skip)] pub busy: bool, pub last_sent: Option<String>, pub active: Option<Active>, pub blocked: bool, pub generation: Option<u64> }
    }

    #[test]
    fn a_queued_panel_prompt_stays_readable_by_the_previous_backend() {
        let (store, panel) = panel_store();
        let queues = Queues::load(&store).unwrap();
        let events = store.panel_events(&panel).unwrap();
        queues.edit(1, |q| {
            q.generation = Some(1);
            q.items.push_back(panel_item(events.clone(), vec![]));
            q.active = Some(Active { item: panel_item(vec![], vec![]), stage: Stage::Typing });
        }).unwrap();
        let old: HashMap<SessionId, before_panels::SessQ> = store.load_runtime("prompt-queues").unwrap().unwrap();
        let item = &old[&1].items[0];
        assert_eq!(item.text, "[panel]");
        assert!(matches!(item.source, before_panels::Source::Chain(p) if p.origin == crate::agents::Origin::Human && p.hop == 0));
        assert!(old[&1].active.is_some());
        // And this backend reads its own format back with the batch intact.
        let reloaded = Queues::load(&store).unwrap();
        assert_eq!(reloaded.inner.lock()[&1].items[0].panel.as_ref().unwrap().events, events);
    }

    #[test]
    fn panel_prompts_carry_whole_events_and_leave_the_rest_queued() {
        let (_, panel) = panel_store();
        let comment = |id: i64, n: usize| PanelEvent { id, panel_id: panel.id, kind: "comment".into(),
            payload: json!({"text": "x".repeat(n)}), at: 0, context: None };
        // Two 8,000-character comments do not fit one 12 KiB prompt: the first goes whole.
        let (text, taken, fetch) = panel_prompt(&[(panel.clone(), vec![comment(1, 8000), comment(2, 7951)])], 12 * 1024).unwrap();
        assert_eq!(taken.iter().map(|e| e.id).collect::<Vec<_>>(), [1]);
        assert!(fetch.is_empty() && text.contains(&"x".repeat(8000)) && text.contains("1 more panel event"));
        assert!(text.starts_with("[panel \"Review\" #1]"));
        // An event too long for any prompt is shown as a fetchable placeholder.
        let (text, taken, fetch) = panel_prompt(&[(panel.clone(), vec![comment(7, 4000), comment(8, 10)])], 2048).unwrap();
        assert_eq!((taken.iter().map(|e| e.id).collect::<Vec<_>>(), fetch), (vec![7, 8], vec![7]));
        assert!(text.contains("panel_event {\"panel_id\":1,\"event_id\":7}") && !text.contains(&"x".repeat(100)));
        assert!(text.len() <= 2048);
    }

    fn hub_fixture() -> Arc<Hub> {
        let store = crate::store::Store::open_memory().unwrap();
        store.insert_workspace("test", "/tmp", false, 0).unwrap();
        store.upsert_window(1, "1", 0, 0).unwrap();
        for ch in 1..=2 {
            let mut channel = crate::mcp::guard::test_channel(ch);
            channel.window_id = Some(1);
            store.insert_channel(&channel).unwrap();
            let mut info = crate::mcp::guard::test_session(0, None);
            info.channel_id = ch;
            info.window_id = Some(1);
            store.insert_session(&crate::store::StoredSession { info, extra_args: vec![] }).unwrap();
        }
        Hub::new(crate::hub::HubConfig { port: 0, client_token: "fixture".into(),
            muster_bin: "/bin/true".into(), terminal_host: None, openrouter_key: None }, store).unwrap()
    }

    /// The human takes back a queued answer (src/questions.rs `undo`) only while it was never
    /// typed in: not while it is being typed, and not after it was typed and requeued (a dialog
    /// or a subagent's view came up before Enter), when its text may still be in the input box.
    #[test]
    fn only_a_prompt_never_typed_in_is_taken_back() {
        let hub = hub_fixture();
        let chain = |msg| crate::agents::Source::Chain(crate::agents::Prov { origin: crate::agents::Origin::Human, hop: 0, msg_id: Some(msg) });
        let item = |text: &str, source| Item { generation: None, text: text.into(), clear_first: false, attempts: 0,
            draft_noticed: false, source, panel: None, handoff: None };
        hub.injection.edit(1, |q| {
            q.active = Some(Active { item: item("[muster msg 7] typing", chain(7)), stage: Stage::Typing });
            q.items.extend([item("[muster msg 8] waiting", chain(8)), item("[muster msg 9] typed, requeued", chain(9)),
                item("[muster answer to question Q3 from human origin=human hop=0]\nAnswer", crate::agents::Source::Human)]);
            q.last_sent = Some("[muster msg 9] typed, requeued".into());
        }).unwrap();
        assert!(waits_message(&hub, 8) && !waits_message(&hub, 7) && !waits_message(&hub, 9));
        assert!(!take_back_message(&hub, 7).unwrap() && !take_back_message(&hub, 9).unwrap());
        assert!(take_back_message(&hub, 8).unwrap());
        assert!(!waits_message(&hub, 8));
        let prefix = "[muster answer to question Q3 from ";
        assert!(!drop_prompt(&hub, 2, prefix).unwrap(), "another session's queue");
        assert!(drop_prompt(&hub, 1, prefix).unwrap());
        let left: Vec<String> = hub.injection.inner.lock()[&1].items.iter().map(|i| i.text.clone()).collect();
        assert_eq!(left, ["[muster msg 9] typed, requeued"]);
    }

    /// A self handoff (src/self_handoff.rs): its held prompt keeps the head of the queue, so
    /// nothing queued after it goes first; it can be taken back; a backend restart never leaves
    /// a `/clear` to be typed out of turn.
    #[test]
    fn a_self_handoff_holds_the_queue_and_settles_after_a_restart() {
        let hub = hub_fixture();
        let item = |text: &str, handoff| Item { generation: None, text: text.into(), clear_first: false, attempts: 0,
            draft_noticed: false, source: crate::agents::Source::Human, panel: None, handoff };
        let texts = |hub: &Hub| hub.injection.inner.lock()[&1].items.iter().map(|i| i.text.clone()).collect::<Vec<_>>();
        hub.injection.edit(1, |q| {
            q.active = Some(Active { item: item("/clear", Some(HandoffPart::Clear)), stage: Stage::Typing });
            q.items.extend([item("read the file", Some(HandoffPart::Prompt { held: true })), item("later", None)]);
        }).unwrap();
        assert_eq!(handoff_queued(&hub, 1), HandoffQueued { clear_waiting: false, clear_active: true, prompt: true });
        // Held at the head: nothing is popped (`kick`) and the queue counts as busy.
        assert!(held(hub.injection.inner.lock()[&1].items.front().unwrap()));
        assert!(!queue_idle(&hub, 1));
        // Taken back: the waiting prompt goes; the /clear being typed stays.
        assert_eq!(withdraw_handoff(&hub, 1).unwrap(), (false, true));
        assert_eq!(texts(&hub), ["later"]);
        assert_eq!(withdraw_handoff(&hub, 2).unwrap(), (false, false), "another session's queue");
        // An interrupting send goes behind a held prompt, never ahead of it.
        hub.injection.edit(1, |q| {
            q.active = None;
            q.items.push_front(item("read the file", Some(HandoffPart::Prompt { held: true })));
            let at = q.items.iter().take_while(|i| held(i)).count();
            q.items.insert(at, item("urgent", None));
        }).unwrap();
        assert_eq!(texts(&hub), ["read the file", "urgent", "later"]);

        // A restart: an untyped /clear goes with its prompt; a typed one lets its prompt go.
        let mut q = SessQ::default();
        q.items.extend([item("/clear", Some(HandoffPart::Clear)), item("read", Some(HandoffPart::Prompt { held: true })),
            item("later", None)]);
        assert!(settle_orphaned_handoff(&mut q));
        assert_eq!(q.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["later"]);
        let mut q = SessQ::default();
        q.items.extend([item("read", Some(HandoffPart::Prompt { held: true })), item("later", None)]);
        assert!(settle_orphaned_handoff(&mut q));
        assert!(!held(&q.items[0]));
        assert!(!settle_orphaned_handoff(&mut SessQ::default()));
    }

    #[test]
    fn moving_the_opener_returns_its_unsent_panel_batch() {
        let hub = hub_fixture();
        let id = crate::panels::open(&hub, 1, crate::panels::Open { title: "Review".into(), app: "markdown".into(),
            data: None, html: None, html_path: None, csp: None, kit: None, notify_on_close: None }).unwrap();
        crate::panels::event(&hub, id, "approve".into(), json!({})).unwrap();
        let panel = crate::panels::get(&hub, id).unwrap();
        let events = hub.store().panel_events(&panel).unwrap();
        hub.injection.edit(1, |q| q.items.push_back(Item { generation: None, ..panel_item(events, vec![]) })).unwrap();
        assert!(ids(hub.store(), &panel).is_empty());
        hub.move_session(1, 2).unwrap();
        assert_eq!(ids(hub.store(), &panel), [1], "the batch is back in task 1's panel");
        assert!(hub.injection.inner.lock()[&1].items.is_empty());
        crate::panels::archive(&hub, 1).unwrap();
    }

    #[test]
    fn user_actions_go_first_and_notices_only_fill_the_room_left() {
        let (_, panel) = panel_store();
        let ev = |panel_id: i64, id: i64, kind: &str| PanelEvent { id, panel_id, kind: kind.into(), payload: json!({}), at: 0, context: None };
        // Older closed panels, each with a 4 KiB context, ahead of the one with a real click.
        let closed: Vec<(Panel, Vec<PanelEvent>)> = (1..=8).map(|n| (Panel { id: n, title: format!("Old {n}"),
            context: Some(json!({"c": "y".repeat(4000)})), closed: true, ..panel.clone() }, vec![ev(n, 100 + n, "closed")])).collect();
        let live = Panel { id: 20, title: "Live".into(), ..panel.clone() };
        let mut panels = closed.clone();
        panels.push((live.clone(), vec![ev(20, 1, "approve"), ev(20, 2, "comment")]));
        let budget = 12 * 1024;
        let (text, taken, fetch) = panel_prompt(&panels, budget).unwrap();
        let kinds: Vec<_> = taken.iter().map(|e| e.kind.as_str()).collect();
        assert!(fetch.is_empty() && kinds.iter().filter(|k| **k != "closed").count() == 2, "both user actions are in: {kinds:?}");
        assert!(kinds.contains(&"closed") && kinds.len() < 10, "notices fill only the room left: {kinds:?}");
        assert!(text.len() <= budget && text.contains("more panel notices") && !text.contains("more panel events are queued"));
        assert!(text.find("[panel \"Old 1\"").unwrap() < text.find("[panel \"Live\"").unwrap(), "panels keep their order");
        // Notices alone never make a prompt.
        let (text, taken, fetch) = panel_prompt(&closed, budget).unwrap();
        assert!(text.is_empty() && taken.is_empty() && fetch.is_empty());
        // With notify_on_close the close is a user action like any other.
        let asked = vec![(Panel { notify_on_close: true, ..closed[0].0.clone() }, closed[0].1.clone())];
        assert_eq!(panel_prompt(&asked, budget).unwrap().1.len(), 1);
    }

    #[test]
    fn untyped_notice_only_batches_from_an_older_backend_return_to_their_panels() {
        let hub = hub_fixture();
        let open = |title: &str| crate::panels::open(&hub, 1, crate::panels::Open { title: title.into(), app: "markdown".into(),
            data: None, html: None, html_path: None, csp: None, kit: None, notify_on_close: None }).unwrap();
        let (gone, live) = (open("Gone"), open("Live"));
        crate::panels::close(&hub, None, gone, true).unwrap();
        crate::panels::event(&hub, live, "approve".into(), json!({})).unwrap();
        let queued = |id| hub.store().panel_events(&crate::panels::get(&hub, id).unwrap()).unwrap();
        // What an older backend queued: one close-only prompt, one with a real click.
        let (notice, click) = (queued(gone), queued(live));
        hub.injection.edit(1, |q| {
            q.items.push_back(Item { generation: None, ..panel_item(notice.clone(), vec![]) });
            q.items.push_back(Item { generation: None, ..panel_item(click.clone(), vec![]) });
        }).unwrap();
        assert!(queued(gone).is_empty() && queued(live).is_empty());
        {
            let _op = hub.panels.operations.lock();
            release_unbound_panel_batches(&hub, 1);
        }
        let items = hub.injection.inner.lock()[&1].items.clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].panel.as_ref().unwrap().events, click, "the real click stays queued for typing");
        assert_eq!(queued(gone).iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["closed"], "the notice waits in its panel");
        crate::panels::archive(&hub, 1).unwrap();
    }
}
