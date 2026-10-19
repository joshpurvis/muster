//! Screen-based state detection and stuck detection. OWNER: agents implementer.
//!
//! Every 250ms, each live session whose output changed (and every 2s regardless) gets its
//! screen parsed by the adapter's `detect_from_screen`; a differing state is applied with
//! `StateSource::Screen` (the hub keeps fresh hook states from being overridden). Details from
//! hooks are richer than the screen's, so a same-state screen result never replaces them.
//!
//! Stuck (agents only, `StateSource::Timer`): Working with no meaningful screen change for 10
//! minutes. "Meaningful" ignores digits and spinner glyphs, so animated timers and token
//! counters don't count as progress. Any meaningful change moves Stuck back to Working (or to
//! what the screen shows). A Stuck session whose screen shows another state (idle, a prompt)
//! moves there even without a further change, so an idle verdict that had to wait for the
//! output to settle is applied on a later tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adapters::{ScreenSnapshot, adapter_for, tui};
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

const CHECKPOINT: &str = "detector-progress-v1";
/// How often each session's process folder (`/proc/<pid>/cwd`) is read.
const CWD_EVERY_MS: i64 = 1000;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Track {
    generation: u64,
    #[serde(skip)]
    seq: u64,
    #[serde(skip)]
    last_eval: i64,
    #[serde(skip)]
    cwd_checked: i64,
    screen_hash: [u8; 32],
    /// Last meaningful screen change (or entry into Working).
    changed_at: i64,
    prev_state: Option<AgentState>,
}

impl Track {
    fn for_generation(&mut self, generation: u64, now: i64) {
        if self.generation != generation {
            *self = Self { generation, changed_at: now, ..Self::default() };
        }
    }

    fn progress(&self) -> (u64, [u8; 32], i64, Option<AgentState>) {
        (self.generation, self.screen_hash, self.changed_at, self.prev_state)
    }
}

/// Called once at daemon start.
pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        let mut tracks: HashMap<SessionId, Track> = match hub.store().load_runtime(CHECKPOINT) {
            Ok(saved) => saved.unwrap_or_default(),
            Err(error) => {
                tracing::warn!("restoring detector progress: {error:#}");
                HashMap::new()
            }
        };
        let mut dirty = false;
        let mut iv = tokio::time::interval(TICK);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            let ids = hub.live_session_ids();
            let previous_len = tracks.len();
            tracks.retain(|id, _| ids.contains(id));
            dirty |= previous_len != tracks.len();
            for id in ids {
                let now = now_ms();
                let Some((info, terminal)) = hub.detection_terminal(id) else { continue };
                let seq = terminal.seq();
                let t = tracks.entry(id).or_default();
                let before = t.progress();
                t.for_generation(terminal.generation, now);
                dirty |= t.progress() != before;
                if now - t.cwd_checked >= CWD_EVERY_MS {
                    t.cwd_checked = now;
                    let marker = hub.run_marker(id, terminal.generation);
                    let cwd = info.pid.zip(marker).and_then(|(pid, run)| process_cwd(pid, &run))
                        .map(|live| aliased(&live, &info.cwd));
                    if info.live_cwd != cwd {
                        hub.note_live_cwd(id, terminal.generation, cwd);
                    }
                }
                if seq == t.seq && now - t.last_eval < FORCE_EVERY_MS {
                    continue;
                }
                t.seq = seq;
                t.last_eval = now;
                let Some(screen) = terminal.screen() else { continue };
                let before = t.progress();
                match evaluate(&info, &screen, t, now) {
                    Verdict::Set(update, source) => {
                        hub.set_state_checked(id, update, source, Some(terminal.generation));
                    }
                    // Re-check on the next tick instead of waiting for the forced interval.
                    Verdict::Recheck => t.last_eval = 0,
                    Verdict::Keep => {}
                }
                dirty |= t.progress() != before;
            }
            // Poll counters are deliberately excluded. Persist only meaningful
            // progress so animation and unchanged screens do not cause disk writes.
            if dirty {
                match hub.store().save_runtime(CHECKPOINT, &tracks) {
                    Ok(()) => dirty = false,
                    Err(error) => tracing::warn!("saving detector progress: {error:#}"),
                }
            }
        }
    });
}

/// The folder process `pid` is in, if it is still the process of the run marked `run` (a
/// pid can be reused once the session's process is gone) and the folder still exists.
fn process_cwd(pid: u32, run: &str) -> Option<String> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let marker = format!("MUSTER_RUN={run}");
    if !env.split(|b| *b == 0).any(|kv| kv == marker.as_bytes()) {
        return None;
    }
    let p = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    // A deleted folder reads as "<path> (deleted)"; is_dir() is false for it.
    p.is_dir().then(|| p.to_string_lossy().into_owned())
}

/// `live` (a physical path, as /proc gives it) spelled from `launch` when it is that folder or
/// inside it, so a symlinked launch folder still compares equal to where the process is.
fn aliased(live: &str, launch: &str) -> String {
    let Ok(root) = std::fs::canonicalize(launch) else { return live.to_string() };
    match std::path::Path::new(live).strip_prefix(&root) {
        Ok(rest) if rest.as_os_str().is_empty() => launch.to_string(),
        Ok(rest) => std::path::Path::new(launch).join(rest).to_string_lossy().into_owned(),
        Err(_) => live.to_string(),
    }
}

fn hash_screen(screen: &ScreenSnapshot) -> [u8; 32] {
    // Unlike DefaultHasher, this digest is stable across backend builds.
    Sha256::digest(tui::normalized_for_change(&screen.lines).as_bytes()).into()
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
        // The screen shows something other than an agent at work (idle, a dialog): go there.
        // This must not depend on `changed`: an idle screen first waits for its output to
        // settle (`Verdict::Recheck`), and by the next tick the hash no longer changes.
        if let Some(u) = detected
            .clone()
            .filter(|u| !matches!(u.state, Stuck | Working))
        {
            return Some((u, StateSource::Timer));
        }
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
    use crate::adapters::tui::testutil::snap;

    #[test]
    fn process_cwd_checks_the_run_marker() {
        let dir = std::env::temp_dir().join(format!("muster-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").current_dir(&dir)
            .env("MUSTER_RUN", "run-a").spawn().unwrap();
        let canon = std::fs::canonicalize(&dir).unwrap().to_string_lossy().into_owned();
        // spawn() can return before the child's exec has replaced its environment.
        let mut seen = None;
        for _ in 0..100 {
            seen = process_cwd(child.id(), "run-a");
            if seen.is_some() { break }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(seen, Some(canon));
        // Same pid, another run's marker (a reused pid): not this session's folder.
        assert_eq!(process_cwd(child.id(), "run-b"), None);
        assert_eq!(process_cwd(u32::MAX, "run-a"), None);
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn live_folders_keep_the_launch_spelling() {
        let base = std::env::temp_dir().join(format!("muster-alias-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canon = std::fs::canonicalize(&real).unwrap();
        let (l, c) = (link.to_string_lossy(), canon.to_string_lossy());
        assert_eq!(aliased(&c, &l), l);
        assert_eq!(aliased(&format!("{c}/sub"), &l), format!("{l}/sub"));
        assert_eq!(aliased("/", &l), "/");
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn info(kind: SessionKind, state: AgentState, since: i64) -> SessionInfo {
        SessionInfo {
            account_profile_id: None,
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
            subagents: 0,
            size_owner: None,
            input_interrupted: false,
            live_cwd: None,
            state_seq: 0,
            created_at: 0,
            window_id: None,
        }
    }

    #[test]
    fn screen_changes_state_and_same_state_keeps_detail() {
        let idle = snap(include_str!("adapters/fixtures/claude_idle_done.txt"));
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
        let mut idle = snap(include_str!("adapters/fixtures/claude_idle_done.txt"));
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
        let working = snap(include_str!("adapters/fixtures/claude_running_tool.txt"));
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
    fn stuck_returns_to_idle_once_output_settles() {
        let working = snap(include_str!("adapters/fixtures/claude_running_tool.txt"));
        let mut t = Track::default();
        let w = info(SessionKind::Claude, AgentState::Working, 0);
        assert!(matches!(
            evaluate(&w, &working, &mut t, 1_000),
            Verdict::Keep
        ));
        let at = 1_000 + STUCK_AFTER_MS;
        let r = evaluate(&w, &working, &mut t, at);
        assert!(
            matches!(
                r,
                Verdict::Set(
                    StateUpdate {
                        state: AgentState::Stuck,
                        ..
                    },
                    StateSource::Timer
                )
            ),
            "{r:?}"
        );
        let s = info(SessionKind::Claude, AgentState::Stuck, at);
        // The agent finishes: the screen turns idle with fresh output, which isn't trusted yet.
        let mut idle = snap(include_str!("adapters/fixtures/claude_idle_done.txt"));
        idle.last_output_at = at + 1_000;
        assert!(matches!(
            evaluate(&s, &idle, &mut t, at + 1_100),
            Verdict::Recheck
        ));
        // Next tick: same screen (hash unchanged), output has settled -> Idle.
        let r = evaluate(&s, &idle, &mut t, at + 1_000 + IDLE_SETTLE_MS + 50);
        assert!(
            matches!(
                r,
                Verdict::Set(
                    StateUpdate {
                        state: AgentState::Idle,
                        ..
                    },
                    StateSource::Timer
                )
            ),
            "{r:?}"
        );
    }

    #[test]
    fn progress_survives_store_reopen_but_not_a_terminal_generation_change() {
        let directory = std::env::temp_dir().join(format!("muster-detector-{}", crate::paths::random_token()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("application.db");
        let working = snap(include_str!("adapters/fixtures/claude_running_tool.txt"));
        let info = info(SessionKind::Claude, AgentState::Working, 0);
        {
            let store = crate::store::Store::open(&path).unwrap();
            let mut track = Track::default();
            track.for_generation(7, 1_000);
            assert!(evaluate_inner(&info, &working, &mut track, 1_000).is_none());
            track.seq = 42;
            track.last_eval = 1_000;
            store.save_runtime(CHECKPOINT, &HashMap::from([(info.id, track)])).unwrap();
        }
        let store = crate::store::Store::open(&path).unwrap();
        let mut tracks = store.load_runtime::<HashMap<SessionId, Track>>(CHECKPOINT).unwrap().unwrap();
        let track = tracks.get_mut(&info.id).unwrap();
        assert_eq!((track.seq, track.last_eval), (0, 0), "poll counters must restart");
        track.for_generation(7, STUCK_AFTER_MS);
        let update = evaluate_inner(&info, &working, track, 1_000 + STUCK_AFTER_MS).unwrap();
        assert_eq!(update.0.state, AgentState::Stuck, "replacement keeps the progress clock");
        // The same screen in a new terminal must not inherit an old stuck timer.
        track.for_generation(8, 2_000 + STUCK_AFTER_MS);
        assert!(evaluate_inner(&info, &working, track, 2_000 + STUCK_AFTER_MS).is_none());
        assert_eq!(track.changed_at, 2_000 + STUCK_AFTER_MS);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
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
