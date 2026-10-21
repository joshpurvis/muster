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

/// Handle every due slot, and the expiry notices. Returns the runs to start.
pub fn tick(hub: &Arc<Hub>, now: i64) -> Vec<Run> {
    let settings = hub.schedules.settings();
    let quiet = quiet_of(&settings);
    let mut start = Vec::new();
    for s in hub.schedules.list() {
        let Some(agent) = hub.agents().by_id(s.agent_id) else { continue };
        let f = facts(&agent);
        if plan::warn_expiry(&s, &f, now) {
            match hub.schedules.edit(s.id, |x| {
                x.expiry_warned = true;
                Ok(())
            }) {
                Ok(x) => {
                    let when = plan::expires_at(&x, &f).map(|e| fmt_local(e)).unwrap_or_default();
                    alert(hub, "warn", &format!("{}: schedule \"{}\" expires {when}", agent.name, x.name),
                        "Claude subscription agents' schedules stop after 7 days. Renew it in the Agents panel to keep it running.");
                    publish(hub, &x);
                }
                Err(e) => tracing::warn!(schedule = s.id, "{e:#}"),
            }
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

/// Act on schedule `id`'s due slot: claim it (with its run row) and say whether to start it.
fn handle_due(hub: &Arc<Hub>, id: i64, now: i64, settings: &super::Settings, quiet: &plan::Quiet, f: &plan::Facts, agent: &str) -> anyhow::Result<Option<Run>> {
    let sched = &hub.schedules;
    let (run, s) = {
        let mut g = sched.inner.lock();
        let Some(s) = g.list.get(&id).cloned() else { return Ok(None) };
        let last = hub.store().runs(id, None, 1)?.into_iter().next();
        let runs_today = hub.store().count_delivered_runs_since(id, now - plan::DAY_MS)?;
        let Some(due) = plan::due(&s, now, settings, quiet, f, runs_today, last.as_ref().and_then(|r| r.reason.as_deref())) else {
            return Ok(None);
        };
        let mut next = s.clone();
        next.next_run = due.next_run;
        next.last_slot = Some(due.slot);
        if next.skip_slot.is_some_and(|k| k <= due.slot) {
            next.skip_slot = None;
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
    let _ = hub.store().prune_runs(id, super::RUNS_KEEP);
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

/// Is the agent in a turn, or does its session have prompts waiting? (Not running: not busy;
/// sending starts it.)
fn agent_busy(hub: &Hub, agent_id: i64) -> Option<SessionId> {
    let a = hub.agents().by_id(agent_id)?;
    let sid = a.session_id?;
    let s = hub.session(sid)?;
    let busy = s.state != AgentState::Exited
        && (s.queued > 0
            || matches!(s.state, AgentState::Starting | AgentState::Working | AgentState::NeedsApproval | AgentState::AskingQuestion | AgentState::Stuck));
    busy.then_some(sid)
}

/// Take a run from pending to its outcome: wait in line, start fresh if asked, send, wait.
async fn execute(hub: &Arc<Hub>, mut run: Run) -> anyhow::Result<()> {
    let sched = &hub.schedules;
    let Some(s) = sched.get(run.schedule_id) else { return Ok(()) };
    let Some(agent) = hub.agents().by_id(s.agent_id) else {
        return finish(hub, &s, &mut run, Outcome::Failed, Some("its agent was removed".into()));
    };
    // Already sent (a restart while it waited for the reply): just wait.
    if run.msg_id.is_none() {
        let previous = hub.store().pending_runs()?.iter().any(|r| r.schedule_id == run.schedule_id && r.id < run.id);
        if let Gate::Skip(why) = plan::gate(previous, false, 0) {
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
        let line = sched.line(agent.id);
        let deadline = run.started_at + plan::LINE_WAIT_MS;
        let left = |now: i64| Duration::from_millis((deadline - now).max(0) as u64);
        // Held until the reply (or the end of the wait for it).
        let _line = match tokio::time::timeout(left(sched.now()), line.lock_owned()).await {
            Ok(g) => g,
            Err(_) => return finish(hub, &s, &mut run, Outcome::Skipped, plan_skip(plan::gate(false, true, plan::LINE_WAIT_MS))),
        };
        let mut rx = hub.subscribe();
        loop {
            let busy = agent_busy(hub, agent.id);
            match plan::gate(false, busy.is_some(), sched.now() - run.started_at) {
                Gate::Go => break,
                Gate::Skip(why) => return finish(hub, &s, &mut run, Outcome::Skipped, Some(why)),
                Gate::Wait => {
                    if let Some(sid) = busy {
                        crate::mcp::wait::next_change(&mut rx, sid, left(sched.now()).min(Duration::from_secs(5))).await;
                    }
                }
            }
        }
        // The schedule may have changed (or gone) while it waited.
        let Some(s) = sched.get(run.schedule_id) else { return Ok(()) };
        let Some(agent) = hub.agents().by_id(s.agent_id) else {
            return finish(hub, &s, &mut run, Outcome::Failed, Some("its agent was removed".into()));
        };
        if s.conversation == Conversation::Fresh {
            // Between turns (checked above), so this never cuts into the human's own turn.
            if let Err(e) = crate::agents::start(hub, &agent.name, true, None).await {
                return finish(hub, &s, &mut run, Outcome::Failed, Some(format!("couldn't start a fresh conversation: {e:#}")));
            }
        }
        let tz = super::cron::zone(&s.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let (last_run, last_reply) = hub.store().last_runs_for_prompt(s.id, run.id)?;
        let text = plan::fill_prompt(&s.prompt, &tz, run.slot, last_run, last_reply.as_deref());
        let sent = crate::agents::send_checked(hub, &agent, text, sender(s.id), None, None, Some(Origin::Schedule)).await;
        match sent {
            Ok((d, _)) => {
                run.msg_id = Some(d.msg_id);
                hub.store().update_run(&run)?;
                publish_run(hub, &run);
            }
            Err(e) => {
                let refused = e.downcast_ref::<crate::agents::Refused>().map(|r| r.0.clone());
                return match refused {
                    Some(why) => finish(hub, &s, &mut run, Outcome::Refused, Some(why)),
                    None => finish(hub, &s, &mut run, Outcome::Failed, Some(format!("{e:#}"))),
                };
            }
        }
        return wait_reply(hub, &s, &mut run).await;
    }
    let line = sched.line(agent.id);
    let _line = line.lock_owned().await;
    wait_reply(hub, &s, &mut run).await
}

fn plan_skip(g: Gate) -> Option<String> {
    match g {
        Gate::Skip(why) => Some(why),
        _ => None,
    }
}

async fn wait_reply(hub: &Arc<Hub>, s: &Schedule, run: &mut Run) -> anyhow::Result<()> {
    let msg = run.msg_id.expect("a sent run");
    let Some(m) = hub.store().agent_message(msg)? else {
        return finish(hub, s, run, Outcome::Failed, Some("its message is gone".into()));
    };
    let Some(sid) = m.to_session else {
        return finish(hub, s, run, Outcome::Failed, Some("its message reached no session".into()));
    };
    let left = (run.started_at + plan::LINE_WAIT_MS + plan::RUN_TIMEOUT_MS - hub.schedules.now()).max(60_000);
    let mut rx = hub.subscribe();
    let waited = crate::agents::wait_for_reply(
        &mut rx,
        sid,
        Duration::from_millis(left.min(plan::RUN_TIMEOUT_MS) as u64),
        || hub.session(sid),
        || hub.agents().msg_status(hub.store(), msg),
    )
    .await;
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
fn finish(hub: &Arc<Hub>, s: &Schedule, run: &mut Run, outcome: Outcome, reason: Option<String>) -> anyhow::Result<()> {
    run.outcome = outcome;
    run.reason = reason;
    run.finished_at = Some(hub.schedules.now());
    hub.store().update_run(run)?;
    let agent = hub.agents().by_id(s.agent_id).map(|a| a.name).unwrap_or_else(|| "?".into());
    if outcome == Outcome::Replied
        && let Some(note) = s.note_id
        && let Err(e) = reply_to_note(hub, s, &agent, run, note)
    {
        let why = format!("{e:#}");
        tracing::warn!(schedule = s.id, "putting the reply in note {note}: {why}");
        hub.notice("warn", format!("{agent}: couldn't add the reply of \"{}\" to its note: {why}", s.name));
    }
    publish_run(hub, run);
    notify_outcome(hub, s, &agent, run, false);
    Ok(())
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

/// At start: runs a previous backend left pending. One whose message was sent waits for its
/// reply again; one that was still in line goes back in line until its hour is up (its message
/// is looked up first, in case the backend stopped right after sending it).
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
            && let Ok(Some(m)) = hub.store().find_agent_message_from(s.agent_id, &sender(s.id), run_started_real(hub, &run))
        {
            run.msg_id = Some(m);
            let _ = hub.store().update_run(&run);
        }
        if run.msg_id.is_none() && now - run.started_at >= plan::LINE_WAIT_MS {
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
