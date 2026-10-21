//! The scheduler loop and the runs it starts.

use std::sync::Arc;
use std::time::Duration;

use super::plan::{self, Action, Gate};
use super::{Conversation, Notify, Outcome, Run, RunKind, Schedule, facts, publish, publish_run, quiet_of};
use crate::agents::{AskVerdict, Origin};
use crate::hub::Hub;
use crate::model::*;

/// Catch-ups wait this long after the scheduler starts (agents autostart meanwhile), and this
/// long after the previous catch-up started. Debug builds read `MUSTER_TEST_CATCHUP_GAP_MS`.
fn catchup_gap_ms() -> i64 {
    if cfg!(debug_assertions)
        && let Some(v) = std::env::var("MUSTER_TEST_CATCHUP_GAP_MS").ok().and_then(|v| v.parse().ok())
    {
        return v;
    }
    30_000
}

/// A fresh conversation never starts within this long of the human typing into the agent's
/// terminal (they may be writing a prompt that a restart would wipe).
const HUMAN_QUIET: Duration = Duration::from_secs(120);

/// The sender label of a schedule's messages (the envelope's `from`).
pub fn sender(schedule: i64) -> String {
    format!("schedule:{schedule}")
}

pub async fn scheduler_loop(hub: Arc<Hub>) {
    let sched = &hub.schedules;
    let mut prev_wall = sched.now();
    let mut prev_mono = tokio::time::Instant::now();
    loop {
        let wall = sched.now();
        let mono = tokio::time::Instant::now();
        if let Some(drift) = plan::clock_jump(prev_wall, wall, mono.duration_since(prev_mono).as_millis() as i64) {
            // Forward: a suspend (the monotonic clock stops while asleep) or the clock set ahead:
            // due slots are handled as missed or on time below. Back: slots already claimed
            // stay claimed (the run table refuses a second claim), so nothing fires twice.
            tracing::info!("schedules: the wall clock moved {} s against the monotonic clock; re-checking", drift / 1000);
        }
        (prev_wall, prev_mono) = (wall, mono);
        for run in tick(&hub, wall) {
            spawn_execute(hub.clone(), run);
        }
        // A slot still due after the tick couldn't be handled (its agent is gone, the store
        // failed): look again in 30 s rather than spin.
        let now = sched.now();
        let next = sched.list().into_iter().filter_map(|s| s.next_run).map(|t| if t <= now { now + 30_000 } else { t });
        let ms = plan::sleep_ms(now, next).max(50);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms as u64)) => {}
            _ = sched.wake.notified() => {}
        }
        if hub.is_shutting_down() {
            return;
        }
    }
}

/// Handle every due slot, the expiry notices, and tell clients when a schedule expires (no slot
/// may be due then). Returns the runs to start.
pub fn tick(hub: &Arc<Hub>, now: i64) -> Vec<Run> {
    let settings = hub.schedules.settings();
    let quiet = quiet_of(&settings);
    let mut start = Vec::new();
    for s in hub.schedules.list() {
        let Some(agent) = hub.agents().by_id(s.agent_id) else { continue };
        let f = facts(hub, &agent);
        if plan::warn_expiry(&s, &f, now) {
            match hub.schedules.edit(s.id, |x| {
                x.expiry_warned = true;
                Ok(())
            }) {
                Ok(x) => {
                    let when = plan::expires_at(&x, &f).map(fmt_local).unwrap_or_default();
                    alert(hub, "warn", &format!("{}: schedule \"{}\" expires {when}", agent.name, x.name),
                        "Claude subscription agents' schedules stop after 7 days. Renew it in the Agents panel to keep it running.");
                    publish(hub, &x);
                }
                Err(e) => tracing::warn!(schedule = s.id, "{e:#}"),
            }
        }
        if plan::expires_at(&s, &f).is_some_and(|e| now >= e) && hub.schedules.expired_seen.lock().insert(s.id) {
            publish(hub, &s);
            cancel_unsent(hub, Some(s.id), plan::EXPIRED, false);
        }
        if s.next_run.is_none_or(|t| t > now) {
            continue;
        }
        match handle_due(hub, s.id, now, &settings, &quiet, &f, &agent.name) {
            Ok(Some(run)) => start.push(run),
            Ok(None) => {}
            Err(e) => tracing::warn!(schedule = s.id, "handling a due run: {e:#}"),
        }
    }
    start
}

/// Runs of `schedule` whose messages reached the agent in the last 24 hours (by the time each
/// message was sent: the daemon's clock, which agent messages are stamped with).
fn runs_today(hub: &Hub, schedule: i64) -> anyhow::Result<u32> {
    hub.store().count_delivered_runs_since(schedule, now_ms() - plan::DAY_MS, None)
}

/// Act on schedule `id`'s due slot: claim it (with its run row) and say whether to start it.
fn handle_due(hub: &Arc<Hub>, id: i64, now: i64, settings: &super::Settings, quiet: &plan::Quiet, f: &plan::Facts, agent: &str) -> anyhow::Result<Option<Run>> {
    let sched = &hub.schedules;
    let (run, s) = {
        let mut g = sched.inner.lock();
        let Some(s) = g.list.get(&id).cloned() else { return Ok(None) };
        let last = hub.store().runs(id, None, 1)?.into_iter().next();
        let today = runs_today(hub, id)?;
        let Some(due) = plan::due(&s, now, settings, quiet, f, today, last.as_ref().and_then(|r| r.reason.as_deref())) else {
            return Ok(None);
        };
        let mut next = s.clone();
        next.next_run = due.next_run;
        next.last_slot = Some(due.slot);
        if next.skip_slot.is_some_and(|k| k <= due.slot) {
            next.skip_slot = None;
        }
        if matches!(due.action, Action::Fire(_)) {
            next.last_fired = Some(due.slot);
        }
        next.updated_at = now;
        let mut run = Run {
            id: 0,
            schedule_id: id,
            slot: due.slot,
            kind: RunKind::Scheduled,
            started_at: now,
            msg_id: None,
            outcome: Outcome::Pending,
            reason: None,
            finished_at: None,
        };
        match &due.action {
            Action::Fire(kind) => {
                run.kind = *kind;
                if *kind == RunKind::CatchUp {
                    run.reason = Some(if due.count > 1 {
                        format!("catch-up: {} runs were missed while Muster was off or asleep; this is the latest", due.count)
                    } else {
                        "catch-up: missed while Muster was off or asleep".into()
                    });
                }
            }
            Action::Skip(why) | Action::Refuse(why) => {
                run.outcome = if matches!(due.action, Action::Skip(_)) { Outcome::Skipped } else { Outcome::Refused };
                run.reason = Some(why.clone());
                run.finished_at = Some(now);
            }
        }
        let claimed = if due.record {
            hub.store().claim_run(&next, &run)?
        } else {
            hub.store().save_schedule(&next)?;
            None
        };
        g.list.insert(id, next.clone());
        (claimed.map(|rid| Run { id: rid, ..run }), next)
    };
    let Some(run) = run else {
        publish(hub, &s);
        return Ok(None);
    };
    publish_run(hub, &run);
    if run.outcome == Outcome::Pending {
        return Ok(Some(run));
    }
    // Expiry is worth telling even when this schedule only notifies on failure.
    let expired = run.reason.as_deref() == Some(plan::EXPIRED);
    notify_outcome(hub, &s, agent, &run, expired);
    Ok(None)
}

pub fn spawn_execute(hub: Arc<Hub>, run: Run) {
    tokio::spawn(async move {
        let id = run.id;
        if let Err(e) = execute(&hub, run).await {
            tracing::warn!(run = id, "schedule run: {e:#}");
            // Never left pending (a pending run holds back its schedule's next ones).
            if let Ok(Some(mut r)) = hub.store().run(id)
                && r.outcome == Outcome::Pending
                && let Some(s) = hub.schedules.get(r.schedule_id)
            {
                let _ = finish(&hub, &s, &mut r, Outcome::Failed, Some(format!("{e:#}")));
            }
        }
    });
}

/// A session in a turn, or with prompts waiting.
fn session_busy(s: &SessionInfo) -> bool {
    s.state != AgentState::Exited
        && (s.queued > 0
            || matches!(s.state, AgentState::Starting | AgentState::Working | AgentState::NeedsApproval | AgentState::AskingQuestion | AgentState::Stuck))
}

/// The agent's session when it is busy (not running: not busy; sending starts it).
fn agent_busy(hub: &Hub, agent_id: i64) -> Option<SessionId> {
    let sid = hub.agents().by_id(agent_id)?.session_id?;
    hub.session(sid).filter(session_busy).map(|_| sid)
}

/// Is an earlier run of schedule `schedule` (before run `before`) still going? A pending one
/// is; so is the newest one whose message's turn is still running in the agent although the run
/// stopped waiting for it (timed out, or blocked on a prompt nobody has answered yet).
pub(super) fn previous_running(hub: &Hub, schedule: i64, before: i64) -> anyhow::Result<bool> {
    if hub.store().pending_runs()?.iter().any(|r| r.schedule_id == schedule && r.id < before) {
        return Ok(true);
    }
    // The newest run that sent a message, however many skips came after it.
    let sent = hub.store().latest_sent_run(schedule, before)?;
    let Some(msg) = sent.and_then(|r| r.msg_id) else { return Ok(false) };
    let st = hub.agents().msg_status(hub.store(), msg);
    if st.reply.is_some() || !st.active {
        return Ok(false);
    }
    let session = hub.store().agent_message(msg)?.and_then(|m| m.to_session).and_then(|s| hub.session(s));
    Ok(session.is_some_and(|s| s.state != AgentState::Idle && s.state != AgentState::Exited))
}

fn refusal(e: &anyhow::Error) -> Option<String> {
    e.downcast_ref::<crate::agents::Refused>().map(|r| r.0.clone())
}

/// Take a run from pending to its outcome: wait in line, check again, start fresh if asked,
/// send, wait for the reply.
async fn execute(hub: &Arc<Hub>, mut run: Run) -> anyhow::Result<()> {
    let sched = &hub.schedules;
    let Some(s) = sched.get(run.schedule_id) else { return Ok(()) };
    let Some(agent) = hub.agents().by_id(s.agent_id) else {
        return finish(hub, &s, &mut run, Outcome::Failed, Some("its agent was removed".into()));
    };
    let line = sched.line(agent.id);
    // Already sent (a restart while it waited for the reply): just wait.
    if run.msg_id.is_some() {
        let _line = line.lock_owned().await;
        return wait_reply(hub, &s, &mut run).await;
    }
    if let Gate::Skip(why) = plan::gate(previous_running(hub, run.schedule_id, run.id)?, false, 0) {
        return finish(hub, &s, &mut run, Outcome::Skipped, Some(why));
    }
    if run.kind == RunKind::CatchUp {
        let mut last = sched.catchup.lock().await;
        let gap = catchup_gap_ms();
        let at = last.map_or(sched.started_at() + gap, |t| t + gap).max(sched.started_at() + gap);
        let wait = at - sched.now();
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
        *last = Some(sched.now());
    }
    // One run at a time per agent, waiting in line behind its other work.
    let deadline = run.started_at + plan::LINE_WAIT_MS;
    let left = |now: i64| Duration::from_millis((deadline - now).max(0) as u64);
    // Held until the reply (or the end of the wait for it).
    let _line = match tokio::time::timeout(left(sched.now()), line.lock_owned()).await {
        Ok(g) => g,
        Err(_) => return finish(hub, &s, &mut run, Outcome::Skipped, Some("busy: the agent was busy for an hour".into())),
    };
    let mut rx = hub.subscribe();
    let (s, d) = loop {
        let busy = agent_busy(hub, agent.id);
        match plan::gate(false, busy.is_some(), sched.now() - run.started_at) {
            Gate::Go => {}
            Gate::Skip(why) => return finish(hub, &s, &mut run, Outcome::Skipped, Some(why)),
            Gate::Wait => {
                if let Some(sid) = busy {
                    crate::mcp::wait::next_change(&mut rx, sid, left(sched.now()).min(Duration::from_secs(5))).await;
                }
                continue;
            }
        }
        // It may have waited a while: the schedule (or its agent, the settings) may have
        // changed. An automatic run checks its rules again; Run now is the human's call.
        let Some(s) = sched.get(run.schedule_id) else { return Ok(()) };
        let Some(agent) = hub.agents().by_id(s.agent_id) else {
            return finish(hub, &s, &mut run, Outcome::Failed, Some("its agent was removed".into()));
        };
        if run.kind != RunKind::Manual {
            let settings = sched.settings();
            let since = s.last_sent.map(|t| now_ms() - t);
            match plan::still_ok(&s, sched.now(), &settings, &quiet_of(&settings), &facts(hub, &agent), runs_today(hub, s.id)?, since) {
                Some(Action::Skip(why)) => return finish(hub, &s, &mut run, Outcome::Skipped, Some(why)),
                Some(Action::Refuse(why)) => return finish(hub, &s, &mut run, Outcome::Refused, Some(why)),
                _ => {}
            }
        }
        let tz = super::cron::zone(&s.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let (last_run, last_reply) = hub.store().last_runs_for_prompt(s.id, run.id)?;
        let text = plan::fill_prompt(&s.prompt, &tz, run.slot, last_run, last_reply.as_deref());
        // Every delivery check before anything is restarted: a refused run leaves the agent's
        // conversation as it was.
        let route = match crate::agents::precheck(hub, &agent, &text, &sender(s.id), None, None, Some(Origin::Schedule)) {
            Ok(r) => r,
            Err(e) => {
                return match refusal(&e) {
                    Some(why) => finish(hub, &s, &mut run, Outcome::Refused, Some(why)),
                    None => finish(hub, &s, &mut run, Outcome::Failed, Some(format!("{e:#}"))),
                };
            }
        };
        if s.conversation == Conversation::Fresh {
            // Checked again under the start lock, together with the restart: never while the
            // agent is in a turn, and never within two minutes of (or since this check of) the
            // human typing into it.
            let seen = agent.session_id.and_then(|sid| hub.human_input_of(sid));
            let quiet_human = seen.is_none_or(|(_, at)| at.elapsed() >= HUMAN_QUIET);
            let ok = |info: &SessionInfo| quiet_human && !session_busy(info) && hub.human_input_of(info.id) == seen;
            match crate::agents::start_fresh_if(hub, &agent.name, &ok).await {
                Ok(_) => {}
                Err(e) if e.downcast_ref::<crate::agents::NotIdle>().is_some() => {
                    // Back in line: the hour still counts.
                    match agent.session_id {
                        Some(sid) => crate::mcp::wait::next_change(&mut rx, sid, left(sched.now()).min(Duration::from_secs(5))).await,
                        None => tokio::time::sleep(Duration::from_secs(1)).await,
                    }
                    if sched.now() - run.started_at >= plan::LINE_WAIT_MS {
                        return finish(hub, &s, &mut run, Outcome::Skipped, Some("busy: you were using the agent, so no fresh conversation started within the hour".into()));
                    }
                    continue;
                }
                Err(e) => return finish(hub, &s, &mut run, Outcome::Failed, Some(format!("couldn't start a fresh conversation: {e:#}"))),
            }
        }
        let out = crate::agents::Outgoing { agent: agent.name.clone(), text, from: sender(s.id), from_session: None, route };
        match crate::agents::deliver(hub, out).await {
            Ok(d) => break (s, d),
            Err(e) => {
                return match refusal(&e) {
                    Some(why) => finish(hub, &s, &mut run, Outcome::Refused, Some(why)),
                    None => finish(hub, &s, &mut run, Outcome::Failed, Some(format!("{e:#}"))),
                };
            }
        }
    };
    // Its prompt may already have been taken back at the submission check (an idle agent takes
    // it at once): then the run has ended, and stays as it was recorded.
    if hub.store().run(run.id)?.is_none_or(|r| r.outcome != Outcome::Pending) {
        return Ok(());
    }
    run.msg_id = Some(d.msg_id);
    hub.store().update_run(&run)?;
    publish_run(hub, &run);
    wait_reply(hub, &s, &mut run).await
}

async fn wait_reply(hub: &Arc<Hub>, s: &Schedule, run: &mut Run) -> anyhow::Result<()> {
    let msg = run.msg_id.expect("a sent run");
    let Some(m) = hub.store().agent_message(msg)? else {
        return finish(hub, s, run, Outcome::Failed, Some("its message is gone".into()));
    };
    let Some(sid) = m.to_session else {
        return finish(hub, s, run, Outcome::Failed, Some("its message reached no session".into()));
    };
    // The hour for the reply counts from when the message was sent, across restarts.
    let left = (m.created_at + plan::RUN_TIMEOUT_MS - now_ms()).max(1000);
    let mut rx = hub.subscribe();
    // Woken when the prompt is taken back before it was submitted (`unsent`): that ends the run.
    let woken = hub.schedules.waiters.lock().entry(run.id).or_default().clone();
    let waited = tokio::select! {
        w = crate::agents::wait_for_reply(
            &mut rx,
            sid,
            Duration::from_millis(left as u64),
            || hub.session(sid),
            || hub.agents().msg_status(hub.store(), msg),
        ) => w,
        _ = woken.notified() => {
            hub.schedules.waiters.lock().remove(&run.id);
            return Ok(());
        }
    };
    hub.schedules.waiters.lock().remove(&run.id);
    // Never submitted within the hour (an unsent draft in the agent's input box kept it
    // waiting): take it back, so clearing the draft later can't run it after all.
    if matches!(waited, Ok((_, AskVerdict::Wait, _))) {
        let st = hub.agents().msg_status(hub.store(), msg);
        if st.delivered_at.is_none() && st.reply.is_none() && crate::inject::drop_message(hub, msg).unwrap_or(false) {
            unsent(hub, msg, Action::Skip(format!("its prompt waited in the agent's queue for {} minutes without being submitted",
                plan::RUN_TIMEOUT_MS / 60_000)));
            return Ok(());
        }
    }
    let (outcome, reason) = match waited {
        Err(e) => (Outcome::Ended, Some(e)),
        Ok((info, v, _)) => match v {
            AskVerdict::Replied(_) => (Outcome::Replied, None),
            AskVerdict::Blocked => (
                Outcome::Blocked,
                Some(info.state_detail.clone().unwrap_or_else(|| info.state.as_str().replace('_', " "))),
            ),
            AskVerdict::Unattributed => (Outcome::NoReply, Some("the turn ended without a final message for this run".into())),
            AskVerdict::Ended => (Outcome::Ended, Some("the agent's process ended".into())),
            AskVerdict::Wait => (Outcome::TimedOut, Some(format!("no reply within {} minutes", plan::RUN_TIMEOUT_MS / 60_000))),
        },
    };
    finish(hub, s, run, outcome, reason)
}

/// Record the outcome, then alert and file the reply as the schedule says.
fn finish(hub: &Hub, s: &Schedule, run: &mut Run, outcome: Outcome, reason: Option<String>) -> anyhow::Result<()> {
    run.outcome = outcome;
    run.reason = reason;
    run.finished_at = Some(hub.schedules.now());
    hub.store().update_run(run)?;
    let agent = hub.agents().by_id(s.agent_id).map(|a| a.name).unwrap_or_else(|| "?".into());
    if outcome == Outcome::Replied {
        reply_to_note_once(hub, s, run);
    }
    publish_run(hub, run);
    notify_outcome(hub, s, &agent, run, false);
    Ok(())
}

/// Put run `run`'s reply in its schedule's note, at most once per run (the run waiting for its
/// reply and a reply that arrives after it stopped waiting both come here).
pub(super) fn reply_to_note_once(hub: &Hub, s: &Schedule, run: &Run) {
    let Some(note) = s.note_id else { return };
    let has_reply = run.msg_id.and_then(|m| hub.store().agent_message(m).ok().flatten()).is_some_and(|m| m.reply.is_some());
    if !has_reply {
        return;
    }
    match hub.store().mark_run_noted(run.id) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => return tracing::warn!(run = run.id, "marking its reply noted: {e:#}"),
    }
    let agent = hub.agents().by_id(s.agent_id).map(|a| a.name).unwrap_or_else(|| "?".into());
    if let Err(e) = reply_to_note(hub, s, &agent, run, note) {
        let why = format!("{e:#}");
        tracing::warn!(schedule = s.id, "putting the reply in note {note}: {why}");
        hub.notice("warn", format!("{agent}: couldn't add the reply of \"{}\" to its note: {why}", s.name));
    }
}

/// A run's prompt was taken back before it was submitted (the submission check said no, or it
/// waited too long): the message is forgotten (it never reached the agent, so it counts toward
/// no cap) and the run ends skipped or refused, waking its waiting task.
pub(super) fn unsent(hub: &Hub, msg: i64, action: Action) {
    let from = hub.store().agent_message(msg).ok().flatten().map(|m| m.from).unwrap_or_default();
    let Some(mut run) = run_for_msg(hub, msg, &from) else {
        let _ = hub.store().delete_agent_message(msg);
        return;
    };
    let _ = hub.store().delete_agent_message(msg);
    let _ = hub.store().clear_run_msg(run.id);
    run.msg_id = None;
    let (outcome, why) = match action {
        Action::Refuse(w) => (Outcome::Refused, w),
        Action::Skip(w) => (Outcome::Skipped, w),
        Action::Fire(_) => (Outcome::Skipped, "it was taken back".to_string()),
    };
    if let Some(s) = hub.schedules.get(run.schedule_id) {
        let _ = finish(hub, &s, &mut run, outcome, Some(format!("not sent: {why}")));
    } else {
        run.outcome = outcome;
        run.reason = Some(format!("not sent: {why}"));
        run.finished_at = Some(hub.schedules.now());
        let _ = hub.store().update_run(&run);
    }
    if let Some(w) = hub.schedules.waiters.lock().remove(&run.id) {
        w.notify_one();
    }
}

/// The run that sent message `msg` (sender `from`). The prompt can reach the injector before
/// the run row records the message id (an idle agent takes it at once): then it is that
/// schedule's pending run that hasn't recorded a message yet (one run of a schedule sends at a
/// time: `previous_running`).
pub(super) fn run_for_msg(hub: &Hub, msg: i64, from: &str) -> Option<Run> {
    if let Ok(Some(r)) = hub.store().run_by_msg(msg) {
        return Some(r);
    }
    let schedule: i64 = from.strip_prefix("schedule:")?.parse().ok()?;
    hub.store().pending_runs().ok()?.into_iter().filter(|r| r.schedule_id == schedule && r.msg_id.is_none()).max_by_key(|r| r.id)
}

/// Take back the prompts of pending runs (of `schedule`, else of every schedule) that are still
/// waiting in an agent's prompt queue, unsubmitted: paused, removed or expired, they mustn't go
/// out later. Run now's prompts are the human's and stay, unless the schedule is removed.
pub(super) fn cancel_unsent(hub: &Hub, schedule: Option<i64>, why: &str, manual_too: bool) {
    let Ok(pending) = hub.store().pending_runs() else { return };
    for r in pending {
        if schedule.is_some_and(|s| s != r.schedule_id) || (r.kind == RunKind::Manual && !manual_too) {
            continue;
        }
        let Some(msg) = r.msg_id else { continue };
        let st = hub.agents().msg_status(hub.store(), msg);
        if st.delivered_at.is_none() && st.reply.is_none() && crate::inject::drop_message(hub, msg).unwrap_or(false) {
            unsent(hub, msg, Action::Skip(why.to_string()));
        }
    }
}

fn reply_to_note(hub: &Hub, s: &Schedule, agent: &str, run: &Run, note: NoteId) -> anyhow::Result<()> {
    let reply = run
        .msg_id
        .and_then(|m| hub.store().agent_message(m).ok().flatten())
        .and_then(|m| m.reply)
        .ok_or_else(|| anyhow::anyhow!("the reply wasn't kept"))?;
    let tz = super::cron::zone(&s.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let heading = format!("## {} · {}", super::cron::format(&tz, run.slot, "%a %-d %b %Y %H:%M"), s.name);
    let n = crate::notes::get(hub, note)?;
    let lead = if n.body.trim().is_empty() { "" } else { "\n" };
    let by = NoteAuthor { kind: "agent".into(), name: Some(agent.to_string()), session_id: None };
    crate::notes::append(hub, note, &format!("{lead}{heading}\n\n{}\n", reply.trim_end()), by)?;
    Ok(())
}

fn notify_outcome(hub: &Hub, s: &Schedule, agent: &str, run: &Run, force: bool) {
    let want = force
        || match s.notify {
            Notify::Always => true,
            Notify::OnFailure => run.outcome.is_failure(),
            Notify::Never => false,
        };
    if !want {
        return;
    }
    let summary = format!("{agent}: \"{}\" {}", s.name, run.outcome.label());
    let body = run.reason.clone().unwrap_or_else(|| match run.outcome {
        Outcome::Replied => "Open the Agents panel for the reply.".into(),
        _ => String::new(),
    });
    let level = if run.outcome.is_failure() || force { "warn" } else { "info" };
    alert(hub, level, &summary, &body);
}

/// An in-app notice and a desktop notification.
fn alert(hub: &Hub, level: &str, summary: &str, body: &str) {
    hub.notice(level, if body.is_empty() { summary.to_string() } else { format!("{summary}: {body}") });
    crate::notify::alert(summary, body);
}

fn fmt_local(ms: i64) -> String {
    super::cron::format(&jiff::tz::TimeZone::system(), ms, "%a %-d %b at %H:%M")
}

/// At start: runs a previous backend left pending. One whose message was typed waits for its
/// reply again (until an hour after it was sent); one whose message was recorded but is in no
/// prompt queue and was never typed (the backend stopped in between) failed; one that was still
/// in line goes back in line until its hour is up (its message is looked up first, in case the
/// backend stopped right after sending it).
pub fn recover(hub: &Arc<Hub>) {
    let pending = match hub.store().pending_runs() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("schedules: reading unfinished runs: {e:#}");
            return;
        }
    };
    let now = hub.schedules.now();
    for mut run in pending {
        let Some(s) = hub.schedules.get(run.schedule_id) else { continue };
        if run.msg_id.is_none()
            && let Ok(Some(m)) = hub.store().find_agent_message_from(s.agent_id, &super::run::sender(s.id), run_started_real(hub, &run))
        {
            run.msg_id = Some(m);
            let _ = hub.store().update_run(&run);
        }
        if let Some(msg) = run.msg_id {
            let st = hub.agents().msg_status(hub.store(), msg);
            if st.delivered_at.is_none() && st.reply.is_none() && !crate::inject::holds_message(hub, msg) {
                let _ = finish(hub, &s, &mut run, Outcome::Failed,
                    Some("Muster stopped before the message was typed; nothing reached the agent".into()));
                continue;
            }
        } else if now - run.started_at >= plan::LINE_WAIT_MS {
            let _ = finish(hub, &s, &mut run, Outcome::Skipped, Some("Muster restarted while it waited in line".into()));
            continue;
        }
        spawn_execute(hub.clone(), run);
    }
}

/// `run.started_at` on the real clock (agent messages are stamped with it; the scheduler's may be
/// offset in tests).
fn run_started_real(hub: &Hub, run: &Run) -> i64 {
    run.started_at - (hub.schedules.now() - now_ms())
}
