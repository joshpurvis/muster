//! Alerts when a session needs the human: desktop notifications, phone alerts (src/push.rs),
//! and schedule alerts (`alert`). OWNER: agents implementer.
//!
//! Listens to `SessionUpserted` events; when a session enters a `needs_human()` state (or the
//! detail of such a state changes, e.g. a second approval), [`decide`] makes one [`Note`]
//! "#channel · session" with the detail and hands it to every sink. Deduped per session: the
//! same (state, detail) never repeats within 60s, and any session alerts at most once per 5s.
//! Sinks never block the loop: the desktop sink shows notifications from one worker thread (a
//! slow or missing notification daemon can't stall the runtime), the push sink sends from its
//! own tasks. MUSTER_NO_NOTIFY=1 turns off the desktop sink only; phones get alerts as long as
//! they have them on.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use tokio::sync::broadcast::error::RecvError;

use crate::hub::{Hub, ServerEvent};
use crate::model::{AgentState, ChannelId, SessionId, SessionInfo, now_ms};

const SAME_ALERT_MS: i64 = 60_000;
const MIN_GAP_MS: i64 = 5_000;

#[derive(Default)]
struct Seen {
    state: Option<AgentState>,
    detail: Option<String>,
    last_alert: Option<(AgentState, Option<String>, i64)>,
}

/// One alert, for every sink.
#[derive(Debug, Clone)]
pub struct Note {
    pub summary: String,
    pub body: String,
    /// None for alerts that aren't about a session (`alert`).
    pub session_id: Option<SessionId>,
    pub channel_id: Option<ChannelId>,
    pub state: Option<AgentState>,
    pub state_since: i64,
    pub state_seq: u64,
}

impl Note {
    fn to_alert(&self) -> crate::push::Alert {
        crate::push::Alert {
            session_id: self.session_id,
            channel_id: self.channel_id,
            state: self.state,
            state_since: self.state_since,
            state_seq: self.state_seq,
            title: self.summary.clone(),
            body: self.body.clone(),
        }
    }
}

/// Somewhere alerts go. `send` must return at once.
pub trait Sink: Send + Sync {
    fn send(&self, note: &Note);
}

/// Desktop notifications, through the worker thread (dropped while it is busy).
struct Desktop(SyncSender<Note>);

impl Sink for Desktop {
    fn send(&self, note: &Note) {
        let _ = self.0.try_send(note.clone());
    }
}

impl Sink for crate::push::Sink {
    fn send(&self, note: &Note) {
        crate::push::Sink::send(self, note.to_alert());
    }
}

/// The desktop worker, for alerts that aren't about a session's state (`alert`).
static WORKER: std::sync::OnceLock<SyncSender<Note>> = std::sync::OnceLock::new();

/// A desktop notification from elsewhere in the daemon (a schedule run's outcome). Dropped when
/// desktop notifications are off (MUSTER_NO_NOTIFY) or the worker is busy. Not sent to phones:
/// a phone alert opens a session, and these are about none.
pub fn alert(summary: &str, body: &str) {
    if let Some(tx) = WORKER.get() {
        let _ = tx.try_send(Note {
            summary: summary.to_string(),
            body: body.to_string(),
            session_id: None,
            channel_id: None,
            state: None,
            state_since: now_ms(),
            state_seq: 0,
        });
    }
}

/// Whether the desktop sink runs, from `MUSTER_NO_NOTIFY` (set and not "0": off).
fn desktop_enabled(no_notify: Option<&std::ffi::OsStr>) -> bool {
    !no_notify.is_some_and(|v| v != "0")
}

/// Called once at daemon start.
pub fn spawn(hub: Arc<Hub>) {
    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if desktop_enabled(std::env::var_os("MUSTER_NO_NOTIFY").as_deref()) {
        let tx = start_worker();
        let _ = WORKER.set(tx.clone());
        sinks.push(Box::new(Desktop(tx)));
    }
    sinks.push(Box::new(crate::push::Sink::new(hub.clone())));
    let mut rx = hub.subscribe();
    tokio::spawn(async move {
        let mut seen: HashMap<SessionId, Seen> = HashMap::new();
        let channel = |id: ChannelId| hub.channel(id).map(|c| c.name);
        loop {
            match rx.recv().await {
                Ok(ServerEvent::SessionUpserted(info)) => {
                    if let Some(note) = decide(&channel, &mut seen, &info, now_ms()) {
                        fan_out(&sinks, &note);
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

fn fan_out(sinks: &[Box<dyn Sink>], note: &Note) {
    for s in sinks {
        s.send(note);
    }
}

fn decide(
    channel: &dyn Fn(ChannelId) -> Option<String>,
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
    let channel = channel(info.channel_id).unwrap_or_else(|| "?".into());
    Some(Note {
        summary: format!("#{channel} · {}", info.name),
        body: info
            .state_detail
            .clone()
            .unwrap_or_else(|| describe(info.state).to_string()),
        session_id: Some(info.id),
        channel_id: Some(info.channel_id),
        state: Some(info.state),
        state_since: info.state_since,
        state_seq: info.state_seq,
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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    fn info(state: AgentState, detail: Option<&str>) -> SessionInfo {
        let v = serde_json::json!({
            "id": 5, "channel_id": 2, "kind": "claude", "name": "claude", "cwd": "/",
            "state": state, "state_detail": detail, "state_since": 1000, "title": null,
            "parent_id": null, "pid": null, "exit_code": null, "cols": 80, "rows": 24,
            "queued": 0, "created_at": 0, "state_seq": 3,
        });
        serde_json::from_value(v).expect("a SessionInfo")
    }

    fn chan(_: ChannelId) -> Option<String> {
        Some("web".into())
    }

    #[test]
    fn decide_alerts_on_entering_and_dedups() {
        let mut seen = HashMap::new();
        let n = decide(&chan, &mut seen, &info(AgentState::NeedsApproval, Some("Bash(ls)")), 10_000).unwrap();
        assert_eq!(n.summary, "#web · claude");
        assert_eq!(n.body, "Bash(ls)");
        assert_eq!((n.session_id, n.channel_id, n.state_since, n.state_seq), (Some(5), Some(2), 1000, 3));
        assert_eq!(n.state, Some(AgentState::NeedsApproval));
        // The same upsert again: nothing.
        assert!(decide(&chan, &mut seen, &info(AgentState::NeedsApproval, Some("Bash(ls)")), 11_000).is_none());
        // Working is not an alert; back to approval within 5 s is held back, then alerts.
        assert!(decide(&chan, &mut seen, &info(AgentState::Working, None), 12_000).is_none());
        assert!(decide(&chan, &mut seen, &info(AgentState::NeedsApproval, Some("Bash(rm)")), 13_000).is_none());
        let n = decide(&chan, &mut seen, &info(AgentState::NeedsApproval, Some("Bash(rm)")), 16_000).unwrap();
        assert_eq!(n.body, "Bash(rm)");
        // No detail: a description of the state.
        let mut seen = HashMap::new();
        assert_eq!(decide(&chan, &mut seen, &info(AgentState::AskingQuestion, None), 0).unwrap().body, "Has a question");
    }

    struct Recorder(Mutex<Vec<String>>);
    impl Sink for Arc<Recorder> {
        fn send(&self, note: &Note) {
            self.0.lock().push(note.summary.clone());
        }
    }

    #[test]
    fn every_sink_gets_each_note_and_no_notify_only_stops_the_desktop() {
        let (a, b) = (Arc::new(Recorder(Mutex::new(vec![]))), Arc::new(Recorder(Mutex::new(vec![]))));
        let sinks: Vec<Box<dyn Sink>> = vec![Box::new(a.clone()), Box::new(b.clone())];
        let mut seen = HashMap::new();
        let note = decide(&chan, &mut seen, &info(AgentState::NeedsApproval, None), 0).unwrap();
        fan_out(&sinks, &note);
        assert_eq!(*a.0.lock(), vec!["#web · claude"]);
        assert_eq!(*b.0.lock(), vec!["#web · claude"]);
        assert!(desktop_enabled(None));
        assert!(desktop_enabled(Some(std::ffi::OsStr::new("0"))));
        assert!(!desktop_enabled(Some(std::ffi::OsStr::new("1"))));
        let alert = note.to_alert();
        assert_eq!((alert.session_id, alert.title.as_str(), alert.body.as_str()), (Some(5), "#web · claude", "Needs approval"));
    }
}
