//! Screen-based state detection and stuck detection. OWNER: agents implementer.
//!
//! Every 250ms, each live session whose output changed (and every 2s regardless) gets its
//! screen parsed by the adapter's `detect_from_screen`; a differing state is applied with
//! `StateSource::Screen` (the hub keeps fresh hook states from being overridden). Details from
//! hooks are richer than the screen's, so a same-state screen result never replaces them.
//!
//! Stuck (agents only, `StateSource::Timer`): Working with no meaningful screen change for 10
//! minutes. "Meaningful" ignores digits and spinner glyphs, so animated timers and token
//! counters don't count as progress. Any meaningful change moves Stuck back to Working.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use crate::agents::{ScreenSnapshot, adapter_for, tui};
use crate::hub::Hub;
use crate::model::{
    AgentState, SessionId, SessionInfo, SessionKind, StateSource, StateUpdate, now_ms,
};

const TICK: Duration = Duration::from_millis(250);
const FORCE_EVERY_MS: i64 = 2000;
/// An agent screen that looks idle only counts once output has been quiet this long: Claude
/// draws no spinner while it streams response text, so mid-stream frames look idle.
const IDLE_SETTLE_MS: i64 = 1200;
pub const STUCK_AFTER_MS: i64 = 10 * 60 * 1000;

#[derive(Default)]
struct Track {
    seq: u64,
    last_eval: i64,
    screen_hash: u64,
    /// Last meaningful screen change (or entry into Working).
    changed_at: i64,
    prev_state: Option<AgentState>,
}

/// Called once at daemon start.
pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        let mut tracks: HashMap<SessionId, Track> = HashMap::new();
        let mut iv = tokio::time::interval(TICK);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let ids = hub.live_session_ids();
            tracks.retain(|id, _| ids.contains(id));
            for id in ids {
                let now = now_ms();
                let seq = hub.output_seq(id);
                let t = tracks.entry(id).or_insert_with(|| Track {
                    changed_at: now,
                    ..Default::default()
                });
                if seq == t.seq && now - t.last_eval < FORCE_EVERY_MS {
                    continue;
                }
                t.seq = seq;
                t.last_eval = now;
                let (Some(info), Some(screen)) = (hub.session(id), hub.screen(id)) else {
                    continue;
                };
                match evaluate(&info, &screen, t, now) {
                    Verdict::Set(update, source) => {
                        hub.set_state(id, update, source);
                    }
                    // Re-check on the next tick instead of waiting for the forced interval.
                    Verdict::Recheck => t.last_eval = 0,
                    Verdict::Keep => {}
                }
            }
        }
    });
}

fn hash_screen(screen: &ScreenSnapshot) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tui::normalized_for_change(&screen.lines).hash(&mut h);
    h.finish()
}

#[derive(Debug)]
enum Verdict {
    Set(StateUpdate, StateSource),
    /// Looks idle but output is too fresh; look again soon.
    Recheck,
    Keep,
}

/// Decide the state change (if any) for one session. Pure apart from `t`, for tests.
fn evaluate(info: &SessionInfo, screen: &ScreenSnapshot, t: &mut Track, now: i64) -> Verdict {
    match evaluate_inner(info, screen, t, now) {
        Some((u, StateSource::Screen | StateSource::Timer))
            if u.state == AgentState::Idle
                && info.kind != SessionKind::Shell
                && now - screen.last_output_at < IDLE_SETTLE_MS =>
        {
            Verdict::Recheck
        }
        Some((u, src)) => Verdict::Set(u, src),
        None => Verdict::Keep,
    }
}

fn evaluate_inner(
    info: &SessionInfo,
    screen: &ScreenSnapshot,
    t: &mut Track,
    now: i64,
) -> Option<(StateUpdate, StateSource)> {
    use AgentState::*;
    let current = info.state;
    if current == Exited {
        return None;
    }
    let h = hash_screen(screen);
    let changed = h != t.screen_hash;
    if changed {
        t.screen_hash = h;
        t.changed_at = now;
    }
    if current == Working && t.prev_state != Some(Working) {
        // Entering Working restarts the stuck clock.
        t.changed_at = t.changed_at.max(info.state_since);
    }
    t.prev_state = Some(current);

    let adapter = adapter_for(info.kind);
    let detected = adapter.detect_from_screen(screen, current);

    if current == Stuck {
        if !changed {
            return None;
        }
        // New output: leave Stuck. Prefer what the screen says, else Working.
        let next = detected
            .filter(|u| u.state != Stuck)
            .unwrap_or(StateUpdate::new(Working));
        return Some((next, StateSource::Timer));
    }

    if let Some(u) = detected
        && u.state != current
    {
        return Some((u, StateSource::Screen));
    }

    if current == Working && info.kind != SessionKind::Shell && now - t.changed_at >= STUCK_AFTER_MS
    {
        let mins = (now - t.changed_at) / 60_000;
        return Some((
            StateUpdate::with(Stuck, format!("No new output for {mins} min")),
            StateSource::Timer,
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tui::testutil::snap;

    fn info(kind: SessionKind, state: AgentState, since: i64) -> SessionInfo {
        SessionInfo {
            id: 1,
            channel_id: 1,
            kind,
            name: "x".into(),
            cwd: "/".into(),
            state,
            state_detail: None,
            state_since: since,
            title: None,
            parent_id: None,
            pid: None,
            exit_code: None,
            cols: 120,
            rows: 32,
            queued: 0,
            created_at: 0,
        }
    }

    #[test]
    fn screen_changes_state_and_same_state_keeps_detail() {
        let idle = snap(include_str!("agents/fixtures/claude_idle_done.txt"));
        let mut t = Track::default();
        let r = evaluate_inner(
            &info(SessionKind::Claude, AgentState::Starting, 0),
            &idle,
            &mut t,
            1000,
        );
        assert_eq!(r.unwrap().0.state, AgentState::Idle);
        // Already Idle (maybe with a hook detail): nothing to do.
        assert!(
            evaluate_inner(
                &info(SessionKind::Claude, AgentState::Idle, 0),
                &idle,
                &mut t,
                2000
            )
            .is_none()
        );
    }

    #[test]
    fn idle_needs_quiet_output() {
        let mut idle = snap(include_str!("agents/fixtures/claude_idle_done.txt"));
        let w = info(SessionKind::Claude, AgentState::Working, 0);
        let mut t = Track::default();
        idle.last_output_at = 10_000;
        assert!(matches!(
            evaluate(&w, &idle, &mut t, 10_500),
            Verdict::Recheck
        ));
        assert!(matches!(
            evaluate(&w, &idle, &mut t, 11_300),
            Verdict::Set(
                StateUpdate {
                    state: AgentState::Idle,
                    ..
                },
                StateSource::Screen
            )
        ));
    }

    #[test]
    fn stuck_after_quiet_working_and_back_on_change() {
        let working = snap(include_str!("agents/fixtures/claude_running_tool.txt"));
        let mut t = Track::default();
        let w = info(SessionKind::Claude, AgentState::Working, 0);
        assert!(evaluate_inner(&w, &working, &mut t, 1_000).is_none());
        // Spinner/timer animation only (digits + glyph) is not progress.
        let mut anim = working.clone();
        for l in anim.lines.iter_mut() {
            *l = l.replace("4s", "9s").replace('✶', "✻");
        }
        assert!(evaluate_inner(&w, &anim, &mut t, 60_000).is_none());
        let r = evaluate_inner(&w, &anim, &mut t, 1_000 + STUCK_AFTER_MS).unwrap();
        assert_eq!((r.0.state, r.1), (AgentState::Stuck, StateSource::Timer));
        let s = info(
            SessionKind::Claude,
            AgentState::Stuck,
            1_000 + STUCK_AFTER_MS,
        );
        assert!(evaluate_inner(&s, &anim, &mut t, 2_000 + STUCK_AFTER_MS).is_none());
        let mut more = anim.clone();
        more.lines[3] = "● new output".into();
        let r = evaluate_inner(&s, &more, &mut t, 3_000 + STUCK_AFTER_MS).unwrap();
        assert_eq!((r.0.state, r.1), (AgentState::Working, StateSource::Timer));
    }

    #[test]
    fn shells_never_stuck() {
        let mut s = snap("$ sleep 100000\n");
        s.foreground_busy = true;
        let mut t = Track::default();
        let w = info(SessionKind::Shell, AgentState::Working, 0);
        assert!(evaluate_inner(&w, &s, &mut t, 1).is_none());
        assert!(evaluate_inner(&w, &s, &mut t, STUCK_AFTER_MS * 3).is_none());
    }
}
