//! OS notifications when a session needs the human. OWNER: agents implementer.
//!
//! Listens to `SessionUpserted` events; when a session enters a `needs_human()` state (or the
//! detail of such a state changes, e.g. a second approval), shows "#channel · session" with the
//! detail. Deduped per session: the same (state, detail) never repeats within 60s, and any
//! session notifies at most once per 5s. Notifications are shown from one worker thread so a
//! slow or missing notification daemon can't stall the runtime. Set MUSTER_NO_NOTIFY=1 to
//! disable.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError};

use tokio::sync::broadcast::error::RecvError;

use crate::hub::{Hub, ServerEvent};
use crate::model::{AgentState, SessionId, SessionInfo, now_ms};

const SAME_ALERT_MS: i64 = 60_000;
const MIN_GAP_MS: i64 = 5_000;

#[derive(Default)]
struct Seen {
    state: Option<AgentState>,
    detail: Option<String>,
    last_alert: Option<(AgentState, Option<String>, i64)>,
}

struct Note {
    summary: String,
    body: String,
}

/// Called once at daemon start.
pub fn spawn(hub: Arc<Hub>) {
    if std::env::var_os("MUSTER_NO_NOTIFY").is_some_and(|v| v != "0") {
        return;
    }
    let tx = start_worker();
    let mut rx = hub.subscribe();
    tokio::spawn(async move {
        let mut seen: HashMap<SessionId, Seen> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(ServerEvent::SessionUpserted(info)) => {
                    if let Some(note) = decide(&hub, &mut seen, &info, now_ms()) {
                        match tx.try_send(note) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                }
                Ok(ServerEvent::SessionRemoved { id }) => {
                    seen.remove(&id);
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn decide(
    hub: &Hub,
    seen: &mut HashMap<SessionId, Seen>,
    info: &SessionInfo,
    now: i64,
) -> Option<Note> {
    let s = seen.entry(info.id).or_default();
    let changed = s.state != Some(info.state) || s.detail != info.state_detail;
    let entered = s.state != Some(info.state);
    s.state = Some(info.state);
    s.detail = info.state_detail.clone();
    if !changed || !info.state.needs_human() {
        return None;
    }
    // Detail-only changes within the same state (e.g. hook vs screen wording) don't re-alert
    // unless it's a new approval/question after a while.
    if let Some((st, det, at)) = &s.last_alert {
        if *st == info.state && *det == info.state_detail && now - at < SAME_ALERT_MS {
            return None;
        }
        if now - at < MIN_GAP_MS {
            // Too soon after the last alert: forget that we saw this state so the next
            // update of this session (any upsert) re-evaluates it instead of losing it.
            s.state = None;
            s.detail = None;
            return None;
        }
        if !entered && *st == info.state && now - at < SAME_ALERT_MS {
            return None;
        }
    }
    s.last_alert = Some((info.state, info.state_detail.clone(), now));
    let channel = hub
        .channel(info.channel_id)
        .map(|c| c.name)
        .unwrap_or_else(|| "?".into());
    Some(Note {
        summary: format!("#{channel} · {}", info.name),
        body: info
            .state_detail
            .clone()
            .unwrap_or_else(|| describe(info.state).to_string()),
    })
}

fn describe(state: AgentState) -> &'static str {
    match state {
        AgentState::NeedsApproval => "Needs approval",
        AgentState::AskingQuestion => "Has a question",
        AgentState::Error => "Hit an error",
        AgentState::Stuck => "Seems stuck",
        _ => "Needs attention",
    }
}

fn start_worker() -> SyncSender<Note> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Note>(8);
    let spawned = std::thread::Builder::new()
        .name("muster-notify".into())
        .spawn(move || {
            let mut paused_until: Option<std::time::Instant> = None;
            while let Ok(n) = rx.recv() {
                if paused_until.is_some_and(|t| std::time::Instant::now() < t) {
                    continue;
                }
                let r = notify_rust::Notification::new()
                    .appname("muster")
                    .summary(&n.summary)
                    .body(&n.body)
                    .timeout(notify_rust::Timeout::Milliseconds(8000))
                    .show();
                if let Err(e) = r {
                    // No notification daemon (or no session bus): back off for a while.
                    tracing::info!("desktop notifications unavailable: {e}");
                    paused_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(300));
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start notification thread: {e}");
    }
    tx
}
