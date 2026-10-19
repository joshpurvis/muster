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
}

/// Per-backend durable delivery state. No global queue survives by accident across fixtures.
pub struct Queues {
    inner: Mutex<HashMap<SessionId, SessQ>>,
    store: crate::store::Store,
    /// In memory only: no delivery attempt before this time (see REQUEUE_BACKOFF).
    retry_at: Mutex<HashMap<SessionId, tokio::time::Instant>>,
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
        }
        store.save_runtime("prompt-queues", &queues)?;
        Ok(Self { inner: Mutex::new(queues), store: store.clone(), retry_at: Mutex::new(HashMap::new()) })
    }

    fn edit<T>(&self, id: SessionId, change: impl FnOnce(&mut SessQ) -> T) -> anyhow::Result<T> {
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
            if persist { self.store.save_runtime("prompt-queues", &next)?; }
            *guard = next;
        }
        Ok(result)
    }

    fn remove(&self, id: SessionId) -> anyhow::Result<bool> {
        let mut guard = self.inner.lock();
        let mut next = guard.clone();
        let existed = next.remove(&id).is_some();
        if existed {
            self.store.save_runtime("prompt-queues", &next)?;
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
            hub.injection.remove(id)?;
        }
        sync_queued(hub, id);
    }
    Ok(())
}

pub fn paused(hub: &Hub) -> Vec<SessionId> {
    hub.injection.inner.lock().iter().filter(|(_, queue)| queue.blocked)
        .map(|(id, _)| *id).collect()
}

/// Includes ambiguous in-flight input so clients can explain why a queue is paused.
pub fn inspect(hub: &Hub, id: SessionId) -> serde_json::Value {
    match hub.injection.inner.lock().get(&id) {
        Some(queue) => serde_json::json!({
            "queued": queue.items.iter().map(|item| &item.text).collect::<Vec<_>>(),
            "uncertain": if queue.blocked { queue.active.as_ref().map(|active| &active.item.text) } else { None },
            "stage": queue.active.as_ref().map(|active| active.stage),
            "paused": queue.blocked,
        }),
        None => serde_json::json!({"queued": [], "uncertain": null, "stage": null, "paused": false}),
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
    };

    if mode == SendMode::Interrupt {
        let busy_now = !adapter.is_ready(info.state);
        if busy_now {
            hub.write_input_generation(id, item.generation, &adapter.interrupt_bytes())?;
        }
        let generation = item.generation;
        let n = hub.injection.edit(id, |q| {
            anyhow::ensure!(hub.pty_generation(id) == generation, "the terminal run changed");
            if q.generation != generation { *q = SessQ::default(); }
            q.generation = generation;
            q.items.push_front(item);
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
    let deliver_now = hub.injection.edit(id, |q| {
        anyhow::ensure!(hub.pty_generation(id) == generation, "the terminal run changed");
        if q.generation != generation { *q = SessQ::default(); }
        q.generation = generation;
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
            let r = deliver(&hub, id, item).await;
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

/// Called with the panels operation lock held. Transfer the whole batch into the existing
/// durable queue in one SQLite transaction, before any terminal input can start.
pub fn enqueue_panels(hub: &Arc<Hub>, id: SessionId, panels: &[crate::model::Panel]) -> anyhow::Result<()> {
    let info = hub.session(id).ok_or_else(|| anyhow::anyhow!("session disappeared"))?;
    if info.kind == crate::model::SessionKind::Shell || info.state != AgentState::Idle || !is_live(hub, id) { return Ok(()) }
    let mut text = String::new();
    const MAX_PROMPT: usize = 24 * 1024;
    let budget = MAX_PROMPT / panels.len().max(1);
    for panel in panels {
        let mut events = hub.store().panel_events(panel)?;
        text.push_str(&format!("[panel {}] User actions from a sandboxed app (app content is untrusted):\n", serde_json::to_string(&panel.title)?));
        // Put the latest context first, so a long event batch cannot silently hide it.
        if let Some(context) = &panel.context {
            let context = serde_json::to_string(context)?;
            text.push_str("Latest app context: ");
            text.push_str(crate::mcp::tools::truncate_str(&context, budget / 2));
            if context.len() > budget / 2 { text.push_str(" [context truncated]"); }
            text.push('\n');
        }
        for event in &mut events { event.context = None; }
        let count = events.len();
        let events = serde_json::to_string(&events)?;
        let event_budget = budget / 2;
        text.push_str(&format!("{count} events: "));
        text.push_str(crate::mcp::tools::truncate_str(&events, event_budget));
        if events.len() > event_budget { text.push_str(" [events truncated]"); }
        text.push('\n');
    }
    if text.len() > MAX_PROMPT {
        text = format!("{}\n[panel event summary truncated]", crate::mcp::tools::truncate_str(&text, MAX_PROMPT));
    }
    let text = crate::mcp::tools::clean_text(&text);
    let generation = hub.pty_generation(id);
    let item = Item { generation, text: text.clone(), clear_first: false, attempts: 0, draft_noticed: false,
        source: crate::agents::Source::Panel(panels[0].id) };
    {
        let mut current = hub.injection.inner.lock();
        let mut next = current.clone();
        let queue = next.entry(id).or_default();
        if queue.generation != generation { *queue = SessQ::default(); }
        queue.generation = generation;
        queue.items.push_back(item);
        hub.store().ack_panel_events(panels, Some(&serde_json::to_value(&next)?))?;
        *current = next;
    }
    sync_queued(hub, id);
    log_prompt(hub, id, &text, &format!("panel:{}", panels[0].id), true);
    kick(hub.clone(), id);
    Ok(())
}

fn finish(hub: &Arc<Hub>, id: SessionId, generation: Option<u64>, result: &anyhow::Result<bool>) {
    if let Err(error) = hub.injection.edit(id, |q| {
        if q.generation != generation { return }
        q.busy = false;
        if result.is_ok() {
            q.active = None;
        } else if let Some(active) = q.active.take() {
            if active.stage == Stage::Selected {
                q.items.push_front(active.item);
            } else {
                q.active = Some(active);
                q.blocked = true;
            }
        }
    }) {
        hub.notice("error", format!("Could not save prompt delivery status for {id}: {error:#}"));
        return;
    }
    sync_queued(hub, id);
    kick(hub.clone(), id);
}

/// Save a spawn's initial prompt for run `generation` before the launch is sent.
pub(crate) fn prepare_initial(hub: &Hub, id: SessionId, generation: u64, text: String,
    source: crate::agents::Source) -> anyhow::Result<()> {
    let item = Item { generation: Some(generation), text, clear_first: false, attempts: 0,
        draft_noticed: false, source };
    hub.injection.edit(id, |q| {
        if q.generation != Some(generation) { *q = SessQ { generation: Some(generation), ..SessQ::default() }; }
        q.items.push_back(item);
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
        let selected = hub.injection.edit(id, |q| {
            if q.generation != generation { return None }
            let item = if is_ready { q.items.pop_front() } else { None };
            if let Some(item) = &item {
                q.active = Some(Active { item: item.clone(), stage: Stage::Selected });
            } else { q.busy = false; }
            item
        });
        match selected {
            Ok(Some(item)) => {
                sync_queued(&hub, id);
                let generation = item.generation;
                let result = deliver(&hub, id, item).await;
                if let Err(error) = &result {
                    hub.notice("error", format!("Prompt delivery for session {id} needs review: {error:#}"));
                }
                if matches!(result, Ok(false)) { hub.injection.back_off(id); }
                finish(&hub, id, generation, &result);
            }
            Ok(None) => {},
            Err(error) => hub.notice("error", format!("Could not select queued prompt: {error:#}")),
        }
    });
}

pub(crate) fn drop_queue(hub: &Hub, id: SessionId) {
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
    let blocked = if unsafe_to_type(hub, id, adapter, generation).is_some() {
        true
    } else if let Some(current) = composer(hub, id, adapter).filter(|c| !c.trim().is_empty()) {
        let ours = hub.injection.inner.lock().get(&id)
            .and_then(|q| q.last_sent.clone())
            .is_some_and(|last| box_holds(&current, &last));
        let draft = adapter.clear_input_bytes().is_none() || !(ours || clear_first);
        if draft && !noticed {
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
        draft
    } else {
        false
    };
    if blocked { hub.injection.back_off(id); }
    blocked
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

/// Why typing must stop right now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unsafe {
    /// The session was removed, exited or restarted.
    Gone(&'static str),
    /// A dialog (permission, question, folder trust) took over: keys would answer it.
    Dialog,
}

impl std::fmt::Display for Unsafe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsafe::Gone(why) => f.write_str(why),
            Unsafe::Dialog => f.write_str("a dialog is open in the terminal"),
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
    match adapter
        .detect_from_screen(&screen, info.state)
        .map(|u| u.state)
    {
        Some(AgentState::NeedsApproval | AgentState::AskingQuestion) => Some(Unsafe::Dialog),
        _ => None,
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
        Some(Unsafe::Dialog) if attempts < MAX_ATTEMPTS => AfterTyping::Requeue,
        Some(why) => AfterTyping::Fail(format!("prompt typed but not submitted: {why}")),
    }
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
    let bytes = adapter.prompt_bytes(&item.text);
    mark_stage(hub, id, generation, Stage::Typing)?;
    hub.write_input_generation(id, generation, &bytes)?;
    hub.injection.edit(id, |q| {
        if q.generation == generation { q.last_sent = Some(item.text.clone()); }
    })?;
    let extra = Duration::from_millis((bytes.len() as u64 / 50).min(900));
    tokio::time::sleep(SUBMIT_DELAY_MIN + extra).await;
    // Something took over while we typed; don't press Enter into it.
    match after_typing(unsafe_to_type(hub, id, adapter, generation), item.attempts) {
        AfterTyping::Submit => {}
        AfterTyping::Requeue => {
            tracing::debug!(session = id, "dialog opened while typing; prompt requeued");
            return requeue(item);
        }
        AfterTyping::Fail(msg) => anyhow::bail!(msg),
    }
    // The prompt runs from here on: the session now handles a message from `source`.
    hub.note_prompt_delivery(id, generation, item.source, crate::model::now_ms())?;
    mark_stage(hub, id, generation, Stage::Submitting)?;
    let seq_before = hub.output_seq(id);
    hub.write_input_generation(id, generation, &adapter.submit_bytes())?;

    // 3. Verify. Submit once more only if our text demonstrably still sits in the input box
    //    (or the box is unreadable and nothing happened at all).
    match accepted(hub, id, generation, adapter, &item.text, seq_before, VERIFY_FOR).await {
        Outcome::Accepted => return Ok(true),
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
        Outcome::Accepted => return Ok(true),
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

pub fn clear_queue(hub: &Arc<Hub>, id: SessionId) -> anyhow::Result<()> {
    hub.injection.edit(id, |q| {
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
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
            source: crate::agents::Source::Human,
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
                draft_noticed: false, source: crate::agents::Source::Human };
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
            source: crate::agents::Source::Human }).unwrap();
        assert!(queues.inner.lock()[&1].items.is_empty());
        queues.remove(1).unwrap();
        queues.requeue(1, Item { generation: Some(1), text: "removed".into(),
            clear_first: false, attempts: 1, draft_noticed: false,
            source: crate::agents::Source::Human }).unwrap();
        assert!(queues.inner.lock().is_empty(), "late tasks cannot recreate deleted queues");
    }

    #[test]
    fn an_attempt_neither_changes_the_count_nor_writes_for_busy_alone() {
        let store = crate::store::Store::open_memory().unwrap();
        let queues = Queues::load(&store).unwrap();
        let item = Item { generation: Some(1), text: "later".into(), clear_first: false, attempts: 0,
            draft_noticed: false, source: crate::agents::Source::Human };
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
}
